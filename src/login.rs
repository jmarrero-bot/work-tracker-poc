//! Signing in to a hub with a GitHub App: GitHub's web flow for a browser,
//! which gives it a session, and GitHub's device flow for a companion,
//! which gives it a credential that reads the hub's external export.
//!
//! Who signed in is the numeric GitHub user ID that `GET /user` answers
//! with the new user token, read through the read-only client. Only IDs on
//! the hub's allowlist get in; a login is never compared, since logins can
//! be renamed and then taken by someone else.
//!
//! Secrets are never kept as given where a hash will do: sessions, pending
//! sign-ins, device flows and companion credentials are looked up by the
//! SHA-256 of what the client presents, so neither a lookup's timing nor
//! the hub's files give one away. The user token the web flow gets is kept
//! in a private file for posting commands as the user later (#13).

use crate::{
    clock::{Clock, Timestamp},
    connector::{Credential, ErrorKind, Target},
    connectors, db,
    hub::{DevicePoll, DeviceStart},
    oauth::{self, OAuthError, Refused, Site, TokenClient, UserToken},
    server::{matches_token, new_token, replace_secret},
};
use anyhow::{bail, Context, Result};
use axum::http::{HeaderMap, StatusCode};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime},
};
use url::Url;

/// Where GitHub sends the browser back, on the hub's name.
pub const CALLBACK_PATH: &str = "/auth/github/callback";
/// The browser session's cookie. `__Host-` makes browsers keep it to the
/// hub's exact name, over HTTPS, for every path.
pub const SESSION_COOKIE: &str = "__Host-workboard_user";
/// The cookie that binds a sign-in to the browser that started it.
pub const LOGIN_COOKIE: &str = "__Host-workboard_login";
/// How long a browser has to come back from GitHub.
pub const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);
/// A session ends after this long unused; each use renews it.
pub const SESSION_IDLE: Duration = Duration::from_secs(12 * 60 * 60);
/// A session ends this long after sign-in, however much it is used.
pub const SESSION_MAX: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Most sign-ins waiting for their browser, sessions, and device flows.
const MAX_PENDING: usize = 16;
const MAX_SESSIONS: usize = 64;
/// Most sign-in attempts in [`ATTEMPT_WINDOW`] from one peer, and from all
/// of them together, a backstop.
const PEER_ATTEMPTS: usize = 10;
const ALL_ATTEMPTS: usize = 100;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(60);
/// Most peers whose attempts are counted apart; past it, only the backstop
/// counts a new one's.
const MAX_PEERS: usize = 1024;
/// Longest peer address kept.
const MAX_PEER: usize = 64;
/// The peer of a request that came straight to the loopback port, not
/// through the front.
const DIRECT: &str = "this machine";
/// The peer of a request through a front that didn't name its address.
const UNNAMED: &str = "an unnamed peer through the front";
/// How often expired sessions and sign-ins are swept away.
pub const SWEEP: Duration = Duration::from_secs(60);
const MAX_DEVICES: usize = 4;
/// Most companion credentials one user keeps; a new one drops the oldest.
const MAX_COMPANIONS: usize = 4;
/// What GitHub adds to the poll interval when it says to slow down.
const SLOW_DOWN: Duration = Duration::from_secs(5);
/// The App's client secret, in the data directory.
pub const SECRET_FILE: &str = "github-app.secret";
/// Where the user tokens are kept, one private file per user ID.
const USERS_DIR: &str = "github-users";
/// The companion credentials the device flow gave, by their hashes.
const COMPANIONS_FILE: &str = "companions.json";
/// Longest client ID accepted.
const MAX_CLIENT_ID: usize = 64;
/// Longest authorization code accepted from a callback.
const MAX_CODE: usize = 512;

/// The GitHub App a hub signs people in with.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub client_id: String,
    /// Where GitHub sends the browser back: HTTPS on one of the hub's
    /// names, at [`CALLBACK_PATH`].
    pub callback: Url,
    /// The numeric GitHub user IDs allowed in.
    pub users: BTreeSet<u64>,
}

/// `url`'s host, lowercase, with its port unless it is the scheme's.
fn authority(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

impl AppConfig {
    /// The App's settings, checked against `hosts`, the hub's names.
    pub fn new(client_id: &str, callback: &str, users: &[u64], hosts: &[String]) -> Result<Self> {
        let client_id = client_id.trim();
        if client_id.is_empty()
            || client_id.len() > MAX_CLIENT_ID
            || !client_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.')
        {
            bail!("Give the GitHub App's client ID, as its settings page shows it (such as Iv23li...)")
        }
        let shape = format!(
            "Give the callback URL as https://NAME{CALLBACK_PATH}, NAME being one of the hub's --allowed-host names"
        );
        let callback = Url::parse(callback.trim()).context(shape.clone())?;
        if callback.scheme() != "https"
            || !callback.username().is_empty()
            || callback.password().is_some()
            || callback.query().is_some()
            || callback.fragment().is_some()
            || callback.path() != CALLBACK_PATH
            || !authority(&callback).is_some_and(|host| hosts.contains(&host))
        {
            bail!("{shape}")
        }
        if users.is_empty() || users.contains(&0) {
            bail!("Allow at least one GitHub user, by numeric ID (--github-user); a login is never enough")
        }
        Ok(Self {
            client_id: client_id.into(),
            callback,
            users: users.iter().copied().collect(),
        })
    }
    /// The hub's name that people sign in on: the callback's.
    pub fn host(&self) -> String {
        authority(&self.callback).expect("invariant: the callback was checked to have a host")
    }
}

/// The App's client secret, from its private file in the data directory.
pub fn read_secret(root: &Path) -> Result<Credential> {
    let path = root.join(SECRET_FILE);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "Cannot read the GitHub App's client secret at {}; set it with `workboard github-app set-secret`",
            path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "{} can be read by others; make it private with chmod 600",
                path.display()
            )
        }
    }
    Credential::new(&text).map_err(|_| anyhow::anyhow!("{} holds no client secret", path.display()))
}

/// Keep the App's client secret in its private file, replacing any before.
pub fn save_secret(root: &Path, value: &str) -> Result<()> {
    let secret = Credential::new(value).map_err(|_| anyhow::anyhow!("Give a client secret"))?;
    db::private_dir(root)?;
    replace_secret(&root.join(SECRET_FILE), secret.expose())
}

/// The SHA-256 of a secret, as what it is looked up and kept by.
pub(crate) fn hashed(secret: &str) -> String {
    URL_SAFE_NO_PAD.encode(ring::digest::digest(
        &ring::digest::SHA256,
        secret.as_bytes(),
    ))
}

/// The PKCE challenge of `verifier`: its SHA-256, base64url-encoded.
fn pkce_challenge(verifier: &str) -> String {
    hashed(verifier)
}

/// How long ago `then` was; no time at all if the wall clock stepped back
/// past it, so a small correction doesn't sign everyone out. That only
/// lengthens a lifetime by the step.
fn elapsed(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or_default()
}

/// Who sent a request, as the hub's front says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The tailnet address, or [`DIRECT`], or [`UNNAMED`].
    pub address: String,
    /// The tailnet user `tailscale serve` names, if it named one.
    pub tailnet_user: Option<String>,
}

impl From<&str> for Peer {
    fn from(address: &str) -> Self {
        Self {
            address: address.into(),
            tailnet_user: None,
        }
    }
}

/// The headers `tailscale serve` names a tailnet user with: their login,
/// and their display name. It drops any copy a peer sent.
const TAILNET_USER: [&str; 2] = ["tailscale-user-login", "tailscale-user-name"];

/// The last value of the header `name`, or of its last comma-separated
/// entry with `last_entry`, bounded and without control characters, if
/// there is any.
fn front_header(headers: &HeaderMap, name: &str, last_entry: bool) -> Option<String> {
    let last = headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| match last_entry {
            true => v.split(',').collect::<Vec<_>>(),
            false => vec![v],
        })
        .map(str::trim)
        .rfind(|s| !s.is_empty())?;
    let mut value: String = last.chars().filter(|c| !c.is_control()).collect();
    value.truncate(MAX_PEER);
    Some(value)
}

