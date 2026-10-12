//! Forgejo, read only, for Fedora Forge (forge.fedoraproject.org): an access
//! token; the open issues and pull requests assigned to the account,
//! requesting its review or mentioning it, and its own open pull requests.
//! Every request goes through the read-only client in `readonly`, which
//! refuses anything that could write. See `docs/connectors/fedora-forge.md`
//! for what Fedora Forge is.

mod readonly;

pub use readonly::Memory;

use super::inaccessible;
use crate::{
    connector::{
        Account, BoxFuture, Connector, ConnectorError, ConnectorResult, ErrorKind, Listing, Reason,
        Refreshed, RemoteItem, Requests, Target, Tracked,
    },
    connectors::{http_error, source_url},
    outcome,
};
use anyhow::{bail, Context, Result};
use readonly::{Got, ReadOnly};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc};

/// The namespace of this connector's local item IDs.
const KIND: &str = "fedora";
const SOURCE: &str = "Fedora Forge";
/// Results asked for per page. Forgejo caps it at its `max_response_items`,
/// 50 by default and on Fedora Forge.
const PER_PAGE: usize = 50;
/// Most results one listing reads; past it, the sync fails rather than
/// save part of them.
const MAX_RESULTS: u64 = 1000;
/// Most pages one listing reads: enough for [`MAX_RESULTS`], and one more
/// for a listing that grew while it was read.
const MAX_PAGES: u64 = MAX_RESULTS.div_ceil(PER_PAGE as u64) + 1;
/// Where Forgejo searches the issues and pull requests the account can see.
const SEARCH: &str = "repos/issues/search";

/// A search for what waits on the account: the filter, whether it looks at
/// pull requests only, the reason it gives, and the attention category of
/// what it finds. The filters only apply to a signed-in request, which
/// every sync is, since it verifies the account first.
struct Search {
    filter: &'static str,
    pulls: bool,
    reason: Option<Reason>,
    action: &'static str,
}

const MINE: &[Search] = &[
    // Requests to the account's teams too: told apart by the pull request's
    // requested reviewers.
    Search {
        filter: "review_requested",
        pulls: true,
        reason: Some(Reason::ReviewRequested),
        action: "review",
    },
    Search {
        filter: "assigned",
        pulls: false,
        reason: Some(Reason::Assigned),
        action: "next",
    },
    Search {
        filter: "mentioned",
        pulls: false,
        reason: Some(Reason::Mentioned),
        action: "next",
    },
    // In the inbox, but not waiting on the account.
    Search {
        filter: "created",
        pulls: true,
        reason: None,
        action: "waiting",
    },
];

pub struct Forgejo {
    client: ReadOnly,
}

