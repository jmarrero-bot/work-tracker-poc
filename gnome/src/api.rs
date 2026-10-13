//! A client of the local Workboard service's HTTP API, the only way this app
//! gets or changes anything. It reaches the service through its private
//! Unix socket, as the CLI does, which only this user can connect to and
//! which needs no token, and never opens the database.
//!
//! The types are this app's own view of the API's JSON: only the fields it
//! shows, each with a default, so a field the service adds later doesn't
//! break it. CI checks them against a running service.

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{header, Method, Request, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

/// The port `workboard serve` listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 8421;
/// The file in the data directory that names the service's socket, and the
/// ID of the service listening there.
const SOCKET_POINTER: &str = "socket-path";
/// The header with which the service on a socket answers its ID.
const INSTANCE_HEADER: &str = "x-workboard-instance";
/// The header with which a request names the service it is for.
const EXPECT_HEADER: &str = "x-workboard-expect-instance";
/// The service's directory in `$XDG_RUNTIME_DIR`, where it puts its
/// socket, as `service-PORT.sock`.
const RUNTIME_DIR: &str = "workboard";
/// How long one request to the local service may take. A sync joins the
/// sources' own syncs, which can take a while.
const TIMEOUT: Duration = Duration::from_secs(120);

/// An item's outcome, derived by the service from its source's status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    #[default]
    Open,
    Delivered,
    ClosedWithoutDelivery,
    Abandoned,
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Zone {
    /// Never leaves this machine. What the service says when in doubt.
    #[default]
    Internal,
    External,
}

/// Why something waits on an account. One the service adds later is kept
/// as `Other`, and shown by its name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    ReviewRequested,
    TeamReviewRequested,
    Assigned,
    Mentioned,
    ChangesRequested,
    ChecksFailing,
    Authored,
    Todo,
    BotWaiting,
    #[serde(untagged)]
    Other(String),
}

/// What kind of failure a connection's last sync had.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Auth,
    Transient,
    Inaccessible,
    Invalid,
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Local,
    Hub,
}

/// Times are RFC 3339 in UTC, as the service writes them.
pub type Time = String;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SyncState {
    pub last_success: Option<Time>,
    pub next_due: Option<Time>,
    pub failures: u32,
    pub error_kind: Option<ErrorKind>,
    pub error: Option<String>,
    pub held_until: Option<Time>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub interval_secs: u64,
    pub running: bool,
    pub needs_user: bool,
    pub stale: bool,
}

