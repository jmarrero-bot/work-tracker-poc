//! Jira Data Center: a personal access token as a bearer token, and the
//! issues assigned to the account that aren't done.

use super::{account, inaccessible, source_url, string, Auth, Fetched, Http};
use crate::{
    connector::{
        Account, BoxFuture, Connector, ConnectorError, ConnectorResult, Reason, Refreshed,
        RemoteItem, Target, Tracked,
    },
    outcome,
};
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;
use url::Url;

/// The Jira issue fields an item is built from.
const FIELDS: &str = "summary,status,resolution,updated";
const SOURCE: &str = "Jira";

pub struct Jira {
    http: Http,
}

impl Jira {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: Http::new(Auth::Bearer)?,
        })
    }
    async fn current_user(&self, target: &Target) -> ConnectorResult<Account> {
        let v = self.http.get(target, "rest/api/2/myself", &[]).await?;
        let id = v["key"]
            .as_str()
            .or(v["accountId"].as_str())
            .map(str::to_string);
        account(&v, id)
    }
    async fn assigned(&self, target: &Target) -> ConnectorResult<Vec<RemoteItem>> {
        let mut all = BTreeMap::<String, RemoteItem>::new();
        let mut start = 0usize;
        for _ in 0..100 {
            let v = self
                .http
                .get(
                    target,
                    "rest/api/2/search",
                    &[
                        (
                            "jql",
                            "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC".into(),
                        ),
                        ("fields", FIELDS.into()),
                        ("maxResults", "100".into()),
                        ("startAt", start.to_string()),
                    ],
                )
                .await?;
            let rows = v["issues"]
                .as_array()
                .ok_or_else(|| ConnectorError::invalid("Unexpected Jira search response"))?;
            for row in rows {
                let mut item =
                    jira_item(&target.base_url, row, "next").map_err(ConnectorError::invalid)?;
                item.reasons = vec![Reason::Assigned];
                all.insert(item.remote_id.clone(), item);
            }
            start += rows.len();
            let total = v["total"]
                .as_u64()
                .ok_or_else(|| ConnectorError::invalid("Jira search response has no total"))?
                as usize;
            if start >= total {
                return Ok(all.into_values().collect());
            }
            if rows.is_empty() {
                return Err(ConnectorError::invalid(
                    "Jira pagination stopped before all items were read",
                ));
            }
        }
        Err(ConnectorError::invalid(
            "Too many Jira issues for one sync; no partial results were saved",
        ))
    }
    async fn issues(
        &self,
        target: &Target,
        tracked: &[Tracked],
    ) -> ConnectorResult<Vec<Refreshed>> {
        let mut answers = Vec::new();
        for item in tracked {
            let key = item
                .key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-');
            if !key {
                return Err(ConnectorError::invalid("Invalid stored Jira key"));
            }
            let fetched = self
                .http
                .fetch(
                    target,
                    &format!("rest/api/2/issue/{}", item.key),
                    &[("fields", FIELDS.into())],
                )
                .await?;
            answers.push(match fetched {
                Fetched::Found(row) => Refreshed::found(
                    &item.remote_id,
                    jira_item(&target.base_url, &row, "follow-up")
                        .map_err(ConnectorError::invalid)?,
                ),
                Fetched::Inaccessible(status) => inaccessible(SOURCE, &item.remote_id, status),
            });
        }
        Ok(answers)
    }
}

impl Connector for Jira {
    fn kind(&self) -> &str {
        "jira"
    }
    fn verify<'a>(&'a self, target: &'a Target) -> BoxFuture<'a, ConnectorResult<Account>> {
        Box::pin(self.current_user(target))
    }
    fn list<'a>(
        &'a self,
        target: &'a Target,
        _account: &'a Account,
    ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
        Box::pin(self.assigned(target))
    }
    fn refresh<'a>(
        &'a self,
        target: &'a Target,
        _account: &'a Account,
        tracked: &'a [Tracked],
    ) -> BoxFuture<'a, ConnectorResult<Vec<Refreshed>>> {
        Box::pin(self.issues(target, tracked))
    }
}

