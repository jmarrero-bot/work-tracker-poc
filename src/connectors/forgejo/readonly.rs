//! The only way Workboard talks to a Forgejo instance, such as Fedora
//! Forge: a client that can read and nothing else, whatever token it holds.
//!
//! It sends `GET` and `HEAD` requests under the instance's API root,
//! `api/v1/`, and refuses everything else before it is sent. Every request
//! goes out through [`ReadOnly::send`], which checks the request exactly as
//! it will be sent; its client is private to this module, and clippy
//! refuses the reqwest calls that could send a write anywhere else in the
//! crate.

use crate::{
    connector::{ConnectorError, ConnectorResult, ErrorKind, Requests, Target},
    connectors::{client, http_error, parse_json, read_body, wait_until},
};
use reqwest::{
    header::{self, HeaderMap, HeaderValue},
    Client, Method, Request, StatusCode,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    hash::BuildHasher,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime},
};
use url::Url;

/// How Forgejo is named in messages.
const SOURCE: &str = "Forgejo";
const ACCEPT: &str = "application/json";
const USER_AGENT: &str = concat!("workboard/", env!("CARGO_PKG_VERSION"));
/// Where a Forgejo instance serves its API, under its web address.
const API_ROOT: &str = "api/v1/";
/// The header that counts every result of a paged listing.
const TOTAL_COUNT: &str = "x-total-count";
/// How long a refusal to go on that names no time is waited out.
const DEFAULT_WAIT: Duration = Duration::from_secs(60);

/// Where an instance serves its API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Api {
    root: Url,
}

impl Api {
    /// The API of the instance at `base`, its web address.
    pub fn of(base: &Url) -> ConnectorResult<Self> {
        Ok(Self {
            root: base.join(API_ROOT).map_err(ConnectorError::invalid)?,
        })
    }
    /// Whether `url` is a resource of this API.
    fn contains(&self, url: &Url) -> bool {
        url.origin() == self.root.origin()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path().starts_with(self.root.path())
    }
}

/// Whether `request`, exactly as it would be sent, only reads.
fn check(api: &Api, request: &Request) -> Result<(), String> {
    let url = request.url();
    match *request.method() {
        Method::GET | Method::HEAD if request.body().is_some() => Err("a read with a body".into()),
        Method::GET | Method::HEAD if api.contains(url) => Ok(()),
        Method::GET | Method::HEAD => Err(format!("a read outside the API: {url}")),
        ref method => Err(format!("{method} {}", url.path())),
    }
}

/// What Forgejo answered, read in full.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// What a GET found.
pub enum Got {
    /// The resource, and for a paged listing, how many results it has in
    /// all.
    Found(Value, Option<u64>),
    /// A redirect, which Forgejo sends for a renamed repository, and where
    /// to: a resource of the same API, as checked, or `None` for anywhere
    /// else. It is never followed here.
    Moved(StatusCode, Option<Url>),
    /// HTTP 403 or 404: missing, or hidden from the account.
    Inaccessible(StatusCode),
}

/// What the client keeps between syncs in one service process, for every
/// connection: until when an instance asked a token to wait. Keyed by a
/// hash of the instance and the token, with a key random to this process,
/// never the token itself.
pub struct Memory {
    keys: std::collections::hash_map::RandomState,
    blocked: Mutex<HashMap<u64, SystemTime>>,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            keys: std::collections::hash_map::RandomState::new(),
            blocked: Mutex::default(),
        }
    }
}

impl Memory {
    fn key(&self, target: &Target) -> u64 {
        self.keys
            .hash_one((target.base_url.as_str(), target.credential.expose()))
    }
    /// Until when the instance asked this token to wait, if it still does.
    fn blocked(&self, target: &Target) -> Option<SystemTime> {
        let blocked = self.blocked.lock().unwrap_or_else(PoisonError::into_inner);
        let until = *blocked.get(&self.key(target))?;
        (until > SystemTime::now()).then_some(until)
    }
    fn block(&self, target: &Target, until: SystemTime) {
        let mut blocked = self.blocked.lock().unwrap_or_else(PoisonError::into_inner);
        blocked.insert(self.key(target), until);
    }
}

