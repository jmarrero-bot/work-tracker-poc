use crate::{
    bot,
    clock::SystemClock,
    connector::{self, Target},
    connectors,
    db::{self, Db, Mode},
    hub::{self, DevicePoll},
    login::{self, AppConfig, Callback, LoginError, Logins},
    model::*,
    oauth::Site,
    schedule::{self, Builtin, Policy, Scheduler, Trigger},
    zone,
};
use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, OriginalUri, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::{Duration, SystemTime},
};
use uuid::Uuid;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct Assets;
#[derive(Clone)]
struct App {
    db: Arc<Mutex<Db>>,
    access: Arc<Access>,
    scheduler: Arc<Scheduler>,
    mode: Mode,
    /// On a companion, its sign-in to a hub while it waits for GitHub.
    hub_login: Arc<Mutex<Option<HubLogin>>>,
}
/// A companion's sign-in to a hub, waiting for its person to enter the code
/// on GitHub.
struct HubLogin {
    url: url::Url,
    handle: crate::connector::Credential,
    expires: SystemTime,
    /// Seconds between polls, as the hub last said.
    interval: u64,
}
type ApiResult<T> = std::result::Result<Json<T>, ApiError>;
struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":self.0.to_string()})),
        )
            .into_response()
    }
}
/// Whether two secrets are equal, in time that doesn't depend on where
/// they differ.
pub(crate) fn matches_token(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}
/// The only path a hub's companion token may read.
const EXPORT_PATH: &str = "/api/export/external";
/// The browser session's cookie.
const SESSION_COOKIE: &str = "workboard_session";
/// Where a hub keeps its companion token, in the data directory.
const COMPANION_TOKEN: &str = "companion.token";

/// Who may use the service, from where: the CLI with the application
/// token, a browser signed in with the hub's GitHub App or, without one, a
/// browser paired with the application token, and companions with a
/// credential that reads the export.
struct Access {
    /// `127.0.0.1:PORT`, where the service listens.
    local: String,
    /// Other names a hub is reached by, such as its tailnet name, always
    /// through an HTTPS front such as `tailscale serve`.
    hosts: Vec<String>,
    /// The application credential: full access. Rotated on each start.
    token: String,
    /// A hub's companion token, which reads the external export and
    /// nothing else. Kept across restarts.
    companion: Option<RwLock<String>>,
    /// Whether a browser may pair with the application token: always on a
    /// local service, and on a hub only with `--pairing`, which can't be
    /// combined with a GitHub App.
    pairing: bool,
    /// A hub's GitHub App sign-ins, if it has one. Then the application
    /// token is the CLI's alone, on loopback.
    logins: Option<Arc<Logins>>,
}

/// Why a request was turned away.
#[derive(Debug, PartialEq, Eq)]
enum Denied {
    Host,
    Unauthorized,
    /// The companion token, used for anything but the export.
    Scope,
    CrossOrigin,
}

impl Denied {
    fn response(self) -> Response {
        let (status, message) = match self {
            Self::Host => (StatusCode::FORBIDDEN, "Unexpected host"),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "Sign in first: with GitHub on a hub that has its App, otherwise with the pairing URL `workboard serve` printed",
            ),
            Self::Scope => (
                StatusCode::FORBIDDEN,
                "This token only reads the hub's external export",
            ),
            Self::CrossOrigin => (
                StatusCode::FORBIDDEN,
                "Cross-origin changes are not allowed",
            ),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl Access {
    /// The origin a browser that reached the service as `host` sends: the
    /// service's own on loopback, or HTTPS under one of the hub's names.
    fn origin(&self, host: &str) -> Option<String> {
        if host == self.local {
            return Some(format!("http://{host}"));
        }
        let host = host.to_ascii_lowercase();
        self.hosts
            .contains(&host)
            .then(|| format!("https://{host}"))
    }
    /// Whether a session cookie set for `host` is marked Secure: under one
    /// of the hub's names, which are only reached over HTTPS.
    fn secure(&self, host: &str) -> bool {
        self.origin(host)
            .is_some_and(|origin| origin.starts_with("https://"))
    }
    /// Whether a request may go on.
    fn check(&self, method: &Method, path: &str, headers: &HeaderMap) -> Result<(), Denied> {
        let host = text(headers, header::HOST).ok_or(Denied::Host)?;
        let origin = self.origin(host).ok_or(Denied::Host)?;
        let bearer = text(headers, header::AUTHORIZATION).and_then(|v| v.strip_prefix("Bearer "));
        // With a GitHub App, the application token is the CLI's, on
        // loopback and not through a proxy; without one, also a paired
        // browser's on the hub's names.
        let direct = host == self.local && !PROXY_HEADERS.iter().any(|h| headers.contains_key(*h));
        let app_token =
            |v: &str| matches_token(v, &self.token) && (direct || self.logins.is_none());
        let bearer_ok = bearer.is_some_and(app_token);
        let paired = self.pairing && cookie(headers, SESSION_COOKIE).is_some_and(app_token);
        let signed_in = self.logins.as_ref().is_some_and(|logins| {
            let renew = !headers.contains_key(BACKGROUND_HEADER);
            cookie(headers, login::SESSION_COOKIE)
                .is_some_and(|v| logins.session(v, renew).is_some())
        });
        if !bearer_ok && !paired && !signed_in {
            let companion = bearer.is_some_and(|v| {
                let shared = self.companion.as_ref().is_some_and(|token| {
                    matches_token(v, &token.read().unwrap_or_else(PoisonError::into_inner))
                });
                shared
                    || self
                        .logins
                        .as_ref()
                        .is_some_and(|logins| logins.companion(v))
            });
            return match companion {
                true if method == Method::GET && path == EXPORT_PATH => Ok(()),
                true => Err(Denied::Scope),
                false => Err(Denied::Unauthorized),
            };
        }
        // A browser's change must come from the page the service served
        // under the same name.
        if method != Method::GET
            && !bearer_ok
            && text(headers, header::ORIGIN) != Some(origin.as_str())
        {
            return Err(Denied::CrossOrigin);
        }
        Ok(())
    }
}

/// Headers a proxy in front of the service adds, such as `tailscale serve`.
/// A client can send any Host header through one, so the loopback name
/// alone doesn't show a request came from this machine.
const PROXY_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "tailscale-user-login",
];
/// The header the page sends with its own refreshes, which don't count as
/// a use of a session.
const BACKGROUND_HEADER: &str = "x-workboard-background";
/// A header's value, if it is text.
fn text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The value of the cookie `name`, from any of the request's cookie
/// headers.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|s| {
            s.trim()
                .strip_prefix(name)
                .and_then(|s| s.strip_prefix('='))
        })
}

async fn protect(State(app): State<App>, req: Request, next: Next) -> Response {
    // Routes nested under /api see their path without it.
    let path = req
        .extensions()
        .get::<OriginalUri>()
        .map_or_else(|| req.uri().path().to_string(), |u| u.path().to_string());
    match app.access.check(req.method(), &path, req.headers()) {
        Ok(()) => next.run(req).await,
        Err(denied) => denied.response(),
    }
}
async fn session(State(app): State<App>, headers: HeaderMap) -> Response {
    if !app.access.pairing {
        let message = match app.access.logins {
            Some(_) => "Browser pairing is off on this hub, which has its GitHub App: sign in with GitHub",
            None => "Browser pairing is off on this hub: set up its GitHub App (docs/github-app.md), or start it with --pairing",
        };
        return (StatusCode::FORBIDDEN, Json(json!({ "error": message }))).into_response();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let secure = if app.access.secure(host) {
        "; Secure"
    } else {
        ""
    };
    let mut res = Json(json!({"ok":true})).into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{SESSION_COOKIE}={}; Path=/api; HttpOnly; SameSite=Strict{secure}",
            app.access.token
        ))
        .unwrap(),
    );
    res
}
/// A hub's companion token, which a companion uses to read the external
/// export.
async fn companion_token(State(app): State<App>) -> ApiResult<Value> {
    let token = app.access.companion.as_ref().ok_or_else(only_hubs)?;
    let token = token.read().unwrap_or_else(PoisonError::into_inner).clone();
    Ok(Json(json!({ "token": token })))
}
/// Replace a hub's companion token: the old one stops working at once, and
/// so does every companion credential its GitHub App's sign-ins gave.
async fn rotate_companion_token(State(app): State<App>) -> ApiResult<Value> {
    let current = app.access.companion.as_ref().ok_or_else(only_hubs)?;
    let token = new_token();
    let root = app.db.lock().unwrap().root.clone();
    replace_secret(&root.join(COMPANION_TOKEN), &token)?;
    *current.write().unwrap_or_else(PoisonError::into_inner) = token.clone();
    // Also those a hub's sign-ins gave while it had its App.
    match &app.access.logins {
        Some(logins) => logins.revoke_companions()?,
        None => login::forget_companions(&root)?,
    }
    Ok(Json(json!({ "token": token })))
}
fn only_hubs() -> anyhow::Error {
    anyhow::anyhow!("Only a hub has a companion token")
}
/// A new random credential.
pub(crate) fn new_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
/// Write `value` to the private file at `path`, replacing it all at once.
pub(crate) fn replace_secret(path: &std::path::Path, value: &str) -> Result<()> {
    let staged = path.with_extension("new");
    db::remove_if_there(&staged)?;
    db::write_secret(&staged, value)?;
    std::fs::rename(&staged, path)?;
    Ok(())
}
/// Where in `$XDG_RUNTIME_DIR` the service also puts its token.
const RUNTIME_DIR: &str = "workboard";

