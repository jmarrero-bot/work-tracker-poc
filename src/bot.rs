//! A harness bot as a source of work: where its
//! board, tracker and heartbeat are, and what the bot connector reads
//! besides an item's usual fields. The connector itself is
//! `connectors/github/bot.rs`; like every GitHub connector, it only reads.

use crate::model::{valid_github_login, valid_github_name, Item, Snapshot, Timestamp};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use url::Url;

/// Whose Projects board it is, as its address says: `orgs/OWNER/projects/N`
/// or `users/OWNER/projects/N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerType {
    Orgs,
    Users,
}

impl OwnerType {
    /// How the board's address names it.
    pub fn path(self) -> &'static str {
        match self {
            Self::Orgs => "orgs",
            Self::Users => "users",
        }
    }
    /// The GraphQL field that reads such an owner.
    pub fn graphql_field(self) -> &'static str {
        match self {
            Self::Orgs => "organization",
            Self::Users => "user",
        }
    }
}

/// Where a bot's work is. Nothing is assumed: each is given when the bot is
/// connected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotConfig {
    /// The Projects (v2) board the bot works from.
    pub board_owner_type: OwnerType,
    pub board_owner: String,
    pub board_number: u32,
    /// The repository, as `OWNER/REPO`, where work is asked of the bot,
    /// and whose issue `heartbeat_issue` holds its heartbeat.
    pub tracker: String,
    pub heartbeat_issue: u64,
    /// The bot's login: the author of its fork pull requests and of its
    /// heartbeat.
    pub bot_login: String,
    /// The bot's numeric account ID, which its login named when it was
    /// connected or verified, looked up with one read. Its heartbeat and
    /// fork pull requests are matched by it, since a login can be renamed,
    /// and then taken by someone else. None for a connection made before
    /// it was kept, until its next sync looks it up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<u64>,
    /// The organization that holds the bot's forks.
    pub forge_org: String,
}

impl BotConfig {
    /// Fail unless every part can be what it names on GitHub, so none can
    /// change the paths, queries and searches it goes into.
    pub fn check(&self) -> Result<()> {
        if !valid_github_login(&self.board_owner) || self.board_number == 0 {
            bail!("Enter the bot's board as orgs/OWNER/NUMBER or users/OWNER/NUMBER")
        }
        if self.tracker_parts().is_none() {
            bail!("Enter the bot's tracker repository as OWNER/REPO")
        }
        if self.heartbeat_issue == 0 {
            bail!("Enter the number of the tracker's heartbeat issue")
        }
        if !valid_github_login(&self.bot_login) {
            bail!("Enter the bot's GitHub login")
        }
        if self.bot_id == Some(0) {
            bail!("A GitHub account ID is never 0")
        }
        if !valid_github_login(&self.forge_org) {
            bail!("Enter the organization that holds the bot's forks")
        }
        Ok(())
    }
    /// The tracker's owner and repository.
    pub fn tracker_parts(&self) -> Option<(&str, &str)> {
        tracker_parts(&self.tracker)
    }
    /// The board's address on the instance whose web address is `base`.
    pub fn board_url(&self, base: &Url) -> Result<Url> {
        Ok(base.join(&format!(
            "{}/{}/projects/{}",
            self.board_owner_type.path(),
            self.board_owner,
            self.board_number
        ))?)
    }
}

/// A tracker's owner and repository, if `tracker` is `OWNER/REPO`, each
/// what it can be on GitHub.
pub fn tracker_parts(tracker: &str) -> Option<(&str, &str)> {
    let (owner, repo) = tracker.split_once('/')?;
    (valid_github_login(owner) && valid_github_name(repo)).then_some((owner, repo))
}

/// Where a bot's work is, as a person gives it when connecting: the board
/// as its address or `bot-operator` writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotInput {
    pub board: String,
    pub tracker: String,
    pub heartbeat_issue: u64,
    pub bot_login: String,
    pub forge_org: String,
}

impl BotInput {
    /// The settings it gives, once checked.
    pub fn config(self) -> Result<BotConfig> {
        let (board_owner_type, board_owner, board_number) = parse_board(&self.board)?;
        let config = BotConfig {
            board_owner_type,
            board_owner,
            board_number,
            tracker: self.tracker.trim().to_string(),
            heartbeat_issue: self.heartbeat_issue,
            bot_login: self.bot_login.trim().to_string(),
            // Looked up with the connection's token, once it is verified.
            bot_id: None,
            forge_org: self.forge_org.trim().to_string(),
        };
        config.check()?;
        Ok(config)
    }
}

