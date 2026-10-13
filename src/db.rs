use crate::{
    bot::AgentRuns,
    hub,
    model::*,
    outcome,
    secrets::{self, Secrets},
    zone::ExternalSnapshot,
};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection as Sqlite, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn write_secret(path: &Path, value: &str) -> Result<()> {
    use std::io::Write;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    opts.open(path)?.write_all(value.as_bytes())?;
    Ok(())
}

/// Lock the data directory `root` for this process, as long as the file
/// returned is open: one service, or credentials migration, at a time.
pub fn lock(root: &Path) -> Result<fs::File> {
    let path = root.join("workboard.lock");
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(&path)
        .with_context(|| format!("Cannot open {}", path.display()))?;
    #[cfg(unix)]
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => (),
        // Only this means someone holds it; anything else is said as it is.
        Err(rustix::io::Errno::WOULDBLOCK) => bail!(
            "Another Workboard service, or a credentials migration, is using {}; stop it first",
            root.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("Cannot lock {}", path.display())),
    }
    Ok(file)
}

/// The database's file in a data directory.
const DATABASE: &str = "work.sqlite";
/// What starts and ends the name of a copy of the database made before an
/// upgrade, `work.vN.sqlite`.
const BACKUP_PREFIX: &str = "work.v";
const BACKUP_SUFFIX: &str = ".sqlite";
/// What ends the name of a copy of the database while it is written.
const PARTIAL: &str = ".partial";
/// The files SQLite keeps beside a database, by the end of their names: its
/// rollback journal, write-ahead log and shared memory.
const SQLITE_COMPANIONS: [&str; 3] = ["-journal", "-wal", "-shm"];

/// Whether a file named `name` in a data directory is one of SQLite's: the
/// database, a copy of it from before an upgrade, or a file SQLite keeps
/// beside either.
fn is_sqlite_file(name: &str) -> bool {
    let base = SQLITE_COMPANIONS
        .iter()
        .find_map(|end| name.strip_suffix(end))
        .unwrap_or(name);
    base == DATABASE || (base.starts_with(BACKUP_PREFIX) && base.ends_with(BACKUP_SUFFIX))
}

/// Create the file at `path` readable and writable by this user only, or
/// open it if it is there and `new` isn't asked for.
fn create_private(path: &Path, new: bool) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .with_context(|| format!("Cannot create {}", path.display()))
}

/// Open the SQLite file at `path` without following a symbolic link,
/// creating it private if `create` says to, and fail unless it is a
/// regular file of this user's; then make it private through the
/// descriptor opened, so the file checked is the file changed. One that
/// wasn't private is named on standard error. A file that isn't there,
/// and isn't to be created, is fine.
#[cfg(unix)]
fn private_sqlite_file(path: &Path, create: bool) -> Result<()> {
    use rustix::fs::{fchmod, fstat, open, FileType, Mode, OFlags};
    let shown = path.display();
    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if create {
        flags |= OFlags::CREATE;
    }
    let fd = match open(path, flags, Mode::from_raw_mode(0o600)) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) if !create => return Ok(()),
        Err(rustix::io::Errno::ISDIR) => {
            bail!("{shown} isn't a regular file; Workboard keeps its SQLite files as plain files")
        }
        Err(rustix::io::Errno::LOOP) => bail!(
            "{shown} is a symbolic link, so it could put the database where others can read it. Workboard keeps its SQLite files in the data directory itself: put the file in place of the link"
        ),
        Err(e) => return Err(e).with_context(|| format!("Cannot open {shown}")),
    };
    let stat = fstat(&fd).with_context(|| format!("Cannot read {shown}"))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        bail!("{shown} isn't a regular file; Workboard keeps its SQLite files as plain files")
    }
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        bail!("{shown} isn't yours; Workboard keeps its SQLite files only in files you own")
    }
    let mode = stat.st_mode & 0o777;
    if mode != 0o600 {
        fchmod(&fd, Mode::from_raw_mode(0o600))
            .with_context(|| format!("Cannot make {shown} private"))?;
        eprintln!("Made {shown} readable by you only; it was mode {mode:o}");
    }
    Ok(())
}

/// Create the database in the data directory `root` private, if it isn't
/// there yet, before SQLite opens it: SQLite gives the write-ahead log and
/// shared memory it creates beside a database that database's mode, so
/// they are private too. Every SQLite file there has to be a regular file
/// of this user's, not a symbolic link, which could put it anywhere; one an
/// earlier version left readable by others, as it did the write-ahead log
/// and shared memory, is made private, and named on standard error.
fn private_sqlite_files(root: &Path) -> Result<()> {
    #[cfg(not(unix))]
    create_private(&root.join(DATABASE), false)?;
    #[cfg(unix)]
    {
        private_sqlite_file(&root.join(DATABASE), true)?;
        let entries =
            fs::read_dir(root).with_context(|| format!("Cannot read {}", root.display()))?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let sqlite = name.to_str().is_some_and(is_sqlite_file);
            if sqlite && name != DATABASE {
                private_sqlite_file(&entry.path(), false)?;
            }
        }
    }
    Ok(())
}

/// Remove the file at `path`, if there is one.
pub fn remove_if_there(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("Cannot remove {}", path.display())),
    }
}

/// The schema version this build writes, kept in SQLite's `user_version`.
/// Version 0 is the prototype's schema, whose items only had a `terminal` flag.
const SCHEMA_VERSION: i32 = 8;

fn schema_version(sql: &Sqlite) -> Result<i32> {
    let version = sql.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        bail!("This data directory was written by a newer Workboard (schema {version}, this build supports {SCHEMA_VERSION}); use that version")
    }
    Ok(version)
}

/// Bring an existing database up to [`SCHEMA_VERSION`] in one transaction, so
/// a failed migration leaves the data as it was. Migrations are one-way, so a
/// database with data is first copied to `work.vN.sqlite`, N being its old
/// version.
fn migrate(sql: &mut Sqlite, root: &Path) -> Result<()> {
    let version = schema_version(sql)?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    backup(sql, &root.join(format!("work.v{version}.sqlite")))?;
    // Immediate: take the write lock before reading the version, so a second
    // service on the same directory cannot migrate the same rows again.
    let tx = sql.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version = schema_version(&tx)?;
    if version < 1 {
        migrate_outcomes(&tx).context("Cannot upgrade stored items to outcomes")?;
    }
    if version < 2 {
        migrate_zones(&tx).context("Cannot add zones to stored connections")?;
    }
    if version < 3 {
        // Version 3: whether an effort's title may leave this machine. No
        // title was shared before.
        tx.execute_batch("ALTER TABLE efforts ADD COLUMN share_title INTEGER NOT NULL DEFAULT 0")
            .context("Cannot add title sharing to stored efforts")?;
    }
    if version < 4 {
        // Version 4: what each connection found watching another user, kept
        // apart from the account's own items.
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS watched(connection_id TEXT PRIMARY KEY, login TEXT NOT NULL, body TEXT NOT NULL, error TEXT)",
        )
        .context("Cannot add watched users")?;
    }
    if version < 5 {
        migrate_sync_state(&tx).context("Cannot move sync state out of stored connections")?;
    }
    if version < 6 {
        // Version 6: settings of the data directory, such as its mode,
        // recorded when a service first opens it.
        tx.execute_batch(SETTINGS_TABLE)
            .context("Cannot add the data directory's settings")?;
    }
    if version < 7 {
        // Version 7: the hub a companion pulls from, and the last export it
        // read. Its sync state is kept with the connections'.
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS hub(id TEXT PRIMARY KEY, url TEXT NOT NULL, snapshot TEXT)",
        )
        .context("Cannot add the hub")?;
    }
    if version < 8 {
        // Version 8: what each bot connection last read of its heartbeat,
        // its agent runs, kept apart from items.
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS agents(connection_id TEXT PRIMARY KEY, body TEXT NOT NULL, error TEXT, last_success INTEGER, board TEXT)",
        )
        .context("Cannot add agent runs")?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

/// Copy a database that has data to `path`. A copy that already exists is
/// kept: it is the original from an earlier, failed attempt.
fn backup(sql: &Sqlite, path: &Path) -> Result<()> {
    let has_data: bool = sql.query_row(
        "SELECT EXISTS(SELECT 1 FROM items) OR EXISTS(SELECT 1 FROM efforts) OR EXISTS(SELECT 1 FROM connections)",
        [],
        |r| r.get(0),
    )?;
    if !has_data || path.exists() {
        return Ok(());
    }
    // Written under another name and put in place once it is complete and
    // on disk, so a crash never leaves a copy that isn't whole where one
    // would pass for the original next time.
    let partial = PathBuf::from(format!("{}{PARTIAL}", path.display()));
    let target = partial
        .to_str()
        .context("The data directory path is not UTF-8")?;
    remove_if_there(&partial)?;
    // VACUUM INTO fills an empty file it is given, so the copy is private
    // from the start, and so is the journal SQLite keeps beside it.
    create_private(&partial, true)?;
    // Unlike a file copy, VACUUM INTO includes changes still in the WAL.
    let copied = sql
        .execute("VACUUM INTO ?1", [target])
        .map_err(anyhow::Error::from)
        .and_then(|_| Ok(fs::File::open(&partial)?.sync_all()?));
    if let Err(e) = copied {
        let _ = fs::remove_file(&partial);
        return Err(e).with_context(|| {
            format!(
                "Cannot back up the database to {} before upgrading it",
                path.display()
            )
        });
    }
    fs::rename(&partial, path)
        .with_context(|| format!("Cannot put the database's copy in {}", path.display()))?;
    if let Some(dir) = path.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Record that the database is opened as `mode`, or fail if it was first
/// opened as the other. One that recorded no mode yet, from before modes
/// existed, may hold internal work, so it becomes a hub only while empty.
fn claim(sql: &mut Sqlite, root: &Path, mode: Mode) -> Result<()> {
    let tx = sql.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let recorded: Option<String> = tx
        .query_row("SELECT value FROM settings WHERE key=?1", [MODE], |r| {
            r.get(0)
        })
        .optional()?;
    let recorded = recorded.as_deref().map(Mode::parse).transpose()?;
    let rows: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM items) OR EXISTS(SELECT 1 FROM efforts) OR EXISTS(SELECT 1 FROM connections) OR EXISTS(SELECT 1 FROM watched) OR EXISTS(SELECT 1 FROM agents) OR EXISTS(SELECT 1 FROM hub)",
        [],
        |r| r.get(0),
    )?;
    // A credential left behind is data too: a connection that was removed
    // from the database, but not its token.
    let secrets = root.join(SECRETS);
    let credentials = fs::read_dir(&secrets)
        .with_context(|| format!("Cannot read {}", secrets.display()))?
        .next()
        .is_some();
    let has_data = rows || credentials;
    match (recorded, mode) {
        (Some(recorded), mode) if recorded == mode => return Ok(()),
        (Some(Mode::Hub), _) => bail!(
            "This data directory is a hub's. Start it with `workboard serve --hub`; a hub never holds internal work, and starting it otherwise could add some"
        ),
        (None, Mode::Hub) if !has_data => (),
        (_, Mode::Hub) => bail!(
            "This data directory isn't a hub's, and may hold internal work. Start the hub with a new --data-dir"
        ),
        (_, Mode::Local) => (),
    }
    tx.execute(
        "INSERT OR REPLACE INTO settings(key,value) VALUES(?1,?2)",
        [MODE, mode.name()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Version 1: an explicit outcome and its reason replace the `terminal` flag,
/// derived from what each row stored. A person's abandoned decision gets its
/// own column, so syncs keep it like they keep effort links.
fn migrate_outcomes(tx: &Transaction) -> Result<()> {
    tx.execute_batch("ALTER TABLE items ADD COLUMN abandoned_at TEXT")?;
    let rows: Vec<(String, String)> = tx
        .prepare("SELECT id,body FROM items")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, body) in rows {
        let mut v: Value =
            serde_json::from_str(&body).with_context(|| format!("Stored item {id} is not JSON"))?;
        let item = v
            .as_object_mut()
            .with_context(|| format!("Stored item {id} is not an object"))?;
        let text = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (provider, status) = (text("provider"), text("status"));
        let synced = item.get("connection_id").is_some_and(|c| !c.is_null());
        let terminal = item
            .remove("terminal")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        let (outcome, reason) = outcome::legacy(&provider, synced, &status, terminal);
        item.insert("outcome".into(), serde_json::to_value(outcome)?);
        item.insert("outcome_reason".into(), reason.into());
        if outcome != Outcome::Open {
            item.insert("action".into(), "".into());
        }
        tx.execute(
            "UPDATE items SET body=?1 WHERE id=?2",
            params![serde_json::to_string(&v)?, id],
        )?;
    }
    Ok(())
}

/// Version 2: every connection has a zone. Connections made before zones
/// existed may hold internal work, so they become internal; external is
/// only ever a person's explicit choice.
fn migrate_zones(tx: &Transaction) -> Result<()> {
    let rows: Vec<(String, String)> = tx
        .prepare("SELECT id,body FROM connections")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, body) in rows {
        let mut v: Value = serde_json::from_str(&body)
            .with_context(|| format!("Stored connection {id} is not JSON"))?;
        v.as_object_mut()
            .with_context(|| format!("Stored connection {id} is not an object"))?
            .insert("zone".into(), serde_json::to_value(Zone::Internal)?);
        tx.execute(
            "UPDATE connections SET body=?1 WHERE id=?2",
            params![serde_json::to_string(&v)?, id],
        )?;
    }
    Ok(())
}

