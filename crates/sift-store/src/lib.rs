//! Persistent storage for the sift file index.
//!
//! SQLite is the durable source of truth. Every indexed filesystem object is
//! one row in `entries`, linked to its parent directory by id; full paths are
//! reconstructed on demand. Storing parent links instead of full paths keeps
//! the database compact. Whole roots are written by [`RootWriter`];
//! filesystem changes are applied incrementally through an [`Updater`].
//!
//! Search does not query this table row by row. The search crate builds a
//! compact, memory-mapped name snapshot from it (see `sift-search`), tagged
//! with the store's [`Store::generation`] so stale snapshots are detected.

mod paths;
mod types;

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

pub use paths::PathResolver;
pub use types::{
    ChildEntry, EntryId, EntryKind, EntryRecord, EntryRow, EventMark, NewEntry, RootId, RootInfo,
    ScanSummary, UpdateSummary,
};

/// Current on-disk schema version, stored in `PRAGMA user_version`.
///
/// - 1: initial schema.
/// - 2: `roots.options` stores the indexing options used for each root.
/// - 3: `roots.event_id` / `roots.event_uuid` record how far filesystem
///   events have been applied, and `entries_parent` indexes children by
///   parent for incremental updates.
const SCHEMA_VERSION: i64 = 3;

const SCHEMA: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE roots (
    id          INTEGER PRIMARY KEY,
    path        BLOB NOT NULL UNIQUE,
    entry_id    INTEGER NOT NULL,
    entry_count INTEGER NOT NULL DEFAULT 0,
    error_count INTEGER NOT NULL DEFAULT 0,
    indexed_at  INTEGER NOT NULL DEFAULT 0,
    scan_millis INTEGER NOT NULL DEFAULT 0,
    -- Opaque, encoded by the indexer: what was included or excluded.
    options     BLOB,
    -- Last FSEvents event id reflected in this root, and the UUID of the
    -- volume's event store it belongs to (ids from another store are
    -- meaningless). NULL until a watcher has recorded one.
    event_id    INTEGER,
    event_uuid  TEXT
);

-- One row per filesystem object. The root directory of each indexed tree
-- has parent_id NULL and stores its full absolute path in `name`.
-- Search never queries names here (it uses the name snapshot). The only
-- secondary index serves incremental updates, which list a directory's
-- children and delete subtrees.
CREATE TABLE entries (
    id        INTEGER PRIMARY KEY,
    root_id   INTEGER NOT NULL,
    parent_id INTEGER,
    kind      INTEGER NOT NULL,
    name      BLOB NOT NULL,
    size      INTEGER,
    mtime     INTEGER
);
CREATE INDEX entries_parent ON entries (parent_id);

INSERT INTO meta (key, value) VALUES ('generation', 0);
";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("index database {path} is corrupt or not a sift index: {source}")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "index database {path} uses schema version {found}, but this build supports up to {supported}"
    )]
    UnsupportedSchema {
        path: PathBuf,
        found: i64,
        supported: i64,
    },
    #[error("{new} is inside the already indexed root {existing}; re-index that root instead")]
    NestedRoot { new: PathBuf, existing: PathBuf },
    #[error("entry {0} does not exist in the index")]
    MissingEntry(EntryId),
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Upgrades from older schema versions, applied in order inside one
/// transaction when a database is opened for writing.
const MIGRATIONS: &[(i64, &str)] = &[
    (2, "ALTER TABLE roots ADD COLUMN options BLOB;"),
    (
        3,
        "ALTER TABLE roots ADD COLUMN event_id INTEGER;
         ALTER TABLE roots ADD COLUMN event_uuid TEXT;
         CREATE INDEX entries_parent ON entries (parent_id);",
    ),
];

/// Handle to an index database.
pub struct Store {
    conn: Connection,
    path: PathBuf,
    /// Schema version of the open database. A read-only handle on an older
    /// database cannot migrate it and adapts its queries instead.
    version: i64,
}

