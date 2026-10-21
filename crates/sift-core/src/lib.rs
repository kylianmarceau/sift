//! Shared vocabulary of the sift launcher: what a search result is, what a
//! provider does, and the small pieces of query bookkeeping every front end
//! needs (stale-result suppression and selection).
//!
//! The crate has no dependencies and knows nothing about the UI, AppKit or
//! the file index, so providers and their ranking can be tested headlessly.
//! Providers: installed applications (`sift-apps`) and the file index
//! (`sift-files`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a result refers to. Drives the label and icon in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResultKind {
    Application,
    File,
    Folder,
    Command,
    Calculator,
    Snippet,
    Clipboard,
}

impl ResultKind {
    pub const ALL: [ResultKind; 7] = [
        Self::Application,
        Self::File,
        Self::Folder,
        Self::Command,
        Self::Calculator,
        Self::Snippet,
        Self::Clipboard,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Application => "Application",
            Self::File => "File",
            Self::Folder => "Folder",
            Self::Command => "Command",
            Self::Calculator => "Calculator",
            Self::Snippet => "Snippet",
            Self::Clipboard => "Clipboard",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.label() == label)
    }
}

/// The match tier encoded in bits 48–55 of a score. Rank keys from
/// `sift_search` use 1 (fuzzy) to 6 (exact); [`TIER_TOP`] is above all of
/// them, for results that answer the query directly (a calculation).
pub const TIER_TOP: u64 = 7;

/// A score in the top tier, above every name match.
pub fn top_score() -> u64 {
    TIER_TOP << 48
}

/// What happens when the user activates a result.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    /// Launch the application bundle, or bring it to the front if running.
    OpenApplication(PathBuf),
    /// Open a file or folder with its default application.
    OpenPath(PathBuf),
    /// Open a URL with its default handler (browser, mail client, ...).
    OpenUrl(Arc<str>),
    /// Put text on the clipboard (calculator results, snippets, clipboard
    /// history).
    CopyText(Arc<str>),
    /// Run a user-defined shell command (from the config file).
    Shell(Arc<str>),
    /// Run one of the launcher's built-in commands, by id.
    Run(&'static str),
}

impl Action {
    /// The file or bundle this action opens, if any (for icons and
    /// "Show in Finder").
    pub fn path(&self) -> Option<&std::path::Path> {
        match self {
            Self::OpenApplication(path) | Self::OpenPath(path) => Some(path),
            _ => None,
        }
    }

    /// A stable text form for storing (recent items, favorites).
    pub fn encode(&self) -> String {
        use std::os::unix::ffi::OsStrExt;
        let path = |p: &PathBuf| String::from_utf8_lossy(p.as_os_str().as_bytes()).into_owned();
        match self {
            Self::OpenApplication(p) => format!("app:{}", path(p)),
            Self::OpenPath(p) => format!("path:{}", path(p)),
            Self::OpenUrl(url) => format!("url:{url}"),
            Self::CopyText(text) => format!("copy:{text}"),
            Self::Shell(command) => format!("shell:{command}"),
            Self::Run(id) => format!("run:{id}"),
        }
    }

    /// Reverses [`Action::encode`]. Built-in command ids are only accepted
    /// if they are in `commands`, so stored data can never name a command
    /// that does not exist.
    pub fn decode(text: &str, commands: &[&'static str]) -> Option<Self> {
        let (tag, rest) = text.split_once(':')?;
        Some(match tag {
            "app" => Self::OpenApplication(rest.into()),
            "path" => Self::OpenPath(rest.into()),
            "url" => Self::OpenUrl(rest.into()),
            "copy" => Self::CopyText(rest.into()),
            "shell" => Self::Shell(rest.into()),
            "run" => Self::Run(commands.iter().copied().find(|c| *c == rest)?),
            _ => return None,
        })
    }
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

/// The match tier encoded in a score (bits 48–55 of a `sift_search` rank
/// key): exact, exact stem, prefix, word prefix, substring, fuzzy.
pub fn tier_of(score: u64) -> u8 {
    (score >> 48) as u8
}

/// Per-item ranking bonus from usage, by result id: higher for favorites
/// and for items used often and recently. Applied within a match tier.
pub type Boosts = Arc<std::collections::HashMap<Arc<str>, u8>>;

/// Queries every provider and merges their results into one ranked list.
#[derive(Clone, Default)]
pub struct Search {
    providers: Vec<Arc<dyn Provider>>,
    boosts: Boosts,
}

impl Search {
    /// `providers` in priority order: within the same match tier, results
    /// of earlier providers come first (applications before files).
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        Self {
            providers,
            boosts: Boosts::default(),
        }
    }

    /// Uses `boosts` to order results within a match tier.
    pub fn with_boosts(mut self, boosts: Boosts) -> Self {
        self.boosts = boosts;
        self
    }

    /// Runs all providers in parallel and merges their results:
    ///
    /// 1. match tier, so an exact file name beats a fuzzy application match;
    /// 2. usage boost (favorites, then frequently and recently used);
    /// 3. provider priority, so `Notes` the app precedes `notes.txt`;
    /// 4. the provider's score, then the title for a stable order.
    ///
    /// Results with the same id (an app that is also in the file index)
    /// are kept once, from the higher-priority provider.
    pub fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
        let per_provider: Vec<Vec<ResultItem>> = if self.providers.len() < 2 {
            self.providers
                .iter()
                .map(|p| p.search(query, limit))
                .collect()
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = self
                    .providers
                    .iter()
                    .map(|p| scope.spawn(move || p.search(query, limit)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_default())
                    .collect()
            })
        };
        let mut results: Vec<(usize, ResultItem)> = per_provider
            .into_iter()
            .enumerate()
            .flat_map(|(i, items)| items.into_iter().map(move |item| (i, item)))
            .collect();
        let boost = |item: &ResultItem| self.boosts.get(&item.id).copied().unwrap_or(0);
        results.sort_by(|(pa, a), (pb, b)| {
            tier_of(b.score)
                .cmp(&tier_of(a.score))
                .then(boost(b).cmp(&boost(a)))
                .then(pa.cmp(pb))
                .then(b.score.cmp(&a.score))
                .then_with(|| a.title.cmp(&b.title))
        });
        let mut seen = HashSet::new();
        results
            .into_iter()
            .filter(|(_, item)| seen.insert(item.id.clone()))
            .map(|(_, item)| item)
            .take(limit)
            .collect()
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

