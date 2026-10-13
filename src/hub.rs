//! The companion's side of a hub: a client that reads the hub's external
//! export and nothing else, the checks that what it read is external and
//! well formed, and how fresh the hub's work is on the companion.
//!
//! Nothing of the companion's ever goes to the hub. The client sends `GET`
//! to the hub's export, exactly, and to sign in, two `POST`s without a body
//! that start and poll GitHub's device flow on the hub; it refuses
//! everything else before it is sent. Its one send is the only place it
//! talks to the hub, and clippy refuses the reqwest calls that could send
//! anything else anywhere in the crate.

use crate::{
    bot::{self, AgentRun, AgentRuns, BoardSummary, BotFields, BotView, Heartbeat},
    connector::{
        safe_id, BoxFuture, ConnectorError, ConnectorResult, Credential, ErrorKind, Requests,
    },
    connectors::{self, http_error, parse_json, read_body},
    model::{
        valid_github_login, valid_provider, Connection, Effort, EffortView, Item, Progress,
        Schedule, Snapshot, SyncState, Timestamp,
    },
    zone::{
        numeric_account_id, ExternalAgents, ExternalBot, ExternalBotFields, ExternalConnection,
        ExternalSnapshot, Zone,
    },
};
use anyhow::{bail, Context, Result};
use reqwest::{
    header::{self, HeaderValue},
    Client, Method, Request, StatusCode,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, PoisonError},
    time::SystemTime,
};
use url::Url;

/// How the hub is named in messages and reports.
pub const LABEL: &str = "Hub";
/// What the IDs of the hub's work start with on its companion, so they never
/// meet the companion's own.
pub const PREFIX: &str = "hub:";
/// Where a hub serves its external export, under its address.
const EXPORT: &str = "api/export/external";
/// Where a hub starts a companion's sign-in with GitHub's device flow, and
/// where the companion polls it, under its address.
const DEVICE: &str = "auth/device";
const DEVICE_POLL: &str = "auth/device/poll";
/// Longest message of a hub's refusal shown.
const MAX_MESSAGE: usize = 300;
/// Most seconds a hub may ask a companion to wait between polls, or give
/// it to sign in.
const MAX_POLL_INTERVAL: u64 = 60;
const MAX_SIGN_IN: u64 = 1800;
const ACCEPT: &str = "application/json";
const USER_AGENT: &str = concat!("workboard/", env!("CARGO_PKG_VERSION"));
/// Longest text field accepted from a hub, in bytes, as from a connector.
const MAX_FIELD: usize = 8192;
/// Longest ID accepted from a hub, in bytes.
const MAX_ID: usize = 512;
/// Most connections, items or efforts accepted from a hub, each.
const MAX_COUNT: usize = 50_000;
/// The provider of a bot connection, the only one with bot fields.
const BOT: &str = "bot";
/// Hosts a hub may be reached at over plain HTTP: this machine's loopback,
/// where its token never crosses a network.
const LOOPBACK: &[&str] = &["127.0.0.1", "[::1]", "localhost"];

/// A hub's address: HTTPS, as `tailscale serve` gives it, or HTTP on this
/// machine's loopback, without credentials, query or fragment, ending in a
/// slash.
pub fn hub_url(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw.trim())
        .context("Enter the hub's complete address, such as https://hub.example.ts.net/")?;
    let host = url.host_str().unwrap_or_default().to_string();
    let scheme_ok = match url.scheme() {
        "https" => !host.is_empty(),
        "http" => LOOPBACK.contains(&host.as_str()),
        _ => false,
    };
    if !scheme_ok
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("Use the hub's HTTPS address (plain HTTP only on this machine's loopback), without credentials, query or fragment")
    }
    let path = format!("{}/", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

/// The hub's export, under its address.
fn export_of(base: &Url) -> ConnectorResult<Url> {
    base.join(EXPORT).map_err(ConnectorError::invalid)
}

/// Whether `request`, exactly as it would be sent, is the read of the
/// export, or a start or poll of the device flow, all without a body.
fn check(base: &Url, request: &Request) -> Result<(), String> {
    let under = |path| base.join(path).map_err(|e| e.to_string());
    let url = request.url();
    match *request.method() {
        _ if request.body().is_some() => Err("a request with a body".into()),
        Method::GET if *url == under(EXPORT)? => Ok(()),
        Method::GET => Err(format!("a read of anything but the export: {url}")),
        Method::POST if *url == under(DEVICE)? || *url == under(DEVICE_POLL)? => Ok(()),
        ref method => Err(format!("{method} {}", url.path())),
    }
}

/// A companion's sign-in with GitHub's device flow, as its hub started it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStart {
    /// What the companion polls with. Never shown.
    pub handle: String,
    /// The code its person enters at `verification_uri`.
    pub user_code: String,
    pub verification_uri: String,
    /// Seconds left to enter it.
    pub expires_in: u64,
    /// Seconds to wait between polls.
    pub interval: u64,
}

/// What a poll of the device flow found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DevicePoll {
    /// Not yet: poll again after `interval` seconds. `approve` once GitHub
    /// has the code, and the sign-in waits for its person to approve it on
    /// the hub's page, signed in there with GitHub.
    Pending {
        interval: u64,
        #[serde(default)]
        approve: bool,
    },
    /// Signed in: a credential that reads the hub's export, bound to the
    /// GitHub user `login` names.
    Done { token: String, login: String },
}

impl DevicePoll {
    /// Not yet: GitHub hasn't got the code.
    pub fn pending(interval: std::time::Duration) -> Self {
        Self::Pending {
            interval: interval.as_secs(),
            approve: false,
        }
    }
}

/// `text` if it is a code of letters, digits and hyphens a person can type.
fn user_code(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 32
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The only page a companion tells its person to enter a code on:
/// GitHub's own, whatever a hub says.
const DEVICE_PAGE: &str = "https://github.com/login/device";

/// What a hub started, if it is fit to show and use.
fn checked_start(start: DeviceStart) -> ConnectorResult<DeviceStart> {
    let invalid = || ConnectorError::invalid("The hub's sign-in isn't usable");
    Credential::new(&start.handle).map_err(|_| invalid())?;
    if !user_code(&start.user_code)
        || start.verification_uri != DEVICE_PAGE
        || !(1..=MAX_POLL_INTERVAL).contains(&start.interval)
        || !(1..=MAX_SIGN_IN).contains(&start.expires_in)
    {
        return Err(invalid());
    }
    Ok(start)
}

/// What a poll found, if it is fit to use.
fn checked_poll(poll: DevicePoll) -> ConnectorResult<DevicePoll> {
    let invalid = || ConnectorError::invalid("The hub's answer to the sign-in isn't usable");
    match poll {
        DevicePoll::Pending { interval, approve }
            if (1..=MAX_POLL_INTERVAL).contains(&interval) =>
        {
            Ok(DevicePoll::Pending { interval, approve })
        }
        DevicePoll::Pending { .. } => Err(invalid()),
        DevicePoll::Done { token, mut login } => {
            let token = Credential::new(&token)
                .map_err(|_| invalid())?
                .expose()
                .to_string();
            printable(&mut login);
            login.truncate(MAX_ID);
            Ok(DevicePoll::Done { token, login })
        }
    }
}

/// Reads a hub's external export.
pub trait Pull: Send + Sync {
    /// The export of the hub at `base`, checked.
    fn pull<'a>(
        &'a self,
        base: &'a Url,
        token: &'a Credential,
    ) -> BoxFuture<'a, ConnectorResult<ExternalSnapshot>>;
    /// What it sent, and refused to.
    fn requests(&self) -> Option<Requests>;
}

/// The client a companion reads its hub with: one `GET` of the export,
/// nothing else, redirects never followed.
pub struct HubClient {
    client: Client,
    counts: Mutex<Requests>,
}

