use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use sift_indexer::{IndexOptions, index_root};
use sift_store::{EntryKind, Store};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    store: Store,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    // Canonicalize: on macOS the temp dir lives behind the /var symlink.
    let base = dir.path().canonicalize().expect("canonicalize");
    let root = base.join("root");
    fs::create_dir(&root).expect("create root");
    let store = Store::open(&base.join("index.db")).expect("open store");
    Fixture {
        _dir: dir,
        root,
        store,
    }
}

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, b"x").expect("write file");
}

/// All indexed paths relative to `root`, with their kinds.
fn indexed(store: &Store, root: &Path) -> BTreeMap<PathBuf, EntryKind> {
    let mut ids = Vec::new();
    store
        .for_each_entry(|row| ids.push((row.id, row.kind)))
        .expect("stream entries");
    let mut resolver = store.path_resolver();
    ids.into_iter()
        .map(|(id, kind)| {
            let path = resolver.resolve(id).expect("resolve");
            let rel = path.strip_prefix(root).expect("under root").to_path_buf();
            (rel, kind)
        })
        .collect()
}

fn options() -> IndexOptions {
    IndexOptions {
        threads: 4,
        ..IndexOptions::default()
    }
}

#[test]
fn indexes_files_and_directories() {
    let mut f = fixture();
    touch(&f.root.join("a.txt"));
    touch(&f.root.join("docs/report.pdf"));
    touch(&f.root.join("docs/deep/nested/notes.md"));
    fs::create_dir(f.root.join("empty")).unwrap();

    let report = index_root(&mut f.store, &f.root, &options(), |_| {}).unwrap();
    assert_eq!(report.files, 3);
    assert_eq!(report.dirs, 5); // root, docs, deep, nested, empty
    assert_eq!(report.entries, 8);
    assert_eq!(report.errors, 0);

    let paths = indexed(&f.store, &f.root);
    assert_eq!(paths.len(), 8);
    assert_eq!(paths[Path::new("")], EntryKind::Dir);
    assert_eq!(
        paths[Path::new("docs/deep/nested/notes.md")],
        EntryKind::File
    );
    assert_eq!(paths[Path::new("empty")], EntryKind::Dir);

    let record = f.store.entry(f.store.roots().unwrap()[0].entry_id).unwrap();
    assert_eq!(record.kind, EntryKind::Dir);
}

#[test]
fn child_ids_are_greater_than_parent_ids() {
    let mut f = fixture();
    for i in 0..20 {
        touch(&f.root.join(format!("d{i}/e{i}/file{i}")));
    }
    index_root(&mut f.store, &f.root, &options(), |_| {}).unwrap();
    f.store
        .for_each_entry(|row| {
            if let Some(parent) = row.parent_id {
                assert!(parent < row.id, "parent {parent} >= child {}", row.id);
            }
        })
        .unwrap();
}

#[test]
fn handles_unicode_spaces_and_unusual_names() {
    let mut f = fixture();
    let names = [
        "with space.txt",
        "café.txt",
        "cafe\u{301} decomposed.txt",
        "日本語のファイル.md",
        "emoji 🚀.png",
        "line\nbreak",
        "tab\there",
        "quote's \"double\".txt",
        "-leading-dash",
        ".hidden",
        "semi;colon&amp*star?.txt",
        "back\\slash",
    ];
    for name in names {
        touch(&f.root.join(name));
    }
    index_root(&mut f.store, &f.root, &options(), |_| {}).unwrap();
    let paths = indexed(&f.store, &f.root);
    for name in names {
        assert!(paths.contains_key(Path::new(name)), "missing {name:?}");
    }
}

#[test]
fn indexes_non_utf8_names_when_the_filesystem_allows_them() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let mut f = fixture();
    let name = OsStr::from_bytes(b"latin1-\xe9t\xe9.txt");
    if fs::write(f.root.join(name), b"x").is_err() {
        // APFS and HFS+ reject invalid UTF-8 names (EILSEQ); other
        // filesystems (exFAT, SMB, NFS) may accept them.
        eprintln!("skipping: filesystem rejects non-UTF-8 names");
        return;
    }
    index_root(&mut f.store, &f.root, &options(), |_| {}).unwrap();
    assert!(indexed(&f.store, &f.root).contains_key(Path::new(name)));
}

#[test]
fn duplicate_names_in_different_directories() {
    let mut f = fixture();
    for dir in ["a", "b", "c/d"] {
        touch(&f.root.join(dir).join("main.rs"));
    }
    index_root(&mut f.store, &f.root, &options(), |_| {}).unwrap();
    let paths = indexed(&f.store, &f.root);
    let mains: Vec<_> = paths
        .keys()
        .filter(|p| p.file_name().is_some_and(|n| n == "main.rs"))
        .collect();
    assert_eq!(mains.len(), 3);
}

#[test]
fn records_symlinks_without_following_them() {
    let mut f = fixture();
    touch(&f.root.join("real/file.txt"));
    // A cycle: root/real/loop -> root
    symlink(&f.root, f.root.join("real/loop")).unwrap();
    symlink("does-not-exist", f.root.join("broken")).unwrap();
    symlink(f.root.join("real/file.txt"), f.root.join("file-link")).unwrap();

    let report = index_root(&mut f.store, &f.root, &options(), |_| {}).unwrap();
    assert_eq!(report.symlinks, 3);
    assert_eq!(report.errors, 0);
    let paths = indexed(&f.store, &f.root);
    assert_eq!(paths[Path::new("real/loop")], EntryKind::Symlink);
    assert_eq!(paths[Path::new("broken")], EntryKind::Symlink);
    assert!(!paths.contains_key(Path::new("real/loop/real")));
    assert_eq!(paths.len(), 6);
}

