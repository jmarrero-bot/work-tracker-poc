//! A harness bot as a source of work, such as jmarrero-bot: where its
//! board, tracker and heartbeat are, and what the bot connector reads
//! besides an item's usual fields. The connector itself is
//! `connectors/github/bot.rs`; like every GitHub connector, it only reads.

use crate::model::{valid_github_login, valid_github_name};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
        if !valid_github_login(&self.forge_org) {
            bail!("Enter the organization that holds the bot's forks")
        }
        Ok(())
    }
    /// The tracker's owner and repository.
    pub fn tracker_parts(&self) -> Option<(&str, &str)> {
        let (owner, repo) = self.tracker.split_once('/')?;
        (valid_github_login(owner) && valid_github_name(repo)).then_some((owner, repo))
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

#[cfg(test)]
pub mod tests {
    use super::*;

    /// jmarrero's bot, as `bot-operator --json` describes it.
    pub fn config() -> BotConfig {
        BotConfig {
            board_owner_type: OwnerType::Orgs,
            board_owner: "jmarrero-forge".into(),
            board_number: 1,
            tracker: "jmarrero-forge/tracker".into(),
            heartbeat_issue: 1,
            bot_login: "jmarrero-bot".into(),
            forge_org: "jmarrero-forge".into(),
        }
    }

    #[test]
    fn boards_as_people_write_them() {
        let org = Some((OwnerType::Orgs, "jmarrero-forge".to_string(), 1));
        for (text, parsed) in [
            ("orgs/jmarrero-forge/1", org.clone()),
            ("orgs/jmarrero-forge/projects/1", org.clone()),
            (
                "https://github.com/orgs/jmarrero-forge/projects/1",
                org.clone(),
            ),
            (
                "https://github.com/orgs/jmarrero-forge/projects/1/views/2",
                None,
            ),
            (
                "users/someone/3",
                Some((OwnerType::Users, "someone".to_string(), 3)),
            ),
            ("teams/jmarrero-forge/1", None),
            ("orgs/jmarrero-forge/0", None),
            ("orgs/jmarrero-forge/x", None),
            ("orgs/a b/1", None),
            ("orgs/../1", None),
            ("jmarrero-forge/1", None),
        ] {
            assert_eq!(parse_board(text).ok(), parsed, "{text}");
        }
    }

    #[test]
    fn configs_are_checked_whole() {
        config().check().unwrap();
        type Break = fn(&mut BotConfig);
        let cases: [(&str, Break); 7] = [
            ("owner", |c| c.board_owner = "a/b".into()),
            ("number", |c| c.board_number = 0),
            ("tracker", |c| c.tracker = "tracker".into()),
            ("tracker path", |c| c.tracker = "o/../r".into()),
            ("heartbeat", |c| c.heartbeat_issue = 0),
            ("login", |c| c.bot_login = "bot is:open".into()),
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
            "https://github.com/orgs/jmarrero-forge/projects/1"
        );
    }
}
