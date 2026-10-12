//! What the views show, worked out from what the service answered, with the
//! same meanings as the web page: an item's outcome next to its source's own
//! status, why something waits on whom, how fresh it is, and how syncs go.
//! Nothing here needs a display, so it is tested on its own.

use crate::api::{
    Board, Connection, ErrorKind, HubStatus, Item, Outcome, Progress, Reason, Snapshot,
};
use std::collections::HashMap;

/// How many of its intervals after its last success a connection's items
/// are stale, as the service decides.
const STALE_INTERVALS: i64 = 3;

/// Seconds since the Unix epoch of an RFC 3339 time, such as the service
/// writes.
pub fn parse_time(text: &str) -> Option<i64> {
    gtk::glib::DateTime::from_iso8601(text, None)
        .ok()
        .map(|t| t.to_unix())
}

/// When `then` is, seen from `now`: "in 4 min", "3 h ago".
pub fn relative(then: i64, now: i64) -> String {
    let seconds = then - now;
    let span = seconds.abs();
    if span < 60 {
        return if seconds >= 0 {
            "within a minute"
        } else {
            "just now"
        }
        .into();
    }
    let amount = if span < 7200 {
        format!("{} min", span / 60)
    } else if span < 172_800 {
        format!("{} h", span / 3600)
    } else {
        format!("{} d", span / 86_400)
    };
    if seconds >= 0 {
        format!("in {amount}")
    } else {
        format!("{amount} ago")
    }
}

/// [`relative`] of an RFC 3339 time, or the text itself if it doesn't parse.
pub fn relative_to(text: &str, now: i64) -> String {
    parse_time(text).map_or_else(|| text.to_string(), |t| relative(t, now))
}

/// An interval as the CLI and the page write it: "5m", "1h", "7d".
pub fn every(seconds: u64) -> String {
    if seconds > 0 && seconds.is_multiple_of(86_400) {
        format!("{}d", seconds / 86_400)
    } else if seconds > 0 && seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds > 0 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// `text` with a capital first letter.
pub fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or(String::new(), |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

pub fn outcome_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Open => "Open",
        Outcome::Delivered => "Delivered",
        Outcome::ClosedWithoutDelivery => "Closed, not delivered",
        Outcome::Abandoned => "Abandoned",
        Outcome::Unknown => "Unknown",
    }
}

/// Finished: delivered, or abandoned by a person.
pub fn settled(outcome: Outcome) -> bool {
    matches!(outcome, Outcome::Delivered | Outcome::Abandoned)
}

/// Never progress: a person decides what happened.
pub fn needs_decision(outcome: Outcome) -> bool {
    matches!(outcome, Outcome::ClosedWithoutDelivery | Outcome::Unknown)
}

/// How an outcome looks: the same four kinds as the page's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Open,
    Delivered,
    Attention,
    Abandoned,
}

impl Tone {
    pub fn of(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Delivered => Self::Delivered,
            Outcome::Abandoned => Self::Abandoned,
            o if needs_decision(o) => Self::Attention,
            _ => Self::Open,
        }
    }
    /// Its icon. Open is the usual, so it has none: an accent on every
    /// open row would read as unread, and accents are kept for what needs
    /// attention.
    pub fn icon_name(self) -> Option<&'static str> {
        match self {
            Self::Open => None,
            Self::Delivered => Some("object-select-symbolic"),
            Self::Attention => Some("dialog-warning-symbolic"),
            Self::Abandoned => Some("window-close-symbolic"),
        }
    }
    /// The style class, from libadwaita's palette.
    pub fn css_class(self) -> &'static str {
        match self {
            Self::Open => "dim-label",
            Self::Delivered => "success",
            Self::Attention => "warning",
            Self::Abandoned => "dim-label",
        }
    }
}

pub fn reason_label(reason: &Reason) -> String {
    match reason {
        Reason::ReviewRequested => "Review requested",
        Reason::TeamReviewRequested => "Team review requested",
        Reason::Assigned => "Assigned",
        Reason::Mentioned => "Mentioned",
        Reason::ChangesRequested => "Needs action: changes requested",
        Reason::ChecksFailing => "Needs action: checks failing",
        Reason::Authored => "Authored",
        Reason::Todo => "On the to-do list",
        Reason::BotWaiting => "Bot waiting on you",
        Reason::Other(name) => return capitalized(&name.replace('_', " ")),
    }
    .into()
}

