//! The connector interface: what Workboard asks of a source (verify the
//! account, list the items relevant to it, refresh tracked items) and the
//! typed errors it can answer with. Built-in connectors implement
//! [`Connector`] in process; the same types are the versioned JSON protocol
//! that out-of-process plugins speak, described in `docs/plugins.md`.
//!
//! Everything a connector returns is checked here before it becomes an
//! [`Item`], so a plugin can't store an unsafe link or a person's decision.

use crate::{
    connectors::source_url,
    model::{Connection, Item, Outcome},
    outcome,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, future::Future, pin::Pin};
use url::Url;

/// The plugin protocol version this build speaks. A change that an existing
/// plugin or host could misread takes a new version; adding an optional
/// field does not.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest credential accepted, in bytes.
const MAX_CREDENTIAL: usize = 16384;
/// Longest text field accepted from a connector, in bytes.
const MAX_FIELD: usize = 8192;
/// Longest remote identity accepted from a connector, in bytes.
const MAX_REMOTE_ID: usize = 256;
/// Attention categories a connector may give an item.
const ACTIONS: &[&str] = &["next", "review", "waiting", "follow-up", ""];
/// The attention category of tracked items fetched again by `refresh`.
const FOLLOW_UP: &str = "follow-up";
/// What replaces a credential that a message echoes.
const REDACTED: &str = "[redacted]";

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What went wrong, so callers can tell a credential to fix from a failure to
/// retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The credential was rejected, lacks a permission, or belongs to another
    /// account. A person has to fix the credential.
    Auth,
    /// Network, timeout, rate limit or server error. Retrying later may work.
    Transient,
    /// One tracked item is missing or hidden from the account. Only valid for
    /// an item in a `refresh` answer; the item becomes unknown, never done.
    Inaccessible,
    /// The source or plugin answered something unusable, or the request
    /// can't be done as asked. Retrying won't help until something changes.
    Invalid,
}

/// A connector's error. Its message is shown to the person, so it never
/// carries a credential or a remote response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ConnectorError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn invalid(message: impl fmt::Display) -> Self {
        Self::new(ErrorKind::Invalid, message.to_string())
    }
    /// The same error with every copy of `credential` in its message
    /// replaced, since a source or plugin can echo it back.
    pub fn redact(mut self, credential: &Credential) -> Self {
        self.message = credential.redact(&self.message);
        self
    }
}

impl fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConnectorError {}

pub type ConnectorResult<T> = Result<T, ConnectorError>;

/// A provider credential. It is handed to a connector for one call and is
/// never printed.
#[derive(Clone)]
pub struct Credential(String);

impl Credential {
    pub fn new(value: &str) -> ConnectorResult<Self> {
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_CREDENTIAL || value.chars().any(char::is_control) {
            return Err(ConnectorError::new(ErrorKind::Auth, "Invalid token"));
        }
        Ok(Self(value.into()))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    /// `text` with every copy of this credential replaced.
    pub fn redact(&self, text: &str) -> String {
        text.replace(&self.0, REDACTED)
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential(..)")
    }
}

/// The account a connector is asked about: where, and with which
/// credential.
#[derive(Debug, Clone)]
pub struct Target {
    /// A canonical HTTPS base URL, ending in a slash.
    pub base_url: Url,
    pub credential: Credential,
}

impl Target {
    pub fn new(base_url: &str, credential: &str) -> anyhow::Result<Self> {
        Ok(Self {
            base_url: Url::parse(&crate::connectors::base_url(base_url)?)?,
            credential: Credential::new(credential)?,
        })
    }
}

/// The account a credential belongs to. `id` is stable on its instance, such
/// as a numeric user ID, never a display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
}

