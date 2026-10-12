//! Zones: where a connection's work may be seen. Internal work (Jira,
//! internal GitLab, Errata) stays on the machine that syncs it; only external
//! work may leave it, for the hub.

use serde::{Deserialize, Serialize};

/// The zone of a connection, and of every item synced through it. A person
/// chooses it when connecting; it isn't a property of the provider, since
/// the same GitLab code serves internal and public instances. Internal is the
/// default, so nothing becomes external without that explicit choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Zone {
    /// Seen only on the machine that syncs it.
    #[default]
    Internal,
    /// May also be seen away from that machine, on the hub.
    External,
}

impl Zone {
    pub fn label(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::External => "external",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConnectInput, Connection, Item};

    #[test]
    fn zones_default_to_internal() {
        let connect: ConnectInput = serde_json::from_value(serde_json::json!({
            "provider": "gitlab-public",
            "label": "GitLab",
            "base_url": "https://gitlab.com/",
            "token": "test-only-token",
        }))
        .unwrap();
        assert_eq!(connect.zone, Zone::Internal);
        // Rows written before zones existed.
        let connection: Connection = serde_json::from_value(serde_json::json!({
            "id": "c1", "provider": "gitlab-public", "label": "GitLab",
            "base_url": "https://gitlab.com/", "account_id": "1", "account_name": "me",
            "last_sync": null, "error": null,
        }))
        .unwrap();
        assert_eq!(connection.zone, Zone::Internal);
        let item: Item = serde_json::from_value(serde_json::json!({
            "id": "c1:gitlab:7:4", "connection_id": "c1", "provider": "gitlab-public",
            "remote_id": "7:4", "key": "!4", "title": "Fix", "url": "https://gitlab.com/p/-/merge_requests/4",
            "status": "opened", "outcome": "open", "outcome_reason": "", "action": "next",
            "updated_at": "", "effort_id": null, "demo": false,
        }))
        .unwrap();
        assert_eq!(item.zone, Zone::Internal);
        let explicit: ConnectInput = serde_json::from_value(serde_json::json!({
            "provider": "gitlab-public", "label": "GitLab", "base_url": "https://gitlab.com/",
            "token": "test-only-token", "zone": "external",
        }))
        .unwrap();
        assert_eq!(explicit.zone, Zone::External);
    }
}