pub fn error_kind_label(kind: &ErrorKind) -> String {
    match kind {
        ErrorKind::Auth => "Credential rejected",
        ErrorKind::Transient => "Temporary failure",
        ErrorKind::Inaccessible => "Not accessible",
        ErrorKind::Invalid => "Unusable answer",
        ErrorKind::Other(name) => return capitalized(&name.replace('_', " ")),
    }
    .into()
}

/// A source's name and the two letters its badge shows.
pub fn provider(provider: &str) -> (&str, &str) {
    match provider {
        "jira" => ("Jira", "JI"),
        "gitlab-internal" => ("Internal GitLab", "GL"),
        "gitlab-public" => ("Public GitLab", "GL"),
        "github" => ("GitHub", "GH"),
        "fedora" => ("Fedora Forge", "FE"),
        "bot" => ("Bot", "BT"),
        "errata" => ("Errata", "ER"),
        other => (other, "··"),
    }
}

/// "Stale: last synced 3 h ago", for what can't be shown as current.
pub fn stale_text(stale: bool, synced_at: Option<&str>, now: i64) -> Option<String> {
    if !stale {
        return None;
    }
    Some(match synced_at {
        Some(t) => format!("Stale: last synced {}", relative_to(t, now)),
        None => "Stale: not synced".into(),
    })
}

/// How an effort is doing besides what was delivered.
pub fn progress_text(p: &Progress) -> String {
    let mut parts = vec![];
    if p.open > 0 {
        parts.push(format!("{} open", p.open));
    }
    match p.needs_attention {
        0 => (),
        1 => parts.push("1 needs attention".into()),
        n => parts.push(format!("{n} need attention")),
    }
    if p.abandoned > 0 {
        parts.push(format!("{} abandoned", p.abandoned));
    }
    if parts.is_empty() {
        "Nothing remaining".into()
    } else {
        parts.join(" · ")
    }
}

/// The share of an effort delivered, from 0 to 1.
pub fn delivered_fraction(p: &Progress) -> f64 {
    if p.total == 0 {
        0.0
    } else {
        p.delivered as f64 / p.total as f64
    }
}

/// Like `workboard next`: open items with a next action or a review, and
/// those that need a decision.
pub fn is_next(item: &Item) -> bool {
    !settled(item.outcome)
        && (needs_decision(item.outcome) || matches!(item.action.as_str(), "next" | "review"))
}

/// The inbox, everything not settled, in groups by what it needs, each
/// sorted with the most recently changed first.
pub fn inbox_groups(items: &[Item]) -> Vec<(&'static str, Vec<&Item>)> {
    let mut groups: Vec<(&'static str, Vec<&Item>)> = [
        "Needs a decision",
        "Next actions",
        "Reviews",
        "Waiting",
        "Other",
    ]
    .into_iter()
    .map(|name| (name, vec![]))
    .collect();
    for item in items.iter().filter(|i| !settled(i.outcome)) {
        let group = if needs_decision(item.outcome) {
            0
        } else {
            match item.action.as_str() {
                "next" => 1,
                "review" => 2,
                "waiting" => 3,
                _ => 4,
            }
        };
        groups[group].1.push(item);
    }
    for (_, items) in &mut groups {
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
    }
    groups.retain(|(_, items)| !items.is_empty());
    groups
}

