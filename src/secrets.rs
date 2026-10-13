//! Where the service keeps credentials: the connections' tokens and the
//! companion token of the hub it pulls from. A data directory keeps them in
//! one store at a time:
//!
//! - **Files**, the default: a private file each, in the data directory's
//!   `secrets` directory. A hub always keeps files.
//! - **The keyring**: the desktop session's Secret Service, which GNOME
//!   Keyring provides, encrypted at rest and locked with the session. Then
//!   `secrets` holds only a marker file, which names the keyring's store for
//!   this data directory, and no credential is ever written to a file. If
//!   the keyring can't be reached, or is locked, the service fails closed:
//!   syncs fail and back off, nothing asks for a password, and nothing
//!   falls back to files; they sync again on their own once it is back.
//!
//! Moving from one store to the other is an explicit command run while the
//! service is stopped, `workboard credentials migrate --to keyring|files`
//! ([`migrate`]); starting the service never moves anything. A lock on the
//! data directory keeps the two from running at once.
//!
//! The Secret Service is never asked to unlock a keyring once the service
//! runs, which would wait on a person while the database is held: a locked
//! keyring fails that connection's sync, as a credential to see to, and
//! every call to it has a time limit. A credential's value is never logged
//! or put in an error message.
//!
//! # Recovery
//!
//! Every change is a sequence of steps, each of which can be the last, as when
//! the process stops or a step fails. Within a store, a credential in use is
//! replaced in one step (a rename, or the keyring taking the new value), and
//! what a cut-short change leaves is sorted out at the next start; a migration
//! cut short is finished by running it again. Every rename and removal in
//! `secrets` is followed by a sync of the directory before anything counts on
//! it, so a power cut can't undo a step the next one relied on, such as the
//! marker's switch after the old copies went. The guarantee covers the files;
//! the keyring's own durability is the Secret Service's: a value gnome-keyring
//! took and gave back is taken to survive a power cut, as gnome-keyring writes
//! its keyring file and syncs it. Tests stop or fail each step of each change
//! in turn, and check that the newest committed token wins, that a removed
//! credential never comes back, and that nothing is lost or left behind.
//!
//! | Found | Done | Why |
//! | --- | --- | --- |
//! | `*.partial` | Deleted at every start and migration, first | A write cut short: the file it was for is the old one, or absent, and it may hold a token in the clear |
//! | `<id>.new`, a file or a keyring copy | Deleted at every start and migration | Staged by a verify that never completed: the token in use stays, as the verify reported |
//! | A credential no connection or hub link uses | Deleted from the store in use, once the database has been read whole ([`crate::db::Db::drop_unused_secrets`]) | A removal or a connection cut short |
//! | No marker | Files are the store | The default |
//! | A marker, migration done | The keyring is the store; the service refuses to start a hub with it | Set by a migration that completed |
//! | A marker with a migration under way | The service refuses to start until `migrate` is run again, to either store | The marker says which store is the authority: the one copied from until every copy was read back, the one copied to from then on |
//! | A marker recording another path, whose directory still has a marker naming the same store | The service and `migrate` refuse, pointing to `workboard credentials rekey` ([`rekey`]) | A copy of the data directory: its cleanup and removals would drop the original's credentials |
//! | A marker recording another path that is gone, or names another store | The path is updated, and the store kept | The directory was moved |
//! | A marker with a re-key under way | The service refuses to start until `rekey` is run again | The new store isn't whole until the marker switches to it; the shared store is never changed |
//!
//! A copy is told from the directory moved only while the original can be
//! seen at its recorded path: a copy started while the original is out of
//! view (its disk unmounted, or outside a container's or toolbox's mounts)
//! is taken for the directory moved, and records its own path. So is the
//! first of two directories to start with a marker from before paths were
//! recorded. Keep copies of a keyring data directory out of the way of the
//! original, or re-key them first.

use crate::connector::{ConnectorError, ErrorKind};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// The data directory's subdirectory of credential files.
pub const DIR: &str = "secrets";
/// A credential staged beside the one in use ends with this.
const STAGED: &str = ".new";
/// A file being written ends with this, until it is complete.
const PARTIAL: &str = ".partial";
/// See the module's documentation.
const MARKER: &str = "in-secret-service.json";

/// A store of credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Private files in the data directory, readable by this user only.
    Files,
    /// The desktop session's Secret Service, such as GNOME Keyring.
    Keyring,
}

/// A store of secrets by name, other than the data directory: the Secret
/// Service, or a fake in tests.
pub trait Vault: Send {
    fn get(&self, name: &str) -> Result<Option<String>>;
    /// Store `value` under `name`, replacing what was there.
    fn put(&self, name: &str, value: &str) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
    /// The names it holds for this data directory.
    fn names(&self) -> Result<Vec<String>>;
    /// What it is, for the person: "GNOME Keyring".
    fn describe(&self) -> &'static str;
}

/// What the marker file says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    /// The keyring's store for this data directory: its items carry it, so
    /// they are found wherever the directory moves, and its path isn't in
    /// them.
    store: String,
    /// While a migration is under way: the store that is the authority, the
    /// one copied from until every copy was read back, then the one copied
    /// to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    migrating: Option<Backend>,
    /// The data directory's canonical path when it last used the store, to
    /// tell a copy of it, which would share the store, from the directory
    /// moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    /// While a re-key is under way: the store being given to this
    /// directory, in place of `store`, which stays the original's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rekeying: Option<String>,
}

/// Where the credentials are kept now.
enum Store {
    Files,
    Keyring(Box<dyn Vault>),
    /// The keyring is the store, but couldn't be reached at start: every
    /// credential fails, closed, with why.
    Unavailable(String),
}

/// The credentials of a data directory.
pub struct Secrets {
    dir: PathBuf,
    store: Store,
}

/// Whether `name` is a credential's: an ID, staged or not. Names come from
/// the data directory and the vault, and become file names.
fn valid_name(name: &str) -> bool {
    let id = name.strip_suffix(STAGED).unwrap_or(name);
    uuid::Uuid::parse_str(id).is_ok()
}

fn checked(name: &str) -> Result<&str> {
    if !valid_name(name) {
        bail!("Not a credential's name")
    }
    Ok(name)
}

fn staged(id: &str) -> String {
    format!("{id}{STAGED}")
}

/// What a step that changes what is stored does, for the tests that stop
/// or fail each step in turn ([`fault`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Writing a file's content, before it is put in place.
    Write,
    /// Putting a file in place: a rename.
    Rename,
    /// Removing a file.
    Unlink,
    /// Making the directory's entries durable: a rename or a removal
    /// before it can be lost to a power cut.
    Sync,
    /// A call to the keyring.
    #[cfg_attr(not(test), allow(dead_code))]
    Vault,
}

/// About to take `_step`. Outside tests, nothing; in tests, it may be made
/// to fail, or to stop everything from there on as a crash would.
#[cfg(not(test))]
fn step(_step: Step) -> Result<()> {
    Ok(())
}
#[cfg(test)]
fn step(step: Step) -> Result<()> {
    fault::step(step)
}

/// Injected failures, for the tests that stop or fail every step of a
/// change in turn. A plan counts the steps taken on this thread, and makes
/// one of them fail: only that one, or, to stand for a crash, that one and
/// every one after it.
#[cfg(test)]
mod fault {
    use super::Step;
    use std::{
        cell::RefCell,
        path::{Path, PathBuf},
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        /// That step and every later one fail: the process stopped there.
        Stop,
        /// As `Stop`, and the machine lost power: renames and removals
        /// whose directory wasn't synced since are undone, as a file system
        /// may.
        Crash,
        /// Only that step fails, and what follows goes on, as callers do.
        Fail,
    }

    /// A change to a directory's entries, not yet made durable, and how to
    /// undo it.
    enum Unsynced {
        /// `to` was renamed from `from`; what was at `to` before.
        Rename {
            from: PathBuf,
            to: PathBuf,
            before: Option<Vec<u8>>,
        },
        /// The file at `path` was removed, with what it held.
        Unlink { path: PathBuf, held: Vec<u8> },
    }

    struct Plan {
        at: usize,
        mode: Mode,
        taken: usize,
        tripped: bool,
    }

    thread_local! {
        static PLAN: RefCell<Option<Plan>> = const { RefCell::new(None) };
        /// The renames and removals of the run not yet synced, in order.
        static UNSYNCED: RefCell<Vec<Unsynced>> = const { RefCell::new(Vec::new()) };
    }

    fn running() -> bool {
        PLAN.with(|plan| plan.borrow().is_some())
    }

