//! Which Mine items to announce. Each is announced once, by its ID, also
//! across restarts: the IDs already seen are kept in a small file of this
//! app's own, which holds nothing but them and when each was last seen. It
//! is bookkeeping for notifications, not work state, which stays in the
//! service. It is written off the main thread, and only when it changed.

use crate::{api::MineRow, model};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// How long an ID is remembered after it was last in the Mine view. An
/// item that comes back after being away longer is new again, and is
/// announced once more; and the file doesn't grow forever.
const RETENTION_SECS: i64 = 30 * 86_400;
/// When an ID was last seen is kept to about a day: refreshed once it is
/// this old, so the file isn't rewritten on every poll.
const REFRESH_SECS: i64 = 86_400;
/// Above this many new items at once, one notification says how many.
pub const MAX_SINGLE: usize = 3;
/// The notification ID of that summary, replaced by the next one.
pub const SUMMARY_ID: &str = "mine-summary";
/// The version of the file's format.
const VERSION: u32 = 1;

/// The Mine items seen, by ID, with when each was last seen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    version: u32,
    seen: BTreeMap<String, i64>,
}

impl Seen {
    /// What was seen before, or None if nothing was ever recorded at `path`.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("Cannot read {}", path.display())),
        };
        let seen: Self = serde_json::from_str(&text)
            .with_context(|| format!("{} doesn't parse", path.display()))?;
        Ok(Some(seen))
    }

    /// Write it to `path` whole: a crash leaves the old file or the new one.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .context("No directory for the notification state")?;
        std::fs::create_dir_all(dir).with_context(|| format!("Cannot create {}", dir.display()))?;
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, serde_json::to_vec(self)?)
            .with_context(|| format!("Cannot write {}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("Cannot replace {}", path.display()))
    }

    #[cfg(test)]
    pub fn contains(&self, id: &str) -> bool {
        self.seen.contains_key(id)
    }
}

/// What [`update`] found.
pub struct Update<'a> {
    /// What is seen after it.
    pub seen: Seen,
    /// What to announce.
    pub new: Vec<&'a MineRow>,
    /// Whether `seen` differs from before, and needs saving.
    pub changed: bool,
}

/// What the Mine view `rows` has that `seen` didn't, as of `now`.
///
/// The rule: an ID is new unless it was seen within [`RETENTION_SECS`], so
/// an item is announced once, and again only if it comes back after being
/// away longer than that. IDs seen longer ago are forgotten first, so one
/// that comes back on the very poll that forgets it is new too. With
/// nothing seen before, as on the first run, everything is recorded and
/// nothing is new, so the first start doesn't announce everything at once.
pub fn update<'a>(seen: Option<&Seen>, rows: &'a [MineRow], now: i64) -> Update<'a> {
    let before = seen.map(|s| s.seen.clone()).unwrap_or_default();
    let mut kept: BTreeMap<String, i64> = before
        .iter()
        .filter(|(_, at)| now - **at <= RETENTION_SECS)
        .map(|(id, at)| (id.clone(), *at))
        .collect();
    let new = match seen {
        None => vec![],
        Some(_) => rows.iter().filter(|r| !kept.contains_key(&r.id)).collect(),
    };
    for row in rows {
        let at = kept.entry(row.id.clone()).or_insert(now);
        if now - *at >= REFRESH_SECS {
            *at = now;
        }
    }
    let changed = seen.is_none() || kept != before;
    Update {
        seen: Seen {
            version: VERSION,
            seen: kept,
        },
        new,
        changed,
    }
}

