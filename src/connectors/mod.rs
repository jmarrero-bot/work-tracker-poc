//! The connectors: Jira, GitLab and GitHub built in, sharing URL rules and an
//! HTTP client that keeps TLS verification, refuses redirects and never
//! surfaces a remote response body; and third-party plugins run out of
//! process.

mod github;
mod gitlab;
mod jira;
#[cfg(unix)]
mod plugin;

use crate::{
    connector::{
        Account, Connector, ConnectorError, ConnectorResult, ErrorKind, Refreshed, Target,
    },
    model::{is_gitlab, valid_provider},
};
use anyhow::{bail, Context, Result};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;
use std::{path::Path, sync::Arc, time::Duration};
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

/// Where a data directory keeps connector plugins: one executable per
/// provider, named after it.
pub const PLUGINS_DIR: &str = "plugins";

/// What built-in connectors keep between calls in one service process, for
/// every connection: GitHub's answers for conditional requests and its rate
/// limits. Never a credential.
#[derive(Clone, Default)]
pub struct Shared {
    github: Arc<github::Memory>,
}

/// The connector for a provider: built in for Jira, GitLab and GitHub,
/// otherwise the plugin named after the provider in `plugins`, if there is
/// one.
pub fn for_provider(provider: &str, plugins: &Path, shared: &Shared) -> Result<Box<dyn Connector>> {
    Ok(match provider {
        "jira" => Box::new(jira::Jira::new()?),
        p if is_gitlab(p) => Box::new(gitlab::GitLab::new()?),
        "github" => Box::new(github::GitHub::new(shared.github.clone())?),
        p if valid_provider(p) => {
            let program = std::path::absolute(plugins.join(p))?;
            if !program.exists() {
                bail!(
                    "Live connection currently supports Jira Data Center, GitLab and GitHub, and connector plugins such as {}",
                    program.display()
                )
            }
            plugin(p, &program)?
        }
        _ => bail!("Choose a supported source"),
    })
}

#[cfg(unix)]
fn plugin(provider: &str, program: &Path) -> Result<Box<dyn Connector>> {
    Ok(Box::new(plugin::Plugin::new(
        provider,
        program,
        plugin::DEFAULT_TIMEOUT,
    )?))
}

#[cfg(not(unix))]
fn plugin(_: &str, _: &Path) -> Result<Box<dyn Connector>> {
    bail!("Connector plugins need Linux or macOS")
}

/// What a request found.
enum Fetched {
    Found(Value),
    /// HTTP 403 or 404: deleted, moved, or not visible to this account.
    Inaccessible(StatusCode),
}

fn http_error(status: StatusCode) -> ConnectorError {
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
async fn read_body(source: &str, mut response: Response) -> ConnectorResult<Vec<u8>> {
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

fn parse_json(source: &str, body: &[u8]) -> ConnectorResult<Value> {
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
        last_sync: None,
        error: None,
        zone: crate::model::Zone::Internal,
        error_kind: None,
        watch_user: None,
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
    fn connectors_by_provider() {
        let plugins = tempfile::tempdir().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plugins/fake-forge");
        // A plugin can serve a known source without a built-in connector,
        // but neither replace a built-in one nor add an unknown source.
        for name in ["errata", "jira", "github", "bugzilla"] {
            std::fs::copy(&script, plugins.path().join(name)).unwrap();
        }
        for (provider, kind) in [
            ("jira", Some("jira")),
            ("gitlab-internal", Some("gitlab")),
            ("gitlab-public", Some("gitlab")),
            ("errata", Some("errata")),
            ("github", Some("github")),
            ("fedora", None),
            ("bugzilla", None),
        ] {
            assert_eq!(
                for_provider(provider, plugins.path(), &Shared::default())
                    .ok()
                    .map(|c| c.kind().to_string())
                    .as_deref(),
                kind,
                "{provider}"
            );
        }
        // The built-in GitHub connector, which counts what it sends, and
        // not the plugin of that name.
        let github = for_provider("github", plugins.path(), &Shared::default()).unwrap();
        assert!(github.requests().is_some());
    }
}
