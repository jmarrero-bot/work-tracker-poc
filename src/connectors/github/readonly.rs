//! The only way Workboard talks to GitHub: a client that can read and
//! nothing else, whatever token it holds.
//!
//! It sends REST `GET` and `HEAD` requests under the instance's API root,
//! and `POST` to the GraphQL endpoint only with a document whose operations
//! are all queries. Everything else is refused before it is sent. Every
//! request goes out through [`ReadOnly::send`], which checks the request
//! exactly as it will be sent, so a write built anywhere else is refused too;
//! its client is private to this module, and clippy refuses the reqwest
//! calls that could send a write anywhere else in the crate.

use crate::{
    connector::{ConnectorError, ConnectorResult, ErrorKind, Requests, Target},
    connectors::{client, http_error, parse_json, read_body, wait_until, MAX_WAIT},
};
use graphql_parser::query::{parse_query, Definition, OperationDefinition};
use reqwest::{
    header::{self, HeaderMap, HeaderValue},
    Client, Method, Request, StatusCode,
};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    hash::BuildHasher,
    sync::{Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

/// The REST API version these requests are written against.
const API_VERSION: &str = "2022-11-28";
const ACCEPT: &str = "application/vnd.github+json";
const USER_AGENT: &str = concat!("workboard/", env!("CARGO_PKG_VERSION"));
/// How GitHub is named in messages.
const SOURCE: &str = "GitHub";
/// Most answers kept for conditional requests, across all connections.
const MAX_CACHED: usize = 512;
/// Most bytes of answers kept for conditional requests, across all
/// connections.
const MAX_CACHED_BYTES: usize = 32 * 1024 * 1024;
/// How long a secondary rate limit that names no time is waited out, as
/// GitHub asks.
const SECONDARY_WAIT: Duration = Duration::from_secs(60);
/// How a sent GraphQL request is counted. Only queries get this far.
const GRAPHQL_QUERY: &str = "POST /graphql (query)";

/// Where an instance serves its APIs: `api.github.com` for github.com, and
/// `api/v3/` and `api/graphql` under a GitHub Enterprise Server's URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Api {
    rest: Url,
    graphql: Url,
}

impl Api {
    /// The APIs of the instance at `base`, its web address.
    pub fn of(base: &Url) -> ConnectorResult<Self> {
        let api = match base.host_str() {
            Some("github.com") if base.path() == "/" => Self {
                rest: Url::parse("https://api.github.com/").map_err(ConnectorError::invalid)?,
                graphql: Url::parse("https://api.github.com/graphql")
                    .map_err(ConnectorError::invalid)?,
            },
            Some("github.com" | "api.github.com" | "www.github.com") => {
                return Err(ConnectorError::invalid(
                    "Use https://github.com/ as the address of github.com",
                ))
            }
            _ => Self {
                rest: base.join("api/v3/").map_err(ConnectorError::invalid)?,
                graphql: base.join("api/graphql").map_err(ConnectorError::invalid)?,
            },
        };
        Ok(api)
    }
    /// Whether `url` is a REST resource of this instance.
    fn is_rest(&self, url: &Url) -> bool {
        url.origin() == self.rest.origin()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path().starts_with(self.rest.path())
            && !self.is_under_graphql(url.path())
    }
    /// Whether `path` is the GraphQL endpoint's or below it.
    fn is_under_graphql(&self, path: &str) -> bool {
        let graphql = self.graphql.path();
        path == graphql || path.starts_with(&format!("{graphql}/"))
    }
    /// The rate limit `url` counts against.
    fn resource(&self, url: &Url) -> &'static str {
        if *url == self.graphql {
            "graphql"
        } else if url
            .path()
            .starts_with(&format!("{}search/", self.rest.path()))
        {
            "search"
        } else {
            "core"
        }
    }
}

