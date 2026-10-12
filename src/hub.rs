//! The companion's side of a hub: a client that reads the hub's external
//! export and nothing else, the checks that what it read is external and
//! well formed, and how fresh the hub's work is on the companion.
//!
//! Nothing ever goes from the companion to the hub but that one read: the
//! client sends `GET` to the hub's export, exactly, and refuses everything
//! else before it is sent. Its one send is the only place it talks to the
//! hub, and clippy refuses the reqwest calls that could send anything else
//! anywhere in the crate.

use crate::{
    connector::{BoxFuture, ConnectorError, ConnectorResult, Credential, ErrorKind, Requests},
    connectors::{self, http_error, parse_json, read_body},
    model::{valid_provider, Connection, Schedule, SyncState},
    zone::{ExternalSnapshot, Zone},
};
use anyhow::{bail, Context, Result};
use reqwest::{
    header::{self, HeaderValue},
    Client, Method, Request, StatusCode,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::{Mutex, PoisonError},
};
use url::Url;

/// How the hub is named in messages and reports.
pub const LABEL: &str = "Hub";
/// Where a hub serves its external export, under its address.
const EXPORT: &str = "api/export/external";
const ACCEPT: &str = "application/json";
const USER_AGENT: &str = concat!("workboard/", env!("CARGO_PKG_VERSION"));
/// Longest text field accepted from a hub, in bytes, as from a connector.
const MAX_FIELD: usize = 8192;
/// Longest ID accepted from a hub, in bytes.
const MAX_ID: usize = 512;
/// Most connections, items or efforts accepted from a hub, each.
const MAX_COUNT: usize = 50_000;
/// Hosts a hub may be reached at over plain HTTP: this machine's loopback,
/// where its token never crosses a network.
const LOOPBACK: &[&str] = &["127.0.0.1", "[::1]", "localhost"];

/// A hub's address: HTTPS, as `tailscale serve` gives it, or HTTP on this
/// machine's loopback, without credentials, query or fragment, ending in a
/// slash.
pub fn hub_url(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw.trim())
        .context("Enter the hub's complete address, such as https://hub.example.ts.net/")?;
    let host = url.host_str().unwrap_or_default().to_string();
    let scheme_ok = match url.scheme() {
        "https" => !host.is_empty(),
        "http" => LOOPBACK.contains(&host.as_str()),
        _ => false,
    };
    if !scheme_ok
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("Use the hub's HTTPS address (plain HTTP only on this machine's loopback), without credentials, query or fragment")
    }
    let path = format!("{}/", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

/// The hub's export, under its address.
fn export_of(base: &Url) -> ConnectorResult<Url> {
    base.join(EXPORT).map_err(ConnectorError::invalid)
}

/// Whether `request`, exactly as it would be sent, is the one read a
/// companion makes of its hub.
fn check(base: &Url, request: &Request) -> Result<(), String> {
    let export = export_of(base).map_err(|e| e.message)?;
    let url = request.url();
    match *request.method() {
        Method::GET if request.body().is_some() => Err("a read with a body".into()),
        Method::GET if *url == export => Ok(()),
        Method::GET => Err(format!("a read of anything but the export: {url}")),
        ref method => Err(format!("{method} {}", url.path())),
    }
}

/// Reads a hub's external export.
pub trait Pull: Send + Sync {
    /// The export of the hub at `base`, checked.
    fn pull<'a>(
        &'a self,
        base: &'a Url,
        token: &'a Credential,
    ) -> BoxFuture<'a, ConnectorResult<ExternalSnapshot>>;
    /// What it sent, and refused to.
    fn requests(&self) -> Option<Requests>;
}

/// The client a companion reads its hub with: one `GET` of the export,
/// nothing else, redirects never followed.
pub struct HubClient {
    client: Client,
    counts: Mutex<Requests>,
}