/// Saves what was seen off the main thread, in order: a thread of its own
/// writes the latest of what it was given. [`Writer::finish`] waits for
/// what is still queued when the app quits.
pub struct Writer {
    send: std::sync::Mutex<Option<std::sync::mpsc::Sender<Seen>>>,
    /// Says the thread finished: everything given to it is saved.
    done: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Writer {
    pub fn spawn(path: PathBuf) -> Self {
        Self::spawn_with(move |seen| seen.save(&path))
    }

    /// A writer that saves with `save`.
    fn spawn_with(save: impl Fn(&Seen) -> Result<()> + Send + 'static) -> Self {
        let (send, receive) = std::sync::mpsc::channel::<Seen>();
        let (finished, done) = std::sync::mpsc::channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("notified".into())
            .spawn(move || {
                while let Ok(mut seen) = receive.recv() {
                    // Only the latest of those waiting matters.
                    while let Ok(newer) = receive.try_recv() {
                        seen = newer;
                    }
                    if let Err(e) = save(&seen) {
                        eprintln!("Cannot keep which items were announced: {e:#}");
                    }
                }
                let _ = finished.send(());
            });
        if let Err(e) = spawned {
            eprintln!("Cannot keep which items were announced: {e}");
        }
        Self {
            send: std::sync::Mutex::new(Some(send)),
            done: std::sync::Mutex::new(Some(done)),
        }
    }

    pub fn save(&self, seen: Seen) {
        if let Some(send) = self.send.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            // Fails only if the thread didn't start, which was said then.
            let _ = send.send(seen);
        }
    }

    /// Save what is still queued, waiting at most `timeout`: when the app
    /// quits, after its main loop, so what it announced last isn't
    /// announced again at the next start.
    pub fn finish(&self, timeout: std::time::Duration) {
        // Without a sender, the thread ends once it saved the rest.
        drop(self.send.lock().unwrap_or_else(|e| e.into_inner()).take());
        if let Some(done) = self.done.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if done.recv_timeout(timeout).is_err() {
                eprintln!("Gave up waiting to keep which items were announced");
            }
        }
    }
}

/// A desktop notification to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Sending another with the same ID replaces it.
    pub id: String,
    pub title: String,
    pub body: String,
    /// What its button opens, if it is about one item.
    pub url: Option<String>,
}

