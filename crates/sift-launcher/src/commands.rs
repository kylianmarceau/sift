//! Built-in commands, offered as search results like apps and files.

use sift_core::{Action, Provider, ResultItem, ResultKind};
use sift_search::{NameMatcher, fold};

pub const SETTINGS: &str = "settings";
pub const EDIT_CONFIG: &str = "edit-config";
pub const CLIPBOARD: &str = "clipboard";
pub const CLEAR_CLIPBOARD: &str = "clear-clipboard";
pub const QUIT: &str = "quit";
pub const CHECK_UPDATES: &str = "check-updates";

/// Every built-in command id (stored recents may only name these).
pub const ALL: &[&str] = &[
    SETTINGS,
    EDIT_CONFIG,
    CLIPBOARD,
    CLEAR_CLIPBOARD,
    CHECK_UPDATES,
    QUIT,
];

struct Spec {
    id: &'static str,
    title: &'static str,
    subtitle: &'static str,
    /// Other words the command is found by.
    aliases: &'static [&'static str],
}

const COMMANDS: &[Spec] = &[
    Spec {
        id: SETTINGS,
        title: "Sift Settings",
        subtitle: "Choose which folders Sift searches",
        aliases: &["preferences", "index folders", "file search"],
    },
    Spec {
        id: EDIT_CONFIG,
        title: "Edit Sift Configuration",
        subtitle: "Snippets and custom commands (config.toml)",
        aliases: &["snippets", "custom commands", "config"],
    },
    Spec {
        id: CLIPBOARD,
        title: "Clipboard History",
        subtitle: "Search text you copied (type clip, then words)",
        aliases: &["paste history", "copied"],
    },
    Spec {
        id: CLEAR_CLIPBOARD,
        title: "Clear Clipboard History",
        subtitle: "Delete every entry Sift has kept",
        aliases: &["delete clipboard history"],
    },
    Spec {
        id: CHECK_UPDATES,
        title: "Check for Updates",
        subtitle: "See whether a newer Sift has been released",
        aliases: &["update", "upgrade", "new version", "about sift", "version"],
    },
    Spec {
        id: QUIT,
        title: "Quit Sift",
        subtitle: "Stop Sift and live file indexing",
        aliases: &["exit"],
    },
];

pub struct CommandProvider;

impl Provider for CommandProvider {
    fn name(&self) -> &'static str {
        "commands"
    }

    fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
        let Some(matcher) = NameMatcher::new(query) else {
            return Vec::new();
        };
        let mut results: Vec<ResultItem> = COMMANDS
            .iter()
            .filter_map(|spec| {
                let score = std::iter::once(spec.title)
                    .chain(spec.aliases.iter().copied())
                    .filter_map(|name| matcher.score(&fold(name.as_bytes())).map(|(_, k)| k))
                    .max()?;
                Some(ResultItem {
                    id: format!("command:{}", spec.id).into(),
                    kind: ResultKind::Command,
                    title: spec.title.into(),
                    subtitle: Some(spec.subtitle.into()),
                    score,
                    action: Action::Run(spec.id),
                })
            })
            .collect();
        results.sort_by_key(|r| std::cmp::Reverse(r.score));
        results.truncate(limit);
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(query: &str) -> Vec<String> {
        CommandProvider
            .search(query, 10)
            .into_iter()
            .map(|r| r.id.to_string())
            .collect()
    }

    #[test]
    fn finds_commands_by_title_and_alias() {
        assert_eq!(ids("settings"), ["command:settings"]);
        assert_eq!(ids("prefer"), ["command:settings"]);
        assert_eq!(ids("quit"), ["command:quit"]);
        assert!(ids("").is_empty());
        assert!(ids("zzzz").is_empty());
    }
}
