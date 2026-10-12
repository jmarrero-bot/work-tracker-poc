//! Third-party connectors: executables that speak the plugin protocol in
//! `docs/plugins.md` over standard input and output, one process per call.
//! A plugin gets the credential on standard input only, an environment with
//! nothing but `PATH` and `LANG`, and a time limit, after which its whole
//! process group is killed. Only a file owned by the user and writable by
//! nobody else is run, and on Linux exactly the file that was checked.

use crate::connector::{
    Account, BoxFuture, Connector, ConnectorError, ConnectorResult, ErrorKind, Refreshed,
    RemoteItem, Target, Tracked, PROTOCOL_VERSION,
};
use anyhow::{bail, Context, Result};
#[cfg(target_os = "linux")]
use rustix::io::FdFlags;
use rustix::process::{kill_process_group, waitid, Pid, Signal, WaitId, WaitIdOptions};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, BorrowedFd};
use std::{
    fs::{self, File},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

/// The whole environment of a plugin. Credentials never go through it.
const PLUGIN_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const PLUGIN_LANG: &str = "C.UTF-8";
/// Largest answer read from a plugin, in bytes.
const MAX_RESPONSE: u64 = 16 * 1024 * 1024;
/// Longest error message kept from a plugin, in characters.
const MAX_MESSAGE: usize = 500;
/// How long one call may take: enough for a listing of many pages.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Method {
    Verify,
    List,
    Refresh,
}

#[derive(Serialize)]
struct Instance<'a> {
    base_url: &'a str,
}

#[derive(Serialize)]
struct Request<'a> {
    protocol: u32,
    method: Method,
    instance: Instance<'a>,
    credential: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<&'a Account>,
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<&'a [Tracked]>,
}

#[derive(Deserialize)]
struct Response {
    protocol: u32,
    #[serde(default)]
    account: Option<Account>,
    #[serde(default)]
    items: Option<Vec<RemoteItem>>,
    #[serde(default)]
    results: Option<Vec<Refreshed>>,
    #[serde(default)]
    error: Option<ConnectorError>,
}

/// A connector plugin executable.
pub struct Plugin {
    kind: String,
    program: PathBuf,
    timeout: Duration,
}

/// Open the plugin at `program` and check what was opened: a regular file,
/// not a symbolic link, owned by this user and not writable by others, in a
/// directory that is the same. The caller runs the opened file rather than
/// the path, so the file checked is the file run.
fn open_checked(program: &Path) -> Result<File> {
    let user = rustix::process::geteuid().as_raw();
    let trusted = |m: &fs::Metadata| m.uid() == user && m.permissions().mode() & 0o022 == 0;
    let shown = program.display();
    let dir = program
        .parent()
        .context("A plugin is a file in a directory")?;
    let dir_meta = fs::metadata(dir)
        .with_context(|| format!("Cannot read the plugin directory {}", dir.display()))?;
    if !dir_meta.is_dir() || !trusted(&dir_meta) {
        bail!(
            "The plugin directory {} must be owned by you and not writable by other users",
            dir.display()
        )
    }
    let link = fs::symlink_metadata(program)
        .with_context(|| format!("Cannot read connector plugin {shown}"))?;
    if link.file_type().is_symlink() {
        bail!("Connector plugin {shown} is a symbolic link; install the file itself")
    }
    let file =
        File::open(program).with_context(|| format!("Cannot open connector plugin {shown}"))?;
    let meta = file.metadata()?;
    if (meta.dev(), meta.ino()) != (link.dev(), link.ino()) {
        bail!("Connector plugin {shown} changed while it was being checked")
    }
    if !meta.is_file() {
        bail!("Connector plugin {shown} is not a file")
    }
    if !trusted(&meta) {
        bail!("Connector plugin {shown} must be owned by you and not writable by other users")
    }
    Ok(file)
}

/// A plugin's process group, killed once, at the latest when dropped, so a
/// helper the plugin started can't outlive the call with its credential.
struct ProcessGroup(Option<Pid>);