/// Version 5: each connection's sync state gets a table of its own, written
/// in the same transaction as what a sync found, instead of fields of the
/// connection saved after it. A connection whose last sync failed keeps
/// that error, and one rejected credential still needs the person.
fn migrate_sync_state(tx: &Transaction) -> Result<()> {
    tx.execute_batch(SYNC_STATE_TABLE)?;
    // When each watch list was found, since a watch can fail while its
    // account syncs. Lists found before this are of unknown age.
    let has_column: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('watched') WHERE name='last_success')",
        [],
        |r| r.get(0),
    )?;
    if !has_column {
        tx.execute_batch("ALTER TABLE watched ADD COLUMN last_success INTEGER")?;
    }
    let rows: Vec<(String, String)> = tx
        .prepare("SELECT id,body FROM connections")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, body) in rows {
        let mut v: Value = serde_json::from_str(&body)
            .with_context(|| format!("Stored connection {id} is not JSON"))?;
        let c = v
            .as_object_mut()
            .with_context(|| format!("Stored connection {id} is not an object"))?;
        let mut take = |key: &str| c.remove(key).filter(|v| !v.is_null());
        let last_sync = take("last_sync");
        let error = take("error").and_then(|e| e.as_str().map(str::to_string));
        let kind = take("error_kind").and_then(|k| serde_json::from_value(k).ok());
        let last_success = last_sync.and_then(|t| t.as_str().and_then(Timestamp::parse));
        let state = SyncState {
            last_success,
            last_attempt: last_success,
            next_due: None,
            failures: u32::from(error.is_some()),
            error_kind: error.as_ref().map(|_| kind.unwrap_or(ErrorKind::Invalid)),
            error,
            held_until: None,
        };
        tx.execute(
            "UPDATE connections SET body=?1 WHERE id=?2",
            params![serde_json::to_string(&v)?, id],
        )?;
        write_sync_state(tx, &id, &state)?;
    }
    Ok(())
}

/// Where a data directory keeps credentials, one file each.
const SECRETS: &str = secrets::DIR;

/// Settings of the data directory, by name.
const SETTINGS_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL)";
/// The setting that records the data directory's [`Mode`].
const MODE: &str = "mode";

/// What a data directory is for, recorded the first time a service opens
/// it, so it can't be started as the other later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A person's own service, internal and external work alike, on the
    /// machine that syncs it. With a hub set, it is the companion.
    Local,
    /// A hub: external work only. Every write is checked, and anything not
    /// shown to be external is refused.
    Hub,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Hub => "hub",
        }
    }
    fn parse(name: &str) -> Result<Self> {
        [Self::Local, Self::Hub]
            .into_iter()
            .find(|m| m.name() == name)
            .ok_or_else(|| anyhow!("This data directory has an unknown mode, '{name}'; use the Workboard that made it"))
    }
}

/// A write into the database, as a hub's check sees it. Each write method
/// says which it is before it writes anything; see [`Db::admit`].
enum Write<'a> {
    /// A connection's settings.
    Connection(&'a Connection),
    /// What a sync of connection `id` found.
    Sync { id: &'a str, found: &'a Found<'a> },
    /// A connection's sync state alone.
    SyncState,
    /// Whom connection `id` watches.
    Watch(&'a str),
    /// A manual source link, new or changed.
    Manual,
    /// The demo's illustrative work.
    Demo,
    /// An effort, linking these items.
    Effort(&'a [String]),
    /// Linking an item to an effort, or ungrouping it.
    Link(&'a str),
    /// A person's decision about work already stored: abandoning a synced
    /// item, or sharing an effort's title.
    Decision,
    /// Removing a connection and everything it brought.
    Disconnect,
    /// Pulling from a hub: its address, or what it sent.
    Hub,
}

/// What a hub refuses, and why.
fn refused(what: &str) -> anyhow::Error {
    anyhow!("This is a hub, which keeps external work only, so it refuses {what}")
}

/// Each connection's [`SyncState`], one row each.
const SYNC_STATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS sync_state(
  connection_id TEXT PRIMARY KEY,
  last_success INTEGER,
  last_attempt INTEGER,
  next_due INTEGER,
  failures INTEGER NOT NULL DEFAULT 0,
  error_kind TEXT,
  error TEXT,
  held_until INTEGER)";

/// The columns of [`SYNC_STATE_TABLE`] that hold the state, in order.
const SYNC_STATE_COLUMNS: &str =
    "last_success,last_attempt,next_due,failures,error_kind,error,held_until";

fn write_sync_state(tx: &Transaction, id: &str, state: &SyncState) -> Result<()> {
    let kind = state
        .error_kind
        .map(|k| serde_json::to_value(k).map(|v| v.as_str().unwrap_or_default().to_string()))
        .transpose()?;
    tx.execute(
        &format!("INSERT OR REPLACE INTO sync_state(connection_id,{SYNC_STATE_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)"),
        params![
            id,
            state.last_success.map(Timestamp::unix),
            state.last_attempt.map(Timestamp::unix),
            state.next_due.map(Timestamp::unix),
            state.failures,
            kind,
            state.error,
            state.held_until.map(Timestamp::unix),
        ],
    )?;
    Ok(())
}

/// A [`SyncState`] from its columns, starting at `first`.
fn read_sync_state(r: &rusqlite::Row, first: usize) -> rusqlite::Result<SyncState> {
    let time = |n: usize| -> rusqlite::Result<Option<Timestamp>> {
        Ok(r.get::<_, Option<i64>>(first + n)?
            .map(Timestamp::from_unix))
    };
    let kind: Option<String> = r.get(first + 4)?;
    Ok(SyncState {
        last_success: time(0)?,
        last_attempt: time(1)?,
        next_due: time(2)?,
        failures: r.get::<_, Option<u32>>(first + 3)?.unwrap_or_default(),
        // A kind this build doesn't know is shown as what it can be sure of.
        error_kind: kind
            .map(|k| serde_json::from_value(Value::String(k)).unwrap_or(ErrorKind::Invalid)),
        error: r.get(first + 5)?,
        held_until: time(6)?,
    })
}

/// A connection as it is stored: without its sync state, which has a table
/// of its own, or its schedule, which is worked out when it is shown.
fn connection_body(c: &Connection) -> Result<String> {
    let mut v = serde_json::to_value(c)?;
    if let Some(body) = v.as_object_mut() {
        body.remove("sync");
        body.remove("schedule");
        body.remove("from_hub");
    }
    Ok(serde_json::to_string(&v)?)
}

/// What a sync of a connection found, to save together.
#[derive(Clone, Copy)]
pub struct Found<'a> {
    pub items: &'a [Item],
    pub watched: Option<&'a WatchList>,
    pub agents: Option<&'a AgentRuns>,
    /// Local IDs of items its connector no longer tracks, to remove.
    pub dropped: &'a [String],
}

/// Whether local item ID `item` is one of connection `id`'s, which are
/// namespaced by it.
fn of_connection(item: &str, id: &str) -> bool {
    item.strip_prefix(id)
        .is_some_and(|rest| rest.starts_with(':'))
}

/// Fail unless connection `id` is still there, so a sync that finishes
/// after a disconnect saves nothing.
fn check_connected(tx: &Transaction, id: &str) -> Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM connections WHERE id=?1)",
        [id],
        |r| r.get(0),
    )?;
    if !exists {
        bail!("Connection not found; it was disconnected")
    }
    Ok(())
}

