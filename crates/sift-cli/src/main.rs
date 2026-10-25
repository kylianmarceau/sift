//! `sift` — development CLI for the sift file index.

mod bench;
mod mem;
mod synth;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use sift_indexer::{IndexOptions, index_root_at};
use sift_search::{
    DB_FILE, KindFilter, Query, SNAPSHOT_FILE, SearchIndex, Snapshot, SnapshotError,
};
use sift_store::{EntryKind, EventMark, Store};

use crate::bench::ms;
use crate::mem::bytes;

#[derive(Parser)]
#[command(name = "sift", version, about = "Fast local file index and search")]
struct Cli {
    /// Directory holding the index database and name snapshot.
    /// [default: ~/Library/Application Support/sift/dev-index]
    #[arg(long, global = true, env = "SIFT_INDEX_DIR", value_name = "DIR")]
    index_dir: Option<PathBuf>,

    /// Log more detail to stderr (-v info, -vv debug).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Control Sift's native window manager. Try `sift wm status` or
    /// `sift wm list-windows`. Sift.app must be running with it enabled.
    Wm {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Scan directories and (re)build their index.
    Index(IndexArgs),
    /// Search indexed file and folder names.
    Search(SearchArgs),
    /// Show indexed roots and index size.
    Status,
    /// Check the index: database integrity, internal consistency and,
    /// unless --quick, whether it matches the files on disk.
    Verify {
        /// Skip rescanning the roots to compare with the disk.
        #[arg(long)]
        quick: bool,
    },
    /// Remove a root and its entries from the index.
    Remove {
        /// Root directory to remove, as shown by `sift status`.
        path: PathBuf,
    },
    /// Benchmark helpers.
    #[command(subcommand)]
    Bench(BenchCommand),
}

#[derive(Args)]
struct IndexArgs {
    /// Directories to index. With none, every saved root is re-indexed
    /// with its saved options.
    paths: Vec<PathBuf>,
    /// Skip this path (repeatable).
    #[arg(long, value_name = "PATH")]
    exclude: Vec<PathBuf>,
    /// Skip files and folders with this exact name anywhere (repeatable),
    /// e.g. `--exclude-name node_modules`.
    #[arg(long, value_name = "NAME")]
    exclude_name: Vec<String>,
    /// Also index ~/Library and ~/.Trash when they fall inside a root.
    #[arg(long)]
    include_sensitive: bool,
    /// Also descend into dependency, tool and cache folders such as
    /// node_modules, .git, .venv and CACHEDIR.TAG-tagged directories.
    #[arg(long)]
    include_dependencies: bool,
    /// Descend into package bundles such as Foo.app.
    #[arg(long)]
    descend_packages: bool,
    /// Descend into other volumes mounted inside a root.
    #[arg(long)]
    cross_devices: bool,
    /// Directory-reading threads [default: CPU count, at most 8].
    #[arg(long)]
    threads: Option<usize>,
    /// Allow indexing `/` or your entire home directory.
    #[arg(long)]
    allow_broad_root: bool,
}

#[derive(Args)]
struct SearchArgs {
    /// Search text. Multiple words are joined with spaces.
    #[arg(required = true)]
    query: Vec<String>,
    /// Maximum number of results.
    #[arg(short = 'n', long, default_value_t = 20)]
    limit: usize,
    /// Only files (anything that is not a folder).
    #[arg(long, conflicts_with = "dirs")]
    files: bool,
    /// Only folders.
    #[arg(long)]
    dirs: bool,
    /// Only names with this extension, e.g. `--ext pdf`. Repeat or
    /// separate with commas for several: `--ext jpg,png`.
    #[arg(long, value_name = "EXT", value_delimiter = ',')]
    ext: Vec<String>,
    /// Disable approximate (fuzzy) matches.
    #[arg(long)]
    no_fuzzy: bool,
    /// Print only paths, one per line.
    #[arg(long)]
    paths_only: bool,
    /// Print timing and memory details.
    #[arg(long)]
    stats: bool,
}

#[derive(Subcommand)]
enum BenchCommand {
    /// Write a synthetic tree directly into the index (no files created).
    /// Use a dedicated --index-dir.
    Synth {
        #[arg(long)]
        count: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
    },
    /// Create a synthetic tree of empty files on disk for indexing
    /// benchmarks. OUT must not exist or be empty.
    Files {
        out: PathBuf,
        #[arg(long)]
        count: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
    },
    /// Time a set of queries against the index.
    Search {
        /// Queries to time [default: a built-in mix].
        queries: Vec<String>,
        #[arg(long, default_value_t = 50)]
        iterations: usize,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
}

fn main() {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        _ => tracing::Level::DEBUG,
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    if let Err(error) = run(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let index_dir = match cli.index_dir {
        Some(dir) => dir,
        None => default_index_dir()?,
    };
    match cli.command {
        Command::Wm { args } => {
            let support = std::env::var_os("SIFT_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(sift_state::support_dir)
                .context("cannot determine Sift support directory")?;
            let reply = sift_wm::protocol::request(&support, args)?;
            if !reply.ok {
                bail!(
                    "{}",
                    reply
                        .error
                        .unwrap_or_else(|| "window manager command failed".into())
                );
            }
            println!("{}", serde_json::to_string_pretty(&reply.data)?);
            Ok(())
        }
        Command::Index(args) => cmd_index(&index_dir, args),
        Command::Search(args) => cmd_search(&index_dir, args),
        Command::Status => cmd_status(&index_dir),
        Command::Verify { quick } => cmd_verify(&index_dir, quick),
        Command::Remove { path } => cmd_remove(&index_dir, &path),
        Command::Bench(command) => cmd_bench(&index_dir, command),
    }
}

fn default_index_dir() -> Result<PathBuf> {
    sift_search::default_index_dir().context("cannot determine home directory")
}

fn open_store(index_dir: &Path) -> Result<Store> {
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("creating index directory {}", index_dir.display()))?;
    Ok(Store::open(&index_dir.join(DB_FILE))?)
}

fn cmd_index(index_dir: &Path, args: IndexArgs) -> Result<()> {
    let mut store = open_store(index_dir)?;
    let saved: Vec<(PathBuf, Vec<u8>)> = store
        .roots()?
        .into_iter()
        .map(|r| (r.path, r.options))
        .collect();

    let paths: Vec<PathBuf> = if args.paths.is_empty() {
        if saved.is_empty() {
            bail!("nothing indexed yet; run `sift index <dir>`");
        }
        saved.iter().map(|(p, _)| p.clone()).collect()
    } else {
        args.paths.clone()
    };

    for path in &paths {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("cannot access {}", path.display()))?;
        if sift_indexer::is_broad_root(&canonical) && !args.allow_broad_root {
            bail!(
                "refusing to index {} without --allow-broad-root; pick specific folders instead",
                canonical.display()
            );
        }
    }

    // Explicit flags define new options (which are then saved); without
    // any, each root keeps the options it was last indexed with.
    let flags_given = !args.exclude.is_empty()
        || !args.exclude_name.is_empty()
        || args.include_sensitive
        || args.include_dependencies
        || args.descend_packages
        || args.cross_devices;
    let from_flags = IndexOptions {
        one_file_system: !args.cross_devices,
        descend_packages: args.descend_packages,
        skip_dependency_dirs: !args.include_dependencies,
        skip_sensitive: !args.include_sensitive,
        exclude_names: args.exclude_name.iter().map(Into::into).collect(),
        exclude_paths: args
            .exclude
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect(),
        ..IndexOptions::default()
    };
    let options_for = |root: &Path| -> IndexOptions {
        let mut options = if flags_given {
            from_flags.clone()
        } else {
            let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
            saved
                .iter()
                .find(|(p, _)| *p == canonical)
                .and_then(|(_, bytes)| IndexOptions::decode_saved(bytes))
                .unwrap_or_default()
        };
        if let Some(threads) = args.threads {
            options.threads = threads.max(1);
        }
        // Never index our own database churn (only relevant, and only
        // saved, when the index lives inside this root).
        if let Ok(dir) = index_dir.canonicalize()
            && dir.starts_with(root.canonicalize().unwrap_or_else(|_| root.to_path_buf()))
            && !options.exclude_paths.contains(&dir)
        {
            options.exclude_paths.push(dir);
        }
        options
    };

    let interactive = std::io::stderr().is_terminal();
    for path in &paths {
        let options = options_for(path);
        eprintln!("Indexing {} …", path.display());
        // Record where FSEvents stood before the scan, so the launcher can
        // pick up every later change without rescanning.
        let mark = sift_watch::event_store_uuid(path).map(|uuid| EventMark {
            id: sift_watch::current_event_id(),
            uuid,
        });
        let report = index_root_at(&mut store, path, &options, mark.as_ref(), |n| {
            if interactive {
                eprint!("\r  {} entries", thousands(n));
                let _ = std::io::stderr().flush();
            }
        })?;
        if interactive {
            eprint!("\r\x1b[2K");
        }
        println!("Indexed {}", report.root.display());
        println!(
            "  entries   {} ({} files, {} folders, {} links)",
            thousands(report.entries),
            thousands(report.files),
            thousands(report.dirs),
            thousands(report.symlinks)
        );
        println!(
            "  time      {} scan, {} total",
            ms(report.scan_time),
            ms(report.total_time)
        );
        if report.errors > 0 {
            println!(
                "  skipped   {} unreadable items, e.g.:",
                thousands(report.errors)
            );
            for error in report.error_samples.iter().take(5) {
                println!("            {error}");
            }
        }
    }

    let started = Instant::now();
    let snapshot = sift_search::rebuild_snapshot(&store, index_dir)?;
    println!(
        "Snapshot   {} names, {}, built in {}",
        thousands(snapshot.entries as u64),
        bytes(snapshot.bytes),
        ms(started.elapsed())
    );
    let db = store.disk_size();
    println!(
        "Index size {} database + {} snapshot = {}  ({})",
        bytes(db),
        bytes(snapshot.bytes),
        bytes(db + snapshot.bytes),
        index_dir.display()
    );
    println!("Memory     {}", mem::describe());
    Ok(())
}

fn cmd_search(index_dir: &Path, args: SearchArgs) -> Result<()> {
    let opened = Instant::now();
    let index = SearchIndex::open(index_dir)?;
    let open_time = opened.elapsed();
    if index.snapshot_rebuilt() {
        eprintln!("(rebuilt name snapshot in {})", ms(open_time));
    }

    let query = Query {
        text: args.query.join(" "),
        limit: args.limit,
        kind: if args.files {
            Some(KindFilter::Files)
        } else if args.dirs {
            Some(KindFilter::Dirs)
        } else {
            None
        },
        extensions: args.ext,
        fuzzy: !args.no_fuzzy,
    };
    let outcome = index.search(&query);
    let resolved_at = Instant::now();
    let results = index.resolve(&outcome.hits)?;
    let resolve_time = resolved_at.elapsed();

    let mut out = std::io::stdout().lock();
    for result in &results {
        if args.paths_only {
            writeln!(out, "{}", result.path.display())?;
            continue;
        }
        let slash = if result.kind == EntryKind::Dir {
            "/"
        } else {
            ""
        };
        writeln!(
            out,
            "{:>6}  {:<5} {}{slash}",
            result.tier.label(),
            result.kind.label(),
            result.path.display()
        )?;
    }
    drop(out);

    if !args.paths_only {
        // When enough substring matches exist, fuzzy matches are never
        // computed, so the true total may be higher.
        let plus = if query.fuzzy && !outcome.fuzzy_ran {
            "+"
        } else {
            ""
        };
        let matched = format!("{}{plus}", thousands(outcome.matched as u64));
        eprintln!(
            "{} of {matched} matches in {}",
            results.len(),
            ms(outcome.elapsed)
        );
    }
    if args.stats {
        eprintln!(
            "open {} · search {} · resolve {} · {} names ({} snapshot)",
            ms(open_time),
            ms(outcome.elapsed),
            ms(resolve_time),
            thousands(index.snapshot().len() as u64),
            bytes(index.snapshot().byte_len() as u64)
        );
        eprintln!("memory: {}", mem::describe());
    }
    Ok(())
}

fn cmd_verify(index_dir: &Path, quick: bool) -> Result<()> {
    let db = index_dir.join(DB_FILE);
    if !db.exists() {
        anyhow::bail!("no index at {}", index_dir.display());
    }
    let store = Store::open_read_only(&db)?;
    let mut sound = true;

    let started = Instant::now();
    let integrity = store.quick_check()?;
    println!(
        "Database   {} ({} ms)",
        integrity.as_deref().unwrap_or("intact"),
        started.elapsed().as_millis()
    );
    sound &= integrity.is_none();

    let started = Instant::now();
    let problems = store.verify()?;
    if problems.is_empty() {
        println!(
            "Structure  consistent ({} ms)",
            started.elapsed().as_millis()
        );
    } else {
        sound = false;
        for problem in &problems {
            println!("Structure  {problem}");
        }
    }

    if !quick {
        let scratch_dir = std::env::temp_dir();
        for root in store.roots()? {
            let options = IndexOptions::decode_saved(&root.options).unwrap_or_default();
            let scratch =
                scratch_dir.join(format!("sift-verify-{}-{}.db", std::process::id(), root.id));
            let started = Instant::now();
            match sift_indexer::compare_with_disk(&store, &root, &options, &scratch) {
                Ok(diff) => {
                    let ms = started.elapsed().as_millis();
                    if diff.is_empty() {
                        println!(
                            "Root       {}: matches the disk ({} entries, {ms} ms)",
                            root.path.display(),
                            thousands(diff.on_disk as u64)
                        );
                    } else {
                        sound = false;
                        println!(
                            "Root       {}: {} missing, {} no longer on disk, {} changed ({ms} ms)",
                            root.path.display(),
                            diff.missing.len(),
                            diff.extra.len(),
                            diff.changed.len()
                        );
                        let show = |label: &str, paths: &[Vec<u8>]| {
                            for path in paths.iter().take(5) {
                                println!("             {label} {}", String::from_utf8_lossy(path));
                            }
                        };
                        show("missing", &diff.missing);
                        show("extra  ", &diff.extra);
                        show("changed", &diff.changed);
                    }
                }
                Err(error) => {
                    sound = false;
                    println!(
                        "Root       {}: cannot compare: {error}",
                        root.path.display()
                    );
                }
            }
        }
        println!(
            "Note       files that change while this runs are reported as differences; run it again \
             if they look like recent edits."
        );
    }
    if !sound {
        anyhow::bail!("the index has problems; `sift index` with no arguments rebuilds it");
    }
    Ok(())
}

fn cmd_status(index_dir: &Path) -> Result<()> {
    let db = index_dir.join(DB_FILE);
    if !db.exists() {
        println!("No index at {}", index_dir.display());
        return Ok(());
    }
    let store = Store::open_read_only(&db)?;
    let generation = store.generation()?;
    println!("Index      {}", index_dir.display());
    println!(
        "Entries    {} in {}",
        thousands(store.entry_count()?),
        bytes(store.disk_size())
    );
    let snapshot = match Snapshot::open(&index_dir.join(SNAPSHOT_FILE), generation) {
        Ok(s) => format!("current, {}", bytes(s.byte_len() as u64)),
        Err(SnapshotError::Stale { .. }) => "stale (rebuilt on next search)".into(),
        Err(SnapshotError::Missing(_)) => "missing (built on next search)".into(),
        Err(e) => format!("unusable: {e}"),
    };
    println!("Snapshot   {snapshot}");
    let roots = store.roots()?;
    if roots.is_empty() {
        println!("Roots      none");
    }
    for root in roots {
        println!(
            "Root       {}\n           {} entries, {} skipped, scanned in {} ms, {}",
            root.path.display(),
            thousands(root.entry_count),
            thousands(root.error_count),
            thousands(root.scan_millis),
            age(root.indexed_at)
        );
        if let Some(options) = IndexOptions::decode_saved(&root.options) {
            println!("           {}", describe_options(&options));
        }
    }
    Ok(())
}

/// One-line summary of saved indexing options for `sift status`.
fn describe_options(options: &IndexOptions) -> String {
    let mut parts = Vec::new();
    parts.push(if options.skip_dependency_dirs {
        "dependency folders skipped".to_string()
    } else {
        "dependency folders included".to_string()
    });
    if !options.skip_sensitive {
        parts.push("sensitive folders included".into());
    }
    if options.descend_packages {
        parts.push("inside packages".into());
    }
    if !options.one_file_system {
        parts.push("crosses volumes".into());
    }
    if !options.exclude_names.is_empty() {
        let names: Vec<_> = options
            .exclude_names
            .iter()
            .map(|n| n.to_string_lossy())
            .collect();
        parts.push(format!("excluding names {}", names.join(", ")));
    }
    let user_paths = options.exclude_paths.len();
    if user_paths > 0 {
        parts.push(format!("{user_paths} excluded paths"));
    }
    parts.join(" · ")
}

fn cmd_remove(index_dir: &Path, path: &Path) -> Result<()> {
    let mut store = open_store(index_dir)?;
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !store.remove_root(&target)? {
        bail!(
            "{} is not an indexed root (see `sift status`)",
            target.display()
        );
    }
    sift_search::rebuild_snapshot(&store, index_dir)?;
    println!("Removed {}", target.display());
    Ok(())
}

fn cmd_bench(index_dir: &Path, command: BenchCommand) -> Result<()> {
    match command {
        BenchCommand::Synth { count, seed } => {
            let mut store = open_store(index_dir)?;
            let write_time = bench::synth_store(&mut store, count, seed)?;
            let started = Instant::now();
            let snapshot = sift_search::rebuild_snapshot(&store, index_dir)?;
            let snapshot_time = started.elapsed();
            println!(
                "Wrote {} synthetic entries (seed {seed}) in {}",
                thousands(count as u64 + 1),
                ms(write_time)
            );
            println!(
                "Snapshot {} names, {}, built in {}",
                thousands(snapshot.entries as u64),
                bytes(snapshot.bytes),
                ms(snapshot_time)
            );
            println!(
                "Index size {} database + {} snapshot",
                bytes(store.disk_size()),
                bytes(snapshot.bytes)
            );
            println!("Memory {}", mem::describe());
        }
        BenchCommand::Files { out, count, seed } => {
            let elapsed = bench::synth_files(&out, count, seed)?;
            println!(
                "Created {} synthetic entries under {} in {}",
                thousands(count as u64),
                out.display(),
                ms(elapsed)
            );
        }
        BenchCommand::Search {
            queries,
            iterations,
            limit,
        } => {
            let started = Instant::now();
            let index = SearchIndex::open(index_dir)?;
            let open_time = started.elapsed();
            let queries: Vec<String> = if queries.is_empty() {
                bench::DEFAULT_QUERIES
                    .iter()
                    .map(|q| q.to_string())
                    .collect()
            } else {
                queries
            };
            println!(
                "Index: {} names, snapshot {}, database {}; opened in {}{}",
                thousands(index.snapshot().len() as u64),
                bytes(index.snapshot().byte_len() as u64),
                bytes(bench::dir_size(index_dir) - index.snapshot().byte_len() as u64),
                ms(open_time),
                if index.snapshot_rebuilt() {
                    " (snapshot rebuilt)"
                } else {
                    ""
                }
            );
            let stats = bench::time_queries(&index, &queries, iterations, limit)?;
            bench::print_query_stats(&stats, iterations, limit);
        }
    }
    Ok(())
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn age(unix_secs: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - unix_secs).max(0);
    match secs {
        0..60 => format!("{secs} s ago"),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
