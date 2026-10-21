//! Benchmark commands: synthetic dataset generation and query timing.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sift_search::{Query, SearchIndex};
use sift_store::{EntryKind, NewEntry, ScanSummary, Store};

use crate::mem;
use crate::synth::{self, SynthEntry};

/// Query mix used when none are given: one of each match tier plus a
/// one-character query (worst case: nearly everything matches) and a miss.
pub const DEFAULT_QUERIES: &[&str] = &[
    "readme.md",
    "proj",
    "report",
    "port",
    "budget-meeting",
    "annual report",
    "prjrpt",
    "a",
    "zzqxj",
];

/// Writes a synthetic tree of `count` entries directly into the store
/// under the virtual root `/synthetic`, skipping the filesystem entirely.
/// Measures storage and search at sizes where creating real files would
/// be slow and disruptive.
pub fn synth_store(store: &mut Store, count: usize, seed: u64) -> Result<Duration> {
    let started = Instant::now();
    let root = Path::new("/synthetic");
    let first_id = store.next_entry_id()?;
    let mut writer = store.replace_root(root, first_id)?;
    let mut result = Ok(());
    synth::generate(count, seed, |entry: SynthEntry<'_>| {
        if result.is_err() {
            return;
        }
        let id = first_id + 1 + entry.index as i64;
        result = writer.insert(&NewEntry {
            id,
            parent_id: entry.parent.map_or(first_id, |p| first_id + 1 + p as i64),
            kind: if entry.is_dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            },
            name: entry.name.as_bytes(),
            size: (!entry.is_dir).then_some(entry.index as u64 % 100_000),
            mtime: Some(1_700_000_000 + entry.index as i64),
        });
    });
    result?;
    writer.commit(ScanSummary {
        entry_count: count as u64 + 1,
        error_count: 0,
        scan_millis: started.elapsed().as_millis() as u64,
    })?;
    store.checkpoint();
    Ok(started.elapsed())
}

/// Creates a synthetic tree of empty files under `out`, which must not
/// exist or be empty, so existing data is never touched.
pub fn synth_files(out: &Path, count: usize, seed: u64) -> Result<Duration> {
    if out.exists() && fs::read_dir(out)?.next().is_some() {
        bail!("{} is not empty; refusing to write into it", out.display());
    }
    fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let started = Instant::now();
    // Paths of directories by entry index; only directories are kept.
    let mut dir_paths: std::collections::HashMap<usize, PathBuf> = Default::default();
    let mut result = Ok(());
    synth::generate(count, seed, |entry| {
        if result.is_err() {
            return;
        }
        let parent = entry.parent.map_or(out, |p| dir_paths[&p].as_path());
        let path = parent.join(entry.name);
        result = if entry.is_dir {
            fs::create_dir(&path).map(|()| {
                dir_paths.insert(entry.index, path);
            })
        } else {
            fs::File::create(&path).map(drop)
        }
        .with_context(|| format!("creating {}", Path::new(entry.name).display()));
    });
    result?;
    Ok(started.elapsed())
}

pub struct QueryStats {
    pub query: String,
    pub matched: usize,
    pub fuzzy_ran: bool,
    /// First execution in this process (cold CPU caches, pages possibly
    /// not yet resident).
    pub first: Duration,
    pub median: Duration,
    pub p95: Duration,
    pub max: Duration,
    /// Time to resolve the returned hits to paths via SQLite (median).
    pub resolve_median: Duration,
}

pub fn time_queries(
    index: &SearchIndex,
    queries: &[String],
    iterations: usize,
    limit: usize,
) -> Result<Vec<QueryStats>> {
    let iterations = iterations.max(1);
    let mut all = Vec::with_capacity(queries.len());
    for text in queries {
        let query = Query::new(text.as_str()).limit(limit);
        let first = index.search(&query);
        let mut samples = Vec::with_capacity(iterations);
        let mut resolve_samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let outcome = index.search(&query);
            samples.push(outcome.elapsed);
            let started = Instant::now();
            std::hint::black_box(index.resolve(&outcome.hits)?);
            resolve_samples.push(started.elapsed());
        }
        samples.sort_unstable();
        resolve_samples.sort_unstable();
        all.push(QueryStats {
            query: text.clone(),
            matched: first.matched,
            fuzzy_ran: first.fuzzy_ran,
            first: first.elapsed,
            median: percentile(&samples, 50),
            p95: percentile(&samples, 95),
            max: samples.last().copied().unwrap_or_default(),
            resolve_median: percentile(&resolve_samples, 50),
        });
    }
    Ok(all)
}

/// Nearest-rank percentile of sorted samples.
fn percentile(sorted: &[Duration], p: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

pub fn print_query_stats(stats: &[QueryStats], iterations: usize, limit: usize) {
    println!(
        "{:<18} {:>9} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "query", "matched", "fuzzy", "first", "median", "p95", "max", "resolve"
    );
    for s in stats {
        println!(
            "{:<18} {:>9} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
            format!("{:?}", s.query),
            s.matched,
            if s.fuzzy_ran { "yes" } else { "no" },
            ms(s.first),
            ms(s.median),
            ms(s.p95),
            ms(s.max),
            ms(s.resolve_median),
        );
    }
    println!(
        "\n{iterations} timed iterations per query after one untimed first run; limit {limit}. \
         Times are engine search time (resolve = SQLite path lookup for the returned hits)."
    );
    println!("process memory: {}", mem::describe());
}

pub fn ms(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1000.0)
}

/// Total size of the regular files directly inside `dir`.
pub fn dir_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| !e.file_name().as_bytes().starts_with(b"."))
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}
