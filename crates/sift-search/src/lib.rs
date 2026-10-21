//! Filename search over the sift index.
//!
//! [`SearchIndex`] pairs the SQLite store (source of truth, used to turn
//! results into paths and metadata) with a memory-mapped name
//! [`Snapshot`] that all matching runs against. See `snapshot` for the
//! storage format and `scan` for the matching and ranking algorithm.

mod fold;
mod scan;
pub mod snapshot;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sift_store::{EntryId, EntryKind, Store, StoreError};

pub use fold::fold;
pub use scan::NameMatcher;
pub use snapshot::{BuildStats, Snapshot, SnapshotError};

/// File name of the database inside an index directory.
pub const DB_FILE: &str = "index.db";
/// File name of the name snapshot inside an index directory.
pub const SNAPSHOT_FILE: &str = "names.snap";

/// Where the CLI and the launcher keep the index by default:
/// `~/Library/Application Support/sift/dev-index`.
pub fn default_index_dir() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join("Library/Application Support/sift/dev-index"))
}

/// How a result matched the query, from strongest to weakest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum MatchTier {
    /// Approximate (fuzzy) match: query characters appear in order.
    Fuzzy = 1,
    /// The query occurs somewhere inside the name.
    Substring = 2,
    /// The query starts a word inside the name (`report` in `q3-report`).
    WordPrefix = 3,
    /// The name starts with the query.
    Prefix = 4,
    /// The name without its extension equals the query.
    ExactStem = 5,
    /// The whole name equals the query (ignoring case and accents).
    Exact = 6,
}

impl MatchTier {
    fn from_u8(value: u8) -> Self {
        match value {
            6 => Self::Exact,
            5 => Self::ExactStem,
            4 => Self::Prefix,
            3 => Self::WordPrefix,
            2 => Self::Substring,
            _ => Self::Fuzzy,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::ExactStem => "stem",
            Self::Prefix => "prefix",
            Self::WordPrefix => "word",
            Self::Substring => "substr",
            Self::Fuzzy => "fuzzy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindFilter {
    /// Anything that is not a directory.
    Files,
    Dirs,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    /// Maximum number of results.
    pub limit: usize,
    pub kind: Option<KindFilter>,
    /// Only names ending in `.<extension>` for one of these extensions
    /// (case-insensitive). Empty means any name.
    pub extensions: Vec<String>,
    /// Allow approximate matches when exact/substring matches are scarce.
    pub fuzzy: bool,
}

impl Query {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            limit: 20,
            kind: None,
            extensions: Vec::new(),
            fuzzy: true,
        }
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// A ranked match, not yet resolved to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub entry_id: EntryId,
    pub tier: MatchTier,
    /// Opaque ranking key; higher is better.
    pub score: u64,
}

#[derive(Debug, Clone)]
pub struct SearchOutcome {
    /// Best results first, at most `Query::limit`.
    pub hits: Vec<Hit>,
    /// Number of names that matched. Exact for substring matches; fuzzy
    /// matches are only counted when the fuzzy pass ran.
    pub matched: usize,
    pub fuzzy_ran: bool,
    pub elapsed: Duration,
}

/// A hit with its path and metadata loaded from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub entry_id: EntryId,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    pub tier: MatchTier,
    /// Ranking key from the [`Hit`]; higher is better.
    pub score: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("no index found at {0}; run `sift index <dir>` first")]
    NoIndex(PathBuf),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// An opened index, ready to answer queries.
pub struct SearchIndex {
    store: Store,
    snapshot: Snapshot,
    /// Whether opening had to (re)build the snapshot.
    rebuilt: bool,
}

impl SearchIndex {
    /// Opens the index stored in `dir`, rebuilding the name snapshot if it
    /// is missing, stale or corrupt. Never touches the indexed filesystem.
    pub fn open(dir: &Path) -> Result<Self, SearchError> {
        let db = dir.join(DB_FILE);
        if !db.exists() {
            return Err(SearchError::NoIndex(dir.to_path_buf()));
        }
        let store = Store::open_read_only(&db)?;
        let snapshot_path = dir.join(SNAPSHOT_FILE);
        let generation = store.generation()?;
        let (snapshot, rebuilt) = match Snapshot::open(&snapshot_path, generation) {
            Ok(snapshot) => (snapshot, false),
            Err(SnapshotError::Io(e)) => return Err(SnapshotError::Io(e).into()),
            Err(reason) => {
                tracing::info!(%reason, "rebuilding name snapshot");
                let stats = snapshot::build(&store, &snapshot_path)?;
                (Snapshot::open(&snapshot_path, stats.generation)?, true)
            }
        };
        Ok(Self {
            store,
            snapshot,
            rebuilt,
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn snapshot_rebuilt(&self) -> bool {
        self.rebuilt
    }

    /// Whether the index changed since this snapshot was built (for example
    /// by another process). Reopen to pick the change up.
    pub fn is_stale(&self) -> bool {
        self.store
            .generation()
            .map_or(true, |g| g != self.snapshot.generation())
    }

    /// Runs `query` against the snapshot. Pure CPU work over mapped memory;
    /// no database or filesystem access.
    pub fn search(&self, query: &Query) -> SearchOutcome {
        let started = Instant::now();
        let Some(prepared) = scan::Prepared::new(query) else {
            return SearchOutcome {
                hits: Vec::new(),
                matched: 0,
                fuzzy_ran: false,
                elapsed: started.elapsed(),
            };
        };
        let result = scan::scan(&self.snapshot, &prepared);
        let hits = result
            .top
            .into_sorted()
            .into_iter()
            .map(|ranked| Hit {
                entry_id: self.snapshot.entry_id(ranked.pos.0 as usize),
                tier: scan::tier_of(ranked.key),
                score: ranked.key,
            })
            .collect();
        SearchOutcome {
            hits,
            matched: result.matched,
            fuzzy_ran: result.fuzzy_ran,
            elapsed: started.elapsed(),
        }
    }

    /// Loads paths and metadata for `hits`, preserving order. Hits whose
    /// entries disappeared (a concurrent re-index) are skipped.
    pub fn resolve(&self, hits: &[Hit]) -> Result<Vec<SearchResult>, SearchError> {
        let mut resolver = self.store.path_resolver();
        let mut results = Vec::with_capacity(hits.len());
        for hit in hits {
            let record = match self.store.entry(hit.entry_id) {
                Ok(record) => record,
                Err(StoreError::MissingEntry(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            let path = match resolver.resolve(hit.entry_id) {
                Ok(path) => path,
                Err(StoreError::MissingEntry(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            results.push(SearchResult {
                entry_id: hit.entry_id,
                path,
                kind: record.kind,
                size: record.size,
                mtime: record.mtime,
                tier: hit.tier,
                score: hit.score,
            });
        }
        Ok(results)
    }
}

/// Rebuilds the snapshot for the index in `dir` from its database. Called
/// by the indexer after a scan so the first search does not pay for it.
pub fn rebuild_snapshot(store: &Store, dir: &Path) -> Result<BuildStats, SnapshotError> {
    snapshot::build(store, &dir.join(SNAPSHOT_FILE))
}