impl Store {
    /// Opens (creating if necessary) the index database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn, path, true)
    }

    /// Opens an existing index database without write access.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        Self::init(conn, path, false)
    }

    fn init(conn: Connection, path: &Path, writable: bool) -> Result<Self> {
        let path = path.to_path_buf();
        let corrupt = |source| StoreError::Corrupt {
            path: path.clone(),
            source,
        };

        // A non-database file is only detected on first read, so probe it.
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(corrupt)?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema {
                path,
                found: version,
                supported: SCHEMA_VERSION,
            });
        }

        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        if writable {
            // WAL lets a long-lived reader (the launcher) keep searching while
            // the indexer writes. NORMAL sync is durable across app crashes;
            // only an OS crash can lose the last transaction, and the index is
            // always rebuildable from the filesystem.
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(corrupt)?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            // Cap the WAL file left behind after large transactions.
            conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
            if version == 0 {
                conn.execute_batch(&format!(
                    "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))
                .map_err(corrupt)?;
            } else if version < SCHEMA_VERSION {
                let mut batch = String::from("BEGIN;");
                for (_, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > version) {
                    batch.push_str(sql);
                }
                batch.push_str(&format!("PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"));
                conn.execute_batch(&batch).map_err(corrupt)?;
            }
            return Ok(Self {
                conn,
                path,
                version: SCHEMA_VERSION,
            });
        } else if version == 0 {
            // Read-only handle on an empty or foreign file.
            return Err(StoreError::Corrupt {
                path,
                source: rusqlite::Error::QueryReturnedNoRows,
            });
        }

        Ok(Self {
            conn,
            path,
            version,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Monotonic counter bumped by every committed change to the index.
    /// Derived data (the search snapshot) records the generation it was
    /// built from and is rebuilt when it no longer matches.
    pub fn generation(&self) -> Result<u64> {
        let value: i64 = self.conn.query_row(
            "SELECT value FROM meta WHERE key = 'generation'",
            [],
            |row| row.get(0),
        )?;
        Ok(value as u64)
    }

    pub fn entry_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT count(*) FROM entries", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    /// The next free entry id. The indexer allocates ids from here upward.
    pub fn next_entry_id(&self) -> Result<EntryId> {
        let max: Option<i64> = self
            .conn
            .query_row("SELECT max(id) FROM entries", [], |row| row.get(0))?;
        Ok(max.unwrap_or(0) + 1)
    }

    pub fn roots(&self) -> Result<Vec<RootInfo>> {
        let options = if self.version >= 2 { "options" } else { "NULL" };
        let events = if self.version >= 3 {
            "event_id, event_uuid"
        } else {
            "NULL, NULL"
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, path, entry_id, entry_count, error_count, indexed_at, scan_millis, {options},
                    {events}
             FROM roots ORDER BY path"
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(RootInfo {
                id: row.get(0)?,
                path: path_from_blob(row.get_ref(1)?.as_blob()?),
                entry_id: row.get(2)?,
                entry_count: row.get::<_, i64>(3)? as u64,
                error_count: row.get::<_, i64>(4)? as u64,
                indexed_at: row.get(5)?,
                scan_millis: row.get::<_, i64>(6)? as u64,
                options: row.get::<_, Option<Vec<u8>>>(7)?.unwrap_or_default(),
                event: match (
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ) {
                    (Some(id), Some(uuid)) => Some(EventMark {
                        id: id as u64,
                        uuid,
                    }),
                    _ => None,
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The root stored at exactly `path`, if any.
    pub fn root(&self, path: &Path) -> Result<Option<RootInfo>> {
        Ok(self.roots()?.into_iter().find(|r| r.path == path))
    }

    /// Records how far filesystem events have been applied to a root.
    /// Does not change the index contents, so the generation stays.
    pub fn set_root_event(&self, root_id: RootId, event: &EventMark) -> Result<()> {
        self.conn.execute(
            "UPDATE roots SET event_id = ?1, event_uuid = ?2 WHERE id = ?3",
            params![event.id as i64, event.uuid, root_id],
        )?;
        Ok(())
    }

    /// Starts an incremental update of one root. Everything happens in one
    /// transaction; readers see the previous index until
    /// [`Updater::commit`].
    pub fn begin_update(&mut self, root_id: RootId) -> Result<Updater<'_>> {
        let next_id = self.next_entry_id()?;
        let tx = self.conn.transaction()?;
        Ok(Updater {
            tx,
            root_id,
            next_id,
            inserted: 0,
            removed: 0,
            updated: 0,
        })
    }

    /// Starts replacing the contents of the root at `root` (an absolute,
    /// canonical directory path).
    ///
    /// Everything happens in one transaction: until [`RootWriter::commit`]
    /// succeeds, readers keep seeing the previous index, and an interrupted
    /// scan leaves the previous index intact.
    ///
    /// Roots may not nest. Indexing a directory inside an existing root is
    /// rejected; indexing a parent of existing roots absorbs them.
    ///
    /// `options` is stored with the root for the indexer to reuse; the store
    /// does not interpret it.
    pub fn replace_root(
        &mut self,
        root: &Path,
        root_entry_id: EntryId,
        options: &[u8],
    ) -> Result<RootWriter<'_>> {
        let existing = self.roots()?;
        if let Some(outer) = existing
            .iter()
            .find(|r| root != r.path && root.starts_with(&r.path))
        {
            return Err(StoreError::NestedRoot {
                new: root.to_path_buf(),
                existing: outer.path.clone(),
            });
        }

        let tx = self.conn.transaction()?;
        for old in existing.iter().filter(|r| r.path.starts_with(root)) {
            delete_root(&tx, old.id)?;
        }

        let root_blob = root.as_os_str().as_bytes();
        tx.execute(
            "INSERT INTO roots (path, entry_id, options) VALUES (?1, ?2, ?3)",
            params![root_blob, root_entry_id, options],
        )?;
        let root_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO entries (id, root_id, parent_id, kind, name) VALUES (?1, ?2, NULL, ?3, ?4)",
            params![root_entry_id, root_id, EntryKind::Dir.as_u8(), root_blob],
        )?;

        Ok(RootWriter { tx, root_id })
    }

    /// Removes a root and all of its entries. Returns whether it existed.
    pub fn remove_root(&mut self, root: &Path) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let id: Option<RootId> = tx
            .query_row(
                "SELECT id FROM roots WHERE path = ?1",
                [root.as_os_str().as_bytes()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            return Ok(false);
        };
        delete_root(&tx, id)?;
        bump_generation(&tx)?;
        tx.commit()?;
        self.checkpoint();
        self.compact_if_fragmented()?;
        Ok(true)
    }

    pub fn entry(&self, id: EntryId) -> Result<EntryRecord> {
        self.conn
            .prepare_cached("SELECT parent_id, kind, size, mtime FROM entries WHERE id = ?1")?
            .query_row([id], |row| {
                Ok(EntryRecord {
                    id,
                    parent_id: row.get(0)?,
                    kind: EntryKind::from_u8(row.get(1)?),
                    size: row.get::<_, Option<i64>>(2)?.map(|s| s as u64),
                    mtime: row.get(3)?,
                })
            })
            .optional()?
            .ok_or(StoreError::MissingEntry(id))
    }

    /// Streams every entry in id order, which guarantees a parent directory
    /// is visited before its children.
    pub fn for_each_entry(&self, mut visit: impl FnMut(EntryRow<'_>)) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, parent_id, kind, name FROM entries ORDER BY id")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            visit(EntryRow {
                id: row.get(0)?,
                parent_id: row.get(1)?,
                kind: EntryKind::from_u8(row.get(2)?),
                name: row.get_ref(3)?.as_blob().map_err(rusqlite::Error::from)?,
            });
        }
        Ok(())
    }

    /// Streams one root's entries with their metadata, in id order (so a
    /// folder always comes before its contents).
    pub fn for_each_root_entry(
        &self,
        root_id: RootId,
        mut visit: impl FnMut(&EntryRow<'_>, Option<u64>, Option<i64>),
    ) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT id, parent_id, kind, name, size, mtime FROM entries
             WHERE root_id = ?1 ORDER BY id",
        )?;
        let mut rows = stmt.query([root_id])?;
        while let Some(row) = rows.next()? {
            let entry = EntryRow {
                id: row.get(0)?,
                parent_id: row.get(1)?,
                kind: EntryKind::from_u8(row.get(2)?),
                name: row.get_ref(3)?.as_blob().map_err(rusqlite::Error::from)?,
            };
            let size: Option<i64> = row.get(4)?;
            visit(&entry, size.map(|s| s as u64), row.get(5)?);
        }
        Ok(())
    }

    /// Returns a resolver that turns entry ids into absolute paths, caching
    /// directory paths for the duration of one query.
    pub fn path_resolver(&self) -> PathResolver<'_> {
        PathResolver::new(&self.conn)
    }

    /// Size of the database and its WAL on disk, in bytes.
    pub fn disk_size(&self) -> u64 {
        let mut total = file_len(&self.path);
        let mut wal = self.path.as_os_str().to_owned();
        wal.push("-wal");
        total += file_len(Path::new(&wal));
        total
    }

    /// Folds the WAL back into the main database file so a large index
    /// transaction does not leave a large WAL behind. Call after a
    /// [`RootWriter`] commits. Best effort.
    pub fn checkpoint(&self) {
        let _ = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    }

    /// Checks the invariants the rest of sift relies on, and returns one
    /// line per kind of violation found (empty when the index is sound):
    ///
    /// - every entry's parent exists, is a folder, belongs to the same root
    ///   and has a smaller id (the snapshot builder depends on that order);
    /// - no folder holds two entries with the same name;
    /// - each root's entry exists, has no parent and belongs to that root,
    ///   and no other entry lacks a parent;
    /// - every entry belongs to a root, and each root's stored entry count
    ///   matches its entries.
    pub fn verify(&self) -> Result<Vec<String>> {
        let dir = EntryKind::Dir as i64;
        let checks: [(&str, String); 9] = [
            (
                "entries whose parent does not exist",
                "SELECT COUNT(*) FROM entries e LEFT JOIN entries p ON p.id = e.parent_id
                 WHERE e.parent_id IS NOT NULL AND p.id IS NULL"
                    .into(),
            ),
            (
                "entries whose parent is not a folder",
                format!(
                    "SELECT COUNT(*) FROM entries e JOIN entries p ON p.id = e.parent_id
                     WHERE p.kind != {dir}"
                ),
            ),
            (
                "entries whose parent is in another root",
                "SELECT COUNT(*) FROM entries e JOIN entries p ON p.id = e.parent_id
                 WHERE p.root_id != e.root_id"
                    .into(),
            ),
            (
                "entries numbered below their parent",
                "SELECT COUNT(*) FROM entries e JOIN entries p ON p.id = e.parent_id
                 WHERE e.id <= p.id"
                    .into(),
            ),
            (
                "names that appear twice in one folder",
                "SELECT COUNT(*) FROM (SELECT 1 FROM entries WHERE parent_id IS NOT NULL
                 GROUP BY parent_id, name HAVING COUNT(*) > 1)"
                    .into(),
            ),
            (
                "roots whose top entry is missing or wrong",
                "SELECT COUNT(*) FROM roots r LEFT JOIN entries e ON e.id = r.entry_id
                 WHERE e.id IS NULL OR e.parent_id IS NOT NULL OR e.root_id != r.id"
                    .into(),
            ),
            (
                "entries without a parent that are not a root's top entry",
                "SELECT COUNT(*) FROM entries e WHERE e.parent_id IS NULL
                 AND NOT EXISTS (SELECT 1 FROM roots r WHERE r.entry_id = e.id)"
                    .into(),
            ),
            (
                "entries that belong to no root",
                "SELECT COUNT(*) FROM entries e
                 WHERE NOT EXISTS (SELECT 1 FROM roots r WHERE r.id = e.root_id)"
                    .into(),
            ),
            (
                "roots whose entry count is wrong",
                "SELECT COUNT(*) FROM roots r
                 WHERE r.entry_count != (SELECT COUNT(*) FROM entries e WHERE e.root_id = r.id)"
                    .into(),
            ),
        ];
        let mut problems = Vec::new();
        for (label, sql) in checks {
            let count: i64 = self.conn.query_row(&sql, [], |r| r.get(0))?;
            if count > 0 {
                problems.push(format!("{count} {label}"));
            }
        }
        Ok(problems)
    }

    /// Runs SQLite's `quick_check`: `Ok(None)` when the database is
    /// intact, otherwise the first problem it reports. Reads every page, so
    /// it costs about as much as reading the file once; run it only when
    /// there is reason to doubt the file (after an unclean shutdown).
    pub fn quick_check(&self) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("PRAGMA quick_check")?;
        let mut rows = stmt.query([])?;
        let mut first = None;
        while let Some(row) = rows.next()? {
            let line: String = row.get(0)?;
            if line != "ok" && first.is_none() {
                first = Some(line);
            }
        }
        Ok(first)
    }

    /// Rewrites the database when more than a quarter of it is free pages,
    /// as happens after re-indexing a root with far fewer entries. SQLite
    /// otherwise keeps the space for reuse and never shrinks the file.
    /// Returns whether a compaction ran.
    pub fn compact_if_fragmented(&self) -> Result<bool> {
        let pages: i64 = self
            .conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let free: i64 = self
            .conn
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        // Small databases are not worth rewriting.
        if pages < 256 || free * 4 < pages {
            return Ok(false);
        }
        self.conn.execute_batch("VACUUM")?;
        self.checkpoint();
        Ok(true)
    }
}