/// Who sent a request, as the hub's front says: the last `X-Forwarded-For`
/// entry, and the tailnet user it names. `tailscale serve` sets those
/// headers itself, to the tailnet address of the peer, dropping any copy
/// the peer sent, and a proxy that keeps a peer's copy appends its own
/// entry after it, so a peer can't pass for another.
///
/// Only a request `from_front`, any that didn't come straight to the
/// loopback port without a proxy's headers, has any; one that did is this
/// machine's. A request through the front is never taken for this
/// machine's, whatever Host it names, even the loopback one. The service
/// can't tell a local process that sends a proxy's headers to the port
/// itself from the front, which also connects on loopback.
pub fn peer(headers: &HeaderMap, from_front: bool) -> Peer {
    if !from_front {
        return Peer::from(DIRECT);
    }
    Peer {
        address: front_header(headers, "x-forwarded-for", true).unwrap_or_else(|| UNNAMED.into()),
        tailnet_user: TAILNET_USER
            .iter()
            .find_map(|name| front_header(headers, name, false)),
    }
}

/// Drop the attempts in `attempts` older than [`ATTEMPT_WINDOW`].
fn prune(now: SystemTime, attempts: &mut VecDeque<SystemTime>) {
    while attempts
        .front()
        .is_some_and(|&t| elapsed(now, t) >= ATTEMPT_WINDOW || t > now)
    {
        attempts.pop_front();
    }
}

/// Sign-in attempts in the last [`ATTEMPT_WINDOW`], by peer and in all.
#[derive(Default)]
struct Limiter {
    peers: HashMap<String, VecDeque<SystemTime>>,
    all: VecDeque<SystemTime>,
}

impl Limiter {
    /// Count an attempt `peer` made `now`, or refuse it past
    /// [`PEER_ATTEMPTS`] of that peer's or [`ALL_ATTEMPTS`] in all.
    fn attempt(&mut self, now: SystemTime, peer: &str) -> Result<(), LoginError> {
        self.peers.retain(|_, attempts| {
            prune(now, attempts);
            !attempts.is_empty()
        });
        prune(now, &mut self.all);
        let peers = self
            .peers
            .get(peer)
            .filter(|a| a.len() >= PEER_ATTEMPTS)
            .and_then(|a| a.front());
        let all = Some(&self.all)
            .filter(|a| a.len() >= ALL_ATTEMPTS)
            .and_then(|a| a.front());
        if let Some(&first) = peers.or(all) {
            return Err(LoginError {
                retry_after: Some(ATTEMPT_WINDOW.saturating_sub(elapsed(now, first))),
                ..LoginError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many sign-in attempts; try again in a minute",
                )
            });
        }
        self.all.push_back(now);
        if self.peers.len() < MAX_PEERS || self.peers.contains_key(peer) {
            self.peers.entry(peer.into()).or_default().push_back(now);
        }
        Ok(())
    }
}

/// Forget the user tokens kept for sessions, as a service does when it
/// starts and when it stops: sessions don't outlive the process, so neither
/// do they.
pub fn forget_user_tokens(root: &Path) -> Result<()> {
    let users = root.join(USERS_DIR);
    match std::fs::remove_dir_all(&users) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("Cannot remove {}", users.display()))
        }
        _ => Ok(()),
    }
}

/// Who signed in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    /// GitHub's numeric user ID, which is what is allowed.
    pub id: u64,
    /// The login at sign-in, for showing only.
    pub login: String,
}

/// Why a sign-in step was refused, as the person is told. Never carries a
/// token or code.
#[derive(Debug, PartialEq, Eq)]
pub struct LoginError {
    pub status: StatusCode,
    pub message: String,
    /// For a rate limit, when to try again.
    pub retry_after: Option<Duration>,
}

impl LoginError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after: None,
        }
    }
    fn expired() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "This sign-in expired, or was started in another browser; start again",
        )
    }
    fn mismatch() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "This sign-in doesn't match the one this browser started; start again",
        )
    }
    fn not_allowed() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "That GitHub account isn't allowed on this hub",
        )
    }
    fn internal(what: &str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("Cannot {what}"))
    }
    fn from_oauth(error: OAuthError) -> Self {
        let status = match &error {
            OAuthError::Refused(Refused::Denied) => StatusCode::FORBIDDEN,
            OAuthError::Refused(Refused::Expired) => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        };
        Self::new(status, error.to_string())
    }
}

/// A sign-in waiting for its browser to come back from GitHub.
struct Pending {
    /// The `state` sent to GitHub, which must come back.
    state: String,
    /// The PKCE verifier its challenge was made from.
    verifier: String,
    started: SystemTime,
}

struct Session {
    user: Identity,
    started: SystemTime,
    seen: SystemTime,
}

/// A companion's device flow, waiting for the person to enter the code on
/// GitHub, and for a signed-in browser to approve it on the hub's page.
struct Device {
    /// What the hub's page names it by, to approve or deny it: random, and
    /// never the companion's handle.
    id: String,
    code: Credential,
    /// The code the companion shows, which the page shows too, so the
    /// person can tell their own sign-in from another.
    user_code: String,
    /// Who asked for it, as the hub's front says.
    peer: Peer,
    started: SystemTime,
    interval: Duration,
    next_poll: SystemTime,
    expires: SystemTime,
    /// Who entered the code on GitHub, once someone allowed did. GitHub
    /// isn't asked again after that.
    user: Option<Identity>,
    /// Who approved it on the hub's page.
    approved: Option<Identity>,
}

/// What the hub's page shows a signed-in user of the companions' sign-ins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceRequests {
    /// Those whose code that user's account entered.
    pub requests: Vec<DeviceRequest>,
    /// How many wait for anyone to enter their code.
    pub waiting: usize,
}

/// A companion's sign-in waiting on the hub, as its page shows it to a
/// signed-in user, to approve or deny.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceRequest {
    pub id: String,
    /// The code the companion shows.
    pub user_code: String,
    pub started: Timestamp,
    pub expires: Timestamp,
    /// The tailnet address it came from, or "this machine".
    pub peer: String,
    /// The tailnet user that asked for it, as `tailscale serve` named them,
    /// to tell a sign-in someone else started.
    pub tailnet_user: Option<String>,
    /// The GitHub login that entered the code, once one did.
    pub github_login: Option<String>,
    pub approved: bool,
}

/// A companion credential, kept by its hash, and whose it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Companion {
    user: u64,
    login: String,
    token_hash: String,
    created: Timestamp,
}

#[derive(Default)]
struct State {
    /// By the hash of the browser's login cookie.
    pending: HashMap<String, Pending>,
    /// By the hash of the session cookie.
    sessions: HashMap<String, Session>,
    /// By the hash of the companion's handle.
    devices: HashMap<String, Device>,
    /// The browsers' attempts: sign-in starts, and callbacks without a
    /// sign-in to finish.
    web: Limiter,
    /// The companions' attempts: device-flow starts, and polls with a
    /// handle the hub doesn't know.
    device: Limiter,
    companions: Vec<Companion>,
}

/// A hub's sign-ins: its GitHub App, its sessions and its companion
/// credentials.
pub struct Logins {
    config: AppConfig,
    secret: Credential,
    oauth: TokenClient,
    clock: Arc<dyn Clock>,
    root: PathBuf,
    state: Mutex<State>,
}