impl HubClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: connectors::client()?,
            counts: Mutex::default(),
        })
    }
    fn count(&self, update: impl FnOnce(&mut Requests)) {
        update(&mut self.counts.lock().unwrap_or_else(PoisonError::into_inner));
    }
    /// Send `request` if it is the read of the export. Every request to the
    /// hub goes through here.
    async fn send(&self, base: &Url, request: Request) -> ConnectorResult<reqwest::Response> {
        if let Err(why) = check(base, &request) {
            self.count(|c| c.refused += 1);
            return Err(ConnectorError::invalid(format!(
                "Refused to send the hub anything but a read of its export or a sign-in: {why}"
            )));
        }
        self.count(|c| *c.sent.entry(request.method().to_string()).or_default() += 1);
        // The one place a request to the hub leaves, after `check` passed it.
        #[allow(clippy::disallowed_methods)]
        let response = self.client.execute(request).await;
        response.map_err(|_| {
            ConnectorError::new(
                ErrorKind::Transient,
                "Cannot reach the hub; check the network and its address",
            )
        })
    }
    /// A request without a body, carrying `token` if there is one.
    fn request(method: Method, url: Url, token: Option<&Credential>) -> Request {
        let mut request = Request::new(method, url);
        let headers = request.headers_mut();
        headers.insert(header::ACCEPT, HeaderValue::from_static(ACCEPT));
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        // A credential has no control characters, so this only fails for
        // one that can't be a header at all.
        if let Some(Ok(mut auth)) =
            token.map(|token| HeaderValue::from_str(&format!("Bearer {}", token.expose())))
        {
            auth.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, auth);
        }
        request
    }
    /// Ask the hub at `base` to start a sign-in with GitHub's device flow.
    /// Nothing is sent but the request itself.
    pub async fn start_login(&self, base: &Url) -> ConnectorResult<DeviceStart> {
        let url = base.join(DEVICE).map_err(ConnectorError::invalid)?;
        let response = self
            .send(base, Self::request(Method::POST, url, None))
            .await?;
        checked_start(Self::sign_in_answer(response).await?)
    }
    /// Ask the hub at `base` whether the sign-in `handle` names is done.
    pub async fn poll_login(&self, base: &Url, handle: &Credential) -> ConnectorResult<DevicePoll> {
        let url = base.join(DEVICE_POLL).map_err(ConnectorError::invalid)?;
        let request = Self::request(Method::POST, url, Some(handle));
        let response = self.send(base, request).await?;
        checked_poll(Self::sign_in_answer(response).await?)
    }
    /// A hub's answer to a sign-in request, or its refusal, whose message
    /// is shown, without control characters and bounded.
    async fn sign_in_answer<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> ConnectorResult<T> {
        let status = response.status();
        if status.is_redirection() {
            return Err(ConnectorError::invalid(
                "The hub redirected the sign-in, which is never followed; check its address",
            ));
        }
        let body = read_body(LABEL, response).await?;
        if status.is_success() {
            return serde_json::from_value(parse_json(LABEL, &body)?).map_err(|_| {
                ConnectorError::invalid("The hub's answer to the sign-in isn't usable")
            });
        }
        let mut message = parse_json(LABEL, &body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        printable(&mut message);
        let message: String = message.chars().take(MAX_MESSAGE).collect();
        let kind = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ErrorKind::Auth,
            s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => ErrorKind::Transient,
            _ => ErrorKind::Invalid,
        };
        Err(ConnectorError::new(
            kind,
            format!("The hub refused the sign-in: {message}"),
        ))
    }
    async fn read(&self, base: &Url, token: &Credential) -> ConnectorResult<ExternalSnapshot> {
        let request = Self::request(Method::GET, export_of(base)?, Some(token));
        let response = self.send(base, request).await?;
        match response.status() {
            s if s.is_success() => {
                let body = read_body(LABEL, response).await?;
                let snapshot = serde_json::from_value(parse_json(LABEL, &body)?).map_err(|_| {
                    ConnectorError::invalid("The hub's answer isn't an external export")
                })?;
                checked(snapshot, Timestamp::of(SystemTime::now()))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(ConnectorError::new(
                ErrorKind::Auth,
                "The hub refused the token; set it again with `workboard hub set`",
            )),
            s if s.is_redirection() => Err(ConnectorError::invalid(format!(
                "The hub redirected the request (HTTP {}), which is never followed; check its address",
                s.as_u16()
            ))),
            s => Err(http_error(s)),
        }
    }
}

impl Pull for HubClient {
    fn pull<'a>(
        &'a self,
        base: &'a Url,
        token: &'a Credential,
    ) -> BoxFuture<'a, ConnectorResult<ExternalSnapshot>> {
        Box::pin(self.read(base, token))
    }
    fn requests(&self) -> Option<Requests> {
        Some(
            self.counts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
    }
}

/// The set of `ids`, if each is there once, and none is empty, overlong or
/// could change a URL path it is part of ([`safe_id`]). The page puts IDs
/// in the paths of API calls, where a hub's `/../`, or an ID of `..`,
/// could otherwise send a call to another route. A set, so checking what
/// each item refers to stays linear: a hub could send lists of up to
/// [`MAX_COUNT`] each.
fn unique_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Option<HashSet<&'a str>> {
    let mut set = HashSet::new();
    for id in ids {
        if id.is_empty() || id.len() > MAX_ID || !safe_id(id) || !set.insert(id) {
            return None;
        }
    }
    Some(set)
}

/// `text` without control characters, which a terminal could take as
/// escapes when the CLI prints it.
fn printable(text: &mut String) {
    if text.contains(char::is_control) {
        text.retain(|c| !c.is_control());
    }
}

/// A hub's bot item's fields, if fit to show here: bounded text, its Branch
/// an HTTP(S) link, and the board item that links a fork pull request an
/// issue or pull request on the instance at `base`. What passes has its
/// links as parsed and its text without control characters.
fn checked_bot_fields(b: &mut ExternalBotFields, base: &Url) -> Option<()> {
    let fields = [&b.priority, &b.why, &b.news, &b.branch, &b.board_item];
    if fields.into_iter().flatten().any(|f| f.len() > MAX_FIELD) {
        return None;
    }
    for text in [&mut b.priority, &mut b.why, &mut b.news]
        .into_iter()
        .flatten()
    {
        printable(text);
    }
    if let Some(branch) = &mut b.branch {
        *branch = connectors::source_url(branch).ok()?;
    }
    if let Some(item) = &mut b.board_item {
        *item = bot::item_on(item, base)?.0;
    }
    Some(())
}

/// A hub's bot, if fit to use here, on the instance at `base`, at `now`:
/// its login and tracker what they can be on GitHub, its board on the
/// instance, and what it read of its heartbeat and board held to the
/// checks the hub's own reading of the heartbeat passed
/// ([`bot::parse_heartbeat`]): names and states of a safe character set,
/// a wake time within the longest sleep after the heartbeat or none, at
/// most as many runs, each on an issue or pull request of the instance,
/// and links to the instance only. A heartbeat from, or a run started,
/// more than a few minutes ahead of this machine's clock is left out
/// rather than refusing the export, so a clock that is off can't hide all
/// of the hub's work: the heartbeat as one that couldn't be read, the run
/// as a malformed one. Its board's counts are bounded, and its status
/// names cleaned as the hub names them ([`bot::status_name`]).
fn checked_bot(b: &mut ExternalBot, base: &Url, now: Timestamp) -> Option<()> {
    if !valid_github_login(&b.bot_login) || bot::tracker_parts(&b.tracker).is_none() {
        return None;
    }
    b.board_url = bot::board_on(&b.board_url, base)?.into();
    let Some(agents) = &mut b.agents else {
        return Some(());
    };
    if agents
        .heartbeat
        .as_ref()
        .is_some_and(|h| !bot::not_ahead(h.updated_at, now))
    {
        eprintln!(
            "Left out the hub's heartbeat of {}: it is from more than a few minutes ahead of this machine's clock",
            b.bot_login
        );
        agents.heartbeat = None;
        agents.failed = true;
    }
    if let Some(h) = &mut agents.heartbeat {
        if !bot::valid_name(&h.session)
            || !bot::valid_word(&h.loop_state)
            || h.runs.len() > bot::MAX_WORKERS
            || h.skipped > MAX_COUNT
        {
            return None;
        }
        h.next_wake_at = bot::wake_after(h.updated_at, h.next_wake_at);
        if let Some(url) = &mut h.comment_url {
            if url.len() > MAX_FIELD {
                return None;
            }
            *url = bot::page_on(url, base)?;
        }
        // A run started ahead of this machine's clock is left out, and
        // counted with the malformed ones, as the hub's reading does.
        let ahead = h.runs.len();
        h.runs.retain(|r| bot::not_ahead(r.started_at, now));
        let ahead = ahead - h.runs.len();
        if ahead > 0 {
            eprintln!(
                "Left out {ahead} of the hub's agent runs of {}: they started more than a few minutes ahead of this machine's clock",
                b.bot_login
            );
            h.skipped = h.skipped.saturating_add(ahead);
        }
        let mut names = HashSet::new();
        for r in &mut h.runs {
            let (url, key) = bot::item_on(&r.item_url, base)?;
            if !bot::valid_name(&r.name)
                || !names.insert(r.name.clone())
                || key != r.item_key
                || !bot::valid_word(&r.status)
                || r.devspace
                    .as_deref()
                    .is_some_and(|d| !bot::valid_devspace(d))
            {
                return None;
            }
            r.item_url = url;
        }
    }
    if let Some(board) = &mut agents.board {
        let total = board
            .statuses
            .values()
            .try_fold(0usize, |total, n| total.checked_add(*n));
        if board.statuses.len() > MAX_COUNT
            || total.is_none_or(|total| total > MAX_COUNT)
            || board.fork_prs > MAX_COUNT
            || board.unreadable > MAX_COUNT
        {
            return None;
        }
        let mut cleaned = BoardSummary::default();
        for (status, n) in &board.statuses {
            cleaned.add(status, *n);
        }
        board.statuses = cleaned.statuses;
    }
    Some(())
}