/// An item as its source reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteItem {
    /// Stable on the instance; part of the local item ID.
    pub remote_id: String,
    /// The reference people use, such as `RHEL-1` or `group/project!4`.
    pub key: String,
    pub title: String,
    /// The item's own web page.
    pub url: String,
    /// The source's own status, unmodified.
    pub status: String,
    /// What the status means for the work. Never `abandoned`: that is a
    /// person's decision, kept in Workboard.
    pub outcome: Outcome,
    /// Why the outcome was chosen.
    pub outcome_reason: String,
    /// Attention category while the item is open: next, review, waiting or
    /// follow-up.
    pub action: String,
    /// When the source last changed the item.
    pub updated_at: String,
}

/// A tracked item to fetch again: it was open or unknown, and `list` no
/// longer returned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tracked {
    pub remote_id: String,
    pub key: String,
}

/// The answer for one tracked item: the item, or why it can't be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refreshed {
    /// The tracked item's `remote_id`.
    pub remote_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<RemoteItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ConnectorError>,
}

impl Refreshed {
    pub fn found(remote_id: &str, item: RemoteItem) -> Self {
        Self {
            remote_id: remote_id.into(),
            item: Some(item),
            error: None,
        }
    }
    pub fn inaccessible(remote_id: &str, message: impl Into<String>) -> Self {
        Self {
            remote_id: remote_id.into(),
            item: None,
            error: Some(ConnectorError::new(ErrorKind::Inaccessible, message)),
        }
    }
}

/// A source of work items. Each call gets the target account and its
/// credential; a connector keeps no credential between calls.
pub trait Connector: Send + Sync {
    /// The namespace of this connector's local item IDs, such as `gitlab`.
    /// Stored item IDs contain it, so it never changes.
    fn kind(&self) -> &str;
    /// The account the credential belongs to.
    fn verify<'a>(&'a self, target: &'a Target) -> BoxFuture<'a, ConnectorResult<Account>>;
    /// Every item relevant to the account, complete or not at all.
    fn list<'a>(
        &'a self,
        target: &'a Target,
        account: &'a Account,
    ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>>;
    /// Fetch tracked items again, one answer each. A missing or hidden item
    /// answers `inaccessible`; any other error fails the whole call.
    fn refresh<'a>(
        &'a self,
        target: &'a Target,
        account: &'a Account,
        tracked: &'a [Tracked],
    ) -> BoxFuture<'a, ConnectorResult<Vec<Refreshed>>>;
}

fn check_text(field: &str, value: &str) -> ConnectorResult<()> {
    if value.len() > MAX_FIELD {
        return Err(ConnectorError::invalid(format!(
            "The connector returned an overlong {field}"
        )));
    }
    Ok(())
}

fn check_remote_id(value: &str) -> ConnectorResult<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_REMOTE_ID
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:~@+=".contains(c));
    if !valid {
        return Err(ConnectorError::invalid(
            "The connector returned an invalid item identity",
        ));
    }
    Ok(())
}

/// Turn what a connector reported into a local item of connection `c`,
/// checking it on the way.
pub fn to_item(c: &Connection, kind: &str, remote: RemoteItem) -> ConnectorResult<Item> {
    check_remote_id(&remote.remote_id)?;
    for (field, value) in [
        ("key", &remote.key),
        ("title", &remote.title),
        ("status", &remote.status),
        ("outcome reason", &remote.outcome_reason),
        ("update time", &remote.updated_at),
    ] {
        check_text(field, value)?;
    }
    if !ACTIONS.contains(&remote.action.as_str()) {
        return Err(ConnectorError::invalid(
            "The connector returned an unknown attention category",
        ));
    }
    if remote.outcome == Outcome::Abandoned {
        return Err(ConnectorError::invalid(
            "A connector cannot mark an item abandoned; that is a person's decision",
        ));
    }
    let url = source_url(&remote.url).map_err(ConnectorError::invalid)?;
    let mut item = Item {
        id: format!("{}:{kind}:{}", c.id, remote.remote_id),
        connection_id: Some(c.id.clone()),
        provider: c.provider.clone(),
        remote_id: remote.remote_id,
        key: remote.key,
        title: remote.title,
        url,
        status: remote.status,
        outcome: Outcome::Open,
        outcome_reason: String::new(),
        action: remote.action,
        updated_at: remote.updated_at,
        effort_id: None,
        abandoned_at: None,
        demo: false,
        zone: c.zone,
    };
    item.set_outcome((remote.outcome, remote.outcome_reason));
    Ok(item)
}

