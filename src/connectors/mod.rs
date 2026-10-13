//! The connectors: Jira, GitLab, GitHub, Fedora Forge (Forgejo) and a
//! harness bot's work on GitHub built in, sharing URL rules and an HTTP
//! client that keeps TLS verification, refuses redirects and never surfaces
//! a remote response body; and third-party plugins run out of process.

mod forgejo;
mod github;
mod gitlab;
mod jira;
#[cfg(unix)]
mod plugin;

use crate::{
    connector::{
        Account, Connector, ConnectorError, ConnectorResult, ErrorKind, Refreshed, Target,
    },
    model::{is_gitlab, valid_provider, BotConfig, Connection},
};
use anyhow::{bail, Context, Result};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

pub fn base_url(raw: &str) -> Result<String> {
    let mut u = Url::parse(raw).context("Enter a complete HTTPS instance URL")?;
    if u.scheme() != "https"
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        bail!("Use an HTTPS instance URL without credentials, query or fragment")
    }
    let path = format!("{}/", u.path().trim_end_matches('/'));
    u.set_path(&path);
    Ok(u.to_string())
}

pub fn source_url(raw: &str) -> Result<String> {
    let u = Url::parse(raw).context("Enter a complete source URL")?;
    if !matches!(u.scheme(), "https" | "http")
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
    {
        bail!("Source links must be HTTP or HTTPS URLs without credentials")
    }
    Ok(u.to_string())
}

pub fn client() -> Result<Client> {
    // Never follow an authenticated redirect to another host or an SSO page.
    Ok(Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// The longest wait a source can ask for before more requests that is
/// honored. A rate limit frees up within an hour on GitHub, and a longer or
/// absurd `retry-after` from a proxy would otherwise hold a token for the
/// life of the process, or overflow the clock.
pub const MAX_WAIT: Duration = Duration::from_secs(60 * 60);

/// When a wait of `wait` from `now` ends, at most [`MAX_WAIT`] later.
pub fn wait_until(now: SystemTime, wait: Duration) -> SystemTime {
    now.checked_add(wait.min(MAX_WAIT)).unwrap_or(now)
}

/// Where a data directory keeps connector plugins: one executable per
/// provider, named after it.
pub const PLUGINS_DIR: &str = "plugins";

/// What built-in connectors keep between calls in one service process, for
/// every connection: GitHub's answers for conditional requests and its rate
/// limits, and how long Forgejo asked to wait. Never a credential.
#[derive(Clone, Default)]
pub struct Shared {
    github: Arc<github::Memory>,
    forgejo: Arc<forgejo::Memory>,
}

/// A plugin connection's own settings: which plugin file it trusts, and
/// which variables of the service's environment its plugin gets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfig {
    /// The SHA-256 of the plugin executable the person trusts, as
    /// lowercase hex, as `sha256sum` prints it: recorded when connecting,
    /// and changed only when the person trusts another
    /// (`workboard plugin trust`). The plugin runs only while its file
    /// hashes to it. None for a connection made before plugins were
    /// pinned, whose plugin doesn't run until the person trusts one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Names of variables of the service's environment the plugin gets
    /// besides `PATH` and `LANG`, such as a proxy's or a CA bundle's, as
    /// [`check_plugin_env`] allows them. Set by the person, never by the
    /// plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
}

/// The variables of the service's environment a plugin may get, if its
/// connection allows them by name, and no others. Each says only how to
/// reach the network, which certificates to trust, or the time zone, and
/// is read as data: none names a program, library, startup file or
/// configuration that an interpreter or a tool would run or load, as
/// `LD_PRELOAD`, `PYTHONPATH`, `BASH_ENV`, `DOTNET_STARTUP_HOOKS`,
/// `GIT_SSH_COMMAND`, `SSH_ASKPASS` or `XDG_CONFIG_HOME` do, and none holds
/// or reaches a credential, as `GH_TOKEN`, `SSH_AUTH_SOCK`, `NETRC` or
/// `KUBECONFIG` do. A list of what is safe keeps up where one of what isn't
/// can't.
///
/// - The proxies, in both cases, since tools differ in which they read:
///   where requests go, and which hosts go direct. A proxy's password in
///   its address is refused at run time like the credential.
/// - CA bundles, by the names OpenSSL, Python's requests, curl, Node.js and
///   git read: files of certificates to trust.
/// - `TZ`: a time zone, or a time zone file to read.
///
/// Not `LANG` or `LC_*`: `LANG` is Workboard's own, and glibc takes a path
/// for a locale name, loading locale data from wherever it points. Not
/// `HOME`: a plugin gets a private, empty home of its own run's instead,
/// since dotfiles there (`.curlrc`, `.gitconfig`, a shell's startup files,
/// Python's user site-packages) would run or configure code the pin
/// doesn't cover.
pub const PLUGIN_ENV_ALLOWED: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
    "TZ",
];