/// A board as `bot-operator` and the board's own address write it:
/// `orgs/OWNER/N` or `users/OWNER/N`, with `projects/` before the number
/// or not, and the instance's address before it or not.
pub fn parse_board(text: &str) -> Result<(OwnerType, String, u32)> {
    let path = match Url::parse(text.trim()) {
        Ok(url) if matches!(url.scheme(), "https" | "http") => url.path().to_string(),
        _ => text.trim().to_string(),
    };
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let (kind, owner, number) = match parts[..] {
        [kind, owner, "projects", number] | [kind, owner, number] => (kind, owner, number),
        _ => bail!("Enter the bot's board as orgs/OWNER/NUMBER or users/OWNER/NUMBER"),
    };
    let kind = match kind {
        "orgs" => OwnerType::Orgs,
        "users" => OwnerType::Users,
        _ => bail!("A board belongs to orgs/OWNER or users/OWNER, not {kind}/{owner}"),
    };
    let number: u32 = number
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .with_context(|| format!("'{number}' is not a board number"))?;
    if !valid_github_login(owner) {
        bail!("'{owner}' is not a GitHub organization or user")
    }
    Ok((kind, owner.to_string(), number))
}

/// What the last listing of a bot's board found: its items by board
/// status, the bot's open fork pull requests, and the items this token
/// can't read. Archived and removed items aren't on the board, and aren't
/// counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSummary {
    pub statuses: BTreeMap<String, usize>,
    pub fork_prs: usize,
    /// Board items whose issue or pull request this token can't read. While
    /// there are any, the board shown is not all of it.
    pub unreadable: usize,
}

/// What a bot item is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotKind {
    /// An issue or pull request on the bot's board.
    #[default]
    BoardItem,
    /// One of the bot's open pull requests in its forge organization, which
    /// waits for review there.
    ForkPr,
}

/// What the bot connector reads besides an item's usual fields: a board
/// item's fields, or the board item that links a fork pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotFields {
    pub kind: BotKind,
    #[serde(default)]
    pub priority: Option<String>,
    /// Why the item is on the board, and its current result, as the bot
    /// writes it.
    #[serde(default)]
    pub why: Option<String>,
    /// What last happened to it.
    #[serde(default)]
    pub news: Option<String>,
    /// The Branch field, when it is a link: the fork pull request, the
    /// upstream one, or a comparison.
    #[serde(default)]
    pub branch: Option<String>,
    /// For a fork pull request: the board item whose Branch links it.
    #[serde(default)]
    pub board_item: Option<String>,
}

/// The first line of the bot's heartbeat comment, as `bot-heartbeat`
/// writes it; the JSON follows in a ```json fence.
pub const HEARTBEAT_MARKER: &str = "<!-- bot-heartbeat v1 -->";
/// What the heartbeat's JSON says it is.
const HEARTBEAT_SCHEMA: &str = "bot-heartbeat/v1";
const JSON_FENCE: &str = "```json\n";
const FENCE_END: &str = "\n```";
/// Longest heartbeat comment read, in bytes. GitHub takes up to 65536
/// characters; `bot-heartbeat` writes at most 32 KiB.
pub const MAX_COMMENT: usize = 64 * 1024;
/// Longest heartbeat JSON parsed, in bytes.
const MAX_JSON: usize = 32 * 1024;
/// Most workers listed, as `bot-heartbeat` allows.
pub const MAX_WORKERS: usize = 32;
/// How far a time may be ahead of this machine's clock.
const MAX_SKEW: Duration = Duration::from_secs(5 * 60);
/// The longest the coordinator says it sleeps, after `updated_at`.
const MAX_SLEEP: Duration = Duration::from_secs(2 * 60 * 60);
/// A heartbeat older than this is stale, unless the wake time it gave,
/// plus [`WAKE_GRACE`], is still ahead.
const HEARTBEAT_STALE: Duration = Duration::from_secs(15 * 60);
const WAKE_GRACE: Duration = Duration::from_secs(5 * 60);
/// The loop state of a coordinator that said it stopped.
const STOPPED: &str = "stopped";

/// The coordinator's heartbeat: its loop state, and the workers it runs on
/// its own machine, as agent runs. Read from a public comment, so every
/// part of it was checked, and it holds names and links only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub updated_at: Timestamp,
    /// The coordinator's session.
    pub session: String,
    /// Such as polling, working, sleeping or stopped.
    pub loop_state: String,
    /// When its loop wakes next, if it said.
    pub next_wake_at: Option<Timestamp>,
    pub runs: Vec<AgentRun>,
    /// Workers left out, as malformed or beyond the most it lists.
    pub skipped: usize,
    /// The comment it was read from.
    pub comment_url: Option<String>,
    /// Too old to trust: the coordinator may have stopped. Set when shown.
    #[serde(default)]
    pub stale: bool,
}

/// One worker the coordinator runs, as an agent run. Agent runs are kept
/// apart from items: a run that finished delivered nothing by finishing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRun {
    pub name: String,
    /// The board item it works on, an issue or pull request on the
    /// instance, and its reference, `OWNER/REPO#N`.
    pub item_url: String,
    pub item_key: String,
    /// Such as starting, working, testing, reviewing, landing or waiting.
    pub status: String,
    pub started_at: Timestamp,
    pub devspace: Option<String>,
}