/// What a hub sent, if all of it is external and well formed: every object
/// says it is external, items belong to connections it sent and efforts
/// to efforts it sent, links are HTTP(S), and nothing is out of bounds.
/// A bot's fields and settings belong to a bot connection it sent, and
/// their links to that connection's instance; a heartbeat or run ahead of
/// `now`, this machine's clock, by more than clocks may disagree, is left
/// out. Anything else refuses all of it. What passes keeps links as they are parsed, and
/// text without control characters.
pub fn checked(mut s: ExternalSnapshot, now: Timestamp) -> ConnectorResult<ExternalSnapshot> {
    let fail = |why: &str| Err(ConnectorError::invalid(format!("The hub's export {why}")));
    if s.zone != Zone::External
        || s.connections.iter().any(|c| c.zone != Zone::External)
        || s.items.iter().any(|i| i.zone != Zone::External)
    {
        return fail("has something that isn't external");
    }
    if [s.connections.len(), s.items.len(), s.efforts.len()]
        .iter()
        .any(|n| *n > MAX_COUNT)
    {
        return fail("is too large");
    }
    let (Some(connections), Some(efforts), Some(_)) = (
        unique_ids(s.connections.iter().map(|c| c.id.as_str())),
        unique_ids(s.efforts.iter().map(|e| e.id.as_str())),
        unique_ids(s.items.iter().map(|i| i.id.as_str())),
    ) else {
        return fail("has missing, overlong, unsafe or repeated IDs");
    };
    let text_ok = |fields: &[&str]| fields.iter().all(|f| f.len() <= MAX_FIELD);
    for c in &s.connections {
        if !valid_provider(&c.provider)
            || connectors::base_url(&c.base_url).ok().as_deref() != Some(c.base_url.as_str())
            || !text_ok(&[&c.label, &c.account_name])
            || c.account_id
                .as_deref()
                .is_some_and(|id| numeric_account_id(id).is_none())
        {
            return fail("has a connection that isn't valid");
        }
    }
    for i in &s.items {
        let fields = [
            i.key.as_str(),
            &i.title,
            &i.status,
            &i.outcome_reason,
            &i.action,
            &i.updated_at,
        ];
        if !valid_provider(&i.provider)
            || !connections.contains(&i.connection_id.as_str())
            || i.effort_id
                .as_deref()
                .is_some_and(|e| !efforts.contains(&e))
            || connectors::source_url(&i.url).is_err()
            || !text_ok(&fields)
        {
            return fail("has an item that isn't valid");
        }
    }
    if !s.efforts.iter().all(|e| text_ok(&[&e.title])) {
        return fail("has an effort that isn't valid");
    }
    // Each connection's instance, and whether it is a bot's. Their
    // addresses were checked above.
    let bases: HashMap<String, (bool, Url)> = s
        .connections
        .iter()
        .filter_map(|c| {
            Some((
                c.id.clone(),
                (c.provider == BOT, Url::parse(&c.base_url).ok()?),
            ))
        })
        .collect();
    let bot_base = |id: &str| match bases.get(id) {
        Some((true, base)) => Some(base),
        _ => None,
    };
    for i in &mut s.items {
        let Some(fields) = &mut i.bot else { continue };
        let fit = i.provider == BOT
            && bot_base(&i.connection_id)
                .is_some_and(|base| checked_bot_fields(fields, base).is_some());
        if !fit {
            return fail("has an item whose bot fields aren't valid");
        }
    }
    let mut bots = HashSet::new();
    for b in &mut s.bots {
        let fit = bots.insert(b.connection_id.clone())
            && bot_base(&b.connection_id).is_some_and(|base| checked_bot(b, base, now).is_some());
        if !fit {
            return fail("has a bot that isn't valid");
        }
    }
    for c in &mut s.connections {
        for text in [&mut c.label, &mut c.account_name] {
            printable(text);
        }
    }
    for i in &mut s.items {
        // Checked above, so this only puts the link in its parsed form.
        i.url = connectors::source_url(&i.url).map_err(ConnectorError::invalid)?;
        for text in [
            &mut i.key,
            &mut i.title,
            &mut i.status,
            &mut i.outcome_reason,
            &mut i.action,
            &mut i.updated_at,
        ] {
            printable(text);
        }
    }
    for e in &mut s.efforts {
        printable(&mut e.title);
    }
    Ok(s)
}

/// The hub a companion pulls from: its address, how its pulls went, and
/// the last export it read.
#[derive(Debug, Clone)]
pub struct Link {
    pub id: String,
    pub url: String,
    pub sync: SyncState,
    pub snapshot: Option<ExternalSnapshot>,
}

impl Link {
    /// The pulls as the schedule sees them: like a connection's syncs, on
    /// the service's interval. Never stored or shown as a connection.
    pub fn as_job(&self) -> Connection {
        Connection {
            id: self.id.clone(),
            provider: "hub".into(),
            label: LABEL.into(),
            base_url: self.url.clone(),
            sync: self.sync.clone(),
            ..Connection::default()
        }
    }
}

/// How a companion's pulls from its hub are going, and how fresh the hub's
/// work is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub url: String,
    pub sync: SyncState,
    /// Stale also as soon as a pull fails: the hub's work is never shown as
    /// current while the hub can't be reached.
    pub schedule: Schedule,
    /// What the last export it read had.
    pub connections: usize,
    pub items: usize,
}

/// Whether `id` names the hub's work, read-only on its companion.
pub fn is_from_hub(id: &str) -> bool {
    id.starts_with(PREFIX)
}

/// Whether the companion's own connection `mine` is to the same account on
/// the same instance as the hub's `theirs`. The account's numeric ID
/// decides when both know it, since an account keeps its ID when renamed,
/// and a name freed by a rename can be taken by another account.
/// Otherwise, for a hub that doesn't export IDs or an account whose ID
/// isn't numeric, the name does, as the instance shows it, without regard
/// to case.
fn same_account(mine: &Connection, theirs: &ExternalConnection) -> bool {
    if mine.from_hub || mine.provider != theirs.provider || mine.base_url != theirs.base_url {
        return false;
    }
    match (numeric_account_id(&mine.account_id), &theirs.account_id) {
        (Some(mine), Some(theirs)) => mine == *theirs,
        _ => mine.account_name.eq_ignore_ascii_case(&theirs.account_name),
    }
}

