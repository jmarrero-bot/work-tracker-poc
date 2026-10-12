use crate::{
    connector::{self, ConnectorError, Target},
    connectors,
    db::{self, Db},
    model::*,
    zone,
};
use anyhow::{bail, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct Assets;
#[derive(Clone)]
struct App {
    db: Arc<Mutex<Db>>,
    token: Arc<String>,
    origin: String,
    sync_gate: Arc<tokio::sync::Mutex<()>>,
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
async fn protect(State(app): State<App>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok());
    if host != app.origin.strip_prefix("http://") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"Unexpected host"})),
        )
            .into_response();
    }
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let cookie = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.split(';')
                .find_map(|s| s.trim().strip_prefix("workboard_session="))
        });
    let bearer_ok = bearer.is_some_and(|v| matches_token(v, &app.token));
    let cookie_ok = cookie.is_some_and(|v| matches_token(v, &app.token));
    if !bearer_ok && !cookie_ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Open the pairing URL printed by `workboard serve`"})),
        )
            .into_response();
    }
    if req.method() != axum::http::Method::GET && !bearer_ok {
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        if origin != Some(app.origin.as_str()) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"Cross-origin changes are not allowed"})),
            )
                .into_response();
        }
    }
    next.run(req).await
}
async fn session(State(app): State<App>) -> Response {
    let mut res = Json(json!({"ok":true})).into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "workboard_session={}; Path=/api; HttpOnly; SameSite=Strict",
            app.token
        ))
        .unwrap(),
    );
    res
}
async fn snapshot(State(app): State<App>) -> ApiResult<Snapshot> {
    Ok(Json(app.db.lock().unwrap().snapshot()?))
}
/// What may leave this machine, for the hub: the external zone only.
async fn export_external(State(app): State<App>) -> ApiResult<zone::ExternalSnapshot> {
    let snapshot = app.db.lock().unwrap().snapshot()?;
    Ok(Json(zone::external_only(&snapshot)))
}
async fn connect(State(app): State<App>, Json(input): Json<ConnectInput>) -> ApiResult<Connection> {
    if input.label.trim().is_empty() || input.label.len() > 100 {
        bail_api("Enter a connection name of 1–100 characters")?
    }
    let base = connectors::base_url(&input.base_url)?;
    let source = connectors::builtin(&input.provider)?;
    let target = Target::new(&base, &input.token)?;
    let account = connector::verify(source.as_ref(), &target).await?;
    let (id, name) = (account.id, account.name);
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
        last_sync: None,
        error: None,
        zone: input.zone,
        error_kind: None,
    };
    let path = db.root.join("secrets").join(&c.id);
    db::write_secret(&path, target.credential.expose())?;
    if let Err(e) = db.save_connection(&c) {
        let _ = std::fs::remove_file(path);
        return Err(e.into());
    }
    Ok(Json(c))
}
fn bail_api(message: &str) -> Result<()> {
    bail!("{message}")
}
async fn disconnect(State(app): State<App>, Path(id): Path<String>) -> ApiResult<Value> {
    let _guard = app.sync_gate.lock().await;
    app.db.lock().unwrap().delete_connection(&id)?;
    Ok(Json(json!({"ok":true})))
}
async fn sync(State(app): State<App>) -> ApiResult<Value> {
    let _guard = app.sync_gate.lock().await;
    let connections = app.db.lock().unwrap().connections()?;
    let mut results = Vec::new();
    for mut c in connections {
        let (token, old) = {
            let db = app.db.lock().unwrap();
            (db.secret(&c.id), db.items()?)
        };
        let result = async {
            let token = token?;
            let source = connectors::builtin(&c.provider)?;
            let target = Target::new(&c.base_url, &token)?;
            Ok::<_, anyhow::Error>(connector::sync(source.as_ref(), &target, &c, &old).await?)
        }
        .await;
        let mut db = app.db.lock().unwrap();
        match result {
            Ok(items) => {
                db.sync_items(&items)?;
                c.last_sync = Some(db.sql.query_row(
                    "SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')",
                    [],
                    |r| r.get(0),
                )?);
                c.error = None;
                c.error_kind = None;
                results.push(json!({"connection":c.id,"label":c.label,"items":items.len()}));
            }
            Err(e) => {
                // Whether to fix a credential or retry later.
                c.error_kind = Some(
                    e.downcast_ref::<ConnectorError>()
                        .map_or(ErrorKind::Invalid, |e| e.kind),
                );
                c.error = Some(e.to_string());
                results.push(
                    json!({"connection":c.id,"label":c.label,"error":c.error,"error_kind":c.error_kind}),
                );
            }
        };
        db.save_connection(&c)?;
    }
    Ok(Json(json!({"results":results})))
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
    let db = app.db.lock().unwrap();
    let updated: String =
        db.sql
            .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')", [], |r| {
                r.get(0)
            })?;
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
    item.updated_at = db
        .sql
        .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')", [], |r| {
            r.get(0)
        })?;
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
pub async fn serve(root: PathBuf, port: u16, demo: bool) -> Result<()> {
    #[cfg(not(unix))]
    bail!("This initial server requires Unix file permissions; use Linux or macOS");
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let mut db = Db::open(&root)?;
    if demo && db.items()?.is_empty() && db.efforts()?.is_empty() {
        db.seed()?
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let path = root.join("service.token");
    if path.exists() {
        std::fs::remove_file(&path)?
    }
    db::write_secret(&path, &token)?;
    let origin = format!("http://127.0.0.1:{port}");
    let app = App {
        db: Arc::new(Mutex::new(db)),
        token: Arc::new(token),
        origin: origin.clone(),
        sync_gate: Arc::new(tokio::sync::Mutex::new(())),
    };
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/export/external", get(export_external))
        .route("/session", post(session))
        .route("/connections", post(connect))
        .route("/connections/{id}", axum::routing::delete(disconnect))
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
    println!("Workboard is running locally. Open this private pairing URL:\n{origin}/#token={}\nKeep this URL private. CLI commands use the same data directory.",app.token);
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
}
