use crate::{
    clock::SystemClock,
    connector::{self, Target},
    connectors,
    db::{self, Db, Mode},
    model::*,
    schedule::{self, Builtin, Policy, Scheduler, Trigger},
    zone,
};
use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, OriginalUri, Path, Request, State},
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
    time::Duration,
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
fn matches_token(a: &str, b: &str) -> bool {
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

/// Who may use the service, from where. This is the interim
/// authentication: a bearer token, and a browser cookie paired with it,
/// until GitHub App login (#12) replaces it.
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
                "Open the pairing URL printed by `workboard serve`",
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
        let text = |name| {
            headers
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
        };
        let origin = text(header::HOST)
            .and_then(|host| self.origin(host))
            .ok_or(Denied::Host)?;
        let bearer = text(header::AUTHORIZATION).and_then(|v| v.strip_prefix("Bearer "));
        let cookie = text(header::COOKIE).and_then(|v| {
            v.split(';').find_map(|s| {
                s.trim()
                    .strip_prefix(SESSION_COOKIE)
                    .and_then(|s| s.strip_prefix('='))
            })
        });
        let bearer_ok = bearer.is_some_and(|v| matches_token(v, &self.token));
        let cookie_ok = cookie.is_some_and(|v| matches_token(v, &self.token));
        if !bearer_ok && !cookie_ok {
            let companion = self.companion.as_ref().is_some_and(|token| {
                let token = token.read().unwrap_or_else(PoisonError::into_inner);
                bearer.is_some_and(|v| matches_token(v, &token))
            });
            return match companion {
                true if method == Method::GET && path == EXPORT_PATH => Ok(()),
                true => Err(Denied::Scope),
                false => Err(Denied::Unauthorized),
            };
        }
        // A browser's change must come from the page the service served
        // under the same name.
        if method != Method::GET && !bearer_ok && text(header::ORIGIN) != Some(origin.as_str()) {
            return Err(Denied::CrossOrigin);
        }
        Ok(())
    }
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
/// Replace a hub's companion token: the old one stops working at once.
async fn rotate_companion_token(State(app): State<App>) -> ApiResult<Value> {
    let current = app.access.companion.as_ref().ok_or_else(only_hubs)?;
    let token = new_token();
    replace_secret(&app.db.lock().unwrap().root.join(COMPANION_TOKEN), &token)?;
    *current.write().unwrap_or_else(PoisonError::into_inner) = token.clone();
    Ok(Json(json!({ "token": token })))
}
fn only_hubs() -> anyhow::Error {
    anyhow::anyhow!("Only a hub has a companion token")
}
/// A new random credential.
fn new_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
/// Write `value` to the private file at `path`, replacing it all at once.
fn replace_secret(path: &std::path::Path, value: &str) -> Result<()> {
    let staged = path.with_extension("new");
    db::remove_if_there(&staged)?;
    db::write_secret(&staged, value)?;
    std::fs::rename(&staged, path)?;
    Ok(())
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
}
async fn snapshot(State(app): State<App>) -> ApiResult<Board> {
    Ok(Json(Board {
        snapshot: fresh_snapshot(&app)?.0,
        mode: app.mode,
    }))
}
/// What waits on each connection's account, and apart from it what watching
/// other users found. A local view: internal work shows here, and never
/// leaves through it.
async fn mine(State(app): State<App>) -> ApiResult<crate::mine::Mine> {
    let (snapshot, watched) = fresh_snapshot(&app)?;
    Ok(Json(crate::mine::mine(&snapshot, &watched)))
}
/// What may leave this machine, for the hub: the external zone only.
async fn export_external(State(app): State<App>) -> ApiResult<zone::ExternalSnapshot> {
    Ok(Json(zone::external_only(&fresh_snapshot(&app)?.0)))
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
    let watch_user = input.watch_user.as_deref().map(str::trim);
    if let Some(login) = watch_user {
        check_watch_user(&input.provider, login)?;
    }
    let sync_interval = input.sync_interval.map(Duration::from_secs);
    if let Some(interval) = sync_interval {
        schedule::check_interval(interval)?;
    }
    let source = app.scheduler.connector(&input.provider)?;
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
        sync: SyncState::default(),
        schedule: Schedule::default(),
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
    Ok(Json(
        app.scheduler.verify(&id, input.token.as_deref()).await?,
    ))
}
async fn disconnect(State(app): State<App>, Path(id): Path<String>) -> ApiResult<Value> {
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
    Ok(Json(app.db.lock().unwrap().set_abandoned(
        &id,
        input.abandoned,
        input.force,
    )?))
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
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let mut db = Db::open_as(&root, mode)?;
    let mode = db.mode();
    if demo && db.items()?.is_empty() && db.efforts()?.is_empty() {
        db.seed()?
    }
    let token = new_token();
    let path = root.join("service.token");
    if path.exists() {
        std::fs::remove_file(&path)?
    }
    db::write_secret(&path, &token)?;
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
        }),
        scheduler,
        mode,
    };
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/mine", get(mine))
        .route("/export/external", get(export_external))
        .route("/session", post(session))
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
    let router = Router::new()
        .nest("/api", api)
        .fallback(assets)
        .layer(DefaultBodyLimit::max(65536))
        .layer(middleware::from_fn(headers))
        .with_state(app.clone());
    let running = match mode {
        Mode::Local => "Workboard is running locally",
        Mode::Hub => "Workboard is running as a hub, for external work only",
    };
    let token = &app.access.token;
    let mut urls = vec![format!("http://{local}/#token={token}")];
    urls.extend(
        app.access
            .hosts
            .iter()
            .map(|host| format!("https://{host}/#token={token}")),
    );
    println!("{running}. Open this private pairing URL:\n{}\nKeep this URL private. CLI commands use the same data directory.", urls.join("\n"));
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
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

    fn access(hub: bool) -> Access {
        Access {
            local: LOCAL.into(),
            hosts: if hub { vec![TAILNET.into()] } else { vec![] },
            token: TOKEN.into(),
            companion: hub.then(|| RwLock::new(COMPANION.into())),
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
