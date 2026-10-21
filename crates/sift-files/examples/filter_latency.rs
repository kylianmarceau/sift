//! Times filtered searches against an existing index (scan, resolve and
//! post-filter; the final existence check is left out, so synthetic
//! indexes whose paths are not on disk can be used).
//!
//! cargo run --release -p sift-files --example filter_latency -- <index-dir>

use std::time::Instant;

use sift_files::{FileIndex, FileQuery};

const ROUNDS: usize = 30;

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: filter_latency <index-dir>");
    let index = FileIndex::open(std::path::Path::new(&dir));
    println!("{} entries in {dir}", index.len());
    let queries = [
        "report",
        "report ext:pdf",
        "ext:pdf",
        "type:image",
        "report type:document",
        "report in:src",
        "report size:>1mb",
        "report modified:<30d",
        "a in:zzzz-no-such-folder",
    ];
    for text in queries {
        let parsed = FileQuery::parse(text).expect("valid");
        let mut times = Vec::with_capacity(ROUNDS);
        let mut count = 0;
        for _ in 0..ROUNDS {
            let started = Instant::now();
            count = match &parsed {
                Some(filters) => filters.run(&index, 40).len(),
                None => index.search(text, 40).len(),
            };
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{text:<28} median {:>7.2} ms   p95 {:>7.2} ms   {count} results",
            times[ROUNDS / 2],
            times[ROUNDS * 95 / 100]
        );
    }
}