/// What a bot connection last read of its heartbeat. It is stored apart
/// from items, in its own table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRuns {
    pub connection_id: String,
    /// None when the bot publishes no heartbeat.
    pub heartbeat: Option<Heartbeat>,
    /// Why the last sync couldn't read it, which then is what an earlier one
    /// read.
    #[serde(default)]
    pub error: Option<String>,
    /// When the sync that read it started: how fresh it is.
    #[serde(default)]
    pub last_success: Option<Timestamp>,
    /// Whether that was too long ago for it to be current. Set when shown.
    #[serde(default)]
    pub stale: bool,
    /// What the same sync's listing of its board found, as fresh as the
    /// connection's last sync.
    #[serde(default)]
    pub board: Option<BoardSummary>,
}

/// Whether `heartbeat` is too old to trust at `now`: older than
/// [`HEARTBEAT_STALE`], and past the wake time it gave, plus a grace, if
/// any. A coordinator that said it stopped isn't stale, just stopped.
pub fn heartbeat_stale(heartbeat: &Heartbeat, now: Timestamp) -> bool {
    if heartbeat.loop_state == STOPPED || now.since(heartbeat.updated_at) <= HEARTBEAT_STALE {
        return false;
    }
    heartbeat
        .next_wake_at
        .is_none_or(|wake| now > wake.after(WAKE_GRACE))
}

/// Whether `text` is 1 to `max` characters, the first one of `first` and
/// the others of `rest`.
fn word(text: &str, max: usize, first: fn(char) -> bool, rest: fn(char) -> bool) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(first) && text.len() <= max && chars.all(rest)
}
/// A worker or session name: 1-64 of `[A-Za-z0-9_.-]`, not starting with
/// punctuation.
pub fn valid_name(text: &str) -> bool {
    word(
        text,
        64,
        |c| c.is_ascii_alphanumeric(),
        |c| c.is_ascii_alphanumeric() || "_.-".contains(c),
    )
}
/// A loop state or worker status: `bot-heartbeat`'s words, or newer ones,
/// shown as they are.
pub fn valid_word(text: &str) -> bool {
    word(
        text,
        24,
        |c| c.is_ascii_lowercase(),
        |c| c.is_ascii_lowercase() || c == '-',
    )
}
/// A `bot-devspace` name.
pub fn valid_devspace(text: &str) -> bool {
    word(
        text,
        63,
        |c| c.is_ascii_lowercase() || c.is_ascii_digit(),
        |c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c),
    )
}
/// A UTC time as `bot-heartbeat` writes it, to the second; fractions of a
/// second are dropped.
fn time(v: &Value) -> Option<Timestamp> {
    let text = v.as_str()?;
    let whole = match text.split_once('.') {
        Some((seconds, fraction)) => {
            let digits = fraction.strip_suffix('Z')?;
            if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            format!("{seconds}Z")
        }
        None => text.to_string(),
    };
    Timestamp::parse(&whole)
}

/// `url`, if it is an issue or pull request on the instance whose web
/// address is `base`, and its reference, `OWNER/REPO#N`.
pub fn item_on(url: &str, base: &Url) -> Option<(String, String)> {
    let parsed = Url::parse(url).ok()?;
    if parsed.origin() != base.origin()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let path = parsed.path().strip_prefix(base.path())?;
    let parts: Vec<&str> = path.split('/').collect();
    match parts[..] {
        [owner, repo, "issues" | "pull", number]
            if valid_github_login(owner)
                && valid_github_name(repo)
                && (1..=10).contains(&number.len())
                && number.bytes().all(|b| b.is_ascii_digit())
                && !number.starts_with('0') =>
        {
            Some((parsed.to_string(), format!("{owner}/{repo}#{number}")))
        }
        _ => None,
    }
}

/// `url`, if it is a link to a comment, or any page, on the instance whose
/// web address is `base`, without credentials.
pub fn page_on(url: &str, base: &Url) -> Option<String> {
    Url::parse(url)
        .ok()
        .filter(|u| {
            u.origin() == base.origin() && u.username().is_empty() && u.password().is_none()
        })
        .map(String::from)
}