/// The notifications for `new` Mine items: one each, or one that counts
/// them when there are many.
pub fn notes(new: &[&MineRow]) -> Vec<Note> {
    if new.len() > MAX_SINGLE {
        let titles: Vec<_> = new
            .iter()
            .take(MAX_SINGLE)
            .map(|r| r.title.as_str())
            .collect();
        return vec![Note {
            id: SUMMARY_ID.into(),
            title: format!("{} new items wait on you", new.len()),
            body: format!("{}, and more", titles.join(", ")),
            url: None,
        }];
    }
    new.iter()
        .map(|r| {
            let why = r
                .waits_on
                .first()
                .and_then(|w| w.reasons.first())
                .map_or("Waits on you".into(), model::reason_label);
            let (source, _) = model::provider(&r.provider);
            Note {
                id: format!("mine-{}", r.id),
                title: format!("{why}: {}", r.title),
                body: format!("{source} {} · {}", r.key, r.status),
                url: Some(r.url.clone()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Reason, WaitsOn};

    fn row(id: &str) -> MineRow {
        MineRow {
            id: id.into(),
            provider: "github".into(),
            key: format!("org/repo#{id}"),
            title: format!("Change {id}"),
            url: format!("https://github.com/org/repo/pull/{id}"),
            status: "Open".into(),
            waits_on: vec![WaitsOn {
                account: "octocat".into(),
                reasons: vec![Reason::ReviewRequested, Reason::Assigned],
                ..WaitsOn::default()
            }],
            ..MineRow::default()
        }
    }

    fn ids(rows: &[&MineRow]) -> Vec<String> {
        rows.iter().map(|r| r.id.clone()).collect()
    }

    #[test]
    fn announces_each_item_once_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/notified.json");
        // The first run records what is there, and announces nothing.
        assert_eq!(Seen::load(&path).unwrap(), None);
        let rows = vec![row("1"), row("2")];
        let first = update(None, &rows, 100);
        assert!(first.new.is_empty() && first.changed);
        first.seen.save(&path).unwrap();

        // After a restart, only what is new.
        let seen = Seen::load(&path).unwrap().unwrap();
        let rows = vec![row("1"), row("2"), row("3")];
        let found = update(Some(&seen), &rows, 200);
        assert_eq!(ids(&found.new), ["3"]);
        assert!(found.changed);
        found.seen.save(&path).unwrap();

        // Again, nothing: 3 was announced, even though 2 is gone. Nothing
        // to save either, until a day later.
        let seen = Seen::load(&path).unwrap().unwrap();
        let rows = vec![row("1"), row("3")];
        let found = update(Some(&seen), &rows, 300);
        assert!(found.new.is_empty() && !found.changed);
        let found = update(Some(&seen), &rows, 100 + REFRESH_SECS);
        assert!(found.new.is_empty() && found.changed);
        assert!(found.seen.contains("2"));
    }

    #[test]
    fn an_item_back_after_a_long_absence_is_announced_again() {
        let rows = [row("1"), row("2")];
        let seen = update(None, &rows, 0).seen;
        // 2 leaves; 1 stays, seen daily.
        let mut seen = seen;
        let mut now = 0;
        while now < RETENTION_SECS {
            now += REFRESH_SECS;
            seen = update(Some(&seen), &rows[..1], now).seen;
        }
        // Just within the retention, 2 is still known.
        let within = update(Some(&seen), &rows, RETENTION_SECS);
        assert!(within.new.is_empty());
        // Forgotten and back on the same poll: announced, once.
        let back = update(Some(&seen), &rows, RETENTION_SECS + 1);
        assert_eq!(ids(&back.new), ["2"]);
        assert!(back.seen.contains("1"));
        let again = update(Some(&back.seen), &rows, RETENTION_SECS + 2);
        assert!(again.new.is_empty());
    }

    /// What was queued when the app quits is saved before it exits, even
    /// when saving is slow.
    #[test]
    fn finishing_saves_what_is_queued() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/notified.json");
        let target = path.clone();
        let writer = Writer::spawn_with(move |seen| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            seen.save(&target)
        });
        let first = update(None, &[row("1")], 1).seen;
        let last = update(Some(&first), &[row("1"), row("2")], 2).seen;
        writer.save(first);
        writer.save(last.clone());
        writer.finish(std::time::Duration::from_secs(10));
        assert_eq!(Seen::load(&path).unwrap(), Some(last));
        // Nothing is taken after it finished.
        writer.save(update(None, &[row("3")], 3).seen);
    }

    #[test]
    fn the_writer_saves_the_latest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/notified.json");
        let writer = Writer::spawn(path.clone());
        let rows = [row("1")];
        let older = update(None, &rows, 1).seen;
        let newer = update(Some(&older), &[row("1"), row("2")], 2).seen;
        writer.save(older);
        writer.save(newer.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while Seen::load(&path).ok().flatten().as_ref() != Some(&newer) {
            assert!(std::time::Instant::now() < deadline, "not saved");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn a_broken_state_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notified.json");
        std::fs::write(&path, "not json").unwrap();
        let error = Seen::load(&path).unwrap_err().to_string();
        assert!(error.contains("doesn't parse"), "{error}");
    }

    #[test]
    fn one_note_each_or_a_summary() {
        let rows: Vec<MineRow> = (1..=5).map(|i| row(&i.to_string())).collect();
        let few: Vec<&MineRow> = rows.iter().take(MAX_SINGLE).collect();
        let notes_of_few = notes(&few);
        assert_eq!(notes_of_few.len(), MAX_SINGLE);
        assert_eq!(
            notes_of_few[0],
            Note {
                id: "mine-1".into(),
                title: "Review requested: Change 1".into(),
                body: "GitHub org/repo#1 · Open".into(),
                url: Some("https://github.com/org/repo/pull/1".into()),
            }
        );
        let all: Vec<&MineRow> = rows.iter().collect();
        let summary = notes(&all);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].id, SUMMARY_ID);
        assert_eq!(summary[0].title, "5 new items wait on you");
        assert_eq!(summary[0].body, "Change 1, Change 2, Change 3, and more");
    }
}