impl Logins {
    pub fn new(
        config: AppConfig,
        secret: Credential,
        site: Site,
        clock: Arc<dyn Clock>,
        root: &Path,
    ) -> Result<Self> {
        forget_user_tokens(root)?;
        let path = root.join(COMPANIONS_FILE);
        let kept: Vec<Companion> = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| {
                format!("{} isn't a list of companion credentials", path.display())
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e).with_context(|| format!("Cannot read {}", path.display())),
        };
        // A user taken off the allowlist loses their credentials for good,
        // so adding them back doesn't bring those back.
        let companions: Vec<Companion> = kept
            .iter()
            .filter(|c| config.users.contains(&c.user))
            .cloned()
            .collect();
        if companions.len() != kept.len() {
            replace_secret(&path, &serde_json::to_string(&companions)?)?;
        }
        Ok(Self {
            config,
            secret,
            oauth: TokenClient::new(site)?,
            clock,
            root: root.to_path_buf(),
            state: Mutex::new(State {
                companions,
                ..State::default()
            }),
        })
    }
    pub fn config(&self) -> &AppConfig {
        &self.config
    }
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn now(&self) -> SystemTime {
        self.clock.now()
    }
    /// Start a browser's sign-in: the value of its login cookie, and the
    /// GitHub page to send it to, with a fresh `state` and PKCE challenge.
    pub fn start(&self, peer: &str) -> Result<(String, Url), LoginError> {
        let mut state = self.state();
        let now = self.now();
        state.web.attempt(now, peer)?;
        state
            .pending
            .retain(|_, p| elapsed(now, p.started) < LOGIN_TTL);
        if state.pending.len() >= MAX_PENDING {
            let oldest = state
                .pending
                .iter()
                .min_by_key(|(_, p)| p.started)
                .map(|(k, _)| k.clone());
            state.pending.retain(|k, _| Some(k) != oldest.as_ref());
        }
        let (binding, csrf, verifier) = (new_token(), new_token(), new_token());
        let mut url = self.oauth.site().authorize().clone();
        url.query_pairs_mut()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", self.config.callback.as_str())
            .append_pair("state", &csrf)
            .append_pair("code_challenge", &pkce_challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("allow_signup", "false");
        state.pending.insert(
            hashed(&binding),
            Pending {
                state: csrf,
                verifier,
                started: now,
            },
        );
        Ok((binding, url))
    }

    /// Finish a browser's sign-in when GitHub sends it back: check it is
    /// the one this browser started, exchange the code, find out who it is,
    /// and give them a session. Each sign-in is finished at most once.
    pub async fn finish(
        &self,
        peer: &str,
        binding: Option<&str>,
        callback: &Callback,
    ) -> Result<(String, Identity), LoginError> {
        let pending = {
            let mut state = self.state();
            let found = binding.and_then(|b| state.pending.remove(&hashed(b)));
            // Only a callback with nothing to finish counts, so the
            // browser that started a sign-in can always finish it.
            if found.is_none() {
                let now = self.now();
                state.web.attempt(now, peer)?;
            }
            found
        };
        let pending = pending
            .filter(|p| elapsed(self.now(), p.started) < LOGIN_TTL)
            .ok_or_else(LoginError::expired)?;
        if !callback
            .state
            .as_deref()
            .is_some_and(|s| matches_token(s, &pending.state))
        {
            return Err(LoginError::mismatch());
        }
        if let Some(error) = &callback.error {
            let refused = match error.as_str() {
                "access_denied" => Refused::Denied,
                _ => Refused::Other("authorization_failed".into()),
            };
            return Err(LoginError::from_oauth(OAuthError::Refused(refused)));
        }
        let code = callback
            .code
            .as_deref()
            .filter(|c| !c.is_empty() && c.len() <= MAX_CODE)
            .ok_or_else(|| LoginError::new(StatusCode::BAD_REQUEST, "GitHub sent no code back"))?;
        let token = self
            .oauth
            .exchange_code(
                &self.config.client_id,
                &self.secret,
                code,
                &self.config.callback,
                &pending.verifier,
            )
            .await
            .map_err(LoginError::from_oauth)?;
        let user = self.identify(&token.access).await?;
        let session = self
            .open_session(user.clone(), &token)
            .map_err(|_| LoginError::internal("keep the GitHub token"))?;
        Ok((session, user))
    }

    /// Who `token` belongs to, from GitHub's `GET /user`, if they are
    /// allowed: by numeric ID, whatever their login is now.
    async fn identify(&self, token: &Credential) -> Result<Identity, LoginError> {
        let target = Target {
            base_url: self.oauth.site().web().clone(),
            credential: token.clone(),
        };
        let account = connectors::github_account(&target).await.map_err(|e| {
            LoginError::new(
                StatusCode::BAD_GATEWAY,
                format!("Cannot tell who signed in: {e}"),
            )
        })?;
        let id = account.id.parse().map_err(|_| {
            LoginError::new(StatusCode::BAD_GATEWAY, "GitHub's user ID isn't a number")
        })?;
        if !self.config.users.contains(&id) {
            return Err(LoginError::not_allowed());
        }
        Ok(Identity {
            id,
            login: account.name,
        })
    }

    fn token_path(&self, user: u64) -> PathBuf {
        self.root.join(USERS_DIR).join(format!("{user}.json"))
    }

    /// Keep `user`'s token, and its refresh token, in a private file, for
    /// posting commands as them later. Replaces the one before.
    fn keep_token(&self, user: &Identity, token: &UserToken) -> Result<()> {
        db::private_dir(&self.root.join(USERS_DIR))?;
        let kept = serde_json::json!({
            "id": user.id,
            "login": user.login,
            "access_token": token.access.expose(),
            "expires_at": token.expires_at,
            "refresh_token": token.refresh.as_ref().map(Credential::expose),
            "refresh_expires_at": token.refresh_expires_at,
            "kept_at": Timestamp::of(self.now()),
        });
        replace_secret(&self.token_path(user.id), &kept.to_string())
    }

    /// End the sessions `ended` picks, and forget the token of each user
    /// left with none.
    fn end_sessions(&self, state: &mut State, ended: impl Fn(&str, &Session) -> bool) {
        let mut users = BTreeSet::new();
        state.sessions.retain(|key, s| {
            let end = ended(key, s);
            if end {
                users.insert(s.user.id);
            }
            !end
        });
        for user in users {
            if !state.sessions.values().any(|s| s.user.id == user) {
                // Best effort: a restart forgets it anyway.
                let _ = db::remove_if_there(&self.token_path(user));
            }
        }
    }

    /// A new session for `user`, as its cookie's value, with their token
    /// kept. Under one lock, so a logout can't forget the token between.
    fn open_session(&self, user: Identity, token: &UserToken) -> Result<String> {
        let now = self.now();
        let mut state = self.state();
        self.end_sessions(&mut state, |_, s| !self.live(s, now));
        if state.sessions.len() >= MAX_SESSIONS {
            let oldest = state
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.seen)
                .map(|(k, _)| k.clone());
            self.end_sessions(&mut state, |key, _| Some(key) == oldest.as_deref());
        }
        self.keep_token(&user, token)?;
        let id = new_token();
        state.sessions.insert(
            hashed(&id),
            Session {
                user,
                started: now,
                seen: now,
            },
        );
        Ok(id)
    }

    fn live(&self, session: &Session, now: SystemTime) -> bool {
        elapsed(now, session.seen) < SESSION_IDLE
            && elapsed(now, session.started) < SESSION_MAX
            && self.config.users.contains(&session.user.id)
    }

    /// Who the session with cookie value `id` is, if it is live. A use by
    /// the person (`renew`) renews it; the page's own refreshes don't, so
    /// an open page left alone still goes idle.
    pub fn session(&self, id: &str, renew: bool) -> Option<Identity> {
        let now = self.now();
        let key = hashed(id);
        let mut state = self.state();
        let live = state.sessions.get(&key).is_some_and(|s| self.live(s, now));
        if !live {
            self.end_sessions(&mut state, |k, _| k == key);
            return None;
        }
        let session = state.sessions.get_mut(&key)?;
        if renew {
            session.seen = now;
        }
        Some(session.user.clone())
    }

    /// End the sessions and drop the sign-ins that expired, forgetting the
    /// token of each user left with no session. The service does this
    /// every [`SWEEP`], so a token goes soon after its last session, even
    /// with no request to notice.
    pub fn sweep(&self) {
        let now = self.now();
        let mut state = self.state();
        self.end_sessions(&mut state, |_, s| !self.live(s, now));
        state
            .pending
            .retain(|_, p| elapsed(now, p.started) < LOGIN_TTL);
        state.devices.retain(|_, d| now < d.expires);
    }

    /// End the session with cookie value `id`, and forget its user's token
    /// once none of their sessions is left.
    pub fn logout(&self, id: &str) -> Result<()> {
        let mut state = self.state();
        let Some(ended) = state.sessions.remove(&hashed(id)) else {
            return Ok(());
        };
        if !state.sessions.values().any(|s| s.user.id == ended.user.id) {
            db::remove_if_there(&self.token_path(ended.user.id))?;
        }
        Ok(())
    }

    /// Start a companion's device flow: the code its person enters on
    /// GitHub, and the handle the companion polls with.
    pub async fn device_start(&self, peer: impl Into<Peer>) -> Result<DeviceStart, LoginError> {
        let peer = peer.into();
        {
            let mut state = self.state();
            let now = self.now();
            state.device.attempt(now, &peer.address)?;
            state.devices.retain(|_, d| now < d.expires);
        }
        let code = self
            .oauth
            .device_code(&self.config.client_id)
            .await
            .map_err(LoginError::from_oauth)?;
        let handle = new_token();
        let now = self.now();
        let mut state = self.state();
        // The oldest waiting one makes way.
        while state.devices.len() >= MAX_DEVICES {
            let oldest = state
                .devices
                .iter()
                .min_by_key(|(_, d)| d.expires)
                .map(|(k, _)| k.clone());
            state.devices.retain(|k, _| Some(k) != oldest.as_ref());
        }
        state.devices.insert(
            hashed(&handle),
            Device {
                id: new_token(),
                code: code.device_code,
                user_code: code.user_code.clone(),
                peer: peer.clone(),
                started: now,
                interval: code.interval,
                next_poll: now + code.interval,
                expires: now + code.expires_in,
                user: None,
                approved: None,
            },
        );
        Ok(DeviceStart {
            handle,
            user_code: code.user_code,
            verification_uri: code.verification_uri,
            expires_in: code.expires_in.as_secs(),
            interval: code.interval.as_secs(),
        })
    }

    /// Poll a companion's device flow by its handle: not yet, or, once its
    /// person entered the code and is allowed, and they approved it on the
    /// hub's page, a companion credential bound to their user ID. GitHub is
    /// asked at most once per interval, and not again once it answered.
    pub async fn device_poll(&self, peer: &str, handle: &str) -> Result<DevicePoll, LoginError> {
        let key = hashed(handle);
        let now = self.now();
        let (code, interval) = {
            let mut state = self.state();
            if !state.devices.contains_key(&key) {
                // Guessing handles counts as attempts.
                state.device.attempt(now, peer)?;
                return Err(LoginError::expired());
            }
            let device = state
                .devices
                .get_mut(&key)
                .expect("invariant: checked above");
            if now >= device.expires {
                state.devices.remove(&key);
                return Err(LoginError::expired());
            }
            if device.user.is_some() {
                drop(state);
                return self.finish_device(&key);
            }
            if now < device.next_poll {
                return Ok(DevicePoll::pending(device.interval));
            }
            device.next_poll = now + device.interval;
            (device.code.clone(), device.interval)
        };
        let polled = self.oauth.poll_device(&self.config.client_id, &code).await;
        let token = match polled {
            Err(OAuthError::Refused(Refused::Pending)) => return Ok(DevicePoll::pending(interval)),
            Err(OAuthError::Refused(Refused::SlowDown(seconds))) => {
                let slower = seconds.map_or_else(
                    || oauth::poll_interval((interval + SLOW_DOWN).as_secs()),
                    oauth::poll_interval,
                );
                if let Some(device) = self.state().devices.get_mut(&key) {
                    device.interval = slower;
                    device.next_poll = now + slower;
                }
                return Ok(DevicePoll::pending(slower));
            }
            // A network failure can pass; the companion polls again.
            Err(OAuthError::Failed(e)) if e.kind == ErrorKind::Transient => {
                return Err(LoginError::from_oauth(OAuthError::Failed(e)))
            }
            Err(e) => {
                self.state().devices.remove(&key);
                return Err(LoginError::from_oauth(e));
            }
            Ok(token) => token,
        };
        let user = match self.identify(&token.access).await {
            Ok(user) => user,
            Err(e) => {
                self.state().devices.remove(&key);
                return Err(e);
            }
        };
        // The device flow's user token isn't kept: it only said who it is.
        match self.state().devices.get_mut(&key) {
            Some(device) => device.user = Some(user),
            // Denied on the page, or swept, meanwhile.
            None => return Err(LoginError::expired()),
        }
        self.finish_device(&key)
    }

    /// The device flow `key` names, whose person GitHub named: a companion
    /// credential once they approved it on the hub's page, used up then,
    /// or not yet.
    fn finish_device(&self, key: &str) -> Result<DevicePoll, LoginError> {
        let mut state = self.state();
        let device = state.devices.get(key).ok_or_else(LoginError::expired)?;
        let user = device
            .user
            .clone()
            .expect("invariant: GitHub named the person first");
        let Some(approver) = device.approved.clone() else {
            return Ok(DevicePoll::Pending {
                interval: device.interval.as_secs(),
                approve: true,
            });
        };
        state.devices.remove(key);
        if approver.id != user.id {
            return Err(LoginError::new(
                StatusCode::FORBIDDEN,
                "This sign-in was approved by another GitHub account than the one that entered its code; start again",
            ));
        }
        let credential = self.issue_companion(&mut state, &user)?;
        Ok(DevicePoll::Done {
            token: credential,
            login: user.login,
        })
    }

    /// The companions' sign-ins waiting on the hub that `viewer`'s GitHub
    /// account entered the code of, newest first, for the hub's page to show
    /// them, and how many wait for anyone to enter theirs. Nobody sees
    /// another account's sign-ins, or a code before it was entered.
    pub fn device_requests(&self, viewer: &Identity) -> DeviceRequests {
        let all = self.all_device_requests();
        let waiting = all.iter().filter(|(user, _)| user.is_none()).count();
        let requests = all
            .into_iter()
            .filter(|(user, _)| *user == Some(viewer.id))
            .map(|(_, request)| request)
            .collect();
        DeviceRequests { requests, waiting }
    }

    /// Every waiting sign-in, newest first, with the user ID that entered
    /// its code, if one did.
    fn all_device_requests(&self) -> Vec<(Option<u64>, DeviceRequest)> {
        let now = self.now();
        let mut state = self.state();
        state.devices.retain(|_, d| now < d.expires);
        let mut requests: Vec<(Option<u64>, DeviceRequest)> = state
            .devices
            .values()
            .map(|d| {
                (
                    d.user.as_ref().map(|u| u.id),
                    DeviceRequest {
                        id: d.id.clone(),
                        user_code: d.user_code.clone(),
                        started: Timestamp::of(d.started),
                        expires: Timestamp::of(d.expires),
                        peer: d.peer.address.clone(),
                        tailnet_user: d.peer.tailnet_user.clone(),
                        github_login: d.user.as_ref().map(|u| u.login.clone()),
                        approved: d.approved.is_some(),
                    },
                )
            })
            .collect();
        requests.sort_by_key(|(_, r)| std::cmp::Reverse(r.started));
        requests
    }

    /// The waiting device flow the page names `id`, if it is live.
    fn device_by_id<'a>(
        &self,
        state: &'a mut State,
        id: &str,
    ) -> Result<(String, &'a mut Device), LoginError> {
        let now = self.now();
        state.devices.retain(|_, d| now < d.expires);
        state
            .devices
            .iter_mut()
            .find(|(_, d)| matches_token(&d.id, id))
            .map(|(key, d)| (key.clone(), d))
            .ok_or_else(|| {
                LoginError::new(
                    StatusCode::NOT_FOUND,
                    "That sign-in is no longer waiting: it finished, expired or was denied",
                )
            })
    }

    /// Approve a companion's sign-in, as `approver`, signed in on the hub's
    /// page: only once its code was entered on GitHub, so the page can show
    /// as whom, and only by that account. Only once: approving it again is
    /// refused.
    pub fn approve_device(&self, approver: &Identity, id: &str) -> Result<(), LoginError> {
        let mut state = self.state();
        let (_, device) = self.device_by_id(&mut state, id)?;
        let Some(user) = &device.user else {
            return Err(not_entered());
        };
        if user.id != approver.id {
            return Err(another_account());
        }
        if device.approved.is_some() {
            return Err(LoginError::new(
                StatusCode::CONFLICT,
                "That sign-in was already approved",
            ));
        }
        device.approved = Some(approver.clone());
        Ok(())
    }

    /// Deny a companion's sign-in, as `denier`, signed in on the hub's
    /// page: it ends, and its companion is told it expired. Only the
    /// account that entered its code may, and only once it has, so one
    /// allowed user can't end another's sign-in; one nobody entered the
    /// code of expires on its own.
    pub fn deny_device(&self, denier: &Identity, id: &str) -> Result<(), LoginError> {
        let mut state = self.state();
        let (key, device) = self.device_by_id(&mut state, id)?;
        let Some(user) = &device.user else {
            return Err(not_entered());
        };
        if user.id != denier.id {
            return Err(another_account());
        }
        state.devices.remove(&key);
        Ok(())
    }

    fn save_companions(&self, companions: &[Companion]) -> Result<()> {
        let text = serde_json::to_string(companions)?;
        replace_secret(&self.root.join(COMPANIONS_FILE), &text)
    }

    /// A new companion credential for `user`. Each user keeps at most
    /// [`MAX_COMPANIONS`], the oldest dropped first.
    fn issue_companion(&self, state: &mut State, user: &Identity) -> Result<String, LoginError> {
        let token = new_token();
        let mut companions = state.companions.clone();
        while companions.iter().filter(|c| c.user == user.id).count() >= MAX_COMPANIONS {
            let oldest = companions
                .iter()
                .enumerate()
                .filter(|(_, c)| c.user == user.id)
                .min_by_key(|(_, c)| c.created)
                .map(|(n, _)| n)
                .expect("invariant: the user has some");
            companions.remove(oldest);
        }
        companions.push(Companion {
            user: user.id,
            login: user.login.clone(),
            token_hash: hashed(&token),
            created: Timestamp::of(self.now()),
        });
        self.save_companions(&companions)
            .map_err(|_| LoginError::internal("keep the companion credential"))?;
        state.companions = companions;
        Ok(token)
    }

    /// Whether `token` is a companion credential of a user still allowed.
    pub fn companion(&self, token: &str) -> bool {
        let presented = hashed(token);
        self.state().companions.iter().any(|c| {
            matches_token(&c.token_hash, &presented) && self.config.users.contains(&c.user)
        })
    }

    /// Revoke every companion credential the device flow gave.
    pub fn revoke_companions(&self) -> Result<()> {
        let mut state = self.state();
        forget_companions(&self.root)?;
        state.companions.clear();
        Ok(())
    }
}