    fn key(tier: u64, rest: u64) -> u64 {
        (tier << 48) | rest
    }

    fn titles(results: &[ResultItem]) -> Vec<String> {
        results.iter().map(|r| r.title.to_string()).collect()
    }

    #[test]
    fn merges_by_tier_then_provider_then_score() {
        // Tiers: 6 exact, 4 prefix, 1 fuzzy.
        let apps = Fixed(
            "apps",
            vec![item("notes", key(6, 1)), item("nota fuzzy", key(1, 99))],
        );
        let files = Fixed(
            "files",
            vec![
                item("notes.txt", key(6, 50)),
                item("notebook", key(4, 9)),
                item("not exact", key(1, 500)),
            ],
        );
        let search = Search::new(vec![Arc::new(apps), Arc::new(files)]);
        assert_eq!(
            titles(&search.search("not", 10)),
            // Same tier: the app first even with a lower score. Different
            // tiers: the better match first, whatever the provider.
            ["notes", "notes.txt", "notebook", "nota fuzzy", "not exact"]
        );
    }

    #[test]
    fn usage_boost_orders_within_a_tier_only() {
        let apps = Fixed("apps", vec![item("notes", key(6, 1))]);
        let files = Fixed(
            "files",
            vec![item("notes.md", key(6, 1)), item("notebook", key(4, 1))],
        );
        let boosts: Boosts = Arc::new(
            [(Arc::from("notes.md"), 3u8), (Arc::from("notebook"), 9u8)]
                .into_iter()
                .collect(),
        );
        let search = Search::new(vec![Arc::new(apps), Arc::new(files)]).with_boosts(boosts);
        // The used file passes the app in the same tier; the heavily used
        // prefix match stays below both exact matches.
        assert_eq!(
            titles(&search.search("not", 10)),
            ["notes.md", "notes", "notebook"]
        );
    }

    #[test]
    fn actions_round_trip_through_their_text_form() {
        let actions = [
            Action::OpenApplication("/Applications/Safari.app".into()),
            Action::OpenPath("/Users/me/a b.txt".into()),
            Action::OpenUrl("https://example.com/?q=1".into()),
            Action::CopyText("line one\nline: two".into()),
            Action::Shell("say hi".into()),
            Action::Run("settings"),
        ];
        for action in actions {
            assert_eq!(
                Action::decode(&action.encode(), &["settings"]),
                Some(action)
            );
        }
        assert_eq!(Action::decode("run:format-disk", &["settings"]), None);
        assert_eq!(Action::decode("nonsense", &[]), None);
        for kind in ResultKind::ALL {
            assert_eq!(ResultKind::from_label(kind.label()), Some(kind));
        }
    }

    #[test]
    fn duplicate_ids_are_kept_once_from_the_first_provider() {
        let mut from_files = item("Safari", key(6, 1));
        from_files.kind = ResultKind::Folder;
        let search = Search::new(vec![
            Arc::new(Fixed("apps", vec![item("Safari", key(6, 1))])),
            Arc::new(Fixed("files", vec![from_files])),
        ]);
        let results = search.search("Safari", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].kind, ResultKind::Application);
    }

    #[test]
    fn merges_providers_by_score_with_stable_ties() {
        let a = Fixed("a", vec![item("alpha", 5), item("beta", 9)]);
        let b = Fixed("b", vec![item("gamma", 9), item("aardvark", 5)]);
        let search = Search::new(vec![Arc::new(a), Arc::new(b)]);
        // All in tier 0: provider order, then score.
        assert_eq!(
            titles(&search.search("a", 10)),
            ["beta", "alpha", "gamma", "aardvark"]
        );
        assert_eq!(titles(&search.search("a", 3)).len(), 3);
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