impl Forgejo {
    pub fn new(memory: Arc<Memory>) -> Result<Self> {
        Ok(Self {
            client: ReadOnly::new(memory)?,
        })
    }
    async fn current_user(&self, target: &Target) -> ConnectorResult<Account> {
        let v = self.client.get_found(target, "user", &[]).await?;
        let id = v["id"]
            .as_u64()
            .ok_or_else(|| ConnectorError::invalid("Forgejo's answer has no account ID"))?;
        let login = v["login"]
            .as_str()
            .ok_or_else(|| ConnectorError::invalid("Forgejo's answer has no login"))?;
        Ok(Account {
            id: id.to_string(),
            name: login.into(),
        })
    }
    /// Every result of the paged listing at `path`, or an error: never part
    /// of them. A listing that changes while it is read fails too, since a
    /// result could have moved between pages.
    async fn all(
        &self,
        target: &Target,
        path: &str,
        query: &[(&str, String)],
    ) -> ConnectorResult<Vec<Value>> {
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        let mut first_total = None;
        for page in 1..=MAX_PAGES {
            let mut query = query.to_vec();
            query.extend([("limit", PER_PAGE.to_string()), ("page", page.to_string())]);
            let (v, total) = match self.client.get(target, path, &query).await? {
                Got::Found(v, total) => (v, total),
                Got::Moved(status, _) | Got::Inaccessible(status) => {
                    return Err(http_error(status))
                }
            };
            let total = total.ok_or_else(|| {
                ConnectorError::invalid("Forgejo's listing didn't say how many results it has")
            })?;
            if total > MAX_RESULTS {
                return Err(ConnectorError::invalid(format!(
                    "Forgejo found {total} results, more than one sync reads; no partial results were saved"
                )));
            }
            if *first_total.get_or_insert(total) != total {
                return Err(ConnectorError::new(
                    ErrorKind::Transient,
                    "Forgejo's listing changed while it was read; no partial results were saved",
                ));
            }
            let page_rows = v
                .as_array()
                .ok_or_else(|| ConnectorError::invalid("Unexpected Forgejo listing"))?;
            let before = rows.len();
            for row in page_rows {
                let id = row["id"]
                    .as_u64()
                    .ok_or_else(|| ConnectorError::invalid("Unexpected Forgejo listing"))?;
                if seen.insert(id) {
                    rows.push(row.clone());
                }
            }
            if rows.len() as u64 >= total {
                return Ok(rows);
            }
            // A short page doesn't end it while Forgejo counts more; an
            // empty one does, as an error.
            if page_rows.is_empty() {
                return Err(ConnectorError::new(
                    ErrorKind::Transient,
                    "Forgejo's listing returned fewer results than it counted; no partial results were saved",
                ));
            }
            // Nor does a page of results already read, as a server that
            // ignores `page` sends: it would never reach the count.
            if rows.len() == before {
                return Err(ConnectorError::new(
                    ErrorKind::Transient,
                    "Forgejo's listing repeated a page; no partial results were saved",
                ));
            }
        }
        Err(ConnectorError::invalid(
            "Forgejo's listing kept paging; no partial results were saved",
        ))
    }
    async fn mine(&self, target: &Target, account: &Account) -> ConnectorResult<Vec<RemoteItem>> {
        let mut all = Listing::default();
        for search in MINE {
            let mut query = vec![
                ("state", "open".to_string()),
                (search.filter, "true".into()),
            ];
            if search.pulls {
                query.push(("type", "pulls".into()));
            }
            for row in self.all(target, SEARCH, &query).await? {
                let mut item =
                    forgejo_item(&row, search.action).map_err(ConnectorError::invalid)?;
                // A draft of one's own isn't waiting on others yet.
                if search.reason.is_none() && item.status == DRAFT {
                    item.action = "next".into();
                }
                let reason = match search.reason {
                    Some(Reason::ReviewRequested) => {
                        Some(self.review_request(target, account, &item.key).await?)
                    }
                    reason => reason,
                };
                item.reasons.extend(reason);
                all.add(item);
            }
        }
        Ok(all.into_items())
    }
    /// Whether a review requested of the account on the pull request `key`
    /// was asked of it directly, or only of one of its teams.
    async fn review_request(
        &self,
        target: &Target,
        account: &Account,
        key: &str,
    ) -> ConnectorResult<Reason> {
        let (repository, number) =
            parse_key(key).ok_or_else(|| ConnectorError::invalid("Invalid Forgejo item key"))?;
        let pull = self
            .client
            .get_found(target, &format!("repos/{repository}/pulls/{number}"), &[])
            .await?;
        let direct = pull["requested_reviewers"].as_array().is_some_and(|users| {
            users.iter().any(|u| {
                u["id"]
                    .as_u64()
                    .is_some_and(|id| id.to_string() == account.id)
            })
        });
        Ok(if direct {
            Reason::ReviewRequested
        } else {
            Reason::TeamReviewRequested
        })
    }
    async fn tracked(
        &self,
        target: &Target,
        tracked: &[Tracked],
    ) -> ConnectorResult<Vec<Refreshed>> {
        let mut answers = Vec::new();
        for item in tracked {
            let (repository, number) = parse_key(&item.key)
                .ok_or_else(|| ConnectorError::invalid("Invalid stored Forgejo item key"))?;
            let path = format!("repos/{repository}/issues/{number}");
            let mut got = self.client.get(target, &path, &[]).await?;
            // A renamed repository redirects to its new place: follow it
            // once, only within the same API.
            if let Got::Moved(_, Some(to)) = got {
                got = self.client.get_url(target, to).await?;
            }
            answers.push(match got {
                Got::Found(row, _) => {
                    let found = forgejo_item(&row, "follow-up").map_err(ConnectorError::invalid)?;
                    if found.remote_id == item.remote_id {
                        Refreshed::found(&item.remote_id, found)
                    } else {
                        Refreshed::inaccessible(
                            &item.remote_id,
                            "Fedora Forge now shows another item at this address",
                        )
                    }
                }
                Got::Moved(status, _) | Got::Inaccessible(status) => {
                    inaccessible(SOURCE, &item.remote_id, status)
                }
            });
        }
        Ok(answers)
    }
}

