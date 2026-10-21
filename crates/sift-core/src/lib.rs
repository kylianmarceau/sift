//! Shared vocabulary of the sift launcher: what a search result is, what a
//! provider does, and the small pieces of query bookkeeping every front end
//! needs (stale-result suppression and selection).
//!
//! The crate has no dependencies and knows nothing about the UI, AppKit or
//! the file index, so providers and their ranking can be tested headlessly.
//! Phase 1 has one provider (installed applications); the file index joins
//! as a second provider in Phase 2.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a result refers to. Drives the label and icon in the UI, and later
/// per-kind filters such as `a:` (applications) or `f:` (files).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResultKind {
    Application,
    File,
    Folder,
    Command,
}

impl ResultKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Application => "Application",
            Self::File => "File",
            Self::Folder => "Folder",
            Self::Command => "Command",
        }
    }
}

/// What happens when the user activates a result.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    /// Launch the application bundle, or bring it to the front if running.
    OpenApplication(PathBuf),
    /// Open a file or folder with its default application.
    OpenPath(PathBuf),
}

/// A single search result, independent of which provider produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultItem {
    /// Stable identifier, unique across providers (for example the bundle
    /// path). Used to keep the selection on the same item while results
    /// update.
    pub id: Arc<str>,
    pub kind: ResultKind,
    pub title: Arc<str>,
    /// Secondary line, such as the containing folder.
    pub subtitle: Option<Arc<str>>,
    /// Ranking key; higher is better. Providers use the same key scheme
    /// (`sift_search` rank keys), so results from different providers can
    /// be merged by sorting.
    pub score: u64,
    pub action: Action,
}

/// A source of results. Implementations must be cheap to query repeatedly:
/// they hold their data in memory (or memory-mapped) and never rescan the
/// filesystem per query.
///
/// `search` runs off the UI thread, so providers are `Send + Sync`.
pub trait Provider: Send + Sync {
    /// Short stable name, used in result ids and diagnostics.
    fn name(&self) -> &'static str;

    /// Returns at most `limit` results for `query`, best first. A blank
    /// query may return nothing or a provider-defined default list.
    fn search(&self, query: &str, limit: usize) -> Vec<ResultItem>;
}

/// Queries every provider and merges their results into one ranked list.
#[derive(Clone, Default)]
pub struct Search {
    providers: Vec<Arc<dyn Provider>>,
}

impl Search {
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        Self { providers }
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
        let mut results: Vec<ResultItem> = self
            .providers
            .iter()
            .flat_map(|p| p.search(query, limit))
            .collect();
        // Stable tie-break on title so equal scores do not reorder between
        // keystrokes.
        results.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        results.truncate(limit);
        results
    }
}

/// Identifies one query among many issued while the user types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket(u64);

/// Suppresses stale results. Every new query takes a ticket; when a
/// (possibly slow, background) query finishes, its results are applied only
/// if no newer query has started since, so fast typing can never let an old
/// result list replace a newer one.
#[derive(Debug, Default)]
pub struct QueryClock {
    latest: AtomicU64,
}

impl QueryClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a new query, making every earlier ticket stale.
    pub fn next(&self) -> Ticket {
        Ticket(self.latest.fetch_add(1, Ordering::AcqRel) + 1)
    }

    pub fn is_current(&self, ticket: Ticket) -> bool {
        self.latest.load(Ordering::Acquire) == ticket.0
    }
}

/// Keyboard selection over a result list.
///
/// When results change, the selection stays on the same item if it is
/// still present (so a background refresh does not move it), and otherwise
/// returns to the top result, which is what a launcher should activate on
/// Enter after the query changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    index: usize,
    len: usize,
}

impl Selection {
    pub fn index(&self) -> Option<usize> {
        (self.len > 0).then_some(self.index)
    }

    /// Moves down one row, stopping at the last.
    pub fn next(&mut self) {
        if self.index + 1 < self.len {
            self.index += 1;
        }
    }

    /// Moves up one row, stopping at the first.
    pub fn previous(&mut self) {
        self.index = self.index.saturating_sub(1);
    }

    pub fn select(&mut self, index: usize) {
        if index < self.len {
            self.index = index;
        }
    }

    /// Adapts to a new result list. `kept` is the new position of the
    /// previously selected item, if it survived.
    pub fn reset(&mut self, len: usize, kept: Option<usize>) {
        self.len = len;
        self.index = kept.filter(|&i| i < len).unwrap_or(0);
    }

    /// Like [`Selection::reset`], keeping the selected item by id when the
    /// user's query did not change (for example a background refresh).
    pub fn reset_keeping(&mut self, old: &[ResultItem], new: &[ResultItem]) {
        let kept = self
            .index()
            .and_then(|i| old.get(i))
            .and_then(|item| new.iter().position(|n| n.id == item.id));
        self.reset(new.len(), kept);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, score: u64) -> ResultItem {
        ResultItem {
            id: id.into(),
            kind: ResultKind::Application,
            title: id.into(),
            subtitle: None,
            score,
            action: Action::OpenApplication(PathBuf::from(format!("/Applications/{id}.app"))),
        }
    }

    struct Fixed(&'static str, Vec<ResultItem>);

    impl Provider for Fixed {
        fn name(&self) -> &'static str {
            self.0
        }
        fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
            self.1
                .iter()
                .filter(|i| i.title.contains(query))
                .take(limit)
                .cloned()
                .collect()
        }
    }

    #[test]
    fn merges_providers_by_score_with_stable_ties() {
        let a = Fixed("a", vec![item("alpha", 5), item("beta", 9)]);
        let b = Fixed("b", vec![item("gamma", 7), item("aardvark", 5)]);
        let search = Search::new(vec![Arc::new(a), Arc::new(b)]);
        let titles: Vec<_> = search
            .search("a", 10)
            .iter()
            .map(|r| r.title.to_string())
            .collect();
        assert_eq!(titles, ["beta", "gamma", "aardvark", "alpha"]);
        assert_eq!(search.search("a", 2).len(), 2);
        assert!(Search::default().search("a", 5).is_empty());
    }

    #[test]
    fn only_the_latest_ticket_is_current() {
        let clock = QueryClock::new();
        let first = clock.next();
        assert!(clock.is_current(first));
        let second = clock.next();
        assert!(!clock.is_current(first));
        assert!(clock.is_current(second));
    }

    #[test]
    fn selection_moves_within_bounds() {
        let mut s = Selection::default();
        assert_eq!(s.index(), None);
        s.next();
        assert_eq!(s.index(), None);

        s.reset(3, None);
        assert_eq!(s.index(), Some(0));
        s.previous();
        assert_eq!(s.index(), Some(0));
        s.next();
        s.next();
        s.next();
        assert_eq!(s.index(), Some(2));
        s.select(1);
        assert_eq!(s.index(), Some(1));
        s.select(7);
        assert_eq!(s.index(), Some(1));
        s.reset(0, None);
        assert_eq!(s.index(), None);
    }

    #[test]
    fn selection_follows_the_same_item_across_updates() {
        let old = vec![item("a", 3), item("b", 2), item("c", 1)];
        let mut s = Selection::default();
        s.reset(old.len(), None);
        s.next(); // "b"

        let reordered = vec![item("b", 3), item("a", 2), item("c", 1)];
        s.reset_keeping(&old, &reordered);
        assert_eq!(s.index(), Some(0));

        let without_b = vec![item("c", 1)];
        s.reset_keeping(&reordered, &without_b);
        assert_eq!(s.index(), Some(0));
        s.reset_keeping(&without_b, &[]);
        assert_eq!(s.index(), None);
    }
}
