//! Zones: where a connection's work may be seen. Internal work (Jira,
//! internal GitLab, Errata) stays on the machine that syncs it; only external
//! work may leave it, only through [`external_only`], as a hub's export
//! does for its companions.

use crate::{
    bot::{AgentRuns, BotFields, BotKind},
    connector::ErrorKind,
    model::{Item, Outcome, Progress, Reason, Snapshot, Timestamp},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use url::Url;

/// The zone of a connection, and of every item synced through it. A person
/// chooses it when connecting; it isn't a property of the provider, since
/// the same GitLab code serves internal and public instances. Internal is the
/// default, so nothing becomes external without that explicit choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Zone {
    /// Seen only on the machine that syncs it.
    #[default]
    Internal,
    /// May leave that machine in its external export; the only zone a hub
    /// keeps.
    External,
}

impl Zone {
    pub fn label(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::External => "external",
        }
    }
}

/// The part of a snapshot that may leave this machine. Each object is built
/// field by field, so a field added to a connection, item or effort stays
/// behind until someone decides it may leave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalSnapshot {
    /// Always external, so whoever reads it can check what it was given.
    pub zone: Zone,
    pub connections: Vec<ExternalConnection>,
    pub items: Vec<ExternalItem>,
    pub efforts: Vec<ExternalEffort>,
    /// The external bot connections: what their Ask links need, and what
    /// they last read of their heartbeat and board. None from hubs that
    /// predate them.
    #[serde(default)]
    pub bots: Vec<ExternalBot>,
}

/// An external connection. A sync error's message can name local paths or
/// users, so only its kind leaves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalConnection {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub base_url: String,
    pub account_name: String,
    /// The account's numeric ID, which the instance shows anyone and which
    /// survives renames, so a companion matches its own connections by it
    /// ([`numeric_account_id`]). None for an account whose ID isn't
    /// numeric, and from hubs that predate it.
    #[serde(default)]
    pub account_id: Option<String>,
    /// When its last successful sync started: how fresh its items are.
    pub last_success: Option<Timestamp>,
    /// Whether that was too long ago for its items to be current, so the
    /// hub never shows them as such.
    pub stale: bool,
    pub error_kind: Option<ErrorKind>,
    pub zone: Zone,
}

/// An external item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalItem {
    pub id: String,
    pub connection_id: String,
    pub provider: String,
    pub key: String,
    pub title: String,
    pub url: String,
    pub status: String,
    pub outcome: Outcome,
    pub outcome_reason: String,
    pub action: String,
    pub updated_at: String,
    /// Only an exported effort.
    pub effort_id: Option<String>,
    pub zone: Zone,
    /// Why it waits on its connection's account, for a companion's Mine
    /// view.
    pub reasons: Vec<Reason>,
    /// For a bot item, its board fields, or the board item that links a
    /// fork pull request. None from hubs that predate them.
    #[serde(default)]
    pub bot: Option<ExternalBotFields>,
}

/// What the bot connector read of an item besides its usual fields, all of
/// it public on the bot's board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalBotFields {
    pub kind: BotKind,
    pub priority: Option<String>,
    pub why: Option<String>,
    pub news: Option<String>,
    pub branch: Option<String>,
    pub board_item: Option<String>,
}

impl ExternalBotFields {
    fn of(b: &BotFields) -> Self {
        Self {
            kind: b.kind,
            priority: b.priority.clone(),
            why: b.why.clone(),
            news: b.news.clone(),
            branch: b.branch.clone(),
            board_item: b.board_item.clone(),
        }
    }
}

/// An external bot connection: what a companion needs to build its Ask
/// links (the tracker and the bot's login), its board's address, and what
/// it last read of its heartbeat and board. Its heartbeat issue and forge
/// organization aren't needed, and aren't exported as settings, though the
/// heartbeat's comment link and the fork pull requests, all public, show
/// them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalBot {
    pub connection_id: String,
    pub bot_login: String,
    /// The repository work is asked of it in, `OWNER/REPO`.
    pub tracker: String,
    pub board_url: String,
    /// None until a sync read them.
    pub agents: Option<ExternalAgents>,
}

