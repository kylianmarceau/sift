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
    let mut w = store.replace_root(root, first_id, b"opts").unwrap();
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
        let mut w = store
            .replace_root(Path::new("/data/root"), 10, &[])
            .unwrap();
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
        .replace_root(Path::new("/data/root/sub"), 100, &[])
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
    let mut w = store.replace_root(Path::new("/data"), 1, &[]).unwrap();
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

#[test]
fn saves_options_with_each_root() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);
    assert_eq!(store.roots().unwrap()[0].options, b"opts");
}

#[test]
fn migrates_version_1_databases() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;
             CREATE TABLE roots (id INTEGER PRIMARY KEY, path BLOB NOT NULL UNIQUE,
                 entry_id INTEGER NOT NULL, entry_count INTEGER NOT NULL DEFAULT 0,
                 error_count INTEGER NOT NULL DEFAULT 0, indexed_at INTEGER NOT NULL DEFAULT 0,
                 scan_millis INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE entries (id INTEGER PRIMARY KEY, root_id INTEGER NOT NULL,
                 parent_id INTEGER, kind INTEGER NOT NULL, name BLOB NOT NULL,
                 size INTEGER, mtime INTEGER);
             INSERT INTO meta VALUES ('generation', 3);
             INSERT INTO roots (path, entry_id) VALUES (CAST('/old' AS BLOB), 1);
             INSERT INTO entries (id, root_id, kind, name) VALUES (1, 1, 1, CAST('/old' AS BLOB));
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }

    // A read-only handle cannot migrate but still reads the old layout.
    let old = Store::open_read_only(&db).unwrap();
    assert!(old.roots().unwrap()[0].options.is_empty());
    drop(old);

    let mut store = Store::open(&db).unwrap();
    assert_eq!(store.generation().unwrap(), 3);
    assert_eq!(store.roots().unwrap()[0].path, PathBuf::from("/old"));
    write_tree(&mut store, Path::new("/old"), 10);
    assert_eq!(store.roots().unwrap()[0].options, b"opts");
    let version: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn compacts_after_large_shrink() {
    let (dir, mut store) = temp_store();
    let big_name = vec![b'x'; 200];
    {
        let mut w = store.replace_root(Path::new("/big"), 1, &[]).unwrap();
        for i in 0..20_000 {
            w.insert(&NewEntry {
                id: 2 + i,
                parent_id: 1,
                kind: EntryKind::File,
                name: &big_name,
                size: Some(1),
                mtime: None,
            })
            .unwrap();
        }
        w.commit(ScanSummary::default()).unwrap();
    }
    store.checkpoint();
    assert!(
        !store.compact_if_fragmented().unwrap(),
        "nothing to reclaim yet"
    );
    let before = std::fs::metadata(dir.path().join("index.db"))
        .unwrap()
        .len();

    // remove_root compacts on its own once most pages are free.
    assert!(store.remove_root(Path::new("/big")).unwrap());
    let after = std::fs::metadata(dir.path().join("index.db"))
        .unwrap()
        .len();
    assert!(after * 4 < before, "{after} not much smaller than {before}");
}

fn root_id(store: &Store) -> RootId {
    store.roots().unwrap()[0].id
}

fn names(children: &[ChildEntry]) -> Vec<String> {
    let mut names: Vec<String> = children
        .iter()
        .map(|c| String::from_utf8_lossy(&c.name).into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn updater_lists_inserts_and_removes_subtrees() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);
    let root = root_id(&store);

    let mut update = store.begin_update(root).unwrap();
    assert_eq!(names(&update.children(1).unwrap()), ["a.txt", "sub"]);
    assert_eq!(update.child(1, b"sub").unwrap().unwrap().id, 3);
    assert!(update.child(1, b"missing").unwrap().is_none());

    // New ids continue above every existing id.
    let id = update.alloc_id();
    assert_eq!(id, 5);
    update
        .insert(&NewEntry {
            id,
            parent_id: 1,
            kind: EntryKind::File,
            name: b"new.md",
            size: Some(1),
            mtime: Some(1),
        })
        .unwrap();
    // Removing `sub` also removes `sub/b.rs`.
    assert_eq!(update.remove_subtree(3).unwrap(), 2);
    update.update_meta(2, Some(99), Some(5)).unwrap();
    let summary = update.commit(None).unwrap();
    assert_eq!(
        summary,
        UpdateSummary {
            inserted: 1,
            removed: 2,
            updated: 1
        }
    );

    assert_eq!(store.generation().unwrap(), 2);
    assert_eq!(store.entry_count().unwrap(), 3);
    assert_eq!(store.roots().unwrap()[0].entry_count, 3);
    assert_eq!(store.entry(2).unwrap().size, Some(99));
    assert!(matches!(store.entry(4), Err(StoreError::MissingEntry(4))));
    assert_eq!(
        store.path_resolver().resolve(5).unwrap(),
        PathBuf::from("/data/root/new.md")
    );
}

