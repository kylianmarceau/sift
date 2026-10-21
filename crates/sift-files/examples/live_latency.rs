//! Time from creating, renaming or deleting a file to the change showing up
//! in search, with the service's default settings and real FSEvents.
//!
//!     cargo run --release -p sift-files --example live_latency [files-in-tree]
//!
//! Works in a temporary directory, which it deletes afterwards.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sift_core::Provider;
use sift_files::{Command, FileIndex, FileProvider, IndexService, RootState, ServiceConfig};
use sift_indexer::IndexOptions;

const ROUNDS: usize = 20;

fn main() {
    let tree: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(10_000);
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().canonicalize().expect("canonicalize");
    let root = base.join("root");
    for i in 0..tree {
        let path = root.join(format!("d{}/e{}/file-{i}.txt", i % 100, i % 7));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"x").expect("write");
    }

    let index = Arc::new(FileIndex::open(&base.join("index")));
    let provider = FileProvider::new(index.clone());
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = ready.clone();
    let service = IndexService::start(
        ServiceConfig::new(base.join("index")),
        index.clone(),
        move |status| {
            if status.roots.iter().any(|r| r.state == RootState::Watching) {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        },
    )
    .expect("service");
    let started = Instant::now();
    service.send(Command::AddRoot {
        path: root.clone(),
        options: IndexOptions::default(),
    });
    while !ready.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("indexed {} entries in {:?}", index.len(), started.elapsed());
    std::thread::sleep(Duration::from_secs(1));

    let wait = |query: &str, present: bool| -> Duration {
        let started = Instant::now();
        loop {
            let found = provider.search(query, 5).iter().any(|r| &*r.title == query);
            if found == present {
                return started.elapsed();
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "timed out on {query}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    let measure = |label: &str, mut op: Box<dyn FnMut(usize) -> Duration>| {
        let mut times: Vec<f64> = (0..ROUNDS)
            .map(|i| {
                let t = op(i).as_secs_f64() * 1000.;
                // Separate rounds so batching does not merge them.
                std::thread::sleep(Duration::from_millis(600));
                t
            })
            .collect();
        times.sort_by(f64::total_cmp);
        println!(
            "{label:>8}: median {:.0} ms, p90 {:.0} ms, min {:.0} ms, max {:.0} ms ({ROUNDS} rounds)",
            times[ROUNDS / 2],
            times[ROUNDS * 9 / 10],
            times[0],
            times[ROUNDS - 1]
        );
    };
    let r = root.clone();
    measure(
        "create",
        Box::new(move |i| {
            let name = format!("created-{i}.md");
            std::fs::write(r.join("d1").join(&name), b"x").expect("create");
            wait(&name, true)
        }),
    );
    let r = root.clone();
    measure(
        "rename",
        Box::new(move |i| {
            let name = format!("renamed-{i}.md");
            rename(
                &r.join("d1").join(format!("created-{i}.md")),
                &r.join("d2").join(&name),
            );
            wait(&name, true)
        }),
    );
    let r = root.clone();
    measure(
        "delete",
        Box::new(move |i| {
            let name = format!("renamed-{i}.md");
            std::fs::remove_file(r.join("d2").join(&name)).expect("delete");
            // Deleted files vanish from results immediately (the provider
            // checks each result); wait for the index itself instead.
            let started = Instant::now();
            while index
                .search(&name, 5)
                .iter()
                .any(|r| r.path.ends_with(&name))
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            started.elapsed()
        }),
    );
    drop(service);
}

fn rename(from: &Path, to: &Path) {
    std::fs::rename(from, to).expect("rename");
}
