//! GitLab, internal or public: a personal access token in the
//! `PRIVATE-TOKEN` header, and the open merge requests the account authored,
//! is assigned or is asked to review.

use super::{account, inaccessible, source_url, string, Auth, Fetched, Http};
use crate::{
    connector::{
        Account, BoxFuture, Connector, ConnectorError, ConnectorResult, Listing, Reason, Refreshed,
        RemoteItem, Target, Tracked,
    },
    outcome,
};
use anyhow::{Context, Result};
use serde_json::Value;

const SOURCE: &str = "GitLab";

pub struct GitLab {
    http: Http,
}

impl GitLab {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: Http::new(Auth::PrivateToken)?,
        })
    }
    async fn current_user(&self, target: &Target) -> ConnectorResult<Account> {
        let v = self.http.get(target, "api/v4/user", &[]).await?;
        let id = v["id"].as_u64().map(|n| n.to_string());
        account(&v, id)
    }
    async fn merge_requests(
        &self,
        target: &Target,
        account: &Account,
    ) -> ConnectorResult<Vec<RemoteItem>> {
        let mut all = Listing::default();
        for (kind, extra, reason) in [
            ("next", ("scope", "created_by_me".into()), None),
            (
                "next",
                ("scope", "assigned_to_me".into()),
                Some(Reason::Assigned),
            ),
            (
                "review",
                ("reviewer_id", account.id.clone()),
                Some(Reason::ReviewRequested),
            ),
        ] {
            let mut complete = false;
            for page in 1..=100 {
                let query = vec![
                    ("state", "opened".into()),
                    ("scope", "all".into()),
                    extra.clone(),
                    ("per_page", "100".into()),
                    ("page", page.to_string()),
                ];
                // Do not send duplicate scope parameters.
                let query: Vec<_> = query
                    .into_iter()
                    .filter(|(k, v)| !(*k == "scope" && v == "all" && extra.0 == "scope"))
                    .collect();
                let v = self
                    .http
                    .get(target, "api/v4/merge_requests", &query)
                    .await?;
                let rows = v.as_array().ok_or_else(|| {
                    ConnectorError::invalid("Unexpected GitLab merge request list")
                })?;
                for row in rows {
                    let mut item = gitlab_item(row, kind).map_err(ConnectorError::invalid)?;
                    item.reasons.extend(reason);
                    // A review request takes precedence when the MR appears in
                    // several lists, and its reasons add up.
                    all.add(item);
                }
                if rows.len() < 100 {
                    complete = true;
                    break;
                }
            }
            if !complete {
                return Err(ConnectorError::invalid(
                    "Too many merge requests for one sync; no partial results were saved",
                ));
            }
        }
        Ok(all.into_items())
    }
    async fn tracked(
        &self,
        target: &Target,
        tracked: &[Tracked],
    ) -> ConnectorResult<Vec<Refreshed>> {
        let mut answers = Vec::new();
        for item in tracked {
            let (project, iid) = item
                .remote_id
                .split_once(':')
                .ok_or_else(|| ConnectorError::invalid("Invalid stored merge request identity"))?;
            let fetched = self
                .http
                .fetch(
                    target,
                    &format!("api/v4/projects/{project}/merge_requests/{iid}"),
                    &[],
                )
                .await?;
            answers.push(match fetched {
                Fetched::Found(row) => Refreshed::found(
                    &item.remote_id,
                    gitlab_item(&row, "follow-up").map_err(ConnectorError::invalid)?,
                ),
                Fetched::Inaccessible(status) => inaccessible(SOURCE, &item.remote_id, status),
            });
        }
        Ok(answers)
    }
}

impl Connector for GitLab {
    fn kind(&self) -> &str {
        "gitlab"
    }
    fn verify<'a>(&'a self, target: &'a Target) -> BoxFuture<'a, ConnectorResult<Account>> {
        Box::pin(self.current_user(target))
    }
    fn list<'a>(
        &'a self,
        target: &'a Target,
        account: &'a Account,
    ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
        Box::pin(self.merge_requests(target, account))
    }
    fn refresh<'a>(
        &'a self,
        target: &'a Target,
        _account: &'a Account,
        tracked: &'a [Tracked],
    ) -> BoxFuture<'a, ConnectorResult<Vec<Refreshed>>> {
        Box::pin(self.tracked(target, tracked))
    }
}