/// Check the names of the variables a plugin may get from the service's
/// environment, and return them sorted, each once. Only the names in
/// [`PLUGIN_ENV_ALLOWED`] are: anything else is refused, saying which are.
pub fn check_plugin_env(names: &[String]) -> Result<Vec<String>> {
    let mut checked = Vec::new();
    for name in names {
        if !PLUGIN_ENV_ALLOWED.contains(&name.as_str()) {
            let why = match name.as_str() {
                "PATH" | "LANG" => "Workboard sets it for every plugin",
                "HOME" => "every run gets a private, empty home of its own",
                _ => "it could make the plugin run or load code its pin doesn't cover, or reach a credential",
            };
            bail!(
                "A plugin can't get {name:?} from the service's environment: {why}. It may get only {}",
                PLUGIN_ENV_ALLOWED.join(", ")
            )
        }
        checked.push(name.clone());
    }
    checked.sort();
    checked.dedup();
    Ok(checked)
}

/// What a connector is built from besides its provider: its connection's
/// own settings.
#[derive(Debug, Clone, Copy, Default)]
pub struct Settings<'a> {
    /// For the bot: where its board, tracker and heartbeat are.
    pub bot: Option<&'a BotConfig>,
    /// For a plugin: which file it trusts.
    pub plugin: Option<&'a PluginConfig>,
}

impl<'a> Settings<'a> {
    /// The settings of connection `c`.
    pub fn of(c: &'a Connection) -> Self {
        Self {
            bot: c.bot.as_ref(),
            plugin: c.plugin.as_ref(),
        }
    }
}

/// Whether `provider` is served by a connector plugin: a source Workboard
/// knows, with no built-in connector.
pub fn is_plugin(provider: &str) -> bool {
    valid_provider(provider)
        && !is_gitlab(provider)
        && !matches!(provider, "jira" | "github" | "fedora" | "bot")
}

/// The executable that serves `provider` in `plugins`, which has to be
/// there.
fn plugin_path(plugins: &Path, provider: &str) -> Result<std::path::PathBuf> {
    let program = std::path::absolute(plugins.join(provider))?;
    if !program.exists() {
        // The data directory's path stays in the service's log.
        eprintln!(
            "No connector plugin for {provider} at {}",
            program.display()
        );
        bail!(
            "Live connection currently supports Jira Data Center, GitLab, GitHub and Fedora Forge, and connector plugins installed in the data directory's plugins directory; the service's log says where it looked"
        )
    }
    Ok(program)
}

/// The SHA-256 of the plugin in `plugins` that serves `provider`, as it
/// would run now, once it passes the checks every run makes.
pub fn plugin_sha256(plugins: &Path, provider: &str) -> Result<String> {
    if !is_plugin(provider) {
        bail!("{provider} has a built-in connector, not a plugin")
    }
    plugin_hash(&plugin_path(plugins, provider)?)
}

#[cfg(unix)]
fn plugin_hash(program: &Path) -> Result<String> {
    plugin::sha256_of(program)
}

#[cfg(not(unix))]
fn plugin_hash(_: &Path) -> Result<String> {
    bail!("Connector plugins need Linux or macOS")
}

