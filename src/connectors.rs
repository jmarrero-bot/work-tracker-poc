use crate::model::*;
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
        if !response.status().is_success() {
            let reason=match response.status() {StatusCode::UNAUTHORIZED=>"Credential rejected",StatusCode::FORBIDDEN=>"Access denied; check token scope and account permissions",StatusCode::TOO_MANY_REQUESTS=>"Rate limited; try again later",s if s.is_redirection()=>"Instance redirected the API request; verify its base URL and API authentication",_=>"Instance API request failed"};
            bail!("{reason} (HTTP {})", response.status().as_u16());
        }
        // Do not surface remote error bodies, request headers or credentials.
        response
            .json()
            .await
            .map_err(|_| anyhow!("Instance did not return the expected JSON response"))
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
                let v=self.get("rest/api/2/search",&[("jql","assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC".into()),("fields","summary,status,updated".into()),("maxResults","100".into()),("startAt",start.to_string())]).await?;
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
        // completed ticket can advance its effort. A failed refresh aborts this sync.
        let missing: Vec<_> = old
            .iter()
            .filter(|i| {
                i.connection_id.as_deref() == Some(&c.id) && !i.terminal && !all.contains_key(&i.id)
            })
            .collect();
        for item in missing {
            let next = if is_gitlab(&self.provider) {
                let (project, iid) = item
                    .remote_id
                    .split_once(':')
                    .context("Invalid stored merge request identity")?;
                let row = self
                    .get(
                        &format!("api/v4/projects/{project}/merge_requests/{iid}"),
                        &[],
                    )
                    .await?;
                gitlab_item(c, &row, "follow-up")?
            } else {
                let key = item
                    .key
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-');
                if !key {
                    bail!("Invalid stored Jira key")
                }
                let row = self
                    .get(
                        &format!("rest/api/2/issue/{}", item.key),
                        &[("fields", "summary,status,updated".into())],
                    )
                    .await?;
                jira_item(c, &row, "follow-up")?
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
    let terminal = v["fields"]["status"]["statusCategory"]["key"].as_str() == Some("done");
    let url = Url::parse(&c.base_url)?
        .join(&format!("browse/{key}"))?
        .to_string();
    Ok(Item {
        id: format!("{}:jira:{remote_id}", c.id),
        connection_id: Some(c.id.clone()),
        provider: c.provider.clone(),
        remote_id,
        key,
        title: string(&v["fields"], "summary")?,
        url: source_url(&url)?,
        status: string(&v["fields"]["status"], "name")?,
        terminal,
        action: if terminal { "" } else { action }.into(),
        updated_at: string(&v["fields"], "updated")?,
        effort_id: None,
        demo: false,
    })
}
pub fn gitlab_item(c: &Connection, v: &Value, action: &str) -> Result<Item> {
    let project = v["project_id"].as_u64().context("Missing project ID")?;
    let iid = v["iid"].as_u64().context("Missing MR number")?;
    let state = string(v, "state")?;
    let terminal = matches!(state.as_str(), "merged" | "closed");
    let draft = v["draft"]
        .as_bool()
        .or(v["work_in_progress"].as_bool())
        .unwrap_or(false);
    Ok(Item {
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
        status: if draft && !terminal {
            "Draft".into()
        } else {
            state
        },
        terminal,
        action: if terminal { "" } else { action }.into(),
        updated_at: string(v, "updated_at")?,
        effort_id: None,
        demo: false,
    })
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
    fn merged_mr_is_terminal() {
        let v = serde_json::json!({"project_id":7,"iid":4,"title":"fix","state":"merged","draft":true,"web_url":"https://example.com/p/-/merge_requests/4","updated_at":"now"});
        let i = gitlab_item(&c(), &v, "review").unwrap();
        assert!(i.terminal);
        assert_eq!(i.action, "");
        assert_eq!(i.status, "merged");
    }
    #[test]
    fn jira_completion_uses_category_not_display_name() {
        let v = serde_json::json!({"id":"1","key":"RHEL-1","fields":{"summary":"fix","updated":"now","status":{"name":"Released","statusCategory":{"key":"done"}}}});
        let i = jira_item(&c(), &v, "next").unwrap();
        assert!(i.terminal);
        assert_eq!(i.action, "");
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
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        // Test-only HTTP fixture; production constructors require HTTPS.
        let p = Provider {
            client: client().unwrap(),
            base: Url::parse(&base).unwrap(),
            token: "test-only-token".into(),
            provider: "gitlab-internal".into(),
        };
        let old = gitlab_item(&c(), &mr(999, "opened"), "next").unwrap();
        let items = p.sync(&c(), &[old]).await.unwrap();
        assert_eq!(items.len(), 102);
        assert_eq!(
            items.iter().find(|i| i.remote_id == "7:1").unwrap().action,
            "review"
        );
        assert!(
            items
                .iter()
                .find(|i| i.remote_id == "7:999")
                .unwrap()
                .terminal
        );
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let p = Provider {
            client: client().unwrap(),
            base: Url::parse(&base).unwrap(),
            token: "sensitive-test-token".into(),
            provider: "gitlab-internal".into(),
        };
        let error = p.identity().await.unwrap_err().to_string();
        assert!(error.contains("Credential rejected"));
        assert!(!error.contains("sensitive-test-token"));
        task.abort();
    }
}