impl Connector for Forgejo {
    fn kind(&self) -> &str {
        KIND
    }
    fn verify<'a>(&'a self, target: &'a Target) -> BoxFuture<'a, ConnectorResult<Account>> {
        Box::pin(self.current_user(target))
    }
    fn list<'a>(
        &'a self,
        target: &'a Target,
        account: &'a Account,
    ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
        Box::pin(self.mine(target, account))
    }
    fn refresh<'a>(
        &'a self,
        target: &'a Target,
        _account: &'a Account,
        tracked: &'a [Tracked],
    ) -> BoxFuture<'a, ConnectorResult<Vec<Refreshed>>> {
        Box::pin(self.tracked(target, tracked))
    }
    fn requests(&self) -> Option<Requests> {
        Some(self.client.requests())
    }
}

/// How a draft pull request's status reads.
const DRAFT: &str = "draft";

/// Whether `name` can be a Forgejo owner or repository name, and so is safe
/// in an API path.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// `OWNER/REPO` and the number of a key such as `OWNER/REPO#12`.
fn parse_key(key: &str) -> Option<(String, u64)> {
    let (repository, number) = key.rsplit_once('#')?;
    let (owner, repo) = repository.split_once('/')?;
    let number = number.parse().ok()?;
    (valid_name(owner) && valid_name(repo)).then(|| (format!("{owner}/{repo}"), number))
}

/// `OWNER/REPO` of an issue's repository.
fn repository(v: &Value) -> Result<String> {
    let full_name = v["repository"]["full_name"]
        .as_str()
        .context("Missing repository")?;
    match full_name.split_once('/') {
        Some((owner, repo)) if valid_name(owner) && valid_name(repo) => Ok(full_name.into()),
        _ => bail!("Invalid repository name"),
    }
}