impl ProcessGroup {
    fn kill(&mut self) {
        if let Some(group) = self.0.take() {
            // An empty group is fine.
            let _ = kill_process_group(group, Signal::KILL);
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Wait until process `pid` exits, but leave it to be reaped later: until
/// then its ID, which is also its process group's, can't be reused.
fn exited(pid: Pid) {
    loop {
        match waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Err(rustix::io::Errno::INTR) => continue,
            _ => return,
        }
    }
}

impl Plugin {
    /// The plugin at `program`, whose item IDs are namespaced by `kind`.
    /// Only an absolute path is run, never one looked up in `PATH`, and only
    /// a file that passes [`open_checked`].
    pub fn new(kind: &str, program: &Path, timeout: Duration) -> Result<Self> {
        let valid_kind = !kind.is_empty()
            && kind.len() <= 64
            && kind
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid_kind {
            bail!("A plugin name uses only lowercase letters, digits and hyphens")
        }
        if !program.is_absolute() {
            bail!("A plugin is run by its absolute path")
        }
        open_checked(program)?;
        Ok(Self {
            kind: kind.into(),
            program: program.into(),
            timeout,
        })
    }

    fn name(&self) -> String {
        format!("Connector plugin '{}'", self.kind)
    }

    fn invalid(&self, problem: &str) -> ConnectorError {
        ConnectorError::invalid(format!("{} {problem}", self.name()))
    }

    /// The command that runs the opened plugin `file`, in a process group
    /// of its own.
    fn command(&self, file: &File) -> Command {
        #[cfg(target_os = "linux")]
        let mut command = {
            let fd = file.as_raw_fd();
            // Executing the descriptor runs the file that was checked, even
            // if the path changed since.
            let mut command = Command::new(format!("/proc/self/fd/{fd}"));
            // SAFETY: the closure runs in the child between fork and exec,
            // and fcntl is async-signal-safe. It changes only the child's
            // copy of `fd`, which stays open in the parent until the child
            // is spawned.
            unsafe {
                command.pre_exec(move || {
                    // A script's interpreter opens the plugin again through
                    // this descriptor, so it has to stay open across exec.
                    rustix::io::fcntl_setfd(BorrowedFd::borrow_raw(fd), FdFlags::empty())?;
                    Ok(())
                });
            }
            command
        };
        // Elsewhere the path is run again; open_checked's ownership checks
        // are what keeps it from being swapped.
        #[cfg(not(target_os = "linux"))]
        let mut command = {
            let _ = file;
            Command::new(&self.program)
        };
        command
            .env_clear()
            .env("PATH", PLUGIN_PATH)
            .env("LANG", PLUGIN_LANG)
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // A plugin's own diagnostics could echo the credential.
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        command
    }

    /// Run the plugin once for one request.
    async fn call(
        &self,
        method: Method,
        target: &Target,
        account: Option<&Account>,
        items: Option<&[Tracked]>,
    ) -> ConnectorResult<Response> {
        let request = serde_json::to_vec(&Request {
            protocol: PROTOCOL_VERSION,
            method,
            instance: Instance {
                base_url: target.base_url.as_str(),
            },
            credential: target.credential.expose(),
            account,
            items,
        })
        .map_err(ConnectorError::invalid)?;
        let file = open_checked(&self.program)
            .map_err(|e| self.invalid(&format!("cannot be run: {e:#}")))?;
        let response = self.run(file, &request).await?;
        if let Some(e) = response.error {
            if e.kind == ErrorKind::Inaccessible {
                return Err(self.invalid("answered a whole call as inaccessible"));
            }
            let message: String = target
                .credential
                .redact(&e.message)
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_MESSAGE)
                .collect();
            return Err(ConnectorError::new(
                e.kind,
                format!("{}: {message}", self.name()),
            ));
        }
        Ok(response)
    }

    /// Run the opened plugin `file` with `request` on its standard input,
    /// and read its answer.
    async fn run(&self, file: File, request: &[u8]) -> ConnectorResult<Response> {
        let mut child = self
            .command(&file)
            .spawn()
            .map_err(|e| self.invalid(&format!("cannot be started: {e}")))?;
        drop(file);
        let leader = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw);
        // Declared after the child, so it is dropped first: the whole group
        // is killed on every way out of here, before the child is reaped.
        let mut group = ProcessGroup(leader);
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(self.invalid("has no standard input or output"));
        };
        let exchange = async {
            // Write and read at once, so a plugin that starts answering
            // before it read everything can't block on a full pipe.
            let write = async {
                // A plugin may exit without reading all of it; its answer,
                // or the lack of one, decides.
                let _ = stdin.write_all(request).await;
                drop(stdin);
            };
            let read = async {
                let mut out = Vec::new();
                stdout
                    .take(MAX_RESPONSE + 1)
                    .read_to_end(&mut out)
                    .await
                    .map(|_| out)
            };
            let ((), out) = tokio::join!(write, read);
            // Kill what the plugin left behind before reaping it, while
            // the group's ID can't have been reused.
            if let Some(pid) = leader {
                let _ = tokio::task::spawn_blocking(move || exited(pid)).await;
            }
            group.kill();
            (out, child.wait().await)
        };
        let finished = tokio::time::timeout(self.timeout, exchange).await;
        let Ok((out, status)) = finished else {
            return Err(ConnectorError::new(
                ErrorKind::Transient,
                format!(
                    "{} did not answer within {} seconds",
                    self.name(),
                    self.timeout.as_secs()
                ),
            ));
        };
        let out = out.map_err(|e| self.invalid(&format!("answer could not be read: {e}")))?;
        let status = status.map_err(|e| self.invalid(&format!("could not be waited for: {e}")))?;
        if !status.success() {
            return Err(self.invalid(&format!("failed ({status})")));
        }
        if out.len() as u64 > MAX_RESPONSE {
            return Err(self.invalid("answered more than the 16 MiB limit"));
        }
        let response: Response = serde_json::from_slice(&out)
            .map_err(|_| self.invalid("did not answer with a protocol response"))?;
        if response.protocol != PROTOCOL_VERSION {
            return Err(self.invalid(&format!(
                "speaks protocol version {}, not {PROTOCOL_VERSION}",
                response.protocol
            )));
        }
        Ok(response)
    }
}

