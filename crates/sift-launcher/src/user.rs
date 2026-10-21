//! The user's own data in the launcher: `config.toml` (snippets, custom
//! commands) and `state.db` (recents, favorites).
//!
//! Stored recents and favorites are never trusted on their own: a snippet
//! or custom command is looked up again in the current config (so a
//! command removed from the file can no longer run from the history), and
//! files and apps must still exist.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use sift_core::{Action, Boosts, Provider, ResultItem, ResultKind};
use sift_search::{NameMatcher, fold};
use sift_state::{CommandAction, Config, CustomCommand, Snippet, StateDb};

use crate::clipboard::ClipboardProvider;
use crate::commands;

/// Recents shown under the favorites when the query is empty.
const HOME_RECENTS: usize = 8;
const SNIPPET_PREFIX: &str = "snippet:";
const CUSTOM_PREFIX: &str = "custom:";

pub struct UserData {
    /// Something the user should be told once (a damaged file replaced).
    notice: Option<String>,
    config_path: PathBuf,
    /// Modification time of the loaded config; `None` if there was none.
    config_stamp: Option<SystemTime>,
    loaded: bool,
    config: Config,
    db: Option<StateDb>,
    snippets: Arc<SnippetProvider>,
    custom: Arc<CustomCommandProvider>,
    clipboard: Arc<ClipboardProvider>,
    favorites: HashSet<Arc<str>>,
    boosts: Boosts,
}

/// What [`UserData::reload_config`] found.
pub enum Reload {
    Unchanged,
    /// The file changed (or appeared, or went away) and was loaded again,
    /// with these problems.
    Reloaded(Vec<String>),
}

impl UserData {
    /// Opens `dir/config.toml` and `dir/state.db`. A database that cannot
    /// be opened only disables recents and favorites.
    pub fn open(dir: &Path) -> Self {
        let mut notice = None;
        let db = match StateDb::open_recovering(&dir.join("state.db")) {
            Ok((db, moved)) => {
                if let Some(moved) = moved {
                    notice = Some(format!(
                        "Recent items and favorites were damaged and have been reset. \
                         The old file is {}.",
                        moved.display()
                    ));
                }
                Some(db)
            }
            Err(error) => {
                tracing::error!(%error, "cannot open state.db");
                None
            }
        };
        let mut this = Self {
            notice,
            config_path: dir.join("config.toml"),
            config_stamp: None,
            loaded: false,
            config: Config::default(),
            db,
            snippets: Arc::default(),
            custom: Arc::default(),
            clipboard: Arc::default(),
            favorites: HashSet::new(),
            boosts: Boosts::default(),
        };
        if let Reload::Reloaded(problems) = this.reload_config() {
            for problem in problems {
                tracing::warn!(%problem, "config");
            }
        }
        this.refresh_usage();
        this
    }

    /// A message for the user, given once.
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Loads `config.toml` again if its modification time changed. One
    /// `stat` when nothing changed.
    pub fn reload_config(&mut self) -> Reload {
        let stamp = std::fs::metadata(&self.config_path)
            .and_then(|m| m.modified())
            .ok();
        if self.loaded && stamp == self.config_stamp {
            return Reload::Unchanged;
        }
        let loaded = Config::load(&self.config_path);
        self.loaded = true;
        self.config_stamp = stamp;
        self.config = loaded.config;
        self.snippets = Arc::new(SnippetProvider::new(&self.config.snippets));
        self.custom = Arc::new(CustomCommandProvider::new(&self.config.commands));
        self.reload_clipboard();
        tracing::info!(
            snippets = self.config.snippets.len(),
            commands = self.config.commands.len(),
            problems = loaded.problems.len(),
            "configuration loaded"
        );
        Reload::Reloaded(loaded.problems)
    }

    pub fn clipboard_enabled(&self) -> bool {
        self.config.clipboard.enabled
    }

    /// Clipboard history entries; empty while the feature is off.
    pub fn clipboard(&self) -> Arc<ClipboardProvider> {
        self.clipboard.clone()
    }

