//! GitHub's OAuth endpoints for a GitHub App: the one exception to Workboard
//! only reading GitHub.
//!
//! Signing a person in to a hub takes a request the read-only client
//! refuses: exchanging an authorization code for a user access token, a
//! form-encoded `POST` to `github.com/login/...`, never to the API.
//! [`TokenClient`] sends that and nothing else. Its one send passes a `POST`
//! only to the exact token URL, with a form body of exactly the fields that
//! request takes, and without credential headers. Everything else, the API included, is refused before it is sent,
//! and counted. Who signed in is then read with `GET /user`, through the
//! read-only client.

use crate::{
    clock::Timestamp,
    connector::{ConnectorError, ConnectorResult, Credential, ErrorKind, Requests},
    connectors::{client, http_error, parse_json, read_body},
};
use reqwest::{
    header::{self, HeaderValue},
    Client, Method, Request,
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fmt,
    sync::{Mutex, PoisonError},
    time::{Duration, SystemTime},
};
use url::{form_urlencoded, Url};

/// github.com, where the App lives.
const GITHUB: &str = "https://github.com/";
/// Where a browser is sent to authorize the App, under the site.
const AUTHORIZE: &str = "login/oauth/authorize";
/// Where codes are exchanged for tokens, under the site.
const ACCESS_TOKEN: &str = "login/oauth/access_token";
const FORM: &str = "application/x-www-form-urlencoded";
const ACCEPT: &str = "application/json";
const USER_AGENT: &str = concat!("workboard/", env!("CARGO_PKG_VERSION"));
/// How GitHub's sign-in is named in messages.
const SOURCE: &str = "GitHub's sign-in";

/// The form fields of the request the client sends, exactly.
const CODE_EXCHANGE: &[&str] = &[
    "client_id",
    "client_secret",
    "code",
    "code_verifier",
    "redirect_uri",
];
/// Longest error code kept from an answer.
const MAX_CODE: usize = 64;

/// The site whose OAuth endpoints are used: github.com. Tests point it at a
/// fixture; nothing else can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    web: Url,
    authorize: Url,
    token: Url,
}

impl Site {
    pub fn github() -> Self {
        Self::at(Url::parse(GITHUB).expect("invariant: a valid constant URL"))
    }
    fn at(web: Url) -> Self {
        let join = |path| {
            web.join(path)
                .expect("invariant: a constant relative path joins any base")
        };
        Self {
            authorize: join(AUTHORIZE),
            token: join(ACCESS_TOKEN),
            web,
        }
    }
    /// A fixture's site, over plain HTTP on loopback.
    #[cfg(test)]
    pub fn fixture(web: Url) -> Self {
        Self::at(web)
    }
    /// The site's web address, from which the read-only client finds its API.
    pub fn web(&self) -> &Url {
        &self.web
    }
    /// Where a browser authorizes the App.
    pub fn authorize(&self) -> &Url {
        &self.authorize
    }
}

/// Whether `request`, exactly as it would be sent, is the code exchange, at
/// GitHub's token endpoint.
fn check(site: &Site, request: &Request) -> Result<(), String> {
    let url = request.url();
    if request.method() != Method::POST {
        return Err(format!("{} {}", request.method(), url.path()));
    }
    let shapes: &[&[&str]] = if *url == site.token {
        &[CODE_EXCHANGE]
    } else {
        return Err(format!(
            "a POST to anything but GitHub's sign-in: {}",
            url.path()
        ));
    };
    let headers = request.headers();
    if headers.get(header::CONTENT_TYPE).map(HeaderValue::as_bytes) != Some(FORM.as_bytes()) {
        return Err("a body that isn't form-encoded".into());
    }
    // An explicit Host could name another site than the URL does.
    if let Some(name) = [
        header::AUTHORIZATION,
        header::COOKIE,
        header::PROXY_AUTHORIZATION,
        header::HOST,
    ]
    .iter()
    .find(|name| headers.contains_key(*name))
    {
        return Err(format!("a request with a {name} header"));
    }
    let body = request
        .body()
        .and_then(|b| b.as_bytes())
        .ok_or("a request without a body")?;
    let mut fields = BTreeSet::new();
    for (key, value) in form_urlencoded::parse(body) {
        if value.is_empty() || !fields.insert(key.to_string()) {
            return Err(format!("an empty or repeated field '{key}'"));
        }
    }
    let matches = |shape: &&[&str]| fields.iter().map(String::as_str).eq(shape.iter().copied());
    if !shapes.iter().any(matches) {
        let fields: Vec<_> = fields.into_iter().collect();
        return Err(format!("fields {} at {}", fields.join(", "), url.path()));
    }
    Ok(())
}