/// The refusal of an approval or denial of a sign-in nobody has entered
/// the code of.
fn not_entered() -> LoginError {
    LoginError::new(
        StatusCode::CONFLICT,
        "Nobody has entered that sign-in's code on GitHub yet; only the account that does can approve or deny it",
    )
}

/// The refusal of an approval or denial by another account than the one
/// that entered the code.
fn another_account() -> LoginError {
    LoginError::new(
        StatusCode::FORBIDDEN,
        "Another GitHub account entered that sign-in's code; only it can approve or deny it",
    )
}

/// Revoke every companion credential a hub's device flow gave, from its
/// data directory at `root`: with its GitHub App configured or not.
pub fn forget_companions(root: &Path) -> Result<()> {
    db::remove_if_there(&root.join(COMPANIONS_FILE))
}

/// What GitHub sends a browser back with.
#[derive(Debug, Default, Deserialize)]
pub struct Callback {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::{
        clock::ManualClock,
        oauth::tests::{fake_github, token_answer, Shared, CLIENT_ID, CODE, SECRET, USER_CODE},
    };
    use serde_json::json;

    pub const TAILNET: &str = "hub.example.ts.net";
    /// The tailnet address sign-ins come from.
    pub const PEER: &str = "100.64.0.1";
    /// The allowed user, and someone else.
    pub const ALLOWED: u64 = 1001;
    pub const OTHER: u64 = 2002;

