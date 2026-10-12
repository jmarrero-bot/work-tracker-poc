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
    pub status: String,
    pub terminal: bool,
    pub action: String,
    pub updated_at: String,
    pub effort_id: Option<String>,
    pub demo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Effort {
    pub id: String,
    pub title: String,
    pub note: String,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub connections: Vec<Connection>,
    pub items: Vec<Item>,
    pub efforts: Vec<Effort>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ConnectInput {
    pub provider: String,
    pub label: String,
    pub base_url: String,
    pub token: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EffortInput {
    pub title: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub item_ids: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ManualInput {
    pub provider: String,
    pub key: String,
    pub title: String,
    pub url: String,
    pub status: String,
    pub action: String,
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