/// Whether `request`, exactly as it would be sent, only reads.
fn check(api: &Api, request: &Request) -> Result<(), String> {
    let url = request.url();
    match *request.method() {
        Method::GET | Method::HEAD if request.body().is_some() => Err("a read with a body".into()),
        Method::GET | Method::HEAD if api.is_rest(url) => Ok(()),
        Method::GET | Method::HEAD => Err(format!("a read outside the REST API: {url}")),
        Method::POST if *url == api.graphql => {
            let body = request
                .body()
                .and_then(|b| b.as_bytes())
                .ok_or("a GraphQL request without a body")?;
            graphql_query_body(body)
        }
        ref method => Err(format!("{method} {}", url.path())),
    }
}

/// Whether a GraphQL request body asks only queries: a JSON object with a
/// `query` document and optionally its `variables`, and nothing else that
/// could pick or change an operation.
fn graphql_query_body(body: &[u8]) -> Result<(), String> {
    let body: Map<String, Value> =
        serde_json::from_slice(body).map_err(|_| "a GraphQL body that isn't a JSON object")?;
    if let Some(key) = body
        .keys()
        .find(|k| !matches!(k.as_str(), "query" | "variables"))
    {
        return Err(format!("a GraphQL body with '{key}'"));
    }
    if !matches!(body.get("variables"), None | Some(Value::Object(_))) {
        return Err("GraphQL variables that aren't an object".into());
    }
    let document = body
        .get("query")
        .and_then(Value::as_str)
        .ok_or("a GraphQL body without a query document")?;
    read_only_document(document).map(drop)
}

/// A GraphQL document whose operations are all queries, printed again from
/// what was parsed, so GitHub is sent exactly what was checked. A mutation
/// or subscription anywhere in it, even next to queries, refuses all of it.
fn read_only_document(document: &str) -> Result<String, String> {
    let parsed =
        parse_query::<String>(document).map_err(|_| "a GraphQL document that doesn't parse")?;
    let mut queries = 0;
    for definition in &parsed.definitions {
        match definition {
            Definition::Operation(
                OperationDefinition::Query(_) | OperationDefinition::SelectionSet(_),
            ) => queries += 1,
            Definition::Operation(OperationDefinition::Mutation(_)) => {
                return Err("a GraphQL mutation".into())
            }
            Definition::Operation(OperationDefinition::Subscription(_)) => {
                return Err("a GraphQL subscription".into())
            }
            Definition::Fragment(_) => (),
        }
    }
    if queries == 0 {
        return Err("a GraphQL document without a query".into());
    }
    Ok(parsed.to_string())
}

/// An answer kept to ask for again conditionally.
struct Cached {
    etag: HeaderValue,
    body: Value,
    /// The size of the answer as it was read.
    size: usize,
}

/// Answers kept for conditional requests, up to [`MAX_CACHED`] of them and
/// [`MAX_CACHED_BYTES`] in all; past either, it starts over.
#[derive(Default)]
struct Cache {
    answers: HashMap<(u64, String), Cached>,
    bytes: usize,
}

impl Cache {
    fn insert(&mut self, key: (u64, String), cached: Cached) {
        if cached.size > MAX_CACHED_BYTES {
            return;
        }
        if let Some(old) = self.answers.remove(&key) {
            self.bytes -= old.size;
        }
        if self.answers.len() >= MAX_CACHED || self.bytes + cached.size > MAX_CACHED_BYTES {
            self.answers.clear();
            self.bytes = 0;
        }
        self.bytes += cached.size;
        self.answers.insert(key, cached);
    }
}

/// What GitHub answered, read in full.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// What a GET found.
pub enum Got {
    Found(Value),
    /// A redirect, which GitHub sends for a moved repository, and where to:
    /// a REST resource under the same API root, as checked, or `None` for
    /// anywhere else. It is never followed here.
    Moved(StatusCode, Option<Url>),
    /// HTTP 403 or 404: missing, or hidden from the account.
    Inaccessible(StatusCode),
}

/// What the client keeps between syncs in one service process, for every
/// connection: answers by ETag, to ask for them again conditionally, which
/// doesn't count against the rate limit when nothing changed; and until
/// when a rate limit is used up. Keyed by a hash of the token with a key
/// random to this process, never the token itself.
pub struct Memory {
    keys: std::collections::hash_map::RandomState,
    cached: Mutex<Cache>,
    blocked: Mutex<HashMap<(u64, String), SystemTime>>,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            keys: std::collections::hash_map::RandomState::new(),
            cached: Mutex::default(),
            blocked: Mutex::default(),
        }
    }
}