    /// `from` is about to be renamed to `to`: keep what to undo.
    pub fn renaming(from: &Path, to: &Path) {
        if running() {
            let before = std::fs::read(to).ok();
            UNSYNCED.with(|u| {
                u.borrow_mut().push(Unsynced::Rename {
                    from: from.into(),
                    to: to.into(),
                    before,
                })
            });
        }
    }

    /// `path` is about to be removed: keep what to undo.
    pub fn unlinking(path: &Path) {
        if let (true, Ok(held)) = (running(), std::fs::read(path)) {
            UNSYNCED.with(|u| {
                u.borrow_mut().push(Unsynced::Unlink {
                    path: path.into(),
                    held,
                })
            });
        }
    }

    /// The entries of `dir` are durable.
    pub fn synced(dir: &Path) {
        UNSYNCED.with(|u| {
            u.borrow_mut().retain(|change| match change {
                Unsynced::Rename { to, .. } => to.parent() != Some(dir),
                Unsynced::Unlink { path, .. } => path.parent() != Some(dir),
            })
        });
    }

    /// The power is lost: undo what wasn't synced, last first.
    fn lose_unsynced() {
        let changes = UNSYNCED.with(|u| std::mem::take(&mut *u.borrow_mut()));
        for change in changes.into_iter().rev() {
            match change {
                Unsynced::Rename { from, to, before } => {
                    if let Ok(moved) = std::fs::read(&to) {
                        std::fs::write(&from, moved).unwrap();
                    }
                    match before {
                        Some(before) => std::fs::write(&to, before).unwrap(),
                        None => {
                            let _ = std::fs::remove_file(&to);
                        }
                    }
                }
                Unsynced::Unlink { path, held } => std::fs::write(&path, held).unwrap(),
            }
        }
    }

    pub fn step(step: Step) -> anyhow::Result<()> {
        PLAN.with(|plan| {
            let mut plan = plan.borrow_mut();
            let Some(plan) = plan.as_mut() else {
                return Ok(());
            };
            let n = plan.taken;
            plan.taken += 1;
            if plan.tripped && plan.mode != Mode::Fail {
                anyhow::bail!("Stopped before step {n}")
            }
            if n == plan.at {
                plan.tripped = true;
                anyhow::bail!("Injected failure of step {n}, {step:?}")
            }
            Ok(())
        })
    }

    /// Run `f` with step `at` failing as `mode` says, or none if `None`;
    /// returns what it returned and how many steps it took.
    pub fn run<T>(at: Option<usize>, mode: Mode, f: impl FnOnce() -> T) -> (T, usize) {
        UNSYNCED.with(|u| u.borrow_mut().clear());
        PLAN.with(|plan| {
            *plan.borrow_mut() = Some(Plan {
                at: at.unwrap_or(usize::MAX),
                mode,
                taken: 0,
                tripped: false,
            })
        });
        let out = f();
        let taken = PLAN.with(|plan| plan.borrow_mut().take().map_or(0, |p| p.taken));
        if mode == Mode::Crash {
            lose_unsynced();
        }
        UNSYNCED.with(|u| u.borrow_mut().clear());
        (out, taken)
    }
}

/// Make the entries of `dir` durable: renames and removals in it survive
/// a power cut once this returns.
fn sync_dir(dir: &Path) -> Result<()> {
    step(Step::Sync)?;
    fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("Cannot make {} durable", dir.display()))?;
    #[cfg(test)]
    fault::synced(dir);
    Ok(())
}

/// The directory `path` is in.
fn parent(path: &Path) -> Result<&Path> {
    path.parent().context("A file without a directory")
}

/// Remove the file at `path`, if there is one, durably.
fn unlink(path: &Path) -> Result<()> {
    step(Step::Unlink)?;
    #[cfg(test)]
    fault::unlinking(path);
    crate::db::remove_if_there(path)?;
    sync_dir(parent(path)?)
}

/// Rename `from` to `to`, durably: nothing is done after it that counts on
/// it, until it would survive a power cut.
fn rename(from: &Path, to: &Path) -> Result<()> {
    step(Step::Rename)?;
    #[cfg(test)]
    fault::renaming(from, to);
    fs::rename(from, to).with_context(|| format!("Cannot replace {}", to.display()))?;
    sync_dir(parent(to)?)
}

/// Write `value` to `path` whole: to a private file beside it, flushed to
/// disk, then put in its place, so `path` is the old file or the complete
/// new one, never part of it.
fn write_whole(path: &Path, value: &str) -> Result<()> {
    let mut partial = path.as_os_str().to_owned();
    partial.push(PARTIAL);
    let partial = PathBuf::from(partial);
    unlink(&partial)?;
    step(Step::Write)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&partial)
        .with_context(|| format!("Cannot write {}", partial.display()))?;
    file.write_all(value.as_bytes())?;
    file.sync_all()?;
    rename(&partial, path)
}

/// The credential in the file at `path`, if there is one: an empty file is
/// none.
fn read_file(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value.trim().to_string()).filter(|v| !v.is_empty())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("Cannot read the credential"),
    }
}

/// The keyring is the store, but couldn't be reached at start: it fails
/// closed.
fn unavailable(why: &str) -> anyhow::Error {
    // Transient, not a rejected credential: the connection backs off and
    // syncs again once the keyring is back, without being verified again.
    ConnectorError::new(
        ErrorKind::Transient,
        format!("GNOME Keyring keeps the credentials and couldn't be reached when the service started ({why}); start the service again once it can be"),
    )
    .into()
}

