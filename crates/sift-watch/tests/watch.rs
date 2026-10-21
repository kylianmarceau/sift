//! These tests use the real FSEvents service on temporary directories.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use sift_watch::{Event, Watcher, current_event_id, event_store_uuid};

const LATENCY: Duration = Duration::from_millis(50);
const TIMEOUT: Duration = Duration::from_secs(10);

fn temp_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    // FSEvents reports real paths (/private/var/...), not the /var symlink.
    let path = dir.path().canonicalize().expect("canonicalize");
    (dir, path)
}

fn watch(path: &Path, since: Option<u64>) -> (Watcher, Receiver<Event>) {
    let (tx, rx) = channel();
    let watcher = Watcher::start(&[path.to_path_buf()], since, LATENCY, move |events| {
        for event in events {
            let _ = tx.send(event);
        }
    })
    .expect("start watcher");
    (watcher, rx)
}

/// Waits for an event matching `want`, returning everything seen on the
/// way (or `None` on timeout).
fn wait_for(rx: &Receiver<Event>, want: impl Fn(&Event) -> bool) -> Option<Vec<Event>> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(event) => {
                let done = want(&event);
                seen.push(event);
                if done {
                    return Some(seen);
                }
            }
            Err(_) => return None,
        }
    }
    None
}

#[test]
fn reports_the_directory_of_a_new_file() {
    let (_guard, dir) = temp_dir();
    std::fs::create_dir(dir.join("docs")).unwrap();
    let (_watcher, rx) = watch(&dir, None);

    std::fs::write(dir.join("docs/report.txt"), b"hello").unwrap();
    let seen = wait_for(&rx, |e| e.path == dir.join("docs"));
    assert!(seen.is_some(), "no event for the changed directory");
}

#[test]
fn replays_changes_made_before_the_watcher_started() {
    let (_guard, dir) = temp_dir();
    let since = current_event_id();
    std::fs::write(dir.join("made-while-away.txt"), b"x").unwrap();
    // Let FSEvents record the change before asking for history.
    std::thread::sleep(Duration::from_millis(300));

    let (_watcher, rx) = watch(&dir, Some(since));
    let seen = wait_for(&rx, |e| e.flags.history_done()).expect("history done");
    assert!(
        seen.iter().any(|e| e.path == dir && e.id > since),
        "replayed history did not include the change: {seen:?}"
    );
}

#[test]
fn reports_a_deleted_root() {
    let (_guard, base) = temp_dir();
    let root = base.join("watched");
    std::fs::create_dir(&root).unwrap();
    let (_watcher, rx) = watch(&root, None);

    std::fs::remove_dir(&root).unwrap();
    let seen = wait_for(&rx, |e| e.flags.root_changed());
    assert!(seen.is_some(), "no root-changed event");
}

#[test]
fn event_ids_increase_and_volumes_have_an_event_store() {
    let (_guard, dir) = temp_dir();
    let before = current_event_id();
    std::fs::write(dir.join("a"), b"x").unwrap();
    let (watcher, rx) = watch(&dir, Some(before));
    let seen = wait_for(&rx, |e| e.flags.history_done()).expect("history done");
    assert!(seen.iter().all(|e| e.id >= before));
    watcher.flush();
    assert!(current_event_id() >= before);
    assert!(event_store_uuid(&dir).is_some_and(|u| u.len() == 36));
}

#[test]
fn no_callbacks_after_drop() {
    let (_guard, dir) = temp_dir();
    let (watcher, rx) = watch(&dir, None);
    drop(watcher);
    // The sender lives in the handler; dropping the watcher frees it, so
    // the channel disconnects instead of receiving more events.
    std::fs::write(dir.join("after-drop.txt"), b"x").unwrap();
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(1)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn rejects_an_empty_path_list() {
    assert!(Watcher::start(&[], None, LATENCY, |_| {}).is_err());
}