/// How a connection's syncs go, and whether that needs the person's
/// attention, as the page says it.
pub fn sync_status(c: &Connection, now: i64) -> (String, bool) {
    let s = &c.sync;
    if c.from_hub {
        let synced = if c.schedule.stale {
            "Stale: synced"
        } else {
            "Synced"
        };
        let last = s.last_success.as_deref().map_or("never yet".into(), |t| {
            format!("last {}", relative_to(t, now))
        });
        let failed = if s.error_kind.is_some() {
            "; its last sync there failed"
        } else {
            ""
        };
        return (
            format!("{synced} by the hub, {last}{failed}. Read-only here; manage it on the hub."),
            c.schedule.stale || s.error_kind.is_some(),
        );
    }
    let last = match &s.last_success {
        Some(t) => format!(
            "{} synced {}.",
            if c.schedule.stale {
                "Stale: last"
            } else {
                "Last"
            },
            relative_to(t, now)
        ),
        None => "Never synced.".into(),
    };
    let next = match s.next_due.as_deref().and_then(parse_time) {
        None => "soon".into(),
        // Past due: the schedule starts it within moments.
        Some(t) if t <= now => "due now".into(),
        Some(t) => relative(t, now),
    };
    if c.schedule.needs_user {
        let error = s.error.as_deref().unwrap_or("credential rejected");
        return (
            format!("Needs you: {error}. Scheduled syncs stopped until the credential works; fix it, then verify again. {last}"),
            true,
        );
    }
    if c.schedule.running {
        return (format!("Syncing now. {last}"), false);
    }
    let Some(error) = &s.error else {
        return (
            format!(
                "{last} Next sync {next}, every {}.",
                every(c.schedule.interval_secs)
            ),
            c.schedule.stale && s.last_success.is_some(),
        );
    };
    let held = s
        .held_until
        .as_deref()
        .and_then(parse_time)
        .is_some_and(|t| t > now);
    let text = if held {
        format!("{error}. The source asked to wait; next try {next}. {last}")
    } else {
        let syncs = if s.failures == 1 { "sync" } else { "syncs" };
        format!(
            "{error}. {} failed {syncs} in a row, backing off; next try {next}. {last}",
            s.failures
        )
    };
    (text, true)
}

/// Whether a connection can be verified again from here: its own, with a
/// credential that was rejected.
pub fn can_verify(c: &Connection) -> bool {
    !c.from_hub && (c.schedule.needs_user || c.sync.error_kind == Some(ErrorKind::Auth))
}

/// How the pulls from the hub go, and whether that needs attention.
pub fn hub_status(h: &HubStatus, now: i64) -> (String, bool) {
    let last = h
        .sync
        .last_success
        .as_deref()
        .map_or("Never pulled.".into(), |t| {
            format!("Last pulled {}.", relative_to(t, now))
        });
    let attention = h.schedule.stale || h.schedule.needs_user;
    if h.schedule.needs_user {
        let error = h
            .sync
            .error
            .as_deref()
            .unwrap_or("the hub refused the token");
        return (format!("Needs you: {error}. Pulls stopped until you run `workboard hub set` with its current companion token. {last}"), true);
    }
    if let Some(error) = &h.sync.error {
        return (
            format!("{error}. Its work is stale until a pull works again. {last}"),
            true,
        );
    }
    let stale = if h.schedule.stale { "Stale: " } else { "" };
    (format!("{stale}{last}"), attention)
}

/// Whether data last synced at `at` is past the stale threshold of
/// something that syncs every `interval` seconds, at `now`.
fn past_threshold(interval: u64, at: Option<&str>, now: i64) -> bool {
    let interval = i64::try_from(interval).unwrap_or(i64::MAX);
    at.and_then(parse_time)
        .is_none_or(|t| now - t > STALE_INTERVALS.saturating_mul(interval))
}

/// The board as of `now`, as the page ages what it shows: the service marks
/// staleness when it answers, and this keeps the marks true while the app
/// shows that answer. Everything synced is stale once the service can't be
/// reached (`offline`), and a connection's work once its last success is
/// past the threshold. The hub's work is stale once its last pull is.
pub fn age(board: &Board, offline: bool, now: i64) -> Board {
    let mut board = board.clone();
    let snapshot = &mut board.snapshot;
    let hub_stale = offline
        || snapshot.hub.as_ref().is_none_or(|h| {
            h.schedule.stale
                || past_threshold(
                    h.schedule.interval_secs,
                    h.sync.last_success.as_deref(),
                    now,
                )
        });
    if let Some(hub) = &mut snapshot.hub {
        hub.schedule.stale = hub_stale;
    }
    for c in &mut snapshot.connections {
        c.schedule.stale = if c.from_hub {
            hub_stale || c.schedule.stale
        } else {
            offline
                || c.schedule.stale
                || past_threshold(
                    c.schedule.interval_secs,
                    c.sync.last_success.as_deref(),
                    now,
                )
        };
    }
    let stale: HashMap<&str, bool> = snapshot
        .connections
        .iter()
        .map(|c| (c.id.as_str(), c.schedule.stale))
        .collect();
    for item in &mut snapshot.items {
        if let Some(id) = &item.connection_id {
            item.stale |= stale.get(id.as_str()).copied().unwrap_or(true);
        }
    }
    // Each Mine row by every connection that found it.
    let mine = &mut board.mine;
    for row in mine.mine.iter_mut().chain(mine.watching.iter_mut()) {
        for w in &mut row.waits_on {
            w.stale |= stale.get(w.connection_id.as_str()).copied().unwrap_or(true);
        }
        row.stale |= offline || row.waits_on.iter().any(|w| w.stale);
    }
    board
}

