//! Incremental updates must leave the index exactly as a fresh full scan
//! would. Most tests therefore change the filesystem, apply a change, and
//! compare against a second store indexed from scratch.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sift_indexer::{Change, IndexError, IndexOptions, apply_changes, index_root, is_watched};
use sift_store::{EntryKind, EventMark, RootInfo, Store};

struct Fixture {
    dir: tempfile::TempDir,
    root: PathBuf,
    store: Store,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().canonicalize().expect("canonicalize");
    let root = base.join("root");
    fs::create_dir(&root).expect("create root");
    let store = Store::open(&base.join("index.db")).expect("open store");
    Fixture { dir, root, store }
}

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, b"x").expect("write file");
}

fn options() -> IndexOptions {
    IndexOptions {
        threads: 2,
        ..IndexOptions::default()
    }
}

/// Indexed paths relative to the root, with kinds and file sizes.
fn indexed(store: &Store, root: &Path) -> BTreeMap<PathBuf, (EntryKind, Option<u64>)> {
    let mut ids = Vec::new();
    store
        .for_each_entry(|row| ids.push(row.id))
        .expect("stream entries");
    let mut resolver = store.path_resolver();
    ids.into_iter()
        .map(|id| {
            let path = resolver.resolve(id).expect("resolve");
            let record = store.entry(id).expect("entry");
            let rel = path.strip_prefix(root).expect("under root").to_path_buf();
            (rel, (record.kind, record.size))
        })
        .collect()
}

/// What a fresh full scan of `root` produces.
fn fresh(f: &Fixture) -> BTreeMap<PathBuf, (EntryKind, Option<u64>)> {
    let path = f.dir.path().join(format!("fresh-{}.db", rand_suffix()));
    let mut store = Store::open(&path).expect("fresh store");
    index_root(&mut store, &f.root, &options(), |_| {}).expect("fresh index");
    indexed(&store, &f.root)
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos()
}

fn root_info(f: &Fixture) -> RootInfo {
    f.store.roots().expect("roots").remove(0)
}

fn apply(changes: &[Change], f: &mut Fixture) -> sift_indexer::UpdateReport {
    let root = root_info(f);
    apply_changes(&mut f.store, &root, &options(), changes, None).expect("apply")
}

fn assert_matches_fresh_scan(f: &Fixture) {
    assert_eq!(indexed(&f.store, &f.root), fresh(f));
    // The snapshot builder needs every child id above its parent's.
    let mut rows = Vec::new();
    f.store
        .for_each_entry(|row| rows.push((row.id, row.parent_id)))
        .expect("stream entries");
    for (id, parent) in rows {
        if let Some(parent) = parent {
            assert!(parent < id, "child {id} has parent {parent}");
        }
    }
    let total = f.store.entry_count().expect("entry count");
    assert_eq!(root_info(f).entry_count, total, "root entry count drifted");
}

fn indexed_fixture(setup: impl FnOnce(&Path)) -> Fixture {
    let mut f = fixture();
    setup(&f.root);
    index_root(&mut f.store, &f.root, &options(), |_| {}).expect("index");
    f
}

#[test]
fn new_modified_and_deleted_files() {
    let mut f = indexed_fixture(|root| {
        touch(&root.join("keep.txt"));
        touch(&root.join("gone.txt"));
        touch(&root.join("grow.txt"));
    });
    touch(&f.root.join("new.txt"));
    fs::remove_file(f.root.join("gone.txt")).unwrap();
    fs::write(f.root.join("grow.txt"), b"much longer contents").unwrap();
    let generation = f.store.generation().unwrap();

    let report = apply(&[Change::dir(&f.root)], &mut f);
    assert_eq!(report.summary.inserted, 1);
    assert_eq!(report.summary.removed, 1);
    assert_eq!(report.summary.updated, 1);
    assert_eq!(f.store.generation().unwrap(), generation + 1);
    assert_matches_fresh_scan(&f);
}

#[test]
fn renamed_file_and_folder() {
    let mut f = indexed_fixture(|root| {
        touch(&root.join("draft.md"));
        touch(&root.join("old-name/inner/deep.txt"));
    });
    fs::rename(f.root.join("draft.md"), f.root.join("final.md")).unwrap();
    fs::rename(f.root.join("old-name"), f.root.join("new-name")).unwrap();

    apply(&[Change::dir(&f.root)], &mut f);
    let paths = indexed(&f.store, &f.root);
    assert!(paths.contains_key(Path::new("new-name/inner/deep.txt")));
    assert!(!paths.contains_key(Path::new("old-name")));
    assert_matches_fresh_scan(&f);
}