/// Insert or update synced items, keeping their effort links and abandoned
/// marks, which live in their own columns.
fn upsert_synced(tx: &Transaction, items: &[Item]) -> Result<()> {
    for i in items {
        tx.execute("INSERT INTO items(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![i.id,serde_json::to_string(i)?])?;
    }
    Ok(())
}

/// The data directory's database. Its SQL connection is private, so every
/// write goes through a method here.
pub struct Db {
    sql: Sqlite,
    pub root: PathBuf,
    mode: Mode,
    /// Where credentials are kept: files, unless the service was told
    /// otherwise.
    secrets: Secrets,
}
impl Db {
    /// The SQL connection, for tests that set up or break what they need.
    #[cfg(test)]
    pub fn sql(&self) -> &Sqlite {
        &self.sql
    }
    /// Open a local data directory, as tests do.
    #[cfg(test)]
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_as(root, Mode::Local)
    }
    /// Open a data directory as `mode`: the mode it was first opened as, or,
    /// for a new one, the mode it becomes. A local directory with data may
    /// hold internal work, so it never becomes a hub.
    pub fn open_as(root: &Path, mode: Mode) -> Result<Self> {
        private_dir(root)?;
        private_dir(&root.join(SECRETS))?;
        private_sqlite_files(root)?;
        let mut sql = Sqlite::open(root.join(DATABASE))?;
        // The prototype's schema; migrate() builds on it.
        sql.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;
          CREATE TABLE IF NOT EXISTS connections(id TEXT PRIMARY KEY, body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS efforts(id TEXT PRIMARY KEY, title TEXT NOT NULL, note TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS items(id TEXT PRIMARY KEY, body TEXT NOT NULL, effort_id TEXT REFERENCES efforts(id));")?;
        migrate(&mut sql, root)?;
        // Again, now that SQLite made its write-ahead log and shared
        // memory: they are private files of this user's too.
        private_sqlite_files(root)?;
        claim(&mut sql, root, mode)?;
        let db = Self {
            sql,
            root: root.to_path_buf(),
            mode,
            secrets: Secrets::files(root),
        };
        if mode == Mode::Hub {
            db.check_hub()?;
        }
        Ok(db)
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    /// Keep credentials in `secrets` from now on.
    pub fn use_secrets(&mut self, secrets: Secrets) {
        self.secrets = secrets;
    }
    /// Keep `value` as the credential of connection `id`, which has none
    /// yet.
    pub fn create_secret(&self, id: &str, value: &str) -> Result<()> {
        self.secrets.create(id, value)
    }
    /// Refuse a hub's database that holds anything not external, as one
    /// changed behind its back would.
    fn check_hub(&self) -> Result<()> {
        let refuse = |what: String| {
            anyhow!("This hub's database holds {what}, which a hub never keeps; it won't start with it. Move the data directory aside and start the hub with a new one")
        };
        let connections = self.connections()?;
        if let Some(c) = connections.iter().find(|c| c.zone != Zone::External) {
            return Err(refuse(format!("an internal connection ({})", c.id)));
        }
        let external =
            |id: Option<&str>| id.is_some_and(|id| connections.iter().any(|c| c.id == id));
        for (id, body) in self.rows("SELECT id,body FROM items")? {
            let item: Item = serde_json::from_str(&body)?;
            if !external(item.connection_id.as_deref()) || item.zone != Zone::External {
                return Err(refuse(format!("work that isn't external ({id})")));
            }
        }
        for (id, body) in self.rows("SELECT connection_id,body FROM watched")? {
            let items: Vec<Item> = serde_json::from_str(&body)?;
            let theirs = |i: &Item| {
                i.connection_id.as_deref() == Some(id.as_str()) && i.zone == Zone::External
            };
            if !external(Some(&id)) || !items.iter().all(theirs) {
                return Err(refuse(format!(
                    "a watch list that isn't an external connection's ({id})"
                )));
            }
        }
        for (id, _) in self.rows("SELECT connection_id,body FROM agents")? {
            if !external(Some(&id)) {
                return Err(refuse(format!(
                    "agent runs of no external connection ({id})"
                )));
            }
        }
        // Efforts link stored items only, which are external as checked
        // above; one linking anything else was changed behind the hub's back.
        let efforts: Vec<String> = self.efforts()?.into_iter().map(|e| e.id).collect();
        for (id, effort) in
            self.rows("SELECT id,effort_id FROM items WHERE effort_id IS NOT NULL")?
        {
            if !efforts.contains(&effort) {
                return Err(refuse(format!("a link to an unknown effort ({id})")));
            }
        }
        // A hub never pulls from another hub.
        if let Some(link) = self.hub()? {
            return Err(refuse(format!("a hub to pull from ({})", link.url)));
        }
        Ok(())
    }
    /// The pairs of text columns `query` selects.
    fn rows(&self, query: &str) -> Result<Vec<(String, String)>> {
        let mut s = self.sql.prepare(query)?;
        let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
    /// Check a write before it is made. A local service takes every write.
    /// A hub takes only what is shown to be external, and nothing else:
    /// each kind of write is listed, and one that can't show its work is
    /// external is refused.
    fn admit(&self, write: Write) -> Result<()> {
        if self.mode == Mode::Local {
            return Ok(());
        }
        match write {
            Write::Connection(c) => {
                if c.zone != Zone::External {
                    return Err(refused(
                        "internal connections; connect only external sources here, with --external",
                    ));
                }
            }
            Write::Sync { id, found } => {
                let Found {
                    items,
                    watched,
                    agents,
                    dropped,
                } = *found;
                self.external_connection(id)?;
                let theirs =
                    |i: &Item| i.connection_id.as_deref() == Some(id) && i.zone == Zone::External;
                let watched_ok =
                    watched.is_none_or(|w| w.connection_id == id && w.items.iter().all(theirs));
                // Agent runs have no zone of their own: they are the
                // connection's, which is external.
                let agents_ok = agents.is_none_or(|a| a.connection_id == id);
                // It may stop tracking its own items only.
                let dropped_ok = dropped.iter().all(|d| of_connection(d, id));
                if !items.iter().all(theirs) || !watched_ok || !agents_ok || !dropped_ok {
                    return Err(refused("work that isn't its external connection's"));
                }
            }
            Write::Watch(id) => self.external_connection(id)?,
            Write::Manual => {
                return Err(refused(
                    "manual source links, which are internal; add them on the companion",
                ))
            }
            Write::Demo => return Err(refused("the demo's illustrative work, which is internal")),
            Write::Hub => return Err(refused("pulling from a hub; only a companion does")),
            Write::Effort(items) => {
                for id in items {
                    self.external_item(id)?;
                }
            }
            Write::Link(item) => self.external_item(item)?,
            // These add no work: they change or remove what was admitted.
            Write::SyncState | Write::Decision | Write::Disconnect => (),
        }
        Ok(())
    }
    /// Fail unless `id` is a stored external connection.
    fn external_connection(&self, id: &str) -> Result<()> {
        match self.connection(id) {
            Ok(c) if c.zone == Zone::External => Ok(()),
            _ => Err(refused("work of a connection that isn't external")),
        }
    }
    /// Fail unless `id` is a stored item of an external connection.
    fn external_item(&self, id: &str) -> Result<()> {
        let external = self
            .items()?
            .iter()
            .any(|i| i.id == id && i.zone == Zone::External);
        if !external {
            return Err(refused("linking anything but its external work"));
        }
        Ok(())
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let items = self.items()?;
        let efforts = self
            .efforts()?
            .into_iter()
            .map(|effort| EffortView {
                progress: Progress::of(
                    items
                        .iter()
                        .filter(|i| i.effort_id.as_deref() == Some(&effort.id))
                        .map(|i| i.outcome),
                ),
                effort,
                from_hub: false,
            })
            .collect();
        Ok(Snapshot {
            connections: self.connections()?,
            items,
            efforts,
        })
    }
    pub fn connections(&self) -> Result<Vec<Connection>> {
        let mut s = self.sql.prepare(&format!(
            "SELECT c.body,{} FROM connections c LEFT JOIN sync_state s ON s.connection_id=c.id ORDER BY c.id",
            SYNC_STATE_COLUMNS
                .split(',')
                .map(|column| format!("s.{column}"))
                .collect::<Vec<_>>()
                .join(",")
        ))?;
        let rows = s.query_map([], |r| Ok((r.get::<_, String>(0)?, read_sync_state(r, 1)?)))?;
        rows.map(|row| {
            let (body, state) = row?;
            let mut c: Connection = serde_json::from_str(&body)?;
            c.sync = state;
            // Only ever the companion's own.
            c.from_hub = false;
            Ok(c)
        })
        .collect()
    }
    pub fn connection(&self, id: &str) -> Result<Connection> {
        self.connections()?
            .into_iter()
            .find(|c| c.id == id)
            .context("Connection not found")
    }
    /// Save a connection's settings. Its sync state is saved only with what
    /// a sync found, by [`Db::save_sync`] or [`Db::save_sync_state`].
    pub fn save_connection(&self, c: &Connection) -> Result<()> {
        self.admit(Write::Connection(c))?;
        self.sql.execute("INSERT INTO connections VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![c.id,connection_body(c)?])?;
        Ok(())
    }
    pub fn secret(&self, id: &str) -> Result<String> {
        Uuid::parse_str(id)?;
        self.secrets.read(id)
    }
    /// Keep `value` as connection `id`'s next credential, beside the one in
    /// use, for [`Db::install_secret`] to put in place.
    pub fn stage_secret(&self, id: &str, value: &str) -> Result<()> {
        self.secrets.stage(id, value)
    }
    /// Replace connection `id`'s credential with the one staged, so a sync
    /// reads the old one or the new one.
    pub fn install_secret(&self, id: &str) -> Result<()> {
        self.secrets.install(id)
    }
    /// Drop the credential staged for connection `id`, if there is one.
    pub fn discard_staged_secret(&self, id: &str) -> Result<()> {
        self.secrets.discard_staged(id)
    }
    /// Each connection's zone. An item's zone is always its connection's,
    /// never what its row says; anything without a known connection is
    /// internal.
    fn zones(&self) -> Result<HashMap<String, Zone>> {
        Ok(self
            .connections()?
            .into_iter()
            .map(|c| (c.id, c.zone))
            .collect())
    }
    pub fn items(&self) -> Result<Vec<Item>> {
        let zones = self.zones()?;
        let mut s = self
            .sql
            .prepare("SELECT body,effort_id,abandoned_at FROM items ORDER BY id")?;
        let rows = s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.map(|v| {
            let (body, effort, abandoned) = v?;
            let mut i: Item = serde_json::from_str(&body)?;
            i.effort_id = effort;
            // Only ever this machine's own.
            i.from_hub = false;
            i.zone = i
                .connection_id
                .as_ref()
                .and_then(|c| zones.get(c).copied())
                .unwrap_or_default();
            // A person's decision outranks the source's outcome.
            if let Some(at) = &abandoned {
                let reason = format!(
                    "Marked abandoned in Workboard at {at}; the source status is '{}'",
                    i.status
                );
                i.set_outcome((Outcome::Abandoned, reason));
            }
            i.abandoned_at = abandoned;
            Ok(i)
        })
        .collect()
    }
    /// Save a manual source link, new or changed.
    pub fn upsert_item(&self, i: &Item) -> Result<()> {
        self.admit(Write::Manual)?;
        self.sql.execute(
            "INSERT INTO items(id,body,effort_id) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET body=excluded.body",
            params![i.id, serde_json::to_string(i)?, i.effort_id],
        )?;
        Ok(())
    }
    /// Record, or with `abandoned` false withdraw, a person's decision that a
    /// synced item will not deliver. Manual items take their outcome from their
    /// status edit instead. An abandoned item is no longer refreshed, so
    /// abandoning work that is open or delivered takes `force`.
    pub fn set_abandoned(&self, id: &str, abandoned: bool, force: bool) -> Result<Item> {
        let find = || -> Result<Item> {
            self.items()?
                .into_iter()
                .find(|i| i.id == id)
                .context("Item not found")
        };
        let item = find()?;
        if item.connection_id.is_none() {
            bail!("Set a manual item's outcome with its status edit instead")
        }
        self.admit(Write::Decision)?;
        if abandoned
            && !force
            && !(item.outcome.needs_attention() || item.outcome == Outcome::Abandoned)
        {
            bail!(
                "This item is {}, not closed without delivery or unknown. Abandoned items are no longer refreshed, so a later merge would be missed; use --force to abandon it anyway",
                item.outcome.label().to_lowercase()
            )
        }
        self.sql.execute(
            "UPDATE items SET abandoned_at=CASE WHEN ?2 THEN COALESCE(abandoned_at,strftime('%Y-%m-%dT%H:%M:%SZ','now')) END WHERE id=?1",
            params![id, abandoned],
        )?;
        find()
    }
    /// Save what a sync of connection `id` found, as [`Db::save_found`] does,
    /// for a connector that reads no heartbeat.
    #[cfg(test)]
    pub fn save_sync(
        &mut self,
        id: &str,
        items: &[Item],
        watched: Option<&WatchList>,
        state: &SyncState,
    ) -> Result<()> {
        let found = Found {
            items,
            watched,
            agents: None,
            dropped: &[],
        };
        self.save_found(id, &found, state)
    }
    /// Save synced items without touching watch lists, as tests set up.
    #[cfg(test)]
    pub fn sync_items(&mut self, items: &[Item]) -> Result<()> {
        let tx = self.sql.transaction()?;
        upsert_synced(&tx, items)?;
        tx.commit()?;
        Ok(())
    }
    /// Save what a sync of connection `id` found, all or nothing, once it
    /// has been processed: its items, what it found watching another user,
    /// which replaces the last such list, the agent runs of a bot's
    /// heartbeat, which replace the last ones, the removal of items its
    /// connector stopped tracking, and the connection's sync state. Nothing
    /// is saved before, so a sync that stops halfway leaves the last good
    /// data and state, and the next one fetches everything again.
    pub fn save_found(&mut self, id: &str, found: &Found, state: &SyncState) -> Result<()> {
        self.admit(Write::Sync { id, found })?;
        let Found {
            items,
            watched,
            agents,
            dropped,
        } = *found;
        let tx = self.sql.transaction()?;
        check_connected(&tx, id)?;
        upsert_synced(&tx, items)?;
        for item in dropped {
            if !of_connection(item, id) {
                bail!("An item of another connection was to be removed")
            }
            tx.execute("DELETE FROM items WHERE id=?1", [item])?;
        }
        tx.execute("DELETE FROM watched WHERE connection_id=?1", [id])?;
        if let Some(w) = watched {
            if w.connection_id != id {
                bail!("A watch list was saved for another connection")
            }
            tx.execute(
                "INSERT INTO watched(connection_id,login,body,error,last_success) VALUES(?1,?2,?3,?4,?5)",
                params![
                    id,
                    w.login,
                    serde_json::to_string(&w.items)?,
                    w.error,
                    w.last_success.map(Timestamp::unix)
                ],
            )?;
        }
        tx.execute("DELETE FROM agents WHERE connection_id=?1", [id])?;
        if let Some(a) = agents {
            if a.connection_id != id {
                bail!("Agent runs were saved for another connection")
            }
            tx.execute(
                "INSERT INTO agents(connection_id,body,error,last_success,board) VALUES(?1,?2,?3,?4,?5)",
                params![
                    id,
                    serde_json::to_string(&a.heartbeat)?,
                    a.error,
                    a.last_success.map(Timestamp::unix),
                    a.board.as_ref().map(serde_json::to_string).transpose()?
                ],
            )?;
        }
        write_sync_state(&tx, id, state)?;
        tx.commit()?;
        Ok(())
    }
    /// Save the sync state of connection `id` after a sync that failed,
    /// leaving its items as they were.
    pub fn save_sync_state(&mut self, id: &str, state: &SyncState) -> Result<()> {
        self.admit(Write::SyncState)?;
        let tx = self.sql.transaction()?;
        check_connected(&tx, id)?;
        write_sync_state(&tx, id, state)?;
        tx.commit()?;
        Ok(())
    }
    /// What each connection found by watching another user, and why its
    /// last sync couldn't update that, if it couldn't.
    pub fn watched(&self) -> Result<Vec<WatchList>> {
        let zones = self.zones()?;
        let mut s = self
            .sql
            .prepare("SELECT connection_id,login,body,error,last_success FROM watched ORDER BY connection_id")?;
        let rows = s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })?;
        let mut lists = Vec::new();
        for row in rows {
            let (connection_id, login, body, error, last_success) = row?;
            let Some(zone) = zones.get(&connection_id).copied() else {
                continue;
            };
            let mut items: Vec<Item> = serde_json::from_str(&body)?;
            for i in &mut items {
                i.zone = zone;
                i.from_hub = false;
            }
            lists.push(WatchList {
                connection_id,
                login,
                items,
                error,
                last_success: last_success.map(Timestamp::from_unix),
            });
        }
        Ok(lists)
    }
    /// What each bot connection last read of its heartbeat, and why its last
    /// sync couldn't read it, if it couldn't.
    pub fn agents(&self) -> Result<Vec<AgentRuns>> {
        let connections = self.connections()?;
        let mut s = self.sql.prepare(
            "SELECT connection_id,body,error,last_success,board FROM agents ORDER BY connection_id",
        )?;
        let rows = s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut all = Vec::new();
        for row in rows {
            let (connection_id, body, error, last_success, board) = row?;
            if !connections.iter().any(|c| c.id == connection_id) {
                continue;
            }
            all.push(AgentRuns {
                connection_id,
                heartbeat: serde_json::from_str(&body)?,
                error,
                last_success: last_success.map(Timestamp::from_unix),
                stale: false,
                board: board.map(|b| serde_json::from_str(&b)).transpose()?,
            });
        }
        Ok(all)
    }
    /// Watch another user from connection `id`, or stop. What was found for
    /// whoever it watched before is dropped.
    pub fn set_watch_user(&mut self, id: &str, login: Option<&str>) -> Result<Connection> {
        self.admit(Write::Watch(id))?;
        let mut c = self.connection(id)?;
        c.watch_user = login.map(str::to_string);
        let tx = self.sql.transaction()?;
        tx.execute(
            "UPDATE connections SET body=?1 WHERE id=?2",
            params![connection_body(&c)?, id],
        )?;
        tx.execute("DELETE FROM watched WHERE connection_id=?1", [id])?;
        tx.commit()?;
        Ok(c)
    }
    pub fn efforts(&self) -> Result<Vec<Effort>> {
        let mut s = self
            .sql
            .prepare("SELECT id,title,note,share_title FROM efforts ORDER BY rowid")?;
        let rows = s.query_map([], |r| {
            Ok(Effort {
                id: r.get(0)?,
                title: r.get(1)?,
                note: r.get(2)?,
                share_title: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
    pub fn create_effort(&mut self, input: EffortInput) -> Result<Effort> {
        if input.title.trim().is_empty() || input.title.len() > 250 {
            bail!("Enter a title of 1–250 characters")
        }
        self.admit(Write::Effort(&input.item_ids))?;
        let e = Effort {
            id: Uuid::new_v4().to_string(),
            title: input.title.trim().into(),
            note: input.note,
            share_title: input.share_title,
        };
        let tx = self.sql.transaction()?;
        tx.execute(
            "INSERT INTO efforts(id,title,note,share_title) VALUES(?1,?2,?3,?4)",
            params![e.id, e.title, e.note, e.share_title],
        )?;
        for id in input.item_ids {
            if tx.execute(
                "UPDATE items SET effort_id=?1 WHERE id=?2",
                params![e.id, id],
            )? != 1
            {
                bail!("Item not found")
            }
        }
        tx.commit()?;
        Ok(e)
    }
    /// Let an effort's title leave this machine with its external items, or
    /// stop it.
    pub fn share_title(&self, id: &str, share: bool) -> Result<Effort> {
        self.admit(Write::Decision)?;
        if self.sql.execute(
            "UPDATE efforts SET share_title=?1 WHERE id=?2",
            params![share, id],
        )? != 1
        {
            bail!("Effort not found")
        }
        self.efforts()?
            .into_iter()
            .find(|e| e.id == id)
            .context("Effort not found")
    }
    pub fn link(&self, item: &str, effort: Option<&str>) -> Result<()> {
        self.admit(Write::Link(item))?;
        if let Some(e) = effort {
            if !self.efforts()?.iter().any(|v| v.id == e) {
                bail!("Effort not found")
            }
        }
        if self.sql.execute(
            "UPDATE items SET effort_id=?1 WHERE id=?2",
            params![effort, item],
        )? != 1
        {
            bail!("Item not found")
        }
        Ok(())
    }
    pub fn delete_connection(&mut self, id: &str) -> Result<()> {
        self.admit(Write::Disconnect)?;
        self.connection(id)?;
        Uuid::parse_str(id)?;
        let ids: Vec<_> = self
            .items()?
            .into_iter()
            .filter(|i| i.connection_id.as_deref() == Some(id))
            .map(|i| i.id)
            .collect();
        let tx = self.sql.transaction()?;
        for item in ids {
            tx.execute("DELETE FROM items WHERE id=?1", [item])?;
        }
        tx.execute("DELETE FROM watched WHERE connection_id=?1", [id])?;
        tx.execute("DELETE FROM agents WHERE connection_id=?1", [id])?;
        tx.execute("DELETE FROM sync_state WHERE connection_id=?1", [id])?;
        tx.execute("DELETE FROM connections WHERE id=?1", [id])?;
        tx.commit()?;
        // The credential, and a new one a verify staged but never put in
        // place.
        self.remove_secrets(id)
    }
    /// The hub this companion pulls from, if one is set.
    pub fn hub(&self) -> Result<Option<hub::Link>> {
        let mut s = self.sql.prepare(&format!(
            "SELECT h.id,h.url,h.snapshot,{} FROM hub h LEFT JOIN sync_state s ON s.connection_id=h.id",
            SYNC_STATE_COLUMNS
                .split(',')
                .map(|column| format!("s.{column}"))
                .collect::<Vec<_>>()
                .join(",")
        ))?;
        let link = s
            .query_row([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    read_sync_state(r, 3)?,
                ))
            })
            .optional()?;
        let Some((id, url, snapshot, sync)) = link else {
            return Ok(None);
        };
        let snapshot = snapshot
            .map(|body| serde_json::from_str(&body))
            .transpose()
            .context("The stored export of the hub is unreadable; set the hub again")?;
        Ok(Some(hub::Link {
            id,
            url,
            sync,
            snapshot,
        }))
    }
    /// Pull from the hub at `url` with `token`, in place of any hub set
    /// before, whose token and export are dropped.
    pub fn set_hub(&mut self, url: &str, token: &str) -> Result<hub::Link> {
        self.admit(Write::Hub)?;
        let old = self.hub()?;
        let id = Uuid::new_v4().to_string();
        self.secrets.create(&id, token)?;
        let saved = (|| -> Result<()> {
            let tx = self.sql.transaction()?;
            if let Some(old) = &old {
                tx.execute("DELETE FROM sync_state WHERE connection_id=?1", [&old.id])?;
            }
            tx.execute("DELETE FROM hub", [])?;
            tx.execute("INSERT INTO hub(id,url) VALUES(?1,?2)", params![id, url])?;
            tx.commit()?;
            Ok(())
        })();
        if let Err(e) = saved {
            let _ = self.secrets.remove(&id);
            return Err(e);
        }
        if let Some(old) = old {
            self.remove_secrets(&old.id)?;
        }
        self.hub()?.context("The hub was not saved")
    }
    /// Stop pulling from the hub, and drop its token and what it sent.
    pub fn unset_hub(&mut self) -> Result<()> {
        self.admit(Write::Hub)?;
        let Some(old) = self.hub()? else {
            bail!("No hub is set")
        };
        let tx = self.sql.transaction()?;
        tx.execute("DELETE FROM sync_state WHERE connection_id=?1", [&old.id])?;
        tx.execute("DELETE FROM hub", [])?;
        tx.commit()?;
        self.remove_secrets(&old.id)
    }
    /// Save what a pull from hub `id` read, if it succeeded, and the state
    /// of its pulls, together. Nothing is saved for a hub that was unset or
    /// replaced meanwhile.
    pub fn save_hub_pull(
        &mut self,
        id: &str,
        snapshot: Option<&ExternalSnapshot>,
        state: &SyncState,
    ) -> Result<()> {
        self.admit(Write::Hub)?;
        let tx = self.sql.transaction()?;
        let exists: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM hub WHERE id=?1)", [id], |r| {
                r.get(0)
            })?;
        if !exists {
            bail!("The hub was unset or replaced")
        }
        if let Some(snapshot) = snapshot {
            tx.execute(
                "UPDATE hub SET snapshot=?1 WHERE id=?2",
                params![serde_json::to_string(snapshot)?, id],
            )?;
        }
        write_sync_state(&tx, id, state)?;
        tx.commit()?;
        Ok(())
    }
    /// Remove the credential of `id`, and a new one staged beside it.
    pub fn remove_secrets(&self, id: &str) -> Result<()> {
        self.secrets.remove(id)
    }
    /// Drop the credentials that nothing stored uses: no connection, and
    /// not the hub this companion pulls from. What a removal cut short
    /// left, such as by a crash between dropping a connection and its
    /// credential. Only once everything stored was read: if any of it
    /// can't be, nothing is dropped, since a credential in use could look
    /// unused. Returns how many went.
    pub fn drop_unused_secrets(&self) -> Result<usize> {
        let mut live: std::collections::HashSet<String> =
            self.connections()?.into_iter().map(|c| c.id).collect();
        let rows: i64 = self
            .sql
            .query_row("SELECT COUNT(*) FROM connections", [], |r| r.get(0))?;
        if usize::try_from(rows).ok() != Some(live.len()) {
            bail!("Not every connection could be told apart; keeping every credential")
        }
        if let Some(link) = self.hub()? {
            live.insert(link.id);
        }
        self.secrets.reconcile(&live)
    }
    pub fn seed(&mut self) -> Result<()> {
        self.admit(Write::Demo)?;
        if !self.items()?.is_empty() || !self.efforts()?.is_empty() {
            bail!("Demo data requires an empty database; use a separate --data-dir")
        }
        let examples = [
            (
                "jira",
                "RHEL-DEMO-1",
                "Deliver the bootc storage fix",
                "In progress",
                "next",
                "https://jira.example.invalid/browse/RHEL-DEMO-1",
                Outcome::Open,
            ),
            (
                "github",
                "upstream",
                "Upstream storage fix",
                "Merged",
                "",
                "https://github.com/bootc-dev/bootc",
                Outcome::Delivered,
            ),
            (
                "gitlab-internal",
                "rhel-backport",
                "Backport the fix to RHEL",
                "Awaiting review",
                "waiting",
                "https://gitlab.example.invalid/rhel/bootc/-/merge_requests/1",
                Outcome::Open,
            ),
            (
                "errata",
                "advisory",
                "Publish the RHEL advisory",
                "Needs validation",
                "next",
                "https://errata.example.invalid/advisory/1",
                Outcome::Open,
            ),
            (
                "gitlab-public",
                "centos-review",
                "Review the CentOS Stream change",
                "Open",
                "review",
                "https://gitlab.com/redhat/centos-stream",
                Outcome::Open,
            ),
            (
                "fedora",
                "fedora-ci",
                "Investigate the Fedora test failure",
                "Checks failed",
                "next",
                "https://forge.fedoraproject.org",
                Outcome::Open,
            ),
            (
                "github",
                "first-attempt",
                "First attempt at the storage fix",
                "Closed",
                "",
                "https://github.com/bootc-dev/bootc",
                Outcome::ClosedWithoutDelivery,
            ),
        ];
        for (n, (provider, key, title, status, action, url, outcome)) in examples.iter().enumerate()
        {
            self.upsert_item(&Item {
                id: format!("demo-{n}"),
                connection_id: None,
                provider: (*provider).into(),
                remote_id: (*key).into(),
                key: (*key).into(),
                title: (*title).into(),
                url: (*url).into(),
                status: (*status).into(),
                outcome: *outcome,
                outcome_reason: outcome.manual_reason().into(),
                action: (*action).into(),
                updated_at: "2026-10-06T20:00:00Z".into(),
                effort_id: None,
                abandoned_at: None,
                demo: true,
                zone: Zone::Internal,
                reasons: vec![],
                synced_at: None,
                stale: false,
                from_hub: false,
                bot: None,
            })?;
        }
        self.create_effort(EffortInput {title:"Ship the bootc storage fix".into(),note:"Example effort linking upstream work, a RHEL backport and an advisory. All items are illustrative.".into(),item_ids:[0,1,2,3,6].map(|n|format!("demo-{n}")).to_vec(),share_title:false})?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};
    #[test]
    fn synchronization_preserves_manual_effort_assignments() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = Db::open(&temp.path().join("db")).unwrap();
        db.seed().unwrap();
        let mut item = db
            .items()
            .unwrap()
            .into_iter()
            .find(|i| i.id == "demo-0")
            .unwrap();
        let effort = item.effort_id.clone();
        item.effort_id = None;
        item.status = "Released".into();
        item.set_outcome((Outcome::Delivered, "Released".into()));
        db.sync_items(&[item]).unwrap();
        let item = db
            .items()
            .unwrap()
            .into_iter()
            .find(|i| i.id == "demo-0")
            .unwrap();
        assert_eq!(item.effort_id, effort);
        assert_eq!(item.outcome, Outcome::Delivered);
    }

    #[test]
    fn abandoned_decision_survives_sync_and_can_be_undone() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = Db::open(&temp.path().join("db")).unwrap();
        db.seed().unwrap();
        let find = |db: &Db, id: &str| {
            db.items()
                .unwrap()
                .into_iter()
                .find(|i| i.id == id)
                .unwrap()
        };
        // Demo items are manual; make one look synced.
        let mut closed = find(&db, "demo-6");
        closed.connection_id = Some("c0ffee00-0000-4000-8000-000000000000".into());
        db.sync_items(&[closed.clone()]).unwrap();
        assert!(
            db.set_abandoned("demo-0", true, true).is_err(),
            "manual items refuse"
        );
        // Open or delivered work needs force: it would no longer be refreshed.
        for (id, outcome) in [("demo-0", Outcome::Open), ("demo-1", Outcome::Delivered)] {
            let mut item = find(&db, id);
            item.connection_id = closed.connection_id.clone();
            db.sync_items(&[item]).unwrap();
            let error = db.set_abandoned(id, true, false).err().unwrap().to_string();
            assert!(error.contains("--force"), "{error}");
            assert_eq!(
                db.set_abandoned(id, true, true).unwrap().outcome,
                Outcome::Abandoned
            );
            assert_eq!(db.set_abandoned(id, false, false).unwrap().outcome, outcome);
        }

        let item = db.set_abandoned("demo-6", true, false).unwrap();
        assert_eq!(item.outcome, Outcome::Abandoned);
        let at = item.abandoned_at.clone().unwrap();
        assert!(item.outcome_reason.contains(&at));
        // Marking it again keeps the first time.
        assert_eq!(
            db.set_abandoned("demo-6", true, false)
                .unwrap()
                .abandoned_at,
            Some(at)
        );
        // A sync rewrites the source's view but keeps the decision.
        db.sync_items(&[closed.clone()]).unwrap();
        let progress = |db: &Db| db.snapshot().unwrap().efforts[0].progress.clone();
        assert_eq!(find(&db, "demo-6").outcome, Outcome::Abandoned);
        assert_eq!(
            (progress(&db).abandoned, progress(&db).needs_attention),
            (1, 0)
        );

        let item = db.set_abandoned("demo-6", false, false).unwrap();
        assert_eq!(item.outcome, Outcome::ClosedWithoutDelivery);
        assert_eq!(item.outcome_reason, closed.outcome_reason);
        assert_eq!(item.abandoned_at, None);
        assert_eq!(
            (progress(&db).abandoned, progress(&db).needs_attention),
            (0, 1)
        );
    }

    /// An item row as the prototype (schema version 0) stored it.
    fn legacy_body(id: &str, synced: bool, provider: &str, status: &str, terminal: bool) -> String {
        serde_json::json!({
            "id": id,
            "connection_id": synced.then_some("c0ffee00-0000-4000-8000-000000000000"),
            "provider": provider,
            "remote_id": id,
            "key": id,
            "title": format!("Title of {id}"),
            "url": format!("https://example.com/{id}"),
            "status": status,
            "terminal": terminal,
            "action": if terminal { "" } else { "next" },
            "updated_at": "2026-10-06T20:00:00Z",
            "effort_id": null,
            "demo": false,
        })
        .to_string()
    }

    #[test]
    fn migrates_prototype_items_to_outcomes() {
        use Outcome::*;
        let rows = [
            (
                "merged-mr",
                true,
                "gitlab-internal",
                "merged",
                true,
                Delivered,
            ),
            (
                "closed-mr",
                true,
                "gitlab-public",
                "closed",
                true,
                ClosedWithoutDelivery,
            ),
            ("draft-mr", true, "gitlab-internal", "Draft", false, Open),
            // Its resolution was never fetched: the next sync decides.
            ("done-jira", true, "jira", "Closed", true, Unknown),
            ("open-jira", true, "jira", "In Progress", false, Open),
            (
                "manual-released",
                false,
                "errata",
                "Released",
                true,
                Delivered,
            ),
            (
                "manual-closed",
                false,
                "github",
                "Closed",
                true,
                ClosedWithoutDelivery,
            ),
            (
                "manual-open",
                false,
                "fedora",
                "Needs validation",
                false,
                Open,
            ),
        ];
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("db");
        private_dir(&root).unwrap();
        {
            // The prototype's exact schema, with no user_version.
            let sql = Sqlite::open(root.join("work.sqlite")).unwrap();
            sql.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;
              CREATE TABLE IF NOT EXISTS connections(id TEXT PRIMARY KEY, body TEXT NOT NULL);
              CREATE TABLE IF NOT EXISTS efforts(id TEXT PRIMARY KEY, title TEXT NOT NULL, note TEXT NOT NULL);
              CREATE TABLE IF NOT EXISTS items(id TEXT PRIMARY KEY, body TEXT NOT NULL, effort_id TEXT REFERENCES efforts(id));
              INSERT INTO efforts VALUES('e1','Ship it','note');").unwrap();
            for (n, (id, synced, provider, status, terminal, _)) in rows.iter().enumerate() {
                let effort = (n % 2 == 0).then_some("e1");
                sql.execute(
                    "INSERT INTO items VALUES(?1,?2,?3)",
                    params![
                        id,
                        legacy_body(id, *synced, provider, status, *terminal),
                        effort
                    ],
                )
                .unwrap();
            }
        }
        // What a copy cut short by a crash left is no copy: it is written
        // again, whole, and put in place only then.
        let partial = root.join("work.v0.sqlite.partial");
        fs::write(&partial, "").unwrap();
        for _ in 0..2 {
            // Opening again must not migrate twice.
            let db = Db::open(&root).unwrap();
            assert!(!partial.exists());
            let version: i32 = db
                .sql
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION);
            let items = db.items().unwrap();
            assert_eq!(items.len(), rows.len());
            for (n, (id, synced, _, status, _, outcome)) in rows.iter().enumerate() {
                let item = items.iter().find(|i| i.id == *id).unwrap();
                assert_eq!(item.outcome, *outcome, "{id}");
                assert!(!item.outcome_reason.is_empty(), "{id}");
                assert_eq!(item.status, *status, "{id}");
                assert_eq!(item.connection_id.is_some(), *synced, "{id}");
                assert_eq!(
                    item.effort_id.as_deref(),
                    (n % 2 == 0).then_some("e1"),
                    "{id}"
                );
                assert_eq!(item.abandoned_at, None);
                if *outcome != Open {
                    assert_eq!(item.action, "", "{id}");
                }
            }
            assert_eq!(db.efforts().unwrap().len(), 1);
        }
        // The prototype's database was kept as it was.
        let backup = Sqlite::open(root.join("work.v0.sqlite")).unwrap();
        let version: i32 = backup
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        let bodies: Vec<String> = backup
            .prepare("SELECT body FROM items")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(version, 0);
        assert_eq!(bodies.len(), rows.len());
        assert!(bodies.iter().all(|b| b.contains("\"terminal\"")));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(root.join("work.v0.sqlite"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    fn connection(id: &str, zone: Zone) -> Connection {
        Connection {
            id: id.into(),
            provider: "gitlab-public".into(),
            label: format!("GitLab {id}"),
            base_url: "https://gitlab.com/".into(),
            account_id: "1".into(),
            account_name: "me".into(),
            zone,
            watch_user: None,
            ..Default::default()
        }
    }

    /// Item `n` of connection `id`, external, as a sync stores it.
    fn synced_item(id: &str, n: u32) -> Item {
        Item {
            id: format!("{id}:errata:{n}"),
            connection_id: Some(id.into()),
            provider: "errata".into(),
            remote_id: n.to_string(),
            key: format!("K-{n}"),
            title: format!("Change {n}"),
            url: format!("https://example.com/{n}"),
            status: "open".into(),
            outcome: Outcome::Open,
            outcome_reason: String::new(),
            action: "next".into(),
            updated_at: String::new(),
            effort_id: None,
            abandoned_at: None,
            demo: false,
            zone: Zone::External,
            reasons: vec![],
            synced_at: None,
            stale: false,
            from_hub: false,
            bot: None,
        }
    }

    #[test]
    fn zones_persist_and_items_take_their_connections() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("db");
        let external = connection("c0ffee00-0000-4000-8000-00000000000e", Zone::External);
        let internal = connection("c0ffee00-0000-4000-8000-000000000001", Zone::Internal);
        {
            let mut db = Db::open(&root).unwrap();
            db.seed().unwrap();
            db.save_connection(&external).unwrap();
            db.save_connection(&internal).unwrap();
            let template = db.items().unwrap().remove(0);
            let items: Vec<_> = [&external, &internal]
                .into_iter()
                .map(|c| Item {
                    id: format!("{}:gitlab:7:4", c.id),
                    connection_id: Some(c.id.clone()),
                    demo: false,
                    // What a row says about its zone is never trusted.
                    zone: Zone::External,
                    ..template.clone()
                })
                .collect();
            db.sync_items(&items).unwrap();
        }
        // Open it again, as after a restart.
        let db = Db::open(&root).unwrap();
        let zones: HashMap<_, _> = db
            .connections()
            .unwrap()
            .into_iter()
            .map(|c| (c.id, c.zone))
            .collect();
        assert_eq!(zones[&external.id], Zone::External);
        assert_eq!(zones[&internal.id], Zone::Internal);
        let items = db.items().unwrap();
        assert_eq!(items.len(), 9);
        for i in items {
            let expected = if i.connection_id.as_ref() == Some(&external.id) {
                Zone::External
            } else {
                // Internal connections and manual items.
                Zone::Internal
            };
            assert_eq!(i.zone, expected, "{}", i.id);
        }
    }

    #[test]
    fn migrates_existing_connections_to_the_internal_zone() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("db");
        drop(Db::open(&root).unwrap());
        // A connection as schema version 1 stored it, without a zone.
        let legacy = serde_json::json!({
            "id": "c1", "provider": "gitlab-public", "label": "GitLab",
            "base_url": "https://gitlab.com/", "account_id": "1", "account_name": "me",
            "last_sync": null, "error": null,
        });
        {
            let sql = Sqlite::open(root.join("work.sqlite")).unwrap();
            sql.execute(
                "INSERT INTO connections VALUES('c1',?1)",
                [legacy.to_string()],
            )
            .unwrap();
            // Version 1 had no title sharing either.
            sql.execute_batch("ALTER TABLE efforts DROP COLUMN share_title")
                .unwrap();
            sql.pragma_update(None, "user_version", 1).unwrap();
        }
        for _ in 0..2 {
            let db = Db::open(&root).unwrap();
            let version: i32 = db
                .sql
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION);
            let body: String = db
                .sql
                .query_row("SELECT body FROM connections WHERE id='c1'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            let stored: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(stored["zone"], "internal");
            assert_eq!(stored["label"], "GitLab");
            assert_eq!(db.connection("c1").unwrap().zone, Zone::Internal);
        }
        let backup = Sqlite::open(root.join("work.v1.sqlite")).unwrap();
        let body: String = backup
            .query_row("SELECT body FROM connections", [], |r| r.get(0))
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), legacy);
    }

    #[test]
    fn effort_titles_are_not_shared_unless_asked() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("db");
        let id = {
            let mut db = Db::open(&root).unwrap();
            db.seed().unwrap();
            let e = db.efforts().unwrap().remove(0);
            assert!(!e.share_title, "the demo effort");
            let shared = db
                .create_effort(EffortInput {
                    title: "Shared".into(),
                    note: String::new(),
                    item_ids: vec![],
                    share_title: true,
                })
                .unwrap();
            assert!(shared.share_title);
            assert!(db.share_title(&e.id, true).unwrap().share_title);
            assert!(db.share_title("missing", true).is_err());
            e.id
        };
        // Open it again, as after a restart.
        let db = Db::open(&root).unwrap();
        let efforts = db.efforts().unwrap();
        assert!(efforts.iter().all(|e| e.share_title));
        assert!(!db.share_title(&id, false).unwrap().share_title);
    }

    #[test]
    fn watch_lists_are_kept_apart_and_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("db");
        let mut db = Db::open(&root).unwrap();
        db.seed().unwrap();
        let c = connection("c0ffee00-0000-4000-8000-00000000000e", Zone::External);
        db.save_connection(&c).unwrap();
        let ok = SyncState::default().succeeded(Timestamp::from_unix(1_791_763_200));
        let mut theirs = db.items().unwrap().remove(0);
        theirs.id = format!("{}:github:1", c.id);
        theirs.connection_id = Some(c.id.clone());
        theirs.zone = Zone::Internal;
        let found_at = Some(Timestamp::from_unix(1_791_763_100));
        let list = |login: &str| WatchList {
            connection_id: c.id.clone(),
            login: login.into(),
            items: vec![theirs.clone()],
            error: None,
            last_success: found_at,
        };
        // Whom each stored list watches.
        let logins = |db: &Db| -> Vec<String> {
            db.sql
                .prepare("SELECT login FROM watched")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        db.save_sync(&c.id, &[], Some(&list("someone")), &ok)
            .unwrap();
        let other = WatchList {
            connection_id: "another".into(),
            ..list("someone")
        };
        assert!(db.save_sync(&c.id, &[], Some(&other), &ok).is_err());
        assert_eq!(logins(&db), ["someone"]);
        let found = db.watched().unwrap();
        assert_eq!((found.len(), found[0].login.as_str()), (1, "someone"));
        // When it was found, apart from the account's own sync.
        assert_eq!(found[0].last_success, found_at);
        // Its connection's zone, like any item; and never one of the
        // account's own items.
        assert_eq!(found[0].items[0].zone, Zone::External);
        assert!(db.items().unwrap().iter().all(|i| i.id != theirs.id));
        // A sync without watching drops the list; so does changing whom
        // it watches, and disconnecting.
        // A failed watch is kept with its error.
        let failed = WatchList {
            error: Some("Watching someone: refused".into()),
            ..list("someone")
        };
        db.save_sync(&c.id, &[], Some(&failed), &ok).unwrap();
        let found = db.watched().unwrap();
        assert_eq!(found[0].error, failed.error);
        assert_eq!(found[0].items.len(), 1);
        db.save_sync(&c.id, &[], None, &ok).unwrap();
        assert!(logins(&db).is_empty());
        db.save_sync(&c.id, &[], Some(&list("someone")), &ok)
            .unwrap();
        let changed = db.set_watch_user(&c.id, Some("someone-else")).unwrap();
        assert_eq!(changed.watch_user.as_deref(), Some("someone-else"));
        assert_eq!(db.connection(&c.id).unwrap().watch_user, changed.watch_user);
        assert!(logins(&db).is_empty());
        db.save_sync(&c.id, &[], Some(&list("someone-else")), &ok)
            .unwrap();
        db.delete_connection(&c.id).unwrap();
        assert!(logins(&db).is_empty());
    }

    #[test]
    fn sync_state_is_saved_with_what_the_sync_found() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("db");
        let c = connection("c0ffee00-0000-4000-8000-00000000000e", Zone::External);
        let at = |minutes: i64| Timestamp::from_unix(1_791_763_200 + minutes * 60);
        let template = {
            let mut db = Db::open(&root).unwrap();
            db.seed().unwrap();
            db.items().unwrap().remove(0)
        };
        let item = |title: &str| Item {
            id: format!("{}:gitlab:1", c.id),
            connection_id: Some(c.id.clone()),
            title: title.into(),
            demo: false,
            ..template.clone()
        };
        let mut db = Db::open(&root).unwrap();
        // Nothing is saved for a connection that isn't there, such as one
        // disconnected while it synced.
        let first = SyncState::default().succeeded(at(0));
        assert!(db.save_sync(&c.id, &[item("first")], None, &first).is_err());
        assert!(db.save_sync_state(&c.id, &first).is_err());
        assert!(db.items().unwrap().iter().all(|i| i.title != "first"));
        db.save_connection(&c).unwrap();
        assert_eq!(db.connection(&c.id).unwrap().sync, SyncState::default());
        db.save_sync(&c.id, &[item("first")], None, &first).unwrap();
        // A failure keeps the items and the last success, and says why.
        let error = anyhow::Error::new(
            ConnectorError::new(ErrorKind::Transient, "Rate limited")
                .held_until(UNIX_EPOCH + Duration::from_secs(at(30).unix() as u64)),
        );
        let failed = first.failed(at(5), &error).failed(at(10), &error);
        db.save_sync_state(&c.id, &failed).unwrap();
        // Saving the connection's settings leaves its state alone.
        db.save_connection(&c).unwrap();
        drop(db);
        // As after a restart.
        let db = Db::open(&root).unwrap();
        let state = db.connection(&c.id).unwrap().sync;
        assert_eq!(
            state,
            SyncState {
                last_success: Some(at(0)),
                last_attempt: Some(at(10)),
                next_due: None,
                failures: 2,
                error_kind: Some(ErrorKind::Transient),
                error: Some("Rate limited".into()),
                held_until: Some(at(30)),
            }
        );
        let titles: Vec<_> = db
            .items()
            .unwrap()
            .into_iter()
            .filter(|i| i.connection_id.is_some())
            .map(|i| i.title)
            .collect();
        assert_eq!(titles, ["first"]);
        // The state isn't part of the stored connection.
        let body: String = db
            .sql
            .query_row("SELECT body FROM connections", [], |r| r.get(0))
            .unwrap();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body.get("sync"), None, "{body}");
        assert_eq!(body.get("schedule"), None, "{body}");
        let mut db = db;
        // A credential, and a new one a failed verify left staged.
        let secrets = root.join("secrets");
        write_secret(&secrets.join(&c.id), "test-only-token").unwrap();
        db.stage_secret(&c.id, "new-token").unwrap();
        db.delete_connection(&c.id).unwrap();
        let rows: i64 = db
            .sql
            .query_row("SELECT COUNT(*) FROM sync_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        let left: Vec<_> = fs::read_dir(&secrets).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// A credential nothing stored uses is dropped at start: a removal cut
    /// short, here between dropping the connection and its credential. One
    /// in use never is, nor anything while the database can't all be read.
    #[test]
    fn drops_only_unused_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("db");
        let mut db = Db::open(&root).unwrap();
        let id = Uuid::new_v4().to_string();
        db.save_connection(&connection(&id, Zone::Internal))
            .unwrap();
        db.create_secret(&id, "connection-token").unwrap();
        // A verify cut short left a new one staged: still the connection's.
        db.stage_secret(&id, "staged-token").unwrap();
        let hub = db
            .set_hub("https://hub.example.ts.net/", "hub-token")
            .unwrap();
        let orphan = Uuid::new_v4().to_string();
        db.create_secret(&orphan, "orphan-token").unwrap();
        db.create_secret(&Uuid::new_v4().to_string(), "orphan-token")
            .unwrap();
        assert_eq!(db.drop_unused_secrets().unwrap(), 2);
        assert_eq!(db.secret(&id).unwrap(), "connection-token");
        assert_eq!(db.secret(&hub.id).unwrap(), "hub-token");
        assert!(db.secret(&orphan).is_err());
        let secrets = root.join(SECRETS);
        let mut left: Vec<_> = fs::read_dir(&secrets)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut expected = vec![id.clone(), format!("{id}.new"), hub.id.clone()];
        expected.sort();
        assert_eq!(left, expected);
        assert_eq!(db.drop_unused_secrets().unwrap(), 0);

        // A connection that can't be read: nothing is dropped.
        let another = Uuid::new_v4().to_string();
        db.create_secret(&another, "maybe-in-use").unwrap();
        db.sql
            .execute(
                "INSERT INTO connections VALUES(?1,'not a connection')",
                [&another],
            )
            .unwrap();
        assert!(db.drop_unused_secrets().is_err());
        assert_eq!(db.secret(&another).unwrap(), "maybe-in-use");
        assert_eq!(db.secret(&id).unwrap(), "connection-token");
    }

    #[test]
    fn migrates_sync_state_out_of_connections() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("db");
        drop(Db::open(&root).unwrap());
        // Connections as schema version 4 stored them.
        let legacy = |id: &str, last_sync: Value, error: Value, kind: Value| {
            serde_json::json!({
                "id": id, "provider": "github", "label": id,
                "base_url": "https://github.com/", "account_id": "1", "account_name": "me",
                "last_sync": last_sync, "error": error, "zone": "external",
                "error_kind": kind, "watch_user": null,
            })
        };
        let rows = [
            legacy(
                "ok",
                "2026-10-12T00:00:00Z".into(),
                Value::Null,
                Value::Null,
            ),
            legacy(
                "denied",
                Value::Null,
                "Credential rejected".into(),
                "auth".into(),
            ),
            // Before error kinds were kept.
            legacy(
                "old",
                "2026-10-11T00:00:00Z".into(),
                "Failed".into(),
                Value::Null,
            ),
        ];
        {
            let sql = Sqlite::open(root.join("work.sqlite")).unwrap();
            sql.execute_batch(
                "DROP TABLE sync_state; ALTER TABLE watched DROP COLUMN last_success;",
            )
            .unwrap();
            for row in &rows {
                sql.execute(
                    "INSERT INTO connections VALUES(?1,?2)",
                    params![row["id"].as_str().unwrap(), row.to_string()],
                )
                .unwrap();
            }
            sql.pragma_update(None, "user_version", 4).unwrap();
        }
        let db = Db::open(&root).unwrap();
        // The version 4 database was kept as it was.
        let backup = Sqlite::open(root.join("work.v4.sqlite")).unwrap();
        let version: i32 = backup
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 4);
        let kept: i64 = backup
            .query_row(
                "SELECT COUNT(*) FROM connections WHERE body LIKE '%last_sync%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 3);
        let states: Vec<_> = db
            .connections()
            .unwrap()
            .into_iter()
            .map(|c| (c.id, c.zone, c.sync))
            .collect();
        let day = |d: &str| Timestamp::parse(&format!("2026-10-{d}T00:00:00Z"));
        assert_eq!(
            states,
            [
                (
                    "denied".to_string(),
                    Zone::External,
                    SyncState {
                        failures: 1,
                        error_kind: Some(ErrorKind::Auth),
                        error: Some("Credential rejected".into()),
                        ..SyncState::default()
                    }
                ),
                (
                    "ok".into(),
                    Zone::External,
                    SyncState::default().succeeded(day("12").unwrap())
                ),
                (
                    "old".into(),
                    Zone::External,
                    SyncState {
                        last_success: day("11"),
                        last_attempt: day("11"),
                        next_due: None,
                        failures: 1,
                        error_kind: Some(ErrorKind::Invalid),
                        error: Some("Failed".into()),
                        held_until: None,
                    }
                ),
            ]
        );
        let bodies: Vec<String> = db
            .sql
            .prepare("SELECT body FROM connections")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            bodies
                .iter()
                .all(|b| !b.contains("last_sync") && !b.contains("error")),
            "{bodies:?}"
        );
    }

    #[test]
    fn a_data_directory_keeps_its_mode() {
        use Mode::*;
        let temp = tempfile::tempdir().unwrap();
        let fresh = |name: &str| temp.path().join(name);
        // (first opened as, then as, opens)
        let cases = [
            (Hub, Hub, true),
            (Hub, Local, false),
            (Local, Local, true),
            // Even while empty: a local directory never becomes a hub.
            (Local, Hub, false),
        ];
        for (n, (first, then, opens)) in cases.into_iter().enumerate() {
            let root = fresh(&format!("case-{n}"));
            drop(Db::open_as(&root, first).unwrap());
            let opened = Db::open_as(&root, then);
            assert_eq!(opened.is_ok(), opens, "case {n}");
            if let Ok(db) = opened {
                assert_eq!(db.mode(), then);
            }
        }
        // From before modes were recorded: it may hold internal work, so it
        // becomes a hub only while empty, credentials included.
        for (n, (seeded, credential, hub)) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ]
        .into_iter()
        .enumerate()
        {
            let root = fresh(&format!("unrecorded-{n}"));
            let mut db = Db::open(&root).unwrap();
            if seeded {
                db.seed().unwrap();
            }
            if credential {
                write_secret(&root.join(SECRETS).join("left-behind"), "x").unwrap();
            }
            db.sql.execute("DELETE FROM settings", []).unwrap();
            drop(db);
            assert_eq!(Db::open_as(&root, Hub).is_ok(), hub, "case {n}");
        }
        // A hub's database with anything but external work in it, as if
        // changed behind its back, isn't opened.
        let external = connection("c0ffee00-0000-4000-8000-00000000000e", Zone::External);
        let internal_item = Item {
            zone: Zone::Internal,
            ..synced_item(&external.id, 1)
        };
        let tamperings: [(&str, &str, Vec<String>); 4] = [
            (
                "internal connection",
                "INSERT INTO connections VALUES('c1',?1)",
                vec![serde_json::to_string(&connection("c1", Zone::Internal)).unwrap()],
            ),
            (
                "watch list",
                "INSERT INTO watched(connection_id,login,body) VALUES(?1,'someone',?2)",
                vec![
                    external.id.clone(),
                    serde_json::to_string(&[internal_item]).unwrap(),
                ],
            ),
            (
                "unknown effort",
                "INSERT INTO items(id,body,effort_id) VALUES(?1,?2,'nowhere')",
                vec![
                    synced_item(&external.id, 2).id,
                    serde_json::to_string(&synced_item(&external.id, 2)).unwrap(),
                ],
            ),
            (
                "hub to pull from",
                "INSERT INTO hub(id,url) VALUES(?1,'https://elsewhere.example/')",
                vec![Uuid::new_v4().to_string()],
            ),
        ];
        for (what, sql, values) in tamperings {
            let root = fresh(&format!("tampered {what}"));
            let db = Db::open_as(&root, Hub).unwrap();
            db.save_connection(&external).unwrap();
            db.sql.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
            db.sql
                .execute(sql, rusqlite::params_from_iter(values))
                .unwrap();
            drop(db);
            let error = Db::open_as(&root, Hub).err().unwrap().to_string();
            assert!(error.contains(what), "{what}: {error}");
        }
    }

    /// Every table's rows, as text.
    fn dump(db: &Db) -> String {
        use rusqlite::types::ValueRef;
        let tables = db
            .rows("SELECT name,sql FROM sqlite_master WHERE type='table'")
            .unwrap();
        let mut out = String::new();
        for (table, _) in tables {
            let mut s = db.sql.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns = s.column_count();
            let rows = s
                .query_map([], |r| {
                    (0..columns)
                        .map(|n| {
                            Ok(match r.get_ref(n)? {
                                ValueRef::Text(t) | ValueRef::Blob(t) => {
                                    String::from_utf8_lossy(t).into_owned()
                                }
                                other => format!("{other:?}"),
                            })
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap();
            for row in rows {
                out += &row.unwrap().join(",");
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn a_hub_refuses_every_write_of_work_not_shown_external() {
        // Everything refused carries this, so a leak shows in the dump.
        const INTERNAL: &str = "INTERNAL";
        let temp = tempfile::tempdir().unwrap();
        let mut db = Db::open_as(&temp.path().join("hub"), Mode::Hub).unwrap();
        let external = connection("c0ffee00-0000-4000-8000-00000000000e", Zone::External);
        let internal = Connection {
            label: format!("{INTERNAL} label"),
            ..connection("c0ffee00-0000-4000-8000-000000000001", Zone::Internal)
        };
        db.save_connection(&external).unwrap();
        let ok = SyncState::default().succeeded(Timestamp::from_unix(1_791_763_200));
        let item = |c: &Connection, zone: Zone, n: u32| Item {
            id: format!("{}:errata:{n}", c.id),
            connection_id: Some(c.id.clone()),
            provider: "errata".into(),
            remote_id: n.to_string(),
            key: format!("K-{n}"),
            title: if zone == Zone::External && c.zone == Zone::External {
                format!("external {n}")
            } else {
                format!("{INTERNAL} {n}")
            },
            url: format!("https://example.com/{n}"),
            status: "open".into(),
            outcome: Outcome::Open,
            outcome_reason: String::new(),
            action: "next".into(),
            updated_at: String::new(),
            effort_id: None,
            abandoned_at: None,
            demo: false,
            zone,
            reasons: vec![],
            synced_at: None,
            stale: false,
            from_hub: false,
            bot: None,
        };
        let theirs = item(&external, Zone::External, 1);
        db.save_sync(&external.id, std::slice::from_ref(&theirs), None, &ok)
            .unwrap();
        let watch = |items: Vec<Item>| WatchList {
            connection_id: external.id.clone(),
            login: "someone".into(),
            items,
            error: None,
            last_success: None,
        };
        let manual = Item {
            connection_id: None,
            ..item(&internal, Zone::Internal, 9)
        };
        type Attempt<'a> = Box<dyn Fn(&mut Db) -> Result<()> + 'a>;
        let refused: Vec<(&str, Attempt)> = vec![
            (
                "an internal connection",
                Box::new(|db| db.save_connection(&internal)),
            ),
            (
                "a row that says internal",
                Box::new(|db| {
                    db.save_sync(
                        &external.id,
                        &[item(&external, Zone::Internal, 2)],
                        None,
                        &ok,
                    )
                }),
            ),
            (
                "another connection's work",
                Box::new(|db| {
                    db.save_sync(
                        &external.id,
                        &[item(&internal, Zone::External, 3)],
                        None,
                        &ok,
                    )
                }),
            ),
            (
                "an internal connection's sync",
                Box::new(|db| {
                    db.save_sync(
                        &internal.id,
                        &[item(&internal, Zone::Internal, 4)],
                        None,
                        &ok,
                    )
                }),
            ),
            (
                "a watch list with internal work",
                Box::new(|db| {
                    let list = watch(vec![item(&internal, Zone::Internal, 5)]);
                    db.save_sync(&external.id, &[], Some(&list), &ok)
                }),
            ),
            (
                "a manual source link",
                Box::new(|db| db.upsert_item(&manual)),
            ),
            ("the demo", Box::new(|db| db.seed())),
            (
                "an effort over unknown work",
                Box::new(|db| {
                    db.create_effort(EffortInput {
                        title: format!("{INTERNAL} effort"),
                        note: String::new(),
                        item_ids: vec![manual.id.clone()],
                        share_title: false,
                    })
                    .map(drop)
                }),
            ),
            (
                "linking unknown work",
                Box::new(|db| db.link(&manual.id, None)),
            ),
            (
                "watching from an internal connection",
                Box::new(|db| db.set_watch_user(&internal.id, Some(INTERNAL)).map(drop)),
            ),
            (
                "pulling from a hub",
                Box::new(|db| {
                    db.set_hub("https://INTERNAL.example/", "test-only-token")
                        .map(drop)
                }),
            ),
            (
                "saving a pull",
                Box::new(|db| db.save_hub_pull(&external.id, None, &ok)),
            ),
        ];
        for (what, attempt) in &refused {
            let error = attempt(&mut db)
                .err()
                .unwrap_or_else(|| panic!("{what} passed"));
            assert!(error.to_string().contains("hub"), "{what}: {error}");
        }
        let dump = dump(&db);
        assert!(!dump.contains(INTERNAL), "{dump}");
        // What is external passes.
        let list = watch(vec![item(&external, Zone::External, 6)]);
        db.save_sync(
            &external.id,
            std::slice::from_ref(&theirs),
            Some(&list),
            &ok,
        )
        .unwrap();
        let e = db
            .create_effort(EffortInput {
                title: "Ship it".into(),
                note: String::new(),
                item_ids: vec![theirs.id.clone()],
                share_title: false,
            })
            .unwrap();
        db.link(&theirs.id, None).unwrap();
        db.link(&theirs.id, Some(&e.id)).unwrap();
        db.share_title(&e.id, true).unwrap();
        db.set_watch_user(&external.id, Some("someone")).unwrap();
        drop(db);
        // And it opens again.
        Db::open_as(&temp.path().join("hub"), Mode::Hub).unwrap();
    }

    #[test]
    fn agent_runs_are_kept_apart_and_replaced_by_each_sync() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = Db::open_as(&temp.path().join("hub"), Mode::Hub).unwrap();
        let c = connection("c0ffee00-0000-4000-8000-00000000000e", Zone::External);
        db.save_connection(&c).unwrap();
        let at = Timestamp::from_unix(1_791_763_200);
        let ok = SyncState::default().succeeded(at);
        let runs = |id: &str, error: Option<&str>| AgentRuns {
            connection_id: id.into(),
            heartbeat: None,
            error: error.map(str::to_string),
            last_success: Some(at),
            stale: false,
            board: Some(crate::bot::BoardSummary {
                unreadable: 1,
                ..Default::default()
            }),
        };
        let save = |db: &mut Db, runs: &AgentRuns| {
            let found = Found {
                items: &[],
                watched: None,
                agents: Some(runs),
                dropped: &[],
            };
            db.save_found(&c.id, &found, &ok)
        };
        save(&mut db, &runs(&c.id, None)).unwrap();
        assert_eq!(db.agents().unwrap(), [runs(&c.id, None)]);
        // Never another connection's, on a hub or anywhere.
        let other = runs("c0ffee00-0000-4000-8000-000000000001", None);
        assert!(save(&mut db, &other).is_err());
        save(&mut db, &runs(&c.id, Some("down"))).unwrap();
        assert_eq!(db.agents().unwrap(), [runs(&c.id, Some("down"))]);
        // They aren't items.
        assert!(db.items().unwrap().is_empty());
        drop(db);
        // A hub opens again with them, and refuses a row of no external
        // connection.
        let db = Db::open_as(&temp.path().join("hub"), Mode::Hub).unwrap();
        db.sql
            .execute(
                "INSERT INTO agents(connection_id,body) VALUES('elsewhere','null')",
                [],
            )
            .unwrap();
        drop(db);
        let error = Db::open_as(&temp.path().join("hub"), Mode::Hub)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("agent runs"), "{error}");
        // Disconnecting removes them.
        let mut db = Db::open(&temp.path().join("local")).unwrap();
        db.save_connection(&c).unwrap();
        save(&mut db, &runs(&c.id, None)).unwrap();
        db.delete_connection(&c.id).unwrap();
        let rows: i64 = db
            .sql
            .query_row("SELECT COUNT(*) FROM agents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn items_a_connector_stops_tracking_are_removed_and_only_its_own() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = Db::open_as(&temp.path().join("hub"), Mode::Hub).unwrap();
        let c = connection("c0ffee00-0000-4000-8000-00000000000e", Zone::External);
        let other = connection("c0ffee00-0000-4000-8000-00000000000f", Zone::External);
        let ok = SyncState::default().succeeded(Timestamp::from_unix(1_791_763_200));
        for conn in [&c, &other] {
            db.save_connection(conn).unwrap();
            let items = [synced_item(&conn.id, 1), synced_item(&conn.id, 2)];
            db.save_sync(&conn.id, &items, None, &ok).unwrap();
        }
        let drop = |db: &mut Db, dropped: &[String]| {
            let found = Found {
                items: &[],
                watched: None,
                agents: None,
                dropped,
            };
            db.save_found(&c.id, &found, &ok)
        };
        // Never another connection's, even one whose ID starts like it.
        for theirs in [synced_item(&other.id, 1).id, format!("{}x:errata:1", c.id)] {
            assert!(drop(&mut db, &[theirs]).is_err());
        }
        drop(&mut db, &[synced_item(&c.id, 1).id]).unwrap();
        let mut left: Vec<_> = db.items().unwrap().into_iter().map(|i| i.id).collect();
        left.sort();
        let mut expected = vec![
            synced_item(&c.id, 2).id,
            synced_item(&other.id, 1).id,
            synced_item(&other.id, 2).id,
        ];
        expected.sort();
        assert_eq!(left, expected);
    }

    #[test]
    fn a_hub_set_again_replaces_the_last_whole() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = Db::open(&temp.path().join("companion")).unwrap();
        let secrets = db.root.join("secrets");
        let files = || {
            let mut names: Vec<_> = fs::read_dir(&secrets)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        assert!(db.hub().unwrap().is_none());
        assert!(db.unset_hub().is_err());
        let first = db.set_hub("https://hub.example/", "first-token").unwrap();
        assert_eq!(files(), [first.id.as_str()]);
        assert_eq!(db.secret(&first.id).unwrap(), "first-token");
        let at = Timestamp::from_unix(1_791_763_200);
        let ok = SyncState::default().succeeded(at);
        db.save_hub_pull(&first.id, Some(&hub::tests::export(2)), &ok)
            .unwrap();
        let read = db.hub().unwrap().unwrap();
        assert_eq!(
            (read.sync.last_success, read.snapshot.unwrap().items.len()),
            (Some(at), 2)
        );
        // Set again: the old token, state and export go.
        let second = db
            .set_hub("https://other-hub.example/", "second-token")
            .unwrap();
        assert_eq!(files(), [second.id.as_str()]);
        let read = db.hub().unwrap().unwrap();
        assert_eq!(read.url, "https://other-hub.example/");
        assert!(read.snapshot.is_none() && read.sync == SyncState::default());
        // A pull of the old one, finishing late, saves nothing.
        assert!(db
            .save_hub_pull(&first.id, Some(&hub::tests::export(1)), &ok)
            .is_err());
        assert!(db.hub().unwrap().unwrap().snapshot.is_none());
        db.unset_hub().unwrap();
        assert!(db.hub().unwrap().is_none());
        assert!(files().is_empty());
        let rows: i64 = db
            .sql
            .query_row("SELECT COUNT(*) FROM sync_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_files_are_private() {
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("db");
        // SQLite creates the write-ahead log and shared memory with the
        // database's mode, whatever the umask lets through.
        let db = Db::open(&root).unwrap();
        db.save_connection(&connection("a", Zone::Internal))
            .unwrap();
        for name in ["work.sqlite", "work.sqlite-wal", "work.sqlite-shm"] {
            assert_eq!(mode(&root.join(name)), 0o600, "{name}");
        }
        drop(db);
        // Files an earlier version left readable by others are made
        // private when the database is opened, and only SQLite's.
        let open = [
            "work.sqlite",
            "work.sqlite-wal",
            "work.sqlite-shm",
            "work.v3.sqlite",
        ];
        for name in open.iter().chain(&["notes.txt"]) {
            let path = root.join(name);
            if !path.exists() {
                fs::write(&path, "").unwrap();
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let db = Db::open(&root).unwrap();
        for name in open {
            assert_eq!(mode(&root.join(name)), 0o600, "{name}");
        }
        assert_eq!(mode(&root.join("notes.txt")), 0o644);
        assert_eq!(db.connections().unwrap().len(), 1);
        drop(db);
        // One that is a symbolic link could put the database anywhere,
        // readable by others, so the database isn't opened; and neither
        // what the link points to nor the link changes. Nor is one that
        // isn't a plain file.
        let outside = temp.path().join("outside");
        fs::write(&outside, "").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
        for name in [
            "work.sqlite",
            "work.sqlite-wal",
            "work.sqlite-shm",
            "work.v3.sqlite",
        ] {
            let path = root.join(name);
            if !path.exists() {
                fs::write(&path, "").unwrap();
            }
            let kept = temp.path().join("kept");
            fs::rename(&path, &kept).unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
            let error = format!("{:#}", Db::open(&root).err().unwrap());
            assert!(error.contains("symbolic link"), "{name}: {error}");
            assert_eq!(mode(&outside), 0o644, "{name}");
            assert!(fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink());
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            let error = format!("{:#}", Db::open(&root).err().unwrap());
            assert!(error.contains("regular file"), "{name}: {error}");
            fs::remove_dir(&path).unwrap();
            fs::rename(&kept, &path).unwrap();
        }
        assert_eq!(Db::open(&root).unwrap().connections().unwrap().len(), 1);
        let names = [
            ("work.sqlite", true),
            ("work.sqlite-journal", true),
            ("work.v12.sqlite-wal", true),
            ("work.sqlite.bak", false),
            ("other.sqlite", false),
            ("work.sqlite-wal.old", false),
        ];
        for (name, sqlite) in names {
            assert_eq!(is_sqlite_file(name), sqlite, "{name}");
        }
    }

    #[test]
    fn refuses_a_newer_schema() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("db");
        drop(Db::open(&root).unwrap());
        // A new, empty database has nothing to back up.
        assert!(!root.join("work.v0.sqlite").exists());
        Sqlite::open(root.join("work.sqlite"))
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let error = Db::open(&root).err().unwrap().to_string();
        assert!(error.contains("newer Workboard"), "{error}");
    }
}