/// `board` without the times it tells, which only change texts that the
/// views update in place, such as "3 min ago". Whether a time is known
/// stays, since that changes what is shown. Two boards that differ only in
/// their times have the same shape, and a view needn't be rebuilt.
pub fn structural(board: &Board) -> Board {
    let known = |t: &mut Option<String>| {
        if let Some(t) = t {
            t.clear();
        }
    };
    let mut board = board.clone();
    let snapshot = &mut board.snapshot;
    let syncs = snapshot
        .connections
        .iter_mut()
        .map(|c| &mut c.sync)
        .chain(snapshot.hub.as_mut().map(|h| &mut h.sync));
    for s in syncs {
        known(&mut s.last_success);
        known(&mut s.next_due);
        known(&mut s.held_until);
    }
    for i in &mut snapshot.items {
        known(&mut i.synced_at);
    }
    for row in board
        .mine
        .mine
        .iter_mut()
        .chain(board.mine.watching.iter_mut())
    {
        known(&mut row.synced_at);
    }
    for agents in board.bots.iter_mut().filter_map(|v| v.agents.as_mut()) {
        known(&mut agents.last_success);
        if let Some(h) = &mut agents.heartbeat {
            h.updated_at.clear();
            known(&mut h.next_wake_at);
            for r in &mut h.runs {
                r.started_at.clear();
            }
        }
    }
    board
}

/// The bot's items that wait on the person: those its board marks Needs
/// human or Draft, still open.
pub fn bot_waiting(snapshot: &Snapshot) -> Vec<&Item> {
    let mut items: Vec<&Item> = snapshot
        .items
        .iter()
        .filter(|i| {
            i.provider == "bot" && !settled(i.outcome) && i.reasons.contains(&Reason::BotWaiting)
        })
        .collect();
    items.sort_by(|a, b| {
        let priority = |i: &Item| {
            i.bot
                .as_ref()
                .and_then(|b| b.priority.clone())
                .unwrap_or_else(|| "P9".into())
        };
        priority(a)
            .cmp(&priority(b))
            .then(b.updated_at.cmp(&a.updated_at))
    });
    items
}

