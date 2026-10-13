//! Scheduled sync: the service keeps every connection fresh on its own, with
//! no model involved.
//!
//! Each connection syncs on its own interval, with jitter, in a task of its
//! own, so a slow or failing source never holds up another; and never
//! alongside itself: an explicit sync joins one already running instead of
//! starting a second. A failure backs off exponentially, up to a cap; a
//! source's rate-limit hold defers the next sync until it ends; a rejected
//! credential stops scheduled syncs until the person verifies it again or
//! reconnects. What a sync found is saved with the connection's sync state,
//! in one transaction, only once the sync processed it ([`Db::save_found`]),
//! so a sync that stops halfway leaves the last good data and is due again.

use crate::{
    bot::{self, AgentRuns, BoardSummary},
    clock::{Clock, Timestamp},
    connector::{self, Account, Connector, Credential, Requests, Target},
    connectors::{self, Settings},
    db::Db,
    hub::{self, Pull},
    model::{
        BotConfig, ConnectInput, Connection, ErrorKind, Item, PluginConfig, Schedule, Snapshot,
        SyncState, WatchList,
    },
    zone::ExternalSnapshot,
};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use tokio::sync::{watch, Notify, OwnedMutexGuard};

/// How often a connection syncs unless it, or the service, says otherwise.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// The shortest sync interval accepted, to spare the sources.
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);
/// The longest sync interval accepted.
pub const MAX_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// How far a wait moves at random, as a share of it, so connections that
/// started together drift apart and no source sees a fixed beat.
const JITTER: f64 = 0.1;
/// The wait after a first failure, doubled after each next one.
const FIRST_BACKOFF: Duration = Duration::from_secs(60);
/// The longest wait after failures.
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// The longest the scheduler sleeps before it looks again, so it soon
/// notices a clock that jumped, as when a laptop wakes.
const MAX_SLEEP: Duration = Duration::from_secs(30);
/// The shortest it sleeps, so nothing it can't start makes it spin.
const MIN_SLEEP: Duration = Duration::from_secs(1);
/// How many of its intervals after its last success a connection's items
/// are stale: enough that one slow or failed sync doesn't make them so.
const STALE_INTERVALS: u32 = 3;

/// When connections sync.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// The interval of a connection that has none of its own.
    pub interval: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
        }
    }
}

impl Policy {
    /// How often `c` syncs.
    pub fn interval(&self, c: &Connection) -> Duration {
        c.sync_interval.map_or(self.interval, Duration::from_secs)
    }
    /// How long after its last success the items of `c` are stale.
    pub fn stale_after(&self, c: &Connection) -> Duration {
        self.interval(c).saturating_mul(STALE_INTERVALS)
    }
    /// Whether the items of `c` are too old to be shown as current at
    /// `now`, or it never synced.
    pub fn is_stale(&self, c: &Connection, now: Timestamp) -> bool {
        c.sync
            .last_success
            .is_none_or(|last| now.since(last) > self.stale_after(c))
    }
    /// When the next scheduled sync of `c` is due, after one that finished
    /// at `now` and left `state`: an interval after a success, a backoff
    /// after a failure, and not before a hold the source asked for ends.
    /// None after a rejected credential, which needs the person. `sample`,
    /// from 0 to 1, picks the jitter.
    pub fn next_due(
        &self,
        c: &Connection,
        state: &SyncState,
        now: Timestamp,
        sample: f64,
    ) -> Option<Timestamp> {
        let wait = match state.error_kind {
            None => jitter(self.interval(c), sample),
            Some(ErrorKind::Auth) => return None,
            // Capped after the jitter, so no backoff is longer than the cap.
            Some(_) => jitter(backoff(state.failures), sample).min(MAX_BACKOFF),
        };
        let due = now.after(wait);
        Some(state.held_until.map_or(due, |held| due.max(held)))
    }
}

/// Whether `interval` may be a sync interval.
pub fn check_interval(interval: Duration) -> Result<()> {
    if !(MIN_INTERVAL..=MAX_INTERVAL).contains(&interval) {
        bail!("Choose a sync interval from 1m to 7d")
    }
    Ok(())
}

/// The wait after `failures` failures in a row: [`FIRST_BACKOFF`], doubled
/// for each further one, up to [`MAX_BACKOFF`].
pub fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    FIRST_BACKOFF
        .saturating_mul(1 << doublings)
        .min(MAX_BACKOFF)
}

/// `wait`, moved by up to [`JITTER`] of it either way: a `sample` of 0 is
/// the shortest, 0.5 leaves it, and one near 1 is the longest.
pub fn jitter(wait: Duration, sample: f64) -> Duration {
    let sample = if sample.is_finite() {
        sample.clamp(0.0, 1.0)
    } else {
        0.5
    };
    wait.mul_f64(1.0 + JITTER * (2.0 * sample - 1.0))
}

/// A random sample from 0 up to 1, for the jitter.
fn random_sample() -> f64 {
    // The low 53 bits of a random UUID, which its version and variant bits
    // leave alone: as many as an f64 holds exactly.
    const BITS: u32 = 53;
    let bits = uuid::Uuid::new_v4().as_u128() & ((1 << BITS) - 1);
    bits as f64 / (1u64 << BITS) as f64
}

/// Fill in how fresh each of `items` is: as fresh as its connection's last
/// success, from `connections` as [`Scheduler::annotate`] left them. An item
/// of a connection that isn't there is stale; a manual one has no sync.
pub fn freshness(connections: &[Connection], items: &mut [Item]) {
    for item in items {
        let Some(id) = item.connection_id.as_deref() else {
            item.synced_at = None;
            item.stale = false;
            continue;
        };
        let c = connections.iter().find(|c| c.id == id);
        item.synced_at = c.and_then(|c| c.sync.last_success);
        item.stale = c.is_none_or(|c| c.schedule.stale);
    }
}

/// Where the scheduler gets a connector for a provider, and the client it
/// reads a hub with.
pub trait Sources: Send + Sync {
    /// The connector for `provider`, built with its connection's
    /// `settings`: for the bot, reading where they say.
    fn connector(&self, provider: &str, settings: Settings) -> Result<Box<dyn Connector>>;
    /// The SHA-256 of the plugin that serves `provider`, as it would run
    /// now.
    fn plugin_sha256(&self, provider: &str) -> Result<String>;
    fn hub(&self) -> Result<Box<dyn Pull>>;
}

/// The built-in connectors and the data directory's plugins, sharing what
/// GitHub and Forgejo remember between syncs.
pub struct Builtin {
    pub plugins: PathBuf,
    pub shared: connectors::Shared,
}

impl Sources for Builtin {
    fn connector(&self, provider: &str, settings: Settings) -> Result<Box<dyn Connector>> {
        connectors::for_provider(provider, settings, &self.plugins, &self.shared)
    }
    fn plugin_sha256(&self, provider: &str) -> Result<String> {
        connectors::plugin_sha256(&self.plugins, provider)
    }
    fn hub(&self) -> Result<Box<dyn Pull>> {
        Ok(Box::new(hub::HubClient::new()?))
    }
}

/// A new connection, verified: the account its credential belongs to, and
/// its settings as they are to be stored.
pub struct Verified {
    pub account: Account,
    pub bot: Option<BotConfig>,
    pub plugin: Option<PluginConfig>,
}

/// Which plugin file a plugin connection trusts, and what its plugin
/// hashes to now, each a SHA-256 as `sha256sum` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginTrust {
    /// The connection's name.
    pub label: String,
    /// None for a connection made before plugins were pinned.
    pub trusted: Option<String>,
    pub current: String,
}

/// Fail unless `sha256` is a SHA-256 as `sha256sum` prints it.
fn check_sha256(sha256: &str) -> Result<()> {
    let hex = sha256.len() == 64 && sha256.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'));
    if !hex {
        bail!("Give the plugin's SHA-256 as `sha256sum` prints it: 64 lowercase hexadecimal digits")
    }
    Ok(())
}

/// What started a sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The schedule.
    Scheduled,
    /// A person, from the CLI, the API or the page.
    Explicit,
}

/// What watching another user found in a sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Watched {
    pub login: String,
    pub items: usize,
    pub error: Option<String>,
}

/// What a bot connection read of its heartbeat in a sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Agents {
    /// The agent runs it lists, which is none if no heartbeat is published.
    pub runs: usize,
    pub error: Option<String>,
}

/// What one sync of one connection did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub connection: String,
    pub label: String,
    /// What started it.
    pub trigger: Trigger,
    /// This asked for a sync while one was running, and got that one's
    /// result instead of starting a second.
    pub coalesced: bool,
    /// How many items it saved, if it succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watched: Option<Watched>,
    /// For a bot: its board as listed, and how many of its items this
    /// token can't read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board: Option<BoardSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Agents>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ErrorKind>,
    /// What the connector sent, for those that count, so a person can check
    /// that a sync only read.
    pub requests: Option<Requests>,
    /// When the next scheduled sync is due.
    pub next_due: Option<Timestamp>,
}

