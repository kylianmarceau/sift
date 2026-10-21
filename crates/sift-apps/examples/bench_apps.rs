//! Times application discovery and provider search on this Mac's real
//! application folders.
//!
//!     cargo run --release -p sift-apps --example bench_apps

use std::time::Instant;

use sift_apps::{AppCatalog, AppProvider, Location};
use sift_core::Provider;

const QUERIES: &[&str] = &[
    "s",
    "te",
    "term",
    "cal",
    "vsc",
    "system settings",
    "xyzzy",
    "safari",
    "act mon",
];
const ROUNDS: usize = 200;

fn main() {
    let started = Instant::now();
    let catalog = AppCatalog::discover(&Location::defaults());
    let discover_ms = started.elapsed().as_secs_f64() * 1000.;
    println!("discovered {} apps in {discover_ms:.2} ms", catalog.len());

    let provider = AppProvider::new(catalog);
    for query in QUERIES {
        let mut times: Vec<f64> = (0..ROUNDS)
            .map(|_| {
                let started = Instant::now();
                let hits = provider.search(query, 40);
                std::hint::black_box(hits);
                started.elapsed().as_secs_f64() * 1_000_000.
            })
            .collect();
        times.sort_by(f64::total_cmp);
        let top = provider
            .search(query, 3)
            .into_iter()
            .map(|item| item.title.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{query:>16}: median {:>7.1} µs  p99 {:>7.1} µs  top: {top}",
            times[ROUNDS / 2],
            times[ROUNDS * 99 / 100],
        );
    }
}
