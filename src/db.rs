use crate::model::*;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection as Sqlite};
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
        item.terminal = true;
        db.sync_items(&[item]).unwrap();
        let item = db
            .items()
            .unwrap()
            .into_iter()
            .find(|i| i.id == "demo-0")
            .unwrap();
        assert_eq!(item.effort_id, effort);
        assert!(item.terminal);
    }
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

pub struct Db {
    pub sql: Sqlite,
    pub root: PathBuf,
}
impl Db {
    pub fn open(root: &Path) -> Result<Self> {
        private_dir(root)?;
        private_dir(&root.join("secrets"))?;
        let sql = Sqlite::open(root.join("work.sqlite"))?;
        sql.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;
          CREATE TABLE IF NOT EXISTS connections(id TEXT PRIMARY KEY, body TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS efforts(id TEXT PRIMARY KEY, title TEXT NOT NULL, note TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS items(id TEXT PRIMARY KEY, body TEXT NOT NULL, effort_id TEXT REFERENCES efforts(id));")?;
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
            .prepare("SELECT body,effort_id FROM items ORDER BY id")?;
        let rows = s.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        rows.map(|v| {
            let (body, effort) = v?;
            let mut i: Item = serde_json::from_str(&body)?;
            i.effort_id = effort;
            Ok(i)
        })
        .collect()
    }
    pub fn upsert_item(&self, i: &Item) -> Result<()> {
        self.sql.execute(
            "INSERT INTO items VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET body=excluded.body",
            params![i.id, serde_json::to_string(i)?, i.effort_id],
        )?;
        Ok(())
    }
    pub fn sync_items(&mut self, items: &[Item]) -> Result<()> {
        let tx = self.sql.transaction()?;
        for i in items {
            tx.execute("INSERT INTO items VALUES(?1,?2,NULL) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![i.id,serde_json::to_string(i)?])?;
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
                false,
            ),
            (
                "github",
                "upstream",
                "Upstream storage fix",
                "Merged",
                "",
                "https://github.com/bootc-dev/bootc",
                true,
            ),
            (
                "gitlab-internal",
                "rhel-backport",
                "Backport the fix to RHEL",
                "Awaiting review",
                "waiting",
                "https://gitlab.example.invalid/rhel/bootc/-/merge_requests/1",
                false,
            ),
            (
                "errata",
                "advisory",
                "Publish the RHEL advisory",
                "Needs validation",
                "next",
                "https://errata.example.invalid/advisory/1",
                false,
            ),
            (
                "gitlab-public",
                "centos-review",
                "Review the CentOS Stream change",
                "Open",
                "review",
                "https://gitlab.com/redhat/centos-stream",
                false,
            ),
            (
                "fedora",
                "fedora-ci",
                "Investigate the Fedora test failure",
                "Checks failed",
                "next",
                "https://forge.fedoraproject.org",
                false,
            ),
        ];
        for (n, (provider, key, title, status, action, url, terminal)) in
            examples.iter().enumerate()
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
                terminal: *terminal,
                action: (*action).into(),
                updated_at: "2026-10-06T20:00:00Z".into(),
                effort_id: None,
                demo: true,
            })?;
        }
        self.create_effort(EffortInput {title:"Ship the bootc storage fix".into(),note:"Example effort linking upstream work, a RHEL backport and an advisory. All items are illustrative.".into(),item_ids:(0..4).map(|n|format!("demo-{n}")).collect()})?;
        Ok(())
    }
}