/// The name of an item's connection, as the person labeled it, or its
/// source's name for a manual item.
pub fn source_label<'a>(
    snapshot: &'a Snapshot,
    item_connection: Option<&str>,
    provider_id: &'a str,
) -> &'a str {
    item_connection
        .and_then(|id| snapshot.connections.iter().find(|c| c.id == id))
        .map(|c| c.label.as_str())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| provider(provider_id).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Mine, MineRow, Schedule, SyncState, WaitsOn};

    const NOW: &str = "2026-10-12T12:00:00Z";

    fn now() -> i64 {
        parse_time(NOW).unwrap()
    }

    #[test]
    fn relative_times() {
        let now = now();
        for (offset, expected) in [
            (0, "within a minute"),
            (30, "within a minute"),
            (-30, "just now"),
            (-60, "1 min ago"),
            (4 * 60, "in 4 min"),
            (-3 * 3600, "3 h ago"),
            (-7199, "119 min ago"),
            (-3 * 86_400, "3 d ago"),
        ] {
            assert_eq!(relative(now + offset, now), expected, "{offset}");
        }
        assert_eq!(relative_to("2026-10-12T11:58:00Z", now), "2 min ago");
        assert_eq!(relative_to("not a time", now), "not a time");
    }

    #[test]
    fn intervals() {
        for (seconds, expected) in [
            (300, "5m"),
            (3600, "1h"),
            (86_400 * 7, "7d"),
            (90, "90s"),
            (0, "0s"),
        ] {
            assert_eq!(every(seconds), expected);
        }
    }

    #[test]
    fn outcomes_look_as_on_the_page() {
        for (outcome, tone, label) in [
            (Outcome::Open, Tone::Open, "Open"),
            (Outcome::Delivered, Tone::Delivered, "Delivered"),
            (
                Outcome::ClosedWithoutDelivery,
                Tone::Attention,
                "Closed, not delivered",
            ),
            (Outcome::Unknown, Tone::Attention, "Unknown"),
            (Outcome::Abandoned, Tone::Abandoned, "Abandoned"),
        ] {
            assert_eq!(Tone::of(outcome), tone, "{outcome:?}");
            assert_eq!(outcome_label(outcome), label);
        }
        assert_eq!(reason_label(&Reason::BotWaiting), "Bot waiting on you");
        assert_eq!(
            reason_label(&Reason::Other("asked_twice".into())),
            "Asked twice"
        );
    }

    fn item(id: &str, outcome: Outcome, action: &str, updated_at: &str) -> Item {
        Item {
            id: id.into(),
            outcome,
            action: action.into(),
            updated_at: updated_at.into(),
            ..Item::default()
        }
    }

    #[test]
    fn next_and_inbox() {
        let items = [
            item("next", Outcome::Open, "next", "1"),
            item("review", Outcome::Open, "review", "2"),
            item("waiting", Outcome::Open, "waiting", "3"),
            item("closed", Outcome::ClosedWithoutDelivery, "", "4"),
            item("unknown", Outcome::Unknown, "next", "5"),
            item("delivered", Outcome::Delivered, "", "6"),
            item("abandoned", Outcome::Abandoned, "", "7"),
            item("none", Outcome::Open, "", "8"),
            item("next2", Outcome::Open, "next", "9"),
        ];
        let next: Vec<_> = items
            .iter()
            .filter(|i| is_next(i))
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(next, ["next", "review", "closed", "unknown", "next2"]);
        let groups: Vec<(&str, Vec<&str>)> = inbox_groups(&items)
            .into_iter()
            .map(|(name, items)| (name, items.iter().map(|i| i.id.as_str()).collect()))
            .collect();
        assert_eq!(
            groups,
            [
                ("Needs a decision", vec!["unknown", "closed"]),
                ("Next actions", vec!["next2", "next"]),
                ("Reviews", vec!["review"]),
                ("Waiting", vec!["waiting"]),
                ("Other", vec!["none"]),
            ]
        );
    }

    #[test]
    fn progress() {
        let p = Progress {
            delivered: 1,
            total: 5,
            open: 3,
            needs_attention: 1,
            abandoned: 1,
        };
        assert_eq!(
            progress_text(&p),
            "3 open · 1 needs attention · 1 abandoned"
        );
        assert_eq!(delivered_fraction(&p), 0.2);
        assert_eq!(progress_text(&Progress::default()), "Nothing remaining");
        assert_eq!(delivered_fraction(&Progress::default()), 0.0);
    }

    fn connection(sync: SyncState, schedule: Schedule) -> Connection {
        Connection {
            id: "c".into(),
            sync,
            schedule,
            ..Connection::default()
        }
    }

    #[test]
    fn sync_statuses() {
        let now = now();
        let fresh = Schedule {
            interval_secs: 300,
            running: false,
            needs_user: false,
            stale: false,
        };
        let synced = SyncState {
            last_success: Some("2026-10-12T11:58:00Z".into()),
            next_due: Some("2026-10-12T12:03:00Z".into()),
            ..SyncState::default()
        };
        let failing = SyncState {
            failures: 2,
            error: Some("Try later".into()),
            error_kind: Some(ErrorKind::Transient),
            ..synced.clone()
        };
        let cases = [
            (
                connection(synced.clone(), fresh.clone()),
                "Last synced 2 min ago. Next sync in 3 min, every 5m.",
                false,
            ),
            (
                connection(
                    synced.clone(),
                    Schedule {
                        running: true,
                        ..fresh.clone()
                    },
                ),
                "Syncing now. Last synced 2 min ago.",
                false,
            ),
            (
                connection(failing.clone(), fresh.clone()),
                "Try later. 2 failed syncs in a row, backing off; next try in 3 min. Last synced 2 min ago.",
                true,
            ),
            (
                connection(
                    SyncState {
                        held_until: Some("2026-10-12T12:30:00Z".into()),
                        ..failing.clone()
                    },
                    fresh.clone(),
                ),
                "Try later. The source asked to wait; next try in 3 min. Last synced 2 min ago.",
                true,
            ),
            (
                connection(
                    SyncState {
                        error: Some("No access".into()),
                        error_kind: Some(ErrorKind::Auth),
                        next_due: None,
                        ..synced.clone()
                    },
                    Schedule {
                        needs_user: true,
                        ..fresh.clone()
                    },
                ),
                "Needs you: No access. Scheduled syncs stopped until the credential works; fix it, then verify again. Last synced 2 min ago.",
                true,
            ),
            (
                connection(SyncState::default(), Schedule::default()),
                "Never synced. Next sync soon, every 0s.",
                false,
            ),
        ];
        for (c, text, attention) in cases {
            assert_eq!(sync_status(&c, now), (text.to_string(), attention));
        }
        let mut hub = connection(synced, fresh);
        hub.from_hub = true;
        assert_eq!(
            sync_status(&hub, now).0,
            "Synced by the hub, last 2 min ago. Read-only here; manage it on the hub."
        );
        assert!(!can_verify(&hub));
    }

    #[test]
    fn verifying_again() {
        let mut c = connection(SyncState::default(), Schedule::default());
        assert!(!can_verify(&c));
        c.schedule.needs_user = true;
        assert!(can_verify(&c));
        c.from_hub = true;
        assert!(!can_verify(&c));
    }

    #[test]
    fn hub_statuses() {
        let now = now();
        let mut hub = HubStatus {
            url: "https://hub.example.ts.net/".into(),
            sync: SyncState {
                last_success: Some("2026-10-12T11:55:00Z".into()),
                ..SyncState::default()
            },
            schedule: Schedule {
                interval_secs: 300,
                stale: false,
                ..Schedule::default()
            },
            ..HubStatus::default()
        };
        assert_eq!(
            hub_status(&hub, now),
            ("Last pulled 5 min ago.".into(), false)
        );
        hub.sync.error = Some("Cannot reach the hub".into());
        hub.schedule.stale = true;
        assert_eq!(
            hub_status(&hub, now),
            (
                "Cannot reach the hub. Its work is stale until a pull works again. Last pulled 5 min ago.".into(),
                true
            )
        );
        hub.schedule.needs_user = true;
        assert!(hub_status(&hub, now)
            .0
            .starts_with("Needs you: Cannot reach the hub."));
    }

    #[test]
    fn aging_marks_stale_work() {
        let now = now();
        let schedule = Schedule {
            interval_secs: 300,
            stale: false,
            ..Schedule::default()
        };
        let at = |t: &str| SyncState {
            last_success: Some(t.into()),
            ..SyncState::default()
        };
        let mut board = Board::default();
        board.snapshot.connections = vec![
            Connection {
                id: "fresh".into(),
                sync: at("2026-10-12T11:58:00Z"),
                schedule: schedule.clone(),
                ..Connection::default()
            },
            // Past three intervals.
            Connection {
                id: "old".into(),
                sync: at("2026-10-12T11:40:00Z"),
                schedule: schedule.clone(),
                ..Connection::default()
            },
            Connection {
                id: "hub:c".into(),
                sync: at("2026-10-12T11:58:00Z"),
                schedule: schedule.clone(),
                from_hub: true,
                ..Connection::default()
            },
        ];
        board.snapshot.hub = Some(HubStatus {
            sync: at("2026-10-12T11:58:00Z"),
            schedule: schedule.clone(),
            ..HubStatus::default()
        });
        board.snapshot.items = ["fresh", "old", "hub:c"]
            .iter()
            .map(|c| Item {
                id: format!("{c}-1"),
                connection_id: Some(c.to_string()),
                ..Item::default()
            })
            .chain([Item {
                id: "manual".into(),
                ..Item::default()
            }])
            .collect();
        board.mine = Mine {
            mine: vec![MineRow {
                id: "fresh-1".into(),
                waits_on: vec![WaitsOn {
                    connection_id: "fresh".into(),
                    ..WaitsOn::default()
                }],
                ..MineRow::default()
            }],
            ..Mine::default()
        };
        let stale = |b: &Board| -> Vec<bool> {
            b.snapshot.items.iter().map(|i| i.stale).collect::<Vec<_>>()
        };
        let aged = age(&board, false, now);
        assert_eq!(stale(&aged), [false, true, false, false]);
        assert!(!aged.mine.mine[0].stale);
        // The hub's last pull is past its threshold.
        let later = now + 20 * 60;
        let aged = age(&board, false, later);
        assert_eq!(stale(&aged), [true, true, true, false]);
        assert!(aged.snapshot.hub.as_ref().unwrap().schedule.stale);
        // Offline: everything synced.
        let aged = age(&board, true, now);
        assert_eq!(stale(&aged), [true, true, true, false]);
        assert!(aged.mine.mine[0].stale);
    }

    #[test]
    fn times_alone_dont_change_the_shape() {
        let mut board = Board::default();
        board.snapshot.connections = vec![Connection {
            id: "c".into(),
            sync: SyncState {
                last_success: Some("2026-10-12T11:58:00Z".into()),
                next_due: Some("2026-10-12T12:03:00Z".into()),
                ..SyncState::default()
            },
            ..Connection::default()
        }];
        board.snapshot.items = vec![Item {
            id: "i".into(),
            synced_at: Some("2026-10-12T11:58:00Z".into()),
            ..Item::default()
        }];
        let mut later = board.clone();
        later.snapshot.connections[0].sync.last_success = Some("2026-10-12T12:03:00Z".into());
        later.snapshot.connections[0].sync.next_due = Some("2026-10-12T12:08:00Z".into());
        later.snapshot.items[0].synced_at = Some("2026-10-12T12:03:00Z".into());
        assert_eq!(structural(&board), structural(&later));
        // What is shown otherwise does.
        let mut failed = later.clone();
        failed.snapshot.connections[0].sync.error = Some("Try later".into());
        assert_ne!(structural(&later), structural(&failed));
        let mut stale = later.clone();
        stale.snapshot.items[0].stale = true;
        assert_ne!(structural(&later), structural(&stale));
        // And so does a time becoming known, or unknown.
        let mut never = later.clone();
        never.snapshot.connections[0].sync.last_success = None;
        assert_ne!(structural(&later), structural(&never));
    }

    #[test]
    fn bot_items_waiting_on_the_person() {
        let bot = |id: &str, priority: &str, reasons: Vec<Reason>, outcome: Outcome| Item {
            id: id.into(),
            provider: "bot".into(),
            outcome,
            reasons,
            bot: Some(crate::api::BotFields {
                priority: Some(priority.into()),
                ..Default::default()
            }),
            ..Item::default()
        };
        let snapshot = Snapshot {
            items: vec![
                bot("p2", "P2", vec![Reason::BotWaiting], Outcome::Open),
                bot("p0", "P0", vec![Reason::BotWaiting], Outcome::Open),
                bot("running", "P0", vec![], Outcome::Open),
                bot("done", "P0", vec![Reason::BotWaiting], Outcome::Delivered),
                Item {
                    id: "github".into(),
                    provider: "github".into(),
                    reasons: vec![Reason::BotWaiting],
                    ..Item::default()
                },
            ],
            ..Snapshot::default()
        };
        let ids: Vec<_> = bot_waiting(&snapshot)
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(ids, ["p0", "p2"]);
    }

    #[test]
    fn staleness() {
        assert_eq!(stale_text(false, None, 0), None);
        assert_eq!(stale_text(true, None, 0).unwrap(), "Stale: not synced");
    }
}
