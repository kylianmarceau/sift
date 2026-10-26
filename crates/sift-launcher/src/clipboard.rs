//! Clipboard history results. Entries are searched only when the query
//! starts with `clip` (`clip`, `clip invoice`), never mixed into ordinary
//! results: copied text can be private.
//!
//! Entries that contain every word come first, in history order. Then
//! come entries whose beginning (the title shown) matches loosely, as
//! file names do ("invpd" finds "invoice paid"), best match first. Loose
//! matching looks only at the beginning, since anywhere in a long text
//! almost any letters can be found in order, and only at the newest
//! [`LOOSE_LIMIT`] entries, to bound its cost.

use std::collections::HashMap;

use sift_core::{Action, Provider, ResultItem, ResultKind, top_score};
use sift_search::{NameMatcher, fold};
use sift_state::ClipEntry;

/// Characters of an entry shown as its title.
const TITLE_CHARS: usize = 100;
const ID_PREFIX: &str = "clip:";
/// Entries that are matched loosely, newest first. Loose matching costs
/// about 4 µs per long entry; this keeps a search of the largest history
/// near 2 ms.
const LOOSE_LIMIT: usize = 2_000;
/// `clip from:safari invoice`: only entries copied in an app whose name
/// contains "safari".
const FROM: &str = "from:";

fn copied_in(entry: &ClipEntry, apps: &[&str]) -> bool {
    entry.source.as_ref().is_some_and(|source| {
        let name = source.name.to_lowercase();
        apps.iter().any(|app| name.contains(app))
    })
}

/// The history entry a result stands for.
pub fn entry_id(item: &ResultItem) -> Option<i64> {
    if item.kind != ResultKind::Clipboard {
        return None;
    }
    item.id.strip_prefix(ID_PREFIX)?.parse().ok()
}

