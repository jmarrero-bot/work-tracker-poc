//! The Mine view: everything that waits on the account behind each
//! connection, each thing once with every reason it waits, per account; and,
//! apart from it, what watching another user found, which is never the
//! account's.

use crate::model::{Item, Outcome, Reason, Snapshot, WatchList, Zone};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One account a row waits on, through one connection, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitsOn {
    /// The connection's account, or the user it watches.
    pub account: String,
    pub connection_id: String,
    /// Why it waits on this account, once each.
    pub reasons: Vec<Reason>,
}

/// One thing that waits, and on whom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// The local ID of the first item found at its address.
    pub id: String,
    pub provider: String,
    pub key: String,
    pub title: String,
    pub url: String,
    /// The source's own status.
    pub status: String,
    pub outcome: Outcome,
    pub outcome_reason: String,
    /// Internal if any connection that found it is.
    pub zone: Zone,
    pub updated_at: String,
    /// Each account it waits on, with that account's reasons: a reason
    /// is never shown as another account's.
    pub waits_on: Vec<WaitsOn>,
}

/// Why a connection's watch list couldn't be updated by its last sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchError {
    pub connection_id: String,
    pub login: String,
    pub error: String,
}

/// The Mine view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mine {
    /// What waits on the connections' own accounts.
    pub mine: Vec<Row>,
    /// What waits on users the connections watch: theirs, never the
    /// accounts'.
    pub watching: Vec<Row>,
    /// Watch lists the last sync couldn't update; they show what an earlier
    /// one found.
    pub watch_errors: Vec<WatchError>,
}

/// Rows of `found`, each item waiting on an account, merged by address:
/// several connections or listings can report the same change.
fn rows<'a>(found: impl IntoIterator<Item = (&'a Item, &'a str)>) -> Vec<Row> {
    let mut rows = BTreeMap::<&str, Row>::new();
    for (item, account) in found {
        let Some(connection) = item.connection_id.as_deref() else {
            continue;
        };
        if item.reasons.is_empty() || item.outcome.is_settled() {
            continue;
        }
        let row = rows.entry(item.url.as_str()).or_insert_with(|| Row {
            id: item.id.clone(),
            provider: item.provider.clone(),
            key: item.key.clone(),
            title: item.title.clone(),
            url: item.url.clone(),
            status: item.status.clone(),
            outcome: item.outcome,
            outcome_reason: item.outcome_reason.clone(),
            zone: item.zone,
            updated_at: item.updated_at.clone(),
            waits_on: vec![],
        });
        if item.zone == Zone::Internal {
            row.zone = Zone::Internal;
        }
        let mut reasons = item.reasons.clone();
        reasons.sort();
        reasons.dedup();
        row.waits_on.push(WaitsOn {
            account: account.to_string(),
            connection_id: connection.to_string(),
            reasons,
        });
    }
    let mut rows: Vec<Row> = rows.into_values().collect();
    for row in &mut rows {
        row.waits_on
            .sort_by(|a, b| (&a.account, &a.connection_id).cmp(&(&b.account, &b.connection_id)));
    }
    // The most recently changed first.
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
    rows
}