/// Writes the entries of one root inside a single transaction.
pub struct RootWriter<'conn> {
    tx: Transaction<'conn>,
    root_id: RootId,
}

impl RootWriter<'_> {
    pub fn insert(&mut self, entry: &NewEntry<'_>) -> Result<()> {
        let mut stmt = self.tx.prepare_cached(
            "INSERT INTO entries (id, root_id, parent_id, kind, name, size, mtime)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        stmt.execute(params![
            entry.id,
            self.root_id,
            entry.parent_id,
            entry.kind.as_u8(),
            entry.name,
            entry.size.map(|s| s as i64),
            entry.mtime,
        ])?;
        Ok(())
    }

    /// Records the filesystem event position this scan reflects.
    pub fn set_event(&mut self, event: &EventMark) -> Result<()> {
        self.tx.execute(
            "UPDATE roots SET event_id = ?1, event_uuid = ?2 WHERE id = ?3",
            params![event.id as i64, event.uuid, self.root_id],
        )?;
        Ok(())
    }

    /// Records the scan summary, bumps the store generation and commits.
    pub fn commit(self, summary: ScanSummary) -> Result<RootId> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.tx.execute(
            "UPDATE roots SET entry_count = ?1, error_count = ?2, indexed_at = ?3, scan_millis = ?4
             WHERE id = ?5",
            params![
                summary.entry_count as i64,
                summary.error_count as i64,
                now,
                summary.scan_millis as i64,
                self.root_id
            ],
        )?;
        bump_generation(&self.tx)?;
        self.tx.commit()?;
        Ok(self.root_id)
    }
}

