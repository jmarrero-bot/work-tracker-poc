//! The service's private Unix socket, through which the CLI and the GNOME
//! app drive it with full access and no token.
//!
//! It is the session's own channel: a socket of mode `0600` in a directory
//! of mode `0700`, in `$XDG_RUNTIME_DIR/workboard` for a user session, or in
//! the data directory without one (as in a container). Every connection is
//! checked with the kernel's peer credentials (`SO_PEERCRED`, or
//! `getpeereid` on macOS) and dropped unless it comes from the service's
//! own user. A browser can't reach a Unix socket, and a proxy in front of
//! the service, such as `tailscale serve`, isn't pointed at it; were one
//! pointed at it, it would connect as its own user (tailscaled runs as
//! root) and be refused. So no Host, Origin or proxy header has to be
//! trusted for it, unlike the loopback port.
//!
//! The data directory names the socket in a file of its own, [`POINTER`],
//! with a random ID of the service that listens there, which every answer
//! on the socket carries ([`INSTANCE_HEADER`]). The CLI finds the service of
//! the data directory it is given that way, wherever the service put the
//! socket, and checks on each connection that it is that one, before it
//! sends a request there: a service on another data directory may have
//! taken over the port, and so the socket's name, after this one was
//! killed. Each request also says which service it means
//! ([`EXPECT_HEADER`]), and another service refuses it before acting on
//! it.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// The file in the data directory that names the socket of the service
/// running on it.
pub const POINTER: &str = "socket-path";
/// The header every answer on the socket carries, with the ID of the
/// service that listens there, as [`POINTER`] names it.
pub const INSTANCE_HEADER: &str = "x-workboard-instance";
/// The header with which a client says which service it means, by its ID:
/// another service refuses the request before acting on it.
pub const EXPECT_HEADER: &str = "x-workboard-expect-instance";
/// The socket's name in the data directory, when there is no runtime
/// directory.
const IN_DATA_DIR: &str = "service.sock";
/// Where in `$XDG_RUNTIME_DIR` the service puts its socket.
pub const RUNTIME_DIR: &str = "workboard";

/// The socket's name in [`RUNTIME_DIR`] for the service on `port`. One
/// service listens on a port, so its socket is the one there.
pub fn runtime_name(port: u16) -> String {
    format!("service-{port}.sock")
}

/// The session's runtime directory, if it has one.
pub fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// Where the service on `port` with the data directory `root` listens:
/// in `runtime`, the session's runtime directory, if there is one.
fn path_for(root: &Path, port: u16, runtime: Option<&Path>) -> PathBuf {
    match runtime {
        Some(dir) => dir.join(RUNTIME_DIR).join(runtime_name(port)),
        None => root.join(IN_DATA_DIR),
    }
}

/// The socket of the service running on a data directory, as that service
/// named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    /// The ID of that service, which its answers carry.
    pub instance: String,
}

/// The socket of the service running on the data directory `root`, as that
/// service named it.
pub fn find(root: &Path) -> Result<Found> {
    let pointer = root.join(POINTER);
    let text = std::fs::read_to_string(&pointer)
        .context("Start `workboard serve` with the same --data-dir first")?;
    let mut lines = text.lines().map(str::trim);
    let path = PathBuf::from(lines.next().unwrap_or_default());
    let instance = lines.next().unwrap_or_default().to_string();
    if !path.is_absolute() || instance.is_empty() {
        bail!(
            "{} doesn't name a socket; start `workboard serve` again",
            pointer.display()
        )
    }
    Ok(Found { path, instance })
}

/// Fail unless an answer came from the service `found` names: one that
/// listens on its socket now may be another data directory's.
pub fn check_instance(found: &Found, answered: Option<&str>) -> Result<()> {
    if answered != Some(found.instance.as_str()) {
        bail!(
            "The service at {} isn't the one of this data directory, which isn't running; start `workboard serve` with the same --data-dir",
            found.path.display()
        )
    }
    Ok(())
}

#[cfg(unix)]
pub use listen::{bind, Client};

#[cfg(unix)]
mod listen {
    use super::*;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::{header, Method, Request, StatusCode};
    use std::os::unix::fs::PermissionsExt;

    /// A client of the service whose socket a data directory names. Each
    /// request goes on a connection of its own, which first asks the
    /// service there who it is, so nothing is sent to another one, even if
    /// it took over the socket between two requests.
    pub struct Client {
        found: Found,
        timeout: std::time::Duration,
    }

    impl Client {
        pub fn new(found: Found, timeout: std::time::Duration) -> Self {
            Self { found, timeout }
        }

