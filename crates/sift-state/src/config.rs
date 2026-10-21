//! `config.toml`: snippets, custom commands and options, edited by hand.
//!
//! A missing file means defaults. A file with mistakes still loads: each
//! problem is reported (and shown in the launcher) and the rest is used,
//! so one typo never disables every snippet.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Written when the user first asks to edit the configuration.
pub const TEMPLATE: &str = r#"# Sift configuration. Changes apply the next time the launcher opens.

[clipboard]
# Keep a history of copied text (plain text only), searchable by typing
# "clip" and then words, as in "clip invoice". Off by default: copied text
# can be sensitive. Text that password managers mark as concealed is never
# kept. Turning this off stops recording and hides the history; the
# "Clear Clipboard History" command deletes it.
enabled = false
# How many entries to keep.
max_items = 200

# Snippets: search by name (or keyword); Enter copies the text.
# {date}, {time} and {datetime} are replaced when copied.
#
# [[snippets]]
# name = "Email signature"
# keyword = "sig"
# text = """
# Best regards,
# Kylian
# """

# Custom commands: search by name; Enter runs them. Each has exactly one
# of `url` (opened in its default app), `open` (a file, folder or app) or
# `shell` (run with /bin/sh; output goes to the log).
#
# [[commands]]
# name = "Sift on GitHub"
# url = "https://github.com/kylianmarceau/sift"
#
# [[commands]]
# name = "Projects folder"
# open = "~/Documents/projects"
#
# [[commands]]
# name = "Flush DNS cache"
# shell = "dscacheutil -flushcache"
"#;

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub clipboard: ClipboardConfig,
    pub snippets: Vec<Snippet>,
    pub commands: Vec<CustomCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    pub enabled: bool,
    pub max_items: usize,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_items: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Snippet {
    pub name: String,
    #[serde(default)]
    pub keyword: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CustomCommand {
    pub name: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub open: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
}

/// What a custom command does, once validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    Url(String),
    Open(PathBuf),
    Shell(String),
}

impl CustomCommand {
    /// The command's action, or why it is invalid.
    pub fn action(&self) -> Result<CommandAction, String> {
        match (&self.url, &self.open, &self.shell) {
            (Some(url), None, None) => Ok(CommandAction::Url(url.clone())),
            (None, Some(path), None) => Ok(CommandAction::Open(expand_tilde(path))),
            (None, None, Some(shell)) => Ok(CommandAction::Shell(shell.clone())),
            _ => Err(format!(
                "command \"{}\" needs exactly one of url, open or shell",
                self.name
            )),
        }
    }
}

/// The loaded configuration and anything wrong with the file.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub config: Config,
    pub problems: Vec<String>,
}

impl Config {
    /// Loads `path`. A missing file gives the defaults; a malformed one
    /// gives the defaults plus the parse error; invalid commands are
    /// dropped with a problem each.
    pub fn load(path: &Path) -> Loaded {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::default(),
            Err(e) => {
                return Loaded {
                    problems: vec![format!("cannot read {}: {e}", path.display())],
                    ..Loaded::default()
                };
            }
        };
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Loaded {
        let mut config: Config = match toml::from_str(text) {
            Ok(config) => config,
            Err(e) => {
                return Loaded {
                    problems: vec![format!("config.toml: {}", e.message())],
                    ..Loaded::default()
                };
            }
        };
        let mut problems = Vec::new();
        config.commands.retain(|c| match c.action() {
            Ok(_) => true,
            Err(problem) => {
                problems.push(problem);
                false
            }
        });
        config.clipboard.max_items = config.clipboard.max_items.clamp(1, 10_000);
        Loaded { config, problems }
    }

    /// Creates `path` from [`TEMPLATE`] if it does not exist yet.
    pub fn ensure_exists(path: &Path) -> std::io::Result<()> {
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, TEMPLATE)
    }
}

/// `~/x` → `/Users/me/x`.
pub fn expand_tilde(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if path == "~" => std::env::home_dir().unwrap_or_else(|| PathBuf::from(path)),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_the_defaults() {
        let loaded = Config::parse(TEMPLATE);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn reads_snippets_commands_and_options() {
        let loaded = Config::parse(
            r#"
            [clipboard]
            enabled = true
            max_items = 50

            [[snippets]]
            name = "Signature"
            keyword = "sig"
            text = "Best,\nK"

            [[commands]]
            name = "Docs"
            url = "https://example.com"

            [[commands]]
            name = "Home"
            open = "~"

            [[commands]]
            name = "Broken"
            url = "https://a"
            shell = "rm -rf /"
            "#,
        );
        let config = loaded.config;
        assert!(config.clipboard.enabled);
        assert_eq!(config.clipboard.max_items, 50);
        assert_eq!(config.snippets[0].keyword.as_deref(), Some("sig"));
        assert_eq!(config.snippets[0].text, "Best,\nK");
        // The ambiguous command is dropped and reported.
        assert_eq!(config.commands.len(), 2);
        assert_eq!(loaded.problems.len(), 1);
        assert!(loaded.problems[0].contains("Broken"));
        assert_eq!(
            config.commands[0].action(),
            Ok(CommandAction::Url("https://example.com".into()))
        );
    }

    #[test]
    fn malformed_file_falls_back_to_defaults_with_a_problem() {
        let loaded = Config::parse("[[snippets]\nname = ");
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.problems.len(), 1);
    }

    #[test]
    fn missing_file_is_not_a_problem_and_template_is_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        let loaded = Config::load(&path);
        assert!(loaded.problems.is_empty());
        Config::ensure_exists(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TEMPLATE);
        std::fs::write(&path, "# mine").unwrap();
        Config::ensure_exists(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# mine");
    }
}
