//! GitHub and GitHub Enterprise Server, read only: a token as a bearer
//! token; the open issues and pull requests that wait on the account, its
//! own open pull requests, and, when asked, another user's activity that the
//! token can see.
//! Every request goes through the read-only client in `readonly`, which
//! refuses anything that could write.

mod bot;
mod readonly;

pub use bot::Bot;
pub use readonly::Memory;

use super::inaccessible;
use crate::{
    connector::{
        Account, BoxFuture, Connector, ConnectorError, ConnectorResult, ErrorKind, Listing, Reason,
        Refreshed, RemoteItem, Requests, Target, Tracked,
    },
    connectors::source_url,
    model::{valid_github_login, valid_github_name},
    outcome,
};
use anyhow::{bail, Context, Result};
use readonly::{Got, ReadOnly};
use serde_json::Value;
use std::sync::Arc;
use url::Url;

const SOURCE: &str = "GitHub";
/// Results per search page, GitHub's largest.
const PER_PAGE: usize = 100;
/// GitHub's search serves at most 1000 results, ten pages.
const MAX_PAGES: usize = 10;
/// Added to every search: open work, in repositories that take changes.
const OPEN: &str = "is:open archived:false";

/// A search for what waits on the account, the reason it gives, and the
/// attention category of what it finds.
struct Search {
    query: &'static str,
    reason: Option<Reason>,
    action: &'static str,
}

/// What waits on the account. GitHub has no `team-review-requested:@me`,
/// but `review-requested:@me` matches the account's teams too, and
/// `user-review-requested:@me` only the account.
const MINE: &[Search] = &[
    Search {
        query: "is:pr review-requested:@me",
        reason: Some(Reason::TeamReviewRequested),
        action: "review",
    },
    Search {
        query: "is:pr user-review-requested:@me",
        reason: Some(Reason::ReviewRequested),
        action: "review",
    },
    Search {
        query: "assignee:@me",
        reason: Some(Reason::Assigned),
        action: "next",
    },
    // In the inbox, but only waiting on the account when one of the next two
    // finds it.
    Search {
        query: "is:pr author:@me",
        reason: None,
        action: "waiting",
    },
    Search {
        query: "is:pr author:@me review:changes_requested",
        reason: Some(Reason::ChangesRequested),
        action: "next",
    },
    Search {
        query: "is:pr author:@me status:failure",
        reason: Some(Reason::ChecksFailing),
        action: "next",
    },
];

/// What waits on a watched user, by search qualifier, and the reason. As
/// for the account, `review-requested:` also matches the user's teams, so
/// only `user-review-requested:` makes a request direct.
const WATCHED: &[(&str, Reason)] = &[
    ("is:pr review-requested:", Reason::TeamReviewRequested),
    ("is:pr user-review-requested:", Reason::ReviewRequested),
    ("assignee:", Reason::Assigned),
    ("author:", Reason::Authored),
];

pub struct GitHub {
    client: ReadOnly,
}

impl GitHub {
    pub fn new(memory: Arc<Memory>) -> Result<Self> {
        Ok(Self {
            client: ReadOnly::new(memory)?,
        })
    }
    async fn mine(&self, target: &Target) -> ConnectorResult<Vec<RemoteItem>> {
        let mut all = Listing::default();
        for search in MINE {
            for row in search_issues(&self.client, target, search.query).await? {
                let mut item = github_item(&row, search.action).map_err(ConnectorError::invalid)?;
                // A draft of one's own isn't waiting on others yet.
                if search.reason.is_none() && item.status == DRAFT {
                    item.action = "next".into();
                }
                item.reasons.extend(search.reason);
                all.add(item);
            }
        }
        let mut items = all.into_items();
        direct_over_team(&mut items);
        Ok(items)
    }
    async fn watched(&self, target: &Target, login: &str) -> ConnectorResult<Vec<RemoteItem>> {
        // It goes into search queries, where a space or a colon would change
        // what they find.
        if !valid_github_login(login) {
            return Err(ConnectorError::invalid(format!(
                "'{login}' is not a GitHub login"
            )));
        }
        let mut all = Listing::default();
        for (qualifier, reason) in WATCHED {
            let query = format!("{qualifier}{login}");
            for row in search_issues(&self.client, target, &query).await? {
                let mut item = github_item(&row, "").map_err(ConnectorError::invalid)?;
                item.reasons.push(*reason);
                all.add(item);
            }
        }
        let mut items = all.into_items();
        direct_over_team(&mut items);
        Ok(items)
    }
}