/// The connector for a provider: built in for Jira, GitLab, GitHub, Fedora
/// Forge and the bot, which reads where its settings say, otherwise the
/// plugin named after the provider in `plugins`, if there is one.
pub fn for_provider(
    provider: &str,
    settings: Settings,
    plugins: &Path,
    shared: &Shared,
) -> Result<Box<dyn Connector>> {
    Ok(match provider {
        "jira" => Box::new(jira::Jira::new()?),
        p if is_gitlab(p) => Box::new(gitlab::GitLab::new()?),
        "github" => Box::new(github::GitHub::new(shared.github.clone())?),
        "bot" => {
            let config = settings.bot.context(
                "A bot connection needs the bot's board, tracker, heartbeat issue, login and forge organization",
            )?;
            Box::new(github::Bot::new(shared.github.clone(), config.clone())?)
        }
        "fedora" => Box::new(forgejo::Forgejo::new(shared.forgejo.clone())?),
        p if is_plugin(p) => plugin(p, &plugin_path(plugins, p)?, settings.plugin)?,
        _ => bail!("Choose a supported source"),
    })
}

#[cfg(unix)]
fn plugin(
    provider: &str,
    program: &Path,
    config: Option<&PluginConfig>,
) -> Result<Box<dyn Connector>> {
    // What the service's environment holds now, of what the connection
    // allows: an unset variable stays unset.
    let names = check_plugin_env(config.map_or(&[], |c| &c.env))?;
    let env = names
        .into_iter()
        .filter_map(|name| std::env::var_os(&name).map(|value| (name, value)))
        .collect();
    Ok(Box::new(plugin::Plugin::new(
        provider,
        program,
        plugin::DEFAULT_TIMEOUT,
        config.and_then(|c| c.sha256.clone()),
        env,
    )?))
}

#[cfg(not(unix))]
fn plugin(_: &str, _: &Path, _: Option<&PluginConfig>) -> Result<Box<dyn Connector>> {
    bail!("Connector plugins need Linux or macOS")
}

/// The GitHub account a token belongs to, read with `GET /user` through the
/// read-only client: its numeric ID, and its login as of now.
pub async fn github_account(target: &Target) -> ConnectorResult<Account> {
    let github = github::GitHub::new(Arc::default()).map_err(ConnectorError::invalid)?;
    crate::connector::verify(&github, target).await
}

/// What a request found.
enum Fetched {
    Found(Value),
    /// HTTP 403 or 404: deleted, moved, or not visible to this account.
    Inaccessible(StatusCode),
}

pub(crate) fn http_error(status: StatusCode) -> ConnectorError {
    let (kind, reason) = match status {
        StatusCode::UNAUTHORIZED => (ErrorKind::Auth, "Credential rejected"),
        StatusCode::FORBIDDEN => (
            ErrorKind::Auth,
            "Access denied; check token scope and account permissions",
        ),
        StatusCode::TOO_MANY_REQUESTS => (ErrorKind::Transient, "Rate limited; try again later"),
        s if s.is_redirection() => (
            ErrorKind::Invalid,
            "Instance redirected the API request; verify its base URL and API authentication",
        ),
        s if s.is_server_error() => (ErrorKind::Transient, "Instance API request failed"),
        _ => (ErrorKind::Invalid, "Instance API request failed"),
    };
    ConnectorError::new(kind, format!("{reason} (HTTP {})", status.as_u16()))
}

