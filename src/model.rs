pub use crate::{
    connector::{ErrorKind, Reason},
    outcome::{Outcome, Progress},
    zone::Zone,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub base_url: String,
    pub account_id: String,
    pub account_name: String,
    pub last_sync: Option<String>,
    pub error: Option<String>,
    /// Chosen when connecting. Connections made before zones existed are
    /// internal.
    #[serde(default)]
    pub zone: Zone,
    /// The kind of the last sync's error, which unlike its message may
    /// leave this machine.
    #[serde(default)]
    pub error_kind: Option<ErrorKind>,
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
pub fn supported(provider: &str) -> bool {
    provider == "jira" || is_gitlab(provider)
}
pub fn valid_provider(provider: &str) -> bool {
    supported(provider) || matches!(provider, "github" | "fedora" | "errata")
}