impl Report {
    fn new(connection: &str, label: &str, trigger: Trigger) -> Self {
        Self {
            connection: connection.into(),
            label: label.into(),
            trigger,
            coalesced: false,
            items: None,
            watched: None,
            board: None,
            agents: None,
            error: None,
            error_kind: None,
            requests: None,
            next_due: None,
        }
    }
    fn failed(mut self, error: &anyhow::Error) -> Self {
        let state = SyncState::default().failed(Timestamp::from_unix(0), error);
        self.error = state.error;
        self.error_kind = state.error_kind;
        self
    }
}

/// A sync started or joined, to wait for.
pub struct Pending {
    connection: String,
    label: String,
    trigger: Trigger,
    coalesced: bool,
    done: watch::Receiver<Option<Report>>,
}

impl Pending {
    /// What the sync did.
    pub async fn report(mut self) -> Report {
        let report = match self.done.wait_for(Option::is_some).await {
            Ok(report) => report.clone(),
            Err(_) => None,
        };
        match report {
            Some(report) => Report {
                coalesced: self.coalesced,
                ..report
            },
            // Its task ended without a report, as when the service stops.
            None => {
                Report::new(&self.connection, &self.label, self.trigger).failed(&anyhow::anyhow!(
                    "The sync was interrupted; nothing was saved, and it is due again"
                ))
            }
        }
    }
}

/// Keeps a connection marked running until its sync's task ends, however it
/// ends.
struct Running {
    scheduler: Arc<Scheduler>,
    connection: String,
}

impl Drop for Running {
    fn drop(&mut self) {
        lock(&self.scheduler.running).remove(&self.connection);
        self.scheduler.wake.notify_one();
    }
}

/// A std mutex's guard, whether or not a panic poisoned it: nothing it
/// guards is left half-changed.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs every connection's syncs: on schedule, and when a person asks.
pub struct Scheduler {
    db: Arc<Mutex<Db>>,
    sources: Box<dyn Sources>,
    clock: Arc<dyn Clock>,
    policy: Policy,
    /// Picks each jitter, from 0 to 1.
    sample: Box<dyn Fn() -> f64 + Send + Sync>,
    /// The syncs running now, by connection, for others to join.
    running: Mutex<HashMap<String, watch::Receiver<Option<Report>>>>,
    /// When each connection's last sync in this process started. One whose
    /// state couldn't be saved waits a backoff before it is tried again.
    attempts: Mutex<HashMap<String, Timestamp>>,
    /// One per connection, held by its sync and by changes to it, such as a
    /// new watch or a disconnect, so they never overlap.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Wakes the schedule to look again.
    wake: Notify,
}