impl HubClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: connectors::client()?,
            counts: Mutex::default(),
        })
    }
    fn count(&self, update: impl FnOnce(&mut Requests)) {
        update(&mut self.counts.lock().unwrap_or_else(PoisonError::into_inner));
    }
    /// Send `request` if it is the read of the export. Every request to the
    /// hub goes through here.
    async fn send(&self, base: &Url, request: Request) -> ConnectorResult<reqwest::Response> {
        if let Err(why) = check(base, &request) {
            self.count(|c| c.refused += 1);
            return Err(ConnectorError::invalid(format!(
                "Refused to send the hub anything but a read of its export: {why}"
            )));
        }
        self.count(|c| *c.sent.entry(request.method().to_string()).or_default() += 1);
        // The one place a request to the hub leaves, after `check` passed it.
        #[allow(clippy::disallowed_methods)]
        let response = self.client.execute(request).await;
        response.map_err(|_| {
            ConnectorError::new(
                ErrorKind::Transient,
                "Cannot reach the hub; check the network and its address",
            )
        })
    }
    async fn read(&self, base: &Url, token: &Credential) -> ConnectorResult<ExternalSnapshot> {
        let mut request = Request::new(Method::GET, export_of(base)?);
        let headers = request.headers_mut();
        headers.insert(header::ACCEPT, HeaderValue::from_static(ACCEPT));
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        // A credential has no control characters, so this only fails for
        // one that can't be a header at all.
        if let Ok(mut auth) = HeaderValue::from_str(&format!("Bearer {}", token.expose())) {
            auth.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, auth);
        }
        let response = self.send(base, request).await?;
        match response.status() {
            s if s.is_success() => {
                let body = read_body(LABEL, response).await?;
                let snapshot = serde_json::from_value(parse_json(LABEL, &body)?).map_err(|_| {
                    ConnectorError::invalid("The hub's answer isn't an external export")
                })?;
                checked(snapshot)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(ConnectorError::new(
                ErrorKind::Auth,
                "The hub refused the token; set it again with `workboard hub set`",
            )),
            s if s.is_redirection() => Err(ConnectorError::invalid(format!(
                "The hub redirected the request (HTTP {}), which is never followed; check its address",
                s.as_u16()
            ))),
            s => Err(http_error(s)),
        }
    }
}

impl Pull for HubClient {
    fn pull<'a>(
        &'a self,
        base: &'a Url,
        token: &'a Credential,
    ) -> BoxFuture<'a, ConnectorResult<ExternalSnapshot>> {
        Box::pin(self.read(base, token))
    }
    fn requests(&self) -> Option<Requests> {
        Some(
            self.counts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
    }
}

/// What a hub sent, if all of it is external and well formed: every object
/// says it is external, items belong to connections it sent and efforts
/// to efforts it sent, links are HTTP(S), and nothing is out of bounds.
/// Anything else refuses all of it.
pub fn checked(s: ExternalSnapshot) -> ConnectorResult<ExternalSnapshot> {
    let fail = |why: &str| Err(ConnectorError::invalid(format!("The hub's export {why}")));
    if s.zone != Zone::External
        || s.connections.iter().any(|c| c.zone != Zone::External)
        || s.items.iter().any(|i| i.zone != Zone::External)
    {
        return fail("has something that isn't external");
    }
    if [s.connections.len(), s.items.len(), s.efforts.len()]
        .iter()
        .any(|n| *n > MAX_COUNT)
    {
        return fail("is too large");
    }
    let unique = |ids: Vec<&str>| {
        let set: HashSet<_> = ids.iter().collect();
        set.len() == ids.len() && ids.iter().all(|id| !id.is_empty() && id.len() <= MAX_ID)
    };
    let connections: Vec<_> = s.connections.iter().map(|c| c.id.as_str()).collect();
    let efforts: Vec<_> = s.efforts.iter().map(|e| e.id.as_str()).collect();
    if !unique(connections.clone())
        || !unique(efforts.clone())
        || !unique(s.items.iter().map(|i| i.id.as_str()).collect())
    {
        return fail("has missing, overlong or repeated IDs");
    }
    let text_ok = |fields: &[&str]| fields.iter().all(|f| f.len() <= MAX_FIELD);
    for c in &s.connections {
        if !valid_provider(&c.provider)
            || connectors::base_url(&c.base_url).ok().as_deref() != Some(c.base_url.as_str())
            || !text_ok(&[&c.label, &c.account_name])
        {
            return fail("has a connection that isn't valid");
        }
    }
    for i in &s.items {
        let fields = [
            i.key.as_str(),
            &i.title,
            &i.status,
            &i.outcome_reason,
            &i.action,
            &i.updated_at,
        ];
        if !valid_provider(&i.provider)
            || !connections.contains(&i.connection_id.as_str())
            || i.effort_id
                .as_deref()
                .is_some_and(|e| !efforts.contains(&e))
            || connectors::source_url(&i.url).is_err()
            || !text_ok(&fields)
        {
            return fail("has an item that isn't valid");
        }
    }
    if !s.efforts.iter().all(|e| text_ok(&[&e.title])) {
        return fail("has an effort that isn't valid");
    }
    Ok(s)
}