/// Add the hub's work in `export`, as of the pulls in `status`, to the
/// companion's own `snapshot`, marked as the hub's: its connections, items
/// and efforts, their IDs starting with [`PREFIX`].
///
/// A hub connection to an account the companion also connects is left out,
/// with its items, so nothing shows twice: the companion's own copy is the
/// one shown, being synced right here. Everything from the hub is as fresh
/// as the hub's connection that synced it was at the last pull, and stale
/// whenever the hub's work is.
///
/// The result is for showing on the companion only. It never goes into the
/// companion's own export, which is built from its own work alone.
pub fn merge(snapshot: &mut Snapshot, export: &ExternalSnapshot, status: &Status) {
    let id = |id: &str| format!("{PREFIX}{id}");
    let shown: Vec<&ExternalConnection> = export
        .connections
        .iter()
        .filter(|theirs| {
            !snapshot
                .connections
                .iter()
                .any(|mine| same_account(mine, theirs))
        })
        .collect();
    let fresh: HashMap<&str, _> = shown
        .iter()
        .map(|c| {
            (
                c.id.as_str(),
                (c.last_success, c.stale || status.schedule.stale),
            )
        })
        .collect();
    for c in &shown {
        snapshot.connections.push(Connection {
            id: id(&c.id),
            provider: c.provider.clone(),
            label: c.label.clone(),
            base_url: c.base_url.clone(),
            account_id: c.account_id.clone().unwrap_or_default(),
            account_name: c.account_name.clone(),
            zone: Zone::External,
            watch_user: None,
            sync_interval: None,
            sync: SyncState {
                last_success: c.last_success,
                error_kind: c.error_kind,
                error: c
                    .error_kind
                    .map(|_| "The hub's last sync of it failed".to_string()),
                ..SyncState::default()
            },
            schedule: Schedule {
                interval_secs: status.schedule.interval_secs,
                running: false,
                needs_user: false,
                stale: fresh[c.id.as_str()].1,
            },
            from_hub: true,
            bot: None,
            plugin: None,
        });
    }
    // The hub's efforts with work shown here: one whose items are all
    // left out, as the companion's own copies, would show empty.
    let mut with_work = HashSet::new();
    for i in &export.items {
        let Some(&(synced_at, stale)) = fresh.get(i.connection_id.as_str()) else {
            continue;
        };
        with_work.extend(i.effort_id.as_deref());
        snapshot.items.push(Item {
            id: id(&i.id),
            connection_id: Some(id(&i.connection_id)),
            provider: i.provider.clone(),
            remote_id: i.key.clone(),
            key: i.key.clone(),
            title: i.title.clone(),
            url: i.url.clone(),
            status: i.status.clone(),
            outcome: i.outcome,
            outcome_reason: i.outcome_reason.clone(),
            action: i.action.clone(),
            updated_at: i.updated_at.clone(),
            effort_id: i.effort_id.as_deref().map(id),
            abandoned_at: None,
            demo: false,
            zone: Zone::External,
            reasons: i.reasons.clone(),
            synced_at,
            stale,
            from_hub: true,
            bot: i.bot.as_ref().map(|b| BotFields {
                kind: b.kind,
                priority: b.priority.clone(),
                why: b.why.clone(),
                news: b.news.clone(),
                branch: b.branch.clone(),
                board_item: b.board_item.clone(),
            }),
        });
    }
    for e in export
        .efforts
        .iter()
        .filter(|e| with_work.contains(e.id.as_str()))
    {
        let p = &e.progress;
        snapshot.efforts.push(EffortView {
            effort: Effort {
                id: id(&e.id),
                title: e.title.clone(),
                note: String::new(),
                share_title: false,
            },
            progress: Progress {
                delivered: p.delivered,
                total: p.total,
                open: p.open,
                needs_attention: p.needs_attention,
                abandoned: p.abandoned,
            },
            from_hub: true,
        });
    }
}

/// What a companion says of a bot whose heartbeat its hub couldn't read at
/// its last sync. The hub's own message stays on the hub.
const HUB_HEARTBEAT_ERROR: &str = "The hub couldn't read the heartbeat at its last sync";

/// The hub's bots, as the companion's Bot view shows them, read-only: where
/// their board is, what the hub last read of their heartbeat and board, as
/// fresh as the hub's work is (`status`), and links that ask them for
/// something, built here, at `now`, from the tracker and login the hub
/// exported. `board` is the companion's board with the hub's work merged
/// in ([`merge`]): a bot whose connection it left out, as the companion's
/// own, isn't shown twice, and only the hub's bot items shown get Ask
/// links.
pub fn bot_views(
    board: &Snapshot,
    export: &ExternalSnapshot,
    status: &Status,
    now: Timestamp,
) -> Vec<BotView> {
    export
        .bots
        .iter()
        .filter_map(|b| {
            let id = format!("{PREFIX}{}", b.connection_id);
            let c = board
                .connections
                .iter()
                .find(|c| c.from_hub && c.id == id)?;
            let base = Url::parse(&c.base_url).ok()?;
            let ask = |item: Option<&Item>| bot::ask_to(&b.tracker, &b.bot_login, &base, item).ok();
            let asks = board
                .items
                .iter()
                .filter(|i| i.bot.is_some() && i.connection_id.as_deref() == Some(id.as_str()))
                .filter_map(|i| Some((i.id.clone(), ask(Some(i))?)))
                .collect();
            Some(BotView {
                label: c.label.clone(),
                bot_login: b.bot_login.clone(),
                board_url: b.board_url.clone(),
                agents: b
                    .agents
                    .as_ref()
                    .map(|a| agent_runs(&id, a, status.schedule.stale, now)),
                ask: ask(None)?,
                asks,
                from_hub: true,
                connection_id: id,
            })
        })
        .collect()
}

