//! End-to-end: the service indexes a folder, FSEvents reports changes, and
//! the provider's results follow. Uses the real FSEvents service.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sift_core::{Provider, ResultKind};
use sift_files::{
    Command, FileIndex, FileProvider, IndexService, RootState, ServiceConfig, Status,
};
use sift_indexer::IndexOptions;

const TIMEOUT: Duration = Duration::from_secs(15);

struct Harness {
    _dir: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    index: Arc<FileIndex>,
    provider: FileProvider,
    service: Option<IndexService>,
    statuses: Receiver<Status>,
    latest: Arc<Mutex<Status>>,
}

fn config(base: &Path) -> ServiceConfig {
    ServiceConfig {
        latency: Duration::from_millis(50),
        quiet: Duration::from_millis(50),
        ..ServiceConfig::new(base.join("index"))
    }
}

fn start(
    base: &Path,
) -> (
    Arc<FileIndex>,
    IndexService,
    Receiver<Status>,
    Arc<Mutex<Status>>,
) {
    let index = Arc::new(FileIndex::open(&base.join("index")));
    let (tx, rx) = channel();
    let latest = Arc::new(Mutex::new(Status::default()));
    let keep = latest.clone();
    let service = IndexService::start(config(base), index.clone(), move |status| {
        *keep.lock().expect("status lock") = status.clone();
        let _ = tx.send(status.clone());
    })
    .expect("start service");
    (index, service, rx, latest)
}

fn harness(setup: impl FnOnce(&Path)) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().canonicalize().expect("canonicalize");
    let root = base.join("root");
    std::fs::create_dir(&root).expect("root");
    setup(&root);
    let (index, service, statuses, latest) = start(&base);
    service.send(Command::AddRoot {
        path: root.clone(),
        options: IndexOptions::default(),
    });
    let h = Harness {
        _dir: dir,
        base,
        root,
        provider: FileProvider::new(index.clone()),
        index,
        service: Some(service),
        statuses,
        latest,
    };
    h.wait_status(|s| s.roots.iter().any(|r| r.state == RootState::Watching));
    h
}

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, b"x").expect("write");
}

impl Harness {
    fn titles(&self, query: &str) -> Vec<String> {
        self.provider
            .search(query, 20)
            .into_iter()
            .map(|r| r.title.to_string())
            .collect()
    }