/// What a bot connection last read of its heartbeat and board. Why a read
/// failed stays behind, as a connection's error message does: only that
/// it failed leaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAgents {
    /// None when the bot publishes no heartbeat.
    pub heartbeat: Option<ExternalHeartbeat>,
    pub failed: bool,
    pub last_success: Option<Timestamp>,
    pub stale: bool,
    pub board: Option<ExternalBoard>,
}

/// A coordinator's heartbeat, as the bot published it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalHeartbeat {
    pub updated_at: Timestamp,
    pub session: String,
    pub loop_state: String,
    pub next_wake_at: Option<Timestamp>,
    pub runs: Vec<ExternalRun>,
    pub skipped: usize,
    pub comment_url: Option<String>,
}

/// One agent run of a heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRun {
    pub name: String,
    pub item_url: String,
    pub item_key: String,
    pub status: String,
    pub started_at: Timestamp,
    pub devspace: Option<String>,
}

/// A bot's board, counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalBoard {
    pub statuses: BTreeMap<String, usize>,
    pub fork_prs: usize,
    pub unreadable: usize,
}

impl ExternalAgents {
    fn of(a: &AgentRuns) -> Self {
        Self {
            heartbeat: a.heartbeat.as_ref().map(|h| ExternalHeartbeat {
                updated_at: h.updated_at,
                session: h.session.clone(),
                loop_state: h.loop_state.clone(),
                next_wake_at: h.next_wake_at,
                runs: h
                    .runs
                    .iter()
                    .map(|r| ExternalRun {
                        name: r.name.clone(),
                        item_url: r.item_url.clone(),
                        item_key: r.item_key.clone(),
                        status: r.status.clone(),
                        started_at: r.started_at,
                        devspace: r.devspace.clone(),
                    })
                    .collect(),
                skipped: h.skipped,
                comment_url: h.comment_url.clone(),
            }),
            failed: a.error.is_some(),
            last_success: a.last_success,
            stale: a.stale,
            board: a.board.as_ref().map(|b| ExternalBoard {
                statuses: b.statuses.clone(),
                fork_prs: b.fork_prs,
                unreadable: b.unreadable,
            }),
        }
    }
}

/// An effort as the external zone sees it. Its title and note are free text
/// that may describe internal work: the note stays behind, and the title
/// leaves only when a person shared it, or from a hub ([`Exporter::Hub`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalEffort {
    pub id: String,
    /// The effort's title if shared or a hub's, otherwise a neutral label.
    pub title: String,
    /// Over its external items only.
    pub progress: ExternalProgress,
}

/// An exported effort's progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalProgress {
    pub delivered: usize,
    pub total: usize,
    pub open: usize,
    pub needs_attention: usize,
    pub abandoned: usize,
}

impl ExternalProgress {
    /// The progress over these outcomes.
    fn of(outcomes: Vec<Outcome>) -> Self {
        let p = Progress::of(outcomes);
        Self {
            delivered: p.delivered,
            total: p.total,
            open: p.open,
            needs_attention: p.needs_attention,
            abandoned: p.abandoned,
        }
    }
}

/// Whose export it is, which decides whether an effort's title leaves
/// without being shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exporter {
    /// A hub, which serves its export to its companions. A hub holds
    /// external work only, and a companion is trusted more than its hub, so
    /// hiding a title typed on the hub from it would protect nothing: its
    /// efforts' titles always leave.
    Hub,
    /// Any other service, whose efforts may mix internal work with
    /// external, so a title leaves only once a person shared it.
    Local,
}

/// Most digits in an exported account ID: a 64-bit number's.
pub const MAX_ACCOUNT_ID: usize = 20;

/// `id`, if it is a numeric account ID, such as GitHub's, GitLab's and
/// Forgejo's, which they show anyone. Any other, such as a plugin's or
/// Jira's user key, which can be a name or a mail address, stays behind.
pub fn numeric_account_id(id: &str) -> Option<String> {
    (!id.is_empty() && id.len() <= MAX_ACCOUNT_ID && id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id.to_string())
}

/// How an effort whose title isn't shared is labeled: "Effort" and the start
/// of its random ID.
pub fn neutral_title(id: &str) -> String {
    let short: String = id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    format!("Effort {short}")
}