/// The account `target`'s token belongs to.
async fn current_user(client: &ReadOnly, target: &Target) -> ConnectorResult<Account> {
    let v = client.get_found(target, "user", &[]).await?;
    let id = v["id"]
        .as_u64()
        .ok_or_else(|| ConnectorError::invalid("GitHub's answer has no account ID"))?;
    let login = v["login"]
        .as_str()
        .ok_or_else(|| ConnectorError::invalid("GitHub's answer has no login"))?;
    Ok(Account {
        id: id.to_string(),
        name: login.into(),
    })
}

/// Every open issue and pull request `query` finds, or an error: never
/// part of them.
async fn search_issues(
    client: &ReadOnly,
    target: &Target,
    query: &str,
) -> ConnectorResult<Vec<Value>> {
    let q = format!("{query} {OPEN}");
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let v = client
            .get_found(
                target,
                "search/issues",
                &[
                    ("q", q.clone()),
                    ("sort", "updated".into()),
                    ("order", "desc".into()),
                    ("per_page", PER_PAGE.to_string()),
                    ("page", page.to_string()),
                ],
            )
            .await?;
        if v["incomplete_results"].as_bool() == Some(true) {
            return Err(ConnectorError::new(
                ErrorKind::Transient,
                "GitHub's search timed out before finding everything; no partial results were saved",
            ));
        }
        let total = v["total_count"]
            .as_u64()
            .ok_or_else(|| ConnectorError::invalid("Unexpected GitHub search answer"))?;
        if total > (PER_PAGE * MAX_PAGES) as u64 {
            return Err(ConnectorError::invalid(format!(
                "GitHub found {total} items for '{query}', more than one sync reads; no partial results were saved"
            )));
        }
        let rows = v["items"]
            .as_array()
            .ok_or_else(|| ConnectorError::invalid("Unexpected GitHub search answer"))?;
        all.extend(rows.iter().cloned());
        if all.len() as u64 >= total {
            return Ok(all);
        }
        // A short page doesn't end it while GitHub counts more; an
        // empty one does, as an error.
        if rows.is_empty() {
            return Err(ConnectorError::new(
                ErrorKind::Transient,
                "GitHub's search returned fewer items than it counted; no partial results were saved",
            ));
        }
    }
    Err(ConnectorError::invalid(
        "GitHub's search kept paging; no partial results were saved",
    ))
}