pub fn gitlab_item(v: &Value, action: &str) -> Result<RemoteItem> {
    let project = v["project_id"].as_u64().context("Missing project ID")?;
    let iid = v["iid"].as_u64().context("Missing MR number")?;
    let state = string(v, "state")?;
    let (outcome, outcome_reason) = outcome::gitlab(&state);
    let draft = v["draft"]
        .as_bool()
        .or(v["work_in_progress"].as_bool())
        .unwrap_or(false);
    Ok(RemoteItem {
        remote_id: format!("{project}:{iid}"),
        key: v["references"]["full"]
            .as_str()
            .map(str::to_string)
            .unwrap_or(format!("!{iid}")),
        title: string(v, "title")?,
        url: source_url(&string(v, "web_url")?)?,
        status: if draft && state == "opened" {
            "Draft".into()
        } else {
            state
        },
        outcome,
        outcome_reason,
        action: action.into(),
        updated_at: string(v, "updated_at")?,
        reasons: vec![],
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
    use reqwest::StatusCode;
    /// Build items the way a sync does, through the connector interface.
    fn gitlab_item(c: &Connection, v: &Value, action: &str) -> Result<Item> {
        Ok(connector::to_item(
            c,
            "gitlab",
            super::gitlab_item(v, action)?,
        )?)
    }
    #[test]
    fn gitlab_items_keep_native_state_next_to_outcome() {
        use Outcome::*;
        for (state, draft, status, outcome, action) in [
            ("opened", false, "opened", Open, "review"),
            ("opened", true, "Draft", Open, "review"),
            ("locked", false, "locked", Open, "review"),
            ("merged", true, "merged", Delivered, ""),
            ("closed", false, "closed", ClosedWithoutDelivery, ""),
            ("closed", true, "closed", ClosedWithoutDelivery, ""),
        ] {
            let v = serde_json::json!({"project_id":7,"iid":4,"title":"fix","state":state,"draft":draft,"web_url":"https://example.com/p/-/merge_requests/4","updated_at":"now"});
            let i = gitlab_item(&c(), &v, "review").unwrap();
            assert_eq!(
                (i.status.as_str(), i.outcome, i.action.as_str()),
                (status, outcome, action),
                "{state} draft={draft}"
            );
            assert_eq!(i.outcome_reason, outcome::gitlab(state).1);
            assert_eq!(i.url, "https://example.com/p/-/merge_requests/4");
        }
    }
    /// The GitLab connector pointed at a fixture.
    struct Fixture {
        connector: GitLab,
        target: Target,
    }
    impl Fixture {
        async fn sync(&self, c: &Connection, old: &[Item]) -> ConnectorResult<Vec<Item>> {
            connector::sync(&self.connector, &self.target, c, old).await
        }
        async fn identity(&self) -> ConnectorResult<Account> {
            self.connector.verify(&self.target).await
        }
    }
    /// The GitLab connector pointed at a fixture serving `router`.
    async fn fixture(router: axum::Router, token: &str) -> (Fixture, tokio::task::JoinHandle<()>) {
        let (target, task) = crate::connectors::fixture(router, token).await;
        let connector = GitLab::new().unwrap();
        (Fixture { connector, target }, task)
    }
    fn mr(iid: u64, state: &str) -> Value {
        serde_json::json!({"project_id":7,"iid":iid,"title":format!("fix {iid}"),"state":state,"web_url":format!("https://example.com/p/-/merge_requests/{iid}"),"updated_at":"2026-10-06T20:00:00Z"})
    }
    #[tokio::test]
    async fn paginates_deduplicates_and_refreshes_merged_items() {
        use axum::{extract::Query, routing::get, Json, Router};
        async fn list(Query(q): Query<std::collections::HashMap<String, String>>) -> Json<Value> {
            if q.contains_key("reviewer_id") {
                return Json(serde_json::json!([mr(1, "opened")]));
            }
            if q.get("scope").map(String::as_str) == Some("assigned_to_me") {
                return Json(serde_json::json!([mr(1, "opened")]));
            }
            let rows = if q.get("page").map(String::as_str) == Some("1") {
                (1..=100).map(|i| mr(i, "opened")).collect::<Vec<_>>()
            } else {
                vec![mr(101, "opened")]
            };
            Json(serde_json::json!(rows))
        }
        let router = Router::new()
            .route(
                "/api/v4/user",
                get(|| async { Json(serde_json::json!({"id":1,"username":"me"})) }),
            )
            .route("/api/v4/merge_requests", get(list))
            .route(
                "/api/v4/projects/7/merge_requests/999",
                get(|| async { Json(mr(999, "merged")) }),
            )
            .route(
                "/api/v4/projects/7/merge_requests/998",
                get(|| async { Json(mr(998, "closed")) }),
            )
            .route(
                "/api/v4/projects/7/merge_requests/997",
                get(|| async { StatusCode::FORBIDDEN }),
            );
        // 996 has no route: GitLab answers 404 for a deleted merge request.
        let (p, task) = fixture(router, "test-only-token").await;
        let old: Vec<_> = [999, 998, 997, 996]
            .into_iter()
            .map(|iid| gitlab_item(&c(), &mr(iid, "opened"), "next").unwrap())
            .collect();
        let items = p.sync(&c(), &old).await.unwrap();
        assert_eq!(items.len(), 105);
        // Listed three times: once, with both reasons and the review first.
        let listed = |remote_id: &str| {
            let i = items.iter().find(|i| i.remote_id == remote_id).unwrap();
            (i.action.clone(), i.reasons.clone())
        };
        assert_eq!(
            listed("7:1"),
            (
                "review".into(),
                vec![Reason::ReviewRequested, Reason::Assigned]
            )
        );
        // Only authored: in the inbox, but not waiting on the account.
        assert_eq!(listed("7:2"), ("next".into(), vec![]));
        for (remote_id, status, outcome, reason) in [
            ("7:999", "merged", Outcome::Delivered, "merged"),
            ("7:998", "closed", Outcome::ClosedWithoutDelivery, "closed"),
            // The last known status stays; the outcome is unknown, never done.
            ("7:997", "opened", Outcome::Unknown, "HTTP 403"),
            ("7:996", "opened", Outcome::Unknown, "HTTP 404"),
        ] {
            let i = items.iter().find(|i| i.remote_id == remote_id).unwrap();
            assert_eq!(
                (i.status.as_str(), i.outcome),
                (status, outcome),
                "{remote_id}"
            );
            assert!(i.outcome_reason.contains(reason), "{}", i.outcome_reason);
            assert_eq!(i.action, "", "{remote_id}");
        }
        task.abort();
    }
    #[tokio::test]
    async fn authentication_errors_hide_response_bodies() {
        use axum::{routing::get, Router};
        let router = Router::new().route(
            "/api/v4/user",
            get(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    "sensitive-test-token echoed by server",
                )
            }),
        );
        let (p, task) = fixture(router, "sensitive-test-token").await;
        let error = p.identity().await.unwrap_err().to_string();
        assert!(error.contains("Credential rejected"));
        assert!(!error.contains("sensitive-test-token"));
        task.abort();
    }
    #[tokio::test]
    async fn access_denied_for_the_account_or_listing_fails_the_sync() {
        use axum::{response::IntoResponse, routing::get, Json, Router};
        // Only a single tracked item may turn unknown on a 403; a denied
        // account check or listing fails the whole sync.
        for account_denied in [true, false] {
            let account = if account_denied {
                get(|| async { StatusCode::FORBIDDEN.into_response() })
            } else {
                get(|| async { Json(serde_json::json!({"id":1,"username":"me"})).into_response() })
            };
            let router = Router::new()
                .route("/api/v4/user", account)
                .route(
                    "/api/v4/merge_requests",
                    get(|| async { StatusCode::FORBIDDEN }),
                )
                .route(
                    "/api/v4/projects/7/merge_requests/5",
                    get(|| async { StatusCode::FORBIDDEN }),
                );
            let (p, task) = fixture(router, "test-only-token").await;
            let old = gitlab_item(&c(), &mr(5, "opened"), "next").unwrap();
            let error = p.sync(&c(), &[old]).await.unwrap_err().to_string();
            assert!(
                error.contains("Access denied") && error.contains("HTTP 403"),
                "account_denied={account_denied}: {error}"
            );
            task.abort();
        }
    }
}