impl Default for Schedule {
    /// Until the service says otherwise, nothing is current.
    fn default() -> Self {
        Self {
            interval_secs: 0,
            running: false,
            needs_user: false,
            stale: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Connection {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub account_name: String,
    pub zone: Zone,
    pub watch_user: Option<String>,
    pub sync: SyncState,
    pub schedule: Schedule,
    pub from_hub: bool,
}

/// What the bot connector reads besides an item's usual fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BotFields {
    pub priority: Option<String>,
    pub why: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Item {
    pub id: String,
    pub connection_id: Option<String>,
    pub provider: String,
    pub key: String,
    pub title: String,
    pub url: String,
    /// The source's own status, unmodified.
    pub status: String,
    pub outcome: Outcome,
    pub outcome_reason: String,
    /// The attention category: next, review, waiting, or none.
    pub action: String,
    pub updated_at: String,
    pub effort_id: Option<String>,
    pub reasons: Vec<Reason>,
    pub synced_at: Option<Time>,
    pub stale: bool,
    pub from_hub: bool,
    pub bot: Option<BotFields>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Progress {
    pub delivered: usize,
    pub total: usize,
    pub open: usize,
    pub needs_attention: usize,
    pub abandoned: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Effort {
    pub id: String,
    pub title: String,
    pub note: String,
    pub progress: Progress,
    pub from_hub: bool,
}

/// How a companion's pulls from its hub go.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct HubStatus {
    pub url: String,
    pub sync: SyncState,
    pub schedule: Schedule,
    pub connections: usize,
    pub items: usize,
}

/// `GET /api/snapshot`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub connections: Vec<Connection>,
    pub items: Vec<Item>,
    pub efforts: Vec<Effort>,
    pub mode: Mode,
    pub hub: Option<HubStatus>,
}

/// One account a Mine row waits on, and why.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct WaitsOn {
    pub account: String,
    pub connection_id: String,
    pub reasons: Vec<Reason>,
    pub stale: bool,
}

/// One thing in the Mine view, once, with every account it waits on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MineRow {
    pub id: String,
    pub provider: String,
    pub key: String,
    pub title: String,
    pub url: String,
    pub status: String,
    pub outcome: Outcome,
    pub outcome_reason: String,
    pub zone: Zone,
    pub waits_on: Vec<WaitsOn>,
    pub synced_at: Option<Time>,
    pub stale: bool,
    pub from_hub: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct WatchError {
    pub login: String,
    pub error: String,
}

/// `GET /api/mine`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Mine {
    pub mine: Vec<MineRow>,
    pub watching: Vec<MineRow>,
    pub watch_errors: Vec<WatchError>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AgentRun {
    pub name: String,
    pub item_url: String,
    pub item_key: String,
    pub status: String,
    pub started_at: Time,
    pub devspace: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Heartbeat {
    pub updated_at: Time,
    pub session: String,
    pub loop_state: String,
    pub next_wake_at: Option<Time>,
    pub runs: Vec<AgentRun>,
    pub skipped: usize,
    pub comment_url: Option<String>,
    pub stale: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BoardSummary {
    pub statuses: BTreeMap<String, usize>,
    pub fork_prs: usize,
    pub unreadable: usize,
}

impl BoardSummary {
    /// The items on the board, by status, without overflowing, whatever
    /// counts the service, or a hub before it, sent.
    pub fn total(&self) -> usize {
        self.statuses
            .values()
            .fold(0, |total, n| total.saturating_add(*n))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AgentRuns {
    pub heartbeat: Option<Heartbeat>,
    pub error: Option<String>,
    pub last_success: Option<Time>,
    pub stale: bool,
    pub board: Option<BoardSummary>,
}

/// Links that ask the bot for something, which the person opens and
/// submits in their browser, as themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Ask {
    pub issue_url: String,
    pub comment_url: Option<String>,
    pub comment: Option<String>,
}

/// One bot connection, from `GET /api/bot`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BotView {
    pub connection_id: String,
    pub label: String,
    pub bot_login: String,
    pub board_url: String,
    pub agents: Option<AgentRuns>,
    pub ask: Ask,
    pub asks: BTreeMap<String, Ask>,
    /// The hub's bot, as of the companion's last pull: read-only here.
    pub from_hub: bool,
}

/// Everything the app shows, read together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Board {
    pub snapshot: Snapshot,
    pub mine: Mine,
    pub bots: Vec<BotView>,
}

/// Where the local service is: its private socket.
#[derive(Debug, Clone)]
pub struct Client {
    data_dir: PathBuf,
    /// The socket the service on the port puts in the session's runtime
    /// directory, where a sandbox can reach it without the data directory.
    runtime_socket: Option<PathBuf>,
}

impl Client {
    pub fn new(data_dir: PathBuf, port: u16) -> Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from);
        Ok(Self::with_runtime(data_dir, port, runtime))
    }

    /// A client of the service on `port`, which puts its socket in the
    /// runtime directory `runtime`, if given, and names it in `data_dir`.
    fn with_runtime(data_dir: PathBuf, port: u16, runtime: Option<PathBuf>) -> Self {
        Self {
            data_dir,
            runtime_socket: runtime
                .map(|dir| dir.join(RUNTIME_DIR).join(format!("service-{port}.sock"))),
        }
    }

    /// The service's socket, as far as can be told: the one the data
    /// directory names, with the ID of the service there, else, as in the
    /// Flatpak, which can't read the data directory, the runtime
    /// directory's for the port. It moves only when the service starts
    /// again, but is looked up for every request, which runs off the main
    /// thread.
    fn socket(&self) -> Result<(PathBuf, Option<String>)> {
        let named = std::fs::read_to_string(self.data_dir.join(SOCKET_POINTER))
            .ok()
            .and_then(|text| {
                let mut lines = text.lines().map(str::trim);
                let path = PathBuf::from(lines.next()?);
                let instance = lines.next().filter(|i| !i.is_empty())?.to_string();
                path.is_absolute().then_some((path, Some(instance)))
            });
        if let Some(named) = named {
            return Ok(named);
        }
        if let Some(path) = self.runtime_socket.as_ref().filter(|p| p.exists()) {
            return Ok((path.clone(), None));
        }
        let runtime = self
            .runtime_socket
            .as_ref()
            .map(|p| format!("{} or ", p.display()))
            .unwrap_or_default();
        bail!(
            "No Workboard service socket at {runtime}in {}; start `workboard serve`, with the same data directory",
            self.data_dir.display()
        )
    }

    /// Call the local service, which the app drives with any method, as
    /// the CLI does. Sources are only read, by the service. Each call goes
    /// on a connection of its own, which, when the data directory names
    /// the service, first asks the service there who it is, so no request
    /// goes to another one, even one that took over the socket meanwhile.
    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T> {
        let (socket, instance) = self.socket()?;
        let (status, bytes) = tokio::time::timeout(
            TIMEOUT,
            self.send(&socket, instance.as_deref(), method, path, body.as_ref()),
        )
        .await
        .context("Workboard didn't answer in time")??;
        if !status.is_success() {
            let message = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("Workboard answered HTTP {status}"));
            bail!("{message}")
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Workboard's answer to {path} doesn't parse"))
    }

    async fn send(
        &self,
        socket: &Path,
        instance: Option<&str>,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(StatusCode, Bytes)> {
        let unreachable = || {
            anyhow!(
                "Cannot reach Workboard at {}; start `workboard serve`",
                socket.display()
            )
        };
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(|_| unreachable())?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .map_err(|_| unreachable())?;
        tokio::spawn(connection);
        let request = |method: Method, path: &str, body: Option<&serde_json::Value>| {
            let mut builder = Request::builder()
                .method(method)
                .uri(format!("/api/{path}"))
                // Only a name: the connection is the socket.
                .header(header::HOST, "workboard");
            if let Some(instance) = instance {
                // Another service refuses it before acting on it.
                builder = builder.header(EXPECT_HEADER, instance);
            }
            let request = match body {
                Some(body) => builder
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Full::new(Bytes::from(serde_json::to_vec(body)?))),
                None => builder.body(Full::new(Bytes::new())),
            };
            request.context("Cannot build a request")
        };
        let mismatch = |answer: &hyper::Response<hyper::body::Incoming>| {
            let answered = answer
                .headers()
                .get(INSTANCE_HEADER)
                .and_then(|v| v.to_str().ok());
            instance.is_some() && answered != instance
        };
        let other = || {
            anyhow!(
                "The Workboard service at {} isn't the one of {}; start `workboard serve`, with that data directory",
                socket.display(),
                self.data_dir.display()
            )
        };
        // This connection's service first, when the data directory says
        // which it is.
        if instance.is_some() {
            let ping = sender
                .send_request(request(Method::GET, "ping", None)?)
                .await
                .map_err(|_| unreachable())?;
            if mismatch(&ping) {
                return Err(other());
            }
            ping.into_body().collect().await?;
            sender.ready().await.map_err(|_| unreachable())?;
        }
        let answer = sender
            .send_request(request(method, path, body)?)
            .await
            .map_err(|_| unreachable())?;
        if mismatch(&answer) {
            return Err(other());
        }
        let status = answer.status();
        let bytes = answer
            .into_body()
            .collect()
            .await
            .context("Workboard's answer was cut short")?
            .to_bytes();
        Ok((status, bytes))
    }

    /// A new URL that pairs a browser with the service, with a code that
    /// works once, for a few minutes: never the service's token.
    pub async fn pairing_url(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct Pairing {
            urls: Vec<String>,
        }
        let pairing: Pairing = self
            .call(Method::POST, "pairing", Some(serde_json::json!({})))
            .await?;
        let url = pairing
            .urls
            .into_iter()
            .next()
            .context("Workboard sent no pairing URL")?;
        if !url.starts_with("http://127.0.0.1:") {
            bail!("Workboard sent a pairing URL that isn't on this machine")
        }
        Ok(url)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.call(Method::GET, path, None).await
    }

    /// Everything the app shows.
    pub async fn board(&self) -> Result<Board> {
        let (snapshot, mine, bots) =
            tokio::try_join!(self.get("snapshot"), self.get("mine"), self.get("bot"))?;
        Ok(Board {
            snapshot,
            mine,
            bots,
        })
    }

    /// Sync every connection now, or the one given, joining syncs that
    /// already run. The answer's per-connection results are in the board.
    pub async fn sync(&self, connection: Option<&str>) -> Result<()> {
        let body = serde_json::json!({ "connection": connection });
        let _: serde_json::Value = self.call(Method::POST, "sync", Some(body)).await?;
        Ok(())
    }

    /// Check a connection's credential again, which resumes its scheduled
    /// syncs if it works.
    pub async fn verify(&self, connection: &str) -> Result<()> {
        if connection.contains('/') {
            bail!("Not a connection ID: {connection}")
        }
        let _: serde_json::Value = self
            .call(
                Method::POST,
                &format!("connections/{connection}/verify"),
                Some(serde_json::json!({})),
            )
            .await?;
        Ok(())
    }
}

/// The data directory the service uses by default, as the CLI finds it.
/// In a Flatpak sandbox, XDG_DATA_HOME is the app's own, so the host's is
/// used instead, which the sandbox is given read access to.
pub fn default_data_dir() -> Result<PathBuf> {
    let flatpak = Path::new("/.flatpak-info").exists();
    let data_home = if flatpak {
        std::env::var_os("HOST_XDG_DATA_HOME")
    } else {
        std::env::var_os("XDG_DATA_HOME")
    };
    if let Some(dir) = data_home.filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("workboard"));
    }
    let home = std::env::var_os("HOME").context("Set --data-dir when HOME is unavailable")?;
    Ok(PathBuf::from(home).join(".local/share/workboard"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// An HTTP server on the Unix socket at `path`, for one connection,
    /// that answers its requests in turn with `answers` (status, extra
    /// header lines, body), and hands back the requests it got.
    fn serve(
        path: &Path,
        answers: Vec<(&'static str, &'static str, &'static str)>,
    ) -> tokio::task::JoinHandle<Vec<String>> {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut seen = vec![];
            let mut pending = Vec::new();
            let last = answers.len().saturating_sub(1);
            for (n, (status, extra, body)) in answers.into_iter().enumerate() {
                // Headers, then a body as long as Content-Length says.
                let request = loop {
                    let text = String::from_utf8_lossy(&pending).to_string();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if rest.len() >= length {
                            let end = head.len() + 4 + length;
                            let request = text[..end].to_string();
                            pending.drain(..end);
                            break Some(request);
                        }
                    }
                    let mut buf = [0u8; 4096];
                    let read = socket.read(&mut buf).await.unwrap();
                    if read == 0 {
                        break None;
                    }
                    pending.extend_from_slice(&buf[..read]);
                };
                let Some(request) = request else { break };
                seen.push(request);
                let connection = if n == last { "close" } else { "keep-alive" };
                let answer = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: {connection}\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(answer.as_bytes()).await.unwrap();
            }
            seen
        })
    }

    /// [`serve`], for one request: the request it got.
    fn serve_once(
        path: &Path,
        status: &'static str,
        body: &'static str,
    ) -> tokio::task::JoinHandle<String> {
        let server = serve(path, vec![(status, "", body)]);
        tokio::spawn(async move { server.await.unwrap().remove(0) })
    }

    /// A data directory, a runtime directory, and a client of the service
    /// on port 8421, whose socket would be at the path returned.
    fn service() -> (tempfile::TempDir, tempfile::TempDir, Client, PathBuf) {
        let data = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let client = Client::with_runtime(data.path().into(), 8421, Some(runtime.path().into()));
        let socket = runtime.path().join(RUNTIME_DIR).join("service-8421.sock");
        (data, runtime, client, socket)
    }

    #[tokio::test]
    async fn calls_the_service_on_its_socket_without_a_token() {
        let (_data, _runtime, client, socket) = service();
        let server = serve_once(&socket, "200 OK", r#"{"results":[]}"#);
        client.sync(Some("c1")).await.unwrap();
        let request = server.await.unwrap();
        assert!(
            request.starts_with("POST /api/sync HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(!request.contains("authorization"), "{request}");
        assert!(request.ends_with(r#"{"connection":"c1"}"#), "{request}");
    }

    #[tokio::test]
    async fn verify_posts_to_the_connection() {
        let (_data, _runtime, client, socket) = service();
        let server = serve_once(&socket, "200 OK", "{}");
        client.verify("abc-123").await.unwrap();
        let request = server.await.unwrap();
        assert!(
            request.starts_with("POST /api/connections/abc-123/verify HTTP/1.1\r\n"),
            "{request}"
        );
        // Nothing that could replace the credential.
        assert!(request.ends_with("\r\n\r\n{}"), "{request}");
        assert!(client.verify("../sync").await.is_err());
    }

    #[tokio::test]
    async fn the_web_page_opens_with_a_one_time_pairing_url() {
        let (_data, _runtime, client, socket) = service();
        let server = serve_once(
            &socket,
            "200 OK",
            r#"{"urls":["http://127.0.0.1:8421/#pair=one-time","https://hub.example.ts.net/#pair=one-time"],"expires_in":600}"#,
        );
        let url = client.pairing_url().await.unwrap();
        assert_eq!(url, "http://127.0.0.1:8421/#pair=one-time");
        let request = server.await.unwrap();
        assert!(
            request.starts_with("POST /api/pairing HTTP/1.1\r\n"),
            "{request}"
        );
        // Only ever a page on this machine.
        let (_data, _runtime, client, socket) = service();
        let _server = serve_once(
            &socket,
            "200 OK",
            r#"{"urls":["https://elsewhere.example/"]}"#,
        );
        assert!(client.pairing_url().await.is_err());
    }

    #[tokio::test]
    async fn errors_say_what_to_do() {
        // No socket: the service never started with this data directory.
        let (data, _runtime, client, socket) = service();
        let error = client.sync(None).await.unwrap_err().to_string();
        assert!(error.contains("start `workboard serve`"), "{error}");

        // The service's own message is shown as it is.
        let server = serve_once(
            &socket,
            "400 Bad Request",
            r#"{"error":"Unknown connection"}"#,
        );
        let error = client.sync(Some("x")).await.unwrap_err().to_string();
        assert_eq!(error, "Unknown connection");
        server.await.unwrap();

        // Without one, the status.
        std::fs::remove_file(&socket).unwrap();
        let _server = serve_once(&socket, "401 Unauthorized", "");
        let error = client.sync(None).await.unwrap_err().to_string();
        assert!(error.contains("401"), "{error}");

        // A socket nothing listens on, as a killed service leaves.
        let gone = data.path().join("gone.sock");
        drop(std::os::unix::net::UnixListener::bind(&gone).unwrap());
        std::fs::write(
            data.path().join(SOCKET_POINTER),
            format!("{}\nkilled\n", gone.display()),
        )
        .unwrap();
        let client = Client::with_runtime(data.path().into(), 8421, None);
        let error = client.sync(None).await.unwrap_err().to_string();
        assert!(error.starts_with("Cannot reach Workboard"), "{error}");
    }

    #[test]
    fn finds_the_socket_the_data_directory_names_else_by_port() {
        let (data, runtime, client, socket) = service();
        assert!(client.socket().is_err());
        // Another port's is another service's.
        let published = runtime.path().join(RUNTIME_DIR);
        std::fs::create_dir(&published).unwrap();
        std::fs::write(published.join("service-8422.sock"), "").unwrap();
        assert!(client.socket().is_err());
        // Without the data directory, as in the Flatpak: the port's.
        std::fs::write(&socket, "").unwrap();
        assert_eq!(client.socket().unwrap(), (socket.clone(), None));
        // The data directory names it, and the service there, first.
        let named = data.path().join("service.sock");
        std::fs::write(
            data.path().join(SOCKET_POINTER),
            format!("{}\nthis-one\n", named.display()),
        )
        .unwrap();
        assert_eq!(client.socket().unwrap(), (named, Some("this-one".into())));
        // A name that isn't a path, or without an ID, is no socket.
        let other = tempfile::tempdir().unwrap();
        for text in ["relative.sock\nid\n", "/run/x.sock\n"] {
            std::fs::write(other.path().join(SOCKET_POINTER), text).unwrap();
            let client = Client::with_runtime(other.path().into(), 8421, None);
            let error = client.socket().unwrap_err().to_string();
            assert!(error.contains("start `workboard serve`"), "{error}");
        }
    }

    /// A client whose data directory names its service's socket, with
    /// the ID `this-one`, and that socket.
    fn named() -> (tempfile::TempDir, Client, PathBuf) {
        let data = tempfile::tempdir().unwrap();
        let named = data.path().join("service.sock");
        std::fs::write(
            data.path().join(SOCKET_POINTER),
            format!("{}\nthis-one\n", named.display()),
        )
        .unwrap();
        let client = Client::with_runtime(data.path().into(), 8421, None);
        (data, client, named)
    }

    #[tokio::test]
    async fn another_service_on_the_named_socket_gets_no_request() {
        let (_data, client, named) = named();
        // It answers the ping with an ID of its own.
        let server = serve(
            &named,
            vec![
                (
                    "200 OK",
                    "X-Workboard-Instance: another\r\n",
                    r#"{"ok":true}"#,
                ),
                (
                    "200 OK",
                    "X-Workboard-Instance: another\r\n",
                    r#"{"results":[]}"#,
                ),
            ],
        );
        let error = client.sync(None).await.unwrap_err().to_string();
        assert!(error.contains("isn't the one of"), "{error}");
        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 1, "only the ping: {seen:?}");
        assert!(
            seen[0].starts_with("GET /api/ping HTTP/1.1\r\n"),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn the_named_service_gets_its_request_on_the_connection_it_answered() {
        let (_data, client, named) = named();
        let server = serve(
            &named,
            vec![
                (
                    "200 OK",
                    "X-Workboard-Instance: this-one\r\n",
                    r#"{"ok":true}"#,
                ),
                (
                    "200 OK",
                    "X-Workboard-Instance: this-one\r\n",
                    r#"{"results":[]}"#,
                ),
            ],
        );
        client.sync(Some("c1")).await.unwrap();
        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(
            seen[0].starts_with("GET /api/ping HTTP/1.1\r\n"),
            "{seen:?}"
        );
        assert!(
            seen[1].starts_with("POST /api/sync HTTP/1.1\r\n"),
            "{seen:?}"
        );
        // Each says which service it is for, which another refuses.
        for request in &seen {
            assert!(
                request.contains("x-workboard-expect-instance: this-one\r\n"),
                "{request}"
            );
        }
    }

    #[test]
    fn parses_what_the_service_sends() {
        let snapshot: Snapshot =
            serde_json::from_str(include_str!("../fixtures/snapshot.json")).unwrap();
        assert_eq!(snapshot.mode, Mode::Local);
        let hub = snapshot.hub.as_ref().unwrap();
        assert_eq!(hub.url, "https://hub.example.ts.net/");
        let item = snapshot.items.iter().find(|i| i.from_hub).unwrap();
        assert_eq!(item.reasons, [Reason::ReviewRequested]);
        let failing = snapshot
            .connections
            .iter()
            .find(|c| c.schedule.needs_user)
            .unwrap();
        assert_eq!(failing.sync.error_kind, Some(ErrorKind::Auth));

        let mine: Mine = serde_json::from_str(include_str!("../fixtures/mine.json")).unwrap();
        assert_eq!(
            mine.mine[0].waits_on[0].reasons,
            [Reason::ReviewRequested, Reason::Other("new_reason".into())]
        );
        let bots: Vec<BotView> =
            serde_json::from_str(include_str!("../fixtures/bot.json")).unwrap();
        let heartbeat = bots[0].agents.as_ref().unwrap().heartbeat.as_ref().unwrap();
        assert_eq!(heartbeat.runs.len(), 2);
        // A service that predates the hub's bots doesn't say.
        assert_eq!(
            bots.iter().map(|b| b.from_hub).collect::<Vec<_>>(),
            [false, true]
        );
    }

    #[test]
    fn board_totals_saturate() {
        for (counts, total) in [
            (vec![], 0),
            (vec![2, 3], 5),
            (vec![usize::MAX, 1], usize::MAX),
            (vec![usize::MAX / 2 + 1, usize::MAX / 2 + 1], usize::MAX),
        ] {
            let summary = BoardSummary {
                statuses: counts
                    .iter()
                    .enumerate()
                    .map(|(n, c)| (format!("s{n}"), *c))
                    .collect(),
                ..BoardSummary::default()
            };
            assert_eq!(summary.total(), total, "{counts:?}");
        }
    }

    #[test]
    fn default_data_dir_is_the_clis() {
        // Only checks the shape; the environment is the test runner's.
        let dir = default_data_dir().unwrap();
        assert!(dir.ends_with("workboard"), "{}", dir.display());
    }
}
