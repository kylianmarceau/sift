//! Built-in commands, offered as search results like apps and files.

use sift_core::{Action, Provider, ResultItem, ResultKind};
use sift_search::{NameMatcher, fold};

pub const SETTINGS: &str = "settings";
pub const QUIT: &str = "quit";

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