/// The heartbeat in the body of the bot's comment, read at `now` from the
/// instance whose web address is `base`. The comment is untrusted: it is
/// bounded in size, its JSON must be what `bot-heartbeat` writes, its
/// times can't be ahead of this machine's clock by more than a few
/// minutes, and every link must be an issue or pull request on the
/// instance. A worker that isn't well formed is left out, and counted.
pub fn parse_heartbeat(body: &str, base: &Url, now: Timestamp) -> Result<Heartbeat, String> {
    if body.len() > MAX_COMMENT {
        return Err(format!("the heartbeat comment is over {MAX_COMMENT} bytes"));
    }
    if !body.starts_with(HEARTBEAT_MARKER) {
        return Err("the comment isn't a heartbeat".into());
    }
    let start = body
        .find(JSON_FENCE)
        .map(|i| i + JSON_FENCE.len())
        .ok_or("the heartbeat has no JSON")?;
    let end = body[start..]
        .find(FENCE_END)
        .map(|i| start + i)
        .ok_or("the heartbeat's JSON doesn't end")?;
    let json = &body[start..end];
    if json.len() > MAX_JSON {
        return Err(format!("the heartbeat's JSON is over {MAX_JSON} bytes"));
    }
    let v: Value = serde_json::from_str(json).map_err(|_| "the heartbeat's JSON doesn't parse")?;
    if v["schema"] != HEARTBEAT_SCHEMA {
        return Err("the heartbeat is of an unknown schema".into());
    }
    let latest = now.after(MAX_SKEW);
    let updated_at = time(&v["updated_at"]).ok_or("the heartbeat has no valid updated_at")?;
    if updated_at > latest {
        return Err("the heartbeat is from the future".into());
    }
    let coordinator = &v["coordinator"];
    let session = coordinator["session"]
        .as_str()
        .filter(|s| valid_name(s))
        .ok_or("the heartbeat has no valid coordinator session")?;
    let loop_state = coordinator["loop_state"]
        .as_str()
        .filter(|s| valid_word(s))
        .ok_or("the heartbeat has no valid loop state")?;
    // A wake time from before the heartbeat, or too far after it, says
    // nothing.
    let next_wake_at = time(&coordinator["next_wake_at"])
        .filter(|wake| *wake >= updated_at && *wake <= updated_at.after(MAX_SLEEP));
    let workers = v["workers"]
        .as_array()
        .ok_or("the heartbeat lists no workers")?;
    let mut runs: Vec<AgentRun> = Vec::new();
    for w in workers.iter().take(MAX_WORKERS) {
        let run = (|| {
            let name = w["name"].as_str().filter(|n| valid_name(n))?;
            let (item_url, item_key) = item_on(w["item_url"].as_str()?, base)?;
            let started_at = time(&w["started_at"]).filter(|t| *t <= latest)?;
            let status = w["status"].as_str().filter(|s| valid_word(s))?;
            let devspace = w["devspace"].as_str().filter(|d| valid_devspace(d));
            Some(AgentRun {
                name: name.into(),
                item_url,
                item_key,
                status: status.into(),
                started_at,
                devspace: devspace.map(str::to_string),
            })
        })();
        match run {
            Some(run) if !runs.iter().any(|r| r.name == run.name) => runs.push(run),
            _ => (),
        }
    }
    Ok(Heartbeat {
        updated_at,
        session: session.into(),
        loop_state: loop_state.into(),
        next_wake_at,
        skipped: workers.len() - runs.len(),
        runs,
        comment_url: None,
        stale: false,
    })
}

/// The bot's heartbeat among an issue's comments, as the REST API lists
/// them: the first comment by the account whose numeric ID is `bot_id`
/// that is one, whatever its login is now. None if there is none. A
/// comment that is one, but malformed, is an error rather than no
/// heartbeat.
pub fn find_heartbeat(
    comments: &[Value],
    bot_id: u64,
    base: &Url,
    now: Timestamp,
) -> Result<Option<Heartbeat>, String> {
    for c in comments {
        let by_bot = c["user"]["id"].as_u64() == Some(bot_id);
        let body = c["body"].as_str().unwrap_or_default();
        if !by_bot || !body.starts_with(HEARTBEAT_MARKER) {
            continue;
        }
        let mut heartbeat = parse_heartbeat(body, base, now)?;
        heartbeat.comment_url = c["html_url"].as_str().and_then(|u| page_on(u, base));
        return Ok(Some(heartbeat));
    }
    Ok(None)
}

/// The label of an issue that asks the bot for something, which the bot
/// triages.
pub const ASK_LABEL: &str = "needs-triage";
/// Longest title an issue asking about an item is given, in characters.
const MAX_ASK_TITLE: usize = 120;

/// Links that ask the bot for something, for the person to open in their
/// own browser and submit there, as themselves. They are only links:
/// building them sends nothing anywhere, and Workboard holds no
/// credential that could write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    /// A new issue in the bot's tracker, labeled for triage, its body a
    /// template to fill in.
    pub issue_url: String,
    /// For an item: its issue or pull request, to comment on, and how the
    /// comment starts. GitHub takes no comment text in a link, so the page
    /// and the CLI show it to paste.
    pub comment_url: Option<String>,
    pub comment: Option<String>,
}