/// The boundary of the internal zone. Everything that leaves this machine
/// goes through here, and only what is positively external passes:
///
/// - connections whose zone is external, without their error messages;
/// - items that say they are external and whose connection is one of those,
///   so a manual item, an item of an internal or unknown connection, or a
///   row that disagrees with its connection stays behind;
/// - efforts with at least one of those items, with their progress over
///   those items only, and their title only if shared, or if `from` is a
///   hub. An effort that mixes zones shows no trace of its internal items:
///   no count, no placeholder, no link;
/// - of those connections that are a bot's, its tracker, login and board,
///   and what `agents` says it last read of its heartbeat and board.
pub fn external_only(
    snapshot: &Snapshot,
    agents: &[AgentRuns],
    from: Exporter,
) -> ExternalSnapshot {
    let connections: Vec<ExternalConnection> = snapshot
        .connections
        .iter()
        .filter(|c| c.zone == Zone::External)
        .map(|c| ExternalConnection {
            id: c.id.clone(),
            provider: c.provider.clone(),
            label: c.label.clone(),
            base_url: c.base_url.clone(),
            account_name: c.account_name.clone(),
            account_id: numeric_account_id(&c.account_id),
            last_success: c.sync.last_success,
            stale: c.schedule.stale,
            error_kind: c
                .sync
                .error
                .as_ref()
                .map(|_| c.sync.error_kind.unwrap_or(ErrorKind::Invalid)),
            zone: Zone::External,
        })
        .collect();
    let external: HashSet<&str> = connections.iter().map(|c| c.id.as_str()).collect();
    let items: Vec<&Item> = snapshot
        .items
        .iter()
        .filter(|i| {
            i.zone == Zone::External
                && i.connection_id
                    .as_deref()
                    .is_some_and(|c| external.contains(c))
        })
        .collect();
    let efforts: Vec<ExternalEffort> = snapshot
        .efforts
        .iter()
        .filter_map(|e| {
            let linked: Vec<_> = items
                .iter()
                .filter(|i| i.effort_id.as_deref() == Some(e.effort.id.as_str()))
                .map(|i| i.outcome)
                .collect();
            (!linked.is_empty()).then(|| ExternalEffort {
                id: e.effort.id.clone(),
                title: if e.effort.share_title || from == Exporter::Hub {
                    e.effort.title.clone()
                } else {
                    neutral_title(&e.effort.id)
                },
                progress: ExternalProgress::of(linked),
            })
        })
        .collect();
    let exported: HashSet<&str> = efforts.iter().map(|e| e.id.as_str()).collect();
    let items = items
        .into_iter()
        .map(|i| ExternalItem {
            id: i.id.clone(),
            connection_id: i.connection_id.clone().unwrap_or_default(),
            provider: i.provider.clone(),
            key: i.key.clone(),
            title: i.title.clone(),
            url: i.url.clone(),
            status: i.status.clone(),
            outcome: i.outcome,
            outcome_reason: i.outcome_reason.clone(),
            action: i.action.clone(),
            updated_at: i.updated_at.clone(),
            effort_id: i
                .effort_id
                .clone()
                .filter(|e| exported.contains(e.as_str())),
            zone: Zone::External,
            reasons: i.reasons.clone(),
            bot: i.bot.as_ref().map(ExternalBotFields::of),
        })
        .collect();
    let bots = snapshot
        .connections
        .iter()
        .filter(|c| external.contains(c.id.as_str()))
        .filter_map(|c| {
            let config = c.bot.as_ref()?;
            let base = Url::parse(&c.base_url).ok()?;
            Some(ExternalBot {
                connection_id: c.id.clone(),
                bot_login: config.bot_login.clone(),
                tracker: config.tracker.clone(),
                board_url: config.board_url(&base).ok()?.into(),
                agents: agents
                    .iter()
                    .find(|a| a.connection_id == c.id)
                    .map(ExternalAgents::of),
            })
        })
        .collect();
    ExternalSnapshot {
        zone: Zone::External,
        connections,
        items,
        efforts,
        bots,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        ConnectInput, Connection, Effort, EffortView, Schedule, SyncState, Timestamp,
    };

    #[test]
    fn zones_default_to_internal() {
        let connect: ConnectInput = serde_json::from_value(serde_json::json!({
            "provider": "gitlab-public",
            "label": "GitLab",
            "base_url": "https://gitlab.com/",
            "token": "test-only-token",
        }))
        .unwrap();
        assert_eq!(connect.zone, Zone::Internal);
        // Rows written before zones existed.
        let connection: Connection = serde_json::from_value(serde_json::json!({
            "id": "c1", "provider": "gitlab-public", "label": "GitLab",
            "base_url": "https://gitlab.com/", "account_id": "1", "account_name": "me",
            "last_sync": null, "error": null,
        }))
        .unwrap();
        assert_eq!(connection.zone, Zone::Internal);
        let item: Item = serde_json::from_value(serde_json::json!({
            "id": "c1:gitlab:7:4", "connection_id": "c1", "provider": "gitlab-public",
            "remote_id": "7:4", "key": "!4", "title": "Fix", "url": "https://gitlab.com/p/-/merge_requests/4",
            "status": "opened", "outcome": "open", "outcome_reason": "", "action": "next",
            "updated_at": "", "effort_id": null, "demo": false,
        }))
        .unwrap();
        assert_eq!(item.zone, Zone::Internal);
        let explicit: ConnectInput = serde_json::from_value(serde_json::json!({
            "provider": "gitlab-public", "label": "GitLab", "base_url": "https://gitlab.com/",
            "token": "test-only-token", "zone": "external",
        }))
        .unwrap();
        assert_eq!(explicit.zone, Zone::External);
    }

    /// Every string of internal data contains this, so a leak shows up in
    /// the serialized export, wherever it is.
    const INTERNAL: &str = "INTERNAL";
    const OUTCOMES: [Outcome; 5] = [
        Outcome::Open,
        Outcome::Delivered,
        Outcome::ClosedWithoutDelivery,
        Outcome::Abandoned,
        Outcome::Unknown,
    ];

    fn tag(external: bool) -> &'static str {
        if external {
            "external"
        } else {
            INTERNAL
        }
    }
    fn connection(n: usize, zone: Zone) -> Connection {
        let tag = tag(zone == Zone::External);
        Connection {
            id: format!("{tag}-connection-{n}"),
            provider: "gitlab-public".into(),
            label: format!("{tag} label {n}"),
            base_url: format!("https://{tag}-{n}.example/"),
            // Numeric, so exported if external.
            account_id: if zone == Zone::External {
                format!("{}", 1000 + n)
            } else {
                format!("{INTERNAL}-account-{n}")
            },
            account_name: format!("{tag} account {n}"),
            zone,
            watch_user: Some(format!("{INTERNAL}-watched")),
            sync_interval: None,
            schedule: Schedule {
                stale: true,
                ..Schedule::default()
            },
            sync: SyncState {
                last_success: Some(Timestamp::from_unix(1_791_763_200 + n as i64)),
                last_attempt: Some(Timestamp::from_unix(1_791_763_300)),
                next_due: None,
                failures: 2,
                error_kind: Some(ErrorKind::Transient),
                error: Some(format!("{INTERNAL} error message {n}")),
                held_until: None,
            },
            from_hub: false,
            // An external bot's board, tracker and login leave, for its
            // companions' Ask links. Its forge organization isn't exported
            // as a setting (its fork pull requests show it).
            bot: Some(crate::model::BotConfig {
                board_owner: format!("{tag}-owner"),
                tracker: format!("{tag}/tracker"),
                bot_login: format!("{tag}-bot"),
                forge_org: format!("{INTERNAL}-forge"),
                ..crate::bot::tests::config()
            }),
            // Not exported either: which plugin file it trusts.
            plugin: Some(crate::model::PluginConfig {
                sha256: Some(format!("{INTERNAL}-sha256")),
                env: vec![format!("{INTERNAL}-variable")],
            }),
        }
    }
    /// What `connection` last read of its heartbeat and board, tagged
    /// internal unless it is external, but for its error message, which
    /// never leaves.
    fn agents(connection: &Connection) -> AgentRuns {
        use crate::bot::{AgentRun, BoardSummary, Heartbeat};
        let tag = tag(connection.zone == Zone::External);
        AgentRuns {
            connection_id: connection.id.clone(),
            heartbeat: Some(Heartbeat {
                updated_at: Timestamp::from_unix(1_791_763_000),
                session: format!("{tag}-session"),
                loop_state: format!("{tag}-state"),
                next_wake_at: None,
                runs: vec![AgentRun {
                    name: format!("{tag}-run"),
                    item_url: format!("https://{tag}.example/o/r/issues/1"),
                    item_key: format!("{tag}/r#1"),
                    status: format!("{tag}-status"),
                    started_at: Timestamp::from_unix(1_791_762_000),
                    devspace: Some(format!("{tag}-devspace")),
                }],
                skipped: 1,
                comment_url: Some(format!(
                    "https://{tag}.example/o/tracker/issues/1#issuecomment-1"
                )),
                stale: false,
            }),
            error: Some(format!("{INTERNAL} heartbeat error")),
            last_success: Some(Timestamp::from_unix(1_791_763_100)),
            stale: true,
            board: Some(BoardSummary {
                statuses: [(format!("{tag} status"), 2)].into(),
                fork_prs: 3,
                unreadable: 1,
            }),
        }
    }
    /// An item of `connection`, or a manual one, whose row says `zone`.
    /// Its text is tagged internal unless it should be exported.
    fn item(
        n: usize,
        connection: Option<&Connection>,
        zone: Zone,
        outcome: Outcome,
        effort: Option<&str>,
    ) -> Item {
        let external =
            zone == Zone::External && connection.is_some_and(|c| c.zone == Zone::External);
        let tag = tag(external);
        Item {
            id: format!("{tag}-item-{n}"),
            connection_id: connection.map(|c| c.id.clone()),
            provider: "gitlab-public".into(),
            remote_id: format!("{INTERNAL}-remote-{n}"),
            key: format!("{tag}-key-{n}"),
            title: format!("{tag} title {n}"),
            url: format!("https://{tag}.example/{n}"),
            status: format!("{tag} status {n}"),
            outcome,
            outcome_reason: format!("{tag} reason {n}"),
            action: String::new(),
            updated_at: format!("{tag} time {n}"),
            effort_id: effort.map(str::to_string),
            abandoned_at: Some(format!("{INTERNAL} abandoned {n}")),
            demo: false,
            zone,
            reasons: vec![],
            synced_at: None,
            stale: false,
            from_hub: false,
            bot: Some(crate::model::BotFields {
                kind: crate::bot::BotKind::ForkPr,
                why: Some(format!("{tag} why {n}")),
                news: Some(format!("{tag} news {n}")),
                priority: Some(format!("{tag} priority {n}")),
                branch: Some(format!("https://{tag}.example/branch/{n}")),
                board_item: Some(format!("https://{tag}.example/board/{n}")),
            }),
        }
    }
    /// An effort whose title is tagged internal unless it's shared.
    fn effort(n: usize, share_title: bool) -> Effort {
        Effort {
            id: format!("effort-{n}"),
            title: if share_title {
                format!("effort title {n}")
            } else {
                format!("{INTERNAL} title {n}")
            },
            note: format!("{INTERNAL} note {n}"),
            share_title,
        }
    }
    /// A snapshot as the database builds one, with progress over all items.
    fn snapshot(connections: Vec<Connection>, items: Vec<Item>, efforts: Vec<Effort>) -> Snapshot {
        let efforts = efforts
            .into_iter()
            .map(|effort| EffortView {
                progress: Progress::of(
                    items
                        .iter()
                        .filter(|i| i.effort_id.as_deref() == Some(&effort.id))
                        .map(|i| i.outcome),
                ),
                effort,
                from_hub: false,
            })
            .collect();
        Snapshot {
            connections,
            items,
            efforts,
        }
    }

    #[test]
    fn only_items_external_on_both_counts_leave() {
        use Zone::*;
        let internal = connection(0, Internal);
        let external = connection(1, External);
        let unknown = connection(2, External);
        // (connection, what the row says, exported)
        let cases = [
            (Some(&external), External, true),
            (Some(&internal), Internal, false),
            (Some(&internal), External, false),
            (Some(&external), Internal, false),
            // Its connection isn't in the snapshot.
            (Some(&unknown), External, false),
            // Manual items.
            (None, Internal, false),
            (None, External, false),
        ];
        for (n, (c, zone, exported)) in cases.into_iter().enumerate() {
            let s = snapshot(
                vec![internal.clone(), external.clone()],
                vec![item(n, c, zone, Outcome::Open, None)],
                vec![],
            );
            let out = external_only(&s, &[], Exporter::Local);
            assert_eq!(out.items.len(), usize::from(exported), "case {n}");
            assert_eq!(out.connections.len(), 1);
            assert_eq!(out.connections[0].id, external.id);
        }
    }

    #[test]
    fn mixed_effort_keeps_only_its_external_items() {
        use Outcome::*;
        use Zone::*;
        let internal = connection(0, Internal);
        let external = connection(1, External);
        for share_title in [false, true] {
            let e = effort(0, share_title);
            let id = Some(e.id.as_str());
            let s = snapshot(
                vec![internal.clone(), external.clone()],
                vec![
                    item(0, Some(&external), External, Open, id),
                    item(1, Some(&external), External, Delivered, id),
                    item(2, Some(&internal), Internal, Delivered, id),
                    item(3, Some(&internal), Internal, ClosedWithoutDelivery, id),
                    item(4, None, Internal, Unknown, id),
                ],
                vec![e.clone(), effort(1, true)],
            );
            // The full snapshot counts all five.
            assert_eq!(s.efforts[0].progress.total, 5);
            let out = external_only(&s, &[], Exporter::Local);
            assert_eq!(
                out.efforts.len(),
                1,
                "an effort without external items stays"
            );
            let shown = &out.efforts[0];
            let title = if share_title {
                "effort title 0".to_string()
            } else {
                neutral_title("effort-0")
            };
            assert_eq!(
                (shown.id.as_str(), shown.title.clone()),
                ("effort-0", title)
            );
            assert_eq!(
                shown.progress,
                ExternalProgress {
                    delivered: 1,
                    total: 2,
                    open: 1,
                    needs_attention: 0,
                    abandoned: 0,
                }
            );
            let ids: Vec<_> = out.items.iter().map(|i| i.id.as_str()).collect();
            assert_eq!(ids, ["external-item-0", "external-item-1"]);
            assert!(out.items.iter().all(|i| i.effort_id == Some(e.id.clone())));
            let json = serde_json::to_string(&out).unwrap();
            assert!(!json.contains(INTERNAL), "{json}");
            assert!(!json.contains("effort title 1"), "{json}");
        }
    }

    #[test]
    fn a_hubs_effort_titles_always_leave() {
        let external = connection(1, Zone::External);
        for share_title in [false, true] {
            let e = Effort {
                title: "hub effort title".into(),
                ..effort(0, share_title)
            };
            let s = snapshot(
                vec![external.clone()],
                vec![item(
                    0,
                    Some(&external),
                    Zone::External,
                    Outcome::Open,
                    Some(&e.id),
                )],
                vec![e],
            );
            let hub = external_only(&s, &[], Exporter::Hub);
            assert_eq!(hub.efforts[0].title, "hub effort title");
            // The note still stays behind.
            let json = serde_json::to_string(&hub).unwrap();
            assert!(!json.contains(INTERNAL), "{json}");
            let local = external_only(&s, &[], Exporter::Local);
            let title = if share_title {
                "hub effort title".to_string()
            } else {
                neutral_title("effort-0")
            };
            assert_eq!(local.efforts[0].title, title);
        }
    }

    #[test]
    fn unshared_titles_are_neutral() {
        let uuid = "1a2b3c4d-5e6f-4a1b-8c2d-3e4f5a6b7c8d";
        assert_eq!(neutral_title(uuid), "Effort 1a2b3c4d");
    }

    /// The exact keys of every exported object. A new field fails here
    /// until someone decides whether it may leave this machine.
    #[test]
    fn exported_objects_have_exactly_these_keys() {
        let external = connection(1, Zone::External);
        let e = effort(0, true);
        let s = snapshot(
            vec![external.clone()],
            vec![item(
                0,
                Some(&external),
                Zone::External,
                Outcome::Open,
                Some(&e.id),
            )],
            vec![e],
        );
        let read = [agents(&external)];
        let out = serde_json::to_value(external_only(&s, &read, Exporter::Local)).unwrap();
        let keys = |v: &serde_json::Value| {
            let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        let agents = &out["bots"][0]["agents"];
        let heartbeat = &agents["heartbeat"];
        let cases: [(&serde_json::Value, &[&str]); 11] = [
            (
                &out["efforts"][0]["progress"],
                &["abandoned", "delivered", "needs_attention", "open", "total"],
            ),
            (&out, &["bots", "connections", "efforts", "items", "zone"]),
            (
                &out["bots"][0],
                &[
                    "agents",
                    "board_url",
                    "bot_login",
                    "connection_id",
                    "tracker",
                ],
            ),
            (
                agents,
                &["board", "failed", "heartbeat", "last_success", "stale"],
            ),
            (
                heartbeat,
                &[
                    "comment_url",
                    "loop_state",
                    "next_wake_at",
                    "runs",
                    "session",
                    "skipped",
                    "updated_at",
                ],
            ),
            (
                &heartbeat["runs"][0],
                &[
                    "devspace",
                    "item_key",
                    "item_url",
                    "name",
                    "started_at",
                    "status",
                ],
            ),
            (&agents["board"], &["fork_prs", "statuses", "unreadable"]),
            (
                &out["items"][0]["bot"],
                &["board_item", "branch", "kind", "news", "priority", "why"],
            ),
            (
                &out["connections"][0],
                &[
                    "account_id",
                    "account_name",
                    "base_url",
                    "error_kind",
                    "id",
                    "label",
                    "last_success",
                    "provider",
                    "stale",
                    "zone",
                ],
            ),
            (
                &out["items"][0],
                &[
                    "action",
                    "bot",
                    "connection_id",
                    "effort_id",
                    "id",
                    "key",
                    "outcome",
                    "outcome_reason",
                    "provider",
                    "reasons",
                    "status",
                    "title",
                    "updated_at",
                    "url",
                    "zone",
                ],
            ),
            (&out["efforts"][0], &["id", "progress", "title"]),
        ];
        for (value, expected) in cases {
            assert_eq!(keys(value), expected, "{value}");
        }
        assert_eq!(out["connections"][0]["error_kind"], "transient");
        // Freshness, and nothing else of the sync state, such as its
        // message.
        assert_eq!(
            out["connections"][0]["last_success"],
            "2026-10-12T00:00:01Z"
        );
        assert_eq!(out["connections"][0]["stale"], true);
        assert_eq!(out["connections"][0]["account_id"], "1001");
    }

    #[test]
    fn only_numeric_account_ids_leave() {
        for (id, exported) in [
            ("1894", Some("1894")),
            ("0", Some("0")),
            ("12345678901234567890", Some("12345678901234567890")),
            ("123456789012345678901", None),
            ("", None),
            ("JIRAUSER1234", None),
            ("someone@example.com", None),
            ("12 34", None),
            ("-1", None),
            ("١٢٣", None),
        ] {
            assert_eq!(numeric_account_id(id).as_deref(), exported, "{id}");
            let c = Connection {
                account_id: id.into(),
                ..connection(1, Zone::External)
            };
            let out = external_only(&snapshot(vec![c], vec![], vec![]), &[], Exporter::Local);
            assert_eq!(out.connections[0].account_id.as_deref(), exported, "{id}");
        }
    }

    /// A small deterministic generator (xorshift64), so a failing case is
    /// named by its seed.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn zone(&mut self) -> Zone {
            if self.next() & 1 == 1 {
                Zone::External
            } else {
                Zone::Internal
            }
        }
    }

    #[test]
    fn nothing_internal_ever_leaves() {
        for seed in 1..=1000u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let connections: Vec<_> = (0..rng.below(5))
                .map(|n| connection(n, rng.zone()))
                .collect();
            // Some connections are left out of the snapshot, as if deleted.
            let mut known = connections.clone();
            known.truncate(rng.below(connections.len() + 1));
            let efforts: Vec<_> = (0..rng.below(4))
                .map(|n| effort(n, rng.next() & 1 == 1))
                .collect();
            let items: Vec<_> = (0..rng.below(12))
                .map(|n| {
                    let c = match rng.below(connections.len() + 1) {
                        0 => None,
                        k => Some(&connections[k - 1]),
                    };
                    let e = match rng.below(efforts.len() + 1) {
                        0 => None,
                        k => Some(efforts[k - 1].id.as_str()),
                    };
                    let zone = rng.zone();
                    item(n, c, zone, OUTCOMES[rng.below(OUTCOMES.len())], e)
                })
                .collect();
            // What some of the bots, known or not, last read.
            let read: Vec<_> = connections
                .iter()
                .filter(|_| rng.next() & 1 == 1)
                .map(agents)
                .collect();
            let s = snapshot(known.clone(), items.clone(), efforts.clone());
            let out = external_only(&s, &read, Exporter::Local);
            let json = serde_json::to_string(&out).unwrap();
            assert!(!json.contains(INTERNAL), "seed {seed}: {json}");
            assert_eq!(out.zone, Zone::External);

            let external = |c: &&Connection| c.zone == Zone::External;
            let expected: Vec<_> = known.iter().filter(external).map(|c| &c.id).collect();
            let shown: Vec<_> = out.connections.iter().map(|c| &c.id).collect();
            assert_eq!(shown, expected, "seed {seed}");
            // With their numeric account IDs: an internal one is tagged.
            for c in &out.connections {
                let known = known.iter().find(|k| k.id == c.id).unwrap();
                let id = numeric_account_id(&known.account_id);
                assert!(id.is_some(), "seed {seed}");
                assert_eq!(c.account_id, id, "seed {seed}");
            }
            // Each is a bot, whose settings leave with it, and what it last
            // read, if it did.
            let bots: Vec<_> = out.bots.iter().map(|b| &b.connection_id).collect();
            assert_eq!(bots, shown, "seed {seed}");
            for b in &out.bots {
                let read = read.iter().find(|a| a.connection_id == b.connection_id);
                assert_eq!(b.agents, read.map(ExternalAgents::of), "seed {seed}");
                assert!(b.agents.as_ref().is_none_or(|a| a.failed), "seed {seed}");
            }
            // Everything external does leave.
            let expected: Vec<_> = items
                .iter()
                .filter(|i| {
                    i.zone == Zone::External
                        && known
                            .iter()
                            .filter(external)
                            .any(|c| i.connection_id.as_ref() == Some(&c.id))
                })
                .collect();
            let shown: Vec<_> = out.items.iter().collect();
            assert_eq!(shown.len(), expected.len(), "seed {seed}");
            for (shown, expected) in shown.iter().zip(&expected) {
                assert_eq!(shown.id, expected.id, "seed {seed}");
                assert_eq!(shown.effort_id, expected.effort_id, "seed {seed}");
                assert_eq!(
                    shown.bot,
                    expected.bot.as_ref().map(ExternalBotFields::of),
                    "seed {seed}"
                );
            }
            for e in &efforts {
                let outcomes: Vec<_> = expected
                    .iter()
                    .filter(|i| i.effort_id.as_deref() == Some(&e.id))
                    .map(|i| i.outcome)
                    .collect();
                let shown = out.efforts.iter().find(|x| x.id == e.id);
                let title = json.contains(&format!("\"{}\"", e.title));
                if outcomes.is_empty() {
                    assert!(shown.is_none() && !title, "seed {seed}: {}", e.id);
                } else {
                    let shown = shown.unwrap();
                    assert_eq!(
                        shown.progress,
                        ExternalProgress::of(outcomes),
                        "seed {seed}"
                    );
                    // Unshared titles are tagged internal, so checked above.
                    assert_eq!(title, e.share_title, "seed {seed}");
                    if !e.share_title {
                        assert_eq!(shown.title, neutral_title(&e.id), "seed {seed}");
                    }
                }
            }
        }
    }
}