impl Memory {
    fn key(&self, target: &Target) -> u64 {
        self.keys.hash_one(target.credential.expose())
    }
    fn cached(&self, target: &Target, url: &Url) -> Option<(HeaderValue, Value)> {
        let cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        cached
            .answers
            .get(&(self.key(target), url.to_string()))
            .map(|c| (c.etag.clone(), c.body.clone()))
    }
    fn cache(&self, target: &Target, url: &Url, etag: HeaderValue, body: &Value, size: usize) {
        let body = body.clone();
        let key = (self.key(target), url.to_string());
        self.cached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, Cached { etag, body, size });
    }
    /// When the rate limit for `resource` frees up, if it is used up.
    fn blocked(&self, target: &Target, resource: &str) -> Option<SystemTime> {
        let blocked = self.blocked.lock().unwrap_or_else(PoisonError::into_inner);
        let until = *blocked.get(&(self.key(target), resource.to_string()))?;
        (until > SystemTime::now()).then_some(until)
    }
    fn block(&self, target: &Target, resource: &str, until: SystemTime) {
        let mut blocked = self.blocked.lock().unwrap_or_else(PoisonError::into_inner);
        blocked.insert((self.key(target), resource.to_string()), until);
    }
}

/// The bucket a secondary rate limit is kept in. Unlike the primary limits
/// (`core`, `search`, `graphql`), it covers every request of the token.
const SECONDARY: &str = "secondary";

/// Which rate limit an answer says is used up, and until when: no requests
/// remaining in the primary limit of the request's resource until it
/// resets; or a secondary limit, from its `retry-after`, or from a refusal
/// that says it hit one without saying for how long. Never more than
/// [`MAX_WAIT`] away, whatever the answer says.
fn used_up(answer: &Answer, resource: &'static str) -> Option<(&'static str, SystemTime)> {
    let headers = &answer.headers;
    let number = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<u64>().ok();
    let now = SystemTime::now();
    if let Some(seconds) = number("retry-after") {
        return Some((SECONDARY, wait_until(now, Duration::from_secs(seconds))));
    }
    if number("x-ratelimit-remaining") == Some(0) {
        let reset = number("x-ratelimit-reset")?;
        let wait = UNIX_EPOCH
            .checked_add(Duration::from_secs(reset))
            .map_or(MAX_WAIT, |reset| {
                reset.duration_since(now).unwrap_or_default()
            });
        return Some((resource, wait_until(now, wait)));
    }
    let refused = matches!(
        answer.status,
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    );
    let says_so = String::from_utf8_lossy(&answer.body)
        .to_lowercase()
        .contains("rate limit");
    (refused && says_so).then(|| (SECONDARY, wait_until(now, SECONDARY_WAIT)))
}

fn rate_limited(resource: &str, until: SystemTime) -> ConnectorError {
    let minutes = until
        .duration_since(SystemTime::now())
        .unwrap_or_default()
        .as_secs()
        .div_ceil(60)
        .max(1);
    ConnectorError::new(
        ErrorKind::Transient,
        format!("GitHub's {resource} rate limit is used up; try again in about {minutes} min"),
    )
}

/// A GitHub client that only reads. One serves one sync, and counts what it
/// sent.
pub struct ReadOnly {
    client: Client,
    memory: std::sync::Arc<Memory>,
    counts: Mutex<Requests>,
}