/// `text` percent-encoded for a URL's query: every byte but the unreserved
/// ones (letters, digits and `-._~`), so nothing in it can end a value,
/// start another, or begin the fragment.
fn encode(text: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// How to ask the bot of `config`, on the instance whose web address is
/// `base`, for something: about `item`, if given, or anything at all.
pub fn ask(config: &BotConfig, base: &Url, item: Option<&Item>) -> Result<Ask> {
    ask_to(&config.tracker, &config.bot_login, base, item)
}

/// How to ask the bot `bot_login`, whose tracker is `tracker`, on the
/// instance whose web address is `base`, for something: about `item`, if
/// given, or anything at all. A companion builds the links for its hub's
/// bot with this, from the tracker and login the hub exported.
pub fn ask_to(tracker: &str, bot_login: &str, base: &Url, item: Option<&Item>) -> Result<Ask> {
    let (owner, repo) = tracker_parts(tracker).context("The bot's tracker isn't a repository")?;
    if !valid_github_login(bot_login) {
        bail!("The bot's login isn't a GitHub login")
    }
    let mut url = base.join(&format!("{owner}/{repo}/issues/new"))?;
    let what = "<!-- Opened from Workboard. Say what the bot should do, and why; it triages this issue. -->";
    let template = "## What to do\n\n\n## Why\n\n";
    let (title, body) = match item {
        Some(i) => {
            let title: String = format!("About {}: {}", i.key, i.title)
                .chars()
                .take(MAX_ASK_TITLE)
                .collect();
            let body = format!(
                "{what}\n\nAbout {} ({}, {})\n\n{template}",
                i.url, i.key, i.status
            );
            (Some(title), body)
        }
        None => (None, format!("{what}\n\n{template}")),
    };
    let mut query = String::new();
    if let Some(title) = &title {
        query = format!("title={}&", encode(title));
    }
    query.push_str(&format!(
        "labels={}&body={}",
        encode(ASK_LABEL),
        encode(&body)
    ));
    url.set_query(Some(&query));
    let comment_url = item.and_then(|i| item_on(&i.url, base)).map(|(url, _)| url);
    Ok(Ask {
        issue_url: url.into(),
        comment: comment_url.as_ref().map(|_| format!("@{bot_login} ")),
        comment_url,
    })
}

/// A bot connection as the page and the CLI show it: where its board is,
/// what it last read of its heartbeat and its board, and how to ask it
/// for something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotView {
    pub connection_id: String,
    pub label: String,
    pub bot_login: String,
    pub board_url: String,
    /// None until a sync read them.
    pub agents: Option<AgentRuns>,
    /// Ask it for anything.
    pub ask: Ask,
    /// Ask it about one of its items, by the item's ID.
    pub asks: BTreeMap<String, Ask>,
}

