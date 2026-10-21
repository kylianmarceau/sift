//! Persistent storage for the sift file index.
//!
//! SQLite is the durable source of truth. Every indexed filesystem object is
//! one row in `entries`, linked to its parent directory by id; full paths are
//! reconstructed on demand. Storing parent links instead of full paths keeps
//! the database compact and makes a directory rename a single-row update once
//! incremental updates exist.
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
    EntryId, EntryKind, EntryRecord, EntryRow, NewEntry, RootId, RootInfo, ScanSummary,
};

/// Current on-disk schema version, stored in `PRAGMA user_version`.
///
/// - 1: initial schema.
/// - 2: `roots.options` stores the indexing options used for each root.
const SCHEMA_VERSION: i64 = 2;

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
    options     BLOB
);

-- One row per filesystem object. The root directory of each indexed tree
-- has parent_id NULL and stores its full absolute path in `name`.
-- No secondary indexes yet: search uses the name snapshot, and the only
-- other access paths are rowid lookups and whole-root deletes.
CREATE TABLE entries (
    id        INTEGER PRIMARY KEY,
    root_id   INTEGER NOT NULL,
    parent_id INTEGER,
    kind      INTEGER NOT NULL,
    name      BLOB NOT NULL,
    size      INTEGER,
    mtime     INTEGER
);

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
const MIGRATIONS: &[(i64, &str)] = &[(2, "ALTER TABLE roots ADD COLUMN options BLOB;")];

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
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, path, entry_id, entry_count, error_count, indexed_at, scan_millis, {options}
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
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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

#[cfg(test)]
mod tests;