/// The words to look for if `query` is a clipboard search: `clip` alone
/// gives `Some("")`, `clip foo bar` gives `Some("foo bar")`, and anything
/// else (`clipboard`, `my clip`) gives `None`.
pub fn scope(query: &str) -> Option<&str> {
    let query = query.trim_start();
    let head = query.get(..4)?;
    if !head.eq_ignore_ascii_case("clip") {
        return None;
    }
    let rest = &query[4..];
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

/// An entry with what searching it needs.
struct Indexed {
    entry: ClipEntry,
    /// The whole text, lowercased, for word matches.
    lower: String,
    /// The title, folded as names are, for loose matches.
    folded_title: Vec<u8>,
}

#[derive(Default)]
pub struct ClipboardProvider {
    /// Pinned entries, then the others newest first.
    entries: Vec<Indexed>,
    /// Position in `entries` by entry id.
    by_id: HashMap<i64, usize>,
}

impl ClipboardProvider {
    pub fn new(entries: Vec<ClipEntry>) -> Self {
        let by_id = entries.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
        Self {
            entries: entries
                .into_iter()
                .map(|entry| Indexed {
                    lower: entry.text.to_lowercase(),
                    folded_title: fold(title(&entry.text).as_bytes()),
                    entry,
                })
                .collect(),
            by_id,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The entry with this id, if it is in the history.
    pub fn entry(&self, id: i64) -> Option<&ClipEntry> {
        self.by_id.get(&id).map(|&i| &self.entries[i].entry)
    }

    /// Whether `item` is a pinned entry.
    pub fn is_pinned(&self, item: &ResultItem) -> bool {
        entry_id(item)
            .and_then(|id| self.entry(id))
            .is_some_and(|e| e.pinned)
    }
}

impl Provider for ClipboardProvider {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
        let Some(words) = scope(query) else {
            return Vec::new();
        };
        let (apps, words): (Vec<String>, Vec<String>) = words
            .split_whitespace()
            .map(str::to_lowercase)
            .partition(|w| w.starts_with(FROM));
        let apps: Vec<&str> = apps
            .iter()
            .map(|a| &a[FROM.len()..])
            .filter(|a| !a.is_empty())
            .collect();
        let entries = self
            .entries
            .iter()
            .filter(|e| apps.is_empty() || copied_in(&e.entry, &apps));
        let mut found: Vec<&ClipEntry> = Vec::new();
        let mut rest: Vec<&Indexed> = Vec::new();
        for e in entries {
            if words.iter().all(|w| e.lower.contains(w.as_str())) {
                found.push(&e.entry);
                if found.len() == limit {
                    // Enough word matches: loose ones would come after.
                    break;
                }
            } else if rest.len() < LOOSE_LIMIT {
                rest.push(e);
            }
        }
        if found.len() < limit
            && let Some(matcher) = NameMatcher::new(&words.join(" "))
        {
            let mut loose: Vec<(u64, &ClipEntry)> = rest
                .into_iter()
                .filter_map(|e| Some((matcher.score(&e.folded_title)?.1, &e.entry)))
                .collect();
            // Stable: equal matches stay in history order.
            loose.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
            found.extend(loose.into_iter().map(|(_, entry)| entry));
        }
        let now = sift_state::now();
        found
            .into_iter()
            .take(limit)
            .enumerate()
            .map(|(rank, entry)| ResultItem {
                id: format!("{ID_PREFIX}{}", entry.id).into(),
                kind: ResultKind::Clipboard,
                title: title(&entry.text).into(),
                subtitle: Some(subtitle(entry, now).into()),
                // In this order, above everything else.
                score: top_score().saturating_sub(rank as u64),
                action: Action::CopyText(entry.text.clone()),
            })
            .collect()
    }
}

/// The text on one line: whitespace runs collapsed, then truncated.
fn title(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() > TITLE_CHARS {
            break;
        }
    }
    if out.chars().count() > TITLE_CHARS {
        out = out.chars().take(TITLE_CHARS).collect::<String>() + "…";
    }
    out
}

fn subtitle(entry: &ClipEntry, now: i64) -> String {
    let age = (now - entry.copied_at).max(0);
    let when = match age {
        0..60 => "Copied just now".to_owned(),
        60..3600 => format!("Copied {} min ago", age / 60),
        3600..86_400 => format!("Copied {} h ago", age / 3600),
        _ => format!("Copied {} days ago", age / 86_400),
    };
    let lines = entry.text.lines().count();
    let chars = entry.text.chars().count();
    let size = if lines > 1 {
        format!("{lines} lines · {chars} characters")
    } else {
        format!("{chars} characters")
    };
    match &entry.source {
        Some(source) => format!("{when} in {} · {size}", source.name),
        None => format!("{when} · {size}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, text: &str, copied_at: i64) -> ClipEntry {
        ClipEntry {
            id,
            text: text.into(),
            copied_at,
            pinned: false,
            source: None,
        }
    }

    #[test]
    fn only_clip_queries_are_scoped() {
        assert_eq!(scope("clip"), Some(""));
        assert_eq!(scope("  CLIP  invoice 42 "), Some("invoice 42"));
        assert_eq!(scope("clipboard"), None);
        assert_eq!(scope("my clip"), None);
        assert_eq!(scope("cl"), None);
        assert_eq!(scope("clé"), None);
    }

    #[test]
    fn matches_all_words_newest_first() {
        let now = sift_state::now();
        let provider = ClipboardProvider::new(vec![
            entry(3, "Invoice 42 paid", now - 30),
            entry(2, "invoice 41\nsecond line", now - 7200),
            entry(1, "unrelated", now - 10),
        ]);
        let results = provider.search("clip invoice", 10);
        let titles: Vec<&str> = results.iter().map(|r| &*r.title).collect();
        assert_eq!(titles, ["Invoice 42 paid", "invoice 41 second line"]);
        assert!(results[0].score > results[1].score);
        assert_eq!(
            results[1].subtitle.as_deref(),
            Some("Copied 2 h ago · 2 lines · 22 characters")
        );
        assert_eq!(
            results[0].action,
            Action::CopyText("Invoice 42 paid".into())
        );
        assert_eq!(provider.search("clip", 10).len(), 3);
        assert!(provider.search("invoice", 10).is_empty());
    }

    #[test]
    fn from_narrows_to_the_app_it_was_copied_in() {
        let now = sift_state::now();
        let mut slack = entry(2, "standup notes", now - 120);
        slack.source = Some(sift_state::ClipSource {
            name: "Slack".into(),
            bundle_id: None,
        });
        let provider = ClipboardProvider::new(vec![slack, entry(1, "notes", now)]);
        let titles = |q: &str| -> Vec<String> {
            provider
                .search(q, 10)
                .into_iter()
                .map(|r| r.title.to_string())
                .collect()
        };
        assert_eq!(titles("clip from:sla"), ["standup notes"]);
        assert_eq!(titles("clip FROM:slack notes"), ["standup notes"]);
        assert!(titles("clip from:safari").is_empty());
        // A bare "from:" filters nothing.
        assert_eq!(titles("clip from:").len(), 2);
        assert_eq!(
            provider.search("clip standup", 1)[0].subtitle.as_deref(),
            Some("Copied 2 min ago in Slack · 13 characters")
        );
    }

    #[test]
    fn loose_matches_of_the_beginning_follow_word_matches() {
        let now = sift_state::now();
        let provider = ClipboardProvider::new(vec![
            entry(4, "invoice paid in full", now - 10),
            entry(3, "the invpd ticket", now - 20),
            entry(2, &format!("{} invoice paid", "x".repeat(200)), now - 30),
            entry(1, "unrelated", now - 40),
        ]);
        let titles: Vec<String> = provider
            .search("clip invpd", 10)
            .into_iter()
            .map(|r| r.title.to_string())
            .collect();
        // The word match first, then the loose match of a beginning; the
        // long entry's beginning does not match.
        assert_eq!(titles, ["the invpd ticket", "invoice paid in full"]);
        let results = provider.search("clip invpd", 10);
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn results_name_their_entry() {
        let mut pinned = entry(7, "keep me", sift_state::now());
        pinned.pinned = true;
        let provider = ClipboardProvider::new(vec![pinned, entry(3, "other", 0)]);
        let results = provider.search("clip", 10);
        assert_eq!(entry_id(&results[0]), Some(7));
        assert!(provider.is_pinned(&results[0]));
        assert!(!provider.is_pinned(&results[1]));
        let snippet = ResultItem {
            kind: ResultKind::Snippet,
            ..results[0].clone()
        };
        assert_eq!(entry_id(&snippet), None);
    }

    /// Search latency on a full history of mixed short and long entries.
    /// Not run by default:
    ///
    /// cargo test --release -p sift-launcher clipboard_search_latency -- --ignored --nocapture
    #[test]
    #[ignore]
    fn clipboard_search_latency() {
        const ROUNDS: usize = 200;
        let now = sift_state::now();
        let paragraph = "The quarterly report lists every open invoice by customer, \
                         with totals per region and the payment terms agreed. ";
        for size in [200, 10_000] {
            let entries: Vec<ClipEntry> = (0..size)
                .map(|i| {
                    let text = match i % 4 {
                        0 => format!("https://example.com/orders/{i}?ref=mail"),
                        1 => format!("invoice {i} paid"),
                        2 => paragraph.repeat(1 + i % 40),
                        _ => format!("fn handler_{i}() {{\n    let total = {i};\n}}"),
                    };
                    entry(i as i64, &text, now - i as i64)
                })
                .collect();
            let provider = ClipboardProvider::new(entries);
            for query in [
                "clip",
                "clip invoice",
                "clip inv 42",
                "clip invocie",
                "clip invpd",
                "clip zzqx",
            ] {
                let mut times: Vec<f64> = (0..ROUNDS)
                    .map(|_| {
                        let started = std::time::Instant::now();
                        std::hint::black_box(provider.search(query, 40));
                        started.elapsed().as_secs_f64() * 1e6
                    })
                    .collect();
                times.sort_by(f64::total_cmp);
                println!(
                    "{size:>6} entries  {query:<14} median {:>8.1} µs   p95 {:>8.1} µs",
                    times[ROUNDS / 2],
                    times[ROUNDS * 95 / 100]
                );
            }
        }
    }

    #[test]
    fn long_text_gets_a_short_title() {
        let long = "word ".repeat(100);
        assert_eq!(title(&long).chars().count(), TITLE_CHARS + 1);
        assert!(title(&long).ends_with('…'));
        assert_eq!(title("  a \n\t b  "), "a b");
    }
}
