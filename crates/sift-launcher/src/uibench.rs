//! A built-in UI benchmark, enabled with `SIFT_UI_BENCH=<rounds>`.
//!
//! It drives the real launcher the way a user would, without synthetic key
//! events (which would need the Accessibility permission): it shows the
//! panel through the same path as the hotkey, then types queries one
//! character at a time into the search field. For each keystroke it
//! records the time until the results are applied and until the next frame
//! is rendered. When done it logs a summary, hides the panel and keeps
//! running, so a script can measure memory (`scripts/ui-bench.sh`).

use std::time::{Duration, Instant};

use gpui::{AsyncApp, WindowHandle};

use crate::launcher::Launcher;

/// Queries typed in each round: application names, file words, a fuzzy
/// typo, multi-word and filtered queries.
const QUERIES: &[&str] = &[
    "safari",
    "terminal",
    "thesis",
    "mockup screenshot",
    "dsc",
    "rport",
    "monday ext:pdf",
];
/// Pause between keystrokes, roughly a fast typist.
const KEY_INTERVAL: Duration = Duration::from_millis(60);
/// Give up on a keystroke after this long.
const TIMEOUT: Duration = Duration::from_secs(2);

/// One keystroke: the query after it, and how long the launcher took.
#[derive(Debug, Clone)]
pub struct Sample {
    pub query: String,
    /// Keystroke until the results were applied to the view.
    pub results: Duration,
    /// Keystroke until the next frame started rendering.
    pub frame: Duration,
    pub count: usize,
}

struct Pending {
    query: String,
    typed: Instant,
    results: Option<(Duration, usize)>,
}

/// Records timings; owned by the launcher while a benchmark runs.
#[derive(Default)]
pub struct Recorder {
    pending: Option<Pending>,
    samples: Vec<Sample>,
    shows: Vec<Duration>,
}

impl Recorder {
    pub fn typed(&mut self, query: &str) {
        self.pending = Some(Pending {
            query: query.to_owned(),
            typed: Instant::now(),
            results: None,
        });
    }

    /// The launcher applied results for `query`.
    pub fn results(&mut self, query: &str, count: usize) {
        if let Some(pending) = &mut self.pending
            && pending.query == query
            && pending.results.is_none()
        {
            pending.results = Some((pending.typed.elapsed(), count));
        }
    }

    /// The launcher started rendering a frame.
    pub fn frame(&mut self) {
        let ready = matches!(&self.pending, Some(p) if p.results.is_some());
        if !ready {
            return;
        }
        if let Some(Pending {
            query,
            typed,
            results: Some((results, count)),
        }) = self.pending.take()
        {
            self.samples.push(Sample {
                query,
                results,
                frame: typed.elapsed(),
                count,
            });
        }
    }

    pub fn shown(&mut self, latency: Duration) {
        self.shows.push(latency);
    }

    fn sample_count(&self) -> usize {
        self.samples.len()
    }
}

/// Runs `rounds` rounds against the launcher in `window`, then logs a
/// summary.
pub async fn run(window: WindowHandle<Launcher>, rounds: usize, cx: &mut AsyncApp) {
    let executor = cx.background_executor().clone();
    let sleep = |d: Duration| executor.timer(d);

    // Wait until applications and the file index are loaded.
    let started = Instant::now();
    loop {
        let ready = window
            .update(cx, |launcher, _, _| launcher.bench_ready())
            .unwrap_or(false);
        if ready || started.elapsed() > Duration::from_secs(30) {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    let _ = window.update(cx, |launcher, _, _| launcher.start_bench());
    tracing::info!(rounds, "ui bench started");

    for _ in 0..rounds {
        let _ = window.update(cx, |launcher, window, cx| {
            launcher.toggle(Instant::now(), window, cx);
        });
        sleep(Duration::from_millis(300)).await;
        for query in QUERIES {
            for (end, _) in query.char_indices().skip(1).chain([(query.len(), ' ')]) {
                let prefix = &query[..end];
                let before = window
                    .update(cx, |launcher, _, cx| {
                        let before = launcher.bench().map_or(0, Recorder::sample_count);
                        launcher.bench_type(prefix, cx);
                        before
                    })
                    .unwrap_or(0);
                let typed = Instant::now();
                loop {
                    let done = window
                        .update(cx, |launcher, _, _| {
                            launcher.bench().map_or(0, Recorder::sample_count) > before
                        })
                        .unwrap_or(true);
                    if done || typed.elapsed() > TIMEOUT {
                        break;
                    }
                    sleep(Duration::from_millis(1)).await;
                }
                sleep(KEY_INTERVAL).await;
            }
            let _ = window.update(cx, |launcher, _, cx| launcher.bench_type("", cx));
            sleep(Duration::from_millis(100)).await;
        }
        let _ = window.update(cx, |launcher, _, cx| launcher.hide(cx));
        sleep(Duration::from_millis(300)).await;
    }

    let _ = window.update(cx, |launcher, _, _| {
        if let Some(recorder) = launcher.bench() {
            report(recorder);
        }
    });
}

fn report(recorder: &Recorder) {
    let typed: Vec<&Sample> = recorder
        .samples
        .iter()
        .filter(|s| !s.query.is_empty())
        .collect();
    let line = |label: &str, mut values: Vec<Duration>| {
        if values.is_empty() {
            return;
        }
        values.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
        let summary = format!(
            "{label:<38} n={:<4} median {:>6.2} ms  p95 {:>6.2} ms  max {:>6.2} ms",
            values.len(),
            ms(at(0.5)),
            ms(at(0.95)),
            ms(values[values.len() - 1]),
        );
        println!("{summary}");
    };
    println!("== ui bench");
    line("show to first frame", recorder.shows.clone());
    line(
        "keystroke to results, all",
        typed.iter().map(|s| s.results).collect(),
    );
    line(
        "keystroke to frame, all",
        typed.iter().map(|s| s.frame).collect(),
    );
    line(
        "results to frame (waiting to draw)",
        typed
            .iter()
            .map(|s| s.frame.saturating_sub(s.results))
            .collect(),
    );
    line(
        "keystroke to frame, one character",
        typed
            .iter()
            .filter(|s| s.query.chars().count() == 1)
            .map(|s| s.frame)
            .collect(),
    );
    line(
        "keystroke to frame, files searched",
        typed
            .iter()
            .filter(|s| s.query.chars().count() >= 2)
            .map(|s| s.frame)
            .collect(),
    );
    for query in QUERIES {
        line(
            &format!("  “{query}” (full query)"),
            typed
                .iter()
                .filter(|s| s.query == *query)
                .map(|s| s.frame)
                .collect(),
        );
    }
    let empty = typed.iter().filter(|s| s.count == 0).count();
    println!("keystrokes with no results: {empty} of {}", typed.len());
    println!("ui bench done");
}