#[test]
fn permission_denied_directories_are_reported_not_fatal() {
    let mut f = fixture();
    touch(&f.root.join("open/visible.txt"));
    touch(&f.root.join("locked/secret.txt"));
    let locked = f.root.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let result = index_root(&mut f.store, &f.root, &options(), |_| {});
    // Restore before asserting so the temp dir can always be cleaned up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let report = result.unwrap();

    if report.errors == 0 {
        // Running with elevated privileges bypasses the permission check.
        eprintln!("skipping: permission checks are not enforced for this user");
        return;
    }
    assert_eq!(report.errors, 1);
    assert_eq!(report.error_samples[0].path, locked);
    assert_eq!(
        report.error_samples[0].kind,
        std::io::ErrorKind::PermissionDenied
    );
    let paths = indexed(&f.store, &f.root);
    assert!(paths.contains_key(Path::new("locked")));
    assert!(paths.contains_key(Path::new("open/visible.txt")));
    assert!(!paths.contains_key(Path::new("locked/secret.txt")));
}

#[test]
fn honours_exclusions() {
    let mut f = fixture();
    touch(&f.root.join("src/main.rs"));
    touch(&f.root.join("node_modules/pkg/index.js"));
    touch(&f.root.join("sub/node_modules/x.js"));
    touch(&f.root.join("private/diary.txt"));
    touch(&f.root.join("keep/private"));

    let opts = IndexOptions {
        exclude_names: vec!["node_modules".into()],
        exclude_paths: vec![f.root.join("private"), PathBuf::from("/elsewhere")],
        ..options()
    };
    index_root(&mut f.store, &f.root, &opts, |_| {}).unwrap();
    let paths = indexed(&f.store, &f.root);
    assert!(paths.contains_key(Path::new("src/main.rs")));
    assert!(paths.contains_key(Path::new("keep/private")));
    assert!(paths.keys().all(|p| !p.starts_with("private")));
    assert!(
        paths
            .keys()
            .all(|p| !p.to_string_lossy().contains("node_modules"))
    );
}

#[test]
fn excluded_root_is_still_indexed_when_named_explicitly() {
    let mut f = fixture();
    touch(&f.root.join("file.txt"));
    let opts = IndexOptions {
        exclude_paths: vec![f.root.clone()],
        ..options()
    };
    let report = index_root(&mut f.store, &f.root, &opts, |_| {}).unwrap();
    assert_eq!(report.entries, 2);
}

#[test]
fn packages_are_opaque_by_default() {
    let mut f = fixture();
    touch(&f.root.join("Tool.app/Contents/Info.plist"));
    touch(&f.root.join("Lib.FRAMEWORK/Headers/lib.h"));
    touch(&f.root.join("not.a.package/file"));

    index_root(&mut f.store, &f.root, &options(), |_| {}).unwrap();
    let paths = indexed(&f.store, &f.root);
    assert_eq!(paths[Path::new("Tool.app")], EntryKind::Dir);
    assert!(!paths.contains_key(Path::new("Tool.app/Contents")));
    assert!(!paths.contains_key(Path::new("Lib.FRAMEWORK/Headers")));
    assert!(paths.contains_key(Path::new("not.a.package/file")));

    let opts = IndexOptions {
        descend_packages: true,
        ..options()
    };
    index_root(&mut f.store, &f.root, &opts, |_| {}).unwrap();
    let paths = indexed(&f.store, &f.root);
    assert!(paths.contains_key(Path::new("Tool.app/Contents/Info.plist")));
}

#[test]
fn reindex_reflects_renames_and_deletions() {
    let mut f = fixture();
    touch(&f.root.join("old-name.txt"));
    touch(&f.root.join("doomed/inner.txt"));
    index_root(&mut f.store, &f.root, &options(), |_| {}).unwrap();

    fs::rename(f.root.join("old-name.txt"), f.root.join("new-name.txt")).unwrap();
    fs::remove_dir_all(f.root.join("doomed")).unwrap();
    let report = index_root(&mut f.store, &f.root, &options(), |_| {}).unwrap();

    assert_eq!(report.entries, 2);
    let paths = indexed(&f.store, &f.root);
    assert!(paths.contains_key(Path::new("new-name.txt")));
    assert!(!paths.contains_key(Path::new("old-name.txt")));
    assert!(!paths.contains_key(Path::new("doomed")));
    assert_eq!(f.store.roots().unwrap().len(), 1);
    assert_eq!(f.store.entry_count().unwrap(), 2);
}

#[test]
fn rejects_missing_root_and_files() {
    let mut f = fixture();
    let missing = f.root.join("missing");
    assert!(index_root(&mut f.store, &missing, &options(), |_| {}).is_err());
    touch(&f.root.join("file"));
    assert!(matches!(
        index_root(&mut f.store, &f.root.join("file"), &options(), |_| {}),
        Err(sift_indexer::IndexError::NotADirectory(_))
    ));
}

#[test]
fn single_threaded_and_parallel_scans_agree() {
    let mut f = fixture();
    for i in 0..50 {
        touch(
            &f.root
                .join(format!("dir{}/sub{}/file{i}.txt", i % 7, i % 3)),
        );
    }
    let one = IndexOptions {
        threads: 1,
        ..options()
    };
    index_root(&mut f.store, &f.root, &one, |_| {}).unwrap();
    let serial = indexed(&f.store, &f.root);
    let many = IndexOptions {
        threads: 8,
        ..options()
    };
    index_root(&mut f.store, &f.root, &many, |_| {}).unwrap();
    assert_eq!(serial, indexed(&f.store, &f.root));
}