    pub fn config() -> AppConfig {
        config_allowing(&[ALLOWED])
    }

    fn config_allowing(users: &[u64]) -> AppConfig {
        AppConfig::new(
            CLIENT_ID,
            &format!("https://{TAILNET}{CALLBACK_PATH}"),
            users,
            &[TAILNET.into()],
        )
        .unwrap()
    }

    /// A hub's sign-ins against a fake GitHub, on a clock the test moves,
    /// and that GitHub, which knows the allowed user's token and another's.
    pub async fn logins(root: &Path) -> (Arc<Logins>, Shared, Arc<ManualClock>) {
        logins_allowing(root, &[ALLOWED]).await
    }

    /// [`logins`], allowing `users`.
    async fn logins_allowing(
        root: &Path,
        users: &[u64],
    ) -> (Arc<Logins>, Shared, Arc<ManualClock>) {
        let (site, fake) = fake_github().await;
        {
            let mut fake = fake.lock().unwrap();
            fake.users
                .insert("ghu_allowed".into(), (ALLOWED, "octocat".into()));
            fake.users
                .insert("ghu_other".into(), (OTHER, "someone".into()));
        }
        let clock = Arc::new(ManualClock::at(Timestamp::from_unix(1_791_763_200)));
        let secret = Credential::new(SECRET).unwrap();
        let logins =
            Logins::new(config_allowing(users), secret, site, clock.clone(), root).unwrap();
        (Arc::new(logins), fake, clock)
    }

    /// Every waiting sign-in, whoever's.
    fn all(logins: &Logins) -> Vec<DeviceRequest> {
        logins
            .all_device_requests()
            .into_iter()
            .map(|(_, r)| r)
            .collect()
    }

    /// The allowed user, as their session on the hub's page says.
    pub fn allowed() -> Identity {
        Identity {
            id: ALLOWED,
            login: "allowed-user".into(),
        }
    }

    /// Approve the waiting device flow that shows `user_code`, as the
    /// allowed user, on the hub's page.
    pub fn approve(logins: &Logins, user_code: &str) {
        let request = logins
            .device_requests(&allowed())
            .requests
            .into_iter()
            .find(|r| r.user_code == user_code)
            .expect("the sign-in waits");
        logins.approve_device(&allowed(), &request.id).unwrap();
    }

    /// Sign in through the fake GitHub as the owner of `token`: the
    /// session's cookie value, or why not.
    pub async fn sign_in(
        logins: &Logins,
        fake: &Shared,
        token: &str,
    ) -> Result<(String, Identity), LoginError> {
        let (binding, url) = logins.start(PEER)?;
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        {
            let mut fake = fake.lock().unwrap();
            fake.challenge = Some(query["code_challenge"].clone());
            fake.token = token.into();
        }
        let callback = Callback {
            code: Some(CODE.into()),
            state: Some(query["state"].clone()),
            error: None,
        };
        logins.finish(PEER, Some(&binding), &callback).await
    }