/// The names of the credential files in `dir`.
fn file_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = vec![];
    for entry in fs::read_dir(dir).with_context(|| format!("Cannot read {}", dir.display()))? {
        let name = entry?.file_name();
        if let Some(name) = name.to_str().filter(|n| valid_name(n)) {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Delete the partial files that writes cut short left in `dir`, since one
/// can hold a token in the clear. Only when nothing is being written.
fn drop_partials(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("Cannot read {}", dir.display()))? {
        let path = entry?.path();
        if path.to_str().is_some_and(|p| p.ends_with(PARTIAL)) {
            unlink(&path)?;
        }
    }
    Ok(())
}

fn read_marker(dir: &Path) -> Result<Option<Marker>> {
    let path = dir.join(MARKER);
    match fs::read_to_string(&path) {
        Ok(text) => {
            Ok(Some(serde_json::from_str(&text).with_context(|| {
                format!("{} doesn't parse", path.display())
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Cannot read {}", path.display())),
    }
}

fn save_marker(dir: &Path, marker: &Marker) -> Result<()> {
    write_whole(&dir.join(MARKER), &serde_json::to_string(marker)?)
}

/// The data directory `root`'s canonical path, as its marker records it.
fn canonical(root: &Path) -> Result<PathBuf> {
    fs::canonicalize(root).with_context(|| format!("Cannot resolve {}", root.display()))
}

/// Whether `a` and `b` are the same directory, such as one reached through
/// a bind mount.
fn same_dir(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (fs::metadata(a), fs::metadata(b)) {
            return a.dev() == b.dev() && a.ino() == b.ino();
        }
    }
    let _ = (a, b);
    false
}

/// Whether the data directory `root` is a copy of `original`, which still
/// uses the keyring store `store`: `original` is another directory, and its
/// marker names the same store.
fn copy_of(root: &Path, original: &Path, store: &str) -> Result<bool> {
    if same_dir(root, original) {
        return Ok(false);
    }
    // A marker that is there but can't be read can't be told apart from
    // the original's: that counts as a copy, which is refused.
    let theirs = read_marker(&original.join(DIR)).with_context(|| {
        format!(
            "Cannot tell whether this data directory is a copy of {}",
            original.display()
        )
    })?;
    Ok(theirs.is_some_and(|m| m.store == store))
}

/// What to do if a copy of a data directory used the keyring.
fn refuse_copy(original: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "This data directory is a copy of {}, which keeps its credentials in the same GNOME Keyring store; with it, this copy would drop or change the original's credentials. Run `workboard credentials rekey` with the service stopped to give this copy a store of its own, or remove the original if this directory replaces it",
        original.display()
    )
}

/// `marker`, checked to be this data directory's own and not a copy's, with
/// `root`'s path recorded: `None` if it already was. A directory moved
/// takes its store along; a copy of one whose original still uses the store
/// is refused.
fn here(root: &Path, marker: &Marker) -> Result<Option<Marker>> {
    let path = canonical(root)?;
    match &marker.path {
        Some(recorded) if *recorded == path => return Ok(None),
        Some(recorded) if copy_of(root, recorded, &marker.store)? => {
            return Err(refuse_copy(recorded))
        }
        _ => (),
    }
    Ok(Some(Marker {
        path: Some(path),
        ..marker.clone()
    }))
}

impl Secrets {
    /// Credential files in the data directory `root`.
    pub fn files(root: &Path) -> Self {
        Self {
            dir: root.join(DIR),
            store: Store::Files,
        }
    }

    /// Where they are kept, for the person.
    pub fn describe(&self) -> &'static str {
        match &self.store {
            Store::Files => "private files",
            Store::Keyring(vault) => vault.describe(),
            Store::Unavailable(_) => "GNOME Keyring, which couldn't be reached",
        }
    }

    /// The keyring, if it is the store and can be used.
    fn vault(&self) -> Result<Option<&dyn Vault>> {
        match &self.store {
            Store::Files => Ok(None),
            Store::Keyring(vault) => Ok(Some(vault.as_ref())),
            Store::Unavailable(why) => Err(unavailable(why)),
        }
    }

    pub fn read(&self, id: &str) -> Result<String> {
        let id = checked(id)?;
        let value = match self.vault()? {
            Some(vault) => vault.get(id)?,
            None => read_file(&self.dir.join(id))?,
        };
        let value = value.map(|v| v.trim().to_string()).unwrap_or_default();
        if value.is_empty() {
            bail!("Missing credential")
        }
        Ok(value)
    }

    /// Keep `value` under `name`, replacing what was there.
    fn write(&self, name: &str, value: &str) -> Result<()> {
        match self.vault()? {
            Some(vault) => vault.put(name, value),
            None => write_whole(&self.dir.join(name), value),
        }
    }

    /// Drop what is kept under `name`, if anything.
    fn delete(&self, name: &str) -> Result<()> {
        match self.vault()? {
            Some(vault) => vault.delete(name),
            None => unlink(&self.dir.join(name)),
        }
    }

    /// The names kept: credentials and staged ones.
    fn names(&self) -> Result<Vec<String>> {
        match self.vault()? {
            Some(vault) => Ok(vault
                .names()?
                .into_iter()
                .filter(|n| valid_name(n))
                .collect()),
            None => file_names(&self.dir),
        }
    }

    /// Keep `value` as `id`'s credential, which it doesn't have yet.
    pub fn create(&self, id: &str, value: &str) -> Result<()> {
        self.write(checked(id)?, value)
    }

    /// Keep `value` as `id`'s next credential, beside the one in use, for
    /// [`Secrets::install`] to put in place.
    pub fn stage(&self, id: &str, value: &str) -> Result<()> {
        self.write(&staged(checked(id)?), value)
    }

    /// Replace `id`'s credential with the staged one, in one step: a rename,
    /// or the keyring taking the new value. A sync reads the old one or the
    /// new one, and so does the next start if this is cut short.
    pub fn install(&self, id: &str) -> Result<()> {
        let id = checked(id)?;
        let name = staged(id);
        match self.vault()? {
            Some(vault) => {
                let value = vault.get(&name)?.context("No new credential is staged")?;
                vault.put(id, &value)?;
                vault.delete(&name)
            }
            None => rename(&self.dir.join(name), &self.dir.join(id)),
        }
    }

    /// Drop `id`'s staged credential, if there is one.
    pub fn discard_staged(&self, id: &str) -> Result<()> {
        self.delete(&staged(checked(id)?))
    }

    /// Remove `id`'s credential, and one staged beside it. What a removal
    /// cut short leaves, the next start drops ([`Secrets::reconcile`]).
    pub fn remove(&self, id: &str) -> Result<()> {
        let id = checked(id)?;
        self.delete(id)?;
        self.delete(&staged(id))
    }

    /// Drop every credential that no ID in `live` uses, from the store in
    /// use. Run at start, once the database has been read, so it catches
    /// what a removal or a new connection cut short left. A staged
    /// credential counts as its ID's. Returns how many went.
    pub fn reconcile(&self, live: &std::collections::HashSet<String>) -> Result<usize> {
        let mut dropped = 0;
        for name in self.names()? {
            if !live.contains(name.strip_suffix(STAGED).unwrap_or(&name)) {
                self.delete(&name)?;
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    /// Drop every staged credential: at start, one belongs to a verify that
    /// never completed.
    fn drop_staged(&self) -> Result<()> {
        for name in self.names()? {
            if name.ends_with(STAGED) {
                self.delete(&name)?;
            }
        }
        Ok(())
    }
}

/// The Secret Service isn't answering a call, so the next ones fail at once
/// until it does: each would hold the database meanwhile.
#[cfg(target_os = "linux")]
fn not_answering() -> anyhow::Error {
    ConnectorError::new(
        ErrorKind::Transient,
        "The Secret Service isn't answering; try again later",
    )
    .into()
}

/// The keyring is locked: a person has to unlock it, which the service
/// never asks for while it runs.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn locked() -> anyhow::Error {
    // Transient: syncs back off and resume once it is unlocked.
    ConnectorError::new(
        ErrorKind::Transient,
        "GNOME Keyring is locked; syncs resume once it is unlocked",
    )
    .into()
}

/// The Secret Service of the session bus, for one store. It is reached
/// from a thread of its own, since the credentials are used from
/// synchronous code, and the Secret Service client is asynchronous.
#[cfg(target_os = "linux")]
pub struct SecretService {
    requests: std::sync::mpsc::Sender<(u64, Request)>,
    calls: std::sync::Arc<std::sync::Mutex<Calls>>,
    timeout: std::time::Duration,
}

/// The calls to the Secret Service's thread, which handles them in order,
/// numbered as they are sent. Whether one is still running and whether one
/// timed out are decided under one lock, so a call that finishes just as
/// its caller gives up is never taken for one still running.
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct Calls {
    /// The number of the next call.
    next: u64,
    /// How many have finished: all the calls numbered below it.
    finished: u64,
    /// The call that timed out and hasn't finished, if one did.
    stuck: Option<u64>,
}

#[cfg(target_os = "linux")]
impl Calls {
    /// Start a call, unless one that timed out is still running: the new
    /// one would only wait behind it.
    fn start(&mut self) -> Option<u64> {
        if self.stuck.is_some() {
            return None;
        }
        self.next += 1;
        Some(self.next - 1)
    }
    /// Call `id` finished, in the thread.
    fn finished(&mut self, id: u64) {
        self.finished = id + 1;
        if self.stuck.is_some_and(|stuck| stuck <= id) {
            self.stuck = None;
        }
    }
    /// The caller of `id` gave up waiting. It is stuck only if it hasn't
    /// finished meanwhile.
    fn timed_out(&mut self, id: u64) {
        if self.finished <= id {
            self.stuck = Some(id);
        }
    }
}

#[cfg(target_os = "linux")]
fn lock(calls: &std::sync::Mutex<Calls>) -> std::sync::MutexGuard<'_, Calls> {
    calls.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(target_os = "linux")]
type Reply<T> = std::sync::mpsc::Sender<Result<T>>;

#[cfg(target_os = "linux")]
enum Request {
    Get(String, Reply<Option<String>>),
    Put(String, String, Reply<()>),
    Delete(String, Reply<()>),
    Names(Reply<Vec<String>>),
}

/// The longest one call to the Secret Service may take, once it runs.
#[cfg(target_os = "linux")]
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// The longest connecting may take, at start, which may ask the person to
/// unlock the keyring.
#[cfg(target_os = "linux")]
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[cfg(target_os = "linux")]
mod service {
    use super::*;

    /// Whose items they are.
    const APPLICATION: &str = "workboard";
    const STORE: &str = "workboard-store";
    const NAME: &str = "workboard-credential";

    pub(super) struct Session {
        keyring: oo7::Keyring,
        store: String,
    }

    impl Session {
        /// At start only: unlocking may ask the person for the password.
        pub(super) async fn open(store: String) -> Result<Self> {
            // Outside a sandbox, as the service runs, this is the Secret
            // Service.
            let keyring = oo7::Keyring::new()
                .await
                .context("Cannot reach the Secret Service on the session bus")?;
            keyring
                .unlock()
                .await
                .context("The keyring is locked, and wasn't unlocked")?;
            Ok(Self { keyring, store })
        }

        fn all(&self) -> [(&str, &str); 2] {
            [("application", APPLICATION), (STORE, &self.store)]
        }

        fn one<'a>(&'a self, name: &'a str) -> [(&'a str, &'a str); 3] {
            [
                ("application", APPLICATION),
                (STORE, &self.store),
                (NAME, name),
            ]
        }

        /// Fail if the keyring is locked, rather than ask to unlock it.
        async fn unlocked(&self) -> Result<()> {
            if self.keyring.is_locked().await? {
                return Err(locked());
            }
            Ok(())
        }

        pub(super) async fn handle(&self, request: Request) {
            // A reply that can't be sent means the caller gave up.
            match request {
                Request::Get(name, reply) => {
                    let _ = reply.send(self.get(&name).await);
                }
                Request::Put(name, value, reply) => {
                    let _ = reply.send(self.put(&name, &value).await);
                }
                Request::Delete(name, reply) => {
                    let _ = reply.send(self.delete(&name).await);
                }
                Request::Names(reply) => {
                    let _ = reply.send(self.names().await);
                }
            }
        }

        async fn get(&self, name: &str) -> Result<Option<String>> {
            self.unlocked().await?;
            let items = self
                .keyring
                .search_items(&self.one(name))
                .await
                .context("Cannot search the Secret Service")?;
            let Some(item) = items.first() else {
                return Ok(None);
            };
            if item.is_locked().await? {
                return Err(locked());
            }
            let secret = item.secret().await.context("Cannot read the credential")?;
            let text = std::str::from_utf8(secret.as_bytes())
                .context("The stored credential isn't text")?;
            Ok(Some(text.to_string()))
        }

        async fn put(&self, name: &str, value: &str) -> Result<()> {
            self.unlocked().await?;
            let label = format!("Workboard credential {name}");
            self.keyring
                .create_item(&label, &self.one(name), oo7::Secret::text(value), true)
                .await
                .context("The Secret Service didn't store the credential")
        }

        async fn delete(&self, name: &str) -> Result<()> {
            self.unlocked().await?;
            self.keyring
                .delete(&self.one(name))
                .await
                .context("The Secret Service didn't delete the credential")
        }

        async fn names(&self) -> Result<Vec<String>> {
            self.unlocked().await?;
            let items = self
                .keyring
                .search_items(&self.all())
                .await
                .context("Cannot search the Secret Service")?;
            let mut names = vec![];
            for item in items {
                if let Some(name) = item.attributes().await?.remove(NAME) {
                    names.push(name);
                }
            }
            names.sort();
            Ok(names)
        }
    }
}

#[cfg(target_os = "linux")]
impl SecretService {
    /// Connect to the session's Secret Service for `store`, unlocking its
    /// keyring if needed. Only at start: it may wait on the person.
    pub fn connect(store: &str) -> Result<Self> {
        let store = store.to_string();
        let (requests, receive) = std::sync::mpsc::channel::<(u64, Request)>();
        let (ready, started) = std::sync::mpsc::channel::<Result<()>>();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Calls::default()));
        let answered = calls.clone();
        std::thread::Builder::new()
            .name("secret-service".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        let _ = ready.send(Err(e.into()));
                        return;
                    }
                };
                let session = match runtime.block_on(service::Session::open(store)) {
                    Ok(session) => session,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let _ = ready.send(Ok(()));
                // Until the service drops its store.
                for (id, request) in receive {
                    runtime.block_on(session.handle(request));
                    lock(&answered).finished(id);
                }
            })
            .context("Cannot start the Secret Service's thread")?;
        match started.recv_timeout(CONNECT_TIMEOUT) {
            Ok(result) => result?,
            Err(_) => bail!("The Secret Service didn't answer"),
        }
        Ok(Self {
            requests,
            calls,
            timeout: CALL_TIMEOUT,
        })
    }

    /// Ask the Secret Service's thread, within the time limit. Once a call
    /// timed out, the next ones fail at once until it finished, since each
    /// would wait behind it.
    fn ask<T>(&self, request: impl FnOnce(Reply<T>) -> Request) -> Result<T> {
        let id = lock(&self.calls).start().ok_or_else(not_answering)?;
        let (reply, answer) = std::sync::mpsc::channel();
        self.requests
            .send((id, request(reply)))
            .ok()
            .context("The Secret Service's thread stopped")?;
        match answer.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                lock(&self.calls).timed_out(id);
                Err(not_answering())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("The Secret Service's thread stopped")
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Vault for SecretService {
    fn get(&self, name: &str) -> Result<Option<String>> {
        self.ask(|reply| Request::Get(name.to_string(), reply))
    }
    fn put(&self, name: &str, value: &str) -> Result<()> {
        self.ask(|reply| Request::Put(name.to_string(), value.to_string(), reply))
    }
    fn delete(&self, name: &str) -> Result<()> {
        self.ask(|reply| Request::Delete(name.to_string(), reply))
    }
    fn names(&self) -> Result<Vec<String>> {
        self.ask(Request::Names)
    }
    fn describe(&self) -> &'static str {
        "the Secret Service (GNOME Keyring)"
    }
}