/// Until when an answer asks for no more requests: a 429, for its
/// `retry-after` or [`DEFAULT_WAIT`], or a 503 that names a `retry-after`,
/// never more than [`crate::connectors::MAX_WAIT`] away. Forgejo has no API
/// rate limit of its own, but a proxy in front of it can.
fn used_up(answer: &Answer) -> Option<SystemTime> {
    let retry_after = answer
        .headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let wait = match answer.status {
        StatusCode::TOO_MANY_REQUESTS => retry_after.unwrap_or(DEFAULT_WAIT),
        StatusCode::SERVICE_UNAVAILABLE => retry_after?,
        _ => return None,
    };
    Some(wait_until(SystemTime::now(), wait))
}

fn rate_limited(until: SystemTime) -> ConnectorError {
    let minutes = until
        .duration_since(SystemTime::now())
        .unwrap_or_default()
        .as_secs()
        .div_ceil(60)
        .max(1);
    ConnectorError::new(
        ErrorKind::Transient,
        format!("{SOURCE} asked to wait before more requests; try again in about {minutes} min"),
    )
    .held_until(until)
}

/// The token, unless `target` is the anonymous test target. Only the live
/// test of a public repository reads without one; Workboard always sends
/// its connection's token.
#[cfg(test)]
fn token(target: &Target) -> Option<&str> {
    let token = target.credential.expose();
    (token != tests::ANONYMOUS).then_some(token)
}

#[cfg(not(test))]
fn token(target: &Target) -> Option<&str> {
    Some(target.credential.expose())
}

/// A Forgejo client that only reads. One serves one sync, and counts what
/// it sent.
pub struct ReadOnly {
    client: Client,
    memory: Arc<Memory>,
    counts: Mutex<Requests>,
}

