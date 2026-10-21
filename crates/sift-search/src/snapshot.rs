//! The name snapshot: a compact, memory-mapped copy of every indexed name.
//!
//! Querying SQLite row by row costs on the order of 100 ns per row, far too
//! slow for substring and fuzzy matching over a million names on every
//! keystroke. Instead, folded names are packed into one contiguous buffer
//! that can be scanned with SIMD substring search, alongside small parallel
//! arrays. The file is memory-mapped, so its pages are clean and
//! file-backed: macOS can drop them under memory pressure and re-read them
//! later, unlike heap memory holding the same data.
//!
//! The snapshot is derived data. It records the store generation it was
//! built from and is rebuilt whenever that no longer matches or the file
//! fails validation. It is written to a temporary file and renamed into
//! place, so an existing mapping is never modified underneath a reader.
//!
//! Layout (little-endian, every section 8-byte aligned):
//!
//! ```text
//! header   64 bytes
//! ids      u64 × n     entry id of each name
//! bags     u64 × n     character bag (see `fold::char_bag`)
//! offsets  u32 × n+1   start of each name in `names`
//! meta     u16 × n     kind (bits 14–15) | noisy (bit 13) | depth (bits 0–12)
//! names    bytes       folded names, each followed by a NUL separator
//! ```

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use sift_store::{EntryId, EntryKind, Store};

use crate::fold::{char_bag, fold_into};

#[cfg(target_endian = "big")]
compile_error!("the snapshot format assumes a little-endian host");

const MAGIC: [u8; 8] = *b"SIFTSNAP";
/// 2: meta gained the "noisy" bit. Older snapshots fail validation and are
/// rebuilt automatically.
const VERSION: u32 = 2;
const HEADER_LEN: usize = 64;
const DEPTH_MASK: u16 = 0x1FFF;
const NOISY_BIT: u16 = 1 << 13;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot {0} does not exist")]
    Missing(PathBuf),
    #[error("snapshot was built from index generation {found}, but the index is at {expected}")]
    Stale { found: u64, expected: u64 },
    #[error("snapshot {path} is invalid: {reason}")]
    Corrupt { path: PathBuf, reason: &'static str },
    #[error("snapshot I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] sift_store::StoreError),
}

/// Read-only view of a snapshot file.
pub struct Snapshot {
    map: Mmap,
    generation: u64,
    count: usize,
    ids: Section,
    bags: Section,
    offsets: Section,
    meta: Section,
    names: Section,
}

#[derive(Clone, Copy)]
struct Section {
    start: usize,
    len: usize,
}

impl Section {
    fn range(self) -> std::ops::Range<usize> {
        self.start..self.start + self.len
    }
}

/// Section positions for a snapshot with `count` names and `names_len`
/// bytes of name data. Returns the sections and the total file length.
fn layout(count: usize, names_len: usize) -> Option<([Section; 5], usize)> {
    let mut pos = HEADER_LEN;
    let mut next = |len: usize| -> Option<Section> {
        let section = Section { start: pos, len };
        pos = pos.checked_add(len)?.checked_next_multiple_of(8)?;
        Some(section)
    };
    let ids = next(count.checked_mul(8)?)?;
    let bags = next(count.checked_mul(8)?)?;
    let offsets = next(count.checked_add(1)?.checked_mul(4)?)?;
    let meta = next(count.checked_mul(2)?)?;
    let names = next(names_len)?;
    Some(([ids, bags, offsets, meta, names], names.start + names_len))
}