/// The session's Secret Service for `store`, where there is one.
#[cfg(target_os = "linux")]
fn secret_service(store: &str) -> Result<Box<dyn Vault>> {
    Ok(Box::new(SecretService::connect(store)?))
}
#[cfg(not(target_os = "linux"))]
fn secret_service(_store: &str) -> Result<Box<dyn Vault>> {
    bail!("The Secret Service is only used on Linux")
}

/// The credentials of the data directory `root`, in the store its marker
/// says, when the service starts. Nothing moves between stores here: that
/// is [`migrate`]. A hub keeps files.
pub fn open(root: &Path, hub: bool) -> Result<Secrets> {
    open_with(root, hub, secret_service)
}

/// [`open`], reaching the keyring's store with `connect`.
fn open_with(
    root: &Path,
    hub: bool,
    connect: impl Fn(&str) -> Result<Box<dyn Vault>>,
) -> Result<Secrets> {
    let mut secrets = Secrets::files(root);
    drop_partials(&secrets.dir)?;
    match read_marker(&secrets.dir)? {
        None => (),
        Some(Marker {
            migrating: Some(_), ..
        }) => bail!(
            "A credentials migration was cut short; run `workboard credentials migrate --to keyring` or `--to files` to finish it, then start the service"
        ),
        Some(Marker {
            rekeying: Some(_), ..
        }) => bail!(
            "A re-key of this data directory's credentials was cut short; run `workboard credentials rekey` again to finish it, then start the service"
        ),
        Some(_) if hub => bail!(
            "A hub keeps its credentials in files; run `workboard credentials migrate --to files` first"
        ),
        Some(marker) => {
            // Before the keyring is reached: a copy must not touch the
            // original's store at all.
            if let Some(updated) = here(root, &marker)? {
                save_marker(&secrets.dir, &updated)?;
            }
            secrets.store = match connect(&marker.store) {
                Ok(vault) => Store::Keyring(vault),
                Err(e) => {
                    // Fails closed: never falls back to files.
                    eprintln!("GNOME Keyring keeps the credentials and can't be reached, so connections fail until the service starts with it: {e:#}");
                    Store::Unavailable(format!("{e:#}"))
                }
            };
        }
    }
    // A verify cut short left a staged credential: the one in use stays.
    if !matches!(secrets.store, Store::Unavailable(_)) {
        secrets.drop_staged()?;
    }
    Ok(secrets)
}

/// What a migration did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Migrated {
    /// Credentials copied into the store migrated to.
    pub copied: usize,
    /// Copies dropped from the other store.
    pub dropped: usize,
}

/// Move the credentials of the data directory `root` into `to`, while the
/// service is stopped (the caller holds the data directory's lock).
///
/// It copies every credential into `to` and reads each copy back; then the
/// marker makes `to` the store; only then do the old copies go. The marker says
/// which store is the authority while it runs, so a migration cut short
/// anywhere is finished by running it again, to either store, and nothing is
/// lost. Every write, rename and removal is durable before the next step counts
/// on it, so this holds across a power cut too: the marker's switch is on disk
/// before the first old copy goes, and the marker stays until the old copies'
/// removal is. Staged credentials and partial files aren't moved: a verify cut
/// short never completed.
pub fn migrate(root: &Path, to: Backend) -> Result<Migrated> {
    migrate_with(root, to, secret_service)
}