/// What the hub's bot connection `id` last read, as the companion shows
/// it: stale if it was on the hub, or the hub's work is here, and its
/// heartbeat as stale as it is at `now`.
fn agent_runs(id: &str, a: &ExternalAgents, hub_stale: bool, now: Timestamp) -> AgentRuns {
    AgentRuns {
        connection_id: id.into(),
        heartbeat: a.heartbeat.as_ref().map(|h| {
            let mut heartbeat = Heartbeat {
                updated_at: h.updated_at,
                session: h.session.clone(),
                loop_state: h.loop_state.clone(),
                next_wake_at: h.next_wake_at,
                runs: h
                    .runs
                    .iter()
                    .map(|r| AgentRun {
                        name: r.name.clone(),
                        item_url: r.item_url.clone(),
                        item_key: r.item_key.clone(),
                        status: r.status.clone(),
                        started_at: r.started_at,
                        devspace: r.devspace.clone(),
                    })
                    .collect(),
                skipped: h.skipped,
                comment_url: h.comment_url.clone(),
                stale: false,
            };
            heartbeat.stale = bot::heartbeat_stale(&heartbeat, now);
            heartbeat
        }),
        error: a.failed.then(|| HUB_HEARTBEAT_ERROR.to_string()),
        last_success: a.last_success,
        stale: a.stale || hub_stale,
        board: a.board.as_ref().map(|b| BoardSummary {
            statuses: b.statuses.clone(),
            fork_prs: b.fork_prs,
            unreadable: b.unreadable,
        }),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::{
        model::{Outcome, Timestamp},
        zone::{ExternalConnection, ExternalEffort, ExternalItem, ExternalProgress},
    };
    use axum::{
        extract::State,
        http::{HeaderMap as Headers, Method as AxumMethod, StatusCode as Code, Uri},
        response::{IntoResponse, Response},
        Router,
    };
    use std::sync::Arc;

    /// An export with `n` items of one connection, the first in an effort.
    pub fn export(n: usize) -> ExternalSnapshot {
        let connection = ExternalConnection {
            id: "hub-connection".into(),
            provider: "github".into(),
            label: "GitHub".into(),
            base_url: "https://github.com/".into(),
            account_name: "someone".into(),
            account_id: Some("7".into()),
            last_success: Some(Timestamp::from_unix(1_791_763_200)),
            stale: false,
            error_kind: None,
            zone: Zone::External,
        };
        let items = (0..n)
            .map(|k| ExternalItem {
                id: format!("hub-connection:github:{k}"),
                connection_id: connection.id.clone(),
                provider: "github".into(),
                key: format!("o/r#{k}"),
                title: format!("Change {k}"),
                url: format!("https://github.com/o/r/pull/{k}"),
                status: "open".into(),
                outcome: Outcome::Open,
                outcome_reason: "Open".into(),
                action: "review".into(),
                updated_at: "2026-10-12T00:00:00Z".into(),
                effort_id: (k == 0).then(|| "hub-effort".into()),
                zone: Zone::External,
                reasons: vec![crate::model::Reason::ReviewRequested],
                bot: None,
            })
            .collect();
        ExternalSnapshot {
            zone: Zone::External,
            connections: vec![connection],
            items,
            efforts: vec![ExternalEffort {
                id: "hub-effort".into(),
                title: "Effort hubeffor".into(),
                progress: ExternalProgress {
                    delivered: 0,
                    total: 1,
                    open: 1,
                    needs_attention: 0,
                    abandoned: 0,
                },
            }],
            bots: vec![],
        }
    }

    /// When the exports below are checked: an hour after their syncs.
    const NOW: Timestamp = Timestamp::from_unix(1_791_763_200 + 3600);

    /// An export with a bot connection: its two items, one a fork pull
    /// request linked from the other, and what it last read.
    pub fn bot_export() -> ExternalSnapshot {
        use crate::zone::{ExternalBoard, ExternalHeartbeat, ExternalRun};
        let mut s = export(0);
        s.efforts.clear();
        s.connections[0] = ExternalConnection {
            id: "hub-bot".into(),
            provider: "bot".into(),
            label: "Bot".into(),
            account_id: Some("8".into()),
            ..s.connections[0].clone()
        };
        let item = |n: u64, kind: &str, bot: ExternalBotFields| ExternalItem {
            id: format!("hub-bot:bot:{n}"),
            connection_id: "hub-bot".into(),
            provider: "bot".into(),
            key: format!("example-org/r#{n}"),
            title: format!("Bot change {n}"),
            url: format!("https://github.com/example-org/r/{kind}/{n}"),
            status: "Needs human".into(),
            outcome: Outcome::Open,
            outcome_reason: "Open".into(),
            action: "review".into(),
            updated_at: "2026-10-12T00:00:00Z".into(),
            effort_id: None,
            zone: Zone::External,
            reasons: vec![crate::model::Reason::BotWaiting],
            bot: Some(bot),
        };
        s.items = vec![
            item(
                1,
                "issues",
                ExternalBotFields {
                    kind: bot::BotKind::BoardItem,
                    priority: Some("P0".into()),
                    why: Some("Which default?".into()),
                    news: Some("Asked".into()),
                    branch: Some("https://github.com/example-org/r/pull/2".into()),
                    board_item: None,
                },
            ),
            item(
                2,
                "pull",
                ExternalBotFields {
                    kind: bot::BotKind::ForkPr,
                    priority: None,
                    why: None,
                    news: None,
                    branch: None,
                    board_item: Some("https://github.com/example-org/r/issues/1".into()),
                },
            ),
        ];
        s.bots = vec![ExternalBot {
            connection_id: "hub-bot".into(),
            bot_login: "example-bot".into(),
            tracker: "example-org/tracker".into(),
            board_url: "https://github.com/orgs/example-org/projects/1".into(),
            agents: Some(ExternalAgents {
                heartbeat: Some(ExternalHeartbeat {
                    updated_at: Timestamp::from_unix(1_791_763_200),
                    session: "coordinator".into(),
                    loop_state: "sleeping".into(),
                    next_wake_at: None,
                    runs: vec![ExternalRun {
                        name: "wtp-one".into(),
                        item_url: "https://github.com/example-org/r/issues/1".into(),
                        item_key: "example-org/r#1".into(),
                        status: "working".into(),
                        started_at: Timestamp::from_unix(1_791_762_000),
                        devspace: Some("devspace-1".into()),
                    }],
                    skipped: 0,
                    comment_url: Some(
                        "https://github.com/example-org/tracker/issues/1#issuecomment-1".into(),
                    ),
                }),
                failed: false,
                last_success: Some(Timestamp::from_unix(1_791_763_200)),
                stale: false,
                board: Some(ExternalBoard {
                    statuses: [("Needs human".to_string(), 1), ("Draft".to_string(), 1)].into(),
                    fork_prs: 1,
                    unreadable: 0,
                }),
            }),
        }];
        s
    }

    #[test]
    fn a_hubs_bot_shows_in_the_companions_bot_view() {
        let export = checked(bot_export(), NOW).unwrap();
        // A heartbeat from two hours before: stale by then.
        let now = Timestamp::from_unix(1_791_763_200 + 2 * 3600);
        for (failed, hub_stale) in [(false, false), (true, true)] {
            let mut export = export.clone();
            if let Some(a) = &mut export.bots[0].agents {
                a.failed = failed;
            }
            let mut board = Snapshot {
                connections: vec![],
                items: vec![],
                efforts: vec![],
            };
            merge(&mut board, &export, &status(hub_stale));
            // The items keep their board fields.
            let fields: Vec<_> = board.items.iter().map(|i| i.bot.clone().unwrap()).collect();
            assert_eq!(
                (
                    fields[0].priority.as_deref(),
                    fields[1].board_item.as_deref()
                ),
                (
                    Some("P0"),
                    Some("https://github.com/example-org/r/issues/1")
                )
            );
            let [view] = &bot_views(&board, &export, &status(hub_stale), now)[..] else {
                panic!()
            };
            assert_eq!(
                (
                    view.connection_id.as_str(),
                    view.from_hub,
                    view.bot_login.as_str(),
                    view.board_url.as_str()
                ),
                (
                    "hub:hub-bot",
                    true,
                    "example-bot",
                    "https://github.com/orgs/example-org/projects/1"
                )
            );
            // Ask links built here, for each of its items.
            assert!(view
                .ask
                .issue_url
                .starts_with("https://github.com/example-org/tracker/issues/new?"));
            let keys: Vec<_> = view.asks.keys().map(String::as_str).collect();
            assert_eq!(keys, ["hub:hub-bot:bot:1", "hub:hub-bot:bot:2"]);
            let comment = &view.asks["hub:hub-bot:bot:2"];
            assert_eq!(
                (comment.comment_url.as_deref(), comment.comment.as_deref()),
                (
                    Some("https://github.com/example-org/r/pull/2"),
                    Some("@example-bot ")
                )
            );
            let agents = view.agents.as_ref().unwrap();
            let heartbeat = agents.heartbeat.as_ref().unwrap();
            assert_eq!(heartbeat.runs[0].name, "wtp-one");
            assert!(heartbeat.stale);
            assert_eq!(agents.stale, hub_stale);
            assert_eq!(agents.error.is_some(), failed);
            assert_eq!(agents.board.as_ref().unwrap().fork_prs, 1);
        }
    }

    #[test]
    fn a_hubs_bot_the_companion_connects_too_shows_once() {
        let export = checked(bot_export(), NOW).unwrap();
        let mut board = Snapshot {
            connections: vec![Connection {
                provider: "bot".into(),
                account_id: "8".into(),
                ..own("someone")
            }],
            items: vec![],
            efforts: vec![],
        };
        merge(&mut board, &export, &status(false));
        let now = Timestamp::from_unix(1_791_763_200);
        assert!(bot_views(&board, &export, &status(false), now).is_empty());
    }

    #[test]
    fn heartbeats_and_runs_ahead_of_the_clock_are_left_out() {
        let ahead = NOW.after(std::time::Duration::from_secs(10 * 60));
        let close = NOW.after(std::time::Duration::from_secs(60));
        let heartbeat = |s: &mut ExternalSnapshot| {
            s.bots[0]
                .agents
                .as_mut()
                .unwrap()
                .heartbeat
                .as_mut()
                .unwrap()
                .clone()
        };
        // A run started ahead: left out and counted, the rest kept.
        let mut s = bot_export();
        let mut h = heartbeat(&mut s);
        let mut later = h.runs[0].clone();
        later.name = "wtp-later".into();
        later.started_at = ahead;
        h.runs[0].started_at = close;
        h.runs.push(later);
        s.bots[0].agents.as_mut().unwrap().heartbeat = Some(h);
        let mut s = checked(s, NOW).unwrap();
        let h = heartbeat(&mut s);
        let names: Vec<_> = h.runs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!((names, h.skipped), (vec!["wtp-one"], 1));
        assert!(!s.bots[0].agents.as_ref().unwrap().failed);
        // A heartbeat ahead: left out as one that couldn't be read, and
        // the rest of the export kept.
        let mut s = bot_export();
        s.bots[0]
            .agents
            .as_mut()
            .unwrap()
            .heartbeat
            .as_mut()
            .unwrap()
            .updated_at = ahead;
        let s = checked(s, NOW).unwrap();
        let agents = s.bots[0].agents.as_ref().unwrap();
        assert!(agents.heartbeat.is_none() && agents.failed);
        assert!(agents.board.is_some());
        assert_eq!(s.items.len(), 2);
    }

    #[test]
    fn bot_parts_are_checked_and_cleaned() {
        let mut s = bot_export();
        let fields = s.items[0].bot.as_mut().unwrap();
        fields.why = Some("Which\u{1b}[2J default?".into());
        fields.branch = Some("HTTPS://GitHub.com/example-org/r/pull/2".into());
        s.bots[0].board_url = "https://GitHub.com/orgs/example-org/projects/1".into();
        let s = checked(s, NOW).unwrap();
        let fields = s.items[0].bot.as_ref().unwrap();
        assert_eq!(fields.why.as_deref(), Some("Which[2J default?"));
        assert_eq!(
            fields.branch.as_deref(),
            Some("https://github.com/example-org/r/pull/2")
        );
        assert_eq!(
            s.bots[0].board_url,
            "https://github.com/orgs/example-org/projects/1"
        );

        // A wake time out of the heartbeat's window says nothing, and odd
        // status names are cleaned, as the hub names them, rather than
        // refusing the export.
        let mut s = bot_export();
        let updated_at = heartbeat(&mut s).updated_at;
        heartbeat(&mut s).next_wake_at =
            Some(updated_at.after(std::time::Duration::from_secs(3 * 60 * 60)));
        board(&mut s).statuses = (0..150)
            .map(|n| (format!("Status {n}"), 1))
            .chain([
                ("Done\u{7}".to_string(), 2),
                ("x".repeat(bot::MAX_STATUS + 50), 3),
            ])
            .collect();
        let s = checked(s, NOW).unwrap();
        let agents = s.bots[0].agents.as_ref().unwrap();
        assert_eq!(agents.heartbeat.as_ref().unwrap().next_wake_at, None);
        let statuses = &agents.board.as_ref().unwrap().statuses;
        assert_eq!(statuses.len(), bot::MAX_STATUSES);
        assert_eq!(statuses.values().sum::<usize>(), 155);
        assert!(statuses
            .keys()
            .all(|k| k.len() <= bot::MAX_STATUS && !k.contains(char::is_control)));

        type Break = fn(&mut ExternalSnapshot);
        fn heartbeat(s: &mut ExternalSnapshot) -> &mut crate::zone::ExternalHeartbeat {
            s.bots[0]
                .agents
                .as_mut()
                .unwrap()
                .heartbeat
                .as_mut()
                .unwrap()
        }
        fn board(s: &mut ExternalSnapshot) -> &mut crate::zone::ExternalBoard {
            s.bots[0].agents.as_mut().unwrap().board.as_mut().unwrap()
        }
        let cases: [(&str, Break); 26] = [
            ("bot fields on another provider's item", |s| {
                s.items[0].provider = "github".into()
            }),
            ("bot fields of a connection that isn't a bot's", |s| {
                s.connections[0].provider = "github".into();
                for i in &mut s.items {
                    i.provider = "github".into();
                }
            }),
            ("script branch", |s| {
                s.items[0].bot.as_mut().unwrap().branch = Some("javascript:alert(1)".into())
            }),
            ("board item elsewhere", |s| {
                s.items[1].bot.as_mut().unwrap().board_item =
                    Some("https://evil.example/example-org/r/issues/1".into())
            }),
            ("overlong why", |s| {
                s.items[0].bot.as_mut().unwrap().why = Some("x".repeat(MAX_FIELD + 1))
            }),
            ("overlong branch", |s| {
                s.items[0].bot.as_mut().unwrap().branch =
                    Some(format!("https://github.com/{}", "x".repeat(MAX_FIELD)))
            }),
            ("overlong comment link", |s| {
                heartbeat(s).comment_url =
                    Some(format!("https://github.com/{}", "x".repeat(MAX_FIELD)))
            }),
            ("bot of an unknown connection", |s| {
                s.bots[0].connection_id = "elsewhere".into()
            }),
            ("repeated bot", |s| s.bots.push(s.bots[0].clone())),
            ("login", |s| s.bots[0].bot_login = "bot is:open".into()),
            ("tracker", |s| s.bots[0].tracker = "o/../r".into()),
            ("board elsewhere", |s| {
                s.bots[0].board_url = "https://evil.example/orgs/example-org/projects/1".into()
            }),
            ("board that isn't one", |s| {
                s.bots[0].board_url = "https://github.com/example-org/r/issues/1".into()
            }),
            ("session", |s| heartbeat(s).session = "a b".into()),
            ("loop state", |s| {
                heartbeat(s).loop_state = "\u{1b}[2J".into()
            }),
            ("comment elsewhere", |s| {
                heartbeat(s).comment_url = Some("https://evil.example/".into())
            }),
            ("too many runs", |s| {
                let run = heartbeat(s).runs[0].clone();
                heartbeat(s).runs = (0..=bot::MAX_WORKERS)
                    .map(|n| crate::zone::ExternalRun {
                        name: format!("run-{n}"),
                        ..run.clone()
                    })
                    .collect();
            }),
            ("repeated run", |s| {
                let run = heartbeat(s).runs[0].clone();
                heartbeat(s).runs.push(run);
            }),
            ("run elsewhere", |s| {
                heartbeat(s).runs[0].item_url = "https://evil.example/example-org/r/issues/1".into()
            }),
            ("run's key that isn't its link's", |s| {
                heartbeat(s).runs[0].item_key = "example-org/r#2".into()
            }),
            ("run status", |s| {
                heartbeat(s).runs[0].status = "Working!".into()
            }),
            ("devspace", |s| {
                heartbeat(s).runs[0].devspace = Some("../x".into())
            }),
            ("counts that overflow", |s| {
                board(s).statuses = [
                    ("Todo".into(), usize::MAX / 2 + 1),
                    ("Done".into(), usize::MAX / 2 + 1),
                ]
                .into()
            }),
            ("count over the most items", |s| {
                board(s).statuses.insert("Todo".into(), MAX_COUNT + 1);
            }),
            ("fork pull requests over the most items", |s| {
                board(s).fork_prs = usize::MAX
            }),
            ("unreadable items over the most items", |s| {
                board(s).unreadable = MAX_COUNT + 1
            }),
        ];
        for (what, break_it) in cases {
            let mut s = bot_export();
            break_it(&mut s);
            assert!(checked(s, NOW).is_err(), "{what}");
        }
    }

    fn status(stale: bool) -> Status {
        Status {
            url: "https://hub.example.ts.net/".into(),
            sync: SyncState::default(),
            schedule: Schedule {
                interval_secs: 300,
                running: false,
                needs_user: false,
                stale,
            },
            connections: 1,
            items: 2,
        }
    }

    /// The companion's own connection to `account` on GitHub, whose ID it
    /// doesn't know.
    fn own(account: &str) -> Connection {
        Connection {
            id: "c0ffee00-0000-4000-8000-000000000001".into(),
            provider: "github".into(),
            base_url: "https://github.com/".into(),
            account_name: account.into(),
            ..Connection::default()
        }
    }

    #[test]
    fn the_hubs_work_joins_the_board_marked_and_read_only() {
        let mut board = Snapshot {
            connections: vec![own("me")],
            items: vec![],
            efforts: vec![],
        };
        merge(&mut board, &export(2), &status(false));
        let [_, c] = &board.connections[..] else {
            panic!("{board:?}")
        };
        assert_eq!(
            (
                c.id.as_str(),
                c.from_hub,
                c.zone,
                c.schedule.stale,
                c.account_id.as_str()
            ),
            ("hub:hub-connection", true, Zone::External, false, "7")
        );
        let items: Vec<_> = board
            .items
            .iter()
            .map(|i| {
                (
                    i.id.as_str(),
                    i.connection_id.as_deref(),
                    i.effort_id.as_deref(),
                    i.from_hub,
                )
            })
            .collect();
        assert_eq!(
            items,
            [
                (
                    "hub:hub-connection:github:0",
                    Some("hub:hub-connection"),
                    Some("hub:hub-effort"),
                    true
                ),
                (
                    "hub:hub-connection:github:1",
                    Some("hub:hub-connection"),
                    None,
                    true
                ),
            ]
        );
        assert!(board.items.iter().all(|i| is_from_hub(&i.id) && !i.stale));
        assert_eq!(
            board.items[0].synced_at,
            export(0).connections[0].last_success
        );
        let [e] = &board.efforts[..] else { panic!() };
        assert_eq!(
            (e.effort.id.as_str(), e.from_hub, e.progress.total),
            ("hub:hub-effort", true, 1)
        );
        // The Mine view shows them as the hub's account's.
        let view = crate::mine::mine(&board, &[]);
        assert_eq!(view.mine.len(), 2);
        assert!(view
            .mine
            .iter()
            .all(|r| r.from_hub && r.waits_on[0].account == "someone"));
    }

    #[test]
    fn the_hubs_work_is_stale_whenever_the_hub_is() {
        for (hub_stale, connection_stale) in [(false, false), (true, false), (false, true)] {
            let mut export = export(1);
            export.connections[0].stale = connection_stale;
            let mut board = Snapshot {
                connections: vec![],
                items: vec![],
                efforts: vec![],
            };
            merge(&mut board, &export, &status(hub_stale));
            let stale = hub_stale || connection_stale;
            assert_eq!(board.connections[0].schedule.stale, stale);
            assert_eq!(board.items[0].stale, stale);
        }
    }

    #[test]
    fn an_account_the_companion_connects_too_shows_once_its_own_copy() {
        // (the companion's own account, whether the hub's copy shows)
        for (account, shown) in [
            ("someone", false),
            ("SomeOne", false),
            ("someone-else", true),
        ] {
            let mut board = Snapshot {
                connections: vec![own(account)],
                items: vec![],
                efforts: vec![],
            };
            merge(&mut board, &export(2), &status(false));
            let from_hub = |n: usize| if shown { n } else { 0 };
            assert_eq!(
                board.connections.iter().filter(|c| c.from_hub).count(),
                from_hub(1),
                "{account}"
            );
            assert_eq!(board.items.len(), from_hub(2), "{account}");
            // An effort whose work is all left out isn't shown empty.
            assert_eq!(board.efforts.len(), from_hub(1), "{account}");
        }
        // Another instance is another account.
        let mut board = Snapshot {
            connections: vec![Connection {
                base_url: "https://github.example.com/".into(),
                ..own("someone")
            }],
            items: vec![],
            efforts: vec![],
        };
        merge(&mut board, &export(2), &status(false));
        assert_eq!(board.items.len(), 2);
    }

    #[test]
    fn the_account_id_decides_when_both_sides_know_it() {
        // (the companion's own account ID and name, the hub's ID, whether
        // the hub's copy shows)
        for (mine, name, theirs, shown) in [
            ("7", "someone", Some("7"), false),
            // Renamed since: the same account.
            ("7", "renamed", Some("7"), false),
            // Another account, which took the name a rename freed.
            ("8", "someone", Some("7"), true),
            // A hub that doesn't export IDs: the name decides.
            ("8", "someone", None, false),
            ("8", "someone-else", None, true),
            // An ID that isn't numeric: the name decides.
            ("JIRAUSER7", "someone", Some("7"), false),
            ("JIRAUSER7", "someone-else", Some("7"), true),
        ] {
            let mut export = export(1);
            export.connections[0].account_id = theirs.map(str::to_string);
            let mut board = Snapshot {
                connections: vec![Connection {
                    account_id: mine.into(),
                    ..own(name)
                }],
                items: vec![],
                efforts: vec![],
            };
            merge(&mut board, &export, &status(false));
            assert_eq!(
                board.items.len(),
                usize::from(shown),
                "{mine} {name} {theirs:?}"
            );
        }
    }

    #[test]
    fn hub_addresses() {
        for (raw, expected) in [
            (
                "https://hub.example.ts.net",
                Some("https://hub.example.ts.net/"),
            ),
            (
                "https://hub.example.ts.net/workboard",
                Some("https://hub.example.ts.net/workboard/"),
            ),
            ("http://127.0.0.1:8422/", Some("http://127.0.0.1:8422/")),
            ("http://localhost:8422", Some("http://localhost:8422/")),
            ("http://[::1]:8422/", Some("http://[::1]:8422/")),
            // The token would cross a network in the clear.
            ("http://hub.example.ts.net/", None),
            ("http://127.0.0.2/", None),
            ("https://me:secret@hub.example.ts.net/", None),
            ("https://hub.example.ts.net/?x=1", None),
            ("https://hub.example.ts.net/#token=x", None),
            ("ftp://hub.example.ts.net/", None),
            ("hub.example.ts.net", None),
        ] {
            assert_eq!(
                hub_url(raw).ok().map(String::from).as_deref(),
                expected,
                "{raw}"
            );
        }
    }

    #[test]
    fn only_the_read_of_the_export_and_the_sign_in_pass() {
        let base = hub_url("https://hub.example.ts.net/").unwrap();
        let request = |method: Method, url: &str, body: Option<&str>| {
            let mut request = Request::new(method, Url::parse(url).unwrap());
            *request.body_mut() = body.map(|b| b.to_string().into());
            request
        };
        const EXPORT_URL: &str = "https://hub.example.ts.net/api/export/external";
        for (n, (method, url, body, passes)) in [
            (Method::GET, EXPORT_URL, None, true),
            (Method::GET, EXPORT_URL, Some("{}"), false),
            (Method::HEAD, EXPORT_URL, None, false),
            (Method::POST, EXPORT_URL, Some("{}"), false),
            (Method::POST, EXPORT_URL, None, false),
            (
                Method::POST,
                "https://hub.example.ts.net/auth/device",
                None,
                true,
            ),
            (
                Method::POST,
                "https://hub.example.ts.net/auth/device/poll",
                None,
                true,
            ),
            (
                Method::POST,
                "https://hub.example.ts.net/auth/device",
                Some("{}"),
                false,
            ),
            (
                Method::POST,
                "https://hub.example.ts.net/auth/device/poll",
                Some("x"),
                false,
            ),
            (
                Method::GET,
                "https://hub.example.ts.net/auth/device",
                None,
                false,
            ),
            (
                Method::PUT,
                "https://hub.example.ts.net/auth/device",
                None,
                false,
            ),
            (
                Method::POST,
                "https://hub.example.ts.net/auth/github/login",
                None,
                false,
            ),
            (
                Method::POST,
                "https://elsewhere.example/auth/device",
                None,
                false,
            ),
            (
                Method::POST,
                "https://hub.example.ts.net/auth/device?x=1",
                None,
                false,
            ),
            (Method::PUT, EXPORT_URL, None, false),
            (Method::DELETE, EXPORT_URL, None, false),
            (
                Method::POST,
                "https://hub.example.ts.net/api/items",
                Some("{}"),
                false,
            ),
            (
                Method::GET,
                "https://hub.example.ts.net/api/snapshot",
                None,
                false,
            ),
            (
                Method::GET,
                "https://hub.example.ts.net/api/export/external?zone=internal",
                None,
                false,
            ),
            (
                Method::GET,
                "http://hub.example.ts.net/api/export/external",
                None,
                false,
            ),
            (
                Method::GET,
                "https://hub.example.ts.net:8443/api/export/external",
                None,
                false,
            ),
            (
                Method::GET,
                "https://elsewhere.example/api/export/external",
                None,
                false,
            ),
            (
                Method::GET,
                "https://me:x@hub.example.ts.net/api/export/external",
                None,
                false,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let checked = check(&base, &request(method.clone(), url, body));
            assert_eq!(
                checked.is_ok(),
                passes,
                "case {n}: {method} {url}: {checked:?}"
            );
        }
    }

    /// What reached a fixture hub: each request's method, path and query,
    /// authorization and body.
    type Seen = Arc<Mutex<Vec<(String, String, String, Vec<u8>)>>>;

    /// A fixture hub on loopback that records what reaches it, and answers
    /// `answer`.
    async fn fixture<F>(answer: F) -> (Url, Seen)
    where
        F: Fn(&str) -> Response + Clone + Send + Sync + 'static,
    {
        let seen = Seen::default();
        let handler = move |State(seen): State<Seen>,
                            method: AxumMethod,
                            uri: Uri,
                            headers: Headers,
                            body: axum::body::Bytes| {
            let answer = answer.clone();
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                seen.lock().unwrap().push((
                    method.to_string(),
                    uri.to_string(),
                    auth,
                    body.to_vec(),
                ));
                answer(uri.path())
            }
        };
        let router = Router::new().fallback(handler).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = hub_url(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, seen)
    }

    fn token() -> Credential {
        Credential::new("test-only-companion").unwrap()
    }

    #[tokio::test]
    async fn a_pull_is_one_get_of_the_export_with_the_token() {
        let (base, seen) = fixture(|_| axum::Json(export(2)).into_response()).await;
        let client = HubClient::new().unwrap();
        let got = client.pull(&base, &token()).await.unwrap();
        assert_eq!(got.items.len(), 2);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            [(
                "GET".to_string(),
                "/api/export/external".to_string(),
                "Bearer test-only-companion".to_string(),
                vec![]
            )]
        );
        let sent: Vec<_> = client.requests().unwrap().sent.into_iter().collect();
        assert_eq!(sent, [("GET".to_string(), 1)]);
    }

    #[tokio::test]
    async fn redirects_refusals_and_anything_not_external_fail_the_pull() {
        let mut internal = serde_json::to_value(export(1)).unwrap();
        internal["items"][0]["zone"] = "internal".into();
        let mut stray = serde_json::to_value(export(1)).unwrap();
        stray["items"][0]["connection_id"] = "elsewhere".into();
        // (answer, kind)
        let cases: Vec<(Response, ErrorKind)> = vec![
            (
                (Code::FOUND, [("location", "https://elsewhere.example/")]).into_response(),
                ErrorKind::Invalid,
            ),
            (Code::UNAUTHORIZED.into_response(), ErrorKind::Auth),
            (Code::FORBIDDEN.into_response(), ErrorKind::Auth),
            (Code::BAD_GATEWAY.into_response(), ErrorKind::Transient),
            (axum::Json(internal).into_response(), ErrorKind::Invalid),
            (axum::Json(stray).into_response(), ErrorKind::Invalid),
            ("not json".into_response(), ErrorKind::Invalid),
        ];
        for (n, (answer, kind)) in cases.into_iter().enumerate() {
            let answer = Arc::new(Mutex::new(Some(answer)));
            let (base, seen) = fixture(move |_| {
                answer
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap_or_else(|| Code::GONE.into_response())
            })
            .await;
            let error = HubClient::new()
                .unwrap()
                .pull(&base, &token())
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind, "case {n}: {error}");
            // One request, and a redirect never followed.
            assert_eq!(seen.lock().unwrap().len(), 1, "case {n}");
        }
    }

    #[tokio::test]
    async fn sign_ins_are_two_posts_without_a_body_and_their_answers_are_checked() {
        let start = serde_json::json!({
            "handle": "test-only-handle",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5,
        });
        let answers = Arc::new(Mutex::new(vec![
            axum::Json(serde_json::json!({"status": "done", "token": "test-only-credential", "login": "octo\u{1b}cat"})).into_response(),
            axum::Json(serde_json::json!({"status": "pending", "interval": 10})).into_response(),
            axum::Json(start.clone()).into_response(),
        ]));
        let (base, seen) = fixture(move |_| answers.lock().unwrap().pop().unwrap()).await;
        let client = HubClient::new().unwrap();
        let started = client.start_login(&base).await.unwrap();
        assert_eq!(started.user_code, "ABCD-1234");
        let handle = Credential::new(&started.handle).unwrap();
        let pending = client.poll_login(&base, &handle).await.unwrap();
        assert_eq!(
            pending,
            DevicePoll::Pending {
                interval: 10,
                approve: false
            }
        );
        let done = client.poll_login(&base, &handle).await.unwrap();
        assert_eq!(
            done,
            DevicePoll::Done {
                token: "test-only-credential".into(),
                login: "octocat".into()
            }
        );
        let seen = seen.lock().unwrap().clone();
        let sent: Vec<_> = seen
            .iter()
            .map(|(method, path, auth, body)| {
                (method.as_str(), path.as_str(), auth.as_str(), body.len())
            })
            .collect();
        assert_eq!(
            sent,
            [
                ("POST", "/auth/device", "", 0),
                ("POST", "/auth/device/poll", "Bearer test-only-handle", 0),
                ("POST", "/auth/device/poll", "Bearer test-only-handle", 0),
            ]
        );
        // What a hub may not start a sign-in with.
        for (key, value) in [
            ("verification_uri", serde_json::json!("javascript:alert(1)")),
            (
                "verification_uri",
                serde_json::json!("http://github.com/login/device"),
            ),
            (
                "verification_uri",
                serde_json::json!("https://github.com.example/login/device"),
            ),
            (
                "verification_uri",
                serde_json::json!("https://github.com/login/device?x=1"),
            ),
            ("user_code", serde_json::json!("<b>")),
            ("interval", serde_json::json!(0)),
            ("expires_in", serde_json::json!(86400)),
            ("handle", serde_json::json!("")),
        ] {
            let mut bad = start.clone();
            bad[key] = value.clone();
            let (base, _) = fixture(move |_| axum::Json(bad.clone()).into_response()).await;
            assert!(client.start_login(&base).await.is_err(), "{key}: {value}");
        }
        // A refusal says why, in the hub's words, without control characters.
        let (base, _) = fixture(|_| {
            (Code::FORBIDDEN, axum::Json(serde_json::json!({"error": "That GitHub account isn't allowed\u{7} on this hub"}))).into_response()
        })
        .await;
        let refused = client.poll_login(&base, &handle).await.unwrap_err();
        assert_eq!(refused.kind, ErrorKind::Auth);
        assert_eq!(
            refused.message,
            "The hub refused the sign-in: That GitHub account isn't allowed on this hub"
        );
    }

    #[test]
    fn what_passes_has_parsed_links_and_printable_text() {
        let mut s = export(1);
        s.items[0].url = "HTTPS://GitHub.com/o/r/pull/0".into();
        s.items[0].title = "Change \u{1b}[31m0\u{7}".into();
        s.connections[0].account_name = "some\none".into();
        s.efforts[0].title = "Effort\r".into();
        let s = checked(s, NOW).unwrap();
        assert_eq!(s.items[0].url, "https://github.com/o/r/pull/0");
        assert_eq!(s.items[0].title, "Change [31m0");
        assert_eq!(s.connections[0].account_name, "someone");
        assert_eq!(s.efforts[0].title, "Effort");
    }

    #[test]
    fn exports_are_checked_whole() {
        type Break = fn(&mut ExternalSnapshot);
        let cases: [(&str, Break); 19] = [
            ("snapshot zone", |s| s.zone = Zone::Internal),
            ("connection zone", |s| {
                s.connections[0].zone = Zone::Internal
            }),
            ("item zone", |s| s.items[1].zone = Zone::Internal),
            ("unknown provider", |s| {
                s.items[0].provider = "bugzilla".into()
            }),
            ("script link", |s| {
                s.items[0].url = "javascript:alert(1)".into()
            }),
            ("plain base", |s| {
                s.connections[0].base_url = "http://github.com/".into()
            }),
            ("unknown effort", |s| {
                s.items[0].effort_id = Some("elsewhere".into())
            }),
            ("repeated item", |s| s.items[1].id = s.items[0].id.clone()),
            ("repeated connection", |s| {
                s.connections.push(s.connections[0].clone())
            }),
            ("empty ID", |s| s.efforts[0].id = String::new()),
            ("path in an item ID", |s| {
                s.items[0].id = "a/../../hub".into()
            }),
            ("path in a connection ID", |s| {
                s.connections[0].id = "../hub".into();
                for i in &mut s.items {
                    i.connection_id = "../hub".into();
                }
            }),
            ("encoded path in an effort ID", |s| {
                s.efforts[0].id = "%2e%2e%2fhub".into();
                s.items[0].effort_id = Some("%2e%2e%2fhub".into());
            }),
            ("space in an ID", |s| s.items[0].id = "a b".into()),
            ("dots for an item ID", |s| s.items[0].id = "..".into()),
            ("a dot for an effort ID", |s| {
                s.efforts[0].id = ".".into();
                s.items[0].effort_id = Some(".".into());
            }),
            ("overlong title", |s| {
                s.items[0].title = "x".repeat(MAX_FIELD + 1)
            }),
            ("account ID that isn't a number", |s| {
                s.connections[0].account_id = Some("someone@example.com".into())
            }),
            ("overlong account ID", |s| {
                s.connections[0].account_id = Some("1".repeat(21))
            }),
        ];
        assert!(checked(export(2), NOW).is_ok());
        for (what, break_it) in cases {
            let mut s = export(2);
            break_it(&mut s);
            assert!(checked(s, NOW).is_err(), "{what}");
        }
    }
}
