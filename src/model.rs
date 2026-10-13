pub use crate::{
    bot::{BotConfig, BotFields, BotInput},
    clock::Timestamp,
    connector::{ConnectorError, ErrorKind, Reason},
    connectors::PluginConfig,
    outcome::{Outcome, Progress},
    zone::Zone,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub base_url: String,
    pub account_id: String,
    pub account_name: String,
    /// Chosen when connecting. Connections made before zones existed are
    /// internal.
    #[serde(default)]
    pub zone: Zone,
    /// Another GitHub user whose activity, as far as this connection's
    /// token can see it, the connection also watches, a setting of its own:
    /// what it finds is theirs, never this account's.
    #[serde(default)]
    pub watch_user: Option<String>,
    /// How often it syncs, in seconds, if not as often as the service's
    /// default.
    #[serde(default)]
    pub sync_interval: Option<u64>,
    /// For the bot connector: where the bot's board, tracker and heartbeat
    /// are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<BotConfig>,
    /// For a source served by a connector plugin: which plugin file it
    /// trusts. A plugin connection made before plugins were pinned has
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginConfig>,
    /// What its syncs did. Kept in a table of its own, saved with what a
    /// sync found, and never stored with the connection.
    #[serde(default)]
    pub sync: SyncState,
    /// How its syncs are scheduled, worked out when it is shown and never
    /// stored.
    #[serde(default)]
    pub schedule: Schedule,
    /// It is the hub's, shown read-only on its companion. Never stored.
    #[serde(default)]
    pub from_hub: bool,
}

/// How a connection's syncs are scheduled now, and how fresh its items are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// How often it syncs, in seconds: its own interval, or the service's.
    pub interval_secs: u64,
    /// A sync of it is running.
    pub running: bool,
    /// Its credential was rejected, so its scheduled syncs stopped until the
    /// person verifies it again, gives it a new credential, or reconnects.
    pub needs_user: bool,
    /// It never synced, or its last success was more than a few intervals
    /// ago, so its items can't be shown as current.
    pub stale: bool,
}

impl Default for Schedule {
    /// Until it is worked out, nothing says its items are current.
    fn default() -> Self {
        Self {
            interval_secs: 0,
            running: false,
            needs_user: false,
            stale: true,
        }
    }
}

/// What a connection's syncs did: what is saved, in the same transaction as
/// the items a sync found, once it has processed them. A sync that never got
/// that far, because it failed or the service stopped, leaves it as it was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    /// When the last sync whose results were saved started: how fresh the
    /// connection's items are.
    pub last_success: Option<Timestamp>,
    /// When the last sync started, whether or not it succeeded.
    pub last_attempt: Option<Timestamp>,
    /// When the next scheduled sync is due. None until one was scheduled,
    /// and while the connection needs the person.
    pub next_due: Option<Timestamp>,
    /// Syncs that failed since the last one that succeeded.
    pub failures: u32,
    /// The kind of the last sync's error, which unlike its message may
    /// leave this machine.
    pub error_kind: Option<ErrorKind>,
    pub error: Option<String>,
    /// Until when the source asked for no more requests, after a rate
    /// limit.
    pub held_until: Option<Timestamp>,
}