/// Ask `connector` whose account the credential belongs to. The account is
/// stored and shown, so an answer that repeats the credential is refused,
/// and an error never repeats it.
pub async fn verify(connector: &dyn Connector, target: &Target) -> ConnectorResult<Account> {
    let account = connector
        .verify(target)
        .await
        .map_err(|e| e.redact(&target.credential))?;
    let credential = target.credential.expose();
    if account.id.contains(credential) || account.name.contains(credential) {
        return Err(ConnectorError::invalid(
            "The connector returned an account containing the credential",
        ));
    }
    check_text("account ID", &account.id)?;
    check_text("account name", &account.name)?;
    Ok(account)
}

/// Sync connection `c`: check that the credential still belongs to its
/// account, list the account's items, and fetch tracked items the listing no
/// longer has, so a merge or a finished ticket can advance its effort. An
/// item the source no longer shows becomes unknown, never done; any other
/// failure fails the whole sync, so no partial result is saved. No error or
/// item carries the credential.
pub async fn sync(
    connector: &dyn Connector,
    target: &Target,
    c: &Connection,
    old: &[Item],
) -> ConnectorResult<Vec<Item>> {
    sync_checked(connector, target, c, old)
        .await
        .map_err(|e| e.redact(&target.credential))
}

async fn sync_checked(
    connector: &dyn Connector,
    target: &Target,
    c: &Connection,
    old: &[Item],
) -> ConnectorResult<Vec<Item>> {
    // Rotating a credential must not change the linked account.
    let account = verify(connector, target).await?;
    if account.id != c.account_id {
        return Err(ConnectorError::new(
            ErrorKind::Auth,
            "Credential belongs to another account; create a separate connection",
        ));
    }
    let kind = connector.kind();
    let checked = |remote: RemoteItem| {
        let credential = target.credential.expose();
        let echoed = [
            &remote.remote_id,
            &remote.key,
            &remote.title,
            &remote.url,
            &remote.status,
            &remote.outcome_reason,
            &remote.action,
            &remote.updated_at,
        ]
        .iter()
        .any(|field| field.contains(credential));
        if echoed {
            return Err(ConnectorError::invalid(
                "The connector returned an item containing the credential",
            ));
        }
        to_item(c, kind, remote)
    };
    let mut all = BTreeMap::<String, Item>::new();
    for remote in connector.list(target, &account).await? {
        let item = checked(remote)?;
        if all.insert(item.id.clone(), item).is_some() {
            return Err(ConnectorError::invalid(
                "The connector listed the same item twice",
            ));
        }
    }
    let missing: Vec<_> = old
        .iter()
        .filter(|i| {
            i.connection_id.as_deref() == Some(&c.id)
                && matches!(i.outcome, Outcome::Open | Outcome::Unknown)
                && !all.contains_key(&i.id)
        })
        .collect();
    if missing.is_empty() {
        return Ok(all.into_values().collect());
    }
    let tracked: Vec<_> = missing
        .iter()
        .map(|i| Tracked {
            remote_id: i.remote_id.clone(),
            key: i.key.clone(),
        })
        .collect();
    let mut answers = BTreeMap::new();
    for answer in connector.refresh(target, &account, &tracked).await? {
        let remote_id = answer.remote_id.clone();
        if answers.insert(remote_id, answer).is_some() {
            return Err(ConnectorError::invalid(
                "The connector answered for the same item twice",
            ));
        }
    }
    for old in missing {
        let answer = answers.remove(&old.remote_id).ok_or_else(|| {
            ConnectorError::invalid("The connector did not answer for every tracked item")
        })?;
        let next = match (answer.item, answer.error) {
            (Some(mut remote), None) => {
                // Otherwise the tracked item would stay as it was, and the
                // answer could overwrite another item.
                if remote.remote_id != old.remote_id {
                    return Err(ConnectorError::invalid(
                        "The connector answered a tracked item with a different item",
                    ));
                }
                remote.action = FOLLOW_UP.into();
                checked(remote)?
            }
            (None, Some(e)) if e.kind == ErrorKind::Inaccessible => {
                check_text("error", &e.message)?;
                let mut gone = old.clone();
                let message = target.credential.redact(&e.message);
                gone.set_outcome(outcome::inaccessible(&message));
                gone
            }
            (None, Some(e)) => return Err(e),
            _ => {
                return Err(ConnectorError::invalid(
                    "The connector answered a tracked item with both or neither of an item and an error",
                ))
            }
        };
        all.insert(next.id.clone(), next);
    }
    if !answers.is_empty() {
        return Err(ConnectorError::invalid(
            "The connector answered for an item that was not asked for",
        ));
    }
    Ok(all.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Zone;
    use std::sync::Mutex;

    /// A connector answering from memory, recording what it was asked to
    /// refresh.
    struct Fake {
        account: ConnectorResult<Account>,
        items: ConnectorResult<Vec<RemoteItem>>,
        answers: ConnectorResult<Vec<Refreshed>>,
        asked: Mutex<Vec<Vec<Tracked>>>,
    }
    impl Fake {
        fn new(items: Vec<RemoteItem>, answers: Vec<Refreshed>) -> Self {
            Self {
                account: Ok(account()),
                items: Ok(items),
                answers: Ok(answers),
                asked: Mutex::default(),
            }
        }
    }
    impl Connector for Fake {
        fn kind(&self) -> &str {
            "fake"
        }
        fn verify<'a>(&'a self, _: &'a Target) -> BoxFuture<'a, ConnectorResult<Account>> {
            Box::pin(async move { self.account.clone() })
        }
        fn list<'a>(
            &'a self,
            _: &'a Target,
            _: &'a Account,
        ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
            Box::pin(async move { self.items.clone() })
        }
        fn refresh<'a>(
            &'a self,
            _: &'a Target,
            _: &'a Account,
            tracked: &'a [Tracked],
        ) -> BoxFuture<'a, ConnectorResult<Vec<Refreshed>>> {
            self.asked.lock().unwrap().push(tracked.to_vec());
            Box::pin(async move { self.answers.clone() })
        }
    }

    fn account() -> Account {
        Account {
            id: "42".into(),
            name: "me".into(),
        }
    }
    fn connection() -> Connection {
        Connection {
            id: "c1".into(),
            provider: "gitlab-public".into(),
            label: "Fake".into(),
            base_url: "https://example.com/".into(),
            account_id: "42".into(),
            account_name: "me".into(),
            last_sync: None,
            error: None,
            zone: Zone::External,
            error_kind: None,
        }
    }
    fn target() -> Target {
        Target::new("https://example.com", "test-only-token").unwrap()
    }
    fn remote(remote_id: &str, outcome: Outcome, action: &str) -> RemoteItem {
        RemoteItem {
            remote_id: remote_id.into(),
            key: format!("KEY-{remote_id}"),
            title: format!("Title {remote_id}"),
            url: format!("https://example.com/{remote_id}"),
            status: format!("{outcome:?}"),
            outcome,
            outcome_reason: format!("Fake says {outcome:?}"),
            action: action.into(),
            updated_at: "2026-10-12T00:00:00Z".into(),
        }
    }
    /// An item a previous sync stored.
    fn stored(remote_id: &str, outcome: Outcome) -> Item {
        to_item(&connection(), "fake", remote(remote_id, outcome, "next")).unwrap()
    }
    async fn run(fake: &Fake, old: &[Item]) -> ConnectorResult<Vec<Item>> {
        sync(fake, &target(), &connection(), old).await
    }

    #[tokio::test]
    async fn sync_lists_and_refreshes_only_missing_tracked_items() {
        use Outcome::*;
        let mut elsewhere = stored("elsewhere", Open);
        elsewhere.connection_id = Some("c2".into());
        elsewhere.id = "c2:fake:elsewhere".into();
        let old = [
            stored("listed", Open),
            stored("merged", Open),
            stored("gone", Unknown),
            stored("done", Delivered),
            elsewhere,
        ];
        let fake = Fake::new(
            vec![
                remote("listed", Open, "review"),
                remote("new", Delivered, "next"),
            ],
            vec![
                Refreshed::inaccessible("gone", "Fake returned HTTP 404 for this item"),
                Refreshed::found("merged", remote("merged", Delivered, "next")),
            ],
        );
        let items = run(&fake, &old).await.unwrap();
        // Only open or unknown items of this connection that the listing
        // lacks are fetched again.
        let asked: Vec<_> = fake.asked.lock().unwrap()[0]
            .iter()
            .map(|t| t.remote_id.clone())
            .collect();
        assert_eq!(asked, ["merged", "gone"]);
        let found: Vec<_> = items
            .iter()
            .map(|i| (i.id.as_str(), i.outcome, i.action.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                ("c1:fake:gone", Unknown, ""),
                ("c1:fake:listed", Open, "review"),
                ("c1:fake:merged", Delivered, ""),
                ("c1:fake:new", Delivered, ""),
            ]
        );
        let gone = &items[0];
        assert_eq!(gone.status, "Unknown", "the last known status stays");
        assert_eq!(
            gone.outcome_reason,
            "Fake returned HTTP 404 for this item; its last known status is shown"
        );
        // Items take their connection's provider and zone.
        assert!(items
            .iter()
            .all(|i| i.provider == "gitlab-public" && i.zone == Zone::External));
    }

    #[tokio::test]
    async fn sync_skips_refresh_when_nothing_is_missing() {
        let fake = Fake::new(vec![remote("a", Outcome::Open, "next")], vec![]);
        let items = run(&fake, &[stored("a", Outcome::Open)]).await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(fake.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sync_rejects_what_it_cannot_trust() {
        use ErrorKind::*;
        use Outcome::*;
        let tracked = [stored("t", Open)];
        let transient = ConnectorError::new(Transient, "Rate limited");
        let cases: Vec<(&str, Fake, ErrorKind)> = vec![
            (
                "another account",
                Fake {
                    account: Ok(Account {
                        id: "43".into(),
                        name: "me".into(),
                    }),
                    ..Fake::new(vec![], vec![])
                },
                Auth,
            ),
            (
                "verify fails",
                Fake {
                    account: Err(transient.clone()),
                    ..Fake::new(vec![], vec![])
                },
                Transient,
            ),
            (
                "list fails",
                Fake {
                    items: Err(transient.clone()),
                    ..Fake::new(vec![], vec![])
                },
                Transient,
            ),
            (
                "duplicate",
                Fake::new(
                    vec![remote("a", Open, "next"), remote("a", Open, "next")],
                    vec![],
                ),
                Invalid,
            ),
            (
                "unsafe URL",
                Fake::new(
                    vec![RemoteItem {
                        url: "javascript:alert(1)".into(),
                        ..remote("a", Open, "next")
                    }],
                    vec![],
                ),
                Invalid,
            ),
            (
                "abandoned",
                Fake::new(vec![remote("a", Abandoned, "")], vec![]),
                Invalid,
            ),
            (
                "attention",
                Fake::new(vec![remote("a", Open, "urgent")], vec![]),
                Invalid,
            ),
            (
                "identity",
                Fake::new(vec![remote("a/../b", Open, "next")], vec![]),
                Invalid,
            ),
            (
                "overlong",
                Fake::new(
                    vec![RemoteItem {
                        title: "x".repeat(MAX_FIELD + 1),
                        ..remote("a", Open, "next")
                    }],
                    vec![],
                ),
                Invalid,
            ),
            ("unanswered", Fake::new(vec![], vec![]), Invalid),
            (
                "refresh fails",
                Fake {
                    answers: Err(transient.clone()),
                    ..Fake::new(vec![], vec![])
                },
                Transient,
            ),
            (
                "one item fails",
                Fake::new(
                    vec![],
                    vec![Refreshed {
                        remote_id: "t".into(),
                        item: None,
                        error: Some(transient),
                    }],
                ),
                Transient,
            ),
            (
                "unasked",
                Fake::new(
                    vec![],
                    vec![
                        Refreshed::found("t", remote("t", Open, "next")),
                        Refreshed::found("u", remote("u", Open, "next")),
                    ],
                ),
                Invalid,
            ),
            (
                "another item",
                Fake::new(
                    vec![],
                    vec![Refreshed::found("t", remote("u", Open, "next"))],
                ),
                Invalid,
            ),
            (
                "credential in an item",
                Fake::new(
                    vec![RemoteItem {
                        title: "Echoes test-only-token".into(),
                        ..remote("a", Open, "next")
                    }],
                    vec![],
                ),
                Invalid,
            ),
            (
                "both",
                Fake::new(
                    vec![],
                    vec![Refreshed {
                        error: Some(ConnectorError::new(Inaccessible, "gone")),
                        ..Refreshed::found("t", remote("t", Open, "next"))
                    }],
                ),
                Invalid,
            ),
        ];
        for (name, fake, kind) in cases {
            let error = run(&fake, &tracked).await.unwrap_err();
            assert_eq!(error.kind, kind, "{name}: {error}");
        }
    }

    #[tokio::test]
    async fn verify_refuses_an_account_that_repeats_the_credential() {
        for (id, name) in [("42", "me test-only-token"), ("test-only-token", "me")] {
            let fake = Fake {
                account: Ok(Account {
                    id: id.into(),
                    name: name.into(),
                }),
                ..Fake::new(vec![], vec![])
            };
            let error = verify(&fake, &target()).await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::Invalid, "{error}");
            assert!(!error.message.contains("test-only-token"), "{error}");
            let error = run(&fake, &[]).await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::Invalid, "{error}");
        }
        assert_eq!(
            verify(&Fake::new(vec![], vec![]), &target()).await.unwrap(),
            account()
        );
    }

    #[tokio::test]
    async fn sync_never_repeats_the_credential() {
        let echo = "Denied for token test-only-token";
        let fake = Fake {
            account: Err(ConnectorError::new(ErrorKind::Auth, echo)),
            ..Fake::new(vec![], vec![])
        };
        let error = run(&fake, &[]).await.unwrap_err();
        assert_eq!(error.message, "Denied for token [redacted]");
        let fake = Fake::new(vec![], vec![Refreshed::inaccessible("t", echo)]);
        let items = run(&fake, &[stored("t", Outcome::Open)]).await.unwrap();
        assert_eq!(items[0].outcome, Outcome::Unknown);
        assert!(
            !items[0].outcome_reason.contains("test-only-token"),
            "{}",
            items[0].outcome_reason
        );
        assert!(items[0].outcome_reason.contains("[redacted]"));
    }

    #[test]
    fn protocol_json_shapes() {
        let answer = Refreshed::inaccessible("7:4", "Gone");
        assert_eq!(
            serde_json::to_value(&answer).unwrap(),
            serde_json::json!({"remote_id":"7:4","error":{"kind":"inaccessible","message":"Gone"}})
        );
        let item = remote("7:4", Outcome::ClosedWithoutDelivery, "");
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["outcome"], "closed_without_delivery");
        assert_eq!(serde_json::from_value::<RemoteItem>(json).unwrap(), item);
        let credential = Credential::new(" test-only-token\n").unwrap();
        assert_eq!(credential.expose(), "test-only-token");
        assert!(!format!("{:?}", target()).contains("test-only-token"));
        for bad in ["", "  ", "a\u{7}b"] {
            assert_eq!(Credential::new(bad).unwrap_err().kind, ErrorKind::Auth);
        }
    }
}