    #[test]
    fn app_settings_are_checked() {
        let hosts = [TAILNET.to_string(), "hub.example.ts.net:8443".to_string()];
        let callback = format!("https://{TAILNET}{CALLBACK_PATH}");
        // (client ID, callback, users, valid)
        let cases: &[(&str, &str, &[u64], bool)] = &[
            (CLIENT_ID, &callback, &[ALLOWED], true),
            (
                CLIENT_ID,
                "https://HUB.example.ts.net/auth/github/callback",
                &[ALLOWED],
                true,
            ),
            (
                CLIENT_ID,
                "https://hub.example.ts.net:8443/auth/github/callback",
                &[ALLOWED],
                true,
            ),
            ("", &callback, &[ALLOWED], false),
            ("Iv23 x", &callback, &[ALLOWED], false),
            (CLIENT_ID, &callback, &[], false),
            (CLIENT_ID, &callback, &[0], false),
            (
                CLIENT_ID,
                "http://hub.example.ts.net/auth/github/callback",
                &[ALLOWED],
                false,
            ),
            (
                CLIENT_ID,
                "https://other.example.ts.net/auth/github/callback",
                &[ALLOWED],
                false,
            ),
            (
                CLIENT_ID,
                "https://hub.example.ts.net/elsewhere",
                &[ALLOWED],
                false,
            ),
            (
                CLIENT_ID,
                "https://hub.example.ts.net/auth/github/callback?x=1",
                &[ALLOWED],
                false,
            ),
            (
                CLIENT_ID,
                "https://me:x@hub.example.ts.net/auth/github/callback",
                &[ALLOWED],
                false,
            ),
            (
                CLIENT_ID,
                "https://hub.example.ts.net:9/auth/github/callback",
                &[ALLOWED],
                false,
            ),
        ];
        for (client_id, callback, users, valid) in cases {
            let got = AppConfig::new(client_id, callback, users, &hosts);
            assert_eq!(
                got.is_ok(),
                *valid,
                "{client_id} {callback} {users:?}: {got:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_client_secret_is_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        assert!(read_secret(root.path()).is_err());
        save_secret(root.path(), " test-only-secret\n").unwrap();
        save_secret(root.path(), "test-only-secret").unwrap();
        assert_eq!(
            read_secret(root.path()).unwrap().expose(),
            "test-only-secret"
        );
        let path = root.path().join(SECRET_FILE);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_secret(root.path())
            .unwrap_err()
            .to_string()
            .contains("chmod 600"));
        assert!(save_secret(root.path(), "  ").is_err());
    }

    #[tokio::test]
    async fn the_allowed_user_signs_in_by_numeric_id() {
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, _) = logins(root.path()).await;
        let (session, user) = sign_in(&logins, &fake, "ghu_allowed").await.unwrap();
        assert_eq!(
            user,
            Identity {
                id: ALLOWED,
                login: "octocat".into()
            }
        );
        assert_eq!(logins.session(&session, true), Some(user));
        // The token is kept in a private file, and the session isn't it.
        let kept = root.path().join(USERS_DIR).join(format!("{ALLOWED}.json"));
        let text = std::fs::read_to_string(&kept).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            (v["access_token"].as_str(), v["refresh_token"].as_str()),
            (Some("ghu_allowed"), Some("ghr_test-only-refresh"))
        );
        assert!(v["expires_at"].is_string());
        assert!(!session.contains("ghu_"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // A renamed login is the same user; another user under the old
        // login is not.
        fake.lock()
            .unwrap()
            .users
            .insert("ghu_renamed".into(), (ALLOWED, "renamed".into()));
        let (_, renamed) = sign_in(&logins, &fake, "ghu_renamed").await.unwrap();
        assert_eq!(
            renamed,
            Identity {
                id: ALLOWED,
                login: "renamed".into()
            }
        );
        fake.lock()
            .unwrap()
            .users
            .insert("ghu_squatter".into(), (OTHER, "octocat".into()));
        for token in ["ghu_squatter", "ghu_other"] {
            let refused = sign_in(&logins, &fake, token).await.unwrap_err();
            assert_eq!(refused, LoginError::not_allowed(), "{token}");
            assert!(!root
                .path()
                .join(USERS_DIR)
                .join(format!("{OTHER}.json"))
                .exists());
        }
        // A token GitHub doesn't know never makes a session.
        let unknown = sign_in(&logins, &fake, "ghu_unknown").await.unwrap_err();
        assert_eq!(unknown.status, StatusCode::BAD_GATEWAY, "{unknown:?}");
    }

    #[tokio::test]
    async fn sign_ins_are_bound_to_their_browser_and_state_and_used_once() {
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, clock) = logins(root.path()).await;
        let (binding, url) = logins.start(PEER).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/login/oauth/authorize");
        assert_eq!(query["client_id"], CLIENT_ID);
        assert_eq!(
            query["redirect_uri"],
            format!("https://{TAILNET}{CALLBACK_PATH}")
        );
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["code_challenge"].len(), 43);
        assert!(query["state"].len() >= 32 && query["state"] != binding);
        fake.lock().unwrap().challenge = Some(query["code_challenge"].clone());
        fake.lock().unwrap().token = "ghu_allowed".into();
        let back = |state: &str| Callback {
            code: Some(CODE.into()),
            state: Some(state.into()),
            error: None,
        };
        let mismatch = logins.finish(PEER, Some(&binding), &back("forged")).await;
        assert_eq!(mismatch.unwrap_err(), LoginError::mismatch());
        // Used up by the refusal: the right state doesn't work after it.
        let replay = logins
            .finish(PEER, Some(&binding), &back(&query["state"]))
            .await;
        assert_eq!(replay.unwrap_err(), LoginError::expired());
        // Another browser, or none, can't finish this browser's sign-in.
        let (binding, url) = logins.start(PEER).unwrap();
        let state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .to_string();
        for cookie in [None, Some("another-browser")] {
            let got = logins.finish(PEER, cookie, &back(&state)).await;
            assert_eq!(got.unwrap_err(), LoginError::expired());
        }
        // Declined on GitHub.
        let declined = Callback {
            error: Some("access_denied".into()),
            ..back(&state)
        };
        assert_eq!(
            logins
                .finish(PEER, Some(&binding), &declined)
                .await
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
        // Too late.
        let (binding, url) = logins.start(PEER).unwrap();
        let state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .to_string();
        clock.advance(LOGIN_TTL);
        let late = logins.finish(PEER, Some(&binding), &back(&state)).await;
        assert_eq!(late.unwrap_err(), LoginError::expired());
        // No session came of any of it, and GitHub was never asked.
        assert!(logins.state().sessions.is_empty());
        assert!(fake.lock().unwrap().seen.is_empty());
    }