#[test]
fn folder_moved_in_is_indexed_with_its_contents() {
    let mut f = indexed_fixture(|root| touch(&root.join("a.txt")));
    // Built outside the root, then moved in: no events for its contents.
    let outside = f.dir.path().canonicalize().unwrap().join("staging");
    touch(&outside.join("project/src/main.rs"));
    touch(&outside.join("project/README.md"));
    fs::rename(outside.join("project"), f.root.join("project")).unwrap();

    let report = apply(&[Change::dir(&f.root)], &mut f);
    assert_eq!(report.summary.inserted, 4); // project, src, main.rs, README.md
    assert_matches_fresh_scan(&f);
}

#[test]
fn deleted_folder_reported_by_its_own_path() {
    let mut f = indexed_fixture(|root| {
        touch(&root.join("photos/2024/a.jpg"));
        touch(&root.join("photos/2024/b.jpg"));
    });
    fs::remove_dir_all(f.root.join("photos/2024")).unwrap();

    // The event names the deleted folder itself.
    let report = apply(&[Change::dir(f.root.join("photos/2024"))], &mut f);
    assert_eq!(report.summary.removed, 3);
    assert_matches_fresh_scan(&f);
}

#[test]
fn change_below_an_unindexed_folder_lists_the_nearest_indexed_ancestor() {
    let mut f = indexed_fixture(|root| touch(&root.join("a.txt")));
    touch(&f.root.join("x/y/z/file.txt"));

    // The event names the deepest new folder, which is not indexed yet.
    apply(&[Change::dir(f.root.join("x/y/z"))], &mut f);
    assert_matches_fresh_scan(&f);
}

#[test]
fn recursive_change_finds_deep_modifications() {
    let mut f = indexed_fixture(|root| touch(&root.join("a/b/c/data.bin")));
    fs::write(f.root.join("a/b/c/data.bin"), vec![0u8; 4096]).unwrap();

    // A shallow change of the root does not look that deep...
    let report = apply(&[Change::dir(&f.root)], &mut f);
    assert!(!report.summary.changed());
    // ...a recursive one does.
    let report = apply(&[Change::tree(&f.root)], &mut f);
    assert_eq!(report.summary.updated, 1);
    assert_matches_fresh_scan(&f);
}

#[test]
fn file_replaced_by_folder_of_the_same_name() {
    let mut f = indexed_fixture(|root| touch(&root.join("thing")));
    fs::remove_file(f.root.join("thing")).unwrap();
    touch(&f.root.join("thing/inside.txt"));

    apply(&[Change::dir(&f.root)], &mut f);
    assert_eq!(
        indexed(&f.store, &f.root)[Path::new("thing")].0,
        EntryKind::Dir
    );
    assert_matches_fresh_scan(&f);
}

#[test]
fn unicode_and_spaces_in_names() {
    let mut f = indexed_fixture(|root| touch(&root.join("plain.txt")));
    touch(&f.root.join("Café menu (final) 📄.pdf"));
    touch(&f.root.join("日本語/ノート.txt"));

    apply(&[Change::dir(&f.root)], &mut f);
    assert_matches_fresh_scan(&f);
}

#[test]
fn unchanged_tree_changes_nothing() {
    let mut f = indexed_fixture(|root| {
        touch(&root.join("a.txt"));
        touch(&root.join("d/b.txt"));
    });
    let generation = f.store.generation().unwrap();
    let report = apply(&[Change::tree(&f.root)], &mut f);
    assert!(!report.summary.changed());
    assert_eq!(f.store.generation().unwrap(), generation);
}

