use crate::{
    connector::{
        Account, BoxFuture, Connector, ConnectorError, ConnectorResult, ErrorKind, Refreshed,
        RemoteItem, Target, Tracked,
    },
    model::*,
    outcome,
};
use anyhow::{bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use url::Url;

pub fn base_url(raw: &str) -> Result<String> {
    let mut u = Url::parse(raw).context("Enter a complete HTTPS instance URL")?;
    if u.scheme() != "https"
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        bail!("Use an HTTPS instance URL without credentials, query or fragment")
    }
    let path = format!("{}/", u.path().trim_end_matches('/'));
    u.set_path(&path);
    Ok(u.to_string())
}

pub fn source_url(raw: &str) -> Result<String> {
    let u = Url::parse(raw).context("Enter a complete source URL")?;
    if !matches!(u.scheme(), "https" | "http")
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
    {
        bail!("Source links must be HTTP or HTTPS URLs without credentials")
    }
    Ok(u.to_string())
}

pub fn client() -> Result<Client> {
    // Never follow an authenticated redirect to another host or an SSO page.
    Ok(Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// The Jira issue fields an item is built from.
const JIRA_FIELDS: &str = "summary,status,resolution,updated";

/// What a request found.
enum Fetched {
    Found(Value),
    /// HTTP 403 or 404: deleted, moved, or not visible to this account.
    Inaccessible(StatusCode),
}

fn http_error(status: StatusCode) -> ConnectorError {
    let (kind, reason) = match status {
        StatusCode::UNAUTHORIZED => (ErrorKind::Auth, "Credential rejected"),
        StatusCode::FORBIDDEN => (
            ErrorKind::Auth,
            "Access denied; check token scope and account permissions",
        ),
        StatusCode::TOO_MANY_REQUESTS => (ErrorKind::Transient, "Rate limited; try again later"),
        s if s.is_redirection() => (
            ErrorKind::Invalid,
            "Instance redirected the API request; verify its base URL and API authentication",
        ),
        s if s.is_server_error() => (ErrorKind::Transient, "Instance API request failed"),
        _ => (ErrorKind::Invalid, "Instance API request failed"),
    };
    ConnectorError::new(kind, format!("{reason} (HTTP {})", status.as_u16()))
}

/// The built-in Jira Data Center and GitLab connectors.
pub struct Provider {
    client: Client,
    provider: String,
}
impl Provider {
    pub fn new(provider: &str) -> Result<Self> {
        if !supported(provider) {
            bail!("Live connection currently supports Jira Data Center and GitLab")
        }
        Ok(Self {
            client: client()?,
            provider: provider.into(),
        })
    }
    async fn get(
        &self,
        target: &Target,
        path: &str,
        query: &[(&str, String)],
    ) -> ConnectorResult<Value> {
        match self.fetch(target, path, query).await? {
            Fetched::Found(v) => Ok(v),
            Fetched::Inaccessible(status) => Err(http_error(status)),
        }
    }
    /// Like `get`, but a 403 or 404 is an answer about that one resource
    /// rather than an error.
    async fn fetch(
        &self,
        target: &Target,
        path: &str,
        query: &[(&str, String)],
    ) -> ConnectorResult<Fetched> {
        let url = target
            .base_url
            .join(path)
            .map_err(ConnectorError::invalid)?;
        if url.origin() != target.base_url.origin() {
            return Err(ConnectorError::invalid("API request changed origin"));
        }
        let req = self.client.get(url).query(query);
        let token = target.credential.expose();
        let req = if is_gitlab(&self.provider) {
            req.header("PRIVATE-TOKEN", token)
        } else {
            req.bearer_auth(token)
        };
        let response = req.send().await.map_err(|_| {
            ConnectorError::new(
                ErrorKind::Transient,
                "Cannot reach the instance; check network, VPN and certificate trust",
            )
        })?;
        let status = response.status();
        if matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN) {
            return Ok(Fetched::Inaccessible(status));
        }
        if !status.is_success() {
            return Err(http_error(status));
        }
        // Do not surface remote error bodies, request headers or credentials.
        response.json().await.map(Fetched::Found).map_err(|_| {
            ConnectorError::invalid("Instance did not return the expected JSON response")
        })
    }
    fn source_name(&self) -> &'static str {
        if is_gitlab(&self.provider) {
            "GitLab"
        } else {
            "Jira"
        }
    }
    async fn identity(&self, target: &Target) -> ConnectorResult<Account> {
        let v = self
            .get(
                target,
                if is_gitlab(&self.provider) {
                    "api/v4/user"
                } else {
                    "rest/api/2/myself"
                },
                &[],
            )
            .await?;
        let id = if is_gitlab(&self.provider) {
            v["id"].as_u64().map(|n| n.to_string())
        } else {
            v["key"]
                .as_str()
                .or(v["accountId"].as_str())
                .map(str::to_string)
        }
        .ok_or_else(|| ConnectorError::invalid("Instance response has no stable account ID"))?;
        let name = v["username"]
            .as_str()
            .or(v["displayName"].as_str())
            .or(v["name"].as_str())
            .unwrap_or(&id)
            .to_string();
        Ok(Account { id, name })
    }
    async fn list_items(
        &self,
        target: &Target,
        account: &Account,
    ) -> ConnectorResult<Vec<RemoteItem>> {
        let mut all = BTreeMap::<String, RemoteItem>::new();
        if is_gitlab(&self.provider) {
            for (kind, extra) in [
                ("next", ("scope", "created_by_me".into())),
                ("next", ("scope", "assigned_to_me".into())),
                ("review", ("reviewer_id", account.id.clone())),
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
                    let v = self.get(target, "api/v4/merge_requests", &query).await?;
                    let rows = v.as_array().ok_or_else(|| {
                        ConnectorError::invalid("Unexpected GitLab merge request list")
                    })?;
                    for row in rows {
                        let item = gitlab_item(row, kind).map_err(ConnectorError::invalid)?;
                        // A review assignment takes precedence when the MR appears in multiple lists.
                        if kind == "review" || !all.contains_key(&item.remote_id) {
                            all.insert(item.remote_id.clone(), item);
                        }
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
        } else {
            let mut start = 0usize;
            let mut complete = false;
            for _ in 0..100 {
                let v = self
                    .get(
                        target,
                        "rest/api/2/search",
                        &[
                            (
                                "jql",
                                "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC".into(),
                            ),
                            ("fields", JIRA_FIELDS.into()),
                            ("maxResults", "100".into()),
                            ("startAt", start.to_string()),
                        ],
                    )
                    .await?;
                let rows = v["issues"]
                    .as_array()
                    .ok_or_else(|| ConnectorError::invalid("Unexpected Jira search response"))?;
                for row in rows {
                    let item = jira_item(&target.base_url, row, "next")
                        .map_err(ConnectorError::invalid)?;
                    all.insert(item.remote_id.clone(), item);
                }
                start += rows.len();
                let total = v["total"]
                    .as_u64()
                    .ok_or_else(|| ConnectorError::invalid("Jira search response has no total"))?
                    as usize;
                if start >= total {
                    complete = true;
                    break;
                }
                if rows.is_empty() {
                    return Err(ConnectorError::invalid(
                        "Jira pagination stopped before all items were read",
                    ));
                }
            }
            if !complete {
                return Err(ConnectorError::invalid(
                    "Too many Jira issues for one sync; no partial results were saved",
                ));
            }
        }
        Ok(all.into_values().collect())
    }
    async fn refresh_items(
        &self,
        target: &Target,
        tracked: &[Tracked],
    ) -> ConnectorResult<Vec<Refreshed>> {
        let mut answers = Vec::new();
        for item in tracked {
            let fetched = if is_gitlab(&self.provider) {
                let (project, iid) = item.remote_id.split_once(':').ok_or_else(|| {
                    ConnectorError::invalid("Invalid stored merge request identity")
                })?;
                self.fetch(
                    target,
                    &format!("api/v4/projects/{project}/merge_requests/{iid}"),
                    &[],
                )
                .await?
            } else {
                let key = item
                    .key
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-');
                if !key {
                    return Err(ConnectorError::invalid("Invalid stored Jira key"));
                }
                self.fetch(
                    target,
                    &format!("rest/api/2/issue/{}", item.key),
                    &[("fields", JIRA_FIELDS.into())],
                )
                .await?
            };
            answers.push(match fetched {
                Fetched::Found(row) if is_gitlab(&self.provider) => Refreshed::found(
                    &item.remote_id,
                    gitlab_item(&row, "follow-up").map_err(ConnectorError::invalid)?,
                ),
                Fetched::Found(row) => Refreshed::found(
                    &item.remote_id,
                    jira_item(&target.base_url, &row, "follow-up")
                        .map_err(ConnectorError::invalid)?,
                ),
                Fetched::Inaccessible(status) => Refreshed::inaccessible(
                    &item.remote_id,
                    format!(
                        "{} returned HTTP {} for this item (missing or no permission)",
                        self.source_name(),
                        status.as_u16()
                    ),
                ),
            });
        }
        Ok(answers)
    }
}
impl Connector for Provider {
    fn kind(&self) -> &str {
        if is_gitlab(&self.provider) {
            "gitlab"
        } else {
            "jira"
        }
    }
    fn verify<'a>(&'a self, target: &'a Target) -> BoxFuture<'a, ConnectorResult<Account>> {
        Box::pin(self.identity(target))
    }
    fn list<'a>(
        &'a self,
        target: &'a Target,
        account: &'a Account,
    ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
        Box::pin(self.list_items(target, account))
    }
    fn refresh<'a>(
        &'a self,
        target: &'a Target,
        _account: &'a Account,
        tracked: &'a [Tracked],
    ) -> BoxFuture<'a, ConnectorResult<Vec<Refreshed>>> {
        Box::pin(self.refresh_items(target, tracked))
    }
}
fn string(v: &Value, key: &str) -> Result<String> {
    v[key]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("Missing field: {key}"))
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
    })
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{self, Credential};
    /// Build items the way a sync does, through the connector interface.
    fn gitlab_item(c: &Connection, v: &Value, action: &str) -> Result<Item> {
        Ok(connector::to_item(
            c,
            "gitlab",
            super::gitlab_item(v, action)?,
        )?)
    }
    fn jira_item(c: &Connection, v: &Value, action: &str) -> Result<Item> {
        let base = Url::parse(&c.base_url)?;
        Ok(connector::to_item(
            c,
            "jira",
            super::jira_item(&base, v, action)?,
        )?)
    }
    fn c() -> Connection {
        Connection {
            id: "a".into(),
            provider: "gitlab-internal".into(),
            label: "Internal".into(),
            base_url: "https://example.com/".into(),
            account_id: "1".into(),
            account_name: "me".into(),
            last_sync: None,
            error: None,
        }
    }
    #[test]
    fn rejects_credential_urls_and_preserves_subpath() {
        assert!(base_url("https://me:secret@example.com").is_err());
        assert!(base_url("http://example.com").is_err());
        assert_eq!(
            base_url("https://example.com/jira").unwrap(),
            "https://example.com/jira/"
        );
        assert!(source_url("javascript:alert(1)").is_err());
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
    /// The GitLab connector pointed at a fixture.
    struct Fixture {
        connector: Provider,
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
    /// Serve a GitLab API fixture over plain HTTP, which production
    /// constructors refuse.
    async fn fixture(router: axum::Router, token: &str) -> (Fixture, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let p = Fixture {
            connector: Provider::new("gitlab-internal").unwrap(),
            target: Target {
                base_url: Url::parse(&base).unwrap(),
                credential: Credential::new(token).unwrap(),
            },
        };
        (p, task)
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
                return Json(serde_json::json!([]));
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
        assert_eq!(
            items.iter().find(|i| i.remote_id == "7:1").unwrap().action,
            "review"
        );
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