pub fn jira_item(base: &Url, v: &Value, action: &str) -> Result<RemoteItem> {
    let remote_id = string(v, "id")?;
    let key = string(v, "key")?;
    let status = string(&v["fields"]["status"], "name")?;
    let (outcome, outcome_reason) = outcome::jira(
        &status,
        v["fields"]["status"]["statusCategory"]["key"].as_str(),
        v["fields"]["resolution"]["name"].as_str(),
    );
    let url = base.join(&format!("browse/{key}"))?.to_string();
    Ok(RemoteItem {
        remote_id,
        key,
        title: string(&v["fields"], "summary")?,
        url: source_url(&url)?,
        status,
        outcome,
        outcome_reason,
        action: action.into(),
        updated_at: string(&v["fields"], "updated")?,
        reasons: vec![],
        bot: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        connector,
        connectors::test_connection as c,
        model::{Connection, Item, Outcome},
    };
    /// Build items the way a sync does, through the connector interface.
    fn jira_item(c: &Connection, v: &Value, action: &str) -> Result<Item> {
        let base = Url::parse(&c.base_url)?;
        Ok(connector::to_item(
            c,
            "jira",
            super::jira_item(&base, v, action)?,
        )?)
    }
    #[test]
    fn jira_items_use_category_and_resolution_not_display_name() {
        use Outcome::*;
        let null = Value::Null;
        for (category, resolution, outcome) in [
            ("indeterminate", null.clone(), Open),
            ("done", serde_json::json!({"name":"Done"}), Delivered),
            (
                "done",
                serde_json::json!({"name":"Won't Do"}),
                ClosedWithoutDelivery,
            ),
            ("done", null.clone(), Delivered),
            ("undefined", null, Unknown),
        ] {
            let v = serde_json::json!({"id":"1","key":"RHEL-1","fields":{"summary":"fix","updated":"now","resolution":resolution.clone(),"status":{"name":"Released","statusCategory":{"key":category}}}});
            let i = jira_item(&c(), &v, "next").unwrap();
            assert_eq!(i.outcome, outcome, "{category} {resolution}");
            assert_eq!(i.status, "Released");
            assert_eq!(i.url, "https://example.com/browse/RHEL-1");
            assert_eq!(i.action, if outcome == Open { "next" } else { "" });
        }
    }
    fn issue(n: u64) -> Value {
        serde_json::json!({"id":n.to_string(),"key":format!("RHEL-{n}"),"fields":{"summary":"fix","updated":"now","resolution":null,"status":{"name":"In Progress","statusCategory":{"key":"indeterminate"}}}})
    }
    #[tokio::test]
    async fn assigned_issues_wait_on_the_account() {
        use axum::{extract::Query, routing::get, Json, Router};
        // Two pages, the second overlapping the first, as a search can when
        // an issue changes between requests.
        async fn search(Query(q): Query<std::collections::HashMap<String, String>>) -> Json<Value> {
            assert!(q["jql"].starts_with("assignee = currentUser()"), "{q:?}");
            let issues: Vec<_> = match q["startAt"].as_str() {
                "0" => vec![issue(1), issue(2)],
                _ => vec![issue(2)],
            };
            Json(serde_json::json!({"issues": issues, "total": 3}))
        }
        let router = Router::new()
            .route(
                "/rest/api/2/myself",
                get(|| async { Json(serde_json::json!({"key":"1","name":"me"})) }),
            )
            .route("/rest/api/2/search", get(search));
        let (target, task) = crate::connectors::fixture(router, "test-only-token").await;
        let items = connector::sync(&Jira::new().unwrap(), &target, &c(), &[])
            .await
            .unwrap();
        let listed: Vec<_> = items
            .iter()
            .map(|i| (i.key.as_str(), i.action.as_str(), i.reasons.clone()))
            .collect();
        assert_eq!(
            listed,
            [
                ("RHEL-1", "next", vec![Reason::Assigned]),
                ("RHEL-2", "next", vec![Reason::Assigned]),
            ]
        );
        task.abort();
    }
}