        /// `method` `path` (under `/api/`) with a JSON `body`: the status
        /// and the JSON answer.
        pub async fn call(
            &self,
            method: Method,
            path: &str,
            body: Option<&serde_json::Value>,
        ) -> Result<(StatusCode, serde_json::Value)> {
            tokio::time::timeout(self.timeout, self.send(method, path, body))
                .await
                .context("Workboard didn't answer in time")?
        }

        fn request(
            &self,
            method: Method,
            path: &str,
            body: Option<&serde_json::Value>,
        ) -> Result<Request<Full<Bytes>>> {
            let builder = Request::builder()
                .method(method)
                .uri(format!("/api/{path}"))
                // Only a name: the connection is the socket.
                .header(header::HOST, "workboard")
                .header(EXPECT_HEADER, &self.found.instance);
            Ok(match body {
                Some(body) => builder
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Full::new(Bytes::from(serde_json::to_vec(body)?)))?,
                None => builder.body(Full::new(Bytes::new()))?,
            })
        }

        /// Fail unless an answer's headers say it came from this client's
        /// service.
        fn check(&self, headers: &header::HeaderMap) -> Result<()> {
            let instance = headers.get(INSTANCE_HEADER).and_then(|v| v.to_str().ok());
            check_instance(&self.found, instance)
        }

