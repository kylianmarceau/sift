//! `state.db`: what the user opened (recents, ranking boosts), what they
//! pinned (favorites) and, when enabled, clipboard history.
//!
//! Everything stays in one local SQLite file. Times are Unix seconds and
//! are passed in, so tests control the clock.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};
use sift_core::{Action, ResultItem, ResultKind};

use crate::StateError;

const SCHEMA_VERSION: i32 = 2;

const SCHEMA: &str = "
CREATE TABLE usage (
    id        TEXT PRIMARY KEY,
    kind      TEXT NOT NULL,
    title     TEXT NOT NULL,
    subtitle  TEXT,
    action    TEXT NOT NULL,
    count     INTEGER NOT NULL,
    last_used INTEGER NOT NULL
);
CREATE INDEX usage_last_used ON usage (last_used);
CREATE TABLE favorites (
    id       TEXT PRIMARY KEY,
    kind     TEXT NOT NULL,
    title    TEXT NOT NULL,
    subtitle TEXT,
    action   TEXT NOT NULL,
    added_at INTEGER NOT NULL
);
CREATE TABLE clipboard (
    id        INTEGER PRIMARY KEY,
    text      TEXT NOT NULL UNIQUE,
    copied_at INTEGER NOT NULL,
    pinned_at INTEGER
);
CREATE INDEX clipboard_copied_at ON clipboard (copied_at);
";

/// Upgrades from each older schema version, in order: `MIGRATIONS[0]`
/// takes version 1 to 2, and so on.
const MIGRATIONS: &[&str] = &[
    // 2: pinned clipboard entries.
    "ALTER TABLE clipboard ADD COLUMN pinned_at INTEGER;",
];

/// Usage rows kept; older ones are dropped.
pub const MAX_USAGE_ROWS: usize = 500;

/// Clipboard entries larger than this are not recorded (a pasted log or
/// file is not something to search later, and it would bloat the file).
pub const MAX_CLIP_BYTES: usize = 64 * 1024;

/// Boost added for favorites, above any usage boost.
pub const FAVORITE_BOOST: u8 = 8;

const DAY: i64 = 24 * 60 * 60;

/// One clipboard history entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipEntry {
    pub id: i64,
    pub text: Arc<str>,
    pub copied_at: i64,
    /// Pinned entries are listed first and kept until unpinned or deleted.
    pub pinned: bool,
}

pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?, Some(path))
    }

    /// Opens the database at `path`; if it is damaged (cannot be opened,
    /// or fails `quick_check`), moves it aside as
    /// `state.db.damaged-<unix time>` (with its journal files) and starts a
    /// new one. Returns where the damaged copy went, if anywhere. A schema
    /// from a newer version is not damage and is returned as an error.
    pub fn open_recovering(path: &Path) -> Result<(Self, Option<PathBuf>), StateError> {
        let problem = match Self::open(path) {
            Ok(db) => match db.quick_check() {
                Ok(true) => return Ok((db, None)),
                Ok(false) => "quick_check failed".to_owned(),
                Err(error) => error.to_string(),
            },
            Err(StateError::Sqlite(error)) if is_corruption(&error) => error.to_string(),
            Err(error) => return Err(error),
        };
        tracing::error!(%problem, path = %path.display(), "state.db is damaged; starting a new one");
        let stamp = crate::now();
        let mut moved = path.as_os_str().to_owned();
        moved.push(format!(".damaged-{stamp}"));
        let moved = PathBuf::from(moved);
        for suffix in ["", "-wal", "-shm"] {
            let mut from = path.as_os_str().to_owned();
            from.push(suffix);
            let mut to = moved.as_os_str().to_owned();
            to.push(suffix);
            if Path::new(&from).exists() {
                std::fs::rename(&from, &to)?;
            }
        }
        Ok((Self::open(path)?, Some(moved)))
    }

    fn quick_check(&self) -> Result<bool, StateError> {
        let result: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        Ok(result == "ok")
    }

    pub fn open_in_memory() -> Result<Self, StateError> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, path: Option<&Path>) -> Result<Self, StateError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                conn.execute_batch(SCHEMA)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            older if older < SCHEMA_VERSION => migrate(&conn, older, path)?,
            newer => return Err(StateError::NewerSchema(newer)),
        }
        Ok(Self { conn })
    }

    /// Records that `item` was activated. Calculator results and clipboard
    /// entries are not recorded: they are not things to reopen, and
    /// clipboard text may be private.
    pub fn record_use(&self, item: &ResultItem, now: i64) -> Result<(), StateError> {
        if !is_recordable(item) {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO usage (id, kind, title, subtitle, action, count, last_used)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
             ON CONFLICT (id) DO UPDATE SET
                kind = excluded.kind, title = excluded.title,
                subtitle = excluded.subtitle, action = excluded.action,
                count = count + 1, last_used = excluded.last_used",
            params![
                &*item.id,
                item.kind.label(),
                &*item.title,
                item.subtitle.as_deref(),
                item.action.encode(),
                now
            ],
        )?;
        self.conn.execute(
            "DELETE FROM usage WHERE id NOT IN
                (SELECT id FROM usage ORDER BY last_used DESC LIMIT ?1)",
            params![MAX_USAGE_ROWS as i64],
        )?;
        Ok(())
    }

    /// The most recently used items, newest first. Rows whose action no
    /// longer decodes (an unknown built-in command) are skipped.
    pub fn recent(
        &self,
        limit: usize,
        commands: &[&'static str],
    ) -> Result<Vec<ResultItem>, StateError> {
        self.items(
            "SELECT id, kind, title, subtitle, action FROM usage
             ORDER BY last_used DESC LIMIT ?1",
            limit,
            commands,
        )
    }

    /// Favorites in the order they were added.
    pub fn favorites(&self, commands: &[&'static str]) -> Result<Vec<ResultItem>, StateError> {
        self.items(
            "SELECT id, kind, title, subtitle, action FROM favorites
             ORDER BY added_at, rowid LIMIT ?1",
            usize::MAX >> 1,
            commands,
        )
    }

    fn items(
        &self,
        sql: &str,
        limit: usize,
        commands: &[&'static str],
    ) -> Result<Vec<ResultItem>, StateError> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (id, kind, title, subtitle, action) = row?;
            let (Some(kind), Some(action)) = (
                ResultKind::from_label(&kind),
                Action::decode(&action, commands),
            ) else {
                continue;
            };
            items.push(ResultItem {
                id: id.into(),
                kind,
                title: title.into(),
                subtitle: subtitle.map(Into::into),
                score: 0,
                action,
            });
        }
        Ok(items)
    }

    /// Per-id ranking boosts: frequent and recent use gives up to 7,
    /// favorites add [`FAVORITE_BOOST`]. Only used to order results
    /// within the same match tier.
    pub fn usage_boosts(&self, now: i64) -> Result<HashMap<Arc<str>, u8>, StateError> {
        let mut boosts = HashMap::new();
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, count, last_used FROM usage")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (id, count, last_used) = row?;
            let boost = frecency(count, now - last_used);
            if boost > 0 {
                boosts.insert(Arc::<str>::from(id), boost);
            }
        }
        let mut stmt = self.conn.prepare_cached("SELECT id FROM favorites")?;
        for id in stmt.query_map([], |r| r.get::<_, String>(0))? {
            let entry = boosts.entry(Arc::<str>::from(id?)).or_insert(0);
            *entry = entry.saturating_add(FAVORITE_BOOST);
        }
        Ok(boosts)
    }

    pub fn is_favorite(&self, id: &str) -> Result<bool, StateError> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM favorites WHERE id = ?1", [id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Adds or removes `item` from favorites; returns whether it is now a
    /// favorite. Items that cannot be recorded (calculator results,
    /// clipboard entries) are never favorites.
    pub fn toggle_favorite(&self, item: &ResultItem, now: i64) -> Result<bool, StateError> {
        if !is_recordable(item) {
            return Ok(false);
        }
        let removed = self
            .conn
            .execute("DELETE FROM favorites WHERE id = ?1", [&*item.id])?;
        if removed > 0 {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO favorites (id, kind, title, subtitle, action, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &*item.id,
                item.kind.label(),
                &*item.title,
                item.subtitle.as_deref(),
                item.action.encode(),
                now
            ],
        )?;
        Ok(true)
    }

    /// Removes `id` from recents and favorites.
    pub fn forget(&self, id: &str) -> Result<(), StateError> {
        self.conn.execute("DELETE FROM usage WHERE id = ?1", [id])?;
        self.conn
            .execute("DELETE FROM favorites WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Adds `text` to clipboard history, keeping the newest `max_items`
    /// unpinned entries (pinned ones do not count and are always kept).
    /// Copying something already in the history moves it to the top.
    /// Blank and oversized text is ignored; returns whether it was kept.
    pub fn clipboard_add(
        &self,
        text: &str,
        now: i64,
        max_items: usize,
    ) -> Result<bool, StateError> {
        if text.trim().is_empty() || text.len() > MAX_CLIP_BYTES {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO clipboard (text, copied_at) VALUES (?1, ?2)
             ON CONFLICT (text) DO UPDATE SET copied_at = excluded.copied_at",
            params![text, now],
        )?;
        self.conn.execute(
            "DELETE FROM clipboard WHERE pinned_at IS NULL AND id NOT IN
                (SELECT id FROM clipboard WHERE pinned_at IS NULL
                 ORDER BY copied_at DESC, id DESC LIMIT ?1)",
            params![max_items as i64],
        )?;
        Ok(true)
    }

    /// Clipboard history: pinned entries in the order they were pinned,
    /// then the newest `limit` others, newest first.
    pub fn clipboard(&self, limit: usize) -> Result<Vec<ClipEntry>, StateError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, text, copied_at, pinned_at FROM clipboard
             WHERE pinned_at IS NOT NULL OR id IN
                (SELECT id FROM clipboard WHERE pinned_at IS NULL
                 ORDER BY copied_at DESC, id DESC LIMIT ?1)
             ORDER BY pinned_at IS NULL, pinned_at, copied_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(ClipEntry {
                id: r.get(0)?,
                text: r.get::<_, String>(1)?.into(),
                copied_at: r.get(2)?,
                pinned: r.get::<_, Option<i64>>(3)?.is_some(),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Pins or unpins entry `id`; returns whether it exists.
    pub fn clipboard_set_pinned(
        &self,
        id: i64,
        pinned: bool,
        now: i64,
    ) -> Result<bool, StateError> {
        let changed = self.conn.execute(
            "UPDATE clipboard SET pinned_at = ?2 WHERE id = ?1",
            params![id, pinned.then_some(now)],
        )?;
        Ok(changed > 0)
    }

    /// Deletes entry `id`, pinned or not; returns whether it existed.
    pub fn clipboard_delete(&self, id: i64) -> Result<bool, StateError> {
        Ok(self
            .conn
            .execute("DELETE FROM clipboard WHERE id = ?1", [id])?
            > 0)
    }

    /// Deletes the unpinned entries; pinned ones stay until unpinned.
    pub fn clipboard_clear(&self) -> Result<(), StateError> {
        self.conn
            .execute("DELETE FROM clipboard WHERE pinned_at IS NULL", [])?;
        Ok(())
    }
}

fn is_corruption(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    )
}

/// Upgrades the schema from `from` to [`SCHEMA_VERSION`] in one
/// transaction. A file on disk is first copied to
/// `state.db.schema-<from>`, since an older Sift refuses the upgraded file
/// (see [`StateError::NewerSchema`]); going back to it means putting the
/// copy back.
fn migrate(conn: &Connection, from: i32, path: Option<&Path>) -> Result<(), StateError> {
    if let Some(path) = path {
        let mut backup = path.as_os_str().to_owned();
        backup.push(format!(".schema-{from}"));
        let backup = PathBuf::from(backup);
        if !backup.exists() {
            // VACUUM INTO writes a consistent copy, including what is still
            // in the write-ahead log.
            conn.execute("VACUUM INTO ?1", [backup.to_string_lossy()])?;
        }
        tracing::info!(from, to = SCHEMA_VERSION, backup = %backup.display(), "upgrading state.db");
    }
    let tx = conn.unchecked_transaction()?;
    for step in &MIGRATIONS[(from - 1).max(0) as usize..] {
        tx.execute_batch(step)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn is_recordable(item: &ResultItem) -> bool {
    !matches!(
        item.kind,
        ResultKind::Calculator | ResultKind::Clipboard | ResultKind::Window | ResultKind::Workspace
    )
}

/// 0–7 from a use count and the time since last use: log2 of the count,
/// weighted down as the last use gets older.
fn frecency(count: i64, age: i64) -> u8 {
    let weight = match age {
        a if a < DAY => 1.0,
        a if a < 7 * DAY => 0.7,
        a if a < 30 * DAY => 0.5,
        _ => 0.25,
    };
    let weighted = count.max(0) as f64 * weight;
    if weighted < 1.0 {
        return if count > 0 { 1 } else { 0 };
    }
    (1.0 + weighted.log2()).clamp(1.0, 7.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: ResultKind, action: Action) -> ResultItem {
        ResultItem {
            id: id.into(),
            kind,
            title: format!("{id} title").into(),
            subtitle: Some("sub".into()),
            score: 123,
            action,
        }
    }

    fn app(id: &str) -> ResultItem {
        item(
            id,
            ResultKind::Application,
            Action::OpenApplication(format!("/Applications/{id}.app").into()),
        )
    }

    #[test]
    fn recents_are_newest_first_and_round_trip() {
        let db = StateDb::open_in_memory().unwrap();
        db.record_use(&app("a"), 10).unwrap();
        db.record_use(&app("b"), 20).unwrap();
        db.record_use(&app("a"), 30).unwrap();
        let recent = db.recent(10, &[]).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(&*recent[0].id, "a");
        assert_eq!(recent[0].kind, ResultKind::Application);
        assert_eq!(recent[0].subtitle.as_deref(), Some("sub"));
        assert_eq!(
            recent[0].action,
            Action::OpenApplication("/Applications/a.app".into())
        );
    }

    #[test]
    fn calculator_and_clipboard_results_are_never_recorded() {
        let db = StateDb::open_in_memory().unwrap();
        let calc = item("calc", ResultKind::Calculator, Action::CopyText("4".into()));
        let clip = item(
            "clip:1",
            ResultKind::Clipboard,
            Action::CopyText("pw".into()),
        );
        db.record_use(&calc, 1).unwrap();
        db.record_use(&clip, 1).unwrap();
        assert!(!db.toggle_favorite(&clip, 1).unwrap());
        assert!(db.recent(10, &[]).unwrap().is_empty());
        assert!(db.favorites(&[]).unwrap().is_empty());
    }

    #[test]
    fn unknown_commands_in_stored_rows_are_skipped() {
        let db = StateDb::open_in_memory().unwrap();
        db.record_use(
            &item("cmd", ResultKind::Command, Action::Run("settings")),
            1,
        )
        .unwrap();
        assert!(db.recent(10, &[]).unwrap().is_empty());
        assert_eq!(db.recent(10, &["settings"]).unwrap().len(), 1);
    }

    #[test]
    fn usage_rows_are_pruned() {
        let db = StateDb::open_in_memory().unwrap();
        for i in 0..MAX_USAGE_ROWS + 20 {
            db.record_use(&app(&format!("app{i}")), i as i64).unwrap();
        }
        let all = db.recent(usize::MAX >> 1, &[]).unwrap();
        assert_eq!(all.len(), MAX_USAGE_ROWS);
        assert_eq!(&*all[0].id, format!("app{}", MAX_USAGE_ROWS + 19));
    }

    #[test]
    fn boosts_grow_with_use_fade_with_age_and_favorites_dominate() {
        let db = StateDb::open_in_memory().unwrap();
        let now = 100 * DAY;
        for _ in 0..8 {
            db.record_use(&app("often"), now - 60).unwrap();
        }
        db.record_use(&app("once"), now - 60).unwrap();
        for _ in 0..8 {
            db.record_use(&app("stale"), now - 60 * DAY).unwrap();
        }
        db.toggle_favorite(&app("fav"), now).unwrap();
        let boosts = db.usage_boosts(now).unwrap();
        assert_eq!(boosts["often"], 4); // 1 + log2(8)
        assert_eq!(boosts["once"], 1);
        assert_eq!(boosts["stale"], 2); // 1 + log2(8 × 0.25)
        assert_eq!(boosts["fav"], FAVORITE_BOOST);
        assert!(boosts["fav"] > boosts["often"]);
    }

    #[test]
    fn favorites_toggle_and_forget() {
        let db = StateDb::open_in_memory().unwrap();
        assert!(db.toggle_favorite(&app("a"), 1).unwrap());
        assert!(db.toggle_favorite(&app("b"), 2).unwrap());
        assert!(db.is_favorite("a").unwrap());
        let ids: Vec<_> = db
            .favorites(&[])
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ids, [Arc::from("a"), Arc::from("b")]);
        assert!(!db.toggle_favorite(&app("a"), 3).unwrap());
        assert!(!db.is_favorite("a").unwrap());
        db.record_use(&app("b"), 4).unwrap();
        db.forget("b").unwrap();
        assert!(db.favorites(&[]).unwrap().is_empty());
        assert!(db.recent(10, &[]).unwrap().is_empty());
    }

    #[test]
    fn clipboard_dedupes_caps_and_skips_blank_or_huge_text() {
        let db = StateDb::open_in_memory().unwrap();
        assert!(db.clipboard_add("one", 1, 3).unwrap());
        assert!(db.clipboard_add("two", 2, 3).unwrap());
        assert!(db.clipboard_add("three", 3, 3).unwrap());
        assert!(db.clipboard_add("one", 4, 3).unwrap()); // moves to top
        assert!(db.clipboard_add("four", 5, 3).unwrap()); // drops "two"
        assert!(!db.clipboard_add("   \n", 6, 3).unwrap());
        assert!(
            !db.clipboard_add(&"x".repeat(MAX_CLIP_BYTES + 1), 7, 3)
                .unwrap()
        );
        let texts: Vec<_> = db
            .clipboard(10)
            .unwrap()
            .into_iter()
            .map(|e| e.text.to_string())
            .collect();
        assert_eq!(texts, ["four", "one", "three"]);
        db.clipboard_clear().unwrap();
        assert!(db.clipboard(10).unwrap().is_empty());
    }

    fn clip_texts(db: &StateDb) -> Vec<String> {
        db.clipboard(100)
            .unwrap()
            .into_iter()
            .map(|e| e.text.to_string())
            .collect()
    }

    fn clip_id(db: &StateDb, text: &str) -> i64 {
        db.clipboard(100)
            .unwrap()
            .into_iter()
            .find(|e| &*e.text == text)
            .expect("entry")
            .id
    }

    #[test]
    fn pinned_clipboard_entries_come_first_and_outlive_the_cap_and_clearing() {
        let db = StateDb::open_in_memory().unwrap();
        db.clipboard_add("old", 1, 2).unwrap();
        db.clipboard_add("keep", 2, 2).unwrap();
        db.clipboard_add("also", 3, 2).unwrap();
        assert!(
            db.clipboard_set_pinned(clip_id(&db, "keep"), true, 10)
                .unwrap()
        );
        assert!(
            db.clipboard_set_pinned(clip_id(&db, "also"), true, 11)
                .unwrap()
        );
        // Pins are in the order they were made; the cap counts only the rest.
        db.clipboard_add("a", 4, 2).unwrap();
        db.clipboard_add("b", 5, 2).unwrap();
        db.clipboard_add("c", 6, 2).unwrap();
        assert_eq!(clip_texts(&db), ["keep", "also", "c", "b"]);
        assert!(db.clipboard(100).unwrap()[0].pinned);
        // Copying a pinned entry again keeps it pinned.
        db.clipboard_add("keep", 7, 2).unwrap();
        assert!(db.clipboard(100).unwrap()[0].pinned);

        db.clipboard_clear().unwrap();
        assert_eq!(clip_texts(&db), ["keep", "also"]);
        assert!(
            db.clipboard_set_pinned(clip_id(&db, "also"), false, 12)
                .unwrap()
        );
        assert!(!db.clipboard_set_pinned(9999, true, 12).unwrap());
        db.clipboard_clear().unwrap();
        assert_eq!(clip_texts(&db), ["keep"]);
        // Deleting one entry removes it even when pinned.
        assert!(db.clipboard_delete(clip_id(&db, "keep")).unwrap());
        assert!(clip_texts(&db).is_empty());
        assert!(!db.clipboard_delete(1).unwrap());
    }

    #[test]
    fn upgrades_a_version_1_file_and_keeps_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE usage (id TEXT PRIMARY KEY, kind TEXT NOT NULL,
                    title TEXT NOT NULL, subtitle TEXT, action TEXT NOT NULL,
                    count INTEGER NOT NULL, last_used INTEGER NOT NULL);
                 CREATE TABLE favorites (id TEXT PRIMARY KEY, kind TEXT NOT NULL,
                    title TEXT NOT NULL, subtitle TEXT, action TEXT NOT NULL,
                    added_at INTEGER NOT NULL);
                 CREATE TABLE clipboard (id INTEGER PRIMARY KEY,
                    text TEXT NOT NULL UNIQUE, copied_at INTEGER NOT NULL);
                 INSERT INTO clipboard (text, copied_at) VALUES ('kept', 1);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let db = StateDb::open(&path).unwrap();
        assert_eq!(clip_texts(&db), ["kept"]);
        assert!(
            db.clipboard_set_pinned(clip_id(&db, "kept"), true, 2)
                .unwrap()
        );
        drop(db);

        // The copy is the version 1 file, untouched.
        let backup = dir.path().join("state.db.schema-1");
        let old = Connection::open(&backup).unwrap();
        let version: i32 = old
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let text: String = old
            .query_row("SELECT text FROM clipboard", [], |r| r.get(0))
            .unwrap();
        assert_eq!(text, "kept");

        // Opening again neither migrates nor overwrites the copy.
        let db = StateDb::open(&path).unwrap();
        assert!(db.clipboard(10).unwrap()[0].pinned);
    }

    #[test]
    fn reopens_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        StateDb::open(&path)
            .unwrap()
            .record_use(&app("a"), 1)
            .unwrap();
        assert_eq!(
            StateDb::open(&path).unwrap().recent(10, &[]).unwrap().len(),
            1
        );
    }

    #[test]
    fn a_damaged_database_is_moved_aside_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        std::fs::write(&path, vec![0x42; 8192]).unwrap();
        let (db, moved) = StateDb::open_recovering(&path).unwrap();
        let moved = moved.expect("moved aside");
        assert!(moved.exists());
        assert_eq!(std::fs::read(&moved).unwrap(), vec![0x42; 8192]);
        db.record_use(&app("a"), 1).unwrap();
        assert_eq!(db.recent(10, &[]).unwrap().len(), 1);

        // An intact database is opened as it is.
        drop(db);
        let (db, moved) = StateDb::open_recovering(&path).unwrap();
        assert_eq!(moved, None);
        assert_eq!(db.recent(10, &[]).unwrap().len(), 1);
    }

    #[test]
    fn refuses_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(matches!(
            StateDb::open(&path),
            Err(StateError::NewerSchema(99))
        ));
    }
}