impl Snapshot {
    /// Maps and validates the snapshot at `path`, requiring that it was
    /// built from store generation `expected_generation`.
    pub fn open(path: &Path, expected_generation: u64) -> Result<Self, SnapshotError> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SnapshotError::Missing(path.to_path_buf()));
            }
            Err(e) => return Err(e.into()),
        };
        // SAFETY: the mapping is read-only, and sift never modifies a
        // snapshot file in place: new snapshots are written to a temporary
        // file and atomically renamed over the old one, which leaves this
        // mapping's inode untouched. External modification of files in the
        // index directory is outside what we can defend against.
        let map = unsafe { Mmap::map(&file)? };
        let corrupt = |reason| SnapshotError::Corrupt {
            path: path.to_path_buf(),
            reason,
        };

        if map.len() < HEADER_LEN || map[..8] != MAGIC {
            return Err(corrupt("bad header"));
        }
        let header = &map[..HEADER_LEN];
        let u32_at =
            |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().unwrap_or([0; 4]));
        let u64_at =
            |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap_or([0; 8]));
        if u32_at(8) != VERSION {
            return Err(corrupt("unsupported version"));
        }
        let generation = u64_at(16);
        let count = usize::try_from(u64_at(24)).map_err(|_| corrupt("bad count"))?;
        let names_len = usize::try_from(u64_at(32)).map_err(|_| corrupt("bad length"))?;
        let ([ids, bags, offsets, meta, names], total) =
            layout(count, names_len).ok_or_else(|| corrupt("bad sizes"))?;
        if total != map.len() {
            return Err(corrupt("length mismatch"));
        }

        let snapshot = Self {
            map,
            generation,
            count,
            ids,
            bags,
            offsets,
            meta,
            names,
        };
        snapshot.validate().map_err(corrupt)?;
        if generation != expected_generation {
            return Err(SnapshotError::Stale {
                found: generation,
                expected: expected_generation,
            });
        }
        Ok(snapshot)
    }

    /// Checks alignment and that every name lies within the name buffer and
    /// ends with a separator. One linear pass; about 1 ms per million names.
    fn validate(&self) -> Result<(), &'static str> {
        let cast_ok = bytemuck::try_cast_slice::<u8, u64>(&self.map[self.ids.range()]).is_ok()
            && bytemuck::try_cast_slice::<u8, u64>(&self.map[self.bags.range()]).is_ok()
            && bytemuck::try_cast_slice::<u8, u32>(&self.map[self.offsets.range()]).is_ok()
            && bytemuck::try_cast_slice::<u8, u16>(&self.map[self.meta.range()]).is_ok();
        if !cast_ok {
            return Err("misaligned section");
        }
        let offsets = self.offsets();
        let names = self.names_blob();
        if offsets.first() != Some(&0) || offsets.last().map(|&o| o as usize) != Some(names.len()) {
            return Err("bad offsets");
        }
        for pair in offsets.windows(2) {
            let (start, end) = (pair[0] as usize, pair[1] as usize);
            if end <= start || names[end - 1] != 0 {
                return Err("bad name boundary");
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Size of the snapshot file in bytes.
    pub fn byte_len(&self) -> usize {
        self.map.len()
    }

    // The casts below cannot fail: `validate` checked alignment and the
    // section lengths are multiples of the element size by construction.

    pub(crate) fn ids(&self) -> &[u64] {
        bytemuck::cast_slice(&self.map[self.ids.range()])
    }

    pub(crate) fn bags(&self) -> &[u64] {
        bytemuck::cast_slice(&self.map[self.bags.range()])
    }

    pub(crate) fn offsets(&self) -> &[u32] {
        bytemuck::cast_slice(&self.map[self.offsets.range()])
    }

    pub(crate) fn meta(&self) -> &[u16] {
        bytemuck::cast_slice(&self.map[self.meta.range()])
    }

    /// All folded names, NUL-separated.
    pub(crate) fn names_blob(&self) -> &[u8] {
        &self.map[self.names.range()]
    }

    /// Folded name of entry `i`, without its separator.
    pub(crate) fn name(&self, i: usize) -> &[u8] {
        let offsets = self.offsets();
        &self.names_blob()[offsets[i] as usize..offsets[i + 1] as usize - 1]
    }

    pub(crate) fn entry_id(&self, i: usize) -> EntryId {
        self.ids()[i] as EntryId
    }
}

pub(crate) fn kind_of(meta: u16) -> EntryKind {
    EntryKind::from_u8((meta >> 14) as u8)
}

pub(crate) fn depth_of(meta: u16) -> u16 {
    meta & DEPTH_MASK
}

/// Whether the entry lives somewhere below a dependency, build or cache
/// folder (see [`is_noise_dir`]). Such results rank below the user's own
/// files of the same match tier.
pub(crate) fn is_noisy(meta: u16) -> bool {
    meta & NOISY_BIT != 0
}

/// Folder names (folded) whose contents are usually generated or third-party
/// code: everything below them is ranked as "noisy". This complements the
/// indexer's skip list: it also covers folders that are indexed by default
/// (build output, vendored code, caches) or named unpredictably.
const NOISE_DIRS: &[&[u8]] = &[
    b"node_modules",
    b"bower_components",
    b"site-packages",
    b"dist-packages",
    b".venv",
    b"venv",
    b".git",
    b"__pycache__",
    b"vendor",
    b"third_party",
    b"third-party",
    b"deps",
    b"dist",
    b"build",
    b"target",
    b"obj",
    b".build",
    b"deriveddata",
    b"pods",
    b"cache",
    b"caches",
    b".cache",
    b"archive-v0",
];

/// Suffixes that mark the same kinds of folders under project-specific
/// names, such as `review-deps` or `test_venv`.
const NOISE_SUFFIXES: &[&[u8]] = &[b"-deps", b"_deps", b"-venv", b"_venv", b"-cache", b"_cache"];

/// Whether a folded directory name marks a dependency, build or cache tree.
pub(crate) fn is_noise_dir(folded: &[u8]) -> bool {
    NOISE_DIRS.contains(&folded) || NOISE_SUFFIXES.iter().any(|s| folded.ends_with(s))
}

/// Summary of a snapshot build.
#[derive(Debug, Clone, Copy)]
pub struct BuildStats {
    pub entries: usize,
    pub bytes: u64,
    pub generation: u64,
}

/// Builds a snapshot of `store` and atomically installs it at `path`.
///
/// The arrays are assembled in memory before writing, so the build
/// temporarily needs about as much heap as the resulting file (roughly
/// 25 bytes plus the name length per entry).
pub fn build(store: &Store, path: &Path) -> Result<BuildStats, SnapshotError> {
    // Read the generation first: if the index changes during the build, the
    // snapshot is labelled with the older generation and rebuilt next time.
    let generation = store.generation()?;
    let capacity = usize::try_from(store.entry_count()?).unwrap_or(0);

    let mut ids = Vec::with_capacity(capacity);
    let mut bags = Vec::with_capacity(capacity);
    let mut offsets = Vec::with_capacity(capacity + 1);
    let mut meta = Vec::with_capacity(capacity);
    let mut names = Vec::with_capacity(capacity * 16);
    // Per directory seen so far: its depth, and whether its children are
    // noisy. Parents precede children in id order, so a child's parent is
    // always known.
    let mut dirs: HashMap<EntryId, (u16, bool)> = HashMap::new();

    store.for_each_entry(|row| {
        let (depth, noisy) = match row.parent_id {
            None => (0, false),
            Some(parent) => dirs
                .get(&parent)
                .map_or((DEPTH_MASK, false), |&(d, n)| ((d + 1).min(DEPTH_MASK), n)),
        };
        // Root entries store their full path; search their last component.
        let name = match row.parent_id {
            None => row
                .name
                .rsplit(|&b| b == b'/')
                .find(|c| !c.is_empty())
                .unwrap_or(row.name),
            Some(_) => row.name,
        };

        offsets.push(names.len() as u32);
        let start = names.len();
        fold_into(name, &mut names);
        let folded = &names[start..];
        if row.kind == EntryKind::Dir {
            dirs.insert(row.id, (depth, noisy || is_noise_dir(folded)));
        }
        bags.push(char_bag(folded));
        names.push(0);
        ids.push(row.id as u64);
        let noisy_bit = if noisy { NOISY_BIT } else { 0 };
        meta.push(((row.kind.as_u8() as u16) << 14) | noisy_bit | depth);
    })?;
    offsets.push(names.len() as u32);
    if u32::try_from(names.len()).is_err() {
        return Err(SnapshotError::Corrupt {
            path: path.to_path_buf(),
            reason: "name data exceeds 4 GiB",
        });
    }

    let count = ids.len();
    let ([ids_s, bags_s, offsets_s, meta_s, names_s], total) =
        layout(count, names.len()).ok_or(SnapshotError::Corrupt {
            path: path.to_path_buf(),
            reason: "snapshot too large",
        })?;

    remove_stale_temp_files(path);
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        let mut header = [0u8; HEADER_LEN];
        header[..8].copy_from_slice(&MAGIC);
        header[8..12].copy_from_slice(&VERSION.to_le_bytes());
        header[16..24].copy_from_slice(&generation.to_le_bytes());
        header[24..32].copy_from_slice(&(count as u64).to_le_bytes());
        header[32..40].copy_from_slice(&(names.len() as u64).to_le_bytes());
        out.write_all(&header)?;

        let mut written = HEADER_LEN;
        let mut section = |out: &mut BufWriter<File>, s: Section, bytes: &[u8]| {
            debug_assert_eq!(s.start, written);
            out.write_all(bytes)?;
            let padded = (s.start + s.len).next_multiple_of(8);
            out.write_all(&[0u8; 8][..padded - s.start - s.len])?;
            written = padded;
            std::io::Result::Ok(())
        };
        section(&mut out, ids_s, bytemuck::cast_slice(&ids))?;
        section(&mut out, bags_s, bytemuck::cast_slice(&bags))?;
        section(&mut out, offsets_s, bytemuck::cast_slice(&offsets))?;
        section(&mut out, meta_s, bytemuck::cast_slice(&meta))?;
        // The file ends right after the names; no trailing padding.
        debug_assert_eq!(names_s.start, written);
        out.write_all(&names)?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }

    Ok(BuildStats {
        entries: count,
        bytes: total as u64,
        generation,
    })
}

/// Temporary snapshot files older than this were left by a process that
/// was killed while writing; a write takes seconds even at a million
/// entries, so a younger one may belong to another running process.
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(600);

/// Removes `<snapshot>.tmp-<pid>` files left behind by a killed writer.
fn remove_stale_temp_files(path: &Path) {
    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem()) else {
        return;
    };
    let prefix = format!("{}.tmp-", stem.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_TEMP_AGE);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            tracing::info!(file = %entry.path().display(), "removed a stale temporary snapshot");
        }
    }
}