    /// Polls the provider until `want` holds; returns how long it took.
    fn wait_search(&self, query: &str, want: impl Fn(&[String]) -> bool) -> Duration {
        let started = Instant::now();
        while started.elapsed() < TIMEOUT {
            if want(&self.titles(query)) {
                return started.elapsed();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out; `{query}` gives {:?}", self.titles(query));
    }

    fn wait_status(&self, want: impl Fn(&Status) -> bool) -> Status {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            {
                let latest = self.latest.lock().expect("status lock");
                if want(&latest) {
                    return latest.clone();
                }
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for status");
            let _ = self
                .statuses
                .recv_timeout(left.min(Duration::from_millis(100)));
        }
    }

    fn send(&self, command: Command) {
        self.service.as_ref().expect("running").send(command);
    }
}

#[test]
fn initial_scan_makes_files_searchable() {
    let h = harness(|root| {
        touch(&root.join("quarterly-report.pdf"));
        touch(&root.join("projects/rust/notes.md"));
    });
    assert_eq!(h.titles("quarterly"), ["quarterly-report.pdf"]);
    let results = h.provider.search("rust", 10);
    assert_eq!(results[0].kind, ResultKind::Folder);
    assert_eq!(
        results[0].subtitle.as_deref(),
        Some(h.root.join("projects").to_string_lossy().as_ref())
    );
    let status = h.latest.lock().unwrap().clone();
    assert_eq!(status.roots.len(), 1);
    assert_eq!(status.roots[0].entries, 5); // root, projects, rust, 2 files
    assert!(status.index_bytes > 0);
}

#[test]
fn created_deleted_and_renamed_files_follow_the_filesystem() {
    let h = harness(|root| touch(&root.join("old-name.txt")));

    touch(&h.root.join("brand-new-file.txt"));
    let took = h.wait_search("brand-new", |t| t == ["brand-new-file.txt"]);
    eprintln!("create → searchable: {took:?}");

    std::fs::rename(h.root.join("old-name.txt"), h.root.join("renamed.txt")).unwrap();
    h.wait_search("renamed", |t| t == ["renamed.txt"]);
    h.wait_search("old-name", |t| t.is_empty());

    std::fs::remove_file(h.root.join("brand-new-file.txt")).unwrap();
    h.wait_search("brand-new", |t| t.is_empty());

    let status = h.wait_status(|s| s.last_update.is_some());
    let update = status.last_update.unwrap();
    assert!(update.changes.changed());
}

#[test]
fn folder_moved_in_brings_its_contents() {
    let h = harness(|root| touch(&root.join("a.txt")));
    let outside = h.base.join("outside");
    touch(&outside.join("album/track-01.flac"));
    touch(&outside.join("album/track-02.flac"));
    std::fs::rename(outside.join("album"), h.root.join("album")).unwrap();
    h.wait_search("track", |t| t.len() == 2);
}

#[test]
fn files_deleted_before_the_update_lands_are_never_offered() {
    let h = harness(|root| touch(&root.join("ephemeral.log")));
    // Delete and search immediately, before FSEvents reports it: the
    // provider checks results against the filesystem.
    std::fs::remove_file(h.root.join("ephemeral.log")).unwrap();
    assert!(h.titles("ephemeral").is_empty());
}

#[test]
fn changes_made_while_stopped_are_replayed_without_a_rescan() {
    let mut h = harness(|root| touch(&root.join("before.txt")));
    let scanned_at = h.latest.lock().unwrap().roots[0].indexed_at;
    // Let the event position be saved, then stop the service.
    touch(&h.root.join("while-running.txt"));
    h.wait_search("while-running", |t| t.len() == 1);
    drop(h.service.take());

    touch(&h.root.join("made-while-stopped.txt"));
    std::fs::remove_file(h.root.join("before.txt")).unwrap();
    // Full scans record whole seconds; make a rescan detectable.
    std::thread::sleep(Duration::from_millis(1100));

    let (index, service, statuses, latest) = start(&h.base);
    h.provider = FileProvider::new(index.clone());
    h.index = index;
    h.service = Some(service);
    h.statuses = statuses;
    h.latest = latest;
    h.wait_search("made-while-stopped", |t| t.len() == 1);
    h.wait_search("before", |t| t.is_empty());
    let status = h.wait_status(|s| !s.roots.is_empty());
    assert_eq!(
        status.roots[0].indexed_at, scanned_at,
        "the root was rescanned instead of replaying events"
    );
}

#[test]
fn a_deleted_root_becomes_unavailable_and_keeps_its_index() {
    let h = harness(|root| touch(&root.join("keepsake.txt")));
    let moved = h.base.join("moved-away");
    std::fs::rename(&h.root, &moved).unwrap();
    h.wait_status(|s| s.roots.iter().any(|r| r.state == RootState::Unavailable));
    // Still in the index, but not offered: the file is not at its path.
    assert!(h.titles("keepsake").is_empty());
    assert_eq!(h.index.len(), 2);

    // Back where it was: rescanned and watched again.
    std::fs::rename(&moved, &h.root).unwrap();
    h.send(Command::Refresh);
    h.wait_status(|s| s.roots.iter().any(|r| r.state == RootState::Watching));
    h.wait_search("keepsake", |t| t.len() == 1);
}

#[test]
fn removing_a_root_removes_its_files() {
    let h = harness(|root| touch(&root.join("temporary.txt")));
    h.send(Command::RemoveRoot(h.root.clone()));
    h.wait_status(|s| s.roots.is_empty());
    assert!(h.titles("temporary").is_empty());
    assert!(h.index.is_empty());
}

#[test]
fn changing_options_rescans_with_them() {
    let h = harness(|root| touch(&root.join("app/node_modules/pkg/index.js")));
    assert!(h.titles("index.js").is_empty());
    h.send(Command::SetOptions {
        path: h.root.clone(),
        options: IndexOptions {
            skip_dependency_dirs: false,
            ..IndexOptions::default()
        },
    });
    h.wait_search("index.js", |t| t.len() == 1);
}

#[test]
fn short_queries_return_no_files() {
    let h = harness(|root| touch(&root.join("a.txt")));
    assert!(h.titles("a").is_empty());
    assert_eq!(h.titles("a.txt"), ["a.txt"]);
}

#[test]
fn filters_narrow_file_results() {
    let h = harness(|root| {
        touch(&root.join("docs/report.pdf"));
        touch(&root.join("docs/report.txt"));
        touch(&root.join("photos/report.jpg"));
        std::fs::write(
            root.join("photos/big-report.png"),
            vec![0u8; 2 * 1024 * 1024],
        )
        .expect("write big file");
        touch(&root.join("archive/old-report.pdf"));
        let old = std::time::SystemTime::now() - Duration::from_secs(400 * 24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(root.join("archive/old-report.pdf"))
            .and_then(|f| f.set_modified(old))
            .expect("age file");
    });
    let sorted = |query: &str| {
        let mut titles = h.titles(query);
        titles.sort();
        titles
    };
    assert_eq!(sorted("report ext:pdf"), ["old-report.pdf", "report.pdf"]);
    assert_eq!(sorted("ext:pdf"), ["old-report.pdf", "report.pdf"]);
    assert_eq!(
        sorted("report type:image"),
        ["big-report.png", "report.jpg"]
    );
    assert_eq!(sorted("report in:docs"), ["report.pdf", "report.txt"]);
    assert_eq!(sorted("report size:>1mb"), ["big-report.png"]);
    assert_eq!(sorted("report ext:pdf modified:>1y"), ["old-report.pdf"]);
    assert_eq!(sorted("report ext:pdf modified:<7d"), ["report.pdf"]);
    assert_eq!(sorted("docs type:folder"), ["docs"]);
    // Nothing to search for, or a value that is not understood.
    assert!(h.titles("in:docs").is_empty());
    assert!(h.titles("report size:huge").is_empty());
}

#[test]
fn a_damaged_index_is_rebuilt_from_the_saved_roots() {
    let mut h = harness(|root| touch(&root.join("survivor.txt")));
    let index_dir = h.base.join("index");
    assert!(index_dir.join("roots.bin").exists());
    drop(h.service.take());

    // Replace the database with garbage, as a disk error might.
    for name in ["index.db-wal", "index.db-shm"] {
        let _ = std::fs::remove_file(index_dir.join(name));
    }
    std::fs::write(index_dir.join("index.db"), vec![0x5a; 64 * 1024]).unwrap();

    let (index, service, statuses, latest) = start(&h.base);
    h.provider = FileProvider::new(index.clone());
    h.index = index;
    h.service = Some(service);
    h.statuses = statuses;
    h.latest = latest;
    let status = h.wait_status(|s| {
        s.message.as_deref().is_some_and(|m| m.contains("damaged"))
            && s.roots.iter().any(|r| r.state == RootState::Watching)
    });
    assert_eq!(status.roots[0].path, h.root);
    h.wait_search("survivor", |t| t == ["survivor.txt"]);
    // The damaged copy is kept, not deleted.
    let kept = std::fs::read_dir(&index_dir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| {
            e.file_name().to_string_lossy().starts_with("damaged-")
                && e.path().join("index.db").exists()
        });
    assert!(kept);
}

#[test]
fn a_verified_start_keeps_an_intact_index() {
    let mut h = harness(|root| touch(&root.join("kept.txt")));
    drop(h.service.take());
    let index = Arc::new(FileIndex::open(&h.base.join("index")));
    let config = ServiceConfig {
        verify: true,
        ..config(&h.base)
    };
    let (tx, rx) = channel();
    let service = IndexService::start(config, index.clone(), move |status| {
        let _ = tx.send(status.clone());
    })
    .expect("start");
    let status = rx.recv_timeout(TIMEOUT).expect("status");
    assert_eq!(status.message, None);
    assert_eq!(status.roots.len(), 1);
    assert_eq!(FileProvider::new(index).search("kept", 5).len(), 1);
    drop(service);
}

/// A small deterministic generator (xorshift64*), so a failing run can be
/// repeated with its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Every folder and file under `dir` (relative), sorted.
fn walk(dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut folders = vec![PathBuf::new()];
    let mut files = Vec::new();
    let mut i = 0;
    while i < folders.len() {
        let current = dir.join(&folders[i]);
        if let Ok(entries) = std::fs::read_dir(&current) {
            for entry in entries.filter_map(Result::ok) {
                let rel = folders[i].join(entry.file_name());
                match entry.file_type() {
                    Ok(t) if t.is_dir() => folders.push(rel),
                    Ok(_) => files.push(rel),
                    Err(_) => {}
                }
            }
        }
        i += 1;
    }
    folders.sort();
    files.sort();
    (folders, files)
}

/// One random filesystem operation inside `root` (or between `root` and
/// `outside`). Failures (a name already taken, say) are fine: whatever
/// happens on disk is what the index must end up matching.
fn random_operation(rng: &mut Rng, root: &Path, outside: &Path, counter: &mut u64) {
    let (folders, files) = walk(root);
    let folder = &folders[rng.below(folders.len())];
    *counter += 1;
    let name = format!(
        "n{counter}-{}",
        ["a", "report", "Ünïcode", "with space", "x.rs"][rng.below(5)]
    );
    // Weighted towards creating, so the tree grows over the run.
    match rng.below(14) {
        0..=3 => {
            let _ = std::fs::write(root.join(folder).join(format!("{name}.txt")), b"hello");
        }
        4 => {
            if !files.is_empty() {
                let file = &files[rng.below(files.len())];
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .append(true)
                    .open(root.join(file))
                {
                    use std::io::Write;
                    let _ = f.write_all(&vec![b'z'; 1 + rng.below(4000)]);
                }
            }
        }
        5 => {
            if !files.is_empty() {
                let _ = std::fs::remove_file(root.join(&files[rng.below(files.len())]));
            }
        }
        6..=8 => {
            let _ = std::fs::create_dir_all(root.join(folder).join(&name).join("inner"));
            let _ = std::fs::write(root.join(folder).join(&name).join("inner/deep.md"), b"d");
        }
        9 => {
            // Only nested folders, so the tree keeps growing.
            let nested: Vec<&PathBuf> = folders
                .iter()
                .filter(|f| f.components().count() >= 2)
                .collect();
            if !nested.is_empty() {
                let gone = nested[rng.below(nested.len())];
                let _ = std::fs::remove_dir_all(root.join(gone));
            }
        }
        10 => {
            if !files.is_empty() {
                let file = &files[rng.below(files.len())];
                let _ = std::fs::rename(root.join(file), root.join(folder).join(&name));
            }
        }
        11 => {
            if folders.len() > 1 {
                let moved = &folders[1 + rng.below(folders.len() - 1)];
                // Not into itself or one of its own subfolders.
                if !folder.starts_with(moved) {
                    let _ = std::fs::rename(root.join(moved), root.join(folder).join(&name));
                }
            }
        }
        12 => {
            if folders.len() > 1 {
                let moved = &folders[1 + rng.below(folders.len() - 1)];
                let _ = std::fs::rename(root.join(moved), outside.join(&name));
            }
        }
        _ => {
            let (_, away) = walk(outside);
            let (away_folders, _) = walk(outside);
            if away_folders.len() > 1 {
                let back = &away_folders[1 + rng.below(away_folders.len() - 1)];
                let _ = std::fs::rename(outside.join(back), root.join(folder).join(&name));
            } else if !away.is_empty() {
                let _ = std::fs::rename(outside.join(&away[0]), root.join(folder).join(&name));
            }
        }
    }
}

#[test]
fn random_edits_through_fsevents_end_in_an_index_that_matches_the_disk() {
    let seed: u64 = std::env::var("SIFT_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_2026);
    let operations: usize = std::env::var("SIFT_FUZZ_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(400);
    eprintln!("seed {seed}, {operations} operations (SIFT_FUZZ_SEED / SIFT_FUZZ_OPS)");
    let h = harness(|root| {
        for dir in ["docs", "src/app", "photos/2024"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            touch(&root.join(dir).join("start.txt"));
        }
    });
    let outside = h.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let mut rng = Rng(seed);
    let mut counter = 0;
    let mut done = 0;
    while done < operations {
        // Bursts of edits with pauses between them, like real work.
        let burst = 1 + rng.below(25);
        for _ in 0..burst.min(operations - done) {
            random_operation(&mut rng, &h.root, &outside, &mut counter);
            done += 1;
        }
        std::thread::sleep(Duration::from_millis(rng.below(300) as u64));
    }

    // Once the service catches up, the index must match a fresh scan.
    let db = h.base.join("index").join(sift_search::DB_FILE);
    let scratch = h.base.join("fresh.db");
    let started = Instant::now();
    let mut last = None;
    while started.elapsed() < TIMEOUT {
        std::thread::sleep(Duration::from_millis(250));
        let store = sift_store::Store::open_read_only(&db).expect("open index");
        let root = store.root(&h.root).expect("roots").expect("root");
        let diff =
            sift_indexer::compare_with_disk(&store, &root, &IndexOptions::default(), &scratch)
                .expect("compare");
        assert_eq!(store.verify().expect("verify"), Vec::<String>::new());
        if diff.is_empty() {
            eprintln!(
                "matches the disk ({} entries) {:?} after the last edit",
                diff.on_disk,
                started.elapsed()
            );
            return;
        }
        last = Some(diff);
    }
    let diff = last.expect("compared at least once");
    let show = |paths: &[Vec<u8>]| {
        paths
            .iter()
            .take(10)
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect::<Vec<_>>()
    };
    panic!(
        "seed {seed}: index never matched the disk. missing {:?}, extra {:?}, changed {:?}",
        show(&diff.missing),
        show(&diff.extra),
        show(&diff.changed)
    );
}