/// Largest answer read from a source, in bytes.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// A response's body, read up to [`MAX_BODY`]. Its text is never shown,
/// since an error body can echo the request.
pub(crate) async fn read_body(source: &str, mut response: Response) -> ConnectorResult<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        ConnectorError::new(
            ErrorKind::Transient,
            format!("{source}'s answer was cut short"),
        )
    })? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(ConnectorError::invalid(format!(
                "{source}'s answer was too large"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) fn parse_json(source: &str, body: &[u8]) -> ConnectorResult<Value> {
    serde_json::from_slice(body).map_err(|_| {
        ConnectorError::invalid(format!(
            "{source} did not return the expected JSON response"
        ))
    })
}

/// How a source takes its credential.
#[derive(Clone, Copy)]
enum Auth {
    /// `Authorization: Bearer`, as Jira personal access tokens.
    Bearer,
    /// GitLab's `PRIVATE-TOKEN` header.
    PrivateToken,
}

/// Read-only HTTP access to an instance's API.
struct Http {
    client: Client,
    auth: Auth,
}

impl Http {
    fn new(auth: Auth) -> Result<Self> {
        Ok(Self {
            client: client()?,
            auth,
        })
    }
    async fn get(
        &self,
        target: &Target,
        path: &str,
        query: &[(&str, String)],
    ) -> ConnectorResult<Value> {
        match self.fetch(target, path, query).await? {
            Fetched::Found(v) => Ok(v),
            Fetched::Inaccessible(status) => Err(http_error(status)),
        }
    }
    /// Like `get`, but a 403 or 404 is an answer about that one resource
    /// rather than an error.
    async fn fetch(
        &self,
        target: &Target,
        path: &str,
        query: &[(&str, String)],
    ) -> ConnectorResult<Fetched> {
        let url = target
            .base_url
            .join(path)
            .map_err(ConnectorError::invalid)?;
        if url.origin() != target.base_url.origin() {
            return Err(ConnectorError::invalid("API request changed origin"));
        }
        let req = self.client.get(url).query(query);
        let token = target.credential.expose();
        let req = match self.auth {
            Auth::PrivateToken => req.header("PRIVATE-TOKEN", token),
            Auth::Bearer => req.bearer_auth(token),
        };
        let response = req.send().await.map_err(|_| {
            ConnectorError::new(
                ErrorKind::Transient,
                "Cannot reach the instance; check network, VPN and certificate trust",
            )
        })?;
        let status = response.status();
        if matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN) {
            return Ok(Fetched::Inaccessible(status));
        }
        if !status.is_success() {
            return Err(http_error(status));
        }
        // Do not surface remote error bodies, request headers or credentials.
        response.json().await.map(Fetched::Found).map_err(|_| {
            ConnectorError::invalid("Instance did not return the expected JSON response")
        })
    }
}

/// The account in a current-user response, given its stable ID.
fn account(v: &Value, id: Option<String>) -> ConnectorResult<Account> {
    let id =
        id.ok_or_else(|| ConnectorError::invalid("Instance response has no stable account ID"))?;
    let name = v["username"]
        .as_str()
        .or(v["displayName"].as_str())
        .or(v["name"].as_str())
        .unwrap_or(&id)
        .to_string();
    Ok(Account { id, name })
}

/// The answer for a tracked item that `fetch` found inaccessible.
fn inaccessible(source: &str, remote_id: &str, status: StatusCode) -> Refreshed {
    Refreshed::inaccessible(
        remote_id,
        format!(
            "{source} returned HTTP {} for this item (missing or no permission)",
            status.as_u16()
        ),
    )
}

fn string(v: &Value, key: &str) -> Result<String> {
    v[key]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("Missing field: {key}"))
}

#[cfg(test)]
fn test_connection() -> crate::model::Connection {
    crate::model::Connection {
        id: "a".into(),
        provider: "gitlab-internal".into(),
        label: "Internal".into(),
        base_url: "https://example.com/".into(),
        account_id: "1".into(),
        account_name: "me".into(),
        zone: crate::model::Zone::Internal,
        watch_user: None,
        ..Default::default()
    }
}