/// The file in [`RUNTIME_DIR`] with the token of the service on `port`.
/// One service listens on a port, so its token is the one there.
fn runtime_token_name(port: u16) -> String {
    format!("service-{port}.token")
}

/// Also put the token in the session's runtime directory, private to the
/// user and gone at logout, so a sandboxed client such as the GNOME app's
/// Flatpak can read it without access to the data directory, which holds
/// the database and the credentials. Without a runtime directory, or if
/// writing fails, clients read the data directory's copy.
fn publish_token(port: u16, token: &str) {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) else {
        return;
    };
    let dir = std::path::PathBuf::from(runtime).join(RUNTIME_DIR);
    let path = dir.join(runtime_token_name(port));
    let written = db::private_dir(&dir).and_then(|()| replace_secret(&path, token));
    if let Err(e) = written {
        // An earlier start's copy would be taken for this one's.
        let _ = db::remove_if_there(&path);
        eprintln!(
            "Cannot put the service token in {}: {e:#}; clients read it from the data directory",
            dir.display()
        );
    }
}
/// A hub's companion token, made on its first start and kept.
fn companion_token_of(root: &std::path::Path) -> Result<String> {
    let path = root.join(COMPANION_TOKEN);
    match std::fs::read_to_string(&path) {
        Ok(token) if !token.trim().is_empty() => Ok(token.trim().to_string()),
        Ok(_) => bail!("{} is empty; remove it to make a new one", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let token = new_token();
            db::write_secret(&path, &token)?;
            Ok(token)
        }
        Err(e) => Err(e).with_context(|| format!("Cannot read {}", path.display())),
    }
}
/// Whether `host` can name a hub: a DNS name, with a port if it isn't
/// HTTPS's.
pub fn valid_host(host: &str) -> bool {
    let (name, port) = host.split_once(':').unwrap_or((host, "443"));
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        && port.parse::<u16>().is_ok_and(|p| p != 0)
}
/// Everything stored, with how fresh it is, and what watching found.
fn fresh_snapshot(app: &App) -> Result<(Snapshot, Vec<WatchList>)> {
    let (mut snapshot, mut watched) = {
        let db = app.db.lock().unwrap();
        (db.snapshot()?, db.watched()?)
    };
    app.scheduler.annotate_snapshot(&mut snapshot, &mut watched);
    Ok((snapshot, watched))
}
/// What the page and the CLI show: everything stored, and what kind of
/// service this is.
#[derive(Serialize)]
struct Board {
    #[serde(flatten)]
    snapshot: Snapshot,
    mode: Mode,
    /// The hub this companion pulls from, if one is set.
    hub: Option<hub::Status>,
}
/// What the page and the CLI show: everything stored here, and on a
/// companion the hub's work too, read-only, as of the last pull. Only for
/// showing: the export is built from this machine's own work alone.
fn board(app: &App) -> Result<(Snapshot, Vec<WatchList>, Option<hub::Status>)> {
    let (mut snapshot, watched) = fresh_snapshot(app)?;
    let hub = app.scheduler.hub_view()?.map(|(status, export)| {
        if let Some(export) = export {
            hub::merge(&mut snapshot, &export, &status);
        }
        status
    });
    Ok((snapshot, watched, hub))
}
async fn snapshot(State(app): State<App>) -> ApiResult<Board> {
    let (snapshot, _, hub) = board(&app)?;
    Ok(Json(Board {
        snapshot,
        mode: app.mode,
        hub,
    }))
}
/// The hub this companion pulls from, if one is set, and how its pulls go.
async fn get_hub(State(app): State<App>) -> ApiResult<Option<hub::Status>> {
    Ok(Json(app.scheduler.hub_status()?))
}
#[derive(Deserialize)]
struct HubInput {
    url: String,
    token: String,
}
/// Pull from a hub from now on, once a first pull shows its address and
/// token work. Nothing is ever sent to it but that read.
async fn set_hub(State(app): State<App>, Json(input): Json<HubInput>) -> ApiResult<hub::Status> {
    if app.mode == Mode::Hub {
        bail_api("This is a hub; only a companion pulls from one")?
    }
    Ok(Json(app.scheduler.set_hub(&input.url, &input.token).await?))
}
/// Stop pulling from the hub, and forget its token and its work.
async fn unset_hub(State(app): State<App>) -> ApiResult<Value> {
    app.scheduler.unset_hub().await?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct HubLoginInput {
    url: String,
}
/// Start signing this companion in to a hub with GitHub's device flow: the
/// code for its person to enter on GitHub, and where. The hub runs the flow
/// with GitHub; nothing is sent to it but the request.
async fn hub_login(State(app): State<App>, Json(input): Json<HubLoginInput>) -> ApiResult<Value> {
    if app.mode == Mode::Hub {
        bail_api("This is a hub; only a companion signs in to one")?
    }
    let url = hub::hub_url(&input.url)?;
    let start = hub::HubClient::new()?.start_login(&url).await?;
    let handle = crate::connector::Credential::new(&start.handle)?;
    *app.hub_login.lock().unwrap() = Some(HubLogin {
        url,
        handle,
        expires: SystemTime::now() + Duration::from_secs(start.expires_in),
        interval: start.interval,
    });
    Ok(Json(json!({
        "user_code": start.user_code,
        "verification_uri": start.verification_uri,
        "expires_in": start.expires_in,
        "interval": start.interval,
    })))
}
/// Ask the hub whether this companion's sign-in is done. Once it is, pull
/// from the hub with the credential it gave, as `hub set` does.
async fn hub_login_poll(State(app): State<App>) -> ApiResult<Value> {
    let waiting = app
        .hub_login
        .lock()
        .unwrap()
        .as_ref()
        .map(|l| (l.url.clone(), l.handle.clone(), l.expires, l.interval));
    let Some((url, handle, expires, interval)) = waiting else {
        return Err(anyhow::anyhow!(
            "No sign-in to a hub is waiting; run `workboard hub login URL`"
        )
        .into());
    };
    let forget = || app.hub_login.lock().unwrap().take();
    if SystemTime::now() >= expires {
        forget();
        bail_api(
            "The sign-in expired before the code was entered; run `workboard hub login URL` again",
        )?
    }
    let polled = hub::HubClient::new()?.poll_login(&url, &handle).await;
    match polled {
        Ok(DevicePoll::Pending { interval }) => {
            if let Some(waiting) = app.hub_login.lock().unwrap().as_mut() {
                waiting.interval = interval;
            }
            Ok(Json(json!({ "status": "pending", "interval": interval })))
        }
        Ok(DevicePoll::Done { token, login }) => {
            forget();
            let status = app.scheduler.set_hub(url.as_str(), &token).await.map_err(|e| {
                anyhow::anyhow!("Signed in to the hub as {login}, but the first pull failed: {e}. Run `workboard hub login URL` again")
            })?;
            Ok(Json(
                json!({ "status": "done", "login": login, "hub": status }),
            ))
        }
        // A failure that may pass leaves the sign-in waiting.
        Err(e) if e.kind == crate::connector::ErrorKind::Transient => Ok(Json(
            json!({ "status": "pending", "interval": interval, "error": e.message }),
        )),
        Err(e) => {
            forget();
            Err(e.into())
        }
    }
}
/// What waits on each connection's account, and apart from it what watching
/// other users found. A local view: internal work shows here, and never
/// leaves through it.
async fn mine(State(app): State<App>) -> ApiResult<crate::mine::Mine> {
    let (snapshot, watched, _) = board(&app)?;
    Ok(Json(crate::mine::mine(&snapshot, &watched)))
}
/// Each bot connection: where its board is, the agent runs of its
/// heartbeat, and how many of its items there are. This machine's own
/// connections only: a companion has no settings of the hub's.
async fn bot_view(State(app): State<App>) -> ApiResult<Vec<bot::BotView>> {
    let (snapshot, _) = fresh_snapshot(&app)?;
    let mut agents = app.db.lock().unwrap().agents()?;
    app.scheduler
        .annotate_agents(&snapshot.connections, &mut agents);
    Ok(Json(bot::views(&snapshot, &agents)))
}
/// What may leave this machine: the external zone of this machine's own
/// work only, never the work a companion pulled from a hub. A hub serves
/// it to its companions.
async fn export_external(State(app): State<App>) -> ApiResult<zone::ExternalSnapshot> {
    Ok(Json(zone::external_only(&fresh_snapshot(&app)?.0)))
}
/// Fail if `id` names the hub's work, which is read-only on its companion.
fn local(id: &str) -> Result<()> {
    if hub::is_from_hub(id) {
        bail!("That comes from the hub and is read-only here; change it on the hub")
    }
    Ok(())
}
async fn connect(State(app): State<App>, Json(input): Json<ConnectInput>) -> ApiResult<Connection> {
    if input.label.trim().is_empty() || input.label.len() > 100 {
        bail_api("Enter a connection name of 1–100 characters")?
    }
    // Before the credential goes anywhere.
    if app.mode == Mode::Hub && input.zone != Zone::External {
        bail_api("This is a hub, which connects external sources only: add --external (or turn on External). Connect internal sources on the companion")?
    }
    let base = connectors::base_url(&input.base_url)?;
    let bot = bot_config(&input)?;
    let watch_user = input.watch_user.as_deref().map(str::trim);
    if let Some(login) = watch_user {
        check_watch_user(&input.provider, login)?;
    }
    let sync_interval = input.sync_interval.map(Duration::from_secs);
    if let Some(interval) = sync_interval {
        schedule::check_interval(interval)?;
    }
    let source = app.scheduler.connector(&input.provider, bot.as_ref())?;
    let target = Target::new(&base, &input.token)?;
    let account = connector::verify(source.as_ref(), &target).await?;
    let (id, name) = (account.id, account.name);
    if watch_user.is_some_and(|login| login.eq_ignore_ascii_case(&name)) {
        bail_api(OWN_ACCOUNT)?
    }
    let db = app.db.lock().unwrap();
    if db
        .connections()?
        .iter()
        .any(|c| c.provider == input.provider && c.base_url == base && c.account_id == id)
    {
        bail_api("This account is already connected to that instance")?
    }
    let c = Connection {
        id: Uuid::new_v4().to_string(),
        provider: input.provider,
        label: input.label.trim().into(),
        base_url: base,
        account_id: id,
        account_name: name,
        zone: input.zone,
        watch_user: watch_user.map(str::to_string),
        sync_interval: input.sync_interval,
        bot,
        sync: SyncState::default(),
        schedule: Schedule::default(),
        from_hub: false,
    };
    let path = db.root.join("secrets").join(&c.id);
    db::write_secret(&path, target.credential.expose())?;
    if let Err(e) = db.save_connection(&c) {
        let _ = std::fs::remove_file(path);
        return Err(e.into());
    }
    drop(db);
    // Its first sync is due now.
    app.scheduler.wake();
    let mut c = c;
    app.scheduler.annotate(std::slice::from_mut(&mut c));
    Ok(Json(c))
}
/// A new connection's bot settings, checked: the bot's are given in full,
/// and nothing else takes any. The bot's work is public on GitHub, so it is
/// connected as external.
fn bot_config(input: &ConnectInput) -> Result<Option<BotConfig>> {
    let config = match (&input.bot, input.provider == "bot") {
        (Some(bot), true) => Some(bot.clone().config()?),
        (None, true) => {
            bail!("Give the bot's board, tracker, heartbeat issue, login and forge organization")
        }
        (Some(_), false) => {
            bail!("Only a bot connection takes the bot's board, tracker and heartbeat")
        }
        (None, false) => None,
    };
    if config.is_some() && input.zone != Zone::External {
        bail!("The bot's work is public on GitHub: connect it as external (--external, or turn on External)")
    }
    Ok(config)
}
fn bail_api(message: &str) -> Result<()> {
    bail!("{message}")
}
const OWN_ACCOUNT: &str =
    "That is this connection's own account, whose work is already yours; watch another user";
/// Whether a connection of `provider` may watch `login`.
fn check_watch_user(provider: &str, login: &str) -> Result<()> {
    if provider != "github" {
        bail!("Only a GitHub connection can watch another user")
    }
    if !valid_github_login(login) {
        bail!("Enter a GitHub login: letters, digits and hyphens")
    }
    Ok(())
}
/// A field that may be left out, apart from one set to null: `None` when
/// absent, `Some(None)` when null.
fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
/// What changes about a connection: only the fields given.
#[derive(Deserialize)]
struct ConnectionPatch {
    /// Who to watch, or with null nobody.
    #[serde(default, deserialize_with = "present")]
    watch_user: Option<Option<String>>,
    /// How often it syncs, in seconds, or with null as often as the
    /// service's default.
    #[serde(default, deserialize_with = "present")]
    sync_interval: Option<Option<u64>>,
}
/// Watch another user's activity visible to a GitHub connection's token, or
/// stop, and change how often a connection syncs. What watching finds shows
/// as theirs, never as the connection's account's.
async fn patch_connection(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(input): Json<ConnectionPatch>,
) -> ApiResult<Connection> {
    local(&id)?;
    // Every field is checked before any is written.
    let c = app.db.lock().unwrap().connection(&id)?;
    let interval = input.sync_interval.map(|i| i.map(Duration::from_secs));
    if let Some(Some(interval)) = interval {
        schedule::check_interval(interval)?;
    }
    let watch_user = input
        .watch_user
        .map(|login| login.map(|login| login.trim().to_string()));
    if let Some(Some(login)) = &watch_user {
        check_watch_user(&c.provider, login)?;
        if login.eq_ignore_ascii_case(&c.account_name) {
            bail_api(OWN_ACCOUNT)?
        }
    }
    if let Some(interval) = interval {
        app.scheduler.set_interval(&id, interval).await?;
    }
    if let Some(login) = watch_user {
        // Not while a sync is saving what it found for the old setting.
        let _held = app.scheduler.hold(&id).await;
        app.db
            .lock()
            .unwrap()
            .set_watch_user(&id, login.as_deref())?;
    }
    let mut c = app.db.lock().unwrap().connection(&id)?;
    app.scheduler.annotate(std::slice::from_mut(&mut c));
    Ok(Json(c))
}
#[derive(Deserialize)]
struct VerifyInput {
    /// A new credential for the same account, to replace the stored one.
    #[serde(default)]
    token: Option<String>,
}
/// Check a connection's credential again, or replace it with a new one for
/// the same account, and sync it soon. A connection whose credential was
/// rejected gets its scheduled syncs back this way.
async fn verify(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(input): Json<VerifyInput>,
) -> ApiResult<Connection> {
    local(&id)?;
    Ok(Json(
        app.scheduler.verify(&id, input.token.as_deref()).await?,
    ))
}
async fn disconnect(State(app): State<App>, Path(id): Path<String>) -> ApiResult<Value> {
    local(&id)?;
    // Not while it syncs, which would save what it found afterwards.
    let _held = app.scheduler.hold(&id).await;
    app.db.lock().unwrap().delete_connection(&id)?;
    app.scheduler.forget(&id);
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize, Default)]
struct SyncInput {
    /// Only this connection; all of them when left out.
    #[serde(default)]
    connection: Option<String>,
}
/// Sync connected sources now: one, or all of them side by side. A sync
/// already running, as on schedule, is joined, not started again.
async fn sync(State(app): State<App>, Json(input): Json<SyncInput>) -> ApiResult<Value> {
    let results = match input.connection {
        Some(id) => {
            app.db.lock().unwrap().connection(&id)?;
            vec![app.scheduler.sync(&id, Trigger::Explicit).await]
        }
        None => app.scheduler.sync_all(Trigger::Explicit).await?,
    };
    Ok(Json(json!({ "results": results })))
}
async fn effort(State(app): State<App>, Json(input): Json<EffortInput>) -> ApiResult<Effort> {
    for id in &input.item_ids {
        local(id)?;
    }
    Ok(Json(app.db.lock().unwrap().create_effort(input)?))
}
#[derive(Deserialize)]
struct ShareInput {
    share_title: bool,
}
/// Let an effort's title leave this machine, or stop it.
async fn share_effort(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(input): Json<ShareInput>,
) -> ApiResult<Effort> {
    local(&id)?;
    Ok(Json(
        app.db.lock().unwrap().share_title(&id, input.share_title)?,
    ))
}
#[derive(Deserialize)]
struct LinkInput {
    item: String,
    effort: Option<String>,
}
async fn link(State(app): State<App>, Json(input): Json<LinkInput>) -> ApiResult<Value> {
    local(&input.item)?;
    if let Some(effort) = &input.effort {
        local(effort)?;
    }
    app.db
        .lock()
        .unwrap()
        .link(&input.item, input.effort.as_deref())?;
    Ok(Json(json!({"ok":true})))
}
async fn manual(State(app): State<App>, Json(input): Json<ManualInput>) -> ApiResult<Item> {
    if !valid_provider(&input.provider) {
        bail_api("Choose a supported source")?
    }
    if input.title.trim().is_empty()
        || input.title.len() > 500
        || input.key.len() > 250
        || input.status.len() > 100
    {
        bail_api("Enter a title and keep source fields within their limits")?
    }
    if !matches!(
        input.action.as_str(),
        "next" | "review" | "waiting" | "follow-up" | ""
    ) {
        bail_api("Invalid attention category")?
    }
    let url = connectors::source_url(&input.url)?;
    let updated = app.scheduler.now().to_string();
    let db = app.db.lock().unwrap();
    let mut item = Item {
        id: Uuid::new_v4().to_string(),
        connection_id: None,
        provider: input.provider,
        remote_id: input.key.clone(),
        key: input.key,
        title: input.title.trim().into(),
        url,
        status: input.status,
        outcome: Outcome::Open,
        outcome_reason: String::new(),
        action: input.action,
        updated_at: updated,
        effort_id: None,
        abandoned_at: None,
        demo: false,
        zone: Zone::Internal,
        reasons: vec![],
        synced_at: None,
        stale: false,
        from_hub: false,
        bot: None,
    };
    // Manual outcomes are explicit; the status text never implies one.
    item.set_outcome((input.outcome, input.outcome.manual_reason().into()));
    db.upsert_item(&item)?;
    Ok(Json(item))
}
#[derive(Deserialize)]
struct StatusInput {
    status: String,
    action: String,
    /// Unchanged when omitted.
    outcome: Option<Outcome>,
}
async fn update_manual(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(input): Json<StatusInput>,
) -> ApiResult<Item> {
    local(&id)?;
    if input.status.trim().is_empty()
        || input.status.len() > 100
        || !matches!(
            input.action.as_str(),
            "next" | "review" | "waiting" | "follow-up" | ""
        )
    {
        bail_api("Enter a valid status and attention category")?
    }
    let db = app.db.lock().unwrap();
    let mut item = db
        .items()?
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| anyhow::anyhow!("Item not found"))?;
    if item.connection_id.is_some() {
        bail_api("Synced statuses can only be updated in the source system")?
    }
    item.status = input.status.trim().into();
    item.action = input.action;
    let derived = match input.outcome {
        Some(outcome) if outcome != item.outcome => (outcome, outcome.manual_reason().into()),
        _ => (item.outcome, std::mem::take(&mut item.outcome_reason)),
    };
    item.set_outcome(derived);
    item.updated_at = app.scheduler.now().to_string();
    db.upsert_item(&item)?;
    Ok(Json(item))
}
#[derive(Deserialize)]
struct AbandonInput {
    abandoned: bool,
    /// Abandon an item that is open or delivered.
    #[serde(default)]
    force: bool,
}
async fn abandon(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(input): Json<AbandonInput>,
) -> ApiResult<Item> {
    local(&id)?;
    Ok(Json(app.db.lock().unwrap().set_abandoned(
        &id,
        input.abandoned,
        input.force,
    )?))
}
/// A sign-in step's refusal, as JSON, saying when to try again after a
/// rate limit.
fn refusal(error: LoginError) -> Response {
    let mut response = (error.status, Json(json!({ "error": error.message }))).into_response();
    if let Some(after) = error.retry_after {
        response.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from(after.as_secs().max(1)),
        );
    }
    response
}
fn refused(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
/// The answer of a service without a GitHub App to a sign-in step.
fn no_app() -> Response {
    refused(
        StatusCode::NOT_FOUND,
        "This service has no GitHub App; see docs/github-app.md",
    )
}
/// The refusal of a browser's sign-in step that didn't come by the name
/// people sign in on, or, for a change, from that name's page.
fn off_sign_in_name(logins: &Logins, headers: &HeaderMap, change: bool) -> Option<Response> {
    let name = logins.config().host();
    let came = text(headers, header::HOST).map(str::to_ascii_lowercase);
    if came.as_deref() != Some(name.as_str()) {
        return Some(refused(
            StatusCode::FORBIDDEN,
            &format!("Sign in on https://{name}/"),
        ));
    }
    if change && text(headers, header::ORIGIN) != Some(format!("https://{name}").as_str()) {
        return Some(Denied::CrossOrigin.response());
    }
    None
}
/// The refusal of a companion's sign-in step that didn't come by one of the
/// service's names, or came from a browser, which always says its origin
/// when it posts.
fn not_from_companion(app: &App, headers: &HeaderMap) -> Option<Response> {
    if text(headers, header::HOST)
        .and_then(|h| app.access.origin(h))
        .is_none()
    {
        return Some(Denied::Host.response());
    }
    if headers.contains_key(header::ORIGIN) {
        return Some(refused(
            StatusCode::FORBIDDEN,
            "A companion signs in with `workboard hub login`, never from a browser",
        ));
    }
    None
}
/// A `Set-Cookie` value for a cookie of the hub's sign-in: kept to the
/// hub's exact name over HTTPS, and out of the page's scripts.
fn sign_in_cookie(name: &str, value: &str, same_site: &str, max_age: Duration) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; HttpOnly; Secure; SameSite={same_site}; Max-Age={}",
        max_age.as_secs()
    ))
    .expect("invariant: cookie names and random values are header text")
}
/// How this service signs browsers in, and who this one is, if signed in.
async fn auth_status(State(app): State<App>, headers: HeaderMap) -> Response {
    if text(&headers, header::HOST)
        .and_then(|h| app.access.origin(h))
        .is_none()
    {
        return Denied::Host.response();
    }
    let logins = app.access.logins.as_ref();
    let user = logins.and_then(|logins| {
        cookie(&headers, login::SESSION_COOKIE).and_then(|v| logins.session(v, false))
    });
    Json(json!({
        "github_app": logins.is_some(),
        "pairing": app.access.pairing,
        "sign_in": logins.map(|l| format!("https://{}/", l.config().host())),
        "user": user,
    }))
    .into_response()
}
/// Start a browser's sign-in with GitHub: where to send it, and a cookie
/// that binds the sign-in to it. Only from the page under the name people
/// sign in on.
async fn login_start(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some(logins) = app.access.logins.as_ref() else {
        return no_app();
    };
    if let Some(response) = off_sign_in_name(logins, &headers, true) {
        return response;
    }
    match logins.start(&login::peer(&headers)) {
        Ok((binding, url)) => {
            let mut response = Json(json!({ "url": url.as_str() })).into_response();
            response.headers_mut().insert(
                header::SET_COOKIE,
                // Lax, so it comes back with GitHub's redirect.
                sign_in_cookie(login::LOGIN_COOKIE, &binding, "Lax", login::LOGIN_TTL),
            );
            response
        }
        Err(e) => refusal(e),
    }
}
/// Where GitHub sends the browser back: finish its sign-in, give it a
/// session, and send it to the board.
async fn login_callback(
    State(app): State<App>,
    headers: HeaderMap,
    Query(callback): Query<Callback>,
) -> Response {
    let Some(logins) = app.access.logins.as_ref() else {
        return no_app();
    };
    if let Some(response) = off_sign_in_name(logins, &headers, false) {
        return response;
    }
    let finished = logins
        .finish(
            &login::peer(&headers),
            cookie(&headers, login::LOGIN_COOKIE),
            &callback,
        )
        .await;
    let mut response = match finished {
        Ok((session, _)) => {
            let mut response = (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
            response.headers_mut().append(
                header::SET_COOKIE,
                sign_in_cookie(
                    login::SESSION_COOKIE,
                    &session,
                    "Strict",
                    login::SESSION_MAX,
                ),
            );
            response
        }
        Err(e) => {
            let body = format!(
                "{}\n\nOpen https://{}/ to try again.\n",
                e.message,
                logins.config().host()
            );
            (
                e.status,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                body,
            )
                .into_response()
        }
    };
    // The sign-in is used up either way.
    response.headers_mut().append(
        header::SET_COOKIE,
        sign_in_cookie(login::LOGIN_COOKIE, "", "Lax", Duration::ZERO),
    );
    response
}
/// End this browser's session, from its own page.
async fn logout(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some(logins) = app.access.logins.as_ref() else {
        return no_app();
    };
    if let Some(response) = off_sign_in_name(logins, &headers, true) {
        return response;
    }
    if let Some(session) = cookie(&headers, login::SESSION_COOKIE) {
        if logins.logout(session).is_err() {
            return refused(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Cannot forget the GitHub token",
            );
        }
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        sign_in_cookie(login::SESSION_COOKIE, "", "Strict", Duration::ZERO),
    );
    response
}
/// Start a companion's sign-in with GitHub's device flow.
async fn device_start(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some(logins) = app.access.logins.as_ref() else {
        return no_app();
    };
    if let Some(response) = not_from_companion(&app, &headers) {
        return response;
    }
    match logins.device_start(&login::peer(&headers)).await {
        Ok(start) => Json(start).into_response(),
        Err(e) => refusal(e),
    }
}
/// Poll a companion's sign-in, by the handle its start gave.
async fn device_poll(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some(logins) = app.access.logins.as_ref() else {
        return no_app();
    };
    if let Some(response) = not_from_companion(&app, &headers) {
        return response;
    }
    let Some(handle) =
        text(&headers, header::AUTHORIZATION).and_then(|v| v.strip_prefix("Bearer "))
    else {
        return Denied::Unauthorized.response();
    };
    match logins.device_poll(&login::peer(&headers), handle).await {
        Ok(poll) => Json(poll).into_response(),
        Err(e) => refusal(e),
    }
}
async fn assets(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let Some(asset) = Assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    };
    let mut response = Response::new(Body::from(asset.data.into_owned()));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    response
}
/// The header Tailscale Funnel adds to every request it brings from the
/// internet. `tailscale serve` and Funnel strip inbound copies, so a request
/// that has it came through Funnel.
const FUNNEL_HEADER: &str = "tailscale-funnel-request";
/// Refuse anything that came through Tailscale Funnel, which would put the
/// service on the internet: a hub is for the tailnet only, and a local
/// service for its own machine.
async fn refuse_funnel(req: Request, next: Next) -> Response {
    if req.headers().contains_key(FUNNEL_HEADER) {
        let message = "Workboard is never served through Tailscale Funnel; use `tailscale serve`";
        return (StatusCode::FORBIDDEN, Json(json!({ "error": message }))).into_response();
    }
    next.run(req).await
}
async fn headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response.headers_mut().insert("content-security-policy",HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    response
}
/// What the service says when it starts: how browsers get in. The pairing
/// URL carries the application token, so it is printed only while pairing
/// is on.
fn welcome(access: &Access, mode: Mode) -> String {
    let running = match mode {
        Mode::Local => "Workboard is running locally",
        Mode::Hub => "Workboard is running as a hub, for external work only",
    };
    let cli = "CLI commands use the same data directory.";
    if let Some(logins) = &access.logins {
        let name = logins.config().host();
        return format!("{running}. Sign in with GitHub at https://{name}/. {cli}");
    }
    if !access.pairing {
        return format!("{running}. No browser can sign in: set up its GitHub App (docs/github-app.md), or start it with --pairing. {cli}");
    }
    let token = &access.token;
    let local = &access.local;
    let mut urls = vec![format!("http://{local}/#token={token}")];
    urls.extend(
        access
            .hosts
            .iter()
            .map(|host| format!("https://{host}/#token={token}")),
    );
    format!(
        "{running}. Open this private pairing URL:\n{}\nKeep this URL private. {cli}",
        urls.join("\n")
    )
}
/// How to serve.
pub struct Options {
    pub port: u16,
    /// Seed illustrative work into an empty local data directory.
    pub demo: bool,
    /// How often a connection syncs unless it says otherwise.
    pub interval: Duration,
    /// Serve a hub: external work only.
    pub hub: bool,
    /// Other names a hub is reached by, through an HTTPS front.
    pub hosts: Vec<String>,
    /// Let a hub's browsers pair with the application token, without a
    /// GitHub App.
    pub pairing: bool,
    /// The GitHub App a hub signs people in with.
    pub github_app: Option<GitHubApp>,
}
/// A hub's GitHub App, as given on the command line. Its client secret is
/// in the data directory.
pub struct GitHubApp {
    pub client_id: String,
    pub callback: String,
    /// The numeric GitHub user IDs allowed in.
    pub users: Vec<u64>,
}
/// The service's routes: the API, which [`Access`] guards, the sign-in
/// steps, which guard themselves, and the page.
fn router(app: App) -> Router {
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/mine", get(mine))
        .route("/bot", get(bot_view))
        .route("/export/external", get(export_external))
        .route("/session", post(session))
        .route("/hub", get(get_hub).put(set_hub).delete(unset_hub))
        .route("/hub/login", post(hub_login))
        .route("/hub/login/poll", post(hub_login_poll))
        .route(
            "/companion-token",
            get(companion_token).post(rotate_companion_token),
        )
        .route("/connections", post(connect))
        .route(
            "/connections/{id}",
            axum::routing::delete(disconnect).patch(patch_connection),
        )
        .route("/connections/{id}/verify", post(verify))
        .route("/sync", post(sync))
        .route("/efforts", post(effort))
        .route("/efforts/{id}", axum::routing::patch(share_effort))
        .route("/links", post(link))
        .route("/items", post(manual))
        .route("/items/{id}", axum::routing::patch(update_manual))
        .route("/items/{id}/abandon", post(abandon))
        .layer(middleware::from_fn_with_state(app.clone(), protect));
    let auth = Router::new()
        .route("/status", get(auth_status))
        .route("/github/login", post(login_start))
        .route("/github/callback", get(login_callback))
        .route("/logout", post(logout))
        .route("/device", post(device_start))
        .route("/device/poll", post(device_poll));
    Router::new()
        .nest("/api", api)
        .nest("/auth", auth)
        .fallback(assets)
        .layer(DefaultBodyLimit::max(65536))
        .layer(middleware::from_fn(headers))
        .layer(middleware::from_fn(refuse_funnel))
        .with_state(app)
}
pub async fn serve(root: PathBuf, options: Options) -> Result<()> {
    #[cfg(not(unix))]
    bail!("This initial server requires Unix file permissions; use Linux or macOS");
    let Options {
        port,
        demo,
        interval,
        hub,
        hosts,
        pairing,
        github_app,
    } = options;
    let mode = if hub { Mode::Hub } else { Mode::Local };
    if hub && demo {
        bail!("A hub keeps external work only, and the demo's work is internal; start the demo without --hub")
    }
    // Only a hub is served under another name: a local service may hold
    // internal work, which never leaves its machine.
    if !hub && !hosts.is_empty() {
        bail!("Only a hub is reached by another name; a local service may hold internal work and stays on loopback")
    }
    let hosts: Vec<String> = hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
    if let Some(host) = hosts.iter().find(|h| !valid_host(h)) {
        bail!("Enter a host name such as hub.example.ts.net, with a port only if it isn't 443: {host}")
    }
    if !hub && (pairing || github_app.is_some()) {
        bail!("Only a hub takes --pairing or a GitHub App; a local service pairs its browser on loopback")
    }
    if pairing && github_app.is_some() {
        bail!("Browser pairing is off once the hub has its GitHub App; leave out --pairing")
    }
    let config = github_app
        .map(|app| AppConfig::new(&app.client_id, &app.callback, &app.users, &hosts))
        .transpose()?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let mut db = Db::open_as(&root, mode)?;
    let mode = db.mode();
    // With the App or not: a hub started without it keeps no sessions.
    login::forget_user_tokens(&root)?;
    let logins = match config {
        Some(config) => {
            let secret = login::read_secret(&root)?;
            let logins = Logins::new(config, secret, Site::github(), Arc::new(SystemClock), &root)?;
            let swept = Arc::new(logins);
            let sweeping = swept.clone();
            tokio::spawn(async move {
                let mut every = tokio::time::interval(login::SWEEP);
                loop {
                    every.tick().await;
                    sweeping.sweep();
                }
            });
            Some(swept)
        }
        None => None,
    };
    if demo && db.items()?.is_empty() && db.efforts()?.is_empty() {
        db.seed()?
    }
    let token = new_token();
    let path = root.join("service.token");
    if path.exists() {
        std::fs::remove_file(&path)?
    }
    db::write_secret(&path, &token)?;
    publish_token(port, &token);
    let local = format!("127.0.0.1:{port}");
    let companion = match mode {
        Mode::Hub => Some(RwLock::new(companion_token_of(&root)?)),
        Mode::Local => None,
    };
    let db = Arc::new(Mutex::new(db));
    let sources = Builtin {
        plugins: root.join(connectors::PLUGINS_DIR),
        shared: connectors::Shared::default(),
    };
    let scheduler = Scheduler::new(
        db.clone(),
        Box::new(sources),
        Arc::new(SystemClock),
        Policy { interval },
    );
    tokio::spawn(scheduler.clone().serve());
    let app = App {
        db,
        access: Arc::new(Access {
            local: local.clone(),
            hosts,
            token,
            companion,
            pairing: mode == Mode::Local || pairing,
            logins,
        }),
        scheduler,
        mode,
        hub_login: Arc::default(),
    };
    println!("{}", welcome(&app.access, mode));
    let stop = stop_requested()?;
    axum::serve(listener, router(app))
        .with_graceful_shutdown(stop)
        .await?;
    Ok(())
}
/// Resolves once the service is asked to stop: on Ctrl-C (SIGINT), or on
/// SIGTERM, which container engines and systemd send. Without a handler,
/// SIGTERM would kill the service anywhere but as a container's PID 1,
/// where it would be ignored until the engine resorts to SIGKILL.
#[cfg(unix)]
fn stop_requested() -> Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate =
        signal(SignalKind::terminate()).context("Cannot handle SIGTERM to stop gracefully")?;
    Ok(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    })
}
#[cfg(not(unix))]
fn stop_requested() -> Result<impl std::future::Future<Output = ()>> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compares_entire_token() {
        assert!(matches_token("abc", "abc"));
        assert!(!matches_token("abc", "abd"));
        assert!(!matches_token("ab", "abc"));
    }

    const LOCAL: &str = "127.0.0.1:8421";
    const TAILNET: &str = "hub.example.ts.net";
    const TOKEN: &str = "test-only-token";
    const COMPANION: &str = "test-only-companion";
    /// The origin of the hub's page under its tailnet name.
    const HUB_ORIGIN: &str = "https://hub.example.ts.net";

    /// A local service's or a hub's access, the hub's paired with browsers.
    fn access(hub: bool) -> Access {
        Access {
            local: LOCAL.into(),
            hosts: if hub { vec![TAILNET.into()] } else { vec![] },
            token: TOKEN.into(),
            companion: hub.then(|| RwLock::new(COMPANION.into())),
            pairing: true,
            logins: None,
        }
    }

    /// What a request carries to authenticate.
    #[derive(Clone, Copy)]
    enum Credential {
        Nothing,
        Bearer(&'static str),
        Cookie(&'static str),
        /// A cookie whose name only starts like the session's.
        Lookalike,
    }

    /// The headers of a request that came by `host`, with `credential`
    /// and `origin`.
    fn headers(host: &str, credential: Credential, origin: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        let mut set = |name, value: String| {
            h.insert(name, HeaderValue::from_str(&value).unwrap());
        };
        set(header::HOST, host.into());
        match credential {
            Credential::Nothing => (),
            Credential::Bearer(token) => set(header::AUTHORIZATION, format!("Bearer {token}")),
            Credential::Cookie(token) => {
                set(header::COOKIE, format!("a=b; {SESSION_COOKIE}={token}"))
            }
            Credential::Lookalike => set(header::COOKIE, format!("{SESSION_COOKIE}x={TOKEN}")),
        }
        if let Some(origin) = origin {
            set(header::ORIGIN, origin.into());
        }
        h
    }

    #[test]
    fn requests_need_a_known_host_a_credential_and_a_matching_origin() {
        use Credential::*;
        use Denied::*;
        const SNAPSHOT: &str = "/api/snapshot";
        const EFFORTS: &str = "/api/efforts";
        const HUB_ORIGIN: &str = "https://hub.example.ts.net";
        const LOCAL_ORIGIN: &str = "http://127.0.0.1:8421";
        let (get, post) = (Method::GET, Method::POST);
        // (hub, method, path, host, credential, origin, expected)
        #[rustfmt::skip]
        let cases = [
            (false, &get, SNAPSHOT, LOCAL, Bearer(TOKEN), None, Ok(())),
            (false, &get, SNAPSHOT, LOCAL, Nothing, None, Err(Unauthorized)),
            (false, &get, SNAPSHOT, LOCAL, Bearer("wrong"), None, Err(Unauthorized)),
            (false, &get, SNAPSHOT, LOCAL, Lookalike, None, Err(Unauthorized)),
            (false, &get, SNAPSHOT, "attacker.example", Bearer(TOKEN), None, Err(Host)),
            // A local service is never reached by another name.
            (false, &get, SNAPSHOT, TAILNET, Bearer(TOKEN), None, Err(Host)),
            (true, &get, SNAPSHOT, TAILNET, Bearer(TOKEN), None, Ok(())),
            (true, &get, SNAPSHOT, "HUB.example.ts.net", Cookie(TOKEN), None, Ok(())),
            (true, &get, SNAPSHOT, "other.example.ts.net", Cookie(TOKEN), None, Err(Host)),
            // A browser's change needs the origin of the name it came by.
            (true, &post, EFFORTS, TAILNET, Cookie(TOKEN), Some(HUB_ORIGIN), Ok(())),
            (true, &post, EFFORTS, TAILNET, Cookie(TOKEN), Some(LOCAL_ORIGIN), Err(CrossOrigin)),
            (true, &post, EFFORTS, TAILNET, Cookie(TOKEN), Some("http://hub.example.ts.net"), Err(CrossOrigin)),
            (true, &post, EFFORTS, TAILNET, Cookie(TOKEN), None, Err(CrossOrigin)),
            (true, &post, EFFORTS, LOCAL, Cookie(TOKEN), Some(LOCAL_ORIGIN), Ok(())),
            (true, &post, EFFORTS, LOCAL, Cookie(TOKEN), Some(HUB_ORIGIN), Err(CrossOrigin)),
            (true, &post, EFFORTS, LOCAL, Bearer(TOKEN), None, Ok(())),
            // The companion token reads the export, and nothing else.
            (true, &get, EXPORT_PATH, TAILNET, Bearer(COMPANION), None, Ok(())),
            (true, &get, EXPORT_PATH, LOCAL, Bearer(COMPANION), None, Ok(())),
            (true, &get, SNAPSHOT, TAILNET, Bearer(COMPANION), None, Err(Scope)),
            (true, &get, "/api/companion-token", TAILNET, Bearer(COMPANION), None, Err(Scope)),
            (true, &post, EXPORT_PATH, TAILNET, Bearer(COMPANION), Some(HUB_ORIGIN), Err(Scope)),
            (true, &get, EXPORT_PATH, TAILNET, Cookie(COMPANION), None, Err(Unauthorized)),
            (false, &get, EXPORT_PATH, LOCAL, Bearer(COMPANION), None, Err(Unauthorized)),
        ];
        for (n, (hub, method, path, host, credential, origin, expected)) in
            cases.into_iter().enumerate()
        {
            let got = access(hub).check(method, path, &headers(host, credential, origin));
            assert_eq!(got, expected, "case {n}: {method} {path} via {host}");
        }
    }

    #[tokio::test]
    async fn a_hub_with_its_app_takes_sessions_and_the_cli_on_loopback_only() {
        use Credential::*;
        use Denied::*;
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, _) = login::tests::logins(root.path()).await;
        let (session, _) = login::tests::sign_in(&logins, &fake, "ghu_allowed")
            .await
            .unwrap();
        let hub = Access {
            pairing: false,
            logins: Some(logins),
            ..access(true)
        };
        let unpaired = Access {
            pairing: false,
            ..access(true)
        };
        let signed_in = |origin: Option<&str>| {
            let mut h = headers(TAILNET, Nothing, origin);
            let value = format!("{}={session}", login::SESSION_COOKIE);
            h.insert(header::COOKIE, HeaderValue::from_str(&value).unwrap());
            h
        };
        // The CLI's token with the loopback name but a proxy's header.
        let proxied = |name: &'static str| {
            let mut h = headers(LOCAL, Bearer(TOKEN), None);
            h.insert(name, HeaderValue::from_static("100.64.0.7"));
            h
        };
        let (get, post) = (Method::GET, Method::POST);
        // (access, method, path, headers, expected)
        let cases = [
            (&hub, &get, "/api/snapshot", signed_in(None), Ok(())),
            (
                &hub,
                &post,
                "/api/efforts",
                signed_in(Some(HUB_ORIGIN)),
                Ok(()),
            ),
            (
                &hub,
                &post,
                "/api/efforts",
                signed_in(Some("https://attacker.example")),
                Err(CrossOrigin),
            ),
            (
                &hub,
                &post,
                "/api/efforts",
                signed_in(None),
                Err(CrossOrigin),
            ),
            (
                &hub,
                &get,
                "/api/snapshot",
                headers(TAILNET, Cookie("forged"), None),
                Err(Unauthorized),
            ),
            // The CLI's token works on loopback alone, and pairing not at all.
            (
                &hub,
                &get,
                "/api/snapshot",
                headers(LOCAL, Bearer(TOKEN), None),
                Ok(()),
            ),
            (
                &hub,
                &get,
                "/api/snapshot",
                headers(TAILNET, Bearer(TOKEN), None),
                Err(Unauthorized),
            ),
            (
                &hub,
                &get,
                "/api/snapshot",
                headers(TAILNET, Cookie(TOKEN), None),
                Err(Unauthorized),
            ),
            (
                &hub,
                &get,
                "/api/snapshot",
                headers(LOCAL, Cookie(TOKEN), None),
                Err(Unauthorized),
            ),
            (
                &hub,
                &get,
                EXPORT_PATH,
                headers(TAILNET, Bearer(COMPANION), None),
                Ok(()),
            ),
            // Without pairing and without an App, only the CLI's token.
            (
                &unpaired,
                &get,
                "/api/snapshot",
                headers(TAILNET, Cookie(TOKEN), None),
                Err(Unauthorized),
            ),
            (
                &unpaired,
                &get,
                "/api/snapshot",
                headers(TAILNET, Bearer(TOKEN), None),
                Ok(()),
            ),
        ];
        for (n, (access, method, path, headers, expected)) in cases.into_iter().enumerate() {
            assert_eq!(
                access.check(method, path, &headers),
                expected,
                "case {n}: {method} {path}"
            );
        }
        // The loopback name through a proxy, as a tailnet client can send it.
        for name in PROXY_HEADERS {
            let got = hub.check(&get, "/api/snapshot", &proxied(name));
            assert_eq!(got, Err(Unauthorized), "{name}");
        }
        assert_eq!(PROXY_HEADERS.len(), 4);
    }

    /// An app on a fresh data directory at `root`: a hub reached at `local`
    /// and by its tailnet name, with `logins`, or a local service.
    fn test_app(
        root: &std::path::Path,
        mode: Mode,
        local: &str,
        logins: Option<Arc<Logins>>,
    ) -> App {
        let db = Arc::new(Mutex::new(Db::open_as(root, mode).unwrap()));
        let sources = Builtin {
            plugins: root.join(connectors::PLUGINS_DIR),
            shared: connectors::Shared::default(),
        };
        let policy = Policy {
            interval: Duration::from_secs(300),
        };
        let scheduler =
            Scheduler::new(db.clone(), Box::new(sources), Arc::new(SystemClock), policy);
        let hub = mode == Mode::Hub;
        App {
            db,
            access: Arc::new(Access {
                local: local.into(),
                pairing: !hub,
                logins,
                ..access(hub)
            }),
            scheduler,
            mode,
            hub_login: Arc::default(),
        }
    }

    /// Send `router` a request, and read its answer.
    // The router itself, in process: no reqwest client is involved.
    #[allow(clippy::disallowed_methods)]
    async fn call(
        router: &Router,
        method: Method,
        uri: &str,
        headers: &[(header::HeaderName, &str)],
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, String) {
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            request = request.header(name, *value);
        }
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        let response = router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let (status, headers) = (response.status(), response.headers().clone());
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    /// The `Set-Cookie` values of an answer.
    fn set_cookies(headers: &HeaderMap) -> Vec<String> {
        headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_callback_route_is_the_one_apps_are_given() {
        assert_eq!(login::CALLBACK_PATH, "/auth/github/callback");
    }

    #[tokio::test]
    async fn the_web_sign_in_gives_a_session_cookie_never_the_token() {
        use header::{COOKIE, HOST, ORIGIN};
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, _) = login::tests::logins(root.path()).await;
        let router = router(test_app(root.path(), Mode::Hub, LOCAL, Some(logins)));
        let origin = format!("https://{TAILNET}");
        let (status, headers, body) = call(
            &router,
            Method::POST,
            "/auth/github/login",
            &[(HOST, TAILNET), (ORIGIN, origin.as_str())],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let cookies = set_cookies(&headers);
        let [started] = &cookies[..] else {
            panic!("{headers:?}")
        };
        for flag in [
            "__Host-workboard_login=",
            "Path=/",
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
            "Max-Age=600",
        ] {
            assert!(started.contains(flag), "{started}");
        }
        let binding = started.split(';').next().unwrap().to_string();
        let answer: Value = serde_json::from_str(&body).unwrap();
        let url = url::Url::parse(answer["url"].as_str().unwrap()).unwrap();
        let query: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        {
            let mut fake = fake.lock().unwrap();
            fake.challenge = Some(query["code_challenge"].clone());
            fake.token = "ghu_allowed".into();
        }
        let back = format!(
            "/auth/github/callback?code={}&state={}",
            crate::oauth::tests::CODE,
            query["state"]
        );
        let (status, headers, body) = call(
            &router,
            Method::GET,
            &back,
            &[(HOST, TAILNET), (COOKIE, binding.as_str())],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        assert_eq!(headers[header::LOCATION], "/");
        let cookies = set_cookies(&headers);
        let session = cookies
            .iter()
            .find(|c| c.starts_with(login::SESSION_COOKIE))
            .unwrap();
        let max_age = format!("Max-Age={}", login::SESSION_MAX.as_secs());
        for flag in ["Path=/", "HttpOnly", "Secure", "SameSite=Strict", &max_age] {
            assert!(session.contains(flag), "{session}");
        }
        assert!(
            !session.contains("ghu_"),
            "the cookie never holds the token"
        );
        assert!(cookies
            .iter()
            .any(|c| c.starts_with(login::LOGIN_COOKIE) && c.contains("Max-Age=0")));
        let session = session.split(';').next().unwrap().to_string();
        // The session reads and, from the hub's own page, changes.
        let signed_in = [(HOST, TAILNET), (COOKIE, session.as_str())];
        assert_eq!(
            call(&router, Method::GET, "/api/snapshot", &signed_in, None)
                .await
                .0,
            StatusCode::OK
        );
        let effort = Some(serde_json::json!({ "title": "Signed in" }));
        let from = |origin: &'static str| {
            [
                (HOST, TAILNET),
                (COOKIE, session.as_str()),
                (ORIGIN, origin),
            ]
        };
        let (status, _, body) = call(
            &router,
            Method::POST,
            "/api/efforts",
            &from(HUB_ORIGIN),
            effort.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _, _) = call(
            &router,
            Method::POST,
            "/api/efforts",
            &from("https://attacker.example"),
            effort,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (_, _, body) = call(&router, Method::GET, "/auth/status", &signed_in, None).await;
        let status: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status["user"]["id"], login::tests::ALLOWED);
        assert_eq!(
            (status["github_app"].as_bool(), status["pairing"].as_bool()),
            (Some(true), Some(false))
        );
        // The same callback again finds nothing to finish.
        let replay = call(
            &router,
            Method::GET,
            &back,
            &[(HOST, TAILNET), (COOKIE, binding.as_str())],
            None,
        )
        .await;
        assert_eq!(replay.0, StatusCode::BAD_REQUEST, "{}", replay.2);
        // Nothing a sign-in carries comes back in what the hub answers.
        for secret in [query["state"].as_str(), crate::oauth::tests::CODE, "ghu_"] {
            assert!(
                !replay.2.contains(secret) && !body.contains(secret),
                "{secret}"
            );
        }
        // Signing out takes the page's origin, and ends the session.
        let (status, _, _) = call(&router, Method::POST, "/auth/logout", &signed_in, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, headers, _) = call(
            &router,
            Method::POST,
            "/auth/logout",
            &from(HUB_ORIGIN),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(set_cookies(&headers)[0].contains("Max-Age=0"));
        let (status, _, _) = call(&router, Method::GET, "/api/snapshot", &signed_in, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn sign_in_steps_from_elsewhere_are_refused_before_github_is_asked() {
        use header::{AUTHORIZATION, COOKIE, HOST, ORIGIN};
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, _) = login::tests::logins(root.path()).await;
        let router = router(test_app(root.path(), Mode::Hub, LOCAL, Some(logins)));
        let origin = format!("https://{TAILNET}");
        let callback = "/auth/github/callback?code=x&state=y";
        let bearer = format!("Bearer {TOKEN}");
        let (get, post) = (Method::GET, Method::POST);
        // (method, path, headers, expected)
        type Case<'a> = (
            &'a Method,
            &'a str,
            Vec<(header::HeaderName, &'a str)>,
            StatusCode,
        );
        let cases: Vec<Case> = vec![
            (
                &post,
                "/auth/github/login",
                vec![(HOST, TAILNET)],
                StatusCode::FORBIDDEN,
            ),
            (
                &post,
                "/auth/github/login",
                vec![(HOST, TAILNET), (ORIGIN, "https://attacker.example")],
                StatusCode::FORBIDDEN,
            ),
            (
                &post,
                "/auth/github/login",
                vec![(HOST, TAILNET), (ORIGIN, "http://hub.example.ts.net")],
                StatusCode::FORBIDDEN,
            ),
            (
                &post,
                "/auth/github/login",
                vec![(HOST, LOCAL), (ORIGIN, "http://127.0.0.1:8421")],
                StatusCode::FORBIDDEN,
            ),
            (
                &post,
                "/auth/github/login",
                vec![
                    (HOST, "other.example.ts.net"),
                    (ORIGIN, "https://other.example.ts.net"),
                ],
                StatusCode::FORBIDDEN,
            ),
            (
                &get,
                callback,
                vec![(HOST, TAILNET)],
                StatusCode::BAD_REQUEST,
            ),
            (
                &get,
                callback,
                vec![(HOST, TAILNET), (COOKIE, "__Host-workboard_login=forged")],
                StatusCode::BAD_REQUEST,
            ),
            (&get, callback, vec![(HOST, LOCAL)], StatusCode::FORBIDDEN),
            (
                &post,
                "/auth/logout",
                vec![(HOST, TAILNET)],
                StatusCode::FORBIDDEN,
            ),
            // A browser never starts a companion's sign-in.
            (
                &post,
                "/auth/device",
                vec![(HOST, TAILNET), (ORIGIN, origin.as_str())],
                StatusCode::FORBIDDEN,
            ),
            (
                &post,
                "/auth/device",
                vec![(HOST, "attacker.example")],
                StatusCode::FORBIDDEN,
            ),
            (
                &post,
                "/auth/device/poll",
                vec![(HOST, TAILNET)],
                StatusCode::UNAUTHORIZED,
            ),
            // No pairing, and the CLI's token on loopback only.
            (
                &post,
                "/api/session",
                vec![(HOST, LOCAL), (AUTHORIZATION, bearer.as_str())],
                StatusCode::FORBIDDEN,
            ),
            (
                &get,
                "/api/snapshot",
                vec![(HOST, LOCAL), (AUTHORIZATION, bearer.as_str())],
                StatusCode::OK,
            ),
            (
                &get,
                "/api/snapshot",
                vec![(HOST, TAILNET), (AUTHORIZATION, bearer.as_str())],
                StatusCode::UNAUTHORIZED,
            ),
        ];
        for (n, (method, path, headers, expected)) in cases.into_iter().enumerate() {
            let (status, _, body) = call(&router, method.clone(), path, &headers, None).await;
            assert_eq!(status, expected, "case {n}: {method} {path}: {body}");
        }
        // A sign-in this browser started, with the wrong state.
        let (_, headers, _) = call(
            &router,
            post.clone(),
            "/auth/github/login",
            &[(HOST, TAILNET), (ORIGIN, origin.as_str())],
            None,
        )
        .await;
        let binding = set_cookies(&headers)[0]
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let (status, _, body) = call(
            &router,
            get,
            callback,
            &[(HOST, TAILNET), (COOKIE, binding.as_str())],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("doesn't match"), "{body}");
        assert!(
            fake.lock().unwrap().seen.is_empty(),
            "GitHub was never asked"
        );
    }

    #[tokio::test]
    async fn sign_in_attempts_are_rate_limited_and_services_without_an_app_say_so() {
        use header::{HOST, ORIGIN};
        let root = tempfile::tempdir().unwrap();
        let (logins, _, _) = login::tests::logins(root.path()).await;
        let router = router(test_app(root.path(), Mode::Hub, LOCAL, Some(logins)));
        let start = [(HOST, TAILNET), (ORIGIN, "https://hub.example.ts.net")];
        for _ in 0..10 {
            assert_eq!(
                call(&router, Method::POST, "/auth/github/login", &start, None)
                    .await
                    .0,
                StatusCode::OK
            );
        }
        let (status, headers, _) =
            call(&router, Method::POST, "/auth/github/login", &start, None).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(headers.contains_key(header::RETRY_AFTER));
        // Another peer on the tailnet, as the front says, isn't held up.
        let other = [
            (HOST, TAILNET),
            (ORIGIN, "https://hub.example.ts.net"),
            (
                header::HeaderName::from_static("x-forwarded-for"),
                "100.64.0.9",
            ),
        ];
        let (status, _, _) = call(&router, Method::POST, "/auth/github/login", &other, None).await;
        assert_eq!(status, StatusCode::OK);
        let local = tempfile::tempdir().unwrap();
        let router = super::router(test_app(local.path(), Mode::Local, LOCAL, None));
        let (status, _, _) = call(
            &router,
            Method::POST,
            "/auth/github/login",
            &[(HOST, LOCAL), (ORIGIN, "http://127.0.0.1:8421")],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, _, body) = call(&router, Method::GET, "/auth/status", &[(HOST, LOCAL)], None).await;
        let status: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (status["github_app"].as_bool(), status["pairing"].as_bool()),
            (Some(false), Some(true))
        );
    }

    #[tokio::test]
    async fn a_companion_signs_in_with_the_device_flow_and_pulls_with_what_it_gave() {
        use crate::oauth::tests::{token_answer, USER_CODE};
        let hub_root = tempfile::tempdir().unwrap();
        let (logins, fake, clock) = login::tests::logins(hub_root.path()).await;
        fake.lock().unwrap().polls = [
            serde_json::json!({"error": "authorization_pending"}),
            token_answer("ghu_allowed"),
        ]
        .into();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let hub = test_app(hub_root.path(), Mode::Hub, &address, Some(logins.clone()));
        let served = router(hub.clone());
        tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
        let companion_root = tempfile::tempdir().unwrap();
        let companion = test_app(companion_root.path(), Mode::Local, LOCAL, None);
        let url = format!("http://{address}/");
        let Json(start) = hub_login(
            State(companion.clone()),
            Json(HubLoginInput { url: url.clone() }),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(start["user_code"], USER_CODE);
        assert!(
            start.get("handle").is_none(),
            "the handle stays in the service"
        );
        let poll = || hub_login_poll(State(companion.clone()));
        // Too soon for the hub to ask GitHub; then GitHub says not yet.
        let Json(early) = poll().await.ok().unwrap();
        assert_eq!(early["status"], "pending");
        clock.advance(Duration::from_secs(5));
        let Json(pending) = poll().await.ok().unwrap();
        assert_eq!(pending["status"], "pending");
        clock.advance(Duration::from_secs(5));
        let Json(done) = poll().await.ok().unwrap();
        assert_eq!(
            (done["status"].as_str(), done["login"].as_str()),
            (Some("done"), Some("jmarrero"))
        );
        assert_eq!(done["hub"]["url"], url);
        assert!(poll().await.is_err(), "nothing waits any more");
        // It pulls with that credential, which reads the export and nothing
        // else, and which the hub's rotation revokes.
        let pulls = companion
            .scheduler
            .sync_all(Trigger::Explicit)
            .await
            .unwrap();
        let [pulled] = &pulls[..] else {
            panic!("{pulls:?}")
        };
        assert!(pulled.error.is_none(), "{pulled:?}");
        let sent: Vec<_> = pulled.requests.clone().unwrap().sent.into_iter().collect();
        assert_eq!(sent, [("GET".to_string(), 1)]);
        let token = hub_token(&companion);
        let mut bearer = HeaderMap::new();
        bearer.insert(header::HOST, HeaderValue::from_str(&address).unwrap());
        bearer.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        assert_eq!(hub.access.check(&Method::GET, EXPORT_PATH, &bearer), Ok(()));
        assert_eq!(
            hub.access.check(&Method::GET, "/api/snapshot", &bearer),
            Err(Denied::Scope)
        );
        let rotated = rotate_companion_token(State(hub.clone())).await;
        assert!(rotated.is_ok());
        let pulls = companion
            .scheduler
            .sync_all(Trigger::Explicit)
            .await
            .unwrap();
        assert_eq!(
            pulls[0].error_kind,
            Some(crate::connector::ErrorKind::Auth),
            "{pulls:?}"
        );
        // GitHub saw the device flow's POSTs, and only the hub sent them.
        let seen = fake.lock().unwrap().seen.clone();
        let paths: Vec<_> = seen.iter().map(|(m, p, _)| format!("{m} {p}")).collect();
        assert_eq!(
            paths,
            [
                "POST /login/device/code",
                "POST /login/oauth/access_token",
                "POST /login/oauth/access_token",
                "GET /api/v3/user"
            ]
        );
    }

    #[tokio::test]
    async fn rotating_the_companion_token_revokes_sign_ins_without_the_app_too() {
        use crate::oauth::tests::token_answer;
        let root = tempfile::tempdir().unwrap();
        let (logins, fake, clock) = login::tests::logins(root.path()).await;
        let start = logins.device_start(login::tests::PEER).await.unwrap();
        fake.lock().unwrap().polls = [token_answer("ghu_allowed")].into();
        clock.advance(Duration::from_secs(5));
        let Ok(DevicePoll::Done { token, .. }) =
            logins.device_poll(login::tests::PEER, &start.handle).await
        else {
            panic!("not done")
        };
        drop(logins);
        // The hub restarted without its App, then rotated.
        let hub = test_app(root.path(), Mode::Hub, LOCAL, None);
        assert!(rotate_companion_token(State(hub)).await.is_ok());
        let (again, _, _) = login::tests::logins(root.path()).await;
        assert!(!again.companion(&token));
    }

    /// The hub credential a companion keeps, from its data directory.
    fn hub_token(companion: &App) -> String {
        let db = companion.db.lock().unwrap();
        let link = db.hub().unwrap().unwrap();
        std::fs::read_to_string(db.root.join("secrets").join(&link.id)).unwrap()
    }

    #[test]
    fn session_cookies_are_secure_under_the_hubs_names() {
        let hub = access(true);
        assert!(hub.secure(TAILNET));
        assert!(!hub.secure(LOCAL));
        assert!(!hub.secure("attacker.example"));
        assert!(!access(false).secure(LOCAL));
    }

    #[test]
    fn hub_names() {
        for (host, valid) in [
            ("hub.example.ts.net", true),
            ("hub", true),
            ("hub.example.ts.net:8443", true),
            ("", false),
            ("hub.example.ts.net:0", false),
            ("hub.example.ts.net:x", false),
            ("https://hub.example.ts.net", false),
            ("hub..example", false),
            ("-hub.example", false),
            ("hub_1.example", false),
            ("hub.example/", false),
        ] {
            assert_eq!(valid_host(host), valid, "{host}");
        }
    }
}