/// [`migrate`], reaching the keyring's store with `connect`.
fn migrate_with(
    root: &Path,
    to: Backend,
    connect: impl Fn(&str) -> Result<Box<dyn Vault>>,
) -> Result<Migrated> {
    let dir = root.join(DIR);
    crate::db::private_dir(&dir)?;
    // Its creation is durable too.
    sync_dir(root)?;
    drop_partials(&dir)?;
    let marker = match read_marker(&dir)? {
        Some(Marker {
            rekeying: Some(_), ..
        }) => bail!(
            "A re-key of this data directory's credentials was cut short; run `workboard credentials rekey` again to finish it first"
        ),
        // A copy's migration would move or drop the original's credentials.
        Some(marker) => here(root, &marker)?.unwrap_or(marker),
        // Files, with no marker, have nothing in the keyring to move or drop.
        None if to == Backend::Files => {
            Secrets::files(root).drop_staged()?;
            return Ok(Migrated::default());
        }
        None => Marker {
            store: uuid::Uuid::new_v4().to_string(),
            migrating: Some(Backend::Files),
            path: Some(canonical(root)?),
            rekeying: None,
        },
    };
    let from = marker.migrating.unwrap_or(Backend::Keyring);
    // Reached before anything changes, so a keyring that can't be reached
    // leaves everything as it was.
    let vault = connect(&marker.store)?;
    let files = Secrets::files(root);
    let keyring = Secrets {
        dir: dir.clone(),
        store: Store::Keyring(vault),
    };
    let (source, target) = match (from, to) {
        (Backend::Files, Backend::Keyring) => (&files, &keyring),
        (Backend::Keyring, Backend::Files) => (&keyring, &files),
        // Already the store: what is left is dropping the other's copies.
        (Backend::Files, Backend::Files) => (&files, &files),
        (Backend::Keyring, Backend::Keyring) => (&keyring, &keyring),
    };
    // The authority until every copy was read back.
    save_marker(
        &dir,
        &Marker {
            migrating: Some(from),
            ..marker.clone()
        },
    )?;
    let mut migrated = Migrated::default();
    if from != to {
        // What only the store migrated to has, the authority doesn't: a
        // leftover of an earlier migration cut short.
        let wanted = source.names()?;
        for name in target.names()? {
            if !wanted.contains(&name) {
                target.delete(&name)?;
            }
        }
        for name in wanted {
            if name.ends_with(STAGED) {
                continue;
            }
            let value = source.read(&name)?;
            target.write(&name, &value)?;
            if target.read(&name)? != value {
                bail!("A credential wasn't kept as it was given; nothing was dropped, so run the migration again")
            }
            migrated.copied += 1;
        }
    }
    // The commit: `to` is the authority from here on.
    save_marker(
        &dir,
        &Marker {
            migrating: Some(to),
            ..marker.clone()
        },
    )?;
    let other = match to {
        Backend::Files => &keyring,
        Backend::Keyring => &files,
    };
    for name in other.names()? {
        other.delete(&name)?;
        migrated.dropped += 1;
    }
    let kept = match to {
        Backend::Files => &files,
        Backend::Keyring => &keyring,
    };
    kept.drop_staged()?;
    match to {
        Backend::Files => unlink(&dir.join(MARKER))?,
        Backend::Keyring => save_marker(
            &dir,
            &Marker {
                migrating: None,
                ..marker
            },
        )?,
    }
    Ok(migrated)
}

/// Give the data directory `root`, a copy of another that keeps its
/// credentials in the same keyring store, a store of its own, while the
/// service is stopped (the caller holds the data directory's lock). Returns
/// how many credentials were copied.
///
/// It records the new store in the marker first, copies every credential of
/// the shared store into it and reads each copy back, and only then makes it
/// this directory's store. Nothing in the shared store changes: it is the
/// original's. Cut short, running it again finishes it, with the same new
/// store, and the service refuses to start meanwhile. What the copy's
/// database doesn't use, its next start drops, now from its own store.
pub fn rekey(root: &Path) -> Result<usize> {
    rekey_with(root, secret_service)
}