    fn reload_clipboard(&mut self) {
        let entries = match (&self.db, self.config.clipboard.enabled) {
            (Some(db), true) => db
                .clipboard(self.config.clipboard.max_items)
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "cannot read clipboard history");
                    Vec::new()
                }),
            _ => Vec::new(),
        };
        self.clipboard = Arc::new(ClipboardProvider::new(entries));
    }

    /// Keeps `text` in the clipboard history (if enabled). Returns whether
    /// it was kept (blank and very large text is not).
    pub fn record_clipboard(&mut self, text: &str) -> bool {
        let (Some(db), true) = (&self.db, self.config.clipboard.enabled) else {
            return false;
        };
        let max = self.config.clipboard.max_items;
        match db.clipboard_add(text, sift_state::now(), max) {
            Ok(true) => {
                self.reload_clipboard();
                true
            }
            Ok(false) => false,
            Err(error) => {
                tracing::warn!(%error, "cannot record clipboard entry");
                false
            }
        }
    }

    pub fn clear_clipboard(&mut self) {
        if let Some(db) = &self.db
            && let Err(error) = db.clipboard_clear()
        {
            tracing::warn!(%error, "cannot clear clipboard history");
        }
        self.reload_clipboard();
    }

    pub fn snippets(&self) -> Arc<dyn Provider> {
        self.snippets.clone()
    }

    pub fn custom_commands(&self) -> Arc<dyn Provider> {
        self.custom.clone()
    }

    pub fn boosts(&self) -> Boosts {
        self.boosts.clone()
    }

    pub fn is_favorite(&self, id: &str) -> bool {
        self.favorites.contains(id)
    }

    /// Re-reads favorites and boosts (boosts fade with time).
    pub fn refresh_usage(&mut self) {
        let Some(db) = &self.db else {
            return;
        };
        match db.usage_boosts(sift_state::now()) {
            Ok(boosts) => self.boosts = Arc::new(boosts),
            Err(error) => tracing::warn!(%error, "cannot read usage"),
        }
        match db.favorites(commands::ALL) {
            Ok(items) => self.favorites = items.into_iter().map(|i| i.id).collect(),
            Err(error) => tracing::warn!(%error, "cannot read favorites"),
        }
    }

    /// Favorites, then recent items, as shown for an empty query. Items
    /// that no longer exist are left out.
    pub fn home(&self) -> Vec<ResultItem> {
        let Some(db) = &self.db else {
            return Vec::new();
        };
        let favorites = db.favorites(commands::ALL).unwrap_or_default();
        let recents = db
            .recent(HOME_RECENTS * 2, commands::ALL)
            .unwrap_or_default();
        let mut seen = HashSet::new();
        let mut items: Vec<ResultItem> = favorites
            .into_iter()
            .filter_map(|item| self.resolve(item))
            .filter(|item| seen.insert(item.id.clone()))
            .collect();
        items.extend(
            recents
                .into_iter()
                .filter_map(|item| self.resolve(item))
                .filter(|item| seen.insert(item.id.clone()))
                .take(HOME_RECENTS),
        );
        items
    }

    /// The current form of a stored item, if it is still valid.
    fn resolve(&self, item: ResultItem) -> Option<ResultItem> {
        if item.id.starts_with(SNIPPET_PREFIX) {
            return self.snippets.get(&item.id);
        }
        if item.id.starts_with(CUSTOM_PREFIX) {
            return self.custom.get(&item.id);
        }
        match &item.action {
            Action::OpenApplication(path) | Action::OpenPath(path) => {
                std::fs::symlink_metadata(path).ok().map(|_| item)
            }
            Action::Run(_) => Some(item),
            // Only snippets and custom commands produce these, and they
            // were resolved above.
            Action::OpenUrl(_) | Action::CopyText(_) | Action::Shell(_) => None,
        }
    }

    pub fn record_use(&mut self, item: &ResultItem) {
        if item.action == Action::Run(commands::QUIT) {
            return;
        }
        if let Some(db) = &self.db {
            if let Err(error) = db.record_use(item, sift_state::now()) {
                tracing::warn!(%error, "cannot record use");
            }
            self.refresh_usage();
        }
    }

    /// Returns whether `item` is now a favorite, or `None` if it cannot be
    /// one (calculator results, clipboard entries, no database).
    pub fn toggle_favorite(&mut self, item: &ResultItem) -> Option<bool> {
        if matches!(item.kind, ResultKind::Calculator | ResultKind::Clipboard) {
            return None;
        }
        let db = self.db.as_ref()?;
        let now = db
            .toggle_favorite(item, sift_state::now())
            .map_err(|error| tracing::warn!(%error, "cannot change favorite"))
            .ok()?;
        self.refresh_usage();
        Some(now)
    }

    /// Removes `id` from recents and favorites.
    pub fn forget(&mut self, id: &str) {
        if let Some(db) = &self.db {
            if let Err(error) = db.forget(id) {
                tracing::warn!(%error, "cannot forget item");
            }
            self.refresh_usage();
        }
    }
}

/// Replaces `{date}`, `{time}` and `{datetime}` in snippet text.
pub fn expand_placeholders(text: &str, date: &str, time: &str) -> String {
    if !text.contains('{') {
        return text.to_owned();
    }
    text.replace("{datetime}", &format!("{date} {time}"))
        .replace("{date}", date)
        .replace("{time}", time)
}

/// Names (and other words) an item is found by, folded for matching.
fn best_score(matcher: &NameMatcher, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .filter_map(|name| matcher.score(&fold(name.as_bytes())).map(|(_, k)| k))
        .max()
}

