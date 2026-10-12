use crate::{model::*, outcome};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection as Sqlite, Transaction, TransactionBehavior};
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
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
const SCHEMA_VERSION: i32 = 1;

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

pub struct Db {
    pub sql: Sqlite,
    pub root: PathBuf,
}
impl Db {
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
        Ok(Snapshot {
            connections: self.connections()?,
            items: self.items()?,
            efforts: self.efforts()?,
        })
    }
    pub fn connections(&self) -> Result<Vec<Connection>> {
        let mut s = self
            .sql
            .prepare("SELECT body FROM connections ORDER BY id")?;
        let rows = s.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|v| Ok(serde_json::from_str(&v?)?)).collect()
    }
    pub fn connection(&self, id: &str) -> Result<Connection> {
        self.connections()?
            .into_iter()
            .find(|c| c.id == id)
            .context("Connection not found")
    }
    pub fn save_connection(&self, c: &Connection) -> Result<()> {
        self.sql.execute("INSERT INTO connections VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![c.id,serde_json::to_string(c)?])?;
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
    pub fn items(&self) -> Result<Vec<Item>> {
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
    pub fn sync_items(&mut self, items: &[Item]) -> Result<()> {
        let tx = self.sql.transaction()?;
        for i in items {
            tx.execute("INSERT INTO items(id,body) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![i.id,serde_json::to_string(i)?])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn efforts(&self) -> Result<Vec<Effort>> {
        let mut s = self
            .sql
            .prepare("SELECT id,title,note FROM efforts ORDER BY rowid")?;
        let rows = s.query_map([], |r| {
            Ok(Effort {
                id: r.get(0)?,
                title: r.get(1)?,
                note: r.get(2)?,
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
        };
        let tx = self.sql.transaction()?;
        tx.execute(
            "INSERT INTO efforts VALUES(?1,?2,?3)",
            params![e.id, e.title, e.note],
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
            })?;
        }
        self.create_effort(EffortInput {title:"Ship the bootc storage fix".into(),note:"Example effort linking upstream work, a RHEL backport and an advisory. All items are illustrative.".into(),item_ids:(0..4).map(|n|format!("demo-{n}")).collect()})?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
