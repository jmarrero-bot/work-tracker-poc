//! The built-in connectors, and what they share: URL rules and an HTTP
//! client that keeps TLS verification, refuses redirects and never surfaces
//! a remote response body.

mod gitlab;
mod jira;

use crate::{
    connector::{
        Account, Connector, ConnectorError, ConnectorResult, ErrorKind, Refreshed, Target,
    },
    model::is_gitlab,
};
use anyhow::{bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::time::Duration;
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

/// The built-in connector for a provider.
pub fn builtin(provider: &str) -> Result<Box<dyn Connector>> {
    Ok(match provider {
        "jira" => Box::new(jira::Jira::new()?),
        p if is_gitlab(p) => Box::new(gitlab::GitLab::new()?),
        _ => bail!("Live connection currently supports Jira Data Center and GitLab"),
    })
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
    }
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
    fn only_jira_and_gitlab_are_built_in() {
        for (provider, kind) in [
            ("jira", Some("jira")),
            ("gitlab-internal", Some("gitlab")),
            ("gitlab-public", Some("gitlab")),
            ("github", None),
            ("errata", None),
        ] {
            assert_eq!(
                builtin(provider)
                    .ok()
                    .map(|c| c.kind().to_string())
                    .as_deref(),
                kind,
                "{provider}"
            );
        }
    }
}