fn search_items(items: &[(Vec<String>, ResultItem)], query: &str, limit: usize) -> Vec<ResultItem> {
    let Some(matcher) = NameMatcher::new(query) else {
        return Vec::new();
    };
    let mut results: Vec<ResultItem> = items
        .iter()
        .filter_map(|(names, item)| {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let score = best_score(&matcher, &names)?;
            Some(ResultItem {
                score,
                ..item.clone()
            })
        })
        .collect();
    results.sort_by_key(|r| std::cmp::Reverse(r.score));
    results.truncate(limit);
    results
}

/// Snippets from the config: found by name or keyword; Enter copies the
/// text (placeholders are filled in at that moment).
#[derive(Default)]
pub struct SnippetProvider {
    items: Vec<(Vec<String>, ResultItem)>,
}

impl SnippetProvider {
    pub fn new(snippets: &[Snippet]) -> Self {
        let items = snippets
            .iter()
            .map(|snippet| {
                let mut names = vec![snippet.name.clone()];
                names.extend(snippet.keyword.clone());
                let preview = snippet
                    .text
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_default();
                let preview: String = preview.trim().chars().take(80).collect();
                let item = ResultItem {
                    id: format!("{SNIPPET_PREFIX}{}", snippet.name).into(),
                    kind: ResultKind::Snippet,
                    title: snippet.name.as_str().into(),
                    subtitle: Some(preview.into()),
                    score: 0,
                    action: Action::CopyText(snippet.text.as_str().into()),
                };
                (names, item)
            })
            .collect();
        Self { items }
    }

    fn get(&self, id: &str) -> Option<ResultItem> {
        self.items
            .iter()
            .find(|(_, item)| &*item.id == id)
            .map(|(_, item)| item.clone())
    }
}

impl Provider for SnippetProvider {
    fn name(&self) -> &'static str {
        "snippets"
    }

    fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
        search_items(&self.items, query, limit)
    }
}

/// Custom commands from the config.
#[derive(Default)]
pub struct CustomCommandProvider {
    items: Vec<(Vec<String>, ResultItem)>,
}

impl CustomCommandProvider {
    pub fn new(commands: &[CustomCommand]) -> Self {
        let items = commands
            .iter()
            .filter_map(|command| {
                let (action, detail) = match command.action().ok()? {
                    CommandAction::Url(url) => {
                        let detail = format!("Open {url}");
                        (Action::OpenUrl(url.into()), detail)
                    }
                    CommandAction::Open(path) => {
                        let detail = format!(
                            "Open {}",
                            sift_files::abbreviate_home(&path, std::env::home_dir().as_deref())
                        );
                        let is_app = path.extension().is_some_and(|e| e == "app");
                        let action = if is_app {
                            Action::OpenApplication(path)
                        } else {
                            Action::OpenPath(path)
                        };
                        (action, detail)
                    }
                    CommandAction::Shell(line) => {
                        let detail = format!("$ {line}");
                        (Action::Shell(line.into()), detail)
                    }
                };
                let item = ResultItem {
                    id: format!("{CUSTOM_PREFIX}{}", command.name).into(),
                    kind: ResultKind::Command,
                    title: command.name.as_str().into(),
                    subtitle: Some(command.subtitle.clone().unwrap_or(detail).into()),
                    score: 0,
                    action,
                };
                Some((vec![command.name.clone()], item))
            })
            .collect();
        Self { items }
    }

    fn get(&self, id: &str) -> Option<ResultItem> {
        self.items
            .iter()
            .find(|(_, item)| &*item.id == id)
            .map(|(_, item)| item.clone())
    }
}