impl SyncState {
    /// After a sync that started at `at` saved what it found.
    pub fn succeeded(&self, at: Timestamp) -> Self {
        Self {
            last_success: Some(at),
            last_attempt: Some(at),
            ..Self::default()
        }
    }
    /// After a sync that started at `at` failed with `error`. Its items stay
    /// as the last success left them.
    pub fn failed(&self, at: Timestamp, error: &anyhow::Error) -> Self {
        let connector = error.downcast_ref::<ConnectorError>();
        Self {
            last_success: self.last_success,
            last_attempt: Some(at),
            next_due: None,
            failures: self.failures.saturating_add(1),
            // Whether to fix a credential or retry later.
            error_kind: Some(connector.map_or(ErrorKind::Invalid, |e| e.kind)),
            error: Some(error.to_string()),
            held_until: connector
                .and_then(|e| e.hold_until)
                .map(Timestamp::of)
                .filter(|until| *until > at),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub connection_id: Option<String>,
    pub provider: String,
    pub remote_id: String,
    pub key: String,
    pub title: String,
    pub url: String,
    /// The source's own status, unmodified.
    pub status: String,
    pub outcome: Outcome,
    /// Why the outcome was chosen.
    pub outcome_reason: String,
    pub action: String,
    pub updated_at: String,
    pub effort_id: Option<String>,
    /// When a person marked this synced item abandoned in Workboard. Kept
    /// through syncs, like the effort link.
    #[serde(default)]
    pub abandoned_at: Option<String>,
    pub demo: bool,
    /// Its connection's zone, set when reading it. Manual items have no
    /// connection and are internal.
    #[serde(default)]
    pub zone: Zone,
    /// Why it waits on its connection's account, as of the last sync.
    #[serde(default)]
    pub reasons: Vec<Reason>,
    /// When its connection last synced successfully, which is how fresh
    /// it is. Set when it is shown; none for manual items.
    #[serde(default)]
    pub synced_at: Option<Timestamp>,
    /// Whether that was too long ago for it to be current. Set when it is
    /// shown.
    #[serde(default)]
    pub stale: bool,
    /// It came from the hub, and is read-only here. Never stored.
    #[serde(default)]
    pub from_hub: bool,
    /// What the bot connector read besides: a board item's fields, or the
    /// board item that links a fork pull request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<BotFields>,
}

impl Item {
    /// Set a derived outcome. Only open items keep an attention category.
    pub fn set_outcome(&mut self, (outcome, reason): crate::outcome::Derived) {
        self.outcome = outcome;
        self.outcome_reason = reason;
        if outcome != Outcome::Open {
            self.action.clear();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Effort {
    pub id: String,
    pub title: String,
    pub note: String,
    /// Whether the title may leave this machine with the effort's external
    /// items. A title can describe internal work, so it is off unless a
    /// person turns it on.
    #[serde(default)]
    pub share_title: bool,
}

/// An effort with its progress, as the snapshot shows it.
#[derive(Debug, Serialize, Deserialize)]
pub struct EffortView {
    #[serde(flatten)]
    pub effort: Effort,
    pub progress: Progress,
    /// It came from the hub, and is read-only here.
    #[serde(default)]
    pub from_hub: bool,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub connections: Vec<Connection>,
    pub items: Vec<Item>,
    pub efforts: Vec<EffortView>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ConnectInput {
    pub provider: String,
    pub label: String,
    pub base_url: String,
    pub token: String,
    /// Internal unless the person explicitly chooses external.
    #[serde(default)]
    pub zone: Zone,
    /// Another GitHub user to watch as well.
    #[serde(default)]
    pub watch_user: Option<String>,
    /// How often it syncs, in seconds, if not as often as the service's
    /// default.
    #[serde(default)]
    pub sync_interval: Option<u64>,
    /// For the bot connector: where the bot's board, tracker and heartbeat
    /// are.
    #[serde(default)]
    pub bot: Option<BotInput>,
}

/// The items a connection found by watching another user, as of its last
/// sync. They wait on that user, never on the connection's account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchList {
    pub connection_id: String,
    /// Who is watched.
    pub login: String,
    pub items: Vec<Item>,
    /// Why the last sync couldn't update the list, which then is what an
    /// earlier sync found. Never the account's own sync error.
    #[serde(default)]
    pub error: Option<String>,
    /// When the sync that found these items started: how fresh the list is,
    /// apart from the account's own items, since a watch can keep failing
    /// while the account syncs.
    #[serde(default)]
    pub last_success: Option<Timestamp>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EffortInput {
    pub title: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub item_ids: Vec<String>,
    #[serde(default)]
    pub share_title: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ManualInput {
    pub provider: String,
    pub key: String,
    pub title: String,
    pub url: String,
    pub status: String,
    pub action: String,
    #[serde(default)]
    pub outcome: Outcome,
}

pub fn is_gitlab(provider: &str) -> bool {
    matches!(provider, "gitlab-internal" | "gitlab-public")
}
/// Whether `login` can be a GitHub login: letters, digits and hyphens, not
/// at either end. Anything else could change the searches it goes into.
pub fn valid_github_login(login: &str) -> bool {
    (1..=39).contains(&login.len())
        && !login.starts_with('-')
        && !login.ends_with('-')
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Whether `name` can be a GitHub owner or repository name, and so is safe
/// in an API path.
pub fn valid_github_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

pub fn valid_provider(provider: &str) -> bool {
    is_gitlab(provider) || matches!(provider, "jira" | "github" | "fedora" | "errata" | "bot")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_logins() {
        let (longest, too_long) = ("a".repeat(39), "a".repeat(40));
        for (login, valid) in [
            ("jmarrero", true),
            ("jmarrero-bot", true),
            ("a", true),
            (longest.as_str(), true),
            (too_long.as_str(), false),
            ("", false),
            ("-a", false),
            ("a-", false),
            ("a b", false),
            ("a is:closed", false),
            ("a,b", false),
            ("@me", false),
            ("a_b", false),
        ] {
            assert_eq!(valid_github_login(login), valid, "{login}");
        }
    }
}
