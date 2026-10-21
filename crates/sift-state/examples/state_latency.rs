//! Times the state database on a full history (500 recents, 20
//! favorites, 200 clipboard entries): what the launcher reads when the
//! query is empty, after each use, and when clipboard history changes.
//!
//! cargo run --release -p sift-state --example state_latency

use std::time::Instant;

use sift_core::{Action, ResultItem, ResultKind};
use sift_state::{MAX_USAGE_ROWS, StateDb};

const ROUNDS: usize = 200;

fn time(label: &str, mut f: impl FnMut()) {
    let mut times: Vec<f64> = (0..ROUNDS)
        .map(|_| {
            let started = Instant::now();
            f();
            started.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    times.sort_by(f64::total_cmp);
    println!(
        "{label:<34} median {:>7.1} µs   p95 {:>7.1} µs",
        times[ROUNDS / 2],
        times[ROUNDS * 95 / 100]
    );
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = StateDb::open(&dir.path().join("state.db")).expect("open");
    let item = |i: usize| ResultItem {
        id: format!("/Users/me/Documents/file-{i}.pdf").into(),
        kind: ResultKind::File,
        title: format!("file-{i}.pdf").into(),
        subtitle: Some("~/Documents".into()),
        score: 0,
        action: Action::OpenPath(format!("/Users/me/Documents/file-{i}.pdf").into()),
    };
    let now = sift_state::now();
    for i in 0..MAX_USAGE_ROWS {
        db.record_use(&item(i), now - i as i64 * 3600)
            .expect("record");
    }
    for i in 0..20 {
        db.toggle_favorite(&item(i * 7), now).expect("favorite");
    }
    for i in 0..200 {
        let text = format!("clipboard entry {i} ").repeat(20);
        db.clipboard_add(&text, now - i, 200).expect("clip");
    }
    let mut n = MAX_USAGE_ROWS;
    time("record_use (insert + prune)", || {
        n += 1;
        db.record_use(&item(n), now).expect("record");
    });
    time("recent(16)", || {
        db.recent(16, &[]).expect("recent");
    });
    time("favorites()", || {
        db.favorites(&[]).expect("favorites");
    });
    time("usage_boosts() over 500 rows", || {
        db.usage_boosts(now).expect("boosts");
    });
    time("clipboard_add (dedupe + cap)", || {
        n += 1;
        db.clipboard_add(&format!("new entry {n}"), now, 200)
            .expect("clip");
    });
    time("clipboard(200)", || {
        db.clipboard(200).expect("clipboard");
    });
}