/// The hub a companion pulls from: its address, how its pulls went, and
/// the last export it read.
#[derive(Debug, Clone)]
pub struct Link {
    pub id: String,
    pub url: String,
    pub sync: SyncState,
    pub snapshot: Option<ExternalSnapshot>,
}

impl Link {
    /// The pulls as the schedule sees them: like a connection's syncs, on
    /// the service's interval. Never stored or shown as a connection.
    pub fn as_job(&self) -> Connection {
        Connection {
            id: self.id.clone(),
            provider: "hub".into(),
            label: LABEL.into(),
            base_url: self.url.clone(),
            sync: self.sync.clone(),
            ..Connection::default()
        }
    }
}

/// How a companion's pulls from its hub are going, and how fresh the hub's
/// work is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub url: String,
    pub sync: SyncState,
    /// Stale also as soon as a pull fails: the hub's work is never shown as
    /// current while the hub can't be reached.
    pub schedule: Schedule,
    /// What the last export it read had.
    pub connections: usize,
    pub items: usize,
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::{
        model::{Outcome, Timestamp},
        zone::{ExternalConnection, ExternalEffort, ExternalItem, ExternalProgress},
    };
    use axum::{
        extract::State,
        http::{HeaderMap as Headers, Method as AxumMethod, StatusCode as Status, Uri},
        response::{IntoResponse, Response},
        Router,
    };
    use std::sync::Arc;

    /// An export with `n` items of one connection, the first in an effort.
    pub fn export(n: usize) -> ExternalSnapshot {
        let connection = ExternalConnection {
            id: "hub-connection".into(),
            provider: "github".into(),
            label: "GitHub".into(),
            base_url: "https://github.com/".into(),
            account_name: "someone".into(),
            last_success: Some(Timestamp::from_unix(1_791_763_200)),
            stale: false,
            error_kind: None,
            zone: Zone::External,
        };
        let items = (0..n)
            .map(|k| ExternalItem {
                id: format!("hub-connection:github:{k}"),
                connection_id: connection.id.clone(),
                provider: "github".into(),
                key: format!("o/r#{k}"),
                title: format!("Change {k}"),
                url: format!("https://github.com/o/r/pull/{k}"),
                status: "open".into(),
                outcome: Outcome::Open,
                outcome_reason: "Open".into(),
                action: "review".into(),
                updated_at: "2026-10-12T00:00:00Z".into(),
                effort_id: (k == 0).then(|| "hub-effort".into()),
                zone: Zone::External,
                reasons: vec![crate::model::Reason::ReviewRequested],
            })
            .collect();
        ExternalSnapshot {
            zone: Zone::External,
            connections: vec![connection],
            items,
            efforts: vec![ExternalEffort {
                id: "hub-effort".into(),
                title: "Effort hubeffor".into(),
                progress: ExternalProgress {
                    delivered: 0,
                    total: 1,
                    open: 1,
                    needs_attention: 0,
                    abandoned: 0,
                },
            }],
        }
    }

    #[test]
    fn hub_addresses() {
        for (raw, expected) in [
            (
                "https://hub.example.ts.net",
                Some("https://hub.example.ts.net/"),
            ),
            (
                "https://hub.example.ts.net/workboard",
                Some("https://hub.example.ts.net/workboard/"),
            ),
            ("http://127.0.0.1:8422/", Some("http://127.0.0.1:8422/")),
            ("http://localhost:8422", Some("http://localhost:8422/")),
            ("http://[::1]:8422/", Some("http://[::1]:8422/")),
            // The token would cross a network in the clear.
            ("http://hub.example.ts.net/", None),
            ("http://127.0.0.2/", None),
            ("https://me:secret@hub.example.ts.net/", None),
            ("https://hub.example.ts.net/?x=1", None),
            ("https://hub.example.ts.net/#token=x", None),
            ("ftp://hub.example.ts.net/", None),
            ("hub.example.ts.net", None),
        ] {
            assert_eq!(
                hub_url(raw).ok().map(String::from).as_deref(),
                expected,
                "{raw}"
            );
        }
    }

    #[test]
    fn only_the_read_of_the_export_passes() {
        let base = hub_url("https://hub.example.ts.net/").unwrap();
        let request = |method: Method, url: &str, body: Option<&str>| {
            let mut request = Request::new(method, Url::parse(url).unwrap());
            *request.body_mut() = body.map(|b| b.to_string().into());
            request
        };
        const EXPORT_URL: &str = "https://hub.example.ts.net/api/export/external";
        for (n, (method, url, body, passes)) in [
            (Method::GET, EXPORT_URL, None, true),
            (Method::GET, EXPORT_URL, Some("{}"), false),
            (Method::HEAD, EXPORT_URL, None, false),
            (Method::POST, EXPORT_URL, Some("{}"), false),
            (Method::PUT, EXPORT_URL, None, false),
            (Method::DELETE, EXPORT_URL, None, false),
            (
                Method::POST,
                "https://hub.example.ts.net/api/items",
                Some("{}"),
                false,
            ),
            (
                Method::GET,
                "https://hub.example.ts.net/api/snapshot",
                None,
                false,
            ),
            (
                Method::GET,
                "https://hub.example.ts.net/api/export/external?zone=internal",
                None,
                false,
            ),
            (
                Method::GET,
                "http://hub.example.ts.net/api/export/external",
                None,
                false,
            ),
            (
                Method::GET,
                "https://hub.example.ts.net:8443/api/export/external",
                None,
                false,
            ),
            (
                Method::GET,
                "https://elsewhere.example/api/export/external",
                None,
                false,
            ),
            (
                Method::GET,
                "https://me:x@hub.example.ts.net/api/export/external",
                None,
                false,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let checked = check(&base, &request(method.clone(), url, body));
            assert_eq!(
                checked.is_ok(),
                passes,
                "case {n}: {method} {url}: {checked:?}"
            );
        }
    }

    /// What reached a fixture hub: each request's method, path and query,
    /// authorization and body.
    type Seen = Arc<Mutex<Vec<(String, String, String, Vec<u8>)>>>;

    /// A fixture hub on loopback that records what reaches it, and answers
    /// `answer`.
    async fn fixture<F>(answer: F) -> (Url, Seen)
    where
        F: Fn(&str) -> Response + Clone + Send + Sync + 'static,
    {
        let seen = Seen::default();
        let handler = move |State(seen): State<Seen>,
                            method: AxumMethod,
                            uri: Uri,
                            headers: Headers,
                            body: axum::body::Bytes| {
            let answer = answer.clone();
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                seen.lock().unwrap().push((
                    method.to_string(),
                    uri.to_string(),
                    auth,
                    body.to_vec(),
                ));
                answer(uri.path())
            }
        };
        let router = Router::new().fallback(handler).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = hub_url(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, seen)
    }

    fn token() -> Credential {
        Credential::new("test-only-companion").unwrap()
    }

    #[tokio::test]
    async fn a_pull_is_one_get_of_the_export_with_the_token() {
        let (base, seen) = fixture(|_| axum::Json(export(2)).into_response()).await;
        let client = HubClient::new().unwrap();
        let got = client.pull(&base, &token()).await.unwrap();
        assert_eq!(got.items.len(), 2);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            [(
                "GET".to_string(),
                "/api/export/external".to_string(),
                "Bearer test-only-companion".to_string(),
                vec![]
            )]
        );
        let sent: Vec<_> = client.requests().unwrap().sent.into_iter().collect();
        assert_eq!(sent, [("GET".to_string(), 1)]);
    }

    #[tokio::test]
    async fn redirects_refusals_and_anything_not_external_fail_the_pull() {
        let mut internal = serde_json::to_value(export(1)).unwrap();
        internal["items"][0]["zone"] = "internal".into();
        let mut stray = serde_json::to_value(export(1)).unwrap();
        stray["items"][0]["connection_id"] = "elsewhere".into();
        // (answer, kind)
        let cases: Vec<(Response, ErrorKind)> = vec![
            (
                (Status::FOUND, [("location", "https://elsewhere.example/")]).into_response(),
                ErrorKind::Invalid,
            ),
            (Status::UNAUTHORIZED.into_response(), ErrorKind::Auth),
            (Status::FORBIDDEN.into_response(), ErrorKind::Auth),
            (Status::BAD_GATEWAY.into_response(), ErrorKind::Transient),
            (axum::Json(internal).into_response(), ErrorKind::Invalid),
            (axum::Json(stray).into_response(), ErrorKind::Invalid),
            ("not json".into_response(), ErrorKind::Invalid),
        ];
        for (n, (answer, kind)) in cases.into_iter().enumerate() {
            let answer = Arc::new(Mutex::new(Some(answer)));
            let (base, seen) = fixture(move |_| {
                answer
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap_or_else(|| Status::GONE.into_response())
            })
            .await;
            let error = HubClient::new()
                .unwrap()
                .pull(&base, &token())
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind, "case {n}: {error}");
            // One request, and a redirect never followed.
            assert_eq!(seen.lock().unwrap().len(), 1, "case {n}");
        }
    }

    #[test]
    fn exports_are_checked_whole() {
        type Break = fn(&mut ExternalSnapshot);
        let cases: [(&str, Break); 10] = [
            ("snapshot zone", |s| s.zone = Zone::Internal),
            ("connection zone", |s| {
                s.connections[0].zone = Zone::Internal
            }),
            ("item zone", |s| s.items[1].zone = Zone::Internal),
            ("unknown provider", |s| {
                s.items[0].provider = "bugzilla".into()
            }),
            ("script link", |s| {
                s.items[0].url = "javascript:alert(1)".into()
            }),
            ("plain base", |s| {
                s.connections[0].base_url = "http://github.com/".into()
            }),
            ("unknown effort", |s| {
                s.items[0].effort_id = Some("elsewhere".into())
            }),
            ("repeated item", |s| s.items[1].id = s.items[0].id.clone()),
            ("empty ID", |s| s.efforts[0].id = String::new()),
            ("overlong title", |s| {
                s.items[0].title = "x".repeat(MAX_FIELD + 1)
            }),
        ];
        assert!(checked(export(2)).is_ok());
        for (what, break_it) in cases {
            let mut s = export(2);
            break_it(&mut s);
            assert!(checked(s).is_err(), "{what}");
        }
    }
}