/// A user access token the App was given, with when it and its refresh token
/// expire. Never printed.
#[derive(Debug, Clone)]
pub struct UserToken {
    pub access: Credential,
    pub expires_at: Option<Timestamp>,
    pub refresh: Option<Credential>,
    pub refresh_expires_at: Option<Timestamp>,
}

/// What GitHub's sign-in answered instead of a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The person hasn't entered the device code yet.
    Pending,
    /// Polling too often: wait this many seconds between polls from now on,
    /// if it says.
    SlowDown(Option<u64>),
    /// The device code or authorization code expired.
    Expired,
    /// The person declined.
    Denied,
    /// Another refusal, by its code, such as `bad_verification_code` or
    /// `device_flow_disabled`.
    Other(String),
}

/// Why no token came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthError {
    Refused(Refused),
    Failed(ConnectorError),
}

impl From<ConnectorError> for OAuthError {
    fn from(e: ConnectorError) -> Self {
        Self::Failed(e)
    }
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(Refused::Pending) => f.write_str("GitHub is still waiting for the code"),
            Self::Refused(Refused::SlowDown(_)) => f.write_str("GitHub asked to poll less often"),
            Self::Refused(Refused::Expired) => {
                f.write_str("The sign-in expired before it was finished; start again")
            }
            Self::Refused(Refused::Denied) => f.write_str("The sign-in was declined on GitHub"),
            Self::Refused(Refused::Other(code)) => write!(f, "GitHub refused the sign-in ({code})"),
            Self::Failed(e) => e.fmt(f),
        }
    }
}

/// The error code of an answer, if it has one, kept only if it is a plain
/// code, since it is shown.
fn refusal(answer: &Value) -> Option<Refused> {
    let code = answer.get("error")?.as_str().unwrap_or_default();
    Some(match code {
        "authorization_pending" => Refused::Pending,
        "slow_down" => Refused::SlowDown(answer["interval"].as_u64()),
        "expired_token" | "token_expired" => Refused::Expired,
        "access_denied" => Refused::Denied,
        code if !code.is_empty()
            && code.len() <= MAX_CODE
            && code.chars().all(|c| c.is_ascii_lowercase() || c == '_') =>
        {
            Refused::Other(code.into())
        }
        _ => Refused::Other("unknown".into()),
    })
}

/// When something that lives `seconds` from `now` expires.
fn expiry(now: SystemTime, seconds: Option<u64>) -> Option<Timestamp> {
    seconds.map(|s| Timestamp::of(now).after(Duration::from_secs(s)))
}

/// The user token in a token answer.
fn user_token(answer: &Value, now: SystemTime) -> ConnectorResult<UserToken> {
    let invalid = || ConnectorError::invalid("GitHub's sign-in answered without a usable token");
    let token = |key: &str| -> ConnectorResult<Option<Credential>> {
        answer[key]
            .as_str()
            .map(|t| Credential::new(t).map_err(|_| invalid()))
            .transpose()
    };
    if !answer["token_type"]
        .as_str()
        .is_none_or(|t| t.eq_ignore_ascii_case("bearer"))
    {
        return Err(invalid());
    }
    Ok(UserToken {
        access: token("access_token")?.ok_or_else(invalid)?,
        expires_at: expiry(now, answer["expires_in"].as_u64()),
        refresh: token("refresh_token")?,
        refresh_expires_at: expiry(now, answer["refresh_token_expires_in"].as_u64()),
    })
}

/// The client for GitHub's OAuth endpoints, and nothing else.
pub struct TokenClient {
    client: Client,
    site: Site,
    counts: Mutex<Requests>,
}