    #[tokio::test]
    async fn sessions_expire_unused_and_after_a_week_and_end_at_logout() {
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, clock) = logins(root.path()).await;
        let (session, _) = sign_in(&logins, &fake, "ghu_allowed").await.unwrap();
        // Each use renews it.
        for _ in 0..3 {
            clock.advance(SESSION_IDLE - Duration::from_secs(1));
            assert!(logins.session(&session, true).is_some());
        }
        clock.advance(SESSION_IDLE);
        assert_eq!(logins.session(&session, true), None, "idle too long");
        // Its token goes with its user's last session.
        let kept = root.path().join(USERS_DIR).join(format!("{ALLOWED}.json"));
        assert!(!kept.exists());
        // However much it is used, a week is the most.
        let (session, _) = sign_in(&logins, &fake, "ghu_allowed").await.unwrap();
        let hour = Duration::from_secs(3600);
        let mut used = Duration::ZERO;
        while used + hour < SESSION_MAX {
            clock.advance(hour);
            used += hour;
            assert!(logins.session(&session, true).is_some());
        }
        clock.advance(hour);
        assert_eq!(logins.session(&session, true), None, "past its lifetime");
        // Logging out ends it, and forgets the token once no session is left.
        let (first, _) = sign_in(&logins, &fake, "ghu_allowed").await.unwrap();
        clock.advance(ATTEMPT_WINDOW);
        let (second, _) = sign_in(&logins, &fake, "ghu_allowed").await.unwrap();
        logins.logout(&first).unwrap();
        assert_eq!(logins.session(&first, true), None);
        assert!(logins.session(&second, true).is_some() && kept.exists());
        logins.logout(&second).unwrap();
        assert!(!kept.exists());
        // A restart signs everyone out, and forgets the tokens.
        sign_in(&logins, &fake, "ghu_allowed").await.unwrap();
        assert!(kept.exists());
        let _restarted = self::logins(root.path()).await;
        assert!(!kept.exists());
        logins.logout("never-a-session").unwrap();
        assert_eq!(logins.session("never-a-session", true), None);
    }

    #[tokio::test]
    async fn sign_in_attempts_are_rate_limited_by_peer_and_in_all() {
        let root = tempfile::tempdir().unwrap();
        let (logins, _, clock) = logins(root.path()).await;
        for _ in 0..PEER_ATTEMPTS {
            logins.start(PEER).unwrap();
        }
        let limited = logins.start(PEER).unwrap_err();
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.retry_after.is_some_and(|d| d <= ATTEMPT_WINDOW));
        // Callbacks with nothing to finish count too.
        let callback = logins.finish(PEER, Some("x"), &Callback::default()).await;
        assert_eq!(callback.unwrap_err().status, StatusCode::TOO_MANY_REQUESTS);
        // Another peer isn't held up by this one, until all of them
        // together reach the backstop.
        logins.start("100.64.0.2").unwrap();
        for n in 0..ALL_ATTEMPTS - PEER_ATTEMPTS - 1 {
            logins.start(&format!("100.64.1.{n}")).unwrap();
        }
        let backstop = logins.start("100.64.0.3").unwrap_err();
        assert_eq!(backstop.status, StatusCode::TOO_MANY_REQUESTS);
        // Companions have limits of their own, which guessed handles use.
        for _ in 0..PEER_ATTEMPTS {
            assert_eq!(
                logins.device_poll(PEER, "guessed").await.unwrap_err(),
                LoginError::expired()
            );
        }
        let guessed = logins.device_poll(PEER, "guessed").await;
        assert_eq!(guessed.unwrap_err().status, StatusCode::TOO_MANY_REQUESTS);
        assert!(logins.device_start(PEER).await.is_err());
        logins.device_start("100.64.0.2").await.unwrap();
        clock.advance(ATTEMPT_WINDOW);
        logins.start(PEER).unwrap();
        logins.device_start(PEER).await.unwrap();
    }

    #[tokio::test]
    async fn the_pages_own_refreshes_dont_keep_a_session_and_sweeps_forget_its_token() {
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, clock) = logins(root.path()).await;
        let (session, _) = sign_in(&logins, &fake, "ghu_allowed").await.unwrap();
        let kept = root.path().join(USERS_DIR).join(format!("{ALLOWED}.json"));
        clock.advance(SESSION_IDLE - Duration::from_secs(1));
        assert!(
            logins.session(&session, false).is_some(),
            "a refresh still reads"
        );
        clock.advance(Duration::from_secs(1));
        assert!(kept.exists());
        // No request needed: the sweep ends it and forgets the token.
        logins.sweep();
        assert!(!kept.exists());
        assert_eq!(logins.session(&session, true), None);
        // A sign-in left unfinished is swept too.
        logins.start(PEER).unwrap();
        clock.advance(LOGIN_TTL);
        logins.sweep();
        assert!(logins.state().pending.is_empty());
    }

    #[test]
    fn the_peer_is_the_fronts_last_forwarded_address() {
        let headers = |values: &[&str]| {
            let mut h = HeaderMap::new();
            for v in values {
                h.append("x-forwarded-for", v.parse().unwrap());
            }
            h
        };
        // (X-Forwarded-For headers, the peer)
        for (values, expected) in [
            // Through a front that names no address, never this machine.
            (&[][..], UNNAMED),
            (&["100.64.0.7"][..], "100.64.0.7"),
            // A peer's own copy can't pass for another peer: the front's
            // entry comes last.
            (&["100.64.0.1, 100.64.0.7"][..], "100.64.0.7"),
            (&["100.64.0.1", "100.64.0.7"][..], "100.64.0.7"),
            (&["100.64.0.7, "][..], "100.64.0.7"),
        ] {
            assert_eq!(peer(&headers(values), true).address, expected, "{values:?}");
            // Straight to the loopback port, it says nothing.
            assert_eq!(peer(&headers(values), false), Peer::from(DIRECT));
        }
        let long = peer(&headers(&[&"9".repeat(500)]), true);
        assert_eq!(long.address.len(), MAX_PEER);
        // The tailnet user the front names, its login first.
        let mut named = headers(&["100.64.0.7"]);
        named.insert("tailscale-user-name", "Some One".parse().unwrap());
        assert_eq!(peer(&named, true).tailnet_user.as_deref(), Some("Some One"));
        named.insert(
            "tailscale-user-login",
            "someone@example.com".parse().unwrap(),
        );
        assert_eq!(
            peer(&named, true).tailnet_user.as_deref(),
            Some("someone@example.com")
        );
        assert_eq!(peer(&named, false).tailnet_user, None);
    }

    #[tokio::test]
    async fn the_device_flow_gives_an_allowed_user_a_companion_credential() {
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, clock) = logins(root.path()).await;
        let start = logins.device_start(PEER).await.unwrap();
        assert_eq!((start.user_code.as_str(), start.interval), (USER_CODE, 5));
        fake.lock().unwrap().polls = [
            json!({"error": "authorization_pending"}),
            json!({"error": "slow_down", "interval": 10}),
            token_answer("ghu_allowed"),
        ]
        .into();
        let pending = |seconds| DevicePoll::pending(Duration::from_secs(seconds));
        // Polled too soon: GitHub isn't asked.
        let early = logins.device_poll(PEER, &start.handle).await.unwrap();
        assert_eq!(early, pending(5));
        let polls = |fake: &Shared| {
            fake.lock()
                .unwrap()
                .seen
                .iter()
                .filter(|s| s.1 == "/login/oauth/access_token")
                .count()
        };
        assert_eq!(polls(&fake), 0);
        clock.advance(Duration::from_secs(5));
        // Polls at once: only one asks GitHub.
        let (one, two) = tokio::join!(
            logins.device_poll(PEER, &start.handle),
            logins.device_poll(PEER, &start.handle)
        );
        assert_eq!((one.unwrap(), two.unwrap()), (pending(5), pending(5)));
        assert_eq!(polls(&fake), 1);
        clock.advance(Duration::from_secs(5));
        assert_eq!(
            logins.device_poll(PEER, &start.handle).await.unwrap(),
            pending(10)
        );
        clock.advance(Duration::from_secs(5));
        assert_eq!(
            logins.device_poll(PEER, &start.handle).await.unwrap(),
            pending(10)
        );
        assert_eq!(polls(&fake), 2, "slowed down");
        clock.advance(Duration::from_secs(5));
        // GitHub has the code; the hub's page has yet to approve it.
        let waiting = DevicePoll::Pending {
            interval: 10,
            approve: true,
        };
        assert_eq!(
            logins.device_poll(PEER, &start.handle).await.unwrap(),
            waiting
        );
        approve(&logins, &start.user_code);
        let DevicePoll::Done { token, login } =
            logins.device_poll(PEER, &start.handle).await.unwrap()
        else {
            panic!("not done")
        };
        assert_eq!(polls(&fake), 3, "GitHub is asked until it answers");
        assert_eq!(login, fake.lock().unwrap().users["ghu_allowed"].1);
        assert!(logins.companion(&token));
        assert!(!logins.companion("anything-else"));
        // Kept by its hash only, and the handle is used up.
        let kept = std::fs::read_to_string(root.path().join(COMPANIONS_FILE)).unwrap();
        assert!(!kept.contains(&token) && kept.contains(&hashed(&token)));
        assert_eq!(
            logins.device_poll(PEER, &start.handle).await.unwrap_err(),
            LoginError::expired()
        );
        // Across a restart, until revoked.
        let (again, _, _) = self::logins(root.path()).await;
        assert!(again.companion(&token));
        logins.revoke_companions().unwrap();
        assert!(!logins.companion(&token));
        assert!(!root.path().join(COMPANIONS_FILE).exists());
    }

    #[tokio::test]
    async fn a_user_taken_off_the_allowlist_loses_their_companion_credentials_for_good() {
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, clock) = logins(root.path()).await;
        let start = logins.device_start(PEER).await.unwrap();
        fake.lock().unwrap().polls = [token_answer("ghu_allowed")].into();
        clock.advance(Duration::from_secs(5));
        logins.device_poll(PEER, &start.handle).await.unwrap();
        approve(&logins, &start.user_code);
        let DevicePoll::Done { token, .. } = logins.device_poll(PEER, &start.handle).await.unwrap()
        else {
            panic!("not done")
        };
        let config = AppConfig {
            users: [OTHER].into(),
            ..config()
        };
        let secret = Credential::new(SECRET).unwrap();
        let without =
            Logins::new(config, secret, Site::github(), clock.clone(), root.path()).unwrap();
        assert!(!without.companion(&token));
        // Allowed again, the old credential stays gone.
        let (back, _, _) = self::logins(root.path()).await;
        assert!(!back.companion(&token));
        let kept = std::fs::read_to_string(root.path().join(COMPANIONS_FILE)).unwrap();
        assert_eq!(kept, "[]");
    }

    #[tokio::test]
    async fn device_flows_that_are_refused_give_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, clock) = logins(root.path()).await;
        // (GitHub's answer, the status the companion gets)
        for (answer, status) in [
            (json!({"error": "access_denied"}), StatusCode::FORBIDDEN),
            (json!({"error": "expired_token"}), StatusCode::BAD_REQUEST),
            (token_answer("ghu_other"), StatusCode::FORBIDDEN),
        ] {
            clock.advance(ATTEMPT_WINDOW);
            let start = logins.device_start(PEER).await.unwrap();
            fake.lock().unwrap().polls = [answer.clone()].into();
            clock.advance(Duration::from_secs(5));
            let refused = logins.device_poll(PEER, &start.handle).await.unwrap_err();
            assert_eq!(refused.status, status, "{answer}");
            assert_eq!(
                logins.device_poll(PEER, &start.handle).await.unwrap_err(),
                LoginError::expired()
            );
        }
        // A device code that runs out.
        let start = logins.device_start(PEER).await.unwrap();
        clock.advance(Duration::from_secs(start.expires_in));
        assert_eq!(
            logins.device_poll(PEER, &start.handle).await.unwrap_err(),
            LoginError::expired()
        );
        assert!(logins.state().companions.is_empty());
        assert!(!root.path().join(COMPANIONS_FILE).exists());
    }

    #[tokio::test]
    async fn a_companion_sign_in_waits_for_a_browser_to_approve_it_once() {
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, clock) = logins_allowing(root.path(), &[ALLOWED, OTHER]).await;
        let other = Identity {
            id: OTHER,
            login: "other-user".into(),
        };
        let started = Timestamp::of(clock.now());
        let start = logins.device_start(PEER).await.unwrap();
        // The page shows the code, when it started and where from, before
        // anyone entered the code.
        let [request] = &all(&logins)[..] else {
            panic!("one waits")
        };
        let expires = started.after(Duration::from_secs(start.expires_in));
        assert_eq!(
            request,
            &DeviceRequest {
                id: request.id.clone(),
                user_code: start.user_code.clone(),
                started,
                expires,
                peer: PEER.into(),
                tailnet_user: None,
                github_login: None,
                approved: false,
            }
        );
        assert_ne!(request.id, start.handle, "the page never sees the handle");
        let id = request.id.clone();
        // Nobody sees it yet, only that one waits, and nobody may deny it.
        let empty = DeviceRequests {
            requests: vec![],
            waiting: 1,
        };
        assert_eq!(logins.device_requests(&allowed()), empty);
        assert_eq!(
            logins.deny_device(&allowed(), &id).unwrap_err().status,
            StatusCode::CONFLICT
        );
        fake.lock().unwrap().polls = [token_answer("ghu_allowed")].into();
        clock.advance(Duration::from_secs(5));
        let waiting = DevicePoll::Pending {
            interval: 5,
            approve: true,
        };
        assert_eq!(
            logins.device_poll(PEER, &start.handle).await.unwrap(),
            waiting
        );
        // GitHub isn't asked again while it waits.
        clock.advance(Duration::from_secs(5));
        assert_eq!(
            logins.device_poll(PEER, &start.handle).await.unwrap(),
            waiting
        );
        let asked = |fake: &Shared| {
            fake.lock()
                .unwrap()
                .seen
                .iter()
                .filter(|s| s.1 == "/login/oauth/access_token")
                .count()
        };
        assert_eq!(asked(&fake), 1);
        // Now its account sees it, and no other.
        let mine = logins.device_requests(&allowed());
        assert_eq!((mine.requests.len(), mine.waiting), (1, 0));
        assert!(logins.device_requests(&other).requests.is_empty());
        let entered = all(&logins)[0].github_login.clone();
        assert_eq!(
            entered,
            Some(fake.lock().unwrap().users["ghu_allowed"].1.clone())
        );
        // Only the account that entered the code approves it, and once.
        let refused = logins.approve_device(&other, &id).unwrap_err();
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(
            logins
                .approve_device(&allowed(), "forged")
                .unwrap_err()
                .status,
            StatusCode::NOT_FOUND
        );
        logins.approve_device(&allowed(), &id).unwrap();
        assert!(all(&logins)[0].approved);
        assert_eq!(
            logins.approve_device(&allowed(), &id).unwrap_err().status,
            StatusCode::CONFLICT
        );
        let DevicePoll::Done { token, .. } = logins.device_poll(PEER, &start.handle).await.unwrap()
        else {
            panic!("not done")
        };
        assert!(logins.companion(&token));
        // Used up: nothing waits, and the approval can't be used again.
        assert!(all(&logins).is_empty());
        assert_eq!(
            logins.approve_device(&allowed(), &id).unwrap_err().status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            logins.device_poll(PEER, &start.handle).await.unwrap_err(),
            LoginError::expired()
        );

        // Not before its code is entered on GitHub, so the page can show
        // whose it is; and denied, the companion is told it expired, and
        // GitHub isn't asked again.
        let start = logins.device_start(PEER).await.unwrap();
        let id = all(&logins)[0].id.clone();
        assert_eq!(
            logins.approve_device(&allowed(), &id).unwrap_err().status,
            StatusCode::CONFLICT
        );
        fake.lock().unwrap().polls = [token_answer("ghu_allowed")].into();
        clock.advance(Duration::from_secs(5));
        logins.device_poll(PEER, &start.handle).await.unwrap();
        // Only by the account that entered the code.
        assert_eq!(
            logins.deny_device(&other, &id).unwrap_err().status,
            StatusCode::FORBIDDEN
        );
        logins.deny_device(&allowed(), &id).unwrap();
        assert_eq!(
            logins.deny_device(&allowed(), &id).unwrap_err().status,
            StatusCode::NOT_FOUND
        );
        clock.advance(Duration::from_secs(5));
        let before = asked(&fake);
        assert_eq!(
            logins.device_poll(PEER, &start.handle).await.unwrap_err(),
            LoginError::expired()
        );
        assert_eq!(asked(&fake), before);

        // Expired: no longer shown, and can't be approved.
        let start = logins.device_start(PEER).await.unwrap();
        let id = all(&logins)[0].id.clone();
        clock.advance(Duration::from_secs(start.expires_in));
        assert!(all(&logins).is_empty());
        assert_eq!(
            logins.approve_device(&allowed(), &id).unwrap_err().status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(logins.state().companions.len(), 1);
    }
}