impl Connector for Plugin {
    fn kind(&self) -> &str {
        &self.kind
    }
    fn verify<'a>(&'a self, target: &'a Target) -> BoxFuture<'a, ConnectorResult<Account>> {
        Box::pin(async move {
            self.call(Method::Verify, target, None, None)
                .await?
                .account
                .ok_or_else(|| self.invalid("answered verify without an account"))
        })
    }
    fn list<'a>(
        &'a self,
        target: &'a Target,
        account: &'a Account,
    ) -> BoxFuture<'a, ConnectorResult<Vec<RemoteItem>>> {
        Box::pin(async move {
            self.call(Method::List, target, Some(account), None)
                .await?
                .items
                .ok_or_else(|| self.invalid("answered list without items"))
        })
    }
    fn refresh<'a>(
        &'a self,
        target: &'a Target,
        account: &'a Account,
        tracked: &'a [Tracked],
    ) -> BoxFuture<'a, ConnectorResult<Vec<Refreshed>>> {
        Box::pin(async move {
            self.call(Method::Refresh, target, Some(account), Some(tracked))
                .await?
                .results
                .ok_or_else(|| self.invalid("answered refresh without results"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        connector,
        model::{Connection, Outcome, Zone},
    };
    use std::time::Instant;

    /// The script plugin under `tests/plugins/`. It checks that it gets no
    /// arguments, the minimal environment and the test credential.
    fn fake_forge(timeout: Duration) -> Plugin {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plugins/fake-forge");
        Plugin::new("fake-forge", &path, timeout).unwrap()
    }
    fn target(base_url: &str, credential: &str) -> Target {
        Target::new(base_url, credential).unwrap()
    }
    fn connection() -> Connection {
        Connection {
            id: "c1".into(),
            provider: "errata".into(),
            label: "Fake forge".into(),
            base_url: "https://forge.example/".into(),
            account_id: "42".into(),
            account_name: "fixture".into(),
            last_sync: None,
            error: None,
            zone: Zone::Internal,
            error_kind: None,
        }
    }

    #[tokio::test]
    async fn syncs_through_a_script_plugin() {
        let plugin = fake_forge(DEFAULT_TIMEOUT);
        let target = target("https://forge.example/", "test-only-token");
        let c = connection();
        let account = plugin.verify(&target).await.unwrap();
        assert_eq!(
            (account.id.as_str(), account.name.as_str()),
            ("42", "fixture")
        );
        let old: Vec<_> = ["3", "gone-4", "echo-5"]
            .into_iter()
            .map(|id| {
                let mut remote = plugin_item(id);
                remote.outcome = Outcome::Open;
                connector::to_item(&c, "fake-forge", remote).unwrap()
            })
            .collect();
        let items = connector::sync(&plugin, &target, &c, &old).await.unwrap();
        let found: Vec<_> = items
            .iter()
            .map(|i| (i.id.as_str(), i.outcome, i.action.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                ("c1:fake-forge:1", Outcome::Open, "review"),
                ("c1:fake-forge:2", Outcome::Open, "next"),
                ("c1:fake-forge:3", Outcome::Delivered, ""),
                ("c1:fake-forge:echo-5", Outcome::Unknown, ""),
                ("c1:fake-forge:gone-4", Outcome::Unknown, ""),
            ]
        );
        assert!(items[4].outcome_reason.contains("HTTP 404"));
        // The script echoes the credential in this item's error.
        assert!(
            items[3].outcome_reason.contains("[redacted]")
                && !items[3].outcome_reason.contains("test-only-token"),
            "{}",
            items[3].outcome_reason
        );
        assert_eq!(items[0].url, "https://forge.example/changes/1");
    }

    /// An item as the script reports one, for building stored items.
    fn plugin_item(id: &str) -> RemoteItem {
        RemoteItem {
            remote_id: id.into(),
            key: format!("FF-{id}"),
            title: format!("Fake change {id}"),
            url: format!("https://forge.example/changes/{id}"),
            status: "open".into(),
            outcome: Outcome::Open,
            outcome_reason: "Fake forge says open".into(),
            action: "next".into(),
            updated_at: "2026-10-12T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn plugin_failures_are_typed() {
        use ErrorKind::*;
        let timeout = Duration::from_secs(2);
        let plugin = fake_forge(timeout);
        for (base_url, credential, kind, message) in [
            (
                "https://forge.example/",
                "wrong-token",
                Auth,
                "Credential rejected",
            ),
            (
                "https://denied.example/",
                "test-only-token",
                Auth,
                "No access",
            ),
            (
                "https://flaky.example/",
                "test-only-token",
                Transient,
                "Try later",
            ),
            (
                "https://slow.example/",
                "test-only-token",
                Transient,
                "within 2 seconds",
            ),
            (
                "https://garbage.example/",
                "test-only-token",
                Invalid,
                "protocol response",
            ),
            (
                "https://future.example/",
                "test-only-token",
                Invalid,
                "version 2",
            ),
            (
                "https://crash.example/",
                "test-only-token",
                Invalid,
                "failed",
            ),
            (
                "https://silent.example/",
                "test-only-token",
                Invalid,
                "without an account",
            ),
            (
                "https://gone.example/",
                "test-only-token",
                Invalid,
                "inaccessible",
            ),
            (
                "https://echo.example/",
                "test-only-token",
                Transient,
                "[redacted]",
            ),
        ] {
            let started = Instant::now();
            let error = plugin
                .verify(&target(base_url, credential))
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind, "{base_url}: {error}");
            assert!(error.message.contains(message), "{base_url}: {error}");
            assert!(
                error.message.starts_with("Connector plugin 'fake-forge'"),
                "{error}"
            );
            assert!(!error.message.contains("test-only-token"), "{error}");
            assert!(started.elapsed() < timeout * 3, "{base_url}");
        }
    }

    #[tokio::test]
    async fn an_account_that_repeats_the_credential_is_refused() {
        let plugin = fake_forge(DEFAULT_TIMEOUT);
        let target = target("https://echo-account.example/", "test-only-token");
        let error = connector::verify(&plugin, &target).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Invalid, "{error}");
        assert!(!error.message.contains("test-only-token"), "{error}");
        let error = connector::sync(&plugin, &target, &connection(), &[])
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Invalid, "{error}");
    }

    #[test]
    fn only_trusted_executables_are_plugins() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plugins/fake-forge");
        assert!(Plugin::new("Fake Forge", &script, DEFAULT_TIMEOUT).is_err());
        assert!(Plugin::new("fake-forge", Path::new("fake-forge"), DEFAULT_TIMEOUT).is_err());
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let error = |name: &str| {
            Plugin::new("fake-forge", &dir.join(name), DEFAULT_TIMEOUT)
                .err()
                .map(|e| format!("{e:#}"))
        };
        fs::copy(&script, dir.join("plugin")).unwrap();
        assert_eq!(error("plugin"), None);
        fs::create_dir(dir.join("dir")).unwrap();
        assert!(error("dir").unwrap().contains("not a file"));
        assert!(error("missing").is_some());
        std::os::unix::fs::symlink(&script, dir.join("link")).unwrap();
        assert!(error("link").unwrap().contains("symbolic link"));
        fs::copy(&script, dir.join("writable")).unwrap();
        fs::set_permissions(dir.join("writable"), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(error("writable").unwrap().contains("other users"));
        // A directory others can write to could have the file replaced.
        fs::set_permissions(dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(error("plugin").unwrap().contains("plugin directory"));
    }

    #[cfg(target_os = "linux")]
    const VERIFY: &[u8] = br#"{"protocol":1,"method":"verify","instance":{"base_url":"https://forge.example/"},"credential":"test-only-token"}"#;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn runs_the_file_it_checked() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("plugin");
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plugins/fake-forge");
        fs::copy(&script, &path).unwrap();
        let plugin = Plugin::new("fake-forge", &path, DEFAULT_TIMEOUT).unwrap();
        let file = open_checked(&path).unwrap();
        // Replace the plugin after it was checked.
        let other = temp.path().join("other");
        fs::write(
            &other,
            "#!/bin/sh\necho '{\"protocol\":1,\"error\":{\"kind\":\"auth\",\"message\":\"swapped\"}}'\n",
        )
        .unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&other, &path).unwrap();
        let response = plugin.run(file, VERIFY).await.unwrap();
        assert_eq!(response.account.unwrap().id, "42");
        assert!(response.error.is_none());
        // The next call checks and runs the new file.
        let error = plugin
            .verify(&target("https://forge.example/", "test-only-token"))
            .await
            .unwrap_err();
        assert!(error.message.contains("swapped"), "{error}");
    }

    /// Whether process `pid` is gone (or a zombie, killed and waiting to be
    /// reaped), within a few seconds.
    #[cfg(target_os = "linux")]
    async fn gone(pid: &str) -> bool {
        for _ in 0..100 {
            match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return true,
                Ok(stat) => {
                    let state = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start());
                    if state.is_some_and(|s| s.starts_with('Z')) {
                        return true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn helpers_die_with_the_call() {
        // The script starts a long-running helper and answers with its PID.
        let plugin = fake_forge(DEFAULT_TIMEOUT);
        let account = plugin
            .verify(&target("https://spawner.example/", "test-only-token"))
            .await
            .unwrap();
        assert!(
            gone(&account.name).await,
            "helper {} survived",
            account.name
        );
        // Here it starts one and hangs; the helper's PID goes to a file.
        let temp = tempfile::tempdir().unwrap();
        let plugin = fake_forge(Duration::from_secs(1));
        let base = format!("https://hang.example{}/", temp.path().display());
        let error = plugin
            .verify(&target(&base, "test-only-token"))
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transient, "{error}");
        let pid = fs::read_to_string(temp.path().join("helper.pid")).unwrap();
        assert!(gone(pid.trim()).await, "helper {pid} survived the timeout");
    }
}