/// Fetch tracked issues and pull requests again, by their keys, each as
/// `github_item` reads it.
async fn refresh_issues(
    client: &ReadOnly,
    target: &Target,
    tracked: &[Tracked],
) -> ConnectorResult<Vec<Refreshed>> {
    let mut answers = Vec::new();
    for item in tracked {
        let (repository, number) = parse_key(&item.key)
            .ok_or_else(|| ConnectorError::invalid("Invalid stored GitHub item key"))?;
        let path = format!("repos/{repository}/issues/{number}");
        let mut got = client.get(target, &path, &[]).await?;
        // A transferred issue redirects to its new place: follow it once,
        // only within the same API.
        if let Got::Moved(_, Some(to)) = got {
            got = client.get_url(target, to).await?;
        }
        answers.push(match got {
            Got::Found(row) => {
                let found = github_item(&row, "follow-up").map_err(ConnectorError::invalid)?;
                if found.remote_id == item.remote_id {
                    Refreshed::found(&item.remote_id, found)
                } else {
                    Refreshed::inaccessible(
                        &item.remote_id,
                        "GitHub now shows another item at this address",
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

impl Connector for GitHub {
    fn kind(&self) -> &str {
        "github"
    }
    fn verify<'a>(&'a self, target: &'a Target) -> BoxFuture<'a, ConnectorResult<Account>> {
        Box::pin(current_user(&self.client, target))
    }
    fn list<'a>(
        &'a self,
        target: &'a Target,
        _account: &'a Account,
    ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
        Box::pin(self.mine(target))
    }
    fn refresh<'a>(
        &'a self,
        target: &'a Target,
        _account: &'a Account,
        tracked: &'a [Tracked],
    ) -> BoxFuture<'a, ConnectorResult<Vec<Refreshed>>> {
        Box::pin(refresh_issues(&self.client, target, tracked))
    }
    fn watch<'a>(
        &'a self,
        target: &'a Target,
        login: &'a str,
    ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
        Box::pin(self.watched(target, login))
    }
    fn requests(&self) -> Option<Requests> {
        Some(self.client.requests())
    }
}

/// A review search for a login also finds requests to its teams, and the
/// `user-review-requested` one only those to the login: an item both found
/// was asked of the login directly.
fn direct_over_team(items: &mut [RemoteItem]) {
    for item in items {
        if item.reasons.contains(&Reason::ReviewRequested) {
            item.reasons.retain(|r| *r != Reason::TeamReviewRequested);
        }
    }
}

/// How a draft pull request's status reads.
const DRAFT: &str = "draft";

/// `OWNER/REPO` and the number of a key such as `OWNER/REPO#12`.
fn parse_key(key: &str) -> Option<(String, u64)> {
    let (repository, number) = key.rsplit_once('#')?;
    let (owner, repo) = repository.split_once('/')?;
    let number = number.parse().ok()?;
    (valid_github_name(owner) && valid_github_name(repo))
        .then(|| (format!("{owner}/{repo}"), number))
}

/// `OWNER/REPO` from an issue's `repository_url`, its API address.
fn repository(v: &Value) -> Result<String> {
    let url = Url::parse(v["repository_url"].as_str().context("Missing repository")?)?;
    let mut segments = url
        .path_segments()
        .context("Invalid repository address")?
        .rev();
    let (repo, owner) = (segments.next(), segments.next());
    match (owner, repo) {
        (Some(owner), Some(repo)) if valid_github_name(owner) && valid_github_name(repo) => {
            Ok(format!("{owner}/{repo}"))
        }
        _ => bail!("Invalid repository address"),
    }
}

/// An issue or pull request as GitHub's issues and search APIs return it.
pub fn github_item(v: &Value, action: &str) -> Result<RemoteItem> {
    let id = v["id"].as_u64().context("Missing issue ID")?;
    let number = v["number"].as_u64().context("Missing issue number")?;
    let state = v["state"].as_str().context("Missing state")?;
    let (status, (outcome, outcome_reason)) = match v["pull_request"].as_object() {
        Some(pr) => {
            let merged = pr.get("merged_at").is_some_and(Value::is_string);
            let draft = v["draft"].as_bool().unwrap_or(false);
            let status = match (state, merged, draft) {
                (_, true, _) => "merged",
                ("open", _, true) => DRAFT,
                _ => state,
            };
            (
                status.to_string(),
                outcome::github_pull_request(state, merged, draft),
            )
        }
        None => {
            let reason = v["state_reason"].as_str();
            let status = match (state, reason) {
                ("closed", Some(reason)) => format!("closed as {}", reason.replace('_', " ")),
                _ => state.to_string(),
            };
            (status, outcome::github_issue(state, reason))
        }
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
        extract::Query,
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::get,
        Json, Router,
    };
    use std::collections::HashMap;

    fn issue(n: u64, state: &str, reason: Option<&str>) -> Value {
        serde_json::json!({
            "id": 1000 + n, "number": n, "title": format!("Issue {n}"),
            "html_url": format!("https://github.com/o/r/issues/{n}"),
            "repository_url": "https://api.github.com/repos/o/r",
            "state": state, "state_reason": reason, "updated_at": "2026-10-12T00:00:00Z",
        })
    }
    fn pr(n: u64, state: &str, merged: bool, draft: bool) -> Value {
        serde_json::json!({
            "id": 1000 + n, "number": n, "title": format!("Change {n}"),
            "html_url": format!("https://github.com/o/r/pull/{n}"),
            "repository_url": "https://api.github.com/repos/o/r",
            "state": state, "draft": draft, "updated_at": "2026-10-12T00:00:00Z",
            "pull_request": {"merged_at": merged.then_some("2026-10-11T00:00:00Z")},
        })
    }
    fn connection() -> Connection {
        Connection {
            provider: "github".into(),
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
            (pr(1, "open", false, true), "draft", Open),
            (pr(1, "open", false, false), "open", Open),
            (issue(1, "open", None), "open", Open),
            (issue(1, "open", Some("reopened")), "open", Open),
            (
                issue(1, "closed", Some("completed")),
                "closed as completed",
                Delivered,
            ),
            (
                issue(1, "closed", Some("not_planned")),
                "closed as not planned",
                ClosedWithoutDelivery,
            ),
            (
                issue(1, "closed", Some("duplicate")),
                "closed as duplicate",
                ClosedWithoutDelivery,
            ),
        ] {
            let i = connector::to_item(&connection(), "github", github_item(&v, "next").unwrap())
                .unwrap();
            assert_eq!((i.status.as_str(), i.outcome), (status, outcome), "{v}");
            assert_eq!(i.key, "o/r#1");
            assert_eq!(i.remote_id, "1001");
            assert_eq!(i.action, if outcome == Open { "next" } else { "" });
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
        let mut v = issue(1, "open", None);
        v["repository_url"] = "https://api.github.com/repos/o/..".into();
        assert!(github_item(&v, "next").is_err());
    }

    /// A search answer: one page of `items` out of `total`.
    fn found(items: Vec<Value>, total: usize) -> Json<Value> {
        Json(serde_json::json!({"total_count": total, "incomplete_results": false, "items": items}))
    }
    async fn search(Query(q): Query<HashMap<String, String>>) -> Response {
        let query = q["q"]
            .strip_suffix(&format!(" {OPEN}"))
            .unwrap()
            .to_string();
        let page: usize = q["page"].parse().unwrap();
        assert_eq!(q["per_page"], "100");
        match query.as_str() {
            "is:pr review-requested:@me" => found(
                vec![pr(1, "open", false, false), pr(9, "open", false, false)],
                2,
            ),
            "is:pr user-review-requested:@me" => found(vec![pr(9, "open", false, false)], 1),
            // Two pages.
            "assignee:@me" if page == 1 => {
                found((100..200).map(|n| issue(n, "open", None)).collect(), 101)
            }
            "assignee:@me" => found(vec![issue(2, "open", None)], 101),
            // A short page, though GitHub counts more.
            "is:pr author:@me" if page == 1 => found(
                vec![pr(4, "open", false, false), pr(5, "open", false, true)],
                3,
            ),
            "is:pr author:@me" => found(vec![pr(6, "open", false, false)], 3),
            "is:pr author:@me review:changes_requested" => {
                found(vec![pr(4, "open", false, false)], 1)
            }
            "is:pr author:@me status:failure" => found(
                vec![pr(4, "open", false, false), pr(6, "open", false, false)],
                2,
            ),
            // Through a team for 1, directly for 21.
            "is:pr review-requested:someone" => found(
                vec![pr(1, "open", false, false), pr(21, "open", false, false)],
                2,
            ),
            "is:pr user-review-requested:someone" => found(vec![pr(21, "open", false, false)], 1),
            "assignee:someone" => {
                found(vec![pr(1, "open", false, false), issue(2, "open", None)], 2)
            }
            "author:someone" => found(vec![pr(20, "open", false, true)], 1),
            other => panic!("unexpected search {other}"),
        }
        .into_response()
    }
    fn router() -> Router {
        routes(get(search))
    }
    fn routes(search: axum::routing::MethodRouter) -> Router {
        Router::new()
            .route(
                "/api/v3/user",
                get(|| async { Json(serde_json::json!({"id": 42, "login": "me"})) }),
            )
            .route("/api/v3/search/issues", get(search))
            .route(
                "/api/v3/repos/o/r/issues/7",
                get(|| async { Json(pr(7, "closed", true, false)) }),
            )
            .route(
                "/api/v3/repos/o/r/issues/8",
                get(|| async { Json(issue(8, "closed", Some("not_planned"))) }),
            )
            // Transferred: followed once, within the same API.
            .route(
                "/api/v3/repos/o/r/issues/10",
                get(|| async {
                    (
                        StatusCode::MOVED_PERMANENTLY,
                        [("location", "/api/v3/repositories/1/issues/10")],
                    )
                }),
            )
            .route(
                "/api/v3/repositories/1/issues/10",
                get(|| async { Json(pr(10, "closed", true, false)) }),
            )
    }
    async fn sync(router: Router, old: &[Item]) -> (ConnectorResult<Vec<Item>>, Requests) {
        let (target, task) = crate::connectors::fixture(router, "test-only-token").await;
        let github = GitHub::new(Arc::default()).unwrap();
        let items = connector::sync(&github, &target, &connection(), old).await;
        task.abort();
        (items, github.requests().unwrap())
    }

    #[tokio::test]
    async fn lists_what_waits_on_the_account_once_with_every_reason() {
        use Reason::*;
        let stored = |v: Value| {
            connector::to_item(&connection(), "github", github_item(&v, "next").unwrap()).unwrap()
        };
        // Tracked before: merged since, closed as not planned since, and
        // gone (no route answers 404).
        let old = [
            stored(pr(7, "open", false, false)),
            stored(issue(8, "open", None)),
            stored(issue(3, "open", None)),
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
        use Outcome::*;
        for (n, expected) in [
            (1, ("review", vec![TeamReviewRequested], Open)),
            // Asked directly: not also through a team.
            (9, ("review", vec![ReviewRequested], Open)),
            (2, ("next", vec![Assigned], Open)),
            (150, ("next", vec![Assigned], Open)),
            (4, ("next", vec![ChangesRequested, ChecksFailing], Open)),
            (5, ("next", vec![], Open)),
            (6, ("next", vec![ChecksFailing], Open)),
            (7, ("", vec![], Delivered)),
            (8, ("", vec![], ClosedWithoutDelivery)),
            (3, ("", vec![], Unknown)),
            (10, ("", vec![], Delivered)),
        ] {
            assert_eq!(listed(n), expected, "item {n}");
        }
        // 2 to review, 101 assigned, 3 authored, 4 tracked.
        assert_eq!(items.len(), 110);
        // Only reads went out: the account, 8 search pages, and 4 items,
        // one of them moved.
        let expected = Requests {
            sent: [("GET".to_string(), 14)].into(),
            not_modified: 0,
            refused: 0,
        };
        assert_eq!(requests, expected);
    }

    #[tokio::test]
    async fn watching_finds_what_waits_on_someone_else() {
        use Reason::*;
        let (target, task) = crate::connectors::fixture(router(), "test-only-token").await;
        let github = GitHub::new(Arc::default()).unwrap();
        let found = connector::watch(&github, &target, &connection(), "someone")
            .await
            .unwrap();
        let found: Vec<_> = found
            .iter()
            .map(|i| (i.key.as_str(), i.action.as_str(), i.reasons.clone()))
            .collect();
        assert_eq!(
            found,
            [
                ("o/r#1", "", vec![TeamReviewRequested, Assigned]),
                ("o/r#2", "", vec![Assigned]),
                ("o/r#20", "", vec![Authored]),
                ("o/r#21", "", vec![ReviewRequested]),
            ]
        );
        // A login that would change the searches is refused unsent.
        for login in ["someone is:closed", "a:b", "@me", ""] {
            let error = connector::watch(&github, &target, &connection(), login)
                .await
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Invalid, "{login}");
        }
        assert_eq!(github.requests().unwrap().sent["GET"], 4);
        task.abort();
    }

    #[tokio::test]
    async fn searches_are_complete_or_fail() {
        for (answer, kind, message) in [
            (
                serde_json::json!({"total_count": 1001, "incomplete_results": false, "items": []}),
                ErrorKind::Invalid,
                "more than one sync reads",
            ),
            (
                serde_json::json!({"total_count": 1, "incomplete_results": true, "items": []}),
                ErrorKind::Transient,
                "timed out",
            ),
            (
                serde_json::json!({"total_count": 5, "incomplete_results": false, "items": []}),
                ErrorKind::Transient,
                "fewer items than it counted",
            ),
        ] {
            let router = routes(get(move || {
                let answer = answer.clone();
                async move { Json(answer) }
            }));
            let (items, _) = sync(router, &[]).await;
            let error = items.unwrap_err();
            assert_eq!(error.kind, kind, "{error}");
            assert!(error.message.contains(message), "{error}");
        }
        // Another account's token is refused before anything is listed.
        let router = Router::new().route(
            "/api/v3/user",
            get(|| async { Json(serde_json::json!({"id": 43, "login": "other"})) }),
        );
        let (items, requests) = sync(router, &[]).await;
        assert_eq!(items.unwrap_err().kind, ErrorKind::Auth);
        assert_eq!(requests.sent["GET"], 1);
        // A rejected token says so, without the response body.
        let router = Router::new().route(
            "/api/v3/user",
            get(|| async { (StatusCode::UNAUTHORIZED, "test-only-token is bad") }),
        );
        let error = sync(router, &[]).await.0.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Auth);
        assert!(!error.message.contains("test-only-token"), "{error}");
    }
}