impl Scheduler {
    pub fn new(
        db: Arc<Mutex<Db>>,
        sources: Box<dyn Sources>,
        clock: Arc<dyn Clock>,
        policy: Policy,
    ) -> Arc<Self> {
        Self::with_sample(db, sources, clock, policy, Box::new(random_sample))
    }
    fn with_sample(
        db: Arc<Mutex<Db>>,
        sources: Box<dyn Sources>,
        clock: Arc<dyn Clock>,
        policy: Policy,
        sample: Box<dyn Fn() -> f64 + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            sources,
            clock,
            policy,
            sample,
            running: Mutex::default(),
            attempts: Mutex::default(),
            locks: Mutex::default(),
            wake: Notify::new(),
        })
    }
    fn db(&self) -> MutexGuard<'_, Db> {
        lock(&self.db)
    }
    pub fn now(&self) -> Timestamp {
        Timestamp::of(self.clock.now())
    }
    /// Check a new connection before it is stored: whose account the
    /// credential in `target` belongs to, with the connector `input` asks
    /// for, built with its bot settings `bot`, already checked. Its
    /// settings come back as they are to be stored. A plugin is trusted as
    /// it is now: the file hashed here is the one that must run to verify
    /// the credential, and the one its later runs must be. A bot's
    /// numeric account ID is looked up by its login, to be kept.
    pub async fn verify_new(
        &self,
        input: &ConnectInput,
        bot: Option<BotConfig>,
        target: &Target,
    ) -> Result<Verified> {
        let plugin = if connectors::is_plugin(&input.provider) {
            let env = connectors::check_plugin_env(&input.plugin_env)?;
            let sha256 = self.sources.plugin_sha256(&input.provider)?;
            Some(PluginConfig {
                sha256: Some(sha256),
                env,
            })
        } else if !input.plugin_env.is_empty() {
            bail!("Only a source served by a connector plugin takes variables of the service's environment")
        } else {
            None
        };
        let settings = Settings {
            bot: bot.as_ref(),
            plugin: plugin.as_ref(),
        };
        let source = self.sources.connector(&input.provider, settings)?;
        let account = connector::verify(source.as_ref(), target).await?;
        let bot = match bot {
            Some(bot) => Some(connector::settle_bot(source.as_ref(), target, bot).await?),
            None => None,
        };
        Ok(Verified {
            account,
            bot,
            plugin,
        })
    }
    /// Which plugin file connection `id` trusts, and what its plugin hashes
    /// to now.
    pub fn plugin_trust(&self, id: &str) -> Result<PluginTrust> {
        let c = self.db().connection(id)?;
        if !connectors::is_plugin(&c.provider) {
            bail!(
                "{} uses a built-in connector, not a plugin, so it trusts no plugin file",
                c.label
            )
        }
        Ok(PluginTrust {
            label: c.label,
            trusted: c.plugin.and_then(|p| p.sha256),
            current: self.sources.plugin_sha256(&c.provider)?,
        })
    }
    /// Trust the plugin file of connection `id` whose SHA-256 is `sha256`,
    /// as the person checked it, from now on, and verify the connection
    /// with it, which brings back its scheduled syncs. Refused if the file
    /// changed again since.
    pub async fn trust_plugin(&self, id: &str, sha256: &str) -> Result<Connection> {
        check_sha256(sha256)?;
        {
            // Not while it syncs with the plugin trusted before.
            let _held = self.hold(id).await;
            let trust = self.plugin_trust(id)?;
            if trust.current != sha256 {
                bail!(
                    "The plugin changed again: its SHA-256 is now {}, not {sha256}. Check it, then trust it again",
                    trust.current
                )
            }
            let db = self.db();
            let mut c = db.connection(id)?;
            c.plugin.get_or_insert_with(PluginConfig::default).sha256 = Some(trust.current);
            db.save_connection(&c)?;
        }
        self.verify(id, None).await
    }
    /// Look at the schedule again, as after a connection was added or
    /// changed.
    pub fn wake(&self) {
        self.wake.notify_one();
    }
    /// Hold connection `id` against its syncs and other changes until the
    /// guard is dropped.
    pub async fn hold(&self, id: &str) -> OwnedMutexGuard<()> {
        let mutex = lock(&self.locks).entry(id.into()).or_default().clone();
        mutex.lock_owned().await
    }
    /// Drop what the scheduler kept about connection `id`, once it is
    /// disconnected.
    pub fn forget(&self, id: &str) {
        lock(&self.attempts).remove(id);
        lock(&self.locks).remove(id);
    }
    fn is_running(&self, id: &str) -> bool {
        lock(&self.running).contains_key(id)
    }
    /// Fill in how each connection is scheduled, and how fresh its items
    /// are.
    pub fn annotate(&self, connections: &mut [Connection]) {
        let now = self.now();
        for c in connections {
            c.schedule = Schedule {
                interval_secs: self.policy.interval(c).as_secs(),
                running: self.is_running(&c.id),
                needs_user: c.sync.error_kind == Some(ErrorKind::Auth),
                stale: self.policy.is_stale(c, now),
            };
        }
    }
    /// Fill in a snapshot's schedules and how fresh everything in it is, and
    /// in what watching found. A watch list is as fresh as the sync that
    /// found it, not its connection's last one: a watch can keep failing,
    /// and keep its list, while the account's own syncs succeed.
    pub fn annotate_snapshot(&self, snapshot: &mut Snapshot, watched: &mut [WatchList]) {
        self.annotate(&mut snapshot.connections);
        freshness(&snapshot.connections, &mut snapshot.items);
        let now = self.now();
        for list in watched {
            let c = snapshot
                .connections
                .iter()
                .find(|c| c.id == list.connection_id);
            let stale = c.is_none_or(|c| {
                list.last_success
                    .is_none_or(|last| now.since(last) > self.policy.stale_after(c))
            });
            for item in &mut list.items {
                item.synced_at = list.last_success;
                item.stale = stale;
            }
        }
    }
    /// Fill in how fresh what each bot connection read of its heartbeat is,
    /// from `connections` as [`Scheduler::annotate`] left them: as fresh as
    /// the sync that read it, since a heartbeat can keep failing while the
    /// board syncs. And whether the heartbeat itself is too old to trust.
    pub fn annotate_agents(&self, connections: &[Connection], agents: &mut [AgentRuns]) {
        let now = self.now();
        for a in agents {
            let c = connections.iter().find(|c| c.id == a.connection_id);
            a.stale = c.is_none_or(|c| {
                a.last_success
                    .is_none_or(|last| now.since(last) > self.policy.stale_after(c))
            });
            if let Some(heartbeat) = &mut a.heartbeat {
                heartbeat.stale = bot::heartbeat_stale(heartbeat, now);
            }
        }
    }
    /// When `c` is next due to sync on schedule: none while it runs or needs
    /// the person, and now if it never was scheduled.
    fn due_at(&self, c: &Connection, now: Timestamp) -> Option<Timestamp> {
        if c.sync.error_kind == Some(ErrorKind::Auth) || self.is_running(&c.id) {
            return None;
        }
        let due = c.sync.next_due.unwrap_or(now);
        // A sync whose state couldn't be saved left no backoff behind.
        let unsaved = lock(&self.attempts)
            .get(&c.id)
            .copied()
            .filter(|at| c.sync.last_attempt.is_none_or(|saved| saved < *at));
        Some(unsaved.map_or(due, |at| due.max(at.after(FIRST_BACKOFF))))
    }
    /// What the schedule runs: every connection's syncs, and the pulls from
    /// the hub, if one is set, which are scheduled the same way.
    fn jobs(&self) -> Result<Vec<Connection>> {
        let db = self.db();
        let mut jobs = db.connections()?;
        jobs.extend(db.hub()?.map(|link| link.as_job()));
        Ok(jobs)
    }
    /// The connections, and the hub, due to sync on schedule now.
    pub fn due(&self) -> Result<Vec<String>> {
        let now = self.now();
        Ok(self
            .jobs()?
            .into_iter()
            .filter(|c| self.due_at(c, now).is_some_and(|due| due <= now))
            .map(|c| c.id)
            .collect())
    }
    /// Start the syncs that are due.
    pub fn tick(self: &Arc<Self>) -> Result<Vec<Pending>> {
        Ok(self
            .due()?
            .iter()
            .map(|id| self.start(id, Trigger::Scheduled))
            .collect())
    }
    /// How long to sleep before the next sync is due, within bounds.
    fn sleep(&self) -> Duration {
        let now = self.now();
        let soonest = match self.jobs() {
            Ok(jobs) => jobs.iter().filter_map(|c| self.due_at(c, now)).min(),
            Err(_) => None,
        };
        soonest
            .map_or(MAX_SLEEP, |due| due.since(now))
            .clamp(MIN_SLEEP, MAX_SLEEP)
    }
    /// Sync every connection when it is due, for as long as the service
    /// runs.
    pub async fn serve(self: Arc<Self>) {
        loop {
            if let Err(e) = self.tick() {
                eprintln!("Cannot start scheduled syncs: {e:#}");
            }
            tokio::select! {
                () = tokio::time::sleep(self.sleep()) => (),
                () = self.wake.notified() => (),
            }
        }
    }
    /// Start a sync of connection `id`, or join the one running.
    pub fn start(self: &Arc<Self>, id: &str, trigger: Trigger) -> Pending {
        // For a report of a sync that never reports itself.
        let label = self
            .jobs()
            .ok()
            .and_then(|jobs| jobs.into_iter().find(|c| c.id == id))
            .map_or_else(|| id.to_string(), |c| c.label);
        let (send, done) = {
            let mut running = lock(&self.running);
            if let Some(done) = running.get(id) {
                return Pending {
                    connection: id.into(),
                    label,
                    trigger,
                    coalesced: true,
                    done: done.clone(),
                };
            }
            let (send, done) = watch::channel(None);
            running.insert(id.into(), done.clone());
            (send, done)
        };
        let marked = Running {
            scheduler: self.clone(),
            connection: id.into(),
        };
        let this = self.clone();
        let connection = id.to_string();
        tokio::spawn(async move {
            let report = this.run(&connection, trigger).await;
            // No longer running before anyone hears it ended, so a sync
            // asked for after that starts afresh.
            drop(marked);
            send.send_replace(Some(report));
        });
        Pending {
            connection: id.into(),
            label,
            trigger,
            coalesced: false,
            done,
        }
    }
    /// Sync connection `id` now, or join the sync running, and say what it
    /// did.
    pub async fn sync(self: &Arc<Self>, id: &str, trigger: Trigger) -> Report {
        self.start(id, trigger).report().await
    }
    /// Sync every connection now, side by side, joining those running.
    pub async fn sync_all(self: &Arc<Self>, trigger: Trigger) -> Result<Vec<Report>> {
        let pending: Vec<_> = self
            .jobs()?
            .iter()
            .map(|c| self.start(&c.id, trigger))
            .collect();
        let mut reports = Vec::new();
        for p in pending {
            reports.push(p.report().await);
        }
        Ok(reports)
    }
    /// One sync of connection or hub `id`, start to end.
    async fn run(&self, id: &str, trigger: Trigger) -> Report {
        let hub = self.db().hub();
        match hub {
            Ok(Some(link)) if link.id == id => self.pull(link, trigger).await,
            _ => self.sync_connection(id, trigger).await,
        }
    }
    /// One pull from the hub, start to end: what it read and the state of
    /// the pulls are saved together, or, if it failed, only the state. The
    /// last export read stays, and shows as stale.
    async fn pull(&self, link: hub::Link, trigger: Trigger) -> Report {
        let _held = self.hold(&link.id).await;
        let started = self.now();
        lock(&self.attempts).insert(link.id.clone(), started);
        let job = link.as_job();
        let mut report = Report::new(&link.id, hub::LABEL, trigger);
        let source = self.sources.hub();
        let result = async {
            let source = source.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
            let token = self.db().secret(&link.id)?;
            let token = Credential::new(&token)?;
            let url = hub::hub_url(&link.url)?;
            Ok::<_, anyhow::Error>(source.pull(&url, &token).await?)
        }
        .await;
        report.requests = source.ok().and_then(|s| s.requests());
        let finished = self.now();
        let sample = (self.sample)();
        let mut db = self.db();
        let (state, snapshot) = match result {
            Ok(snapshot) => {
                report.items = Some(snapshot.items.len());
                (link.sync.succeeded(started), Some(snapshot))
            }
            Err(e) => {
                let state = link.sync.failed(started, &e);
                report.error = state.error.clone();
                report.error_kind = state.error_kind;
                (state, None)
            }
        };
        let state = SyncState {
            next_due: self.policy.next_due(&job, &state, finished, sample),
            ..state
        };
        match db.save_hub_pull(&link.id, snapshot.as_ref(), &state) {
            Ok(()) => report.next_due = state.next_due,
            Err(e) => {
                report.items = None;
                report = report.failed(&anyhow::anyhow!("Cannot save what the pull read: {e:#}"));
            }
        }
        report
    }
    /// Pull from the hub at `url` with `token` from now on, in place of any
    /// hub before, once a first pull shows the address and the token work.
    /// That pull is kept, and the next is due an interval later.
    pub async fn set_hub(&self, url: &str, token: &str) -> Result<hub::Status> {
        let url = hub::hub_url(url)?;
        let token = Credential::new(token)?;
        let started = self.now();
        let client = self.sources.hub()?;
        let snapshot = client.pull(&url, &token).await?;
        let mut db = self.db();
        let old = db.hub()?.map(|link| link.id);
        let link = db.set_hub(url.as_str(), token.expose())?;
        let state = link.sync.succeeded(started);
        let state = SyncState {
            next_due: self
                .policy
                .next_due(&link.as_job(), &state, self.now(), (self.sample)()),
            ..state
        };
        db.save_hub_pull(&link.id, Some(&snapshot), &state)?;
        drop(db);
        if let Some(old) = old {
            self.forget(&old);
        }
        self.wake();
        self.hub_status()?.context("The hub was not saved")
    }
    /// Stop pulling from the hub, and forget its token and its work.
    pub async fn unset_hub(&self) -> Result<()> {
        let id = self.db().hub()?.map(|link| link.id);
        // Not while it pulls, which would save what it read afterwards.
        let _held = match &id {
            Some(id) => Some(self.hold(id).await),
            None => None,
        };
        self.db().unset_hub()?;
        if let Some(id) = id {
            self.forget(&id);
        }
        Ok(())
    }
    /// How the pulls from the hub are going, if one is set.
    pub fn hub_status(&self) -> Result<Option<hub::Status>> {
        Ok(self.hub_view()?.map(|(status, _)| status))
    }
    /// How the pulls from the hub are going, if one is set, and the last
    /// export they read. Its work is stale when the last pull is too old,
    /// as a connection's, and also as soon as a pull fails: the hub's work
    /// is never shown as current while the hub can't be reached.
    pub fn hub_view(&self) -> Result<Option<(hub::Status, Option<ExternalSnapshot>)>> {
        let Some(link) = self.db().hub()? else {
            return Ok(None);
        };
        let mut job = link.as_job();
        self.annotate(std::slice::from_mut(&mut job));
        let snapshot = link.snapshot;
        let status = hub::Status {
            url: link.url,
            schedule: Schedule {
                stale: job.schedule.stale || link.sync.error.is_some(),
                ..job.schedule
            },
            sync: link.sync,
            connections: snapshot.as_ref().map_or(0, |s| s.connections.len()),
            items: snapshot.as_ref().map_or(0, |s| s.items.len()),
        };
        Ok(Some((status, snapshot)))
    }
    /// One sync of connection `id`, start to end: what it found and its
    /// state are saved together once it is processed, or, if it failed,
    /// only its state.
    async fn sync_connection(&self, id: &str, trigger: Trigger) -> Report {
        let _held = self.hold(id).await;
        let started = self.now();
        lock(&self.attempts).insert(id.into(), started);
        let loaded = {
            let db = self.db();
            db.connection(id).and_then(|c| {
                let last = db.watched()?.into_iter().find(|w| w.connection_id == c.id);
                let runs = db.agents()?.into_iter().find(|a| a.connection_id == c.id);
                Ok((db.secret(&c.id), db.items()?, last, runs, c))
            })
        };
        let (token, old, last, runs, mut c) = match loaded {
            Ok(loaded) => loaded,
            // Its label is unknown too, so its ID stands in.
            Err(e) => return Report::new(id, id, trigger).failed(&e),
        };
        // A bot connection kept from before its bot's account ID was. When
        // its own account is the bot, as with the bot's own token, the ID is
        // the one its credential was verified as, which this sync checks
        // again; otherwise the sync looks it up by the bot's login.
        let unsettled = c.bot.as_ref().is_some_and(|bot| bot.bot_id.is_none());
        let own_id = c
            .bot
            .as_ref()
            .filter(|bot| unsettled && bot.bot_login.eq_ignore_ascii_case(&c.account_name))
            .and_then(|_| c.account_id.parse::<u64>().ok());
        if let (Some(own), Some(bot)) = (own_id, c.bot.as_mut()) {
            bot.bot_id = Some(own);
        }
        let mut report = Report::new(id, &c.label, trigger);
        let source = self.sources.connector(&c.provider, Settings::of(&c));
        let result = async {
            let source = source.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
            let target = Target::new(&c.base_url, &token?)?;
            let (synced, watched) = connector::sync_and_watch(
                source.as_ref(),
                &target,
                &c,
                &old,
                last.as_ref(),
                started,
            )
            .await?;
            let agents =
                connector::agents(source.as_ref(), &target, &c, runs.as_ref(), started).await;
            Ok::<_, anyhow::Error>((synced, watched, agents))
        }
        .await;
        let source = source.ok();
        report.requests = source.as_ref().and_then(|s| s.requests());
        let board = source.as_ref().and_then(|s| s.board());
        let finished = self.now();
        let sample = (self.sample)();
        let mut db = self.db();
        // The ID of a bot connection kept from before, as this sync found
        // it, kept from now on, whatever became of the rest of the sync.
        let settled = c.bot.as_ref().filter(|_| unsettled).and_then(|bot| {
            let id = own_id.or_else(|| source.as_ref()?.bot_id())?;
            Some(BotConfig {
                bot_id: Some(id),
                ..bot.clone()
            })
        });
        if let Some(bot) = settled {
            let login = bot.bot_login.clone();
            let id = bot.bot_id.unwrap_or_default();
            match db.save_connection(&Connection {
                bot: Some(bot),
                ..c.clone()
            }) {
                Ok(()) if own_id.is_some() => eprintln!(
                    "{}: the bot {login} is this connection's own account, GitHub account {id}, as its credential was verified; its heartbeat and pull requests are matched by that ID from now on",
                    c.label
                ),
                Ok(()) => eprintln!(
                    "warning: {}: the bot's login {login} names GitHub account {id} now, looked up at this sync rather than when the bot was connected. A person should confirm that account is the bot's (GitHub's `GET /user/{id}` names its login), or connect the bot again; its heartbeat and pull requests are matched by that ID from now on",
                    c.label
                ),
                Err(e) => eprintln!("{}: cannot keep the bot's account ID {id}: {e:#}", c.label),
            }
        }
        let saved = match result {
            Ok((synced, watched, agents)) => {
                let mut state = c.sync.succeeded(started);
                state.next_due = self.policy.next_due(&c, &state, finished, sample);
                report.items = Some(synced.items.len());
                report.watched = watched.as_ref().map(|w| Watched {
                    login: w.login.clone(),
                    items: w.items.len(),
                    error: w.error.clone(),
                });
                report.board = board;
                report.agents = agents.as_ref().map(|a| Agents {
                    runs: a.heartbeat.as_ref().map_or(0, |h| h.runs.len()),
                    error: a.error.clone(),
                });
                let found = crate::db::Found {
                    items: &synced.items,
                    watched: watched.as_ref(),
                    agents: agents.as_ref(),
                    dropped: &synced.dropped,
                };
                db.save_found(&c.id, &found, &state).map(|()| state)
            }
            Err(e) => {
                let mut state = c.sync.failed(started, &e);
                state.next_due = self.policy.next_due(&c, &state, finished, sample);
                report.error = state.error.clone();
                report.error_kind = state.error_kind;
                db.save_sync_state(&c.id, &state).map(|()| state)
            }
        };
        match saved {
            Ok(state) => report.next_due = state.next_due,
            Err(e) => {
                report.items = None;
                report.watched = None;
                report.board = None;
                report.agents = None;
                report = report.failed(&anyhow::anyhow!("Cannot save what the sync found: {e:#}"));
            }
        }
        report
    }
    /// Check connection `id`'s credential again, or a new one for the same
    /// account, which replaces it, and schedule a sync now. This is how a
    /// connection whose credential was rejected gets its scheduled syncs
    /// back once the person fixed it.
    pub async fn verify(&self, id: &str, token: Option<&str>) -> Result<Connection> {
        let _held = self.hold(id).await;
        let (c, stored) = {
            let db = self.db();
            let c = db.connection(id)?;
            let stored = db.secret(id);
            (c, stored)
        };
        let target = Target::new(
            &c.base_url,
            &match token {
                Some(token) => token.to_string(),
                None => stored?,
            },
        )?;
        let source = self.sources.connector(&c.provider, Settings::of(&c))?;
        let account = connector::verify(source.as_ref(), &target).await?;
        if account.id != c.account_id {
            bail!(
                "That credential belongs to {}, not {}; connect it as a separate connection",
                account.name,
                c.account_name
            )
        }
        // The bot's login still names the account kept, or one is kept now.
        let bot = match &c.bot {
            Some(bot) => Some(connector::settle_bot(source.as_ref(), &target, bot.clone()).await?),
            None => None,
        };
        let state = SyncState {
            next_due: Some(self.now()),
            failures: 0,
            error_kind: None,
            error: None,
            held_until: None,
            ..c.sync.clone()
        };
        let mut db = self.db();
        // A new credential is staged first and put in place only once the
        // state is saved. A crash in between leaves the old credential with
        // the schedule back, which fails as before and stops it again, never
        // the new credential with the schedule stopped; verifying again
        // recovers either way. The connection's lock keeps its syncs out
        // until the credential is in place.
        if bot != c.bot {
            db.save_connection(&Connection { bot, ..c.clone() })?;
        }
        let staged = token.is_some();
        if staged {
            db.stage_secret(id, target.credential.expose())?;
        }
        let installed = db.save_sync_state(id, &state).and_then(|()| {
            if staged {
                db.install_secret(id)
            } else {
                Ok(())
            }
        });
        if let Err(e) = installed {
            // Never leave the new credential behind; and say so if it
            // stays, which the next start sorts out.
            if staged {
                if let Err(discarded) = db.discard_staged_secret(id) {
                    bail!(
                        "{e:#}; and the new credential wasn't dropped, which the next start of the service does: {discarded:#}"
                    );
                }
            }
            return Err(e);
        }
        let mut c = db.connection(id)?;
        drop(db);
        self.annotate(std::slice::from_mut(&mut c));
        self.wake();
        Ok(c)
    }
    /// Let the connector plugin of connection `id` get the variables named
    /// in `env` of the service's environment, and no others, from its next
    /// run on.
    pub async fn set_plugin_env(&self, id: &str, env: &[String]) -> Result<Connection> {
        let env = connectors::check_plugin_env(env)?;
        let _held = self.hold(id).await;
        let db = self.db();
        let mut c = db.connection(id)?;
        if !connectors::is_plugin(&c.provider) {
            bail!(
                "{} uses a built-in connector, which gets no variables of the service's environment",
                c.label
            )
        }
        c.plugin.get_or_insert_with(PluginConfig::default).env = env;
        db.save_connection(&c)?;
        drop(db);
        self.annotate(std::slice::from_mut(&mut c));
        Ok(c)
    }
    /// Give connection `id` its own sync interval, or with `None` the
    /// service's. A shorter one takes effect now, a longer one after the
    /// sync already due.
    pub async fn set_interval(&self, id: &str, interval: Option<Duration>) -> Result<Connection> {
        if let Some(interval) = interval {
            check_interval(interval)?;
        }
        let _held = self.hold(id).await;
        let mut db = self.db();
        let mut c = db.connection(id)?;
        c.sync_interval = interval.map(|i| i.as_secs());
        db.save_connection(&c)?;
        if c.sync.error_kind.is_none() {
            let sooner = self
                .policy
                .next_due(&c, &c.sync, self.now(), (self.sample)());
            // One never scheduled is due now anyway.
            let state = SyncState {
                next_due: c
                    .sync
                    .next_due
                    .map(|due| sooner.map_or(due, |sooner| due.min(sooner))),
                ..c.sync.clone()
            };
            db.save_sync_state(id, &state)?;
            c.sync = state;
        }
        drop(db);
        self.annotate(std::slice::from_mut(&mut c));
        self.wake();
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        clock::ManualClock,
        connector::{
            Account, BoxFuture, ConnectorError, ConnectorResult, Reason, Refreshed, RemoteItem,
            Tracked,
        },
        db,
        model::{Item, Outcome, Zone},
    };
    use std::collections::VecDeque;
    use tokio::sync::Semaphore;

    const TOKEN: &str = "test-only-token";
    /// A token that belongs to another account.
    const OTHER_ACCOUNT: &str = "other-account-token";
    const MINUTE: Duration = Duration::from_secs(60);

    /// The sources, as the fake connectors see them: what each instance
    /// answers to `list`, and how often it was asked.
    #[derive(Default)]
    struct World {
        /// What `list` answers next, by instance host: a number of items,
        /// or an error. Two items when nothing is queued.
        answers: Mutex<HashMap<String, VecDeque<ConnectorResult<usize>>>>,
        lists: Mutex<HashMap<String, usize>>,
        /// Hosts whose `list` waits for a permit.
        gates: Mutex<HashMap<String, Arc<Semaphore>>>,
        /// Told when a gated `list` starts waiting.
        entered: Notify,
        /// The account a bot's login names, if not 7.
        bot_account: Mutex<Option<u64>>,
    }

    impl World {
        fn answer(&self, host: &str, answer: ConnectorResult<usize>) {
            lock(&self.answers)
                .entry(host.into())
                .or_default()
                .push_back(answer);
        }
        fn lists(&self, host: &str) -> usize {
            lock(&self.lists).get(host).copied().unwrap_or_default()
        }
        fn gate(&self, host: &str) -> Arc<Semaphore> {
            lock(&self.gates)
                .entry(host.into())
                .or_insert_with(|| Arc::new(Semaphore::new(0)))
                .clone()
        }
    }

    struct Fake(Arc<World>);

    /// What a host answers to `watch` is queued under its name and this.
    const WATCH: &str = "#watch";

    /// `count` open items of `host`, their IDs starting with `prefix`.
    fn remote_items(host: &str, prefix: &str, count: usize) -> Vec<RemoteItem> {
        (0..count)
            .map(|n| RemoteItem {
                remote_id: format!("{prefix}{n}"),
                key: format!("{host}-{prefix}{n}"),
                title: format!("Item {prefix}{n} of {host}"),
                url: format!("https://{host}/{prefix}{n}"),
                status: "open".into(),
                outcome: Outcome::Open,
                outcome_reason: "Open".into(),
                action: "next".into(),
                updated_at: "2026-10-12T00:00:00Z".into(),
                reasons: if prefix.is_empty() {
                    vec![]
                } else {
                    vec![Reason::Assigned]
                },
                bot: None,
            })
            .collect()
    }

    fn host(target: &Target) -> String {
        target.base_url.host_str().unwrap_or_default().into()
    }

    impl Connector for Fake {
        fn kind(&self) -> &str {
            "fake"
        }
        fn verify<'a>(&'a self, target: &'a Target) -> BoxFuture<'a, ConnectorResult<Account>> {
            let id = if target.credential.expose() == OTHER_ACCOUNT {
                "43"
            } else {
                "42"
            };
            Box::pin(async move {
                Ok(Account {
                    id: id.into(),
                    name: format!("user{id}"),
                })
            })
        }
        fn list<'a>(
            &'a self,
            target: &'a Target,
            _: &'a Account,
        ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
            Box::pin(async move {
                let host = host(target);
                *lock(&self.0.lists).entry(host.clone()).or_default() += 1;
                let gate = lock(&self.0.gates).get(&host).cloned();
                if let Some(gate) = gate {
                    self.0.entered.notify_one();
                    gate.acquire().await.unwrap().forget();
                }
                let answer = lock(&self.0.answers)
                    .get_mut(&host)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or(Ok(2));
                Ok(remote_items(&host, "", answer?))
            })
        }
        fn watch<'a>(
            &'a self,
            target: &'a Target,
            _: &'a str,
        ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
            Box::pin(async move {
                let host = host(target);
                let answer = lock(&self.0.answers)
                    .get_mut(&format!("{host}{WATCH}"))
                    .and_then(VecDeque::pop_front)
                    .unwrap_or(Ok(1));
                Ok(remote_items(&host, "w", answer?))
            })
        }
        fn refresh<'a>(
            &'a self,
            _: &'a Target,
            _: &'a Account,
            tracked: &'a [Tracked],
        ) -> BoxFuture<'a, ConnectorResult<Vec<Refreshed>>> {
            Box::pin(async move {
                Ok(tracked
                    .iter()
                    .map(|t| Refreshed::inaccessible(&t.remote_id, "Gone"))
                    .collect())
            })
        }
        fn look_up_bot_id<'a>(
            &'a self,
            _: &'a Target,
        ) -> Option<BoxFuture<'a, ConnectorResult<u64>>> {
            let id = self.bot_id();
            Some(Box::pin(async move { Ok(id.unwrap_or_default()) }))
        }
        fn bot_id(&self) -> Option<u64> {
            Some(lock(&self.0.bot_account).unwrap_or(7))
        }
    }

    /// A hub answers as many items as queued under its host, two unless
    /// something is queued, and refuses any token but [`TOKEN`].
    impl Pull for Fake {
        fn pull<'a>(
            &'a self,
            base: &'a url::Url,
            token: &'a Credential,
        ) -> BoxFuture<'a, ConnectorResult<crate::zone::ExternalSnapshot>> {
            Box::pin(async move {
                let host = base.host_str().unwrap_or_default().to_string();
                *lock(&self.0.lists).entry(host.clone()).or_default() += 1;
                if token.expose() != TOKEN {
                    return Err(ConnectorError::new(ErrorKind::Auth, "Refused"));
                }
                let answer = lock(&self.0.answers)
                    .get_mut(&host)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or(Ok(2));
                Ok(hub::tests::export(answer?))
            })
        }
        fn requests(&self) -> Option<Requests> {
            None
        }
    }

    struct FakeSources(Arc<World>);

    impl Sources for FakeSources {
        fn connector(&self, _: &str, _: Settings) -> Result<Box<dyn Connector>> {
            Ok(Box::new(Fake(self.0.clone())))
        }
        fn plugin_sha256(&self, _: &str) -> Result<String> {
            bail!("The fake sources have no plugins")
        }
        fn hub(&self) -> Result<Box<dyn Pull>> {
            Ok(Box::new(Fake(self.0.clone())))
        }
    }

    /// When the tests start: 2026-10-12T00:00:00Z.
    fn t0() -> Timestamp {
        Timestamp::from_unix(1_791_763_200)
    }
    fn at(minutes: u64) -> Timestamp {
        t0().after(MINUTE * u32::try_from(minutes).unwrap())
    }

    /// A data directory with a connection to each host, and a scheduler
    /// over it, its clock at [`t0`] and its jitter none.
    struct Harness {
        temp: tempfile::TempDir,
        world: Arc<World>,
        clock: Arc<ManualClock>,
        db: Arc<Mutex<Db>>,
        scheduler: Arc<Scheduler>,
        /// Connection IDs, by host.
        ids: HashMap<String, String>,
    }

    impl Harness {
        fn new(hosts: &[(&str, Option<Duration>)]) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let db = Db::open(temp.path()).unwrap();
            let mut ids = HashMap::new();
            for (n, (host, interval)) in hosts.iter().enumerate() {
                let c = Connection {
                    id: format!("c0ffee00-0000-4000-8000-{n:012}"),
                    provider: "errata".into(),
                    label: (*host).into(),
                    base_url: format!("https://{host}/"),
                    account_id: "42".into(),
                    account_name: "user42".into(),
                    zone: Zone::External,
                    sync_interval: interval.map(|i| i.as_secs()),
                    ..Connection::default()
                };
                db.save_connection(&c).unwrap();
                db::write_secret(&temp.path().join("secrets").join(&c.id), TOKEN).unwrap();
                ids.insert((*host).to_string(), c.id);
            }
            let world = Arc::new(World::default());
            let clock = Arc::new(ManualClock::at(t0()));
            let db = Arc::new(Mutex::new(db));
            let scheduler = Self::scheduler(&db, &world, &clock, || 0.5);
            Self {
                temp,
                world,
                clock,
                db,
                scheduler,
                ids,
            }
        }
        fn scheduler(
            db: &Arc<Mutex<Db>>,
            world: &Arc<World>,
            clock: &Arc<ManualClock>,
            sample: impl Fn() -> f64 + Send + Sync + 'static,
        ) -> Arc<Scheduler> {
            Scheduler::with_sample(
                db.clone(),
                Box::new(FakeSources(world.clone())),
                clock.clone(),
                Policy::default(),
                Box::new(sample),
            )
        }
        fn id(&self, host: &str) -> &str {
            &self.ids[host]
        }
        fn connection(&self, host: &str) -> Connection {
            let mut c = lock(&self.db).connection(self.id(host)).unwrap();
            self.scheduler.annotate(std::slice::from_mut(&mut c));
            c
        }
        fn state(&self, host: &str) -> SyncState {
            self.connection(host).sync
        }
        /// The hosts due now.
        fn due(&self) -> Vec<String> {
            let due = self.scheduler.due().unwrap();
            let mut hosts: Vec<_> = self
                .ids
                .iter()
                .filter(|(_, id)| due.contains(id))
                .map(|(host, _)| host.clone())
                .collect();
            hosts.sort();
            hosts
        }
        /// Run what is due now to the end, and say what it did.
        async fn tick(&self) -> Vec<Report> {
            let mut reports = Vec::new();
            for pending in self.scheduler.tick().unwrap() {
                reports.push(pending.report().await);
            }
            reports.sort_by(|a, b| a.label.cmp(&b.label));
            reports
        }
        /// Move the clock to `t`.
        fn set(&self, t: Timestamp) {
            let now = Timestamp::of(self.clock.now());
            assert!(t >= now, "the clock only moves forward");
            self.clock.advance(t.since(now));
        }
        fn items(&self, host: &str) -> Vec<String> {
            let id = self.id(host);
            lock(&self.db)
                .items()
                .unwrap()
                .into_iter()
                .filter(|i| i.connection_id.as_deref() == Some(id))
                .map(|i: Item| format!("{} {:?}", i.key, i.outcome))
                .collect()
        }
    }

    /// `state`, with its next sync due at `t`.
    fn due_at(state: SyncState, t: Timestamp) -> SyncState {
        SyncState {
            next_due: Some(t),
            ..state
        }
    }
    /// A second before `t`.
    fn before(t: Timestamp) -> Timestamp {
        Timestamp::from_unix(t.unix() - 1)
    }

    fn transient() -> ConnectorError {
        ConnectorError::new(ErrorKind::Transient, "Network down")
    }

    #[test]
    fn backoff_never_passes_its_cap_even_with_jitter() {
        let c = Connection::default();
        let failing = SyncState {
            failures: 20,
            error_kind: Some(ErrorKind::Transient),
            ..SyncState::default()
        };
        for (sample, wait) in [
            (0.0, MAX_BACKOFF.mul_f64(0.9)),
            (0.5, MAX_BACKOFF),
            (1.0, MAX_BACKOFF),
        ] {
            let due = Policy::default().next_due(&c, &failing, t0(), sample);
            assert_eq!(due, Some(t0().after(wait)), "{sample}");
        }
    }

    #[test]
    fn waits_are_jittered_within_bounds() {
        let wait = 5 * MINUTE;
        let (shortest, longest) = (wait.mul_f64(0.9), wait.mul_f64(1.1));
        assert_eq!(jitter(wait, 0.0), shortest);
        assert_eq!(jitter(wait, 0.5), wait);
        assert!(jitter(wait, 0.999_999) < longest);
        // Out of range samples are kept in it.
        assert_eq!(jitter(wait, -1.0), shortest);
        assert_eq!(jitter(wait, 2.0), longest);
        assert_eq!(jitter(wait, f64::NAN), wait);
        let samples: Vec<_> = (0..2000).map(|_| random_sample()).collect();
        assert!(samples.iter().all(|s| (0.0..1.0).contains(s)));
        // Not all on one side, as a stuck generator would be.
        assert!(samples.iter().any(|s| *s < 0.25) && samples.iter().any(|s| *s > 0.75));
        for sample in samples {
            let jittered = jitter(wait, sample);
            assert!(jittered >= shortest && jittered <= longest, "{sample}");
        }
    }

    #[tokio::test]
    async fn each_connection_syncs_on_its_own_schedule() {
        let h = Harness::new(&[("a.example", None), ("b.example", Some(10 * MINUTE))]);
        // Never synced: both are due at once.
        assert_eq!(h.due(), ["a.example", "b.example"]);
        let reports = h.tick().await;
        assert!(reports
            .iter()
            .all(|r| r.error.is_none() && r.items == Some(2)));
        assert!(reports
            .iter()
            .all(|r| r.trigger == Trigger::Scheduled && !r.coalesced));
        let a = h.connection("a.example");
        assert_eq!(a.sync, due_at(SyncState::default().succeeded(t0()), at(5)));
        assert_eq!(a.schedule.interval_secs, 300);
        assert_eq!(h.state("b.example").next_due, Some(at(10)));
        assert_eq!(h.connection("b.example").schedule.interval_secs, 600);
        assert_eq!(
            h.items("a.example"),
            ["a.example-0 Open", "a.example-1 Open"]
        );
        // Nothing until the first is due, then only it.
        h.set(at(4));
        assert!(h.due().is_empty());
        assert!(h.tick().await.is_empty());
        h.set(at(5));
        assert_eq!(h.due(), ["a.example"]);
        h.tick().await;
        assert_eq!(h.state("a.example").next_due, Some(at(10)));
        h.set(at(10));
        assert_eq!(h.due(), ["a.example", "b.example"]);
        assert_eq!(
            (h.world.lists("a.example"), h.world.lists("b.example")),
            (2, 1)
        );
    }

    #[tokio::test]
    async fn the_next_sync_is_jittered() {
        let h = Harness::new(&[("a.example", None)]);
        for (sample, due) in [(0.0, 270), (0.5, 300), (1.0, 330)] {
            let scheduler = Harness::scheduler(&h.db, &h.world, &h.clock, move || sample);
            let report = scheduler.sync(h.id("a.example"), Trigger::Explicit).await;
            let now = Timestamp::of(h.clock.now());
            assert_eq!(report.next_due, Some(now.after(Duration::from_secs(due))));
        }
    }

    #[tokio::test]
    async fn failures_back_off_exponentially_up_to_a_cap() {
        let h = Harness::new(&[("a.example", None)]);
        let mut now = t0();
        for (failures, minutes) in [
            (1, 1),
            (2, 2),
            (3, 4),
            (4, 8),
            (5, 16),
            (6, 32),
            (7, 60),
            (8, 60),
        ] {
            h.world.answer("a.example", Err(transient()));
            h.set(now);
            assert_eq!(h.due(), ["a.example"]);
            let [report] = h.tick().await.try_into().unwrap();
            assert_eq!(report.error_kind, Some(ErrorKind::Transient));
            let state = h.state("a.example");
            assert_eq!(state.failures, failures);
            assert_eq!(state.last_success, None);
            assert_eq!(state.last_attempt, Some(now));
            assert_eq!(state.error.as_deref(), Some("Network down"));
            now = now.after(MINUTE * minutes);
            assert_eq!(state.next_due, Some(now), "after {failures} failures");
            assert_eq!(report.next_due, Some(now));
            h.set(before(now));
            assert!(h.due().is_empty(), "not before");
        }
        // A success starts over.
        h.set(now);
        h.tick().await;
        let state = h.state("a.example");
        assert_eq!(
            state,
            due_at(SyncState::default().succeeded(now), now.after(5 * MINUTE))
        );
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
        assert_eq!(backoff(0), FIRST_BACKOFF);
    }

    #[tokio::test]
    async fn a_rate_limit_hold_defers_the_next_sync_until_it_ends() {
        let h = Harness::new(&[("a.example", None)]);
        let held = |minutes| {
            let until = std::time::UNIX_EPOCH + Duration::from_secs(at(minutes).unix() as u64);
            ConnectorError::new(ErrorKind::Transient, "Rate limited").held_until(until)
        };
        h.world.answer("a.example", Err(held(20)));
        h.tick().await;
        let state = h.state("a.example");
        assert_eq!(
            (state.held_until, state.next_due),
            (Some(at(20)), Some(at(20)))
        );
        h.set(before(at(20)));
        assert!(h.due().is_empty());
        h.set(at(20));
        assert_eq!(h.due(), ["a.example"]);
        // A hold shorter than the backoff waits the backoff.
        h.world.answer("a.example", Err(held(20)));
        h.tick().await;
        assert_eq!(h.state("a.example").next_due, Some(at(22)));
        assert_eq!(h.world.lists("a.example"), 2);
    }

    #[tokio::test]
    async fn a_bot_kept_without_its_account_id_gets_it_at_its_next_sync() {
        let h = Harness::new(&[("a.example", None), ("b.example", None)]);
        let id = h.id("a.example");
        let bot = |id: Option<u64>| BotConfig {
            bot_id: id,
            ..bot::tests::config()
        };
        {
            let db = lock(&h.db);
            let mut c = db.connection(id).unwrap();
            c.bot = Some(bot(None));
            db.save_connection(&c).unwrap();
        }
        let kept = || lock(&h.db).connection(id).unwrap().bot;
        let report = h.scheduler.sync(id, Trigger::Explicit).await;
        assert_eq!(report.error, None);
        assert_eq!(kept(), Some(bot(Some(7))));
        // When the connection's own account is the bot, that is the ID,
        // as its credential is verified, with nothing looked up.
        let other = h.id("b.example");
        {
            let db = lock(&h.db);
            let mut c = db.connection(other).unwrap();
            c.account_name = bot(None).bot_login;
            c.bot = Some(bot(None));
            db.save_connection(&c).unwrap();
        }
        let report = h.scheduler.sync(other, Trigger::Explicit).await;
        assert_eq!(report.error, None);
        let own = lock(&h.db).connection(other).unwrap().bot;
        assert_eq!(own, Some(bot(Some(42))));
        // Verifying looks it up again, and refuses a login that names
        // another account now, keeping the one it had.
        *lock(&h.world.bot_account) = Some(8);
        let error = h.scheduler.verify(id, None).await.unwrap_err();
        assert!(format!("{error:#}").contains("not account 7"), "{error:#}");
        assert_eq!(kept(), Some(bot(Some(7))));
        *lock(&h.world.bot_account) = None;
        h.scheduler.verify(id, None).await.unwrap();
        assert_eq!(kept(), Some(bot(Some(7))));
    }

    #[tokio::test]
    async fn a_rejected_credential_stops_scheduled_syncs_until_verified() {
        let h = Harness::new(&[("a.example", None), ("b.example", None)]);
        let denied = ConnectorError::new(ErrorKind::Auth, "Credential rejected");
        h.world.answer("a.example", Err(denied.clone()));
        h.tick().await;
        let a = h.connection("a.example");
        assert_eq!(a.sync.error_kind, Some(ErrorKind::Auth));
        assert_eq!(a.sync.next_due, None);
        assert!(a.schedule.needs_user);
        assert!(!h.connection("b.example").schedule.needs_user);
        // A day later, only the other one ever synced again.
        for minutes in (5..=24 * 60).step_by(5) {
            h.set(at(minutes));
            assert!(!h.due().contains(&"a.example".to_string()));
            h.tick().await;
        }
        assert_eq!(h.world.lists("a.example"), 1);
        assert!(h.world.lists("b.example") > 200);
        // Asked for, it syncs, and stays stopped while it fails.
        h.world.answer("a.example", Err(denied));
        let report = h.scheduler.sync(h.id("a.example"), Trigger::Explicit).await;
        assert_eq!(report.error_kind, Some(ErrorKind::Auth));
        assert!(h.connection("a.example").schedule.needs_user);
        // A credential of another account is refused, and changes nothing.
        let error = h
            .scheduler
            .verify(h.id("a.example"), Some(OTHER_ACCOUNT))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("separate connection"), "{error}");
        assert!(h.connection("a.example").schedule.needs_user);
        // Verified again with a new credential for the account, it is due
        // now, and syncs on schedule again.
        let c = h
            .scheduler
            .verify(h.id("a.example"), Some("new-token"))
            .await
            .unwrap();
        assert!(!c.schedule.needs_user);
        assert_eq!((c.sync.error, c.sync.failures), (None, 0));
        let secret = h.temp.path().join("secrets").join(h.id("a.example"));
        assert_eq!(std::fs::read_to_string(secret).unwrap(), "new-token");
        assert!(h.due().contains(&"a.example".to_string()));
        h.tick().await;
        assert_eq!(h.state("a.example").error, None);
        // Verifying the stored credential works the same.
        let c = h.scheduler.verify(h.id("a.example"), None).await.unwrap();
        assert_eq!(c.sync.next_due, Some(Timestamp::of(h.clock.now())));
    }

    #[tokio::test]
    async fn a_failed_verify_leaves_no_staged_credential() {
        let h = Harness::new(&[("a.example", None)]);
        let id = h.id("a.example");
        let secrets = h.temp.path().join("secrets");
        let staged = secrets.join(format!("{id}.new"));
        // Where verify fails: saving the state, or putting the new
        // credential in place.
        let fail_state = |db: &Db| {
            db.sql()
                .execute_batch("CREATE TRIGGER fail BEFORE INSERT ON sync_state BEGIN SELECT RAISE(ABORT, 'disk full'); END")
                .unwrap()
        };
        let fail_install = |_: &Db| {
            let secret = secrets.join(id);
            std::fs::remove_file(&secret).unwrap();
            std::fs::create_dir(&secret).unwrap();
            std::fs::write(secret.join("in-the-way"), "").unwrap();
        };
        let breaks: [&dyn Fn(&Db); 2] = [&fail_state, &fail_install];
        for (n, fail) in breaks.into_iter().enumerate() {
            fail(&lock(&h.db));
            assert!(
                h.scheduler.verify(id, Some("new-token")).await.is_err(),
                "case {n}"
            );
            assert!(!staged.exists(), "case {n}");
            // Repaired for the next case.
            lock(&h.db)
                .sql()
                .execute_batch("DROP TRIGGER IF EXISTS fail")
                .unwrap();
            let secret = secrets.join(id);
            if secret.is_dir() {
                std::fs::remove_dir_all(&secret).unwrap();
                db::write_secret(&secret, TOKEN).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn a_connection_never_syncs_alongside_itself_and_others_go_on() {
        let h = Harness::new(&[("a.example", None), ("b.example", None)]);
        let gate = h.world.gate("a.example");
        let first = h.scheduler.start(h.id("a.example"), Trigger::Scheduled);
        h.world.entered.notified().await;
        assert!(h.connection("a.example").schedule.running);
        // The schedule doesn't start it again, and the other one syncs
        // while it is stuck.
        assert_eq!(h.due(), ["b.example"]);
        let [b] = h.tick().await.try_into().unwrap();
        assert_eq!((b.label.as_str(), b.items), ("b.example", Some(2)));
        // Asked for, it joins the sync running, as does a sync of all.
        let joined = h.scheduler.start(h.id("a.example"), Trigger::Explicit);
        let all = {
            let scheduler = h.scheduler.clone();
            tokio::spawn(async move { scheduler.sync_all(Trigger::Explicit).await })
        };
        tokio::task::yield_now().await;
        gate.add_permits(1);
        let (first, joined) = (first.report().await, joined.report().await);
        let all = all.await.unwrap().unwrap();
        assert!(!first.coalesced && joined.coalesced, "{joined:?}");
        assert_eq!(
            Report {
                coalesced: false,
                ..joined
            },
            first
        );
        assert_eq!(first.trigger, Trigger::Scheduled);
        let labels: Vec<_> = all
            .iter()
            .map(|r| (r.label.as_str(), r.coalesced))
            .collect();
        assert_eq!(labels, [("a.example", true), ("b.example", false)]);
        assert_eq!(h.world.lists("a.example"), 1, "one sync of it");
        assert!(!h.connection("a.example").schedule.running);
        // Once it ended, a new one starts.
        gate.add_permits(1);
        let again = h.scheduler.sync(h.id("a.example"), Trigger::Explicit).await;
        assert!(!again.coalesced && again.trigger == Trigger::Explicit);
        assert_eq!(h.world.lists("a.example"), 2);
    }

    #[test]
    fn a_sync_stopped_before_it_saved_leaves_the_last_good_data_and_is_due_again() {
        let h = Harness::new(&[("a.example", None)]);
        let runtime = || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        };
        runtime().block_on(h.tick());
        let saved = h.state("a.example");
        assert_eq!(h.items("a.example").len(), 2);
        // The next sync, on schedule, finds three items, but the service
        // stops while it is still listing them: as in a crash, its task
        // ends at that point, and nothing after it runs.
        h.set(at(5));
        h.world.answer("a.example", Ok(3));
        let gate = h.world.gate("a.example");
        let stopped = runtime();
        stopped.block_on(async {
            let pending = h.scheduler.tick().unwrap();
            assert_eq!(pending.len(), 1);
            h.world.entered.notified().await;
        });
        drop(stopped);
        drop(h.scheduler);
        // Started again on the same data directory.
        let db = Arc::new(Mutex::new(Db::open(h.temp.path()).unwrap()));
        let h = Harness {
            scheduler: Harness::scheduler(&db, &h.world, &h.clock, || 0.5),
            db,
            ..h
        };
        assert_eq!(h.state("a.example"), saved);
        assert_eq!(
            h.items("a.example"),
            ["a.example-0 Open", "a.example-1 Open"]
        );
        // It wasn't skipped: it is due, and syncs what it missed.
        assert_eq!(h.due(), ["a.example"]);
        gate.add_permits(1);
        let [report] = runtime().block_on(h.tick()).try_into().unwrap();
        assert_eq!(report.items, Some(3));
        assert_eq!(h.items("a.example").len(), 3);
        assert_eq!(h.state("a.example").last_success, Some(at(5)));
    }

    #[tokio::test]
    async fn a_sync_that_saved_nothing_waits_before_it_is_tried_again() {
        let h = Harness::new(&[("a.example", None)]);
        h.tick().await;
        h.set(at(5));
        // A sync started now couldn't save its state, as on a full disk, so
        // the state still says it is due.
        lock(&h.scheduler.attempts).insert(h.id("a.example").into(), at(5));
        assert!(h.due().is_empty());
        h.set(before(at(6)));
        assert!(h.due().is_empty());
        h.set(at(6));
        assert_eq!(h.due(), ["a.example"]);
        // One disconnected from under it says so, and isn't left running.
        lock(&h.db)
            .sql()
            .execute("DELETE FROM connections", [])
            .unwrap();
        let report = h.scheduler.sync(h.id("a.example"), Trigger::Explicit).await;
        assert!(report.error.unwrap().contains("not found"));
        assert!(h.due().is_empty());
        assert!(!h.scheduler.is_running(h.id("a.example")));
    }

    #[tokio::test]
    async fn a_shorter_interval_takes_effect_now() {
        let h = Harness::new(&[("a.example", None)]);
        // Never synced: it stays due now.
        let c = h
            .scheduler
            .set_interval(h.id("a.example"), Some(30 * MINUTE))
            .await
            .unwrap();
        assert_eq!((c.sync.next_due, c.schedule.interval_secs), (None, 1800));
        h.tick().await;
        assert_eq!(h.state("a.example").next_due, Some(at(30)));
        let c = h
            .scheduler
            .set_interval(h.id("a.example"), Some(10 * MINUTE))
            .await
            .unwrap();
        assert_eq!(c.sync.next_due, Some(at(10)));
        // A longer one after the sync already due.
        let c = h
            .scheduler
            .set_interval(h.id("a.example"), None)
            .await
            .unwrap();
        assert_eq!((c.sync.next_due, c.sync_interval), (Some(at(5)), None));
        for wrong in [Duration::from_secs(59), MAX_INTERVAL + MINUTE] {
            assert!(h
                .scheduler
                .set_interval(h.id("a.example"), Some(wrong))
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn items_are_as_fresh_as_their_connections_last_success() {
        let h = Harness::new(&[("a.example", None), ("b.example", Some(10 * MINUTE))]);
        // With manual items too.
        lock(&h.db).seed().unwrap();
        let snapshot = |h: &Harness| {
            let mut s = lock(&h.db).snapshot().unwrap();
            let mut watched = lock(&h.db).watched().unwrap();
            h.scheduler.annotate_snapshot(&mut s, &mut watched);
            s
        };
        // Never synced: stale, though there is nothing to show yet.
        let s = snapshot(&h);
        assert!(s.connections.iter().all(|c| c.schedule.stale));
        h.tick().await;
        // A failing sync doesn't make what the last one found stale yet.
        h.world.answer("a.example", Err(transient()));
        h.set(at(10));
        h.tick().await;
        let fresh = |h: &Harness, host: &str| {
            let s = snapshot(h);
            let c = s.connections.iter().find(|c| c.label == host).unwrap();
            let items: Vec<_> = s
                .items
                .iter()
                .filter(|i| i.connection_id.as_ref() == Some(&c.id))
                .map(|i| (i.synced_at, i.stale))
                .collect();
            assert_eq!(items.len(), 2);
            // Every item as fresh as its connection.
            assert!(items
                .iter()
                .all(|i| *i == (c.sync.last_success, c.schedule.stale)));
            (c.sync.last_success, c.schedule.stale)
        };
        assert_eq!(fresh(&h, "a.example"), (Some(t0()), false));
        // Three intervals after the last success, it is stale; a connection
        // with a longer interval isn't yet.
        h.set(at(15));
        assert_eq!(fresh(&h, "a.example"), (Some(t0()), false));
        h.set(at(16));
        assert_eq!(fresh(&h, "a.example"), (Some(t0()), true));
        assert_eq!(fresh(&h, "b.example"), (Some(at(10)), false));
        // A success makes it current again.
        h.tick().await;
        assert_eq!(fresh(&h, "a.example"), (Some(at(16)), false));
        // Manual items have no sync to be fresh from.
        let s = snapshot(&h);
        let manual: Vec<_> = s
            .items
            .iter()
            .filter(|i| i.connection_id.is_none())
            .collect();
        assert_eq!(manual.len(), 7);
        assert!(manual.iter().all(|i| i.synced_at.is_none() && !i.stale));
        // An item whose connection isn't known is never current.
        let mut orphan = s.items[0].clone();
        orphan.connection_id = Some("gone".into());
        freshness(&s.connections, std::slice::from_mut(&mut orphan));
        assert!(orphan.stale && orphan.synced_at.is_none());
    }

    #[tokio::test]
    async fn a_failing_watch_goes_stale_while_the_account_syncs() {
        let h = Harness::new(&[("a.example", None)]);
        lock(&h.db)
            .set_watch_user(h.id("a.example"), Some("someone"))
            .unwrap();
        h.tick().await;
        // From now on the watch fails, and keeps what it found at t0,
        // while the account's own syncs go on succeeding.
        let watched = |h: &Harness| {
            let mut s = lock(&h.db).snapshot().unwrap();
            let mut watched = lock(&h.db).watched().unwrap();
            h.scheduler.annotate_snapshot(&mut s, &mut watched);
            let [list] = watched.try_into().unwrap();
            let c = s.connections[0].clone();
            let mine = crate::mine::mine(&s, std::slice::from_ref(&list));
            let rows: Vec<_> = mine
                .watching
                .iter()
                .map(|r| (r.synced_at, r.stale))
                .collect();
            assert_eq!(rows, [(list.items[0].synced_at, list.items[0].stale)]);
            assert!(list.items.iter().all(|i| i.synced_at == list.last_success));
            (
                c.sync.last_success,
                c.schedule.stale,
                list.last_success,
                list.items[0].stale,
            )
        };
        assert_eq!(watched(&h), (Some(t0()), false, Some(t0()), false));
        for minutes in [5, 10, 15, 20] {
            h.world
                .answer(&format!("a.example{WATCH}"), Err(transient()));
            h.set(at(minutes));
            let [report] = h.tick().await.try_into().unwrap();
            assert!(report.error.is_none() && report.watched.unwrap().error.is_some());
            let (account, account_stale, list, list_stale) = watched(&h);
            assert_eq!((account, account_stale), (Some(at(minutes)), false));
            assert_eq!(list, Some(t0()), "the list is as old as when it was found");
            // Stale past three intervals, though the account is current.
            assert_eq!(list_stale, minutes > 15, "{minutes}");
        }
        // A watch that works again is current again.
        h.set(at(25));
        h.tick().await;
        assert_eq!(watched(&h), (Some(at(25)), false, Some(at(25)), false));
    }

    #[tokio::test]
    async fn the_hub_is_pulled_on_schedule_and_is_stale_once_unreachable() {
        const HUB: &str = "hub.example";
        let url = format!("https://{HUB}/");
        let h = Harness::new(&[("a.example", None)]);
        // A token the hub refuses is never kept.
        assert!(h.scheduler.set_hub(&url, "wrong-token").await.is_err());
        assert_eq!(h.scheduler.hub_status().unwrap(), None);
        let status = h.scheduler.set_hub(&url, TOKEN).await.unwrap();
        let summary = |status: &hub::Status| {
            (
                status.items,
                status.sync.last_success,
                status.schedule.stale,
                status.sync.next_due,
            )
        };
        assert_eq!(summary(&status), (2, Some(t0()), false, Some(at(5))));
        let hub_id = lock(&h.db).hub().unwrap().unwrap().id;
        let status = || h.scheduler.hub_status().unwrap().unwrap();
        let pulls = || h.world.lists(HUB);
        assert_eq!(pulls(), 2);
        // Not due again until an interval later, then pulled on schedule.
        assert!(!h.scheduler.due().unwrap().contains(&hub_id));
        h.set(at(5));
        assert!(h.scheduler.due().unwrap().contains(&hub_id));
        h.tick().await;
        assert_eq!(pulls(), 3);
        // A failed pull keeps the last export, which is stale at once, and
        // backs off.
        h.world.answer(HUB, Err(transient()));
        h.set(at(10));
        h.tick().await;
        assert_eq!(summary(&status()), (2, Some(at(5)), true, Some(at(11))));
        assert!(status().sync.error.is_some());
        // It comes back current once a pull works again.
        h.world.answer(HUB, Ok(3));
        h.set(at(11));
        h.tick().await;
        assert_eq!(summary(&status()), (3, Some(at(11)), false, Some(at(16))));
        // An explicit sync of everything pulls it too.
        let reports = h.scheduler.sync_all(Trigger::Explicit).await.unwrap();
        assert!(reports
            .iter()
            .any(|r| r.label == hub::LABEL && r.items == Some(2)));
        // Without a token the hub takes, the pulls stop until it is set
        // again.
        h.world
            .answer(HUB, Err(ConnectorError::new(ErrorKind::Auth, "Refused")));
        h.set(at(20));
        h.tick().await;
        assert!(status().schedule.needs_user && status().schedule.stale);
        let before = pulls();
        h.set(at(120));
        h.tick().await;
        assert_eq!(pulls(), before);
        // Unset, nothing of it is left, and nothing more is pulled.
        h.scheduler.unset_hub().await.unwrap();
        assert_eq!(h.scheduler.hub_status().unwrap(), None);
        h.set(at(240));
        h.tick().await;
        assert_eq!(pulls(), before);
    }

    #[tokio::test]
    async fn the_schedule_sleeps_until_the_next_sync_is_due() {
        let h = Harness::new(&[("a.example", None)]);
        // Due now: as short a sleep as it takes.
        assert_eq!(h.scheduler.sleep(), MIN_SLEEP);
        h.tick().await;
        // Due in five minutes: it looks again sooner anyway.
        assert_eq!(h.scheduler.sleep(), MAX_SLEEP);
        h.set(Timestamp::from_unix(at(5).unix() - 10));
        assert_eq!(h.scheduler.sleep(), Duration::from_secs(10));
        // Waiting for the person: nothing to wake up for.
        let denied = ConnectorError::new(ErrorKind::Auth, "Credential rejected");
        h.world.answer("a.example", Err(denied));
        h.scheduler.sync(h.id("a.example"), Trigger::Explicit).await;
        assert_eq!(h.scheduler.sleep(), MAX_SLEEP);
    }
}
