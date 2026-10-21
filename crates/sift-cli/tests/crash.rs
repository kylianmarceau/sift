//! Kills `sift index` with SIGKILL part-way through a scan and checks the
//! index database afterwards: it must pass SQLite's integrity check, and
//! the root must be either absent or complete (a scan is one transaction).

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use sift_store::Store;

const FILES: usize = 20_000;
const FOLDERS: usize = 50;
/// Kill points, from just after start to after the scan commits.
const KILL_AFTER_MS: &[u64] = &[5, 10, 20, 30, 40, 50, 60, 80, 100, 150];

fn make_tree(root: &Path) {
    for dir in 0..FOLDERS {
        let dir_path = root.join(format!("folder-{dir:02}"));
        std::fs::create_dir_all(&dir_path).expect("create dir");
        for file in 0..FILES / FOLDERS {
            std::fs::File::create(dir_path.join(format!("file-{file:04}.txt"))).expect("create");
        }
    }
}

#[test]
fn a_killed_scan_leaves_a_consistent_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canonicalize");
    let root = base.join("tree");
    make_tree(&root);

    let mut complete = 0;
    let mut absent = 0;
    for &after in KILL_AFTER_MS {
        let index_dir = base.join(format!("index-{after}"));
        let mut child = Command::new(env!("CARGO_BIN_EXE_sift"))
            .args(["--index-dir"])
            .arg(&index_dir)
            .arg("index")
            .arg(&root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sift");
        std::thread::sleep(Duration::from_millis(after));
        // SIGKILL: no destructors, no rollback by the process itself.
        let _ = child.kill();
        let _ = child.wait();

        let db = index_dir.join("index.db");
        if !db.exists() {
            absent += 1;
            continue;
        }
        let store = Store::open(&db).expect("the database opens after a kill");
        assert_eq!(
            store.quick_check().expect("check"),
            None,
            "killed after {after} ms"
        );
        match store.roots().expect("roots").as_slice() {
            [] => absent += 1,
            [root_info] => {
                // Root, folders, files.
                let all = 1 + FOLDERS + FILES;
                assert_eq!(
                    root_info.entry_count as usize, all,
                    "killed after {after} ms"
                );
                assert_eq!(store.entry_count().expect("count") as usize, all);
                complete += 1;
            }
            more => panic!("unexpected roots: {}", more.len()),
        }
    }
    eprintln!(
        "{} kills: {complete} complete, {absent} absent (killed before commit), 0 partial",
        KILL_AFTER_MS.len()
    );
}