impl ReadOnly {
    pub fn new(memory: Arc<Memory>) -> anyhow::Result<Self> {
        Ok(Self {
            client: client()?,
            memory,
            counts: Mutex::default(),
        })
    }
    /// What it sent so far, and refused to.
    pub fn requests(&self) -> Requests {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    fn count(&self, update: impl FnOnce(&mut Requests)) {
        update(&mut self.counts.lock().unwrap_or_else(PoisonError::into_inner));
    }
    /// A request carrying the token as Forgejo takes it.
    fn request(&self, target: &Target, method: Method, url: Url) -> Request {
        let mut request = Request::new(method, url);
        let headers = request.headers_mut();
        headers.insert(header::ACCEPT, HeaderValue::from_static(ACCEPT));
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        // A token is checked for control characters when it is read, so
        // this only fails for one that can't be a header at all.
        if let Some(Ok(mut auth)) =
            token(target).map(|t| HeaderValue::from_str(&format!("token {t}")))
        {
            auth.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, auth);
        }
        request
    }
    /// Send `request` if, as it would be sent, it only reads. Every Forgejo
    /// request goes through here.
    async fn send(&self, target: &Target, request: Request) -> ConnectorResult<Answer> {
        let api = Api::of(&target.base_url)?;
        if let Err(why) = check(&api, &request) {
            self.count(|c| c.refused += 1);
            return Err(ConnectorError::invalid(format!(
                "Refused to send a request that could change {SOURCE}: {why}"
            )));
        }
        if let Some(until) = self.memory.blocked(target) {
            return Err(rate_limited(until));
        }
        self.count(|c| *c.sent.entry(request.method().to_string()).or_default() += 1);
        // The one place a request leaves, after `check` passed it.
        #[allow(clippy::disallowed_methods)]
        let response = self.client.execute(request).await.map_err(|_| {
            ConnectorError::new(
                ErrorKind::Transient,
                format!("Cannot reach {SOURCE}; check network and certificate trust"),
            )
        })?;
        let answer = Answer {
            status: response.status(),
            headers: response.headers().clone(),
            body: read_body(SOURCE, response).await?,
        };
        if let Some(until) = used_up(&answer) {
            self.memory.block(target, until);
            return Err(rate_limited(until));
        }
        Ok(answer)
    }
    /// Read the resource at `path` under the API root. A 403 or 404 is an
    /// answer about that one resource, as is a redirect, which is never
    /// followed.
    pub async fn get(
        &self,
        target: &Target,
        path: &str,
        query: &[(&str, String)],
    ) -> ConnectorResult<Got> {
        let api = Api::of(&target.base_url)?;
        let mut url = api.root.join(path).map_err(ConnectorError::invalid)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        self.get_url(target, url).await
    }
    /// Like `get`, for a URL a redirect named, which `Got::Moved` checked was
    /// in the API, and which `send` checks again.
    pub async fn get_url(&self, target: &Target, url: Url) -> ConnectorResult<Got> {
        let api = Api::of(&target.base_url)?;
        let request = self.request(target, Method::GET, url.clone());
        let answer = self.send(target, request).await?;
        match answer.status {
            s if s.is_success() => {
                let total = answer
                    .headers
                    .get(TOTAL_COUNT)
                    .and_then(|v| v.to_str().ok()?.trim().parse().ok());
                Ok(Got::Found(parse_json(SOURCE, &answer.body)?, total))
            }
            s @ (StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) => Ok(Got::Inaccessible(s)),
            s if s.is_redirection() => {
                let to = answer
                    .headers
                    .get(header::LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .and_then(|l| url.join(l).ok());
                Ok(Got::Moved(s, to.filter(|to| api.contains(to))))
            }
            s => Err(http_error(s)),
        }
    }
    /// Like `get`, but any answer other than the resource is an error.
    pub async fn get_found(
        &self,
        target: &Target,
        path: &str,
        query: &[(&str, String)],
    ) -> ConnectorResult<Value> {
        match self.get(target, path, query).await? {
            Got::Found(v, _) => Ok(v),
            Got::Moved(status, _) | Got::Inaccessible(status) => Err(http_error(status)),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::{HeaderMap as Headers, Method as AxumMethod, StatusCode as Status, Uri},
        response::IntoResponse,
        Router,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The credential of the anonymous live test, which sends no token.
    /// Only test builds know it.
    pub const ANONYMOUS: &str = "anonymous-live-test-of-a-public-repository";

    fn api() -> Api {
        Api::of(&Url::parse("https://forge.example.org/").unwrap()).unwrap()
    }
    fn request(method: Method, url: &str, body: Option<&str>) -> Request {
        let mut request = Request::new(method, Url::parse(url).unwrap());
        *request.body_mut() = body.map(|b| b.to_string().into());
        request
    }

    #[test]
    fn instances_serve_their_api_under_their_address() {
        for (base, root) in [
            (
                "https://forge.fedoraproject.org/",
                "https://forge.fedoraproject.org/api/v1/",
            ),
            (
                "https://example.com/forge/",
                "https://example.com/forge/api/v1/",
            ),
        ] {
            assert_eq!(
                Api::of(&Url::parse(base).unwrap()).unwrap().root.as_str(),
                root
            );
        }
    }

    #[test]
    fn only_reads_pass() {
        const ISSUE: &str = "https://forge.example.org/api/v1/repos/o/r/issues/1";
        let cases = [
            ("GET", Method::GET, ISSUE, None, true),
            ("HEAD", Method::HEAD, ISSUE, None, true),
            (
                "search",
                Method::GET,
                "https://forge.example.org/api/v1/repos/issues/search?assigned=true",
                None,
                true,
            ),
            ("POST", Method::POST, ISSUE, Some("{}"), false),
            (
                "comment",
                Method::POST,
                "https://forge.example.org/api/v1/repos/o/r/issues/1/comments",
                Some(r#"{"body":"x"}"#),
                false,
            ),
            (
                "PATCH",
                Method::PATCH,
                ISSUE,
                Some(r#"{"state":"closed"}"#),
                false,
            ),
            (
                "merge",
                Method::POST,
                "https://forge.example.org/api/v1/repos/o/r/pulls/1/merge",
                None,
                false,
            ),
            ("PUT", Method::PUT, ISSUE, None, false),
            ("DELETE", Method::DELETE, ISSUE, None, false),
            ("OPTIONS", Method::OPTIONS, ISSUE, None, false),
            ("GET with a body", Method::GET, ISSUE, Some("{}"), false),
            (
                "GET outside the API",
                Method::GET,
                "https://forge.example.org/o/r/issues/1",
                None,
                false,
            ),
            (
                "GET the API's parent",
                Method::GET,
                "https://forge.example.org/api/v2/version",
                None,
                false,
            ),
            (
                "GET elsewhere",
                Method::GET,
                "https://example.com/api/v1/user",
                None,
                false,
            ),
            (
                "GET over HTTP",
                Method::GET,
                "http://forge.example.org/api/v1/user",
                None,
                false,
            ),
            (
                "GET on another port",
                Method::GET,
                "https://forge.example.org:8443/api/v1/user",
                None,
                false,
            ),
            (
                "GET with credentials",
                Method::GET,
                "https://me:secret@forge.example.org/api/v1/user",
                None,
                false,
            ),
        ];
        for (name, method, url, body, allowed) in cases {
            let result = check(&api(), &request(method, url, body));
            assert_eq!(result.is_ok(), allowed, "{name}: {result:?}");
        }
    }

    /// Try to change the issue at `path` every way Forgejo's API could,
    /// and check that each was refused before it was sent.
    pub async fn attempt_writes(client: &ReadOnly, target: &Target, path: &str) {
        let api = Api::of(&target.base_url).unwrap();
        let issue = api.root.join(path).unwrap();
        let sent = client.requests().sent;
        for method in [Method::POST, Method::PATCH, Method::PUT, Method::DELETE] {
            let mut request = client.request(target, method.clone(), issue.clone());
            *request.body_mut() = Some(r#"{"state":"closed"}"#.into());
            let error = client.send(target, request).await.err().unwrap();
            assert!(error.message.starts_with("Refused"), "{method}: {error}");
            assert_eq!(error.kind, ErrorKind::Invalid);
        }
        assert_eq!(client.requests().sent, sent, "nothing more was sent");
    }

    /// A Forgejo fixture that counts what reaches it, and answers `answer`.
    async fn counting<F, R>(answer: F) -> (ReadOnly, Target, Arc<AtomicUsize>)
    where
        F: Fn(AxumMethod, Uri, Headers) -> R + Clone + Send + Sync + 'static,
        R: IntoResponse,
    {
        let seen = Arc::new(AtomicUsize::new(0));
        let handler = move |State(seen): State<Arc<AtomicUsize>>,
                            method: AxumMethod,
                            uri: Uri,
                            headers: Headers| {
            let answer = answer.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                answer(method, uri, headers).into_response()
            }
        };
        let router = Router::new().fallback(handler).with_state(seen.clone());
        let (target, _task) = crate::connectors::fixture(router, "test-only-token").await;
        let client = ReadOnly::new(Arc::default()).unwrap();
        (client, target, seen)
    }

    #[tokio::test]
    async fn writes_are_refused_before_anything_is_sent() {
        let (client, target, seen) = counting(|method: AxumMethod, _, headers: Headers| {
            // Reads carry the token as Forgejo takes it.
            assert_eq!(method, AxumMethod::GET);
            assert_eq!(headers["authorization"], "token test-only-token");
            axum::Json(serde_json::json!({"id": 1}))
        })
        .await;
        attempt_writes(&client, &target, "repos/o/r/issues/1").await;
        assert_eq!(seen.load(Ordering::SeqCst), 0, "nothing reached Forgejo");
        assert_eq!(client.requests().refused, 4);
        // Reads still go out, and are counted by what was sent.
        client.get_found(&target, "user", &[]).await.unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        let sent: Vec<_> = client.requests().sent.into_iter().collect();
        assert_eq!(sent, [("GET".into(), 1)]);
    }

    #[tokio::test]
    async fn waits_asked_for_are_waited_out() {
        // (status, retry-after, whether it blocks)
        for (status, retry_after, blocks) in [
            (Status::TOO_MANY_REQUESTS, Some("120"), true),
            (Status::TOO_MANY_REQUESTS, None, true),
            (Status::SERVICE_UNAVAILABLE, Some("30"), true),
            (Status::SERVICE_UNAVAILABLE, None, false),
        ] {
            let (client, target, seen) = counting(move |_, uri: Uri, _| {
                if uri.path().ends_with("/search") {
                    let mut answer = status.into_response();
                    if let Some(seconds) = retry_after {
                        answer
                            .headers_mut()
                            .insert("retry-after", seconds.parse().unwrap());
                    }
                    return answer;
                }
                axum::Json(serde_json::json!({"id": 1})).into_response()
            })
            .await;
            let error = client
                .get(&target, "repos/issues/search", &[])
                .await
                .err()
                .unwrap();
            assert_eq!(error.kind, ErrorKind::Transient, "{error}");
            // The next request, in this sync or a later one, waits too, but
            // another token on the same instance doesn't.
            let later = ReadOnly::new(client.memory.clone()).unwrap();
            let next = later.get_found(&target, "user", &[]).await;
            let other = Target {
                credential: crate::connector::Credential::new("other-token").unwrap(),
                ..target.clone()
            };
            later.get_found(&other, "user", &[]).await.unwrap();
            // Saying until when, so a scheduled sync waits as long.
            let held = error.hold_until;
            assert_eq!(held.is_some(), blocks, "{status}");
            if blocks {
                let error = next.unwrap_err();
                assert!(error.message.contains("asked to wait"), "{error}");
                assert_eq!(error.hold_until, held, "{status}");
                assert_eq!(seen.load(Ordering::SeqCst), 2, "{status}");
            } else {
                next.unwrap();
                assert_eq!(seen.load(Ordering::SeqCst), 3, "{status}");
            }
        }
    }

    #[test]
    fn absurd_waits_are_capped() {
        let max = crate::connectors::MAX_WAIT;
        for retry_after in [u64::MAX.to_string(), "7200".into()] {
            let answer = Answer {
                status: StatusCode::TOO_MANY_REQUESTS,
                headers: [(header::RETRY_AFTER, retry_after.parse().unwrap())]
                    .into_iter()
                    .collect(),
                body: vec![],
            };
            let before = SystemTime::now();
            let wait = used_up(&answer).unwrap().duration_since(before).unwrap();
            assert!(
                wait <= max + Duration::from_secs(1) && wait >= max - Duration::from_secs(1),
                "{retry_after}: {wait:?}"
            );
        }
    }

    #[tokio::test]
    async fn missing_moved_and_hidden_resources_are_answers() {
        let (client, target, _) = counting(|_, uri: Uri, _| {
            let moved = |to: &'static str| (Status::MOVED_PERMANENTLY, [("location", to)]);
            match uri.path() {
                "/api/v1/repos/o/r/issues/1" => Status::NOT_FOUND.into_response(),
                "/api/v1/repos/o/r/issues/2" => Status::FORBIDDEN.into_response(),
                // A renamed repository, as Forgejo redirects it.
                "/api/v1/repos/o/r/issues/3" => {
                    moved("/api/v1/repos/o/renamed/issues/3").into_response()
                }
                "/api/v1/repos/o/r/issues/4" => {
                    moved("https://elsewhere.example/api/v1/repos/o/r/issues/4").into_response()
                }
                "/api/v1/repos/o/r/issues/5" => moved("/o/r/issues/5").into_response(),
                "/api/v1/repos/o/renamed/issues/3" => (
                    [("x-total-count", "1")],
                    axum::Json(serde_json::json!({"id": 3})),
                )
                    .into_response(),
                _ => Status::UNAUTHORIZED.into_response(),
            }
        })
        .await;
        for (n, status, to) in [
            (1, 404, None),
            (2, 403, None),
            (3, 301, Some("/api/v1/repos/o/renamed/issues/3")),
            (4, 301, None),
            (5, 301, None),
        ] {
            let path = format!("repos/o/r/issues/{n}");
            match client.get(&target, &path, &[]).await {
                Ok(Got::Inaccessible(s)) if status != 301 => assert_eq!(s.as_u16(), status),
                Ok(Got::Moved(s, moved)) if status == 301 => {
                    assert!(moved
                        .as_ref()
                        .is_none_or(|u| u.origin() == target.base_url.origin()));
                    let moved = moved.as_ref().map(Url::path);
                    assert_eq!((s.as_u16(), moved), (status, to), "issue {n}")
                }
                _ => panic!("issue {n}"),
            }
        }
        let error = client.get(&target, "user", &[]).await.err().unwrap();
        assert_eq!(error.kind, ErrorKind::Auth, "{error}");
        assert_eq!(client.requests().sent["GET"], 6, "no redirect was followed");
        // Following one goes exactly where it was checked to go, and the
        // total count comes with what it found.
        let Ok(Got::Moved(_, Some(to))) = client.get(&target, "repos/o/r/issues/3", &[]).await
        else {
            panic!("moved")
        };
        match client.get_url(&target, to).await {
            Ok(Got::Found(v, total)) => assert_eq!((v["id"].as_u64(), total), (Some(3), Some(1))),
            _ => panic!("followed"),
        }
        assert_eq!(client.requests().refused, 0);
    }

    #[tokio::test]
    async fn the_anonymous_test_target_sends_no_token() {
        let (client, target, _) = counting(|_, _, headers: Headers| {
            assert!(!headers.contains_key("authorization"));
            axum::Json(serde_json::json!({}))
        })
        .await;
        let anonymous = Target {
            credential: crate::connector::Credential::new(ANONYMOUS).unwrap(),
            ..target
        };
        client.get_found(&anonymous, "version", &[]).await.unwrap();
    }
}