#[test]
fn dependency_folders_and_exclusions_stay_unindexed() {
    let mut f = indexed_fixture(|root| touch(&root.join("app/package.json")));
    touch(&f.root.join("app/node_modules/left-pad/index.js"));
    touch(&f.root.join("app/.DS_Store"));

    let opts = IndexOptions {
        exclude_names: vec![".DS_Store".into()],
        ..options()
    };
    // Changes inside the dependency folder are ignored outright.
    assert!(!is_watched(
        &f.root,
        &opts,
        &f.root.join("app/node_modules/left-pad")
    ));
    assert!(is_watched(&f.root, &opts, &f.root.join("app")));

    let root = root_info(&f);
    apply_changes(
        &mut f.store,
        &root,
        &opts,
        &[
            Change::dir(f.root.join("app")),
            Change::dir(f.root.join("app/node_modules/left-pad")),
        ],
        None,
    )
    .unwrap();
    let paths = indexed(&f.store, &f.root);
    // The folder itself is recorded (and findable); its contents are not.
    assert!(paths.contains_key(Path::new("app/node_modules")));
    assert!(!paths.contains_key(Path::new("app/node_modules/left-pad")));
    assert!(!paths.contains_key(Path::new("app/.DS_Store")));
}

#[test]
fn changes_outside_the_root_are_ignored() {
    let mut f = indexed_fixture(|root| touch(&root.join("a.txt")));
    let report = apply(&[Change::dir("/somewhere/else")], &mut f);
    assert_eq!(report.ignored, 1);
    assert!(!report.summary.changed());
}

#[test]
fn unreadable_folder_keeps_its_previous_entries() {
    let mut f = indexed_fixture(|root| touch(&root.join("private/secret.txt")));
    let private = f.root.join("private");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();

    let report = apply(&[Change::dir(&private)], &mut f);
    fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(report.errors, 1);
    assert!(indexed(&f.store, &f.root).contains_key(Path::new("private/secret.txt")));
}

#[test]
fn missing_root_leaves_the_index_untouched() {
    let mut f = indexed_fixture(|root| touch(&root.join("a.txt")));
    let before = indexed(&f.store, &f.root);
    let root = root_info(&f);
    fs::remove_dir_all(&f.root).unwrap();

    let result = apply_changes(
        &mut f.store,
        &root,
        &options(),
        &[Change::dir(&f.root)],
        None,
    );
    assert!(matches!(result, Err(IndexError::RootUnavailable(_))));
    assert_eq!(indexed(&f.store, &f.root), before);
}

#[test]
fn records_the_event_position_with_the_changes() {
    let mut f = indexed_fixture(|root| touch(&root.join("a.txt")));
    touch(&f.root.join("b.txt"));
    let mark = EventMark {
        id: 1234,
        uuid: "TEST-UUID".into(),
    };
    let root = root_info(&f);
    apply_changes(
        &mut f.store,
        &root,
        &options(),
        &[Change::dir(&f.root)],
        Some(&mark),
    )
    .unwrap();
    assert_eq!(root_info(&f).event, Some(mark));
}

#[test]
fn many_random_edits_converge_to_a_fresh_scan() {
    let mut f = indexed_fixture(|root| {
        for i in 0..30 {
            touch(&root.join(format!("d{}/sub{}/f{i}.txt", i % 5, i % 3)));
        }
    });
    // Deterministic pseudo-random edits.
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut changed_dirs = Vec::new();
    for step in 0..60 {
        let dir = f.root.join(format!("d{}", next() % 6));
        match next() % 4 {
            0 => touch(&dir.join(format!("new{step}.txt"))),
            1 => {
                let _ = fs::remove_dir_all(dir.join(format!("sub{}", next() % 3)));
            }
            2 => {
                // FSEvents reports every directory whose contents changed.
                let sub = dir.join(format!("sub{}", next() % 4));
                touch(&sub.join(format!("n{step}/x.txt")));
                changed_dirs.push(Change::dir(&sub));
                changed_dirs.push(Change::dir(sub.join(format!("n{step}"))));
            }
            _ => {
                let from = dir.join(format!("sub{}", next() % 3));
                let to = f.root.join(format!("d{}/moved{step}", next() % 5));
                if let Some(parent) = to.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::rename(&from, &to);
                changed_dirs.push(Change::dir(to.parent().unwrap()));
            }
        }
        changed_dirs.push(Change::dir(&dir));
    }
    // Directory-level events, as FSEvents reports them (deepest first is
    // fine: order does not matter).
    changed_dirs.reverse();
    changed_dirs.push(Change::dir(&f.root));
    apply(&changed_dirs, &mut f);
    let report = apply(&[Change::tree(&f.root)], &mut f);
    assert!(
        !report.summary.changed(),
        "directory-level changes missed something: {:?}",
        report.summary
    );
    assert_matches_fresh_scan(&f);
}
