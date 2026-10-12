use crate::{model::*, outcome};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection as Sqlite, Transaction, TransactionBehavior};
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

/// The schema version this build writes, kept in SQLite's `user_version`.
/// Version 0 is the prototype's schema, whose items only had a `terminal` flag.
const SCHEMA_VERSION: i32 = 5;

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
    let target = path
        .to_str()
        .context("The data directory path is not UTF-8")?;
    // Unlike a file copy, VACUUM INTO includes changes still in the WAL.
    sql.execute("VACUUM INTO ?1", [target])
        .with_context(|| format!("Cannot back up the database to {target} before upgrading it"))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
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
    }
    Ok(serde_json::to_string(&v)?)
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
}
impl Db {
    /// The SQL connection, for tests that set up or break what they need.
    #[cfg(test)]
    pub fn sql(&self) -> &Sqlite {
        &self.sql
    }
    pub fn open(root: &Path) -> Result<Self> {
        private_dir(root)?;
        private_dir(&root.join("secrets"))?;
        let mut sql = Sqlite::open(root.join("work.sqlite"))?;
        // The prototype's schema; migrate() builds on it.
        sql.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;
          CREATE TABLE IF NOT EXISTS connections(id TEXT PRIMARY KEY, body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS efforts(id TEXT PRIMARY KEY, title TEXT NOT NULL, note TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS items(id TEXT PRIMARY KEY, body TEXT NOT NULL, effort_id TEXT REFERENCES efforts(id));")?;
        migrate(&mut sql, root)?;
        #[cfg(unix)]
        fs::set_permissions(root.join("work.sqlite"), fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            sql,
            root: root.to_path_buf(),
        })
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
        self.sql.execute("INSERT INTO connections VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![c.id,connection_body(c)?])?;
        Ok(())
    }
    pub fn secret(&self, id: &str) -> Result<String> {
        Uuid::parse_str(id)?;
        let value = fs::read_to_string(self.root.join("secrets").join(id))?;
        let value = value.trim().to_string();
        if value.is_empty() {
            bail!("Missing credential")
        }
        Ok(value)
    }
    /// Write `value` as connection `id`'s next credential, beside the one
    /// in use, for [`Db::install_secret`] to put in place.
    pub fn stage_secret(&self, id: &str, value: &str) -> Result<PathBuf> {
        Uuid::parse_str(id)?;
        let staged = self.root.join("secrets").join(format!("{id}.new"));
        match fs::remove_file(&staged) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        write_secret(&staged, value)?;
        Ok(staged)
    }
    /// Replace connection `id`'s credential with the one staged at
    /// `staged`, all at once, so a sync reads the old one or the new one.
    pub fn install_secret(&self, id: &str, staged: &Path) -> Result<()> {
        Uuid::parse_str(id)?;
        fs::rename(staged, self.root.join("secrets").join(id))?;
        Ok(())
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
    pub fn upsert_item(&self, i: &Item) -> Result<()> {
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
    /// which replaces the last such list, and the connection's sync state.
    /// Nothing is saved before, so a sync that stops halfway leaves the last
    /// good data and state, and the next one fetches everything again.
    pub fn save_sync(
        &mut self,
        id: &str,
        items: &[Item],
        watched: Option<&WatchList>,
        state: &SyncState,
    ) -> Result<()> {
        let tx = self.sql.transaction()?;
        check_connected(&tx, id)?;
        upsert_synced(&tx, items)?;
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
        write_sync_state(&tx, id, state)?;
        tx.commit()?;
        Ok(())
    }
    /// Save the sync state of connection `id` after a sync that failed,
    /// leaving its items as they were.
    pub fn save_sync_state(&mut self, id: &str, state: &SyncState) -> Result<()> {
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
    /// Watch another user from connection `id`, or stop. What was found for
    /// whoever it watched before is dropped.
    pub fn set_watch_user(&mut self, id: &str, login: Option<&str>) -> Result<Connection> {
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
        tx.execute("DELETE FROM sync_state WHERE connection_id=?1", [id])?;
        tx.execute("DELETE FROM connections WHERE id=?1", [id])?;
        tx.commit()?;
        match fs::remove_file(self.root.join("secrets").join(id)) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        };
        Ok(())
    }
    pub fn seed(&mut self) -> Result<()> {
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
        for _ in 0..2 {
            // Opening again must not migrate twice.
            let db = Db::open(&root).unwrap();
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
        db.delete_connection(&c.id).unwrap();
        let rows: i64 = db
            .sql
            .query_row("SELECT COUNT(*) FROM sync_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
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