/// Each bot connection of `snapshot`, with what it read of its heartbeat
/// and its board, from `agents`.
pub fn views(snapshot: &Snapshot, agents: &[AgentRuns]) -> Vec<BotView> {
    snapshot
        .connections
        .iter()
        .filter(|c| !c.from_hub)
        .filter_map(|c| {
            let config = c.bot.as_ref()?;
            let base = Url::parse(&c.base_url).ok()?;
            let mut asks = BTreeMap::new();
            for i in &snapshot.items {
                let mine = i.connection_id.as_deref() == Some(c.id.as_str());
                if i.bot.is_some() && mine {
                    asks.insert(i.id.clone(), ask(config, &base, Some(i)).ok()?);
                }
            }
            Some(BotView {
                connection_id: c.id.clone(),
                label: c.label.clone(),
                bot_login: config.bot_login.clone(),
                board_url: config.board_url(&base).ok()?.into(),
                agents: agents.iter().find(|a| a.connection_id == c.id).cloned(),
                ask: ask(config, &base, None).ok()?,
                asks,
            })
        })
        .collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// An example bot's settings, as `bot-operator --json` describes one.
    pub fn config() -> BotConfig {
        BotConfig {
            board_owner_type: OwnerType::Orgs,
            board_owner: "example-org".into(),
            board_number: 1,
            tracker: "example-org/tracker".into(),
            heartbeat_issue: 1,
            bot_login: "example-bot".into(),
            bot_id: Some(7),
            forge_org: "example-org".into(),
        }
    }

    #[test]
    fn boards_as_people_write_them() {
        let org = Some((OwnerType::Orgs, "example-org".to_string(), 1));
        for (text, parsed) in [
            ("orgs/example-org/1", org.clone()),
            ("orgs/example-org/projects/1", org.clone()),
            (
                "https://github.com/orgs/example-org/projects/1",
                org.clone(),
            ),
            (
                "https://github.com/orgs/example-org/projects/1/views/2",
                None,
            ),
            (
                "users/someone/3",
                Some((OwnerType::Users, "someone".to_string(), 3)),
            ),
            ("teams/example-org/1", None),
            ("orgs/example-org/0", None),
            ("orgs/example-org/x", None),
            ("orgs/a b/1", None),
            ("orgs/../1", None),
            ("example-org/1", None),
        ] {
            assert_eq!(parse_board(text).ok(), parsed, "{text}");
        }
    }

    #[test]
    fn configs_are_checked_whole() {
        config().check().unwrap();
        type Break = fn(&mut BotConfig);
        let cases: [(&str, Break); 8] = [
            ("owner", |c| c.board_owner = "a/b".into()),
            ("number", |c| c.board_number = 0),
            ("tracker", |c| c.tracker = "tracker".into()),
            ("tracker path", |c| c.tracker = "o/../r".into()),
            ("heartbeat", |c| c.heartbeat_issue = 0),
            ("login", |c| c.bot_login = "bot is:open".into()),
            ("account ID", |c| c.bot_id = Some(0)),
            ("forge", |c| c.forge_org = "@me".into()),
        ];
        for (what, break_it) in cases {
            let mut c = config();
            break_it(&mut c);
            assert!(c.check().is_err(), "{what}");
        }
        let base = Url::parse("https://github.com/").unwrap();
        assert_eq!(
            config().board_url(&base).unwrap().as_str(),
            "https://github.com/orgs/example-org/projects/1"
        );
    }

    /// A heartbeat comment as `bot-heartbeat` writes it, around `json`.
    fn comment(json: &str) -> String {
        format!("{HEARTBEAT_MARKER}\nCoordinator `s` is working.\n\n```json\n{json}\n```\n")
    }
    fn github() -> Url {
        Url::parse("https://github.com/").unwrap()
    }
    /// When the heartbeats below are read.
    fn now() -> Timestamp {
        Timestamp::parse("2026-10-12T04:00:00Z").unwrap()
    }
    fn worker(name: &str, url: &str) -> Value {
        serde_json::json!({
            "name": name, "item_url": url, "started_at": "2026-10-12T03:28:44Z",
            "status": "working",
        })
    }
    fn heartbeat(workers: Vec<Value>) -> Value {
        serde_json::json!({
            "schema": "bot-heartbeat/v1",
            "updated_at": "2026-10-12T03:58:44.5Z",
            "coordinator": {"session": "b32c91a0-6ef2", "loop_state": "sleeping",
                            "next_wake_at": "2026-10-12T04:28:44Z"},
            "workers": workers,
        })
    }

    #[test]
    fn heartbeats_are_read_as_agent_runs() {
        let mut first = worker("wtp-slice8", "https://github.com/example-org/app/issues/9");
        first["devspace"] = "wtp-8".into();
        // Newer keys and words are shown as they are, or left out.
        first["engine"] = "claude".into();
        let body = comment(
            &heartbeat(vec![
                first,
                worker("review", "https://github.com/o/r/pull/12"),
            ])
            .to_string(),
        );
        let h = parse_heartbeat(&body, &github(), now()).unwrap();
        assert_eq!(
            (h.session.as_str(), h.loop_state.as_str(), h.skipped),
            ("b32c91a0-6ef2", "sleeping", 0)
        );
        // A fraction of a second is dropped.
        assert_eq!(h.updated_at.to_string(), "2026-10-12T03:58:44Z");
        assert_eq!(h.next_wake_at.unwrap().to_string(), "2026-10-12T04:28:44Z");
        let runs: Vec<_> = h
            .runs
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.item_key.as_str(),
                    r.status.as_str(),
                    r.devspace.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            runs,
            [
                ("wtp-slice8", "example-org/app#9", "working", Some("wtp-8")),
                ("review", "o/r#12", "working", None),
            ]
        );
        // The first comment by the bot's account that is one, wherever it
        // is, and whatever that account's login is now. Another account
        // that took the bot's login is no one.
        let empty = comment(&heartbeat(vec![]).to_string());
        let comments = [
            serde_json::json!({"user": {"login": "someone", "id": 9}, "body": empty}),
            serde_json::json!({"user": {"login": "example-bot", "id": 666}, "body": empty}),
            serde_json::json!({"user": {"login": "example-bot"}, "body": empty}),
            serde_json::json!({"user": {"login": "example-bot", "id": 7}, "body": "Not a heartbeat"}),
            serde_json::json!({"user": {"login": "renamed-bot", "id": 7}, "body": body,
                               "html_url": "https://github.com/example-org/tracker/issues/1#issuecomment-1"}),
            serde_json::json!({"user": {"login": "example-bot", "id": 7}, "body": comment("{}")}),
        ];
        let found = find_heartbeat(&comments, 7, &github(), now())
            .unwrap()
            .unwrap();
        assert_eq!(found.runs.len(), 2);
        assert_eq!(
            found.comment_url.as_deref(),
            Some("https://github.com/example-org/tracker/issues/1#issuecomment-1")
        );
        assert_eq!(
            find_heartbeat(&comments[..4], 7, &github(), now()),
            Ok(None)
        );
    }

    #[test]
    fn malformed_workers_are_left_out_and_counted() {
        let ok = "https://github.com/o/r/issues/1";
        let mut bad_time = worker("late", ok);
        bad_time["started_at"] = "2026-10-12T05:00:00Z".into();
        let mut bad_status = worker("shouting", ok);
        bad_status["status"] = "<b>LOUD</b>".into();
        let mut bad_devspace = worker("devspace", ok);
        bad_devspace["devspace"] = "../../etc".into();
        let mut workers = vec![
            worker("fine", ok),
            worker("script", "javascript:alert(1)"),
            worker("elsewhere", "https://github.com.evil.example/o/r/issues/1"),
            worker("plain", "http://github.com/o/r/issues/1"),
            worker("userinfo", "https://me@github.com/o/r/issues/1"),
            worker("traversal", "https://github.com/o/r/issues/../../settings"),
            worker("query", "https://github.com/o/r/issues/1?x=1"),
            worker("not an item", "https://github.com/o/r/commits/main"),
            worker("bad name!", ok),
            worker("fine", ok),
            bad_time,
            bad_status,
            bad_devspace,
            serde_json::json!("not a worker"),
            serde_json::json!({"name": "partial"}),
        ];
        // More than bot-heartbeat lists: the rest are left out too.
        workers.extend((0..40).map(|n| worker(&format!("w{n}"), ok)));
        let h =
            parse_heartbeat(&comment(&heartbeat(workers).to_string()), &github(), now()).unwrap();
        let names: Vec<_> = h.runs.iter().map(|r| r.name.as_str()).collect();
        // Fine, the devspace left out of its own worker, and the first of
        // the extra ones that fit within the 32 read.
        assert_eq!(names[..2], ["fine", "devspace"]);
        assert!(h.runs[1].devspace.is_none());
        assert_eq!(h.runs.len(), 2 + (32 - 15));
        assert_eq!(h.skipped, 15 + 40 - h.runs.len());
    }

    #[test]
    fn heartbeats_that_cant_be_trusted_are_refused() {
        let good = heartbeat(vec![]);
        let with = |key: &str, value: Value| {
            let mut v = good.clone();
            v[key] = value;
            comment(&v.to_string())
        };
        let coordinator = |key: &str, value: Value| {
            let mut v = good.clone();
            v["coordinator"][key] = value;
            comment(&v.to_string())
        };
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        let cases = [
            ("not a heartbeat", "Hello".to_string()),
            ("no JSON", format!("{HEARTBEAT_MARKER}\nNothing else")),
            ("unended JSON", format!("{HEARTBEAT_MARKER}\n```json\n{{}}")),
            ("not JSON", comment("{\"schema\": ")),
            ("too deep", comment(&deep)),
            ("too large", comment(&" ".repeat(MAX_COMMENT))),
            (
                "large JSON",
                comment(&format!("{{\"pad\": \"{}\"}}", "x".repeat(MAX_JSON))),
            ),
            ("schema", with("schema", "bot-heartbeat/v2".into())),
            ("no time", with("updated_at", Value::Null)),
            ("bad time", with("updated_at", "yesterday".into())),
            ("future", with("updated_at", "2026-10-12T05:00:00Z".into())),
            ("workers", with("workers", "none".into())),
            ("session", coordinator("session", "a b".into())),
            ("loop state", coordinator("loop_state", "\\u001b[2J".into())),
        ];
        for (what, body) in cases {
            assert!(parse_heartbeat(&body, &github(), now()).is_err(), "{what}");
        }
        // A wake time before the heartbeat or hours after it says nothing.
        for wake in ["2026-10-12T03:00:00Z", "2026-10-12T09:00:00Z", "soon"] {
            let h = parse_heartbeat(&coordinator("next_wake_at", wake.into()), &github(), now())
                .unwrap();
            assert!(h.next_wake_at.is_none(), "{wake}");
        }
        // A comment by the bot that says it is one, but isn't well formed,
        // is an error, not a missing heartbeat.
        let comments = [
            serde_json::json!({"user": {"login": "example-bot", "id": 7}, "body": with("schema", "x".into())}),
        ];
        assert!(find_heartbeat(&comments, 7, &github(), now()).is_err());
    }

    #[test]
    fn a_heartbeat_is_stale_once_old_and_past_its_wake() {
        let at = |text: &str| Timestamp::parse(text).unwrap();
        let h = |state: &str, wake: Option<&str>| Heartbeat {
            updated_at: at("2026-10-12T04:00:00Z"),
            session: "s".into(),
            loop_state: state.into(),
            next_wake_at: wake.map(at),
            runs: vec![],
            skipped: 0,
            comment_url: None,
            stale: false,
        };
        // (heartbeat, read at, stale)
        let cases = [
            (h("working", None), "2026-10-12T04:15:00Z", false),
            (h("working", None), "2026-10-12T04:15:01Z", true),
            (
                h("sleeping", Some("2026-10-12T04:30:00Z")),
                "2026-10-12T04:35:00Z",
                false,
            ),
            (
                h("sleeping", Some("2026-10-12T04:30:00Z")),
                "2026-10-12T04:35:01Z",
                true,
            ),
            // Stopped, not stale.
            (h("stopped", None), "2026-10-13T00:00:00Z", false),
        ];
        for (n, (heartbeat, now, stale)) in cases.into_iter().enumerate() {
            assert_eq!(heartbeat_stale(&heartbeat, at(now)), stale, "case {n}");
        }
    }

    #[test]
    fn items_are_issues_and_pull_requests_on_the_instance() {
        let ghes = Url::parse("https://ghe.example.com/github/").unwrap();
        for (url, base, key) in [
            ("https://github.com/o/r/issues/1", github(), Some("o/r#1")),
            ("https://github.com/o/r/pull/12", github(), Some("o/r#12")),
            (
                "https://ghe.example.com/github/o/r/pull/3",
                ghes.clone(),
                Some("o/r#3"),
            ),
            ("https://ghe.example.com/o/r/pull/3", ghes, None),
            ("https://github.com/o/r/issues/0", github(), None),
            ("https://github.com/o/r/issues/01", github(), None),
            ("https://github.com/o/r/issues/12345678901", github(), None),
            ("https://github.com/o/r/issues/1/", github(), None),
            ("https://github.com/o/r/issues/1#c", github(), None),
            ("https://github.com/-o/r/issues/1", github(), None),
        ] {
            let found = item_on(url, &base).map(|(_, key)| key);
            assert_eq!(found.as_deref(), key, "{url}");
        }
    }

    /// An item of the bot's, with `title`, at `url`.
    fn bot_item(title: &str, url: &str) -> Item {
        let mut item: Item = serde_json::from_value(serde_json::json!({
            "id": "c1:bot:PVTI_a", "connection_id": "c1", "provider": "bot",
            "remote_id": "PVTI_a", "key": "o/r#7", "title": title, "url": url,
            "status": "Needs human", "outcome": "open", "outcome_reason": "", "action": "next",
            "updated_at": "", "effort_id": null, "demo": false,
        }))
        .unwrap();
        item.bot = Some(BotFields::default());
        item
    }
    /// The query of `url`, decoded, by name.
    fn query(url: &str) -> BTreeMap<String, String> {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn asking_the_bot_anything_opens_a_new_tracker_issue() {
        let a = ask(&config(), &github(), None).unwrap();
        let url = Url::parse(&a.issue_url).unwrap();
        assert_eq!(
            (url.host_str(), url.path()),
            (Some("github.com"), "/example-org/tracker/issues/new")
        );
        let q = query(&a.issue_url);
        assert_eq!(q["labels"], "needs-triage");
        assert!(q["body"].contains("## What to do"), "{q:?}");
        assert!(!q.contains_key("title"), "{q:?}");
        assert!(a.comment_url.is_none() && a.comment.is_none());
        // Under the instance's own address, on GitHub Enterprise Server.
        let ghes = Url::parse("https://ghe.example.com/github/").unwrap();
        let url = ask(&config(), &ghes, None).unwrap().issue_url;
        assert!(
            url.starts_with("https://ghe.example.com/github/example-org/tracker/issues/new?"),
            "{url}"
        );
    }

    #[test]
    fn asking_about_an_item_carries_it_whole_and_encoded() {
        let title = "Fix \"a&b=c\" #1 + 100% <script>alert(1)</script>\nnext?line ü";
        let item = bot_item(title, "https://github.com/o/r/issues/7");
        let a = ask(&config(), &github(), Some(&item)).unwrap();
        // Nothing in the title ends a value, starts another, or begins the
        // fragment: every value decodes back to what it was.
        let url = Url::parse(&a.issue_url).unwrap();
        assert!(url.fragment().is_none(), "{url}");
        let raw = url.query().unwrap();
        assert_eq!(raw.matches('&').count(), 2, "{raw}");
        assert!(!raw.contains(['#', ' ', '\n', '+', '"', '<']), "{raw}");
        let q = query(&a.issue_url);
        assert_eq!(q.len(), 3, "{q:?}");
        assert_eq!(q["title"], format!("About o/r#7: {title}"));
        assert_eq!(q["labels"], "needs-triage");
        assert!(
            q["body"].contains("About https://github.com/o/r/issues/7 (o/r#7, Needs human)"),
            "{}",
            q["body"]
        );
        // Or a comment on the item itself, which starts by naming the bot.
        assert_eq!(
            a.comment_url.as_deref(),
            Some("https://github.com/o/r/issues/7")
        );
        assert_eq!(a.comment.as_deref(), Some("@example-bot "));
        // A long title is cut, by characters.
        let long = bot_item(&"ü".repeat(500), "https://github.com/o/r/issues/7");
        let q = query(&ask(&config(), &github(), Some(&long)).unwrap().issue_url);
        assert_eq!(q["title"].chars().count(), MAX_ASK_TITLE);
        // An item elsewhere can only be asked about in a new issue.
        let elsewhere = bot_item("Elsewhere", "https://gitlab.com/o/r/-/issues/7");
        let a = ask(&config(), &github(), Some(&elsewhere)).unwrap();
        assert!(a.comment_url.is_none() && a.comment.is_none());
        assert!(query(&a.issue_url)["body"].contains("https://gitlab.com/o/r/-/issues/7"));
    }

    #[test]
    fn encoding_keeps_only_unreserved_bytes() {
        for (text, encoded) in [
            ("abc-._~XYZ09", "abc-._~XYZ09"),
            ("a b", "a%20b"),
            ("a+b&c=d#e?f/g%h", "a%2Bb%26c%3Dd%23e%3Ff%2Fg%25h"),
            ("\n\r\t", "%0A%0D%09"),
            ("ü", "%C3%BC"),
        ] {
            assert_eq!(encode(text), encoded, "{text}");
        }
    }
}