impl Provider for CustomCommandProvider {
    fn name(&self) -> &'static str {
        "custom commands"
    }

    fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
        search_items(&self.items, query, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &Path, text: &str) {
        let path = dir.join("config.toml");
        std::fs::write(&path, text).expect("write config");
        // Make sure the modification time differs from the last load.
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(later))
            .expect("set mtime");
    }

    const CONFIG: &str = r#"
        [[snippets]]
        name = "Email signature"
        keyword = "sig"
        text = "\nBest,\nK {date}"

        [[commands]]
        name = "Flush DNS"
        shell = "dscacheutil -flushcache"

        [[commands]]
        name = "Docs"
        url = "https://example.com/docs"
    "#;

    fn titles(provider: &dyn Provider, query: &str) -> Vec<String> {
        provider
            .search(query, 10)
            .into_iter()
            .map(|r| r.title.to_string())
            .collect()
    }

    #[test]
    fn snippets_and_commands_are_searchable() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_config(dir.path(), CONFIG);
        let user = UserData::open(dir.path());
        assert_eq!(titles(&*user.snippets(), "sig"), ["Email signature"]);
        assert_eq!(titles(&*user.snippets(), "email"), ["Email signature"]);
        let snippet = &user.snippets().search("sig", 1)[0];
        assert_eq!(snippet.subtitle.as_deref(), Some("Best,"));
        assert_eq!(snippet.action, Action::CopyText("\nBest,\nK {date}".into()));
        let flush = &user.custom_commands().search("flush", 1)[0];
        assert_eq!(
            flush.action,
            Action::Shell("dscacheutil -flushcache".into())
        );
        assert_eq!(flush.subtitle.as_deref(), Some("$ dscacheutil -flushcache"));
        assert!(user.snippets().search("", 10).is_empty());
    }

    #[test]
    fn recents_follow_the_current_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_config(dir.path(), CONFIG);
        let mut user = UserData::open(dir.path());
        let flush = user.custom_commands().search("flush", 1)[0].clone();
        user.record_use(&flush);
        assert_eq!(user.home().len(), 1);

        // The command is removed from the file: it can no longer be run
        // from the history.
        write_config(dir.path(), "");
        assert!(matches!(user.reload_config(), Reload::Reloaded(p) if p.is_empty()));
        assert!(user.home().is_empty());
        assert!(matches!(user.reload_config(), Reload::Unchanged));
    }

    #[test]
    fn home_lists_favorites_then_existing_recents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut user = UserData::open(dir.path());
        let file = dir.path().join("report.pdf");
        std::fs::write(&file, b"x").expect("write");
        let item = |id: &str, path: &Path| ResultItem {
            id: id.into(),
            kind: ResultKind::File,
            title: id.into(),
            subtitle: None,
            score: 0,
            action: Action::OpenPath(path.to_path_buf()),
        };
        let report = item("report", &file);
        let gone = item("gone", &dir.path().join("deleted.txt"));
        let settings = ResultItem {
            id: "command:settings".into(),
            kind: ResultKind::Command,
            title: "Sift Settings".into(),
            subtitle: None,
            score: 0,
            action: Action::Run(commands::SETTINGS),
        };
        user.record_use(&report);
        user.record_use(&gone);
        user.record_use(&settings);
        assert_eq!(user.toggle_favorite(&report), Some(true));
        assert!(user.is_favorite("report"));
        assert!(user.boosts()["report"] >= sift_state::FAVORITE_BOOST);
        let ids: Vec<String> = user.home().iter().map(|i| i.id.to_string()).collect();
        assert_eq!(ids, ["report", "command:settings"]);

        user.forget("report");
        assert!(!user.is_favorite("report"));
        let ids: Vec<String> = user.home().iter().map(|i| i.id.to_string()).collect();
        assert_eq!(ids, ["command:settings"]);
    }

    #[test]
    fn quit_and_calculator_results_are_not_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut user = UserData::open(dir.path());
        let quit = ResultItem {
            id: "command:quit".into(),
            kind: ResultKind::Command,
            title: "Quit Sift".into(),
            subtitle: None,
            score: 0,
            action: Action::Run(commands::QUIT),
        };
        user.record_use(&quit);
        let calc = ResultItem {
            id: "calculator".into(),
            kind: ResultKind::Calculator,
            title: "4".into(),
            subtitle: None,
            score: 0,
            action: Action::CopyText("4".into()),
        };
        assert_eq!(user.toggle_favorite(&calc), None);
        assert!(user.home().is_empty());
    }

    #[test]
    fn clipboard_history_is_kept_only_when_enabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut user = UserData::open(dir.path());
        assert!(!user.record_clipboard("secret"));
        assert_eq!(user.clipboard().len(), 0);

        write_config(dir.path(), "[clipboard]\nenabled = true\nmax_items = 2\n");
        user.reload_config();
        assert!(user.record_clipboard("one"));
        assert!(user.record_clipboard("two"));
        assert!(user.record_clipboard("three"));
        let titles: Vec<String> = user
            .clipboard()
            .search("clip", 10)
            .into_iter()
            .map(|r| r.title.to_string())
            .collect();
        assert_eq!(titles, ["three", "two"]);

        // Turning it off hides what was kept (without deleting it).
        write_config(dir.path(), "");
        user.reload_config();
        assert_eq!(user.clipboard().len(), 0);
        write_config(dir.path(), "[clipboard]\nenabled = true\n");
        user.reload_config();
        assert_eq!(user.clipboard().len(), 2);
        user.clear_clipboard();
        assert_eq!(user.clipboard().len(), 0);
    }

    #[test]
    fn placeholders_are_filled_in() {
        assert_eq!(
            expand_placeholders("on {date} at {time} ({datetime})", "2026-10-21", "09:30"),
            "on 2026-10-21 at 09:30 (2026-10-21 09:30)"
        );
        assert_eq!(expand_placeholders("plain", "d", "t"), "plain");
    }
}
