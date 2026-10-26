//! Clipboard history results. Entries are searched only when the query
//! starts with `clip` (`clip`, `clip invoice`), never mixed into ordinary
//! results: copied text can be private.

use sift_core::{Action, Provider, ResultItem, ResultKind, top_score};
use sift_state::ClipEntry;

/// Characters of an entry shown as its title.
const TITLE_CHARS: usize = 100;

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

#[derive(Default)]
pub struct ClipboardProvider {
    /// Entries newest first, with a lowercased copy for matching.
    entries: Vec<(ClipEntry, String)>,
}

impl ClipboardProvider {
    pub fn new(entries: Vec<ClipEntry>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|e| {
                    let lower = e.text.to_lowercase();
                    (e, lower)
                })
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
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
        let words: Vec<String> = words.split_whitespace().map(str::to_lowercase).collect();
        let now = sift_state::now();
        self.entries
            .iter()
            .filter(|(_, lower)| words.iter().all(|w| lower.contains(w.as_str())))
            .take(limit)
            .enumerate()
            .map(|(rank, (entry, _))| ResultItem {
                id: format!("clip:{}", entry.id).into(),
                kind: ResultKind::Clipboard,
                title: title(&entry.text).into(),
                subtitle: Some(subtitle(entry, now).into()),
                // Newest first, above everything else.
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
    if lines > 1 {
        format!("{when} · {lines} lines · {chars} characters")
    } else {
        format!("{when} · {chars} characters")
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
    fn long_text_gets_a_short_title() {
        let long = "word ".repeat(100);
        assert_eq!(title(&long).chars().count(), TITLE_CHARS + 1);
        assert!(title(&long).ends_with('…'));
        assert_eq!(title("  a \n\t b  "), "a b");
    }
}