/// What waits on each connection's account in `snapshot`, and what its
/// watch lists found, kept apart.
pub fn mine(snapshot: &Snapshot, watched: &[WatchList]) -> Mine {
    let accounts: BTreeMap<&str, &str> = snapshot
        .connections
        .iter()
        .map(|c| (c.id.as_str(), c.account_name.as_str()))
        .collect();
    let own = snapshot.items.iter().filter_map(|i| {
        let account = accounts.get(i.connection_id.as_deref()?)?;
        Some((i, *account))
    });
    let lists: Vec<_> = watched
        .iter()
        .filter(|w| accounts.contains_key(w.connection_id.as_str()))
        .collect();
    let theirs = lists
        .iter()
        .copied()
        .flat_map(|w| w.items.iter().map(move |i| (i, w.login.as_str())));
    Mine {
        mine: rows(own),
        watching: rows(theirs),
        watch_errors: lists
            .iter()
            .filter_map(|w| {
                Some(WatchError {
                    connection_id: w.connection_id.clone(),
                    login: w.login.clone(),
                    error: w.error.clone()?,
                })
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Connection;
    use Reason::*;

    fn connection(id: &str, account: &str, zone: Zone) -> Connection {
        Connection {
            id: id.into(),
            provider: "github".into(),
            label: id.into(),
            base_url: "https://github.com/".into(),
            account_id: "1".into(),
            account_name: account.into(),
            last_sync: None,
            error: None,
            zone,
            error_kind: None,
            watch_user: None,
        }
    }
    fn item(connection: &str, n: u32, reasons: Vec<Reason>, outcome: Outcome) -> Item {
        Item {
            id: format!("{connection}:github:{n}"),
            connection_id: Some(connection.into()),
            provider: "github".into(),
            remote_id: n.to_string(),
            key: format!("o/r#{n}"),
            title: format!("Change {n}"),
            url: format!("https://github.com/o/r/pull/{n}"),
            status: "open".into(),
            outcome,
            outcome_reason: String::new(),
            action: String::new(),
            updated_at: format!("2026-10-{:02}T00:00:00Z", n),
            effort_id: None,
            abandoned_at: None,
            demo: false,
            zone: Zone::External,
            reasons,
        }
    }
    fn snapshot(items: Vec<Item>) -> Snapshot {
        Snapshot {
            connections: vec![
                connection("a", "me", Zone::External),
                connection("b", "me-at-work", Zone::Internal),
            ],
            items,
            efforts: vec![],
        }
    }
    /// Each account a row waits on, and its reasons.
    type Accounts<'a> = Vec<(&'a str, Vec<Reason>)>;
    /// (key, accounts, zone) of each row.
    fn summary(rows: &[Row]) -> Vec<(&str, Accounts<'_>, Zone)> {
        rows.iter()
            .map(|r| {
                let waits_on = r
                    .waits_on
                    .iter()
                    .map(|w| (w.account.as_str(), w.reasons.clone()))
                    .collect();
                (r.key.as_str(), waits_on, r.zone)
            })
            .collect()
    }

    #[test]
    fn each_waiting_item_once_with_each_accounts_reasons() {
        use Outcome::*;
        let mut internal = item("b", 1, vec![Mentioned, Mentioned], Open);
        internal.zone = Zone::Internal;
        let mut manual = item("a", 6, vec![Assigned], Open);
        manual.connection_id = None;
        let s = snapshot(vec![
            item("a", 1, vec![ReviewRequested, Assigned], Open),
            // The same change seen from an internal connection's account.
            internal,
            item("a", 2, vec![ChecksFailing], ClosedWithoutDelivery),
            // Not waiting on anyone: only in the inbox.
            item("a", 3, vec![], Open),
            // Finished.
            item("a", 4, vec![Assigned], Delivered),
            item("a", 5, vec![Assigned], Abandoned),
            manual,
            // Its connection is gone.
            item("gone", 7, vec![Assigned], Open),
        ]);
        let view = mine(&s, &[]);
        assert_eq!(
            summary(&view.mine),
            [
                ("o/r#2", vec![("me", vec![ChecksFailing])], Zone::External),
                (
                    "o/r#1",
                    vec![
                        ("me", vec![ReviewRequested, Assigned]),
                        ("me-at-work", vec![Mentioned]),
                    ],
                    Zone::Internal
                ),
            ]
        );
        let connections: Vec<_> = view.mine[1]
            .waits_on
            .iter()
            .map(|w| w.connection_id.as_str())
            .collect();
        assert_eq!(connections, ["a", "b"]);
        assert!(view.watching.is_empty() && view.watch_errors.is_empty());
    }

    #[test]
    fn watched_items_are_never_the_accounts() {
        let watched = |connection: &str, login: &str, items, error: Option<&str>| WatchList {
            connection_id: connection.into(),
            login: login.into(),
            items,
            error: error.map(str::to_string),
        };
        let s = snapshot(vec![item("a", 1, vec![ReviewRequested], Outcome::Open)]);
        let lists = [
            // Also asked of the account: once in each list, never merged.
            watched(
                "a",
                "someone",
                vec![
                    item("a", 1, vec![ReviewRequested], Outcome::Open),
                    item("a", 2, vec![Authored], Outcome::Open),
                ],
                None,
            ),
            // The last sync couldn't update it, so it shows what an earlier
            // one found, and why.
            watched(
                "b",
                "someone-else",
                vec![item("b", 2, vec![Assigned], Outcome::Open)],
                Some("Watching someone-else: refused"),
            ),
            watched(
                "gone",
                "nobody",
                vec![item("gone", 3, vec![Assigned], Outcome::Open)],
                Some("gone"),
            ),
        ];
        let view = mine(&s, &lists);
        assert_eq!(
            summary(&view.mine),
            [("o/r#1", vec![("me", vec![ReviewRequested])], Zone::External)]
        );
        assert_eq!(
            summary(&view.watching),
            [
                (
                    "o/r#2",
                    vec![
                        ("someone", vec![Authored]),
                        ("someone-else", vec![Assigned])
                    ],
                    Zone::External
                ),
                (
                    "o/r#1",
                    vec![("someone", vec![ReviewRequested])],
                    Zone::External
                ),
            ]
        );
        assert_eq!(
            view.watch_errors,
            [WatchError {
                connection_id: "b".into(),
                login: "someone-else".into(),
                error: "Watching someone-else: refused".into(),
            }]
        );
    }
}
