use std::path::PathBuf;

/// Database identifier of an indexed entry.
///
/// Ids are allocated by the indexer before rows are written, and a child is
/// always allocated a larger id than its parent directory. Consumers (for
/// example the search snapshot builder) rely on that ordering.
pub type EntryId = i64;

/// Database identifier of an index root.
pub type RootId = i64;

/// What kind of filesystem object an entry describes.
///
/// Symbolic links are recorded as links and never followed, which is how the
/// indexer avoids symlink cycles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EntryKind {
    File = 0,
    Dir = 1,
    Symlink = 2,
    /// Sockets, FIFOs, devices and anything else that is not a plain file.
    Other = 3,
}

impl EntryKind {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decodes a stored kind. Unknown values map to [`EntryKind::Other`] so a
    /// newer database read by an older binary degrades instead of failing.
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::File,
            1 => Self::Dir,
            2 => Self::Symlink,
            _ => Self::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dir => "dir",
            Self::Symlink => "link",
            Self::Other => "other",
        }
    }
}

/// A row to be written by [`crate::RootWriter::insert`].
///
/// `name` holds the raw bytes of the final path component. macOS filesystems
/// other than APFS/HFS+ (exFAT, SMB, NFS) can contain names that are not
/// valid UTF-8, so names are stored as bytes rather than text.
#[derive(Debug, Clone, Copy)]
pub struct NewEntry<'a> {
    pub id: EntryId,
    pub parent_id: EntryId,
    pub kind: EntryKind,
    pub name: &'a [u8],
    /// Size in bytes. `None` for directories.
    pub size: Option<u64>,
    /// Modification time in whole seconds since the Unix epoch.
    pub mtime: Option<i64>,
}

/// Metadata about an indexed root directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootInfo {
    pub id: RootId,
    pub path: PathBuf,
    pub entry_id: EntryId,
    pub entry_count: u64,
    pub error_count: u64,
    /// Unix timestamp (seconds) of the last completed scan.
    pub indexed_at: i64,
    pub scan_millis: u64,
    /// Indexing options saved with the root (opaque to the store; empty if
    /// none were saved).
    pub options: Vec<u8>,
    /// How far filesystem events have been applied, if a watcher recorded
    /// it.
    pub event: Option<EventMark>,
}

/// A position in the macOS FSEvents history: an event id, and the UUID of
/// the volume's event store it belongs to. If the store is reset (the UUID
/// changes), the id is meaningless and the root must be rescanned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventMark {
    pub id: u64,
    pub uuid: String,
}

/// A directory's child, as listed by [`crate::Updater::children`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildEntry {
    pub id: EntryId,
    pub kind: EntryKind,
    pub name: Vec<u8>,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
}

/// What an incremental update changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateSummary {
    pub inserted: u64,
    pub removed: u64,
    pub updated: u64,
}

impl UpdateSummary {
    pub fn changed(&self) -> bool {
        self.inserted + self.removed + self.updated > 0
    }
}

/// Summary of a finished scan, stored with the root.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanSummary {
    pub entry_count: u64,
    pub error_count: u64,
    pub scan_millis: u64,
}

/// Stored metadata for one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRecord {
    pub id: EntryId,
    pub parent_id: Option<EntryId>,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
}

/// A borrowed view of an entry used when streaming the whole table.
#[derive(Debug, Clone, Copy)]
pub struct EntryRow<'a> {
    pub id: EntryId,
    pub parent_id: Option<EntryId>,
    pub kind: EntryKind,
    /// Raw name bytes. For root entries this is the full absolute path.
    pub name: &'a [u8],
}