/// An issue or pull request as Forgejo's issue search and issue APIs return
/// it.
pub fn forgejo_item(v: &Value, action: &str) -> Result<RemoteItem> {
    let id = v["id"].as_u64().context("Missing issue ID")?;
    let number = v["number"].as_u64().context("Missing issue number")?;
    let state = v["state"].as_str().context("Missing state")?;
    let (status, (outcome, outcome_reason)) = match v["pull_request"].as_object() {
        Some(pr) => {
            let merged = pr.get("merged").and_then(Value::as_bool).unwrap_or(false);
            let draft = pr.get("draft").and_then(Value::as_bool).unwrap_or(false);
            let status = match (state, merged, draft) {
                (_, true, _) => "merged",
                ("open", _, true) => DRAFT,
                _ => state,
            };
            (
                status.to_string(),
                outcome::forgejo_pull_request(state, merged, draft),
            )
        }
        None => (state.to_string(), outcome::forgejo_issue(state)),
    };
    Ok(RemoteItem {
        remote_id: id.to_string(),
        key: format!("{}#{number}", repository(v)?),
        title: v["title"].as_str().context("Missing title")?.into(),
        url: source_url(v["html_url"].as_str().context("Missing web address")?)?,
        status,
        outcome,
        outcome_reason,
        action: action.into(),
        updated_at: v["updated_at"]
            .as_str()
            .context("Missing update time")?
            .into(),
        reasons: vec![],
        bot: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        connector,
        connectors::test_connection,
        model::{Connection, Item, Outcome},
    };
    use axum::{
        extract::{Path, Query},
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::get,
        Json, Router,
    };
    use std::collections::{BTreeMap, HashMap};

    fn issue(n: u64, state: &str) -> Value {
        serde_json::json!({
            "id": 1000 + n, "number": n, "title": format!("Issue {n}"),
            "html_url": format!("https://forge.example.org/o/r/issues/{n}"),
            "repository": {"id": 1, "name": "r", "owner": "o", "full_name": "o/r"},
            "state": state, "updated_at": "2026-10-12T00:00:00Z", "pull_request": null,
        })
    }
    fn pr(n: u64, state: &str, merged: bool, draft: bool) -> Value {
        serde_json::json!({
            "id": 1000 + n, "number": n, "title": format!("Change {n}"),
            "html_url": format!("https://forge.example.org/o/r/pulls/{n}"),
            "repository": {"id": 1, "name": "r", "owner": "o", "full_name": "o/r"},
            "state": state, "updated_at": "2026-10-12T00:00:00Z",
            "pull_request": {
                "merged": merged, "draft": draft,
                "merged_at": merged.then_some("2026-10-11T00:00:00Z"),
            },
        })
    }
    fn connection() -> Connection {
        Connection {
            provider: "fedora".into(),
            base_url: "https://forge.example.org/".into(),
            account_id: "42".into(),
            ..test_connection()
        }
    }

    #[test]
    fn outcomes_and_native_statuses() {
        use Outcome::*;
        for (v, status, outcome) in [
            (pr(1, "closed", true, false), "merged", Delivered),
            (
                pr(1, "closed", false, false),
                "closed",
                ClosedWithoutDelivery,
            ),
            // A work-in-progress title is a draft too.
            (pr(1, "open", false, true), "draft", Open),
            (
                pr(1, "closed", false, true),
                "closed",
                ClosedWithoutDelivery,
            ),
            (pr(1, "open", false, false), "open", Open),
            (issue(1, "open"), "open", Open),
            // Forgejo doesn't say why an issue was closed.
            (issue(1, "closed"), "closed", ClosedWithoutDelivery),
            (issue(1, "locked"), "locked", Unknown),
        ] {
            let i =
                connector::to_item(&connection(), KIND, forgejo_item(&v, "next").unwrap()).unwrap();
            assert_eq!((i.status.as_str(), i.outcome), (status, outcome), "{v}");
            assert_eq!(i.key, "o/r#1");
            assert_eq!(i.remote_id, "1001");
            assert_eq!(i.id, format!("{}:fedora:1001", connection().id));
            assert_eq!(i.action, if outcome == Open { "next" } else { "" });
            assert!(!i.outcome_reason.is_empty());
        }
    }

    #[test]
    fn keys_are_checked_before_they_become_paths() {
        for (key, parsed) in [
            ("o/r#12", Some(("o/r", 12))),
            ("my-org/my.repo_1#3", Some(("my-org/my.repo_1", 3))),
            ("o/../r#1", None),
            ("../r#1", None),
            ("o/r/x#1", None),
            ("o/r#x", None),
            ("o/r?q=1#1", None),
            ("o#1", None),
        ] {
            let found = parse_key(key);
            let found = found.as_ref().map(|(r, n)| (r.as_str(), *n));
            assert_eq!(found, parsed, "{key}");
        }
        for name in ["o/..", "o", "o/r/x", "../r"] {
            let mut v = issue(1, "open");
            v["repository"]["full_name"] = name.into();
            assert!(forgejo_item(&v, "next").is_err(), "{name}");
        }
    }

    /// One page of a listing: `items` out of `total`.
    fn page(items: Vec<Value>, total: usize) -> Response {
        ([("x-total-count", total.to_string())], Json(items)).into_response()
    }
    async fn search(Query(q): Query<HashMap<String, String>>) -> Response {
        assert_eq!(q["state"], "open");
        assert_eq!(q["limit"], "50");
        let page_number: usize = q["page"].parse().unwrap();
        let filters: Vec<_> = ["review_requested", "assigned", "mentioned", "created"]
            .into_iter()
            .filter(|f| q.get(*f).map(String::as_str) == Some("true"))
            .collect();
        let pulls = q.get("type").map(String::as_str) == Some("pulls");
        match (filters.as_slice(), pulls) {
            // Directly for 9, through a team for 1.
            (["review_requested"], true) => page(
                vec![pr(1, "open", false, false), pr(9, "open", false, false)],
                2,
            ),
            // Two pages.
            (["assigned"], false) if page_number == 1 => {
                page((100..150).map(|n| issue(n, "open")).collect(), 51)
            }
            (["assigned"], false) => page(vec![pr(9, "open", false, false)], 51),
            (["mentioned"], false) => page(vec![issue(2, "open"), issue(100, "open")], 2),
            // A short page, though Forgejo counts more.
            (["created"], true) if page_number == 1 => page(
                vec![pr(4, "open", false, false), pr(5, "open", false, true)],
                3,
            ),
            (["created"], true) => page(vec![pr(6, "open", false, false)], 3),
            other => panic!("unexpected search {other:?}"),
        }
    }
    fn router() -> Router {
        routes(get(search))
    }
    fn routes(search: axum::routing::MethodRouter) -> Router {
        Router::new()
            .route(
                "/api/v1/user",
                get(|| async { Json(serde_json::json!({"id": 42, "login": "me"})) }),
            )
            .route("/api/v1/repos/issues/search", get(search))
            .route(
                "/api/v1/repos/o/r/pulls/{n}",
                get(|Path(n): Path<u64>| async move {
                    let reviewers = match n {
                        9 => serde_json::json!([{"id": 7, "login": "other"}, {"id": 42, "login": "me"}]),
                        _ => serde_json::json!([{"id": 7, "login": "other"}]),
                    };
                    let mut v = pr(n, "open", false, false);
                    v["requested_reviewers"] = reviewers;
                    Json(v)
                }),
            )
            .route(
                "/api/v1/repos/o/r/issues/7",
                get(|| async { Json(pr(7, "closed", true, false)) }),
            )
            .route(
                "/api/v1/repos/o/r/issues/8",
                get(|| async { Json(issue(8, "closed")) }),
            )
            // Renamed: followed once, within the same API.
            .route(
                "/api/v1/repos/o/r/issues/10",
                get(|| async {
                    (
                        StatusCode::MOVED_PERMANENTLY,
                        [("location", "/api/v1/repos/o/renamed/issues/10")],
                    )
                }),
            )
            .route(
                "/api/v1/repos/o/renamed/issues/10",
                get(|| async { Json(pr(10, "closed", false, false)) }),
            )
    }
    async fn sync(router: Router, old: &[Item]) -> (ConnectorResult<Vec<Item>>, Requests) {
        let (target, task) = crate::connectors::fixture(router, "test-only-token").await;
        let forgejo = Forgejo::new(Arc::default()).unwrap();
        let items = connector::sync(&forgejo, &target, &connection(), old).await;
        task.abort();
        (items, forgejo.requests().unwrap())
    }

    #[tokio::test]
    async fn lists_what_waits_on_the_account_once_with_every_reason() {
        use Outcome::*;
        use Reason::*;
        let stored = |v: Value| {
            connector::to_item(&connection(), KIND, forgejo_item(&v, "next").unwrap()).unwrap()
        };
        // Tracked before: merged since, closed since, gone (no route
        // answers 404), and closed unmerged in a renamed repository.
        let old = [
            stored(pr(7, "open", false, false)),
            stored(issue(8, "open")),
            stored(issue(3, "open")),
            stored(pr(10, "open", false, false)),
        ];
        let (items, requests) = sync(router(), &old).await;
        let items = items.unwrap();
        let listed = |n: u64| {
            let i = items
                .iter()
                .find(|i| i.remote_id == (1000 + n).to_string())
                .unwrap();
            (i.action.as_str(), i.reasons.clone(), i.outcome)
        };
        for (n, expected) in [
            (1, ("review", vec![TeamReviewRequested], Open)),
            (9, ("review", vec![ReviewRequested, Assigned], Open)),
            (2, ("next", vec![Mentioned], Open)),
            (100, ("next", vec![Assigned, Mentioned], Open)),
            (149, ("next", vec![Assigned], Open)),
            (4, ("waiting", vec![], Open)),
            (5, ("next", vec![], Open)),
            (6, ("waiting", vec![], Open)),
            (7, ("", vec![], Delivered)),
            (8, ("", vec![], ClosedWithoutDelivery)),
            (3, ("", vec![], Unknown)),
            (10, ("", vec![], ClosedWithoutDelivery)),
        ] {
            assert_eq!(listed(n), expected, "item {n}");
        }
        // 2 to review, 50 more assigned, 1 more mentioned, 3 authored and 4
        // tracked.
        assert_eq!(items.len(), 60);
        // Only reads went out: the account, 6 listing pages, 2 pull
        // requests' reviewers, and 4 items, one of them moved.
        let expected = Requests {
            sent: [("GET".to_string(), 14)].into(),
            not_modified: 0,
            refused: 0,
        };
        assert_eq!(requests, expected);
    }

    #[tokio::test]
    async fn listings_are_complete_or_fail() {
        for (answer, kind, message) in [
            (
                page(vec![], 1001),
                ErrorKind::Invalid,
                "more than one sync reads",
            ),
            (
                page(vec![], 5),
                ErrorKind::Transient,
                "fewer results than it counted",
            ),
            (
                Json(Vec::<Value>::new()).into_response(),
                ErrorKind::Invalid,
                "how many results",
            ),
            (
                page(vec![issue(1, "open")], 1)
                    .into_response()
                    .map(|_| axum::body::Body::from("{}")),
                ErrorKind::Invalid,
                "Unexpected",
            ),
        ] {
            let answer = std::sync::Arc::new(std::sync::Mutex::new(Some(answer)));
            let router = routes(get(move || {
                let answer = answer.clone();
                async move { answer.lock().unwrap().take().unwrap() }
            }));
            let (items, _) = sync(router, &[]).await;
            let error = items.unwrap_err();
            assert_eq!(error.kind, kind, "{error}");
            assert!(error.message.contains(message), "{error}");
        }
        // A listing whose count changes between pages.
        let router = routes(get(|q: Query<HashMap<String, String>>| async move {
            let total = if q["page"] == "1" { 51 } else { 52 };
            page((0..50).map(|n| issue(n, "open")).collect(), total)
        }));
        let error = sync(router, &[]).await.0.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transient, "{error}");
        assert!(error.message.contains("changed while"), "{error}");
        // A page that repeats results, so that the count is never reached.
        let router = routes(get(|q: Query<HashMap<String, String>>| async move {
            match q["page"].as_str() {
                "1" | "2" => page(vec![issue(1, "open")], 2),
                _ => page(vec![], 2),
            }
        }));
        let error = sync(router, &[]).await.0.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transient, "{error}");
        assert!(error.message.contains("repeated a page"), "{error}");
        // A server that ignores `page` is asked twice, not until the count.
        let router = routes(get(|| async { page(vec![issue(1, "open")], 2) }));
        let (items, requests) = sync(router, &[]).await;
        let error = items.unwrap_err();
        assert!(error.message.contains("repeated a page"), "{error}");
        assert_eq!(requests.sent["GET"], 1 + 2, "the account and two pages");
        // One that sends a new result a page never ends either: it is read
        // for as many pages as the most results take, and one more.
        let router = routes(get(|q: Query<HashMap<String, String>>| async move {
            let n: u64 = q["page"].parse().unwrap();
            page(vec![issue(n, "open")], 1000)
        }));
        let (items, requests) = sync(router, &[]).await;
        let error = items.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Invalid, "{error}");
        assert!(error.message.contains("kept paging"), "{error}");
        assert_eq!(requests.sent["GET"], 1 + MAX_PAGES);
        // Another account's token is refused before anything is listed.
        let router = Router::new().route(
            "/api/v1/user",
            get(|| async { Json(serde_json::json!({"id": 43, "login": "other"})) }),
        );
        let (items, requests) = sync(router, &[]).await;
        assert_eq!(items.unwrap_err().kind, ErrorKind::Auth);
        assert_eq!(requests.sent["GET"], 1);
        // A rejected token says so, without the response body.
        let router = Router::new().route(
            "/api/v1/user",
            get(|| async { (StatusCode::UNAUTHORIZED, "test-only-token is bad") }),
        );
        let error = sync(router, &[]).await.0.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Auth);
        assert!(!error.message.contains("test-only-token"), "{error}");
    }

    /// Reads a public repository on the real Fedora Forge, through the
    /// client, paging and mapping a sync uses, and refreshes one item of
    /// each status it found. This is a test only, and the only way
    /// Workboard reads without a token: Forgejo's search filters need a
    /// signed-in account, and the bot has none on Fedora Forge. It prints
    /// counts only. Ignored, so CI doesn't depend on Fedora Forge; run it
    /// from the test binary with `--ignored --nocapture live_fedora_forge`.
    #[tokio::test]
    #[ignore = "reads the real Fedora Forge"]
    async fn live_fedora_forge_public_repository() {
        const REPOSITORY: &str = "atomic/docs";
        let target = Target::new(
            "https://forge.fedoraproject.org/",
            readonly::tests::ANONYMOUS,
        )
        .unwrap();
        let live = Connection {
            base_url: target.base_url.to_string(),
            ..connection()
        };
        let forgejo = Forgejo::new(Arc::default()).unwrap();
        let version = forgejo
            .client
            .get_found(&target, "version", &[])
            .await
            .unwrap();
        println!("Forgejo {}", version["version"]);
        let path = format!("repos/{REPOSITORY}/issues");
        let rows = forgejo
            .all(&target, &path, &[("state", "all".into())])
            .await
            .unwrap();
        let items: Vec<_> = rows
            .iter()
            .map(|r| connector::to_item(&live, KIND, forgejo_item(r, "next").unwrap()).unwrap())
            .collect();
        let mut kinds = BTreeMap::<_, Vec<&Item>>::new();
        for item in &items {
            let pull = item.url.contains("/pulls/");
            kinds
                .entry((pull, item.status.as_str(), item.outcome.label()))
                .or_default()
                .push(item);
        }
        println!("{REPOSITORY}: {} items", items.len());
        for ((pull, status, outcome), found) in &kinds {
            let kind = if *pull { "pull requests" } else { "issues" };
            println!("  {kind}, {status}, {outcome}: {}", found.len());
        }
        assert!(kinds.len() > 1, "a repository with history");
        // One of each, fetched again as tracked items are, and a number
        // that doesn't exist.
        let mut tracked: Vec<_> = kinds
            .values()
            .map(|found| Tracked {
                remote_id: found[0].remote_id.clone(),
                key: found[0].key.clone(),
                bot: None,
            })
            .collect();
        tracked.push(Tracked {
            remote_id: "0".into(),
            key: format!("{REPOSITORY}#999999"),
            bot: None,
        });
        let refreshed = forgejo.tracked(&target, &tracked).await.unwrap();
        let (missing, found) = refreshed.split_last().unwrap();
        assert_eq!(
            missing.error.as_ref().unwrap().kind,
            ErrorKind::Inaccessible
        );
        for (answer, listed) in found.iter().zip(kinds.values()) {
            let item = answer.item.as_ref().expect("found again");
            assert_eq!(
                (item.status.as_str(), item.outcome),
                (listed[0].status.as_str(), listed[0].outcome)
            );
        }
        println!("refreshed: {} found, 1 missing", found.len());
        readonly::tests::attempt_writes(&forgejo.client, &target, &format!("{path}/1")).await;
        let requests = forgejo.requests().unwrap();
        println!("sent: {:?}, refused: {}", requests.sent, requests.refused);
        assert_eq!(requests.sent.keys().collect::<Vec<_>>(), ["GET"]);
        assert_eq!(requests.refused, 4);
    }
}
