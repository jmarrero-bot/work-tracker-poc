//! The bot connector: what a harness bot such as jmarrero-bot works on,
//! read through GitHub's read-only client like everything else on GitHub.
//!
//! - The items on its Projects (v2) board, which only GraphQL serves, so
//!   they are read with queries, which the client checks: their title,
//!   link, the board's Status (their native status), Priority, Why, News
//!   and Branch.
//! - Its open pull requests in its forge organization, with REST search,
//!   each linked to the board item whose Branch is its address.
//! - Its coordinator's heartbeat, the comment by the bot on its tracker's
//!   heartbeat issue, with REST: the coordinator's loop state, and each of
//!   its workers as an agent run. Agent runs are not items.
//!
//! The bot's pull requests and heartbeat are matched by its numeric account
//! ID, not its login, which can be renamed and taken by someone else. The
//! ID is kept in its settings, and looked up by its login with one REST
//! read when it is connected or verified, or when a sync finds none kept.
//!
//! Outcomes follow the rule every connector does: closed is not delivered,
//! and neither is the board saying Done (see `outcome::bot_board_item`).

use super::{
    current_user, github_item, readonly::ReadOnly, refresh_issues, search_issues, Got, Memory,
};
use crate::{
    bot::{find_heartbeat, BoardSummary, BotConfig, BotFields, BotKind, Heartbeat},
    connector::{
        Account, BoxFuture, Connector, ConnectorError, ConnectorResult, ErrorKind, Reason,
        Refreshed, RemoteItem, Requests, Target, Tracked,
    },
    connectors::source_url,
    model::{valid_github_name, Outcome, Timestamp},
    outcome::{self, Linked},
};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Map};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError},
};
use url::Url;

/// Board items per GraphQL page, GitHub's largest.
const PAGE: usize = 100;
/// Most board pages one sync reads: a board with more fails the sync,
/// rather than show part of it.
const MAX_PAGES: usize = 10;
/// What starts the GraphQL ID of a board item, and so tells a tracked
/// board item from a tracked fork pull request.
const BOARD_ITEM_ID: &str = "PVTI_";
/// Board statuses that wait on the person: a decision or an action
/// (Needs human), or a review of a draft (Draft).
const NEEDS_HUMAN: &str = "Needs human";
const DRAFT: &str = "Draft";
/// How an item without a board status reads.
const NO_STATUS: &str = "No status";
/// The GraphQL error GitHub answers for an ID that names nothing.
const NOT_FOUND: &str = "NOT_FOUND";
/// Comments per page of the heartbeat issue, GitHub's largest.
const COMMENTS_PAGE: usize = 100;
/// Most pages of comments read on the heartbeat issue. It is locked, so
/// its comments are few.
const COMMENT_PAGES: usize = 3;

/// What is read of each board item, in listings and refreshes alike.
const BOARD_ITEM: &str = r#"
fragment BoardItem on ProjectV2Item {
  id
  isArchived
  updatedAt
  project { url }
  content {
    __typename
    ... on Issue { url title number state stateReason updatedAt repository { nameWithOwner } }
    ... on PullRequest { url title number state merged updatedAt repository { nameWithOwner } }
  }
  status: fieldValueByName(name: "Status") { ... on ProjectV2ItemFieldSingleSelectValue { name } }
  priority: fieldValueByName(name: "Priority") { ... on ProjectV2ItemFieldSingleSelectValue { name } }
  why: fieldValueByName(name: "Why") { ... on ProjectV2ItemFieldTextValue { text } }
  news: fieldValueByName(name: "News") { ... on ProjectV2ItemFieldTextValue { text } }
  branch: fieldValueByName(name: "Branch") { ... on ProjectV2ItemFieldTextValue { text } }
}
"#;

/// A page of the board of an owner read through `owner_field`
/// (`organization` or `user`).
fn board_query(owner_field: &str) -> String {
    format!(
        "query Board($owner: String!, $number: Int!, $after: String) {{
  owner: {owner_field}(login: $owner) {{
    projectV2(number: $number) {{
      url
      items(first: {PAGE}, after: $after) {{
        pageInfo {{ hasNextPage endCursor }}
        nodes {{ ...BoardItem }}
      }}
    }}
  }}
}}
{BOARD_ITEM}"
    )
}

/// Board items by their GraphQL IDs, archived ones included.
fn items_query() -> String {
    format!(
        "query Items($ids: [ID!]!) {{
  nodes(ids: $ids) {{ ... on ProjectV2Item {{ ...BoardItem }} }}
}}
{BOARD_ITEM}"
    )
}

#[derive(Deserialize)]
struct Page {
    owner: Option<Owner>,
}

#[derive(Deserialize)]
struct Owner {
    #[serde(rename = "projectV2")]
    project: Option<Project>,
}

#[derive(Deserialize)]
struct Project {
    url: String,
    items: Items,
}

#[derive(Deserialize)]
struct Items {
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
    nodes: Vec<Option<BoardNode>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

/// A board item as the queries read it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoardNode {
    id: String,
    is_archived: bool,
    updated_at: Option<String>,
    project: Option<ProjectRef>,
    content: Option<Content>,
    status: Option<Select>,
    priority: Option<Select>,
    why: Option<Text>,
    news: Option<Text>,
    branch: Option<Text>,
}

#[derive(Deserialize)]
struct ProjectRef {
    url: String,
}

/// A single-select field's value: empty when the field holds none.
#[derive(Deserialize)]
struct Select {
    #[serde(default)]
    name: Option<String>,
}

/// A text field's value.
#[derive(Deserialize)]
struct Text {
    #[serde(default)]
    text: Option<String>,
}

/// What a board item is about. A draft issue has no address, so it is no
/// item; anything else is something this token can't read.
#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum Content {
    Issue(Issue),
    PullRequest(PullRequest),
    DraftIssue {},
    #[serde(other)]
    Other,
}

