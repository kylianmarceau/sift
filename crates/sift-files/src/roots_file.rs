//! `roots.bin`: a copy of which folders are indexed and with which options.
//!
//! The index database holds the same information, but it is a cache that
//! can be rebuilt from the filesystem, while the list of folders is the
//! user's choice. Keeping a copy next to it means a damaged database can be
//! replaced and rescanned without asking the user to add folders again.
//!
//! Format: a `sift-roots 1\n` header, then per root a little-endian `u32`
//! length and the path bytes, and a `u32` length and the saved options
//! (`IndexOptions::encode_saved`). Paths may contain any byte but NUL, so
//! lengths are used instead of separators. Written to a temporary file and
//! renamed into place.

use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use sift_indexer::IndexOptions;

pub const FILE: &str = "roots.bin";
const HEADER: &[u8] = b"sift-roots 1\n";

/// Writes `roots` (path, saved options) to `dir/roots.bin`.
pub fn save(dir: &Path, roots: &[(PathBuf, Vec<u8>)]) -> std::io::Result<()> {
    let mut out = HEADER.to_vec();
    for (path, options) in roots {
        let path = path.as_os_str().as_bytes();
        for field in [path, options.as_slice()] {
            let len = u32::try_from(field.len()).map_err(std::io::Error::other)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(field);
        }
    }
    let target = dir.join(FILE);
    let tmp = dir.join(format!("{FILE}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, &target).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Reads `dir/roots.bin`. A missing or malformed file gives no roots.
pub fn load(dir: &Path) -> Vec<(PathBuf, IndexOptions)> {
    let Ok(bytes) = std::fs::read(dir.join(FILE)) else {
        return Vec::new();
    };
    parse(&bytes).unwrap_or_else(|| {
        tracing::warn!("{FILE} is malformed; ignoring it");
        Vec::new()
    })
}

fn parse(bytes: &[u8]) -> Option<Vec<(PathBuf, IndexOptions)>> {
    let mut rest = bytes.strip_prefix(HEADER)?;
    let mut field = || -> Option<&[u8]> {
        let (len, tail) = rest.split_first_chunk::<4>()?;
        let len = u32::from_le_bytes(*len) as usize;
        let (value, tail) = (tail.get(..len)?, &tail[len..]);
        rest = tail;
        Some(value)
    };
    let mut roots = Vec::new();
    while let Some(path) = field() {
        let options = field()?;
        let path = PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()));
        let options = IndexOptions::decode_saved(options).unwrap_or_default();
        roots.push((path, options));
    }
    rest.is_empty().then_some(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_paths_and_options() {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = IndexOptions {
            skip_dependency_dirs: false,
            exclude_paths: vec!["/Users/me/Projects/big".into()],
            ..IndexOptions::default()
        };
        let odd = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/odd\n\xffname".to_vec()));
        save(
            dir.path(),
            &[
                ("/Users/me/Documents".into(), options.encode_saved()),
                (odd.clone(), IndexOptions::default().encode_saved()),
            ],
        )
        .expect("save");
        let roots = load(dir.path());
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].0, Path::new("/Users/me/Documents"));
        assert!(!roots[0].1.skip_dependency_dirs);
        assert_eq!(roots[0].1.exclude_paths, options.exclude_paths);
        assert_eq!(roots[1].0, odd);
    }

    #[test]
    fn missing_or_malformed_files_give_no_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load(dir.path()).is_empty());
        std::fs::write(dir.path().join(FILE), b"sift-roots 1\n\x05\x00\x00\x00ab").expect("write");
        assert!(load(dir.path()).is_empty());
        std::fs::write(dir.path().join(FILE), b"something else").expect("write");
        assert!(load(dir.path()).is_empty());
    }
}
