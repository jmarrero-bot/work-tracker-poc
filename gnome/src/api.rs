//! A client of the local Workboard service's HTTP API, the only way this app
//! gets or changes anything. It reads the service's token from the data
//! directory, as the CLI does, and never opens the database.
//!
//! The types are this app's own view of the API's JSON: only the fields it
//! shows, each with a default, so a field the service adds later doesn't
//! break it. CI checks them against a running service.

use anyhow::{anyhow, bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

/// The port `workboard serve` listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 8421;
/// The file in the data directory that holds the service's token, rotated
/// on every start of the service.
const TOKEN_FILE: &str = "service.token";
/// The service's directory in `$XDG_RUNTIME_DIR`, where it also puts its
/// token, as `service-PORT.token`.
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
}

/// Everything the app shows, read together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Board {
    pub snapshot: Snapshot,
    pub mine: Mine,
    pub bots: Vec<BotView>,
}

/// Where the local service is, and how to authenticate to it.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    data_dir: PathBuf,
    /// The service's copy of its token in the session's runtime directory,
    /// where a sandbox can read it without the data directory.
    runtime_token: Option<PathBuf>,
    base: String,
}

impl Client {
    pub fn new(data_dir: PathBuf, port: u16) -> Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from);
        Self::with_runtime(data_dir, port, runtime)
    }

    /// A client of the service on `port`, which keeps its token in
    /// `data_dir` and, if given, the runtime directory `runtime`.
    fn with_runtime(data_dir: PathBuf, port: u16, runtime: Option<PathBuf>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            // Loopback only: no proxy may see the token.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Cannot set up an HTTP client")?;
        Ok(Self {
            http,
            data_dir,
            runtime_token: runtime
                .map(|dir| dir.join(RUNTIME_DIR).join(format!("service-{port}.token"))),
            base: format!("http://127.0.0.1:{port}/api/"),
        })
    }

    /// The service's current token, as far as can be told: the copy in the
    /// runtime directory, else the data directory's. It changes whenever
    /// the service starts, so it is read again for every request, which
    /// runs off the main thread.
    fn token(&self) -> Result<String> {
        Ok(self.tokens()?.remove(0))
    }

    /// The tokens to try, in order: the runtime directory's copy, then the
    /// data directory's, if it differs. A runtime copy can be an earlier
    /// start's, if this one couldn't replace it.
    fn tokens(&self) -> Result<Vec<String>> {
        let read = |path: &Path| {
            std::fs::read_to_string(path)
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        };
        let mut tokens: Vec<String> = self
            .runtime_token
            .as_deref()
            .and_then(read)
            .into_iter()
            .collect();
        if let Some(token) = read(&self.data_dir.join(TOKEN_FILE)) {
            if !tokens.contains(&token) {
                tokens.push(token);
            }
        }
        if tokens.is_empty() {
            let runtime = self
                .runtime_token
                .as_ref()
                .map(|p| format!("{} or ", p.display()))
                .unwrap_or_default();
            bail!(
                "No Workboard service token in {runtime}{}; start `workboard serve`, with the same data directory",
                self.data_dir.display()
            )
        }
        Ok(tokens)
    }

    /// Call the local service, which the app drives with any method, as
    /// the CLI does. Sources are only read, by the service.
    #[allow(clippy::disallowed_methods)]
    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T> {
        let tokens = self.tokens()?;
        let last = tokens.len() - 1;
        let mut answer = None;
        for (i, token) in tokens.into_iter().enumerate() {
            let mut request = self
                .http
                .request(method.clone(), format!("{}{path}", self.base))
                .bearer_auth(token);
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request.send().await.map_err(|_| {
                anyhow!(
                    "Cannot reach Workboard at {}; start `workboard serve`",
                    self.base.trim_end_matches("api/")
                )
            })?;
            // A token refused may be a stale copy: the next one is tried.
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && i < last {
                continue;
            }
            answer = Some(response);
            break;
        }
        let response = answer.context("No token was tried")?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .context("Workboard's answer was cut short")?;
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

    /// The private URL that pairs a browser with the service, as `workboard
    /// serve` prints it.
    pub fn pairing_url(&self) -> Result<String> {
        let token = self.token()?;
        Ok(format!(
            "{}#token={token}",
            self.base.trim_end_matches("api/")
        ))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.call(reqwest::Method::GET, path, None).await
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
        let _: serde_json::Value = self.call(reqwest::Method::POST, "sync", Some(body)).await?;
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
                reqwest::Method::POST,
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

    /// A one-request HTTP server on loopback that answers `status` and
    /// `body`, and hands back the request it got.
    async fn serve_once(
        status: &'static str,
        body: &'static str,
    ) -> (u16, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            // Headers, then a body as long as Content-Length says.
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request).to_string();
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
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let answer = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(answer.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (port, task)
    }

    fn data_dir(token: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(token) = token {
            std::fs::write(dir.path().join(TOKEN_FILE), token).unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn sends_the_service_token_read_from_the_data_directory() {
        let dir = data_dir(Some("secret-token\n"));
        let (port, server) = serve_once("200 OK", r#"{"results":[]}"#).await;
        let client = Client::with_runtime(dir.path().into(), port, None).unwrap();
        client.sync(Some("c1")).await.unwrap();
        let request = server.await.unwrap();
        assert!(
            request.starts_with("POST /api/sync HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains("authorization: Bearer secret-token\r\n"),
            "{request}"
        );
        assert!(request.ends_with(r#"{"connection":"c1"}"#), "{request}");
    }

    #[tokio::test]
    async fn verify_posts_to_the_connection() {
        let dir = data_dir(Some("t"));
        let (port, server) = serve_once("200 OK", "{}").await;
        let client = Client::with_runtime(dir.path().into(), port, None).unwrap();
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
    async fn errors_say_what_to_do() {
        // No token: the service never started with this data directory.
        let dir = data_dir(None);
        let client = Client::with_runtime(dir.path().into(), 1, None).unwrap();
        let error = client.sync(None).await.unwrap_err().to_string();
        assert!(error.contains("start `workboard serve`"), "{error}");

        // The service's own message is shown as it is.
        let dir = data_dir(Some("t"));
        let (port, _server) =
            serve_once("400 Bad Request", r#"{"error":"Unknown connection"}"#).await;
        let client = Client::with_runtime(dir.path().into(), port, None).unwrap();
        let error = client.sync(Some("x")).await.unwrap_err().to_string();
        assert_eq!(error, "Unknown connection");

        // Without one, the status.
        let (port, _server) = serve_once("401 Unauthorized", "").await;
        let client = Client::with_runtime(dir.path().into(), port, None).unwrap();
        let error = client.sync(None).await.unwrap_err().to_string();
        assert!(error.contains("401"), "{error}");

        // Nothing listening.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let client = Client::with_runtime(dir.path().into(), port, None).unwrap();
        let error = client.sync(None).await.unwrap_err().to_string();
        assert!(error.starts_with("Cannot reach Workboard"), "{error}");
    }

    /// A stale runtime copy, an earlier start's, is refused; the data
    /// directory's token is tried next.
    #[tokio::test]
    async fn a_stale_runtime_token_falls_back_to_the_data_directory() {
        let dir = data_dir(Some("current"));
        let runtime = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        std::fs::create_dir(runtime.path().join(RUNTIME_DIR)).unwrap();
        std::fs::write(
            runtime
                .path()
                .join(RUNTIME_DIR)
                .join(format!("service-{port}.token")),
            "stale",
        )
        .unwrap();
        let server = tokio::spawn(async move {
            let mut seen = vec![];
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let current = request.contains("authorization: Bearer current\r\n");
                let (status, body) = if current {
                    ("200 OK", r#"{"results":[]}"#)
                } else {
                    ("401 Unauthorized", r#"{"error":"Unauthorized"}"#)
                };
                let answer = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(answer.as_bytes()).await.unwrap();
                seen.push(current);
            }
            seen
        });
        let client =
            Client::with_runtime(dir.path().into(), port, Some(runtime.path().into())).unwrap();
        client.sync(None).await.unwrap();
        assert_eq!(server.await.unwrap(), [false, true]);
    }

    #[test]
    fn reads_the_runtime_copy_of_the_token_first() {
        let dir = data_dir(Some("data-dir-token"));
        let runtime = tempfile::tempdir().unwrap();
        let client =
            Client::with_runtime(dir.path().into(), 8421, Some(runtime.path().into())).unwrap();
        // None there yet: the data directory's.
        assert_eq!(client.token().unwrap(), "data-dir-token");
        let published = runtime.path().join(RUNTIME_DIR);
        std::fs::create_dir(&published).unwrap();
        // Another port's is another service's.
        std::fs::write(published.join("service-8422.token"), "other").unwrap();
        assert_eq!(client.token().unwrap(), "data-dir-token");
        std::fs::write(published.join("service-8421.token"), "runtime-token\n").unwrap();
        assert_eq!(client.token().unwrap(), "runtime-token");

        let empty = data_dir(None);
        let client =
            Client::with_runtime(empty.path().into(), 8421, Some(runtime.path().join("none")))
                .unwrap();
        let error = client.token().unwrap_err().to_string();
        assert!(error.contains("service-8421.token or "), "{error}");
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
    }

    #[test]
    fn default_data_dir_is_the_clis() {
        // Only checks the shape; the environment is the test runner's.
        let dir = default_data_dir().unwrap();
        assert!(dir.ends_with("workboard"), "{}", dir.display());
    }
}