/// Applies filesystem changes to one root inside a single transaction.
///
/// New entries take ids above every existing id ([`Updater::alloc_id`]), so
/// a child's id is still always larger than its parent's.
pub struct Updater<'conn> {
    tx: Transaction<'conn>,
    root_id: RootId,
    next_id: EntryId,
    inserted: u64,
    removed: u64,
    updated: u64,
}

impl Updater<'_> {
    pub fn root_id(&self) -> RootId {
        self.root_id
    }

    /// Allocates one entry id.
    pub fn alloc_id(&mut self) -> EntryId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The next id [`Updater::alloc_id`] would return. A bulk writer that
    /// allocates ids itself starts here and reports back through
    /// [`Updater::set_next_id`].
    pub fn next_id(&self) -> EntryId {
        self.next_id
    }

    pub fn set_next_id(&mut self, next: EntryId) {
        self.next_id = self.next_id.max(next);
    }

    /// The direct children of a directory entry.
    pub fn children(&self, parent: EntryId) -> Result<Vec<ChildEntry>> {
        let mut stmt = self.tx.prepare_cached(
            "SELECT id, kind, name, size, mtime FROM entries WHERE parent_id = ?1",
        )?;
        let rows = stmt.query_map([parent], child_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The child of `parent` named exactly `name` (byte comparison).
    pub fn child(&self, parent: EntryId, name: &[u8]) -> Result<Option<ChildEntry>> {
        let mut stmt = self.tx.prepare_cached(
            "SELECT id, kind, name, size, mtime FROM entries WHERE parent_id = ?1 AND name = ?2",
        )?;
        Ok(stmt
            .query_row(params![parent, name], child_from_row)
            .optional()?)
    }

    pub fn insert(&mut self, entry: &NewEntry<'_>) -> Result<()> {
        let mut stmt = self.tx.prepare_cached(
            "INSERT INTO entries (id, root_id, parent_id, kind, name, size, mtime)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        stmt.execute(params![
            entry.id,
            self.root_id,
            entry.parent_id,
            entry.kind.as_u8(),
            entry.name,
            entry.size.map(|s| s as i64),
            entry.mtime,
        ])?;
        self.next_id = self.next_id.max(entry.id + 1);
        self.inserted += 1;
        Ok(())
    }

    /// Updates the size and modification time of an existing entry.
    pub fn update_meta(
        &mut self,
        id: EntryId,
        size: Option<u64>,
        mtime: Option<i64>,
    ) -> Result<()> {
        self.tx
            .prepare_cached("UPDATE entries SET size = ?1, mtime = ?2 WHERE id = ?3")?
            .execute(params![size.map(|s| s as i64), mtime, id])?;
        self.updated += 1;
        Ok(())
    }

    /// Deletes an entry and everything below it. Returns the number of
    /// entries removed.
    pub fn remove_subtree(&mut self, id: EntryId) -> Result<u64> {
        let removed = self
            .tx
            .prepare_cached(
                "DELETE FROM entries WHERE id IN (
                     WITH RECURSIVE sub(id) AS (
                         SELECT ?1
                         UNION ALL
                         SELECT e.id FROM entries e JOIN sub ON e.parent_id = sub.id
                     )
                     SELECT id FROM sub
                 )",
            )?
            .execute([id])? as u64;
        self.removed += removed;
        Ok(removed)
    }

    /// Commits the changes. The generation is bumped only if something
    /// changed, so derived data is rebuilt only when needed. `event`, if
    /// given, is recorded in the same transaction as the changes it covers.
    pub fn commit(self, event: Option<&EventMark>) -> Result<UpdateSummary> {
        let summary = UpdateSummary {
            inserted: self.inserted,
            removed: self.removed,
            updated: self.updated,
        };
        if summary.changed() {
            self.tx.execute(
                "UPDATE roots SET entry_count = max(0, entry_count + ?1) WHERE id = ?2",
                params![self.inserted as i64 - self.removed as i64, self.root_id],
            )?;
            bump_generation(&self.tx)?;
        }
        if let Some(event) = event {
            self.tx.execute(
                "UPDATE roots SET event_id = ?1, event_uuid = ?2 WHERE id = ?3",
                params![event.id as i64, event.uuid, self.root_id],
            )?;
        }
        self.tx.commit()?;
        Ok(summary)
    }
}