#[test]
fn update_without_changes_keeps_generation_but_records_event() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);
    let root = root_id(&store);
    let mark = EventMark {
        id: 42,
        uuid: "VOLUME-UUID".into(),
    };

    let summary = store
        .begin_update(root)
        .unwrap()
        .commit(Some(&mark))
        .unwrap();
    assert!(!summary.changed());
    assert_eq!(store.generation().unwrap(), 1);
    assert_eq!(store.roots().unwrap()[0].event, Some(mark));

    let later = EventMark {
        id: 50,
        uuid: "VOLUME-UUID".into(),
    };
    store.set_root_event(root, &later).unwrap();
    assert_eq!(
        store.root(Path::new("/data/root")).unwrap().unwrap().event,
        Some(later)
    );
}

#[test]
fn rolled_back_update_changes_nothing() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);
    let root = root_id(&store);
    {
        let mut update = store.begin_update(root).unwrap();
        update.remove_subtree(1).unwrap();
        // Dropped without commit.
    }
    assert_eq!(store.entry_count().unwrap(), 4);
    assert_eq!(store.generation().unwrap(), 1);
}

#[test]
fn migrates_version_2_databases_and_keeps_entries() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;
             CREATE TABLE roots (id INTEGER PRIMARY KEY, path BLOB NOT NULL UNIQUE,
                 entry_id INTEGER NOT NULL, entry_count INTEGER NOT NULL DEFAULT 0,
                 error_count INTEGER NOT NULL DEFAULT 0, indexed_at INTEGER NOT NULL DEFAULT 0,
                 scan_millis INTEGER NOT NULL DEFAULT 0, options BLOB);
             CREATE TABLE entries (id INTEGER PRIMARY KEY, root_id INTEGER NOT NULL,
                 parent_id INTEGER, kind INTEGER NOT NULL, name BLOB NOT NULL,
                 size INTEGER, mtime INTEGER);
             INSERT INTO meta VALUES ('generation', 7);
             INSERT INTO roots (path, entry_id, entry_count) VALUES (CAST('/v2' AS BLOB), 1, 2);
             INSERT INTO entries (id, root_id, kind, name) VALUES (1, 1, 1, CAST('/v2' AS BLOB));
             INSERT INTO entries (id, root_id, parent_id, kind, name)
                 VALUES (2, 1, 1, 0, CAST('kept.txt' AS BLOB));
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }

    // Read-only handles on the old layout report no event mark.
    let old = Store::open_read_only(&db).unwrap();
    assert_eq!(old.roots().unwrap()[0].event, None);
    drop(old);

    let mut store = Store::open(&db).unwrap();
    assert_eq!(store.generation().unwrap(), 7);
    let root = root_id(&store);
    let update = store.begin_update(root).unwrap();
    assert_eq!(names(&update.children(1).unwrap()), ["kept.txt"]);
    drop(update);
    let indexes: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'entries_parent'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 1);
}

#[test]
fn quick_check_passes_on_an_intact_store() {
    let (_dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);
    assert_eq!(store.quick_check().unwrap(), None);
}

#[test]
fn damaged_pages_are_detected() {
    let (dir, mut store) = temp_store();
    // Enough rows to span several pages.
    let mut w = store.replace_root(Path::new("/data/big"), 1, b"").unwrap();
    for i in 0..5000i64 {
        let name = format!("file-{i:05}-with-a-longer-name.txt");
        w.insert(&NewEntry {
            id: 2 + i,
            parent_id: 1,
            kind: EntryKind::File,
            name: name.as_bytes(),
            size: Some(i as u64),
            mtime: Some(1_700_000_000),
        })
        .unwrap();
    }
    w.commit(ScanSummary {
        entry_count: 5001,
        error_count: 0,
        scan_millis: 1,
    })
    .unwrap();
    store.checkpoint();
    let path = store.path().to_path_buf();
    drop(store);

    // Scribble over the middle of the file, as a disk or crash bug might.
    let mut bytes = std::fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    for b in &mut bytes[middle..middle + 8192] {
        *b = 0xA5;
    }
    std::fs::write(&path, &bytes).unwrap();

    let detected = match Store::open(&path) {
        Err(StoreError::Corrupt { .. }) => true,
        Err(other) => panic!("unexpected error: {other}"),
        Ok(store) => store.quick_check().map_or(true, |p| p.is_some()),
    };
    assert!(detected);
    drop(dir);
}

#[test]
fn move_aside_keeps_the_database_and_its_journal() {
    let (dir, mut store) = temp_store();
    write_tree(&mut store, Path::new("/data/root"), 1);
    let path = store.path().to_path_buf();
    drop(store);
    std::fs::write(dir.path().join("index.db-wal"), b"wal").unwrap();

    let moved = move_aside(&path).unwrap();
    assert!(!path.exists());
    assert!(moved.join("index.db").exists());
    assert!(moved.join("index.db-wal").exists());
    assert!(moved.starts_with(dir.path()));
    // A fresh store can be created in its place.
    assert_eq!(Store::open(&path).unwrap().entry_count().unwrap(), 0);
}