/// [`rekey`], reaching the keyring's stores with `connect`.
fn rekey_with(root: &Path, connect: impl Fn(&str) -> Result<Box<dyn Vault>>) -> Result<usize> {
    let dir = root.join(DIR);
    let marker = match read_marker(&dir)? {
        None => bail!("This data directory keeps its credentials in files, not GNOME Keyring; there is no store to re-key"),
        Some(Marker {
            migrating: Some(_), ..
        }) => bail!(
            "A credentials migration was cut short; run `workboard credentials migrate --to keyring` or `--to files` to finish it first"
        ),
        Some(marker) => marker,
    };
    // Only a copy: re-keying the original would strand its credentials in a
    // store nothing names.
    let path = canonical(root)?;
    let is_copy = match marker.path.as_deref() {
        Some(recorded) if recorded != path => copy_of(root, recorded, &marker.store)?,
        _ => false,
    };
    if marker.rekeying.is_none() && !is_copy {
        bail!("This data directory isn't a copy of another that shares its GNOME Keyring store; it has nothing to re-key")
    }
    drop_partials(&dir)?;
    let new = marker
        .rekeying
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // Both reached before anything changes.
    let shared = Secrets {
        dir: dir.clone(),
        store: Store::Keyring(connect(&marker.store)?),
    };
    let own = Secrets {
        dir: dir.clone(),
        store: Store::Keyring(connect(&new)?),
    };
    save_marker(
        &dir,
        &Marker {
            rekeying: Some(new.clone()),
            ..marker.clone()
        },
    )?;
    // What only the new store has: a leftover of a re-key cut short.
    let wanted: Vec<String> = shared
        .names()?
        .into_iter()
        .filter(|n| !n.ends_with(STAGED))
        .collect();
    for name in own.names()? {
        if !wanted.contains(&name) {
            own.delete(&name)?;
        }
    }
    for name in &wanted {
        let value = shared.read(name)?;
        own.write(name, &value)?;
        if own.read(name)? != value {
            bail!("A credential wasn't kept as it was given; run the re-key again")
        }
    }
    // The switch: this directory's store from here on.
    save_marker(
        &dir,
        &Marker {
            store: new,
            migrating: None,
            path: Some(path),
            rekeying: None,
        },
    )?;
    Ok(wanted.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fault::Mode;
    use std::{
        collections::{BTreeMap, HashSet},
        sync::{Arc, Mutex},
    };

    /// A vault in memory, which can be told to fail or be locked. Every
    /// call is a step a test can stop or fail.
    #[derive(Clone, Default)]
    struct Memory {
        items: Arc<Mutex<BTreeMap<String, String>>>,
        broken: Arc<Mutex<bool>>,
        locked: Arc<Mutex<bool>>,
    }

    impl Memory {
        fn check(&self) -> Result<()> {
            step(Step::Vault)?;
            if *self.broken.lock().unwrap() {
                bail!("The vault failed")
            }
            if *self.locked.lock().unwrap() {
                return Err(locked());
            }
            Ok(())
        }
        fn snapshot(&self) -> BTreeMap<String, String> {
            self.items.lock().unwrap().clone()
        }
    }

    impl Vault for Memory {
        fn get(&self, name: &str) -> Result<Option<String>> {
            self.check()?;
            Ok(self.items.lock().unwrap().get(name).cloned())
        }
        fn put(&self, name: &str, value: &str) -> Result<()> {
            self.check()?;
            self.items.lock().unwrap().insert(name.into(), value.into());
            Ok(())
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.check()?;
            self.items.lock().unwrap().remove(name);
            Ok(())
        }
        fn names(&self) -> Result<Vec<String>> {
            self.check()?;
            Ok(self.items.lock().unwrap().keys().cloned().collect())
        }
        fn describe(&self) -> &'static str {
            "memory"
        }
    }

    fn reach(vault: &Memory) -> impl Fn(&str) -> Result<Box<dyn Vault>> + '_ {
        move |_| -> Result<Box<dyn Vault>> { Ok(Box::new(vault.clone())) }
    }

    fn unreachable(_: &str) -> Result<Box<dyn Vault>> {
        bail!("No Secret Service")
    }

    fn data_dir() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        crate::db::private_dir(&root.path().join(DIR)).unwrap();
        root
    }

    /// The names in the credentials' directory: files of any kind.
    fn files(root: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(root.join(DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    const A: &str = "00000000-0000-4000-8000-00000000000a";
    const B: &str = "00000000-0000-4000-8000-00000000000b";
    const C: &str = "00000000-0000-4000-8000-00000000000c";
    const CURRENT: &str = "current";
    const NEWER: &str = "newer";

    /// A data directory with A and C, kept by `backend`.
    fn kept_by(backend: Backend) -> (tempfile::TempDir, Memory) {
        let root = data_dir();
        let vault = Memory::default();
        let files = Secrets::files(root.path());
        files.create(A, CURRENT).unwrap();
        files.create(C, "c").unwrap();
        if backend == Backend::Keyring {
            migrate_with(root.path(), Backend::Keyring, reach(&vault)).unwrap();
        }
        (root, vault)
    }

    /// What a healthy start leaves, whatever came before: no partial file,
    /// no staged credential, and with the keyring, no credential in a
    /// file at all.
    fn check_clean(root: &Path, vault: &Memory, backend: Backend, case: &str) {
        let left = files(root);
        assert!(
            !left.iter().any(|f| f.ends_with(PARTIAL)),
            "{case}: {left:?}"
        );
        assert!(
            !left.iter().any(|f| f.ends_with(STAGED)),
            "{case}: {left:?}"
        );
        let names: Vec<_> = vault.snapshot().into_keys().collect();
        assert!(
            !names.iter().any(|n| n.ends_with(STAGED)),
            "{case}: {names:?}"
        );
        match backend {
            Backend::Keyring => assert_eq!(left, [MARKER], "{case}"),
            Backend::Files => {
                assert!(!left.contains(&MARKER.to_string()), "{case}: {left:?}");
                assert!(names.is_empty(), "{case}: {names:?}");
            }
        }
    }

    /// The same life of a credential in each store.
    #[test]
    fn create_verify_and_remove() {
        for backend in [Backend::Files, Backend::Keyring] {
            let (root, vault) = kept_by(backend);
            let secrets = open_with(root.path(), false, reach(&vault)).unwrap();
            secrets.create(B, "first\n").unwrap();
            assert_eq!(secrets.read(B).unwrap(), "first");
            secrets.stage(B, "second").unwrap();
            assert_eq!(secrets.read(B).unwrap(), "first");
            secrets.install(B).unwrap();
            assert_eq!(secrets.read(B).unwrap(), "second");
            secrets.stage(B, "third").unwrap();
            secrets.discard_staged(B).unwrap();
            assert_eq!(secrets.read(B).unwrap(), "second");
            secrets.stage(B, "fourth").unwrap();
            secrets.remove(B).unwrap();
            assert_eq!(
                secrets.read(B).unwrap_err().to_string(),
                "Missing credential"
            );
            assert_eq!(secrets.read(A).unwrap(), CURRENT);
            check_clean(root.path(), &vault, backend, &format!("{backend:?}"));
            for name in ["../escape", "", "abc"] {
                assert!(secrets.read(name).is_err(), "{name}");
                assert!(secrets.create(name, "x").is_err(), "{name}");
            }
        }
    }

    /// Every change within a store, stopped or failed at each of its steps
    /// in turn, then started again: the newest committed token wins, a
    /// removed credential never comes back, and a healthy start leaves no
    /// partial or staged copy, nor, with the keyring, any file.
    #[test]
    fn every_step_of_a_change_in_a_store_can_be_the_last() {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Change {
            /// A new connection's credential.
            Create,
            /// Verify Again: staged, then put in place, or discarded if
            /// that fails, as the scheduler does.
            Verify,
            /// Verify Again whose state couldn't be saved: staged, then
            /// discarded.
            Discard,
            /// Disconnect.
            Remove,
        }
        let change = |change: Change, secrets: &Secrets| -> bool {
            match change {
                Change::Create => secrets.create(B, "b").is_ok(),
                Change::Verify => {
                    if secrets.stage(A, NEWER).is_err() {
                        return false;
                    }
                    let installed = secrets.install(A);
                    if installed.is_err() {
                        let _ = secrets.discard_staged(A);
                    }
                    installed.is_ok()
                }
                Change::Discard => {
                    secrets.stage(A, NEWER).is_ok() && secrets.discard_staged(A).is_ok()
                }
                Change::Remove => secrets.remove(A).is_ok(),
            }
        };
        let mut combinations = 0;
        for change_now in [
            Change::Create,
            Change::Verify,
            Change::Discard,
            Change::Remove,
        ] {
            for backend in [Backend::Files, Backend::Keyring] {
                let (root, vault) = kept_by(backend);
                let secrets = open_with(root.path(), false, reach(&vault)).unwrap();
                let (_, steps) = fault::run(None, Mode::Stop, || change(change_now, &secrets));
                assert!(steps > 0, "{change_now:?} {backend:?}");
                for at in 0..=steps {
                    for mode in [Mode::Stop, Mode::Crash, Mode::Fail] {
                        combinations += 1;
                        let case = format!(
                            "{change_now:?} with {backend:?}, {mode:?} at step {at} of {steps}"
                        );
                        let (root, vault) = kept_by(backend);
                        let secrets = open_with(root.path(), false, reach(&vault)).unwrap();
                        let (ok, _) = fault::run(Some(at), mode, || change(change_now, &secrets));
                        drop(secrets);
                        // The token in place now, after the stop or the
                        // power cut: the newest one committed. A verify
                        // that said it succeeded put the new one there, for
                        // good; any other leaves either, never another.
                        let in_place = match backend {
                            Backend::Files => read_file(&root.path().join(DIR).join(A)).unwrap(),
                            Backend::Keyring => vault.snapshot().get(A).cloned(),
                        };
                        match change_now {
                            Change::Verify if ok => {
                                assert_eq!(in_place.as_deref(), Some(NEWER), "{case}")
                            }
                            Change::Verify => assert!(
                                matches!(in_place.as_deref(), Some(CURRENT | NEWER)),
                                "{case}: {in_place:?}"
                            ),
                            Change::Remove => (),
                            Change::Create | Change::Discard => {
                                assert_eq!(in_place.as_deref(), Some(CURRENT), "{case}")
                            }
                        }
                        let mut live: HashSet<String> = [C.to_string()].into();
                        if change_now != Change::Remove {
                            live.insert(A.to_string());
                        }
                        if change_now == Change::Create && ok {
                            live.insert(B.to_string());
                        }
                        // Started again, twice: the second sees what the
                        // first left.
                        for _ in 0..2 {
                            let again = open_with(root.path(), false, reach(&vault))
                                .unwrap_or_else(|e| panic!("{case}: {e:#}"));
                            again.reconcile(&live).unwrap();
                            check_clean(root.path(), &vault, backend, &case);
                            assert_eq!(again.read(C).unwrap(), "c", "{case}");
                            match change_now {
                                Change::Verify => assert_eq!(
                                    Some(again.read(A).unwrap()),
                                    in_place,
                                    "{case}: rolled back"
                                ),
                                Change::Remove => {
                                    assert!(again.read(A).is_err(), "{case}");
                                    let names = vault.snapshot();
                                    assert!(!names.keys().any(|n| n.starts_with(A)), "{case}");
                                    assert!(
                                        !files(root.path()).iter().any(|f| f.starts_with(A)),
                                        "{case}"
                                    );
                                }
                                _ => assert_eq!(again.read(A).unwrap(), CURRENT, "{case}"),
                            }
                            if change_now == Change::Create {
                                match ok {
                                    true => assert_eq!(again.read(B).unwrap(), "b", "{case}"),
                                    false => assert!(again.read(B).is_err(), "{case}"),
                                }
                            }
                        }
                    }
                }
            }
        }
        println!("{combinations} combinations");
    }

    /// The migration, both ways, stopped or failed at each of its steps in
    /// turn. Meanwhile the service either refuses to start or reads every
    /// credential as it was; running the migration again, to either store,
    /// loses nothing and leaves nothing in the other.
    #[test]
    fn every_step_of_a_migration_can_be_the_last() {
        let mut combinations = 0;
        for from in [Backend::Files, Backend::Keyring] {
            let to = match from {
                Backend::Files => Backend::Keyring,
                Backend::Keyring => Backend::Files,
            };
            let setup = || {
                let (root, vault) = kept_by(from);
                // A verify and a write cut short, before the migration.
                let secrets = open_with(root.path(), false, reach(&vault)).unwrap();
                secrets.stage(A, NEWER).unwrap();
                fs::write(root.path().join(DIR).join(format!("{C}{PARTIAL}")), "c").unwrap();
                (root, vault)
            };
            let (root, vault) = setup();
            let (_, steps) = fault::run(None, Mode::Stop, || {
                migrate_with(root.path(), to, reach(&vault))
            });
            assert!(steps > 0, "{from:?}");
            for at in 0..=steps {
                for mode in [Mode::Stop, Mode::Crash, Mode::Fail] {
                    for again_to in [to, from] {
                        combinations += 1;
                        let case = format!(
                            "{from:?} to {to:?}, {mode:?} at step {at} of {steps}, then to {again_to:?}"
                        );
                        let (root, vault) = setup();
                        let (migrated, _) = fault::run(Some(at), mode, || {
                            migrate_with(root.path(), to, reach(&vault))
                        });
                        // Whatever survived, the store the marker names as
                        // the authority holds every credential: never an
                        // old authority whose copies are already gone.
                        let dir = root.path().join(DIR);
                        let authority = match read_marker(&dir).unwrap() {
                            None => Backend::Files,
                            Some(m) => m.migrating.unwrap_or(Backend::Keyring),
                        };
                        for (id, value) in [(A, CURRENT), (C, "c")] {
                            let held = match authority {
                                Backend::Files => read_file(&dir.join(id)).unwrap(),
                                Backend::Keyring => vault.snapshot().get(id).cloned(),
                            };
                            assert_eq!(
                                held.as_deref(),
                                Some(value),
                                "{case}: {authority:?} lost {id}"
                            );
                        }
                        // Cut short, the service refuses to start, or reads
                        // what was there.
                        if let Ok(secrets) = open_with(root.path(), false, reach(&vault)) {
                            assert_eq!(secrets.read(A).unwrap(), CURRENT, "{case}");
                            assert_eq!(secrets.read(C).unwrap(), "c", "{case}");
                        } else {
                            assert!(migrated.is_err(), "{case}: refused after it completed");
                        }
                        migrate_with(root.path(), again_to, reach(&vault))
                            .unwrap_or_else(|e| panic!("{case}: {e:#}"));
                        let secrets = open_with(root.path(), false, reach(&vault))
                            .unwrap_or_else(|e| panic!("{case}: {e:#}"));
                        assert_eq!(secrets.read(A).unwrap(), CURRENT, "{case}");
                        assert_eq!(secrets.read(C).unwrap(), "c", "{case}");
                        check_clean(root.path(), &vault, again_to, &case);
                        let kept = match again_to {
                            Backend::Keyring => vault.snapshot().into_keys().collect::<Vec<_>>(),
                            Backend::Files => files(root.path()),
                        };
                        assert_eq!(kept, [A, C], "{case}");
                    }
                }
            }
        }
        println!("{combinations} combinations");
    }

    /// With the keyring as the store, one that can't be reached fails every
    /// credential closed, and nothing is written to a file instead.
    #[test]
    fn a_keyring_that_cant_be_reached_fails_closed() {
        let (root, _vault) = kept_by(Backend::Keyring);
        let secrets = open_with(root.path(), false, unreachable).unwrap();
        for result in [
            secrets.read(A).map(drop),
            secrets.create(B, "b"),
            secrets.stage(A, NEWER),
        ] {
            let error = result.unwrap_err();
            let connector = error.downcast_ref::<ConnectorError>().unwrap();
            // Not a rejected credential: syncs back off and resume.
            assert_eq!(connector.kind, ErrorKind::Transient);
            assert!(error.to_string().contains("GNOME Keyring"), "{error}");
        }
        assert!(secrets.reconcile(&HashSet::new()).is_err());
        assert_eq!(files(root.path()), [MARKER]);
    }

    /// A locked keyring fails a read, asking nothing, as a failure the
    /// connection backs off from and recovers from once it is unlocked, not
    /// as a rejected credential.
    #[test]
    fn a_locked_keyring_is_a_transient_error() {
        let (root, vault) = kept_by(Backend::Keyring);
        let secrets = open_with(root.path(), false, reach(&vault)).unwrap();
        *vault.locked.lock().unwrap() = true;
        let error = secrets.read(A).unwrap_err();
        let connector = error.downcast_ref::<ConnectorError>().unwrap();
        assert_eq!(connector.kind, ErrorKind::Transient);
        assert!(
            error.to_string().contains("GNOME Keyring is locked"),
            "{error}"
        );
        *vault.locked.lock().unwrap() = false;
        assert_eq!(secrets.read(A).unwrap(), CURRENT);
    }

    /// Credentials nothing uses go from the store in use, and only from it.
    #[test]
    fn unused_credentials_go() {
        for backend in [Backend::Files, Backend::Keyring] {
            let (root, vault) = kept_by(backend);
            let secrets = open_with(root.path(), false, reach(&vault)).unwrap();
            secrets.stage(A, NEWER).unwrap();
            let live: HashSet<String> = [A.to_string()].into();
            // C, nothing uses; A's staged copy is A's.
            assert_eq!(secrets.reconcile(&live).unwrap(), 1, "{backend:?}");
            assert_eq!(secrets.read(A).unwrap(), CURRENT);
            assert!(secrets.read(C).is_err());
            secrets.install(A).unwrap();
            assert_eq!(secrets.read(A).unwrap(), NEWER);
        }
    }

    /// The store is named in the marker, not by the directory's path, so
    /// the credentials are found after the directory moves.
    #[test]
    fn the_store_moves_with_the_directory() {
        let root = data_dir();
        Secrets::files(root.path()).create(A, "token-a").unwrap();
        let stores = Arc::new(Mutex::new(vec![]));
        let vault = Memory::default();
        let connect = |store: &str| -> Result<Box<dyn Vault>> {
            stores.lock().unwrap().push(store.to_string());
            Ok(Box::new(vault.clone()))
        };
        migrate_with(root.path(), Backend::Keyring, connect).unwrap();
        let moved = tempfile::tempdir().unwrap();
        let to = moved.path().join("elsewhere");
        fs::rename(root.path(), &to).unwrap();
        let again = open_with(&to, false, connect).unwrap();
        assert_eq!(again.read(A).unwrap(), "token-a");
        // The original is gone, so it was moved, not copied.
        assert_eq!(marker_of(&to).path, Some(canonical(&to).unwrap()));
        let stores = stores.lock().unwrap();
        assert_eq!(stores.len(), 2);
        assert_eq!(stores[0], stores[1]);
        assert!(!stores[0].contains('/'));
    }

    /// Keyring stores by ID, each a vault of its own, as the Secret Service
    /// keeps them apart by their attribute.
    #[derive(Clone, Default)]
    struct Stores(Arc<Mutex<BTreeMap<String, Memory>>>);

    impl Stores {
        fn get(&self, store: &str) -> Memory {
            self.0
                .lock()
                .unwrap()
                .entry(store.into())
                .or_default()
                .clone()
        }
        fn connect(&self) -> impl Fn(&str) -> Result<Box<dyn Vault>> + '_ {
            move |store| -> Result<Box<dyn Vault>> { Ok(Box::new(self.get(store))) }
        }
        /// The stores holding anything, but `except`.
        fn others(&self, except: &[&str]) -> Vec<String> {
            let stores = self.0.lock().unwrap();
            stores
                .iter()
                .filter(|(id, vault)| {
                    !except.contains(&id.as_str()) && !vault.snapshot().is_empty()
                })
                .map(|(id, _)| id.clone())
                .collect()
        }
    }

    /// A data directory with A and C, kept in a keyring store of its own,
    /// and that store's ID.
    fn kept_in(stores: &Stores) -> (tempfile::TempDir, String) {
        let root = data_dir();
        let files = Secrets::files(root.path());
        files.create(A, CURRENT).unwrap();
        files.create(C, "c").unwrap();
        migrate_with(root.path(), Backend::Keyring, stores.connect()).unwrap();
        let store = store_of(root.path());
        (root, store)
    }

    fn marker_of(root: &Path) -> Marker {
        read_marker(&root.join(DIR)).unwrap().unwrap()
    }

    fn store_of(root: &Path) -> String {
        marker_of(root).store
    }

    /// A copy of the data directory `root`, as `cp -a` makes one.
    fn copy_of_dir(root: &Path) -> tempfile::TempDir {
        let copy = tempfile::tempdir().unwrap();
        let dir = copy.path().join(DIR);
        crate::db::private_dir(&dir).unwrap();
        for entry in fs::read_dir(root.join(DIR)).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
        }
        copy
    }

    /// A copy of a data directory whose original still uses the keyring
    /// store is refused, by the service and by a migration, either of which
    /// would drop the original's credentials; re-keyed, it has a store of
    /// its own, and the original's is as it was.
    #[test]
    fn a_copy_is_refused_until_it_has_a_store_of_its_own() {
        let stores = Stores::default();
        let (root, shared) = kept_in(&stores);
        assert_eq!(
            marker_of(root.path()).path,
            Some(canonical(root.path()).unwrap())
        );
        let copy = copy_of_dir(root.path());
        // The original goes on, and adds B.
        open_with(root.path(), false, stores.connect())
            .unwrap()
            .create(B, "b")
            .unwrap();
        let refusals = [
            open_with(copy.path(), false, stores.connect()).map(drop),
            migrate_with(copy.path(), Backend::Files, stores.connect()).map(drop),
            migrate_with(copy.path(), Backend::Keyring, stores.connect()).map(drop),
        ];
        for refused in refusals {
            let error = format!("{:#}", refused.unwrap_err());
            assert!(error.contains("is a copy of"), "{error}");
            assert!(error.contains("workboard credentials rekey"), "{error}");
        }
        let original = BTreeMap::from([
            (A.to_string(), CURRENT.to_string()),
            (B.to_string(), "b".to_string()),
            (C.to_string(), "c".to_string()),
        ]);
        assert_eq!(stores.get(&shared).snapshot(), original);
        // The original can't be re-keyed, nor files.
        assert!(rekey_with(root.path(), stores.connect()).is_err());
        let (files_dir, _) = kept_by(Backend::Files);
        assert!(rekey_with(files_dir.path(), stores.connect()).is_err());

        assert_eq!(rekey_with(copy.path(), stores.connect()).unwrap(), 3);
        let own = store_of(copy.path());
        assert_ne!(own, shared);
        assert_eq!(
            marker_of(copy.path()).path,
            Some(canonical(copy.path()).unwrap())
        );
        let secrets = open_with(copy.path(), false, stores.connect()).unwrap();
        // Its start drops what its database doesn't use, B, from its own
        // store alone; so does a removal.
        let live: HashSet<String> = [A.to_string(), C.to_string()].into();
        assert_eq!(secrets.reconcile(&live).unwrap(), 1);
        secrets.remove(A).unwrap();
        assert_eq!(secrets.read(C).unwrap(), "c");
        assert_eq!(
            stores.get(&own).snapshot().into_keys().collect::<Vec<_>>(),
            [C]
        );
        assert_eq!(stores.get(&shared).snapshot(), original);
        // Once it is, there is nothing more to re-key.
        assert!(rekey_with(copy.path(), stores.connect()).is_err());
        let again = open_with(root.path(), false, stores.connect()).unwrap();
        assert_eq!(again.read(A).unwrap(), CURRENT);
    }

    /// A re-key stopped or failed at each of its steps in turn: the
    /// original's store never changes, the copy is refused or reads every
    /// credential from its own store, and running it again finishes it,
    /// leaving nothing in a store no directory names.
    #[test]
    fn every_step_of_a_rekey_can_be_the_last() {
        let setup = || {
            let stores = Stores::default();
            let (root, shared) = kept_in(&stores);
            let copy = copy_of_dir(root.path());
            (stores, root, copy, shared)
        };
        let (stores, _root, copy, _) = setup();
        let (_, steps) = fault::run(None, Mode::Stop, || {
            rekey_with(copy.path(), stores.connect())
        });
        assert!(steps > 0);
        let mut combinations = 0;
        for at in 0..=steps {
            for mode in [Mode::Stop, Mode::Crash, Mode::Fail] {
                combinations += 1;
                let case = format!("{mode:?} at step {at} of {steps}");
                let (stores, root, copy, shared) = setup();
                let original = stores.get(&shared).snapshot();
                let (rekeyed, _) =
                    fault::run(Some(at), mode, || rekey_with(copy.path(), stores.connect()));
                assert_eq!(stores.get(&shared).snapshot(), original, "{case}");
                match open_with(copy.path(), false, stores.connect()) {
                    Ok(secrets) => {
                        assert_ne!(store_of(copy.path()), shared, "{case}");
                        assert_eq!(secrets.read(A).unwrap(), CURRENT, "{case}");
                        assert_eq!(secrets.read(C).unwrap(), "c", "{case}");
                    }
                    Err(_) => {
                        assert!(rekeyed.is_err(), "{case}: refused after it completed");
                        rekey_with(copy.path(), stores.connect())
                            .unwrap_or_else(|e| panic!("{case}: {e:#}"));
                    }
                }
                let own = store_of(copy.path());
                let secrets = open_with(copy.path(), false, stores.connect())
                    .unwrap_or_else(|e| panic!("{case}: {e:#}"));
                assert_eq!(secrets.read(A).unwrap(), CURRENT, "{case}");
                assert_eq!(secrets.read(C).unwrap(), "c", "{case}");
                assert_eq!(stores.get(&own).snapshot(), original, "{case}");
                assert_eq!(stores.get(&shared).snapshot(), original, "{case}");
                assert_eq!(
                    stores.others(&[&shared, &own]),
                    Vec::<String>::new(),
                    "{case}"
                );
                assert_eq!(files(copy.path()), [MARKER], "{case}");
                let reopened = open_with(root.path(), false, stores.connect()).unwrap();
                assert_eq!(reopened.read(A).unwrap(), CURRENT, "{case}");
            }
        }
        println!("{combinations} combinations");
    }

    /// The same directory by another path, such as a bind mount's, is no
    /// copy; nor is one whose marker predates recording the path, which
    /// records it.
    #[cfg(unix)]
    #[test]
    fn the_same_directory_by_another_path_is_no_copy() {
        let stores = Stores::default();
        let (root, store) = kept_in(&stores);
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(root.path(), &link).unwrap();
        let here = canonical(root.path()).unwrap();
        for recorded in [Some(link), None] {
            let marker = Marker {
                path: recorded.clone(),
                ..marker_of(root.path())
            };
            save_marker(&root.path().join(DIR), &marker).unwrap();
            let secrets = open_with(root.path(), false, stores.connect()).unwrap();
            assert_eq!(secrets.read(A).unwrap(), CURRENT, "{recorded:?}");
            assert_eq!(marker_of(root.path()).path.as_ref(), Some(&here));
            assert_eq!(store_of(root.path()), store);
        }
    }

    #[test]
    fn a_hub_keeps_files() {
        let (root, vault) = kept_by(Backend::Keyring);
        let error = open_with(root.path(), true, reach(&vault))
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("A hub keeps its credentials in files"),
            "{error}"
        );
        let (root, vault) = kept_by(Backend::Files);
        let secrets = open_with(root.path(), true, reach(&vault)).unwrap();
        assert_eq!(secrets.describe(), "private files");
    }

    /// Partial files a write cut short left go at start, before anything
    /// else: one can hold a token in the clear.
    #[test]
    fn partial_files_go_at_start() {
        let (root, vault) = kept_by(Backend::Files);
        let dir = root.path().join(DIR);
        fs::write(dir.join(format!("{A}{PARTIAL}")), "token").unwrap();
        fs::write(dir.join(format!("{MARKER}{PARTIAL}")), "{").unwrap();
        let opened = open_with(root.path(), false, reach(&vault)).unwrap();
        assert_eq!(files(root.path()), [A, C]);
        assert_eq!(opened.read(A).unwrap(), CURRENT);
    }

    /// A Secret Service that doesn't answer in time fails that call, and
    /// the next ones fast, until the late call finishes; then calls go
    /// through again.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_secret_service_that_doesnt_answer_fails_fast_then_recovers() {
        let (requests, receive) = std::sync::mpsc::channel::<(u64, Request)>();
        let calls = Arc::new(Mutex::new(Calls::default()));
        let service = SecretService {
            requests,
            calls: calls.clone(),
            timeout: std::time::Duration::from_millis(100),
        };
        // A thread that answers the first call late, and the others at once.
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            for (id, request) in receive {
                if id == 0 {
                    gate.recv().unwrap();
                }
                if let Request::Get(_, reply) = request {
                    let _ = reply.send(Ok(Some("token".into())));
                }
                lock(&calls).finished(id);
            }
        });
        let error = service.get(A).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ConnectorError>().unwrap().kind,
            ErrorKind::Transient
        );
        // The next call isn't even sent while the first is stuck.
        let started = std::time::Instant::now();
        assert!(service.get(A).is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        // Once it finishes, calls go through.
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match service.get(A) {
                Ok(value) => {
                    assert_eq!(value.as_deref(), Some("token"));
                    break;
                }
                Err(_) => assert!(std::time::Instant::now() < deadline, "never recovered"),
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(service);
        worker.join().unwrap();
    }

    /// A call that finishes just as its caller gives up isn't taken for a
    /// stuck one: nothing would ever clear that.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_call_finished_as_it_timed_out_is_not_stuck() {
        let mut calls = Calls::default();
        let first = calls.start().unwrap();
        // The thread finishes it before the caller notes its timeout.
        calls.finished(first);
        calls.timed_out(first);
        assert_eq!(calls.stuck, None);
        let second = calls.start().unwrap();
        calls.timed_out(second);
        assert_eq!(calls.stuck, Some(second));
        assert_eq!(calls.start(), None);
        calls.finished(second);
        assert!(calls.start().is_some());
    }

    /// Against the session's real Secret Service, such as GNOME Keyring in
    /// a test session (see the `keyring` CI job).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a Secret Service on the session bus"]
    fn secret_service_round_trip() {
        let root = data_dir();
        Secrets::files(root.path()).create(A, "file-token").unwrap();
        let moved = migrate(root.path(), Backend::Keyring).unwrap();
        assert_eq!(moved.copied, 1);
        assert_eq!(files(root.path()), [MARKER]);
        let kept = open(root.path(), false).unwrap();
        assert_eq!(kept.describe(), "the Secret Service (GNOME Keyring)");
        assert_eq!(kept.read(A).unwrap(), "file-token");
        kept.stage(A, "new-token").unwrap();
        kept.install(A).unwrap();
        assert_eq!(kept.read(A).unwrap(), "new-token");
        drop(kept);

        // Another store's credentials are its own.
        let theirs = SecretService::connect("another-store").unwrap();
        assert_eq!(theirs.get(A).unwrap(), None);
        assert!(theirs.names().unwrap().is_empty());

        // Back to files.
        migrate(root.path(), Backend::Files).unwrap();
        let back = open(root.path(), false).unwrap();
        assert_eq!(back.read(A).unwrap(), "new-token");
        assert_eq!(files(root.path()), [A]);
    }
}