fn child_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChildEntry> {
    Ok(ChildEntry {
        id: row.get(0)?,
        kind: EntryKind::from_u8(row.get(1)?),
        name: row.get(2)?,
        size: row.get::<_, Option<i64>>(3)?.map(|s| s as u64),
        mtime: row.get(4)?,
    })
}

fn delete_root(tx: &Transaction<'_>, root_id: RootId) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM entries WHERE root_id = ?1", [root_id])?;
    tx.execute("DELETE FROM roots WHERE id = ?1", [root_id])?;
    Ok(())
}

fn bump_generation(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE meta SET value = value + 1 WHERE key = 'generation'",
        [],
    )?;
    Ok(())
}

fn path_from_blob(bytes: &[u8]) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Moves a damaged database at `path`, with its `-wal` and `-shm` files,
/// into a new `damaged-<unix time>` folder next to it, so a fresh one can be
/// created without losing the old file. Returns the folder.
pub fn move_aside(path: &Path) -> std::io::Result<PathBuf> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut target = dir.join(format!("damaged-{stamp}"));
    let mut n = 1;
    while target.exists() {
        n += 1;
        target = dir.join(format!("damaged-{stamp}-{n}"));
    }
    std::fs::create_dir_all(&target)?;
    let name = path.file_name().unwrap_or_default().to_os_string();
    for suffix in ["", "-wal", "-shm"] {
        let mut file = name.clone();
        file.push(suffix);
        let from = dir.join(&file);
        if from.exists() {
            std::fs::rename(&from, target.join(&file))?;
        }
    }
    Ok(target)
}

#[cfg(test)]
mod tests;