impl ReadOnly {
    pub fn new(memory: std::sync::Arc<Memory>) -> anyhow::Result<Self> {
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
    /// A request carrying the token and the headers GitHub expects.
    fn request(&self, target: &Target, method: Method, url: Url) -> Request {
        let mut request = Request::new(method, url);
        let headers = request.headers_mut();
        headers.insert(header::ACCEPT, HeaderValue::from_static(ACCEPT));
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );
        // A token is checked for control characters when it is read, so
        // this only fails for one that can't be a header at all.
        if let Ok(mut auth) =
            HeaderValue::from_str(&format!("Bearer {}", target.credential.expose()))
        {
            auth.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, auth);
        }
        request
    }
    /// Send `request` if, as it would be sent, it only reads. Every GitHub
    /// request goes through here.
    async fn send(&self, target: &Target, request: Request) -> ConnectorResult<Answer> {
        let api = Api::of(&target.base_url)?;
        if let Err(why) = check(&api, &request) {
            self.count(|c| c.refused += 1);
            return Err(ConnectorError::invalid(format!(
                "Refused to send a request that could change GitHub: {why}"
            )));
        }
        let resource = api.resource(request.url());
        for limit in [SECONDARY, resource] {
            if let Some(until) = self.memory.blocked(target, limit) {
                return Err(rate_limited(limit, until));
            }
        }
        let sent = match *request.method() {
            Method::POST => GRAPHQL_QUERY.to_string(),
            ref method => method.to_string(),
        };
        self.count(|c| *c.sent.entry(sent).or_default() += 1);
        // The one place a request leaves, after `check` passed it.
        #[allow(clippy::disallowed_methods)]
        let response = self.client.execute(request).await.map_err(|_| {
            ConnectorError::new(
                ErrorKind::Transient,
                "Cannot reach GitHub; check network and certificate trust",
            )
        })?;
        let answer = Answer {
            status: response.status(),
            headers: response.headers().clone(),
            body: read_body(SOURCE, response).await?,
        };
        if let Some((limit, until)) = used_up(&answer, resource) {
            self.memory.block(target, limit, until);
            if matches!(
                answer.status,
                StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
            ) {
                return Err(rate_limited(limit, until));
            }
        }
        Ok(answer)
    }
    /// Read the REST resource at `path` under the API root. A 403 or 404 is
    /// an answer about that one resource, as is a redirect, which GitHub
    /// sends for a moved repository and which is never followed.
    pub async fn get(
        &self,
        target: &Target,
        path: &str,
        query: &[(&str, String)],
    ) -> ConnectorResult<Got> {
        let api = Api::of(&target.base_url)?;
        let mut url = api.rest.join(path).map_err(ConnectorError::invalid)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        self.get_url(target, url).await
    }
    /// Like `get`, for a URL a redirect named, which `Got::Moved` checked was
    /// under the REST API root, and which `send` checks again.
    pub async fn get_url(&self, target: &Target, url: Url) -> ConnectorResult<Got> {
        let api = Api::of(&target.base_url)?;
        let mut request = self.request(target, Method::GET, url.clone());
        let cached = self.memory.cached(target, &url);
        if let Some((etag, _)) = &cached {
            request
                .headers_mut()
                .insert(header::IF_NONE_MATCH, etag.clone());
        }
        let answer = self.send(target, request).await?;
        match answer.status {
            StatusCode::NOT_MODIFIED => {
                let (_, body) = cached.ok_or_else(|| {
                    ConnectorError::invalid("GitHub answered a request it was never asked")
                })?;
                self.count(|c| c.not_modified += 1);
                Ok(Got::Found(body))
            }
            s if s.is_success() => {
                let body = parse_json(SOURCE, &answer.body)?;
                if let Some(etag) = answer.headers.get(header::ETAG) {
                    let size = answer.body.len();
                    self.memory.cache(target, &url, etag.clone(), &body, size);
                }
                Ok(Got::Found(body))
            }
            s @ (StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) => Ok(Got::Inaccessible(s)),
            s if s.is_redirection() => {
                let to = answer
                    .headers
                    .get(header::LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .and_then(|l| url.join(l).ok());
                Ok(Got::Moved(s, to.filter(|to| api.is_rest(to))))
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
            Got::Found(v) => Ok(v),
            Got::Moved(status, _) | Got::Inaccessible(status) => Err(http_error(status)),
        }
    }
    /// Ask GitHub's GraphQL API `document`, which has to be queries only,
    /// and return its `data`.
    // The bot plugin (#9) reads Projects v2, which only GraphQL serves.
    #[allow(dead_code)]
    pub async fn graphql(
        &self,
        target: &Target,
        document: &str,
        variables: Map<String, Value>,
    ) -> ConnectorResult<Value> {
        let document = read_only_document(document).map_err(|why| {
            self.count(|c| c.refused += 1);
            ConnectorError::invalid(format!(
                "Refused to send a request that could change GitHub: {why}"
            ))
        })?;
        let api = Api::of(&target.base_url)?;
        let body = serde_json::json!({"query": document, "variables": variables});
        let mut request = self.request(target, Method::POST, api.graphql);
        request.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        *request.body_mut() = Some(body.to_string().into());
        let answer = self.send(target, request).await?;
        if !answer.status.is_success() {
            return Err(http_error(answer.status));
        }
        let mut answer = parse_json(SOURCE, &answer.body)?;
        if answer["errors"].as_array().is_some_and(|e| !e.is_empty()) {
            return Err(ConnectorError::invalid(
                "GitHub's GraphQL API answered with errors",
            ));
        }
        Ok(answer.get_mut("data").map(Value::take).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::{HeaderMap as Headers, Method as AxumMethod, StatusCode as Status, Uri},
        response::IntoResponse,
        Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn instances_serve_their_apis() {
        for (base, rest, graphql) in [
            (
                "https://github.com/",
                "https://api.github.com/",
                "https://api.github.com/graphql",
            ),
            (
                "https://ghe.example.com/",
                "https://ghe.example.com/api/v3/",
                "https://ghe.example.com/api/graphql",
            ),
            (
                "https://example.com/github/",
                "https://example.com/github/api/v3/",
                "https://example.com/github/api/graphql",
            ),
        ] {
            let api = Api::of(&Url::parse(base).unwrap()).unwrap();
            assert_eq!((api.rest.as_str(), api.graphql.as_str()), (rest, graphql));
        }
        for wrong in [
            "https://api.github.com/",
            "https://github.com/org/",
            "https://www.github.com/",
        ] {
            assert!(Api::of(&Url::parse(wrong).unwrap()).is_err(), "{wrong}");
        }
    }

    fn api() -> Api {
        Api::of(&Url::parse("https://github.com/").unwrap()).unwrap()
    }
    fn request(method: Method, url: &str, body: Option<Value>) -> Request {
        let mut request = Request::new(method, Url::parse(url).unwrap());
        *request.body_mut() = body.map(|b| b.to_string().into());
        request
    }
    fn graphql(query: &str) -> Option<Value> {
        Some(serde_json::json!({ "query": query }))
    }

    #[test]
    fn only_reads_pass() {
        const REST: &str = "https://api.github.com/repos/o/r/issues/1";
        const GRAPHQL: &str = "https://api.github.com/graphql";
        let query = "query { viewer { login } }";
        let cases = [
            ("REST GET", Method::GET, REST, None, true),
            ("REST HEAD", Method::HEAD, REST, None, true),
            ("search", Method::GET, "https://api.github.com/search/issues?q=x", None, true),
            ("named query", Method::POST, GRAPHQL, graphql(query), true),
            ("anonymous query", Method::POST, GRAPHQL, graphql("{ viewer { login } }"), true),
            (
                "query with variables and a fragment",
                Method::POST,
                GRAPHQL,
                Some(serde_json::json!({
                    "query": "query Q($n: Int!) { viewer { ...F } } fragment F on User { repositories(first: $n) { totalCount } }",
                    "variables": {"n": 1},
                })),
                true,
            ),
            (
                "the word mutation in a string",
                Method::POST,
                GRAPHQL,
                graphql(r#"query { search(query: "mutation { deleteIssue }", type: ISSUE, first: 1) { issueCount } }"#),
                true,
            ),
            ("POST", Method::POST, REST, Some(serde_json::json!({})), false),
            ("PATCH", Method::PATCH, REST, Some(serde_json::json!({"state":"closed"})), false),
            ("PUT", Method::PUT, "https://api.github.com/repos/o/r/pulls/1/merge", None, false),
            ("DELETE", Method::DELETE, REST, None, false),
            ("GET with a body", Method::GET, REST, Some(serde_json::json!({})), false),
            ("GET elsewhere", Method::GET, "https://example.com/user", None, false),
            ("GET GraphQL", Method::GET, GRAPHQL, None, false),
            ("GET GraphQL/", Method::GET, "https://api.github.com/graphql/", None, false),
            ("GET under GraphQL", Method::GET, "https://api.github.com/graphql/x", None, false),
            ("GET graphqlx", Method::GET, "https://api.github.com/graphqlx", None, true),
            (
                "mutation",
                Method::POST,
                GRAPHQL,
                graphql(r#"mutation { closeIssue(input: {issueId: "1"}) { clientMutationId } }"#),
                false,
            ),
            (
                "subscription",
                Method::POST,
                GRAPHQL,
                graphql("subscription { viewer { login } }"),
                false,
            ),
            (
                "mutation after a query",
                Method::POST,
                GRAPHQL,
                graphql("query A { viewer { login } } mutation B { deleteIssue(input: {issueId: \"1\"}) { clientMutationId } }"),
                false,
            ),
            (
                "mutation chosen by name",
                Method::POST,
                GRAPHQL,
                Some(serde_json::json!({"query": query, "operationName": "B"})),
                false,
            ),
            ("only a fragment", Method::POST, GRAPHQL, graphql("fragment F on User { login }"), false),
            ("not GraphQL", Method::POST, GRAPHQL, graphql("{ viewer { login }"), false),
            ("not JSON", Method::POST, GRAPHQL, Some(Value::String(query.into())), false),
            ("no body", Method::POST, GRAPHQL, None, false),
            (
                "variables as a string",
                Method::POST,
                GRAPHQL,
                Some(serde_json::json!({"query": query, "variables": "{}"})),
                false,
            ),
            (
                "GraphQL with a query string",
                Method::POST,
                "https://api.github.com/graphql?x=1",
                graphql(query),
                false,
            ),
        ];
        for (name, method, url, body, allowed) in cases {
            let result = check(&api(), &request(method, url, body));
            assert_eq!(result.is_ok(), allowed, "{name}: {result:?}");
        }
    }

    #[test]
    fn checked_documents_are_sent_as_parsed() {
        let printed = read_only_document("query{viewer{login}} # a comment").unwrap();
        assert!(
            printed.starts_with("query") && printed.contains("login"),
            "{printed}"
        );
        assert!(!printed.contains("comment"), "{printed}");
        // Printing what was parsed is stable, so what is sent is what was
        // checked.
        assert_eq!(read_only_document(&printed).unwrap(), printed);
    }

    /// A GitHub fixture that counts what reaches it, and answers `answer`.
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
        let (client, target, seen) = counting(|_, _, _| {
            axum::Json(serde_json::json!({
                "data": {"viewer": {"login": "me"}}
            }))
        })
        .await;
        let api = Api::of(&target.base_url).unwrap();
        let rest = api.rest.join("repos/o/r/issues/1").unwrap();
        for method in [Method::POST, Method::PATCH, Method::PUT, Method::DELETE] {
            let mut request = client.request(&target, method.clone(), rest.clone());
            *request.body_mut() = Some("{}".into());
            let error = client.send(&target, request).await.err().unwrap();
            assert!(error.message.starts_with("Refused"), "{method}: {error}");
        }
        let mutation = "mutation { deleteIssue(input: {issueId: \"1\"}) { clientMutationId } }";
        let error = client
            .graphql(&target, mutation, Map::new())
            .await
            .unwrap_err();
        assert!(error.message.contains("mutation"), "{error}");
        assert_eq!(seen.load(Ordering::SeqCst), 0, "nothing reached GitHub");
        assert_eq!(client.requests().refused, 5);
        // Reads still go out, and are counted by what was sent.
        let data = client
            .graphql(&target, "query { viewer { login } }", Map::new())
            .await
            .unwrap();
        assert_eq!(data["viewer"]["login"], "me");
        client.get_found(&target, "user", &[]).await.unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 2);
        let sent: Vec<_> = client.requests().sent.into_iter().collect();
        assert_eq!(sent, [("GET".into(), 1), (GRAPHQL_QUERY.into(), 1)]);
    }

    #[tokio::test]
    async fn unchanged_answers_come_from_memory() {
        const ETAG: &str = "W/\"v1\"";
        let (client, target, seen) = counting(|_, _, headers: Headers| {
            if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(ETAG) {
                return Status::NOT_MODIFIED.into_response();
            }
            ([("etag", ETAG)], axum::Json(serde_json::json!({"id": 1}))).into_response()
        })
        .await;
        for _ in 0..2 {
            assert_eq!(
                client.get_found(&target, "user", &[]).await.unwrap()["id"],
                1
            );
        }
        // A later sync shares the memory, not the counts.
        let later = ReadOnly::new(client.memory.clone()).unwrap();
        assert_eq!(
            later.get_found(&target, "user", &[]).await.unwrap()["id"],
            1
        );
        assert_eq!(seen.load(Ordering::SeqCst), 3);
        assert_eq!(client.requests().not_modified, 1);
        assert_eq!(later.requests().not_modified, 1);
        // Another token doesn't get this one's answers.
        let other = Target {
            credential: crate::connector::Credential::new("other-token").unwrap(),
            ..target.clone()
        };
        later.get_found(&other, "user", &[]).await.unwrap();
        assert_eq!(later.requests().not_modified, 1);
    }

    #[tokio::test]
    async fn used_up_rate_limits_are_waited_out() {
        // (status, header, value, the limit it hits)
        for limited in [
            (Status::FORBIDDEN, "x-ratelimit-remaining", "0", "search"),
            (Status::TOO_MANY_REQUESTS, "retry-after", "120", SECONDARY),
            (Status::FORBIDDEN, "retry-after", "60", SECONDARY),
            // A secondary limit that says so only in its body.
            (Status::FORBIDDEN, "x-unrelated", "1", SECONDARY),
        ] {
            let reset = (SystemTime::now() + Duration::from_secs(600))
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string();
            let (client, target, seen) = counting(move |_, uri: Uri, _| {
                if uri.path().contains("/search/") {
                    let (status, name, value, _) = limited;
                    let headers = [
                        (name, value.to_string()),
                        ("x-ratelimit-reset", reset.clone()),
                    ];
                    let body = "You have exceeded a secondary rate limit.";
                    return (status, headers, body).into_response();
                }
                axum::Json(serde_json::json!({"data": {}, "id": 1})).into_response()
            })
            .await;
            let query = [("q", "x".to_string())];
            let search = || client.get(&target, "search/issues", &query);
            let error = search().await.err().unwrap();
            assert_eq!(error.kind, ErrorKind::Transient, "{error}");
            let limit = limited.3;
            assert!(
                error.message.contains(&format!("{limit} rate limit")),
                "{error}"
            );
            // Not asked again until it frees up.
            let error = search().await.err().unwrap();
            assert_eq!(error.kind, ErrorKind::Transient, "{error}");
            let user = client.get_found(&target, "user", &[]).await;
            let graphql = client
                .graphql(&target, "{ viewer { login } }", Map::new())
                .await;
            if limit == SECONDARY {
                // A secondary limit covers every request of the token,
                // in this sync and the next.
                let later = ReadOnly::new(client.memory.clone()).unwrap();
                let next = later.get_found(&target, "user", &[]).await;
                for error in [user.err(), graphql.err(), next.err()] {
                    let error = error.expect("blocked");
                    assert_eq!(error.kind, ErrorKind::Transient, "{error}");
                }
                assert_eq!(seen.load(Ordering::SeqCst), 1, "{limited:?}");
            } else {
                // A primary limit is its resource's alone.
                user.unwrap();
                graphql.unwrap();
                assert_eq!(seen.load(Ordering::SeqCst), 3, "{limited:?}");
            }
        }
    }

    #[tokio::test]
    async fn missing_moved_and_hidden_resources_are_answers() {
        let (client, target, _) = counting(|_, uri: Uri, _| {
            let moved = |to: &'static str| (Status::MOVED_PERMANENTLY, [("location", to)]);
            match uri.path() {
                "/api/v3/repos/o/r/issues/1" => Status::NOT_FOUND.into_response(),
                "/api/v3/repos/o/r/issues/2" => {
                    (Status::FORBIDDEN, "Resource not accessible").into_response()
                }
                "/api/v3/repos/o/r/issues/3" => {
                    moved("/api/v3/repositories/1/issues/3").into_response()
                }
                "/api/v3/repos/o/r/issues/4" => {
                    moved("https://elsewhere.example/api/v3/repositories/1/issues/4")
                        .into_response()
                }
                "/api/v3/repos/o/r/issues/5" => moved("/api/graphql").into_response(),
                // A path that would read as another host if joined again.
                "/api/v3/repos/o/r/issues/6" => {
                    moved("/api/v3///elsewhere.example/issues/6").into_response()
                }
                "/api/v3///elsewhere.example/issues/6" => {
                    axum::Json(serde_json::json!({"id": 6})).into_response()
                }
                _ => Status::UNAUTHORIZED.into_response(),
            }
        })
        .await;
        for (n, status, to) in [
            (1, 404, None),
            (2, 403, None),
            (3, 301, Some("/api/v3/repositories/1/issues/3")),
            (4, 301, None),
            (5, 301, None),
            (6, 301, Some("/api/v3///elsewhere.example/issues/6")),
        ] {
            let path = format!("repos/o/r/issues/{n}");
            match client.get(&target, &path, &[]).await {
                Ok(Got::Inaccessible(s)) if to.is_none() && status != 301 => {
                    assert_eq!(s.as_u16(), status)
                }
                Ok(Got::Moved(s, moved)) if status == 301 => {
                    // The checked URL itself, on the same host.
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
        assert_eq!(client.requests().sent["GET"], 7, "no redirect was followed");
        // Following one goes exactly where it was checked to go.
        let Ok(Got::Moved(_, Some(to))) = client.get(&target, "repos/o/r/issues/6", &[]).await
        else {
            panic!("moved")
        };
        match client.get_url(&target, to).await {
            Ok(Got::Found(v)) => assert_eq!(v["id"], 6),
            _ => panic!("followed"),
        }
        assert_eq!(client.requests().refused, 0);
    }

    #[test]
    fn absurd_waits_are_capped() {
        let answer = |headers: &[(&'static str, &str)]| Answer {
            status: StatusCode::TOO_MANY_REQUESTS,
            headers: headers
                .iter()
                .map(|&(k, v)| (header::HeaderName::from_static(k), v.parse().unwrap()))
                .collect(),
            body: vec![],
        };
        let max = u64::MAX.to_string();
        for (headers, limit) in [
            (vec![("retry-after", max.as_str())], SECONDARY),
            (vec![("retry-after", "7200")], SECONDARY),
            (
                vec![
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", max.as_str()),
                ],
                "core",
            ),
        ] {
            let before = SystemTime::now();
            let (hit, until) = used_up(&answer(&headers), "core").unwrap();
            assert_eq!(hit, limit, "{headers:?}");
            let wait = until.duration_since(before).unwrap();
            assert!(
                wait <= MAX_WAIT + Duration::from_secs(1)
                    && wait >= MAX_WAIT - Duration::from_secs(1),
                "{headers:?}: {wait:?}"
            );
        }
    }

    #[test]
    fn kept_answers_are_bounded_in_bytes_too() {
        let cached = |size| Cached {
            etag: HeaderValue::from_static("x"),
            body: Value::Null,
            size,
        };
        let mut cache = Cache::default();
        let half = MAX_CACHED_BYTES / 2;
        cache.insert((1, "a".into()), cached(half));
        cache.insert((1, "a".into()), cached(half));
        cache.insert((1, "b".into()), cached(half));
        assert_eq!((cache.answers.len(), cache.bytes), (2, MAX_CACHED_BYTES));
        // Past the bound, it starts over.
        cache.insert((1, "c".into()), cached(1));
        assert_eq!((cache.answers.len(), cache.bytes), (1, 1));
        // Too large to keep at all.
        cache.insert((1, "d".into()), cached(MAX_CACHED_BYTES + 1));
        assert_eq!(cache.answers.len(), 1);
    }
}