/// What a board item turned out to be.
enum Node {
    Item(Box<Read>),
    /// A draft issue: left out, as it has no address.
    Draft,
    /// Its issue or pull request is hidden from this token, which GitHub
    /// answers with no content: counted, so the board is never shown as
    /// complete without it.
    Unreadable,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Issue {
    url: String,
    title: String,
    number: u64,
    state: String,
    state_reason: Option<String>,
    updated_at: String,
    repository: Repository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequest {
    url: String,
    title: String,
    number: u64,
    state: String,
    merged: bool,
    updated_at: String,
    repository: Repository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    name_with_owner: String,
}

/// A board item read, before its linked pull request is.
struct Read {
    item: RemoteItem,
    status: Option<String>,
    /// What its own issue or pull request came to.
    content: outcome::Derived,
    /// Its Branch, if that is a pull request on this instance: `OWNER/REPO`
    /// and its number.
    linked: Option<(String, u64)>,
}

fn unexpected() -> ConnectorError {
    ConnectorError::invalid("GitHub's answer about the bot's board isn't what was asked for")
}

/// A text field's value, trimmed, if there is one.
fn text(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `OWNER/REPO` and the number of `url`, if it is a pull request on the
/// instance whose web address is `base`.
fn pull_request_of(url: &str, base: &Url) -> Option<(String, u64)> {
    let url = Url::parse(url).ok()?;
    if url.origin() != base.origin() || url.query().is_some() {
        return None;
    }
    let path = url.path().strip_prefix(base.path())?;
    let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    match parts[..] {
        [owner, repo, "pull", number] if valid_github_name(owner) && valid_github_name(repo) => {
            Some((format!("{owner}/{repo}"), number.parse().ok()?))
        }
        _ => None,
    }
}

/// A board item as an item, unless it is a draft or can't be read.
fn read_node(node: BoardNode, base: &Url) -> ConnectorResult<Node> {
    let (key, title, url, content, updated) = match node.content {
        Some(Content::Issue(i)) => {
            let state = i.state.to_lowercase();
            let reason = i.state_reason.map(|r| r.to_lowercase());
            (
                format!("{}#{}", i.repository.name_with_owner, i.number),
                i.title,
                i.url,
                outcome::github_issue(&state, reason.as_deref()),
                i.updated_at,
            )
        }
        Some(Content::PullRequest(p)) => (
            format!("{}#{}", p.repository.name_with_owner, p.number),
            p.title,
            p.url,
            outcome::github_pull_request(&p.state.to_lowercase(), p.merged, false),
            p.updated_at,
        ),
        Some(Content::DraftIssue {}) => return Ok(Node::Draft),
        Some(Content::Other) | None => return Ok(Node::Unreadable),
    };
    let status = node.status.and_then(|s| text(s.name));
    let branch = text(node.branch.and_then(|b| b.text)).filter(|b| source_url(b).is_ok());
    let waits = matches!(status.as_deref(), Some(NEEDS_HUMAN | DRAFT));
    let action = match status.as_deref() {
        Some(NEEDS_HUMAN) => "next",
        Some(DRAFT) => "review",
        _ => "waiting",
    };
    // When it last changed, on the board or at its source.
    let updated_at = node
        .updated_at
        .filter(|board| *board > updated)
        .unwrap_or(updated);
    let item = RemoteItem {
        remote_id: node.id,
        key,
        title,
        url: source_url(&url).map_err(ConnectorError::invalid)?,
        status: status.clone().unwrap_or_else(|| NO_STATUS.into()),
        outcome: content.0,
        outcome_reason: content.1.clone(),
        action: action.into(),
        updated_at,
        reasons: if waits {
            vec![Reason::BotWaiting]
        } else {
            vec![]
        },
        bot: Some(BotFields {
            kind: BotKind::BoardItem,
            priority: node.priority.and_then(|p| text(p.name)),
            why: text(node.why.and_then(|w| w.text)),
            news: text(node.news.and_then(|n| n.text)),
            branch: branch.clone(),
            board_item: None,
        }),
    };
    Ok(Node::Item(Box::new(Read {
        item,
        status,
        content,
        linked: branch.and_then(|b| pull_request_of(&b, base)),
    })))
}

pub struct Bot {
    client: ReadOnly,
    config: BotConfig,
    /// What its last listing found.
    summary: Mutex<Option<BoardSummary>>,
    /// The bot's numeric account ID: as its settings keep it, or as it was
    /// looked up for settings without one.
    id: Mutex<Option<u64>>,
}

impl Bot {
    pub fn new(memory: Arc<Memory>, config: BotConfig) -> Result<Self> {
        config.check()?;
        Ok(Self {
            client: ReadOnly::new(memory)?,
            id: Mutex::new(config.bot_id),
            config,
            summary: Mutex::default(),
        })
    }
    /// The numeric ID of the account the bot's login names now, read with
    /// `GET users/{login}`.
    async fn look_up(&self, target: &Target) -> ConnectorResult<u64> {
        let login = &self.config.bot_login;
        let user = match self
            .client
            .get(target, &format!("users/{login}"), &[])
            .await?
        {
            Got::Found(user) => user,
            Got::Moved(..) | Got::Inaccessible(_) => {
                return Err(ConnectorError::invalid(format!(
                    "GitHub has no account {login} that this token can see; check the bot's login"
                )))
            }
        };
        let named = user["login"]
            .as_str()
            .is_some_and(|l| l.eq_ignore_ascii_case(login));
        match user["id"].as_u64() {
            Some(id) if named && id != 0 => Ok(id),
            _ => Err(ConnectorError::invalid(format!(
                "GitHub's answer about the account {login} isn't that account"
            ))),
        }
    }
    /// The bot's numeric account ID: as its settings keep it, or else
    /// looked up once.
    async fn account_id(&self, target: &Target) -> ConnectorResult<u64> {
        if let Some(id) = *self.id.lock().unwrap_or_else(PoisonError::into_inner) {
            return Ok(id);
        }
        let id = self.look_up(target).await?;
        *self.id.lock().unwrap_or_else(PoisonError::into_inner) = Some(id);
        Ok(id)
    }
    /// The board's address, which the queries check that they read.
    fn board_url(&self, target: &Target) -> ConnectorResult<String> {
        self.config
            .board_url(&target.base_url)
            .map(String::from)
            .map_err(ConnectorError::invalid)
    }
    /// Every item on the board but archived ones and drafts, or an error:
    /// never part of them; and how many it has that this token can't read.
    async fn read_board(&self, target: &Target) -> ConnectorResult<(Vec<Read>, usize)> {
        let board = self.board_url(target)?;
        let document = board_query(self.config.board_owner_type.graphql_field());
        let mut all = Vec::new();
        let mut unreadable = 0;
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut variables = Map::new();
            variables.insert("owner".into(), json!(self.config.board_owner));
            variables.insert("number".into(), json!(self.config.board_number));
            variables.insert("after".into(), json!(after));
            let answer = self.client.graphql(target, &document, variables).await?;
            let page: Page = serde_json::from_value(answer.data).map_err(|_| unexpected())?;
            let Some(project) = page.owner.and_then(|o| o.project) else {
                return Err(ConnectorError::new(
                    ErrorKind::Auth,
                    format!("GitHub shows this token no board at {board}; check the board, and that the token can read projects"),
                ));
            };
            if !answer.errors.is_empty() {
                return Err(ConnectorError::invalid(
                    "GitHub's GraphQL API answered with errors about the bot's board",
                ));
            }
            if !project.url.eq_ignore_ascii_case(&board) {
                return Err(unexpected());
            }
            for node in project.items.nodes {
                // GitHub lists an item it can't show this token as null.
                let Some(node) = node else {
                    unreadable += 1;
                    continue;
                };
                if node.is_archived {
                    continue;
                }
                match read_node(node, &target.base_url)? {
                    Node::Item(read) => all.push(*read),
                    Node::Draft => (),
                    Node::Unreadable => unreadable += 1,
                }
            }
            let info = project.items.page_info;
            if !info.has_next_page {
                return Ok((all, unreadable));
            }
            after = Some(info.end_cursor.ok_or_else(unexpected)?);
        }
        Err(ConnectorError::invalid(format!(
            "The bot's board has more than {} items, more than one sync reads; no partial results were saved",
            PAGE * MAX_PAGES
        )))
    }
    /// What the pull request `OWNER/REPO` number `number` came to.
    async fn linked(
        &self,
        target: &Target,
        repository: &str,
        number: u64,
    ) -> ConnectorResult<Linked> {
        let path = format!("repos/{repository}/pulls/{number}");
        Ok(match self.client.get(target, &path, &[]).await? {
            Got::Found(pr) if pr["merged_at"].is_string() => Linked::Merged,
            Got::Found(_) => Linked::NotMerged,
            Got::Moved(..) | Got::Inaccessible(_) => Linked::Unreadable,
        })
    }
    /// A board item with its outcome: its board status, its own issue or
    /// pull request, and, only for an item the board says is Done that
    /// isn't delivered by itself, its linked pull request.
    async fn finish(&self, target: &Target, read: Read) -> ConnectorResult<RemoteItem> {
        let Read {
            mut item,
            status,
            content,
            linked,
        } = read;
        // Only Done reads it, so only then is it fetched.
        let done = status.as_deref() == Some(outcome::BOARD_DONE);
        let linked = match linked {
            Some((repository, number)) if done && content.0 != Outcome::Delivered => {
                self.linked(target, &repository, number).await?
            }
            _ => Linked::Absent,
        };
        let (outcome, reason) = outcome::bot_board_item(status.as_deref(), content, linked);
        item.outcome = outcome;
        item.outcome_reason = reason;
        Ok(item)
    }
    async fn list_all(&self, target: &Target) -> ConnectorResult<Vec<RemoteItem>> {
        let (board, unreadable) = self.read_board(target).await?;
        let mut items = Vec::new();
        for read in board {
            items.push(self.finish(target, read).await?);
        }
        let mut summary = BoardSummary {
            unreadable,
            ..BoardSummary::default()
        };
        for i in &items {
            *summary.statuses.entry(i.status.clone()).or_default() += 1;
        }
        // Each fork pull request, linked to the board item whose Branch is
        // its address. One that is a board item itself is that item.
        let key = |url: &str| url.trim_end_matches('/').to_ascii_lowercase();
        let on_board: HashSet<String> = items.iter().map(|i| key(&i.url)).collect();
        let linking: HashMap<String, String> = items
            .iter()
            .filter_map(|i| {
                let branch = i.bot.as_ref()?.branch.as_deref()?;
                Some((key(branch), i.url.clone()))
            })
            .collect();
        let query = format!(
            "is:pr author:{} org:{}",
            self.config.bot_login, self.config.forge_org
        );
        let bot = self.account_id(target).await?;
        for row in search_issues(&self.client, target, &query).await? {
            // The search goes by login: one that was renamed and taken
            // finds another account's pull requests, which aren't the bot's.
            if row["user"]["id"].as_u64() != Some(bot) {
                continue;
            }
            let mut pr = github_item(&row, "review").map_err(ConnectorError::invalid)?;
            if on_board.contains(&key(&pr.url)) {
                continue;
            }
            pr.bot = Some(BotFields {
                kind: BotKind::ForkPr,
                board_item: linking.get(&key(&pr.url)).cloned(),
                ..BotFields::default()
            });
            items.push(pr);
            summary.fork_prs += 1;
        }
        *self.summary.lock().unwrap_or_else(PoisonError::into_inner) = Some(summary);
        Ok(items)
    }
    /// Board items the listing no longer has, archived or removed since,
    /// by their GraphQL IDs, in one query per page of them.
    async fn refresh_board(
        &self,
        target: &Target,
        tracked: &[&Tracked],
    ) -> ConnectorResult<Vec<Refreshed>> {
        let board = self.board_url(target)?;
        let document = items_query();
        let mut answers = Vec::new();
        for chunk in tracked.chunks(PAGE) {
            let ids: Vec<&str> = chunk.iter().map(|t| t.remote_id.as_str()).collect();
            let mut variables = Map::new();
            variables.insert("ids".into(), json!(ids));
            let answer = self.client.graphql(target, &document, variables).await?;
            // An ID that names nothing any more is an answer about that
            // item; any other error fails the refresh.
            if answer
                .errors
                .iter()
                .any(|e| e["type"].as_str() != Some(NOT_FOUND))
            {
                return Err(ConnectorError::invalid(
                    "GitHub's GraphQL API answered with errors about the bot's board",
                ));
            }
            let nodes: Vec<Option<BoardNode>> =
                serde_json::from_value(answer.data["nodes"].clone()).map_err(|_| unexpected())?;
            if nodes.len() != chunk.len() {
                return Err(unexpected());
            }
            for (t, node) in chunk.iter().zip(nodes) {
                let gone = |why: &str| Refreshed::inaccessible(&t.remote_id, why);
                let Some(node) = node else {
                    answers.push(gone(
                        "It is no longer on the bot's board, or this account can't see it",
                    ));
                    continue;
                };
                let ours = node
                    .project
                    .as_ref()
                    .is_some_and(|p| p.url.eq_ignore_ascii_case(&board));
                if node.id != t.remote_id || !ours {
                    answers.push(gone("GitHub answered with an item of another board"));
                    continue;
                }
                match read_node(node, &target.base_url)? {
                    Node::Item(read) => {
                        let item = self.finish(target, *read).await?;
                        answers.push(Refreshed::found(&t.remote_id, item));
                    }
                    Node::Draft => answers.push(gone("It was made a draft issue")),
                    Node::Unreadable => {
                        answers.push(gone("Its issue or pull request can't be read"))
                    }
                }
            }
        }
        Ok(answers)
    }
    /// The heartbeat the bot publishes, read in a sync that started `at`,
    /// or none if it publishes none.
    async fn heartbeat(
        &self,
        target: &Target,
        at: Timestamp,
    ) -> ConnectorResult<Option<Heartbeat>> {
        let (owner, repo) = self
            .config
            .tracker_parts()
            .ok_or_else(|| ConnectorError::invalid("Invalid tracker repository"))?;
        let issue = self.config.heartbeat_issue;
        let bot = self.account_id(target).await?;
        let path = format!("repos/{owner}/{repo}/issues/{issue}/comments");
        for page in 1..=COMMENT_PAGES {
            let query = [
                ("per_page", COMMENTS_PAGE.to_string()),
                ("page", page.to_string()),
            ];
            let comments = match self.client.get(target, &path, &query).await? {
                Got::Found(comments) => comments,
                Got::Moved(..) | Got::Inaccessible(_) => {
                    return Err(ConnectorError::invalid(format!(
                        "The heartbeat issue {owner}/{repo}#{issue} can't be read"
                    )))
                }
            };
            let comments = comments.as_array().ok_or_else(|| {
                ConnectorError::invalid(
                    "GitHub's answer about the heartbeat issue isn't its comments",
                )
            })?;
            let found = find_heartbeat(comments, bot, &target.base_url, at)
                .map_err(|why| ConnectorError::invalid(format!("Not a heartbeat: {why}")))?;
            if found.is_some() || comments.len() < COMMENTS_PAGE {
                return Ok(found);
            }
        }
        // Not "no heartbeat": there are more comments than are read.
        Err(ConnectorError::invalid(format!(
            "No heartbeat by {} among the first {} comments of {owner}/{repo}#{issue}",
            self.config.bot_login,
            COMMENTS_PAGE * COMMENT_PAGES
        )))
    }
    async fn refresh_all(
        &self,
        target: &Target,
        tracked: &[Tracked],
    ) -> ConnectorResult<Vec<Refreshed>> {
        let (board, prs): (Vec<&Tracked>, Vec<&Tracked>) = tracked
            .iter()
            .partition(|t| t.remote_id.starts_with(BOARD_ITEM_ID));
        let mut answers = self.refresh_board(target, &board).await?;
        let prs: Vec<Tracked> = prs.into_iter().cloned().collect();
        for mut answer in refresh_issues(&self.client, target, &prs).await? {
            // The board item that linked it, as the listing last found.
            let link = prs
                .iter()
                .find(|t| t.remote_id == answer.remote_id)
                .and_then(|t| t.bot.as_ref()?.board_item.clone());
            if let Some(item) = &mut answer.item {
                // Confirmed closed or merged, as bot-pr closes one it
                // promotes upstream, and linked: its board item carries what
                // became of the work, so it is no longer tracked. One no
                // board item links stays closed without delivery, for a
                // person to decide; one in a state GitHub doesn't say, or
                // one that can't be read (no item), stays, unknown.
                let finished = matches!(
                    item.outcome,
                    Outcome::Delivered | Outcome::ClosedWithoutDelivery
                );
                if finished && link.is_some() {
                    answers.push(Refreshed::dropped(&answer.remote_id));
                    continue;
                }
                item.bot = Some(BotFields {
                    kind: BotKind::ForkPr,
                    board_item: link,
                    ..BotFields::default()
                });
            }
            answers.push(answer);
        }
        Ok(answers)
    }
}

impl Connector for Bot {
    fn kind(&self) -> &str {
        "bot"
    }
    fn verify<'a>(&'a self, target: &'a Target) -> BoxFuture<'a, ConnectorResult<Account>> {
        Box::pin(current_user(&self.client, target))
    }
    fn list<'a>(
        &'a self,
        target: &'a Target,
        _account: &'a Account,
    ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
        Box::pin(self.list_all(target))
    }
    fn refresh<'a>(
        &'a self,
        target: &'a Target,
        _account: &'a Account,
        tracked: &'a [Tracked],
    ) -> BoxFuture<'a, ConnectorResult<Vec<Refreshed>>> {
        Box::pin(self.refresh_all(target, tracked))
    }
    fn board(&self) -> Option<BoardSummary> {
        self.summary
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    fn agents<'a>(
        &'a self,
        target: &'a Target,
        at: Timestamp,
    ) -> Option<BoxFuture<'a, ConnectorResult<Option<Heartbeat>>>> {
        Some(Box::pin(self.heartbeat(target, at)))
    }
    fn look_up_bot_id<'a>(
        &'a self,
        target: &'a Target,
    ) -> Option<BoxFuture<'a, ConnectorResult<u64>>> {
        Some(Box::pin(self.look_up(target)))
    }
    fn bot_id(&self) -> Option<u64> {
        *self.id.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn requests(&self) -> Option<Requests> {
        Some(self.client.requests())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bot::tests::config,
        connector,
        connectors::test_connection,
        model::{Connection, Snapshot},
    };
    use axum::{
        extract::Query,
        http::{header, HeaderMap},
        response::{IntoResponse, Response},
        routing::{get, post},
        Json, Router,
    };
    use serde_json::Value;

    /// The fixture's own address, which its web links are under, as a
    /// GitHub Enterprise Server's are.
    fn base(headers: &HeaderMap) -> String {
        let host = headers[header::HOST].to_str().unwrap();
        format!("http://{host}/")
    }
    fn board_url(base: &str) -> String {
        format!("{base}orgs/jmarrero-forge/projects/1")
    }
    /// An issue on the board.
    fn issue(base: &str, n: u64, state: &str, reason: Option<&str>) -> Value {
        json!({
            "__typename": "Issue", "url": format!("{base}o/r/issues/{n}"),
            "title": format!("Issue {n}"), "number": n, "state": state, "stateReason": reason,
            "updatedAt": "2026-10-11T00:00:00Z", "repository": {"nameWithOwner": "o/r"},
        })
    }
    /// A board item: its ID's end, what it is about, its status and Branch.
    fn node(
        base: &str,
        id: &str,
        content: Value,
        status: Option<&str>,
        branch: Option<&str>,
    ) -> Value {
        json!({
            "id": format!("PVTI_{id}"), "isArchived": false, "updatedAt": "2026-10-12T00:00:00Z",
            "project": {"url": board_url(base)},
            "content": content,
            "status": status.map(|name| json!({"name": name})),
            "priority": {"name": "P1"},
            "why": {"text": format!("Why {id}")},
            "news": {},
            "branch": branch.map(|b| json!({"text": b.replace("BASE/", base)})),
        })
    }
    /// The board, as two pages.
    fn board(base: &str) -> [Vec<Value>; 2] {
        let pr = |n: u64, repo: &str, state: &str, merged: bool| {
            json!({
                "__typename": "PullRequest", "url": format!("{base}{repo}/pull/{n}"),
                "title": format!("Change {n}"), "number": n, "state": state, "merged": merged,
                "updatedAt": "2026-10-11T00:00:00Z", "repository": {"nameWithOwner": repo},
            })
        };
        let mut archived = node(base, "i", issue(base, 9, "OPEN", None), Some("Todo"), None);
        archived["isArchived"] = true.into();
        [
            vec![
                node(
                    base,
                    "a",
                    issue(base, 1, "OPEN", None),
                    Some("Needs human"),
                    None,
                ),
                node(
                    base,
                    "b",
                    issue(base, 2, "OPEN", None),
                    Some("Draft"),
                    Some("BASE/jmarrero-forge/r/pull/11"),
                ),
                node(base, "c", pr(3, "o/r", "MERGED", true), Some("Done"), None),
                node(
                    base,
                    "d",
                    issue(base, 4, "CLOSED", Some("COMPLETED")),
                    Some("Done"),
                    None,
                ),
                node(
                    base,
                    "e",
                    issue(base, 5, "OPEN", None),
                    Some("Done"),
                    Some("BASE/o/r/pull/5"),
                ),
                node(
                    base,
                    "f",
                    issue(base, 6, "CLOSED", Some("NOT_PLANNED")),
                    Some("Done"),
                    Some("BASE/o/r/pull/6"),
                ),
                node(
                    base,
                    "g",
                    issue(base, 7, "OPEN", None),
                    Some("Done"),
                    Some("BASE/o/r/pull/7/"),
                ),
            ],
            vec![
                node(
                    base,
                    "h",
                    json!({"__typename": "DraftIssue", "title": "A draft"}),
                    Some("Todo"),
                    None,
                ),
                archived,
                node(
                    base,
                    "j",
                    issue(base, 10, "OPEN", None),
                    Some("In Progress"),
                    Some("see the PR"),
                ),
                node(base, "k", Value::Null, Some("Todo"), None),
                node(
                    base,
                    "l",
                    pr(12, "jmarrero-forge/homegit", "OPEN", false),
                    Some("Draft"),
                    None,
                ),
                node(base, "m", issue(base, 13, "OPEN", None), None, None),
                // An item GitHub won't show this token at all.
                Value::Null,
            ],
        ]
    }
    async fn graphql(headers: HeaderMap, Json(body): Json<Value>) -> Response {
        let base = base(&headers);
        let query = body["query"].as_str().unwrap();
        let variables = &body["variables"];
        if query.contains("nodes(ids") {
            // An archived item, one removed, and one of another board.
            let ids = variables["ids"].as_array().unwrap();
            let nodes: Vec<Value> = ids
                .iter()
                .map(|id| match id.as_str().unwrap() {
                    "PVTI_z" => {
                        let mut n = node(
                            &base,
                            "z",
                            issue(&base, 20, "CLOSED", Some("COMPLETED")),
                            Some("Done"),
                            None,
                        );
                        n["isArchived"] = true.into();
                        n
                    }
                    "PVTI_x" => {
                        let mut n = node(
                            &base,
                            "x",
                            issue(&base, 21, "OPEN", None),
                            Some("Todo"),
                            None,
                        );
                        n["project"]["url"] = format!("{base}orgs/elsewhere/projects/2").into();
                        n
                    }
                    _ => Value::Null,
                })
                .collect();
            let errors: Vec<Value> = ids
                .iter()
                .enumerate()
                .filter(|(_, id)| !["PVTI_z", "PVTI_x"].contains(&id.as_str().unwrap()))
                .map(|(n, _)| json!({"type": NOT_FOUND, "path": ["nodes", n]}))
                .collect();
            return Json(json!({"data": {"nodes": nodes}, "errors": errors})).into_response();
        }
        assert!(query.contains("organization(login: $owner)"), "{query}");
        assert_eq!(
            (&variables["owner"], &variables["number"]),
            (&json!("jmarrero-forge"), &json!(1))
        );
        let [first, second] = board(&base);
        let (nodes, next) = match variables["after"].as_str() {
            None => (first, true),
            Some("cursor-1") => (second, false),
            Some(other) => panic!("unexpected cursor {other}"),
        };
        Json(json!({"data": {"owner": {"projectV2": {
            "url": board_url(&base),
            "items": {"pageInfo": {"hasNextPage": next, "endCursor": "cursor-1"}, "nodes": nodes},
        }}}}))
        .into_response()
    }
    /// A pull request as REST search finds it.
    fn found_pr(base: &str, repo: &str, n: u64, draft: bool) -> Value {
        json!({
            "id": 5000 + n, "number": n, "title": format!("Fork change {n}"),
            "html_url": format!("{base}{repo}/pull/{n}"),
            "repository_url": format!("{base}api/v3/repos/{repo}"),
            "state": "open", "draft": draft, "updated_at": "2026-10-12T00:00:00Z",
            "pull_request": {"merged_at": null},
            "user": {"login": "jmarrero-bot", "id": 7},
        })
    }
    async fn search(headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
        let base = base(&headers);
        assert_eq!(
            q["q"],
            "is:pr author:jmarrero-bot org:jmarrero-forge is:open archived:false"
        );
        // By another account that took the bot's login: not the bot's.
        let mut squatter = found_pr(&base, "jmarrero-forge/r", 19, false);
        squatter["user"]["id"] = 666.into();
        let items = [
            found_pr(&base, "jmarrero-forge/r", 11, true),
            // On the board itself, as item l.
            found_pr(&base, "jmarrero-forge/homegit", 12, false),
            found_pr(&base, "jmarrero-forge/r", 14, false),
            squatter,
        ];
        Json(json!({"total_count": 4, "incomplete_results": false, "items": items})).into_response()
    }
    fn router() -> Router {
        let pull = |merged: bool| {
            get(move || async move {
                Json(json!({"merged_at": merged.then_some("2026-10-11T00:00:00Z")}))
            })
        };
        Router::new()
            .route(
                "/api/v3/user",
                get(|| async { Json(json!({"id": 7, "login": "jmarrero-bot"})) }),
            )
            .route(
                "/api/v3/users/jmarrero-bot",
                get(|| async { Json(json!({"id": 7, "login": "jmarrero-bot", "type": "User"})) }),
            )
            .route("/api/graphql", post(graphql))
            .route("/api/v3/search/issues", get(search))
            .route("/api/v3/repos/o/r/pulls/5", pull(true))
            .route("/api/v3/repos/o/r/pulls/6", pull(false))
            // Closed since, without merging.
            .route(
                "/api/v3/repos/jmarrero-forge/r/issues/15",
                get(|headers: HeaderMap| async move {
                    let mut pr = found_pr(&base(&headers), "jmarrero-forge/r", 15, false);
                    pr["state"] = "closed".into();
                    Json(pr)
                }),
            )
            .route(
                "/api/v3/repos/jmarrero-forge/r/issues/16",
                get(|headers: HeaderMap| async move {
                    let mut pr = found_pr(&base(&headers), "jmarrero-forge/r", 16, false);
                    pr["state"] = "closed".into();
                    Json(pr)
                }),
            )
            .route(
                "/api/v3/repos/jmarrero-forge/r/issues/18",
                get(|headers: HeaderMap| async move {
                    let mut pr = found_pr(&base(&headers), "jmarrero-forge/r", 18, false);
                    pr["state"] = "locked".into();
                    Json(pr)
                }),
            )
            .route(
                "/api/v3/repos/jmarrero-forge/r/issues/17",
                get(|headers: HeaderMap| async move {
                    Json(found_pr(&base(&headers), "jmarrero-forge/r", 17, false))
                }),
            )
    }
    fn connection() -> Connection {
        Connection {
            provider: "bot".into(),
            account_id: "7".into(),
            account_name: "jmarrero-bot".into(),
            bot: Some(config()),
            ..test_connection()
        }
    }

    #[tokio::test]
    async fn board_items_and_fork_prs_become_items() {
        use Outcome::*;
        let (target, task) = crate::connectors::fixture(router(), "test-only-token").await;
        let bot = Bot::new(Arc::default(), config()).unwrap();
        let items = connector::sync(&bot, &target, &connection(), &[]).await;
        task.abort();
        let requests = bot.requests().unwrap();
        let items = items.unwrap();
        let by_key = |key: &str| {
            items
                .iter()
                .find(|i| i.key == key)
                .unwrap_or_else(|| panic!("{key} in {items:#?}"))
        };
        // (key, native status, outcome, attention, waits on the person)
        for (key, status, outcome, action, waits) in [
            ("o/r#1", "Needs human", Open, "next", true),
            ("o/r#2", "Draft", Open, "review", true),
            ("o/r#3", "Done", Delivered, "", false),
            ("o/r#4", "Done", Delivered, "", false),
            // Done, and its Branch merged; or not; or unreadable.
            ("o/r#5", "Done", Delivered, "", false),
            ("o/r#6", "Done", ClosedWithoutDelivery, "", false),
            ("o/r#7", "Done", Unknown, "", false),
            ("o/r#10", "In Progress", Open, "waiting", false),
            ("jmarrero-forge/homegit#12", "Draft", Open, "review", true),
            ("o/r#13", "No status", Open, "waiting", false),
            // The fork pull requests.
            ("jmarrero-forge/r#11", "draft", Open, "review", false),
            ("jmarrero-forge/r#14", "open", Open, "review", false),
        ] {
            let i = by_key(key);
            assert_eq!(
                (i.status.as_str(), i.outcome, i.action.as_str()),
                (status, outcome, action),
                "{key}: {}",
                i.outcome_reason
            );
            let reasons = if waits {
                vec![Reason::BotWaiting]
            } else {
                vec![]
            };
            assert_eq!(i.reasons, reasons, "{key}");
            assert_eq!(i.provider, "bot");
        }
        // No draft issue, archived or unreadable item, and the fork pull
        // request on the board only once.
        assert_eq!(items.len(), 12, "{items:#?}");
        // The board's fields, and the fork pull request each Branch links.
        let b = by_key("o/r#2").bot.clone().unwrap();
        assert_eq!(
            (b.kind, b.priority.as_deref(), b.why.as_deref(), b.news),
            (BotKind::BoardItem, Some("P1"), Some("Why b"), None)
        );
        let fork = by_key("jmarrero-forge/r#11").bot.clone().unwrap();
        assert_eq!(fork.kind, BotKind::ForkPr);
        assert_eq!(
            fork.board_item.as_deref(),
            Some(by_key("o/r#2").url.as_str())
        );
        assert_eq!(
            b.branch.as_deref(),
            Some(by_key("jmarrero-forge/r#11").url.as_str())
        );
        assert!(by_key("jmarrero-forge/r#14")
            .bot
            .as_ref()
            .unwrap()
            .board_item
            .is_none());
        // A Branch that isn't a link is left out.
        assert!(by_key("o/r#10").bot.as_ref().unwrap().branch.is_none());
        // Only reads went out: the account, the search, and the linked pull
        // requests of the three Done items that need them; and two pages of
        // the board, as queries.
        let expected = Requests {
            sent: [
                ("GET".to_string(), 5),
                ("POST /graphql (query)".to_string(), 2),
            ]
            .into(),
            not_modified: 0,
            refused: 0,
        };
        assert_eq!(requests, expected);
        // Needs human and Draft wait on the person, in the Mine view.
        let snapshot = Snapshot {
            connections: vec![Connection {
                base_url: target.base_url.to_string(),
                ..connection()
            }],
            items: items.clone(),
            efforts: vec![],
        };
        let mine = crate::mine::mine(&snapshot, &[]);
        let mut keys: Vec<_> = mine.mine.iter().map(|r| r.key.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["jmarrero-forge/homegit#12", "o/r#1", "o/r#2"]);
        assert!(mine
            .mine
            .iter()
            .all(|r| r.waits_on[0].reasons == [Reason::BotWaiting]));
        assert_eq!(Reason::BotWaiting.label(), "bot waiting on you");
        // Asking the bot about each of them is only a link to build: the
        // views take no client, and nothing more was sent.
        let views = crate::bot::views(&snapshot, &[]);
        let [view] = &views[..] else {
            panic!("{views:?}")
        };
        assert_eq!(view.asks.len(), items.len());
        let ask = &view.asks[&by_key("o/r#1").id];
        assert!(
            ask.issue_url
                .contains("/jmarrero-forge/tracker/issues/new?"),
            "{ask:?}"
        );
        assert_eq!(
            ask.comment_url.as_deref(),
            Some(by_key("o/r#1").url.as_str())
        );
        assert_eq!(bot.requests().unwrap(), requests);
    }

    #[tokio::test]
    async fn items_that_leave_the_board_are_read_again_by_id() {
        let (target, task) = crate::connectors::fixture(router(), "test-only-token").await;
        let bot = Bot::new(Arc::default(), config()).unwrap();
        let tracked = |remote_id: &str, key: &str| Tracked {
            remote_id: remote_id.into(),
            key: key.into(),
            bot: None,
        };
        let linked = |remote_id: &str, key: &str, board_item: &str| Tracked {
            bot: Some(BotFields {
                kind: BotKind::ForkPr,
                board_item: Some(board_item.into()),
                ..BotFields::default()
            }),
            ..tracked(remote_id, key)
        };
        let answers = bot
            .refresh_all(
                &target,
                &[
                    tracked("PVTI_z", "o/r#20"),
                    tracked("PVTI_y", "o/r#21"),
                    tracked("PVTI_x", "o/r#22"),
                    tracked("5015", "jmarrero-forge/r#15"),
                    linked(
                        "5016",
                        "jmarrero-forge/r#16",
                        "https://github.com/o/r/issues/2",
                    ),
                    linked(
                        "5017",
                        "jmarrero-forge/r#17",
                        "https://github.com/o/r/issues/3",
                    ),
                    linked(
                        "5018",
                        "jmarrero-forge/r#18",
                        "https://github.com/o/r/issues/4",
                    ),
                    linked(
                        "5019",
                        "jmarrero-forge/r#19",
                        "https://github.com/o/r/issues/5",
                    ),
                ],
            )
            .await
            .unwrap();
        task.abort();
        let found: Vec<_> = answers
            .iter()
            .map(|a| (a.remote_id.as_str(), a.item.as_ref().map(|i| i.outcome)))
            .collect();
        assert_eq!(
            found,
            [
                // Archived since, and its issue completed.
                ("PVTI_z", Some(Outcome::Delivered)),
                // Removed: unknown, never done.
                ("PVTI_y", None),
                // Another board's.
                ("PVTI_x", None),
                // A fork pull request closed without merging, which no
                // board item links: a person decides.
                ("5015", Some(Outcome::ClosedWithoutDelivery)),
                // One a board item links, closed as it is once promoted
                // upstream: the board item carries what became of it.
                ("5016", None),
                // Still open: kept, with its link.
                ("5017", Some(Outcome::Open)),
                // Linked, but in a state GitHub doesn't say is closed or
                // merged: kept, unknown, never removed.
                ("5018", Some(Outcome::Unknown)),
                // Linked, but gone (404): an answer that it can't be read,
                // which the sync keeps as unknown, never removed.
                ("5019", None),
            ]
        );
        assert!(!answers[6].dropped && !answers[7].dropped);
        assert_eq!(
            answers[7].error.as_ref().map(|e| e.kind),
            Some(ErrorKind::Inaccessible)
        );
        assert!(answers[4].dropped && answers[4].error.is_none());
        assert!(answers.iter().filter(|a| a.dropped).count() == 1);
        let kept = answers[5].item.as_ref().unwrap().bot.clone().unwrap();
        assert_eq!(
            (kept.kind, kept.board_item.as_deref()),
            (BotKind::ForkPr, Some("https://github.com/o/r/issues/3"))
        );
        assert_eq!(
            answers[3].item.as_ref().unwrap().bot.as_ref().unwrap().kind,
            BotKind::ForkPr
        );
        let requests = bot.requests().unwrap();
        assert_eq!(requests.sent["POST /graphql (query)"], 1);
        assert_eq!(requests.refused, 0);
    }

    #[tokio::test]
    async fn a_sync_removes_only_a_linked_fork_pr_confirmed_closed() {
        let (target, task) = crate::connectors::fixture(router(), "test-only-token").await;
        let bot = Bot::new(Arc::default(), config()).unwrap();
        let c = connection();
        // Fork pull requests tracked before, each linked from the board,
        // and none open any more: closed (16), in a state GitHub doesn't
        // say is closed or merged (18), and gone (19).
        let stored = |n: u64| {
            let mut remote = github_item(
                &found_pr("https://github.com/", "jmarrero-forge/r", n, false),
                "review",
            )
            .unwrap();
            remote.bot = Some(BotFields {
                kind: BotKind::ForkPr,
                board_item: Some(format!("https://github.com/o/r/issues/{n}")),
                ..BotFields::default()
            });
            connector::to_item(&c, "bot", remote).unwrap()
        };
        let old = [stored(16), stored(18), stored(19)];
        let at = Timestamp::parse("2026-10-12T04:00:00Z").unwrap();
        let (synced, _) = connector::sync_and_watch(&bot, &target, &c, &old, None, at)
            .await
            .unwrap();
        task.abort();
        assert_eq!(synced.dropped, [old[0].id.clone()]);
        for kept in &old[1..] {
            let item = synced
                .items
                .iter()
                .find(|i| i.id == kept.id)
                .unwrap_or_else(|| panic!("{} was not kept", kept.key));
            assert_eq!(item.outcome, Outcome::Unknown, "{}", kept.key);
        }
    }

    #[tokio::test]
    async fn the_board_is_counted_with_what_this_token_cant_read() {
        let (target, task) = crate::connectors::fixture(router(), "test-only-token").await;
        let bot = Bot::new(Arc::default(), config()).unwrap();
        assert_eq!(bot.board(), None);
        connector::sync(&bot, &target, &connection(), &[])
            .await
            .unwrap();
        task.abort();
        let summary = bot.board().unwrap();
        let statuses: Vec<_> = summary
            .statuses
            .iter()
            .map(|(s, n)| (s.as_str(), *n))
            .collect();
        // Listed items only: no draft, nothing archived. The item without
        // content and the one GitHub sends as null can't be read, and are
        // counted, so the board is never shown as all there is.
        assert_eq!(
            statuses,
            [
                ("Done", 5),
                ("Draft", 2),
                ("In Progress", 1),
                ("Needs human", 1),
                ("No status", 1)
            ]
        );
        assert_eq!((summary.fork_prs, summary.unreadable), (2, 2));
    }

    #[tokio::test]
    async fn the_bot_is_matched_by_its_account_id() {
        let (target, task) = crate::connectors::fixture(router(), "test-only-token").await;
        // Settings kept before the ID was: one read looks it up, and the
        // sync goes by it.
        let unsettled = BotConfig {
            bot_id: None,
            ..config()
        };
        let bot = Bot::new(Arc::default(), unsettled.clone()).unwrap();
        let items = connector::sync(&bot, &target, &connection(), &[])
            .await
            .unwrap();
        assert_eq!(items.len(), 12, "{items:#?}");
        assert_eq!(bot.bot_id(), Some(7));
        assert_eq!(bot.requests().unwrap().sent["GET"], 6);
        // Connecting and verifying look it up, to keep it.
        let settled = connector::settle_bot(&bot, &target, unsettled)
            .await
            .unwrap();
        assert_eq!(settled.bot_id, Some(7));
        // A login that names another account than the one kept now is
        // refused, and so is one that names none.
        let other = BotConfig {
            bot_id: Some(8),
            ..config()
        };
        let error = connector::settle_bot(&bot, &target, other)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Auth, "{error}");
        assert!(error.message.contains("account 8"), "{error}");
        let gone = BotConfig {
            bot_login: "gone-bot".into(),
            bot_id: None,
            ..config()
        };
        let looking = Bot::new(Arc::default(), gone.clone()).unwrap();
        let error = connector::settle_bot(&looking, &target, gone)
            .await
            .unwrap_err();
        task.abort();
        assert!(error.message.contains("no account gone-bot"), "{error}");
    }

    #[tokio::test]
    async fn a_board_the_token_cant_read_fails_the_sync() {
        for answer in [
            json!({"data": {"owner": null}, "errors": [{"type": NOT_FOUND}]}),
            json!({"data": {"owner": {"projectV2": null}}}),
        ] {
            let router = Router::new()
                .route(
                    "/api/v3/user",
                    get(|| async { Json(json!({"id": 7, "login": "jmarrero-bot"})) }),
                )
                .route(
                    "/api/graphql",
                    post(move || {
                        let answer = answer.clone();
                        async move { Json(answer) }
                    }),
                );
            let (target, task) = crate::connectors::fixture(router, "test-only-token").await;
            let bot = Bot::new(Arc::default(), config()).unwrap();
            let error = connector::sync(&bot, &target, &connection(), &[])
                .await
                .unwrap_err();
            task.abort();
            assert_eq!(error.kind, ErrorKind::Auth, "{error}");
            assert!(
                error.message.contains("orgs/jmarrero-forge/projects/1"),
                "{error}"
            );
        }
        // A board that keeps paging fails rather than show part of itself.
        let router = Router::new().route(
            "/api/graphql",
            post(|headers: HeaderMap| async move {
                let base = base(&headers);
                Json(json!({"data": {"owner": {"projectV2": {
                    "url": board_url(&base),
                    "items": {"pageInfo": {"hasNextPage": true, "endCursor": "more"}, "nodes": []},
                }}}}))
            }),
        );
        let (target, task) = crate::connectors::fixture(router, "test-only-token").await;
        let bot = Bot::new(Arc::default(), config()).unwrap();
        let error = bot.read_board(&target).await.err().unwrap();
        task.abort();
        assert!(
            error.message.contains("more than one sync reads"),
            "{error}"
        );
        assert_eq!(bot.requests().unwrap().sent["POST /graphql (query)"], 10);
    }

    #[tokio::test]
    async fn the_heartbeat_is_read_as_agent_runs_apart_from_items() {
        let comments = |headers: HeaderMap| async move {
            let base = base(&headers);
            let heartbeat = json!({
                "schema": "bot-heartbeat/v1", "updated_at": "2026-10-12T03:58:44Z",
                "coordinator": {"session": "s1", "loop_state": "working"},
                "workers": [
                    {"name": "wtp-slice8", "item_url": format!("{base}jmarrero-bot/work-tracker-poc/issues/9"),
                     "started_at": "2026-10-12T03:28:44Z", "status": "working"},
                    // Not on this instance.
                    {"name": "elsewhere", "item_url": "https://github.com/o/r/issues/1",
                     "started_at": "2026-10-12T03:28:44Z", "status": "working"},
                ],
            });
            let body = format!(
                "{}\nCoordinator.\n\n```json\n{heartbeat}\n```\n",
                crate::bot::HEARTBEAT_MARKER
            );
            Json(json!([
                {"user": {"login": "someone", "id": 9}, "body": "Hi"},
                {"user": {"login": "jmarrero-bot", "id": 7}, "body": body,
                 "html_url": format!("{base}jmarrero-forge/tracker/issues/1#issuecomment-1")},
            ]))
        };
        let with_comments = router().route(
            "/api/v3/repos/jmarrero-forge/tracker/issues/1/comments",
            get(comments),
        );
        let (target, task) = crate::connectors::fixture(with_comments, "test-only-token").await;
        let bot = Bot::new(Arc::default(), config()).unwrap();
        let at = Timestamp::parse("2026-10-12T04:00:00Z").unwrap();
        let c = connection();
        let runs = connector::agents(&bot, &target, &c, None, at)
            .await
            .unwrap();
        let heartbeat = runs.heartbeat.clone().unwrap();
        assert_eq!(
            (heartbeat.loop_state.as_str(), heartbeat.skipped),
            ("working", 1)
        );
        let [run] = &heartbeat.runs[..] else {
            panic!("{heartbeat:?}")
        };
        assert_eq!(
            (run.name.as_str(), run.item_key.as_str()),
            ("wtp-slice8", "jmarrero-bot/work-tracker-poc#9")
        );
        assert_eq!((runs.last_success, runs.error.as_deref()), (Some(at), None));
        task.abort();
        // A heartbeat that can't be read keeps what was read before, as of
        // when, with the error; the board's sync is untouched by it.
        let (target, task) = crate::connectors::fixture(router(), "test-only-token").await;
        let later = Timestamp::parse("2026-10-12T04:05:00Z").unwrap();
        let kept = connector::agents(&bot, &target, &c, Some(&runs), later)
            .await
            .unwrap();
        task.abort();
        assert_eq!(kept.heartbeat, runs.heartbeat);
        assert_eq!(kept.last_success, Some(at));
        let error = kept.error.unwrap();
        assert!(
            error.contains("heartbeat issue jmarrero-forge/tracker#1"),
            "{error}"
        );
        // Only GETs of the comments went out.
        let requests = bot.requests().unwrap();
        assert_eq!((requests.sent["GET"], requests.refused), (2, 0));
        assert!(!requests.sent.contains_key("POST /graphql (query)"));
    }

    #[tokio::test]
    async fn a_heartbeat_past_the_comments_read_is_an_error_not_none() {
        // Every page full, and none of it a heartbeat.
        let full = || async {
            let comments: Vec<_> = (0..COMMENTS_PAGE)
                .map(|_| json!({"user": {"login": "someone"}, "body": "Hi"}))
                .collect();
            Json(json!(comments))
        };
        let router = Router::new().route(
            "/api/v3/repos/jmarrero-forge/tracker/issues/1/comments",
            get(full),
        );
        let (target, task) = crate::connectors::fixture(router, "test-only-token").await;
        let bot = Bot::new(Arc::default(), config()).unwrap();
        let at = Timestamp::parse("2026-10-12T04:00:00Z").unwrap();
        let error = bot.heartbeat(&target, at).await.unwrap_err();
        task.abort();
        assert!(error.message.contains("first 300 comments"), "{error}");
        assert_eq!(bot.requests().unwrap().sent["GET"], COMMENT_PAGES as u64);
    }

    #[test]
    fn linked_pull_requests_are_on_this_instance() {
        let base = Url::parse("https://github.com/").unwrap();
        for (url, linked) in [
            ("https://github.com/o/r/pull/5", Some(("o/r", 5))),
            ("https://github.com/o/r/pull/5/", Some(("o/r", 5))),
            ("https://github.com/o/r/pull/5/files", None),
            ("https://github.com/o/r/issues/5", None),
            ("https://github.com/o/r/compare/a..b", None),
            ("https://github.com/o/../pull/5", None),
            ("https://github.com/o/r/pull/x", None),
            ("https://github.com/o/r/pull/5?x=1", None),
            ("https://elsewhere.example/o/r/pull/5", None),
            ("http://github.com/o/r/pull/5", None),
        ] {
            let found = pull_request_of(url, &base);
            let found = found.as_ref().map(|(r, n)| (r.as_str(), *n));
            assert_eq!(found, linked, "{url}");
        }
    }
}