impl TokenClient {
    pub fn new(site: Site) -> anyhow::Result<Self> {
        Ok(Self {
            client: client()?,
            site,
            counts: Mutex::default(),
        })
    }
    pub fn site(&self) -> &Site {
        &self.site
    }
    /// What it sent so far, and refused to.
    #[cfg(test)]
    pub fn requests(&self) -> Requests {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    fn count(&self, update: impl FnOnce(&mut Requests)) {
        update(&mut self.counts.lock().unwrap_or_else(PoisonError::into_inner));
    }
    /// A form-encoded POST of `fields` to `url`.
    fn form(url: &Url, fields: &[(&str, &str)]) -> Request {
        let mut request = Request::new(Method::POST, url.clone());
        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        let headers = request.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(FORM));
        headers.insert(header::ACCEPT, HeaderValue::from_static(ACCEPT));
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        *request.body_mut() = Some(body.into());
        request
    }
    /// Send `request` if it is one of the OAuth requests, and read its JSON
    /// answer. Every request of this client goes through here.
    async fn send(&self, request: Request) -> ConnectorResult<Value> {
        if let Err(why) = check(&self.site, &request) {
            self.count(|c| c.refused += 1);
            return Err(ConnectorError::invalid(format!(
                "Refused to send GitHub's sign-in anything but its own requests: {why}"
            )));
        }
        let sent = format!("POST {}", request.url().path());
        self.count(|c| *c.sent.entry(sent).or_default() += 1);
        // The one place an OAuth request leaves, after `check` passed it.
        #[allow(clippy::disallowed_methods)]
        let response = self.client.execute(request).await.map_err(|_| {
            ConnectorError::new(
                ErrorKind::Transient,
                "Cannot reach GitHub's sign-in; check network and certificate trust",
            )
        })?;
        let status = response.status();
        let body = read_body(SOURCE, response).await?;
        // GitHub answers refusals with 200 and an error code; a 400 can
        // carry one too.
        match parse_json(SOURCE, &body) {
            Ok(answer) if status.is_success() || answer.get("error").is_some() => Ok(answer),
            _ => Err(http_error(status)),
        }
    }
    /// Exchange the authorization code a browser brought back for a user
    /// token, with the PKCE verifier its challenge was made from.
    pub async fn exchange_code(
        &self,
        client_id: &str,
        secret: &Credential,
        code: &str,
        redirect_uri: &Url,
        verifier: &str,
    ) -> Result<UserToken, OAuthError> {
        let request = Self::form(
            &self.site.token,
            &[
                ("client_id", client_id),
                ("client_secret", secret.expose()),
                ("code", code),
                ("redirect_uri", redirect_uri.as_str()),
                ("code_verifier", verifier),
            ],
        );
        let answer = self.send(request).await?;
        match refusal(&answer) {
            Some(refused) => Err(OAuthError::Refused(refused)),
            None => Ok(user_token(&answer, SystemTime::now())?),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, Method as AxumMethod, StatusCode, Uri},
        response::{IntoResponse, Response},
        Json, Router,
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde_json::json;
    use std::{collections::HashMap, sync::Arc};

    pub const CLIENT_ID: &str = "Iv23test";
    pub const SECRET: &str = "test-only-client-secret";
    /// The authorization code the fake GitHub accepts once.
    pub const CODE: &str = "test-only-code";

    /// What reached the fake GitHub: method, path, and the form fields.
    pub type Seen = Vec<(String, String, HashMap<String, String>)>;

    /// A fake GitHub: the OAuth endpoints and `/user`, with what reached it.
    #[derive(Default)]
    pub struct Fake {
        pub seen: Seen,
        /// The PKCE challenge the next code exchange must match.
        pub challenge: Option<String>,
        /// The user each access token belongs to: numeric ID and login.
        pub users: HashMap<String, (u64, String)>,
        /// The token the next code exchange gives.
        pub token: String,
    }

    pub type Shared = Arc<Mutex<Fake>>;

    /// A token answer as GitHub gives a GitHub App's user.
    pub fn token_answer(token: &str) -> Value {
        json!({
            "access_token": token,
            "expires_in": 28800,
            "refresh_token": "ghr_test-only-refresh",
            "refresh_token_expires_in": 15897600,
            "scope": "",
            "token_type": "bearer",
        })
    }

    fn answer(
        fake: &Shared,
        method: &AxumMethod,
        uri: &Uri,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Response {
        let fields: HashMap<String, String> = form_urlencoded::parse(body).into_owned().collect();
        let mut fake = fake.lock().unwrap();
        fake.seen
            .push((method.to_string(), uri.path().to_string(), fields.clone()));
        let field = |k: &str| fields.get(k).map(String::as_str);
        match (method.as_str(), uri.path()) {
            ("POST", "/login/oauth/access_token") => {
                let verifier = field("code_verifier").unwrap_or_default();
                let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
                let pkce = fake.challenge.take() == Some(URL_SAFE_NO_PAD.encode(digest));
                let client =
                    field("client_id") == Some(CLIENT_ID) && field("client_secret") == Some(SECRET);
                if !client {
                    return Json(json!({"error": "incorrect_client_credentials"})).into_response();
                }
                if field("code") != Some(CODE) || !pkce {
                    return Json(json!({"error": "bad_verification_code"})).into_response();
                }
                Json(token_answer(&fake.token)).into_response()
            }
            ("GET", "/api/v3/user") => {
                let token = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .unwrap_or_default();
                match fake.users.get(token) {
                    Some((id, login)) => Json(json!({"id": id, "login": login})).into_response(),
                    None => StatusCode::UNAUTHORIZED.into_response(),
                }
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    /// Serve a fake GitHub on loopback, and its site.
    pub async fn fake_github() -> (Site, Shared) {
        let fake = Shared::default();
        let handler =
            |State(fake): State<Shared>,
             method: AxumMethod,
             uri: Uri,
             headers: HeaderMap,
             body: Bytes| async move { answer(&fake, &method, &uri, &headers, &body) };
        let router = Router::new().fallback(handler).with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let web = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (Site::fixture(web), fake)
    }

    fn github_request(url: &str, fields: &[(&str, &str)]) -> Request {
        TokenClient::form(&Url::parse(url).unwrap(), fields)
    }

    // Requests of other methods are built here only to be refused.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn only_the_code_exchange_passes() {
        const TOKEN: &str = "https://github.com/login/oauth/access_token";
        let exchange = [
            ("client_id", "c"),
            ("client_secret", "s"),
            ("code", "x"),
            ("redirect_uri", "https://hub.example/auth/github/callback"),
            ("code_verifier", "v"),
        ];
        let site = Site::github();
        let with = |mut request: Request, edit: fn(&mut Request)| {
            edit(&mut request);
            request
        };
        // (what, request, passes)
        let cases = [
            ("code exchange", github_request(TOKEN, &exchange), true),
            (
                "the API",
                github_request("https://api.github.com/user", &[("client_id", "c")]),
                false,
            ),
            (
                "GraphQL",
                github_request("https://api.github.com/graphql", &[("client_id", "c")]),
                false,
            ),
            (
                "authorize",
                github_request("https://github.com/login/oauth/authorize", &exchange),
                false,
            ),
            (
                "plain HTTP",
                github_request("http://github.com/login/oauth/access_token", &exchange),
                false,
            ),
            (
                "a query",
                github_request(&format!("{TOKEN}?x=1"), &exchange),
                false,
            ),
            (
                "another host",
                github_request(
                    "https://github.com.example/login/oauth/access_token",
                    &exchange,
                ),
                false,
            ),
            (
                "credentials in the URL",
                github_request(
                    "https://me:x@github.com/login/oauth/access_token",
                    &exchange,
                ),
                false,
            ),
            (
                "an extra field",
                github_request(TOKEN, &[&exchange[..], &[("scope", "repo")][..]].concat()),
                false,
            ),
            (
                "a missing field",
                github_request(TOKEN, &exchange[..4]),
                false,
            ),
            (
                "a repeated field",
                github_request(TOKEN, &[&exchange[..], &exchange[..1]].concat()),
                false,
            ),
            (
                "an empty field",
                github_request(
                    TOKEN,
                    &[&exchange[..4], &[("code_verifier", "")][..]].concat(),
                ),
                false,
            ),
            (
                "the device-code URL",
                github_request(
                    "https://github.com/login/device/code",
                    &[("client_id", "c")],
                ),
                false,
            ),
            (
                "GET",
                with(github_request(TOKEN, &exchange), |r| {
                    *r.method_mut() = Method::GET
                }),
                false,
            ),
            (
                "PUT",
                with(github_request(TOKEN, &exchange), |r| {
                    *r.method_mut() = Method::PUT
                }),
                false,
            ),
            (
                "DELETE",
                with(github_request(TOKEN, &exchange), |r| {
                    *r.method_mut() = Method::DELETE
                }),
                false,
            ),
            (
                "JSON",
                with(github_request(TOKEN, &exchange), |r| {
                    r.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                }),
                false,
            ),
            (
                "no content type",
                with(github_request(TOKEN, &exchange), |r| {
                    r.headers_mut().remove(header::CONTENT_TYPE);
                }),
                false,
            ),
            (
                "no body",
                with(github_request(TOKEN, &exchange), |r| *r.body_mut() = None),
                false,
            ),
            (
                "a token header",
                with(github_request(TOKEN, &exchange), |r| {
                    r.headers_mut()
                        .insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer x"));
                }),
                false,
            ),
            (
                "an explicit Host",
                with(github_request(TOKEN, &exchange), |r| {
                    r.headers_mut()
                        .insert(header::HOST, HeaderValue::from_static("api.github.com"));
                }),
                false,
            ),
            (
                "a cookie",
                with(github_request(TOKEN, &exchange), |r| {
                    r.headers_mut()
                        .insert(header::COOKIE, HeaderValue::from_static("a=b"));
                }),
                false,
            ),
        ];
        for (what, request, passes) in cases {
            let checked = check(&site, &request);
            assert_eq!(checked.is_ok(), passes, "{what}: {checked:?}");
        }
    }

    #[tokio::test]
    async fn refused_requests_never_reach_github() {
        let (site, fake) = fake_github().await;
        let client = TokenClient::new(site.clone()).unwrap();
        let api = site.web().join("api/v3/user").unwrap();
        for request in [
            TokenClient::form(&api, &[("client_id", CLIENT_ID)]),
            TokenClient::form(site.authorize(), &[("client_id", CLIENT_ID)]),
            TokenClient::form(&site.token, &[("client_id", CLIENT_ID)]),
        ] {
            let error = client.send(request).await.unwrap_err();
            assert!(error.message.starts_with("Refused"), "{error}");
        }
        assert!(
            fake.lock().unwrap().seen.is_empty(),
            "nothing reached GitHub"
        );
        assert_eq!(client.requests().refused, 3);
        assert!(client.requests().sent.is_empty());
    }

    #[tokio::test]
    async fn codes_are_exchanged_with_their_pkce_verifier() {
        let (site, fake) = fake_github().await;
        let client = TokenClient::new(site).unwrap();
        let secret = Credential::new(SECRET).unwrap();
        let callback = Url::parse("https://hub.example.ts.net/auth/github/callback").unwrap();
        let verifier = "test-only-verifier-of-enough-length-for-pkce-0123";
        let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
        // (the challenge GitHub was given, the code, what the exchange gets)
        for (challenge, code, expected) in [
            (URL_SAFE_NO_PAD.encode(digest), CODE, None),
            (
                "another-challenge".to_string(),
                CODE,
                Some(Refused::Other("bad_verification_code".into())),
            ),
            (
                URL_SAFE_NO_PAD.encode(digest),
                "wrong",
                Some(Refused::Other("bad_verification_code".into())),
            ),
        ] {
            {
                let mut fake = fake.lock().unwrap();
                fake.challenge = Some(challenge);
                fake.token = "ghu_test-only".into();
            }
            let got = client
                .exchange_code(CLIENT_ID, &secret, code, &callback, verifier)
                .await;
            match (got, expected) {
                (Ok(token), None) => {
                    assert_eq!(token.access.expose(), "ghu_test-only");
                    assert!(token.expires_at.is_some() && token.refresh_expires_at.is_some());
                    assert_eq!(token.refresh.unwrap().expose(), "ghr_test-only-refresh");
                }
                (Err(OAuthError::Refused(r)), Some(expected)) => assert_eq!(r, expected),
                (got, expected) => panic!("{got:?}, expected {expected:?}"),
            }
        }
        let seen = fake.lock().unwrap().seen.clone();
        let (method, path, fields) = &seen[0];
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("POST", "/login/oauth/access_token")
        );
        assert_eq!(fields["redirect_uri"], callback.as_str());
        assert_eq!(fields["code_verifier"], verifier);
        let sent: Vec<_> = client.requests().sent.into_iter().collect();
        assert_eq!(sent, [("POST /login/oauth/access_token".to_string(), 3)]);
    }

    #[test]
    fn answers_are_checked() {
        let now = SystemTime::now();
        for bad in [
            json!({}),
            json!({"access_token": ""}),
            json!({"access_token": "ghu_x", "token_type": "mac"}),
            json!({"access_token": 1}),
        ] {
            assert!(user_token(&bad, now).is_err(), "{bad}");
        }
        let token = user_token(&json!({"access_token": "ghu_x"}), now).unwrap();
        assert!(token.expires_at.is_none() && token.refresh.is_none());
    }
}
