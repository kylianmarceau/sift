use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::*;

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("index.db")).unwrap();
    (dir, store)
}

/// Writes a small tree: /data/root { a.txt, sub/ { b.rs } }.
fn write_tree(store: &mut Store, root: &Path, first_id: EntryId) {
    let mut w = store.replace_root(root, first_id).unwrap();
    let entries = [
        (first_id + 1, first_id, EntryKind::File, "a.txt"),
        (first_id + 2, first_id, EntryKind::Dir, "sub"),
        (first_id + 3, first_id + 2, EntryKind::File, "b.rs"),
    ];
    for (id, parent_id, kind, name) in entries {
        w.insert(&NewEntry {
            id,
            parent_id,
            kind,
            name: name.as_bytes(),
            size: (kind == EntryKind::File).then_some(10),
            mtime: Some(1_700_000_000),
        })
        .unwrap();
    }
    w.commit(ScanSummary {
        entry_count: 4,
        error_count: 0,
        scan_millis: 1,
    })
    .unwrap();
}

#[test]
fn new_store_is_empty() {
    let (_dir, store) = temp_store();
    assert_eq!(store.generation().unwrap(), 0);
    assert_eq!(store.entry_count().unwrap(), 0);
    assert_eq!(store.next_entry_id().unwrap(), 1);
    assert!(store.roots().unwrap().is_empty());
}

#[test]
fn writes_and_resolves_paths() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);

    assert_eq!(store.generation().unwrap(), 1);
    assert_eq!(store.entry_count().unwrap(), 4);
    let mut resolver = store.path_resolver();
    assert_eq!(resolver.resolve(1).unwrap(), PathBuf::from("/data/root"));
    assert_eq!(
        resolver.resolve(2).unwrap(),
        PathBuf::from("/data/root/a.txt")
    );
    assert_eq!(
        resolver.resolve(4).unwrap(),
        PathBuf::from("/data/root/sub/b.rs")
    );
    // Cached path is reused correctly.
    assert_eq!(
        resolver.resolve(3).unwrap(),
        PathBuf::from("/data/root/sub")
    );

    let record = store.entry(4).unwrap();
    assert_eq!(record.kind, EntryKind::File);
    assert_eq!(record.parent_id, Some(3));
    assert_eq!(record.size, Some(10));
    assert!(matches!(store.entry(99), Err(StoreError::MissingEntry(99))));
}

#[test]
fn persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    {
        let mut store = Store::open(&db).unwrap();
        write_tree(&mut store, Path::new("/data/root"), 1);
    }
    let store = Store::open_read_only(&db).unwrap();
    assert_eq!(store.entry_count().unwrap(), 4);
    assert_eq!(store.generation().unwrap(), 1);
    let roots = store.roots().unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].path, PathBuf::from("/data/root"));
    assert_eq!(roots[0].entry_count, 4);
}

#[test]
fn reindexing_replaces_root_contents() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);
    let next = store.next_entry_id().unwrap();
    write_tree(&mut store, Path::new("/data/root"), next);

    assert_eq!(store.entry_count().unwrap(), 4);
    assert_eq!(store.roots().unwrap().len(), 1);
    assert_eq!(store.generation().unwrap(), 2);
    assert!(store.entry(1).is_err());
}

#[test]
fn uncommitted_scan_keeps_previous_index() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);
    {
        let mut w = store.replace_root(Path::new("/data/root"), 10).unwrap();
        w.insert(&NewEntry {
            id: 11,
            parent_id: 10,
            kind: EntryKind::File,
            name: b"partial",
            size: None,
            mtime: None,
        })
        .unwrap();
        // Dropped without commit, as if the scan was interrupted.
    }
    assert_eq!(store.entry_count().unwrap(), 4);
    assert_eq!(store.generation().unwrap(), 1);
    assert_eq!(store.roots().unwrap()[0].entry_id, 1);
}

#[test]
fn rejects_nested_root_and_absorbs_children() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root/inner"), 1);
    write_tree(&mut store, Path::new("/data/other"), 10);

    let parent_id = store.next_entry_id().unwrap();
    write_tree(&mut store, Path::new("/data/root"), parent_id);
    let roots: Vec<_> = store.roots().unwrap().into_iter().map(|r| r.path).collect();
    assert_eq!(
        roots,
        [PathBuf::from("/data/other"), PathBuf::from("/data/root")]
    );

    let err = store
        .replace_root(Path::new("/data/root/sub"), 100)
        .err()
        .unwrap();
    assert!(matches!(err, StoreError::NestedRoot { .. }));

    // A sibling whose name merely shares a prefix is not nested.
    let next = store.next_entry_id().unwrap();
    write_tree(&mut store, Path::new("/data/rootless"), next);
    assert_eq!(store.roots().unwrap().len(), 3);
}

#[test]
fn removes_roots() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);
    assert!(store.remove_root(Path::new("/data/root")).unwrap());
    assert!(!store.remove_root(Path::new("/data/root")).unwrap());
    assert_eq!(store.entry_count().unwrap(), 0);
    assert_eq!(store.generation().unwrap(), 2);
}

#[test]
fn streams_entries_in_id_order() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);
    let mut seen = Vec::new();
    store
        .for_each_entry(|row| seen.push((row.id, row.parent_id, row.name.to_vec())))
        .unwrap();
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0], (1, None, b"/data/root".to_vec()));
    assert!(seen.windows(2).all(|w| w[0].0 < w[1].0));
}

#[test]
fn stores_non_utf8_and_unicode_names() {
    let (_dir, mut store) = temp_store();
    let mut w = store.replace_root(Path::new("/data"), 1).unwrap();
    let names: [&[u8]; 3] = [
        b"caf\xc3\xa9 menu.txt",
        b"bad\xff\xfename",
        "日本語 ファイル".as_bytes(),
    ];
    for (i, name) in names.iter().enumerate() {
        w.insert(&NewEntry {
            id: 2 + i as i64,
            parent_id: 1,
            kind: EntryKind::File,
            name,
            size: Some(0),
            mtime: None,
        })
        .unwrap();
    }
    w.commit(ScanSummary::default()).unwrap();

    let mut resolver = store.path_resolver();
    for (i, name) in names.iter().enumerate() {
        let path = resolver.resolve(2 + i as i64).unwrap();
        assert_eq!(path.file_name().unwrap(), OsStr::from_bytes(name));
    }
}

#[test]
fn reports_corrupt_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    std::fs::write(&db, vec![0x42u8; 8192]).unwrap();
    assert!(matches!(Store::open(&db), Err(StoreError::Corrupt { .. })));
    assert!(matches!(
        Store::open_read_only(&db),
        Err(StoreError::Corrupt { .. })
    ));
}

#[test]
fn read_only_open_of_missing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Store::open_read_only(&dir.path().join("missing.db")).is_err());
}