        async fn send(
            &self,
            method: Method,
            path: &str,
            body: Option<&serde_json::Value>,
        ) -> Result<(StatusCode, serde_json::Value)> {
            let unreachable = "Cannot reach local Workboard; start the service first";
            let stream = tokio::net::UnixStream::connect(&self.found.path)
                .await
                .context(unreachable)?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                    .await
                    .context(unreachable)?;
            tokio::spawn(connection);
            // This connection's service first.
            let ping = sender
                .send_request(self.request(Method::GET, "ping", None)?)
                .await
                .context(unreachable)?;
            self.check(ping.headers())?;
            ping.into_body().collect().await?;
            sender.ready().await.context(unreachable)?;
            let answer = sender
                .send_request(self.request(method, path, body)?)
                .await
                .context(unreachable)?;
            self.check(answer.headers())?;
            let status = answer.status();
            let bytes = answer.into_body().collect().await?.to_bytes();
            let value = serde_json::from_slice(&bytes)
                .with_context(|| format!("Workboard's answer to {path} doesn't parse"))?;
            Ok((status, value))
        }
    }

    /// A Unix listener that only accepts connections from `uid`.
    pub struct OwnerOnly {
        listener: tokio::net::UnixListener,
        uid: u32,
    }

    impl OwnerOnly {
        /// Whether a peer with `uid` may connect.
        fn allows(&self, uid: u32) -> bool {
            uid == self.uid
        }
    }

    impl axum::serve::Listener for OwnerOnly {
        type Io = tokio::net::UnixStream;
        type Addr = tokio::net::unix::SocketAddr;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                // Retries failed accepts as the plain listener does.
                let (stream, addr) = axum::serve::Listener::accept(&mut self.listener).await;
                match stream.peer_cred() {
                    Ok(peer) if self.allows(peer.uid()) => return (stream, addr),
                    Ok(peer) => eprintln!(
                        "Refused a connection to the service's socket from user {}: only its own user may connect",
                        peer.uid()
                    ),
                    Err(e) => eprintln!(
                        "Refused a connection to the service's socket whose user can't be told: {e}"
                    ),
                }
            }
        }

        fn local_addr(&self) -> std::io::Result<Self::Addr> {
            self.listener.local_addr()
        }
    }

    /// The socket a service listens on, and the data directory's file that
    /// names it, both removed when the service stops.
    pub struct Bound {
        path: PathBuf,
        pointer: PathBuf,
        instance: String,
    }

    impl Bound {
        /// The service's ID, for [`INSTANCE_HEADER`].
        pub fn instance(&self) -> &str {
            &self.instance
        }
    }

    impl Drop for Bound {
        fn drop(&mut self) {
            // Only once it is gone: a CLI that reads the pointer meanwhile
            // finds nothing listening, and says to start the service.
            let _ = crate::db::remove_if_there(&self.pointer);
            let _ = crate::db::remove_if_there(&self.path);
        }
    }

    /// Listen on the private socket of the service on `port` with the data
    /// directory `root`, which the caller holds the lock of, as it holds
    /// `port`: a socket left there by a service that was killed is replaced.
    pub fn bind(root: &Path, port: u16) -> Result<(OwnerOnly, Bound)> {
        bind_in(root, port, runtime_dir().as_deref())
    }

    pub(crate) fn bind_in(
        root: &Path,
        port: u16,
        runtime: Option<&Path>,
    ) -> Result<(OwnerOnly, Bound)> {
        let path = path_for(root, port, runtime);
        let dir = path.parent().context("A socket has a directory")?;
        crate::db::private_dir(dir).with_context(|| {
            format!(
                "Cannot make {} this user's private directory for the service's socket (mode 0700); see who owns it",
                dir.display()
            )
        })?;
        // A socket a service that was killed left is replaced; one a
        // service still listens on, such as in another network namespace
        // that shares the runtime directory, is never taken over.
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            bail!(
                "Another Workboard service listens on {}; stop it first",
                path.display()
            )
        }
        crate::db::remove_if_there(&path)?;
        let listener = tokio::net::UnixListener::bind(&path).with_context(|| {
            format!(
                "Cannot listen on {}; a socket's path must be short (about 100 bytes): use a shorter --data-dir, or run in a session with XDG_RUNTIME_DIR",
                path.display()
            )
        })?;
        // Bound in a private directory, so nobody else could reach it
        // before this.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let pointer = root.join(POINTER);
        let instance = uuid::Uuid::new_v4().simple().to_string();
        let bound = Bound {
            path,
            pointer,
            instance,
        };
        let path = bound
            .path
            .to_str()
            .context("The socket's path isn't text")?;
        let named = format!("{path}\n{}\n", bound.instance);
        crate::server::replace_secret(&bound.pointer, &named)?;
        let uid = rustix::process::geteuid().as_raw();
        Ok((OwnerOnly { listener, uid }, bound))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::MetadataExt;

        #[tokio::test]
        async fn the_socket_is_private_and_named_in_the_data_directory() {
            let root = tempfile::tempdir().unwrap();
            let runtime = tempfile::tempdir().unwrap();
            for runtime in [Some(runtime.path()), None] {
                let (listener, bound) = bind_in(root.path(), 8421, runtime).unwrap();
                let path = bound.path.clone();
                let expected = match runtime {
                    Some(dir) => dir.join("workboard/service-8421.sock"),
                    None => root.path().join("service.sock"),
                };
                assert_eq!(path, expected);
                let found = find(root.path()).unwrap();
                assert_eq!(
                    (&found.path, found.instance.as_str()),
                    (&path, bound.instance())
                );
                check_instance(&found, Some(bound.instance())).unwrap();
                assert!(check_instance(&found, Some("another")).is_err());
                assert!(check_instance(&found, None).is_err());
                let mode = |p: &Path| std::fs::metadata(p).unwrap().mode() & 0o777;
                assert_eq!(mode(&path), 0o600);
                assert_eq!(mode(path.parent().unwrap()), 0o700);
                assert_eq!(mode(&root.path().join(POINTER)), 0o600);
                // This user's own connections only.
                assert!(listener.allows(rustix::process::geteuid().as_raw()));
                assert!(!listener.allows(listener.uid.wrapping_add(1)));
                assert!(!listener.allows(0) || listener.uid == 0);
                // A socket a killed service left is replaced.
                drop(listener);
                std::mem::forget(bound);
                let (_listener, bound) = bind_in(root.path(), 8421, runtime).unwrap();
                assert_ne!(find(root.path()).unwrap(), found, "a new ID");
                drop(bound);
                assert!(!path.exists());
                assert!(find(root.path()).is_err(), "gone with the service");
            }
        }

        #[tokio::test]
        async fn a_live_socket_is_never_taken_over() {
            let root = tempfile::tempdir().unwrap();
            let (_listener, bound) = bind_in(root.path(), 8421, None).unwrap();
            let error = bind_in(root.path(), 8421, None).err().unwrap().to_string();
            assert!(error.contains("listens on"), "{error}");
            assert!(bound.path.exists());
        }

        #[tokio::test]
        async fn a_connection_from_this_user_is_accepted() {
            use axum::serve::Listener;
            let root = tempfile::tempdir().unwrap();
            let (mut listener, bound) = bind_in(root.path(), 8421, None).unwrap();
            let path = bound.path.clone();
            let connect = tokio::spawn(async move { tokio::net::UnixStream::connect(path).await });
            let (stream, _) = listener.accept().await;
            assert_eq!(
                stream.peer_cred().unwrap().uid(),
                rustix::process::geteuid().as_raw()
            );
            connect.await.unwrap().unwrap();
        }
    }
}
