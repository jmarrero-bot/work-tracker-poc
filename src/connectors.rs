use crate::{model::*, outcome};
use anyhow::{anyhow, bail, Context, Result};
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

fn http_error(status: StatusCode) -> anyhow::Error {
    let reason = match status {
        StatusCode::UNAUTHORIZED => "Credential rejected",
        StatusCode::FORBIDDEN => "Access denied; check token scope and account permissions",
        StatusCode::TOO_MANY_REQUESTS => "Rate limited; try again later",
        s if s.is_redirection() => {
            "Instance redirected the API request; verify its base URL and API authentication"
        }
        _ => "Instance API request failed",
    };
    anyhow!("{reason} (HTTP {})", status.as_u16())
}

pub struct Provider {
    client: Client,
    base: Url,
    token: String,
    provider: String,
}
impl Provider {
    pub fn new(provider: &str, base: &str, token: &str) -> Result<Self> {
        if !supported(provider) {
            bail!("Live connection currently supports Jira Data Center and GitLab")
        }
        if token.trim().is_empty() || token.len() > 16384 || token.chars().any(|c| c.is_control()) {
            bail!("Invalid token")
        }
        Ok(Self {
            client: client()?,
            base: Url::parse(&base_url(base)?)?,
            token: token.into(),
            provider: provider.into(),
        })
    }
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        match self.fetch(path, query).await? {
            Fetched::Found(v) => Ok(v),
            Fetched::Inaccessible(status) => Err(http_error(status)),
        }
    }
    /// Like `get`, but a 403 or 404 is an answer about that one resource
    /// rather than an error.
    async fn fetch(&self, path: &str, query: &[(&str, String)]) -> Result<Fetched> {
        let url = self.base.join(path)?;
        if url.origin() != self.base.origin() {
            bail!("API request changed origin")
        }
        let req = self.client.get(url).query(query);
        let req = if is_gitlab(&self.provider) {
            req.header("PRIVATE-TOKEN", &self.token)
        } else {
            req.bearer_auth(&self.token)
        };
        let response = req.send().await.map_err(|_| {
            anyhow!("Cannot reach the instance; check network, VPN and certificate trust")
        })?;
        let status = response.status();
        if matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN) {
            return Ok(Fetched::Inaccessible(status));
        }
        if !status.is_success() {
            return Err(http_error(status));
        }
        // Do not surface remote error bodies, request headers or credentials.
        response
            .json()
            .await
            .map(Fetched::Found)
            .map_err(|_| anyhow!("Instance did not return the expected JSON response"))
    }
    fn source_name(&self) -> &'static str {
        if is_gitlab(&self.provider) {
            "GitLab"
        } else {
            "Jira"
        }
    }
    pub async fn identity(&self) -> Result<(String, String)> {
        let v = self
            .get(
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
        .context("Instance response has no stable account ID")?;
        let name = v["username"]
            .as_str()
            .or(v["displayName"].as_str())
            .or(v["name"].as_str())
            .unwrap_or(&id)
            .to_string();
        Ok((id, name))
    }
    pub async fn sync(&self, c: &Connection, old: &[Item]) -> Result<Vec<Item>> {
        // Verify that rotating a credential has not changed the linked account.
        let (id, _) = self.identity().await?;
        if id != c.account_id {
            bail!("Credential belongs to another account; create a separate connection")
        }
        let mut all = BTreeMap::<String, Item>::new();
        if is_gitlab(&self.provider) {
            for (kind, extra) in [
                ("next", ("scope", "created_by_me".into())),
                ("next", ("scope", "assigned_to_me".into())),
                ("review", ("reviewer_id", id.clone())),
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
                    let v = self.get("api/v4/merge_requests", &query).await?;
                    let rows = v
                        .as_array()
                        .context("Unexpected GitLab merge request list")?;
                    for row in rows {
                        let item = gitlab_item(c, row, kind)?;
                        // A review assignment takes precedence when the MR appears in multiple lists.
                        if kind == "review" || !all.contains_key(&item.id) {
                            all.insert(item.id.clone(), item);
                        }
                    }
                    if rows.len() < 100 {
                        complete = true;
                        break;
                    }
                }
                if !complete {
                    bail!("Too many merge requests for one sync; no partial results were saved")
                }
            }
        } else {
            let mut start = 0usize;
            let mut complete = false;
            for _ in 0..100 {
                let v=self.get("rest/api/2/search",&[("jql","assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC".into()),("fields",JIRA_FIELDS.into()),("maxResults","100".into()),("startAt",start.to_string())]).await?;
                let rows = v["issues"]
                    .as_array()
                    .context("Unexpected Jira search response")?;
                for row in rows {
                    let item = jira_item(c, row, "next")?;
                    all.insert(item.id.clone(), item);
                }
                start += rows.len();
                let total = v["total"]
                    .as_u64()
                    .context("Jira search response has no total")?
                    as usize;
                if start >= total {
                    complete = true;
                    break;
                }
                if rows.is_empty() {
                    bail!("Jira pagination stopped before all items were read")
                }
            }
            if !complete {
                bail!("Too many Jira issues for one sync; no partial results were saved")
            }
        }
        // Refresh previously tracked items that left the open inbox, so a merge or
        // completed ticket can advance its effort. An item the source no longer
        // shows becomes unknown, never done; any other failed refresh aborts
        // this sync.
        let missing: Vec<_> = old
            .iter()
            .filter(|i| {
                i.connection_id.as_deref() == Some(&c.id)
                    && matches!(i.outcome, Outcome::Open | Outcome::Unknown)
                    && !all.contains_key(&i.id)
            })
            .collect();
        for item in missing {
            let fetched = if is_gitlab(&self.provider) {
                let (project, iid) = item
                    .remote_id
                    .split_once(':')
                    .context("Invalid stored merge request identity")?;
                self.fetch(
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
                    bail!("Invalid stored Jira key")
                }
                self.fetch(
                    &format!("rest/api/2/issue/{}", item.key),
                    &[("fields", JIRA_FIELDS.into())],
                )
                .await?
            };
            let next = match fetched {
                Fetched::Found(row) if is_gitlab(&self.provider) => {
                    gitlab_item(c, &row, "follow-up")?
                }
                Fetched::Found(row) => jira_item(c, &row, "follow-up")?,
                Fetched::Inaccessible(status) => {
                    let mut gone = item.clone();
                    gone.set_outcome(outcome::inaccessible(self.source_name(), status.as_u16()));
                    gone
                }
            };
            all.insert(next.id.clone(), next);
        }
        Ok(all.into_values().collect())
    }
}
fn string(v: &Value, key: &str) -> Result<String> {
    v[key]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("Missing field: {key}"))
}
pub fn jira_item(c: &Connection, v: &Value, action: &str) -> Result<Item> {
    let remote_id = string(v, "id")?;
    let key = string(v, "key")?;
    let status = string(&v["fields"]["status"], "name")?;
    let derived = outcome::jira(
        &status,
        v["fields"]["status"]["statusCategory"]["key"].as_str(),
        v["fields"]["resolution"]["name"].as_str(),
    );
    let url = Url::parse(&c.base_url)?
        .join(&format!("browse/{key}"))?
        .to_string();
    let mut item = Item {
        id: format!("{}:jira:{remote_id}", c.id),
        connection_id: Some(c.id.clone()),
        provider: c.provider.clone(),
        remote_id,
        key,
        title: string(&v["fields"], "summary")?,
        url: source_url(&url)?,
        status,
        outcome: Outcome::Open,
        outcome_reason: String::new(),
        action: action.into(),
        updated_at: string(&v["fields"], "updated")?,
        effort_id: None,
        abandoned_at: None,
        demo: false,
    };
    item.set_outcome(derived);
    Ok(item)
}
pub fn gitlab_item(c: &Connection, v: &Value, action: &str) -> Result<Item> {
    let project = v["project_id"].as_u64().context("Missing project ID")?;
    let iid = v["iid"].as_u64().context("Missing MR number")?;
    let state = string(v, "state")?;
    let derived = outcome::gitlab(&state);
    let draft = v["draft"]
        .as_bool()
        .or(v["work_in_progress"].as_bool())
        .unwrap_or(false);
    let mut item = Item {
        id: format!("{}:gitlab:{project}:{iid}", c.id),
        connection_id: Some(c.id.clone()),
        provider: c.provider.clone(),
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
        outcome: Outcome::Open,
        outcome_reason: String::new(),
        action: action.into(),
        updated_at: string(v, "updated_at")?,
        effort_id: None,
        abandoned_at: None,
        demo: false,
    };
    item.set_outcome(derived);
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    /// Serve a GitLab API fixture over plain HTTP, which production
    /// constructors refuse.
    async fn fixture(router: axum::Router, token: &str) -> (Provider, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let p = Provider {
            client: client().unwrap(),
            base: Url::parse(&base).unwrap(),
            token: token.into(),
            provider: "gitlab-internal".into(),
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