/// Serve an API fixture over plain HTTP, which production constructors
/// refuse, and the target that points a connector at it.
#[cfg(test)]
async fn fixture(router: axum::Router, token: &str) -> (Target, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let target = Target {
        base_url: Url::parse(&base).unwrap(),
        credential: crate::connector::Credential::new(token).unwrap(),
    };
    (target, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_credential_urls_and_preserves_subpath() {
        assert!(base_url("https://me:secret@example.com").is_err());
        assert!(base_url("http://example.com").is_err());
        assert_eq!(
            base_url("https://example.com/jira").unwrap(),
            "https://example.com/jira/"
        );
        assert!(source_url("javascript:alert(1)").is_err());
    }
    #[test]
    fn plugin_environment_allowlists_are_checked() {
        let names = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let checked = check_plugin_env(&names(PLUGIN_ENV_ALLOWED)).unwrap();
        assert_eq!(checked.len(), PLUGIN_ENV_ALLOWED.len());
        assert!(checked.is_sorted());
        assert_eq!(
            check_plugin_env(&names(&["NO_PROXY", "TZ", "NO_PROXY"])).unwrap(),
            names(&["NO_PROXY", "TZ"])
        );
        // Everything else, whatever it is: what runs or loads code, what
        // holds or reaches a credential, and what only looks harmless.
        for name in [
            "",
            "A=B",
            "PATH",
            "LANG",
            "lang",
            "HOME",
            "LC_ALL",
            "TMPDIR",
            "LD_PRELOAD",
            "ld_preload",
            "DYLD_INSERT_LIBRARIES",
            "GLIBC_TUNABLES",
            "GCONV_PATH",
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "PERL5LIB",
            "RUBYOPT",
            "NODE_OPTIONS",
            "JAVA_TOOL_OPTIONS",
            "DOTNET_STARTUP_HOOKS",
            "CORECLR_PROFILER_PATH",
            "ELECTRON_RUN_AS_NODE",
            "BASH_ENV",
            "ENV",
            "ZDOTDIR",
            "VIMINIT",
            "BROWSER",
            "SSH_ASKPASS",
            "SUDO_ASKPASS",
            "GIT_SSH_COMMAND",
            "OPENSSL_CONF",
            "XDG_CONFIG_HOME",
            "GNUPGHOME",
            "KUBECONFIG",
            "DOCKER_HOST",
            "NETRC",
            "PGPASSFILE",
            "MYSQL_PWD",
            "DATABASE_URL",
            "GH_TOKEN",
            "WORKBOARD_ERRATA_TOKEN",
            "SSH_AUTH_SOCK",
            "KRB5CCNAME",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
            "Https_Proxy",
        ] {
            let error = check_plugin_env(&names(&["TZ", name]))
                .unwrap_err()
                .to_string();
            assert!(error.contains("HTTPS_PROXY, ALL_PROXY"), "{name}: {error}");
        }
        let home = check_plugin_env(&names(&["HOME"])).unwrap_err().to_string();
        assert!(home.contains("private, empty home"), "{home}");
    }
    #[test]
    fn waits_are_capped() {
        let now = SystemTime::now();
        let second = Duration::from_secs(1);
        assert_eq!(wait_until(now, second), now + second);
        for wait in [MAX_WAIT + second, Duration::MAX] {
            assert_eq!(wait_until(now, wait), now + MAX_WAIT);
        }
    }
    #[test]
    fn connectors_by_provider() {
        let plugins = tempfile::tempdir().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plugins/fake-forge");
        // A plugin can serve a known source without a built-in connector,
        // but neither replace a built-in one nor add an unknown source.
        for name in ["errata", "jira", "github", "fedora", "bot", "bugzilla"] {
            std::fs::copy(&script, plugins.path().join(name)).unwrap();
        }
        let config = crate::bot::tests::config();
        let mut wrong = config.clone();
        wrong.tracker = "no-slash".into();
        // (provider, its bot settings, the connector)
        for (provider, bot, kind) in [
            ("jira", None, Some("jira")),
            ("gitlab-internal", None, Some("gitlab")),
            ("gitlab-public", None, Some("gitlab")),
            ("errata", None, Some("errata")),
            ("github", None, Some("github")),
            ("fedora", None, Some("fedora")),
            ("bot", Some(&config), Some("bot")),
            // The bot only reads where it is told to, never a plugin.
            ("bot", None, None),
            ("bot", Some(&wrong), None),
            ("bugzilla", None, None),
        ] {
            let settings = Settings {
                bot,
                ..Settings::default()
            };
            assert_eq!(
                for_provider(provider, settings, plugins.path(), &Shared::default())
                    .ok()
                    .map(|c| c.kind().to_string())
                    .as_deref(),
                kind,
                "{provider}"
            );
        }
        // The built-in GitHub, Forgejo and bot connectors, which count what
        // they send, and not the plugins of those names.
        for (provider, bot) in [("github", None), ("fedora", None), ("bot", Some(&config))] {
            let settings = Settings {
                bot,
                ..Settings::default()
            };
            let connector =
                for_provider(provider, settings, plugins.path(), &Shared::default()).unwrap();
            assert!(connector.requests().is_some(), "{provider}");
        }
    }
}
