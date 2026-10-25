//! `config.toml`: snippets, custom commands and options, edited by hand.
//! The settings window can also change the options ([`Setting`]); it
//! edits the file in place and keeps everything else in it.
//!
//! A missing file means defaults. A file with mistakes still loads: each
//! problem is reported (and shown in the launcher) and the rest is used,
//! so one typo never disables every snippet.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The most clipboard entries `max_items` can ask for.
pub const MAX_CLIP_ITEMS: usize = 10_000;

/// Written when the user first asks to edit the configuration.
pub const TEMPLATE: &str = r#"# Sift configuration. Changes apply the next time the launcher opens.
# The [clipboard] and [updates] options can also be changed in Sift
# Settings, which edits this file and leaves the rest of it as it is.

[clipboard]
# Keep a history of copied text (plain text only), searchable by typing
# "clip" and then words, as in "clip invoice". Off by default: copied text
# can be sensitive. Text that password managers mark as concealed is never
# kept. Turning this off stops recording and hides the history; the
# "Clear Clipboard History" command deletes it.
enabled = false
# How many entries to keep.
max_items = 200

[updates]
# Once a day at most, ask GitHub whether a newer Sift has been released,
# and say so in the launcher. One HTTPS request; nothing is downloaded or
# installed, and nothing about you is sent. Set to false to never check.
check = true

[window_manager]
# Native tiling and workspaces. Requires Accessibility permission when on.
# Off means no window observers, worker threads, or window-management hotkeys.
enabled = false
# Optional layout/rule/binding examples: docs/window-manager.md

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

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub clipboard: ClipboardConfig,
    pub updates: UpdatesConfig,
    pub window_manager: sift_wm::Config,
    pub snippets: Vec<Snippet>,
    pub commands: Vec<CustomCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    pub enabled: bool,
    pub max_items: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct UpdatesConfig {
    /// Check for a newer release once a day.
    pub check: bool,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self { check: true }
    }
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
        config.clipboard.max_items = config.clipboard.max_items.clamp(1, MAX_CLIP_ITEMS);
        if let Err(error) = config.window_manager.validate() {
            problems.push(format!("Window Management: {error}"));
            config.window_manager.enabled = false;
        }
        Loaded { config, problems }
    }

    /// Changes one option in the file at `path` and keeps the rest of the
    /// file as it was: comments, snippets, commands and layout. A missing
    /// file starts from [`TEMPLATE`]. A file that is not valid TOML is
    /// left alone, so a hand edit in progress is never overwritten.
    ///
    /// The file is replaced atomically. If `path` is a symbolic link (a
    /// dotfiles checkout), the file it points to is the one written.
    pub fn save_setting(path: &Path, setting: Setting) -> Result<(), String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_string(),
            Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
        };
        let edited = apply_setting(&text, setting)?;
        if edited == text && path.exists() {
            return Ok(());
        }
        write_replacing(path, &edited).map_err(|e| format!("Cannot save {}: {e}", path.display()))
    }
}

/// An option the settings window can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// `[clipboard] enabled`
    ClipboardEnabled(bool),
    /// `[clipboard] max_items`, clamped to 1..=[`MAX_CLIP_ITEMS`].
    ClipboardMaxItems(usize),
    /// `[updates] check`
    UpdateChecks(bool),
    /// `[window_manager] enabled`
    WindowManager(bool),
}

/// `text` (a `config.toml`) with `setting` applied and everything else
/// unchanged. Fails, without changing anything, if `text` is not valid
/// TOML or the option's table is something other than a table.
pub fn apply_setting(text: &str, setting: Setting) -> Result<String, String> {
    use toml_edit::{DocumentMut, Item, Value};

    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        format!(
            "config.toml has a mistake, so it was not changed: {}",
            e.message()
        )
    })?;
    let (table, key, value): (&str, &str, Value) = match setting {
        Setting::ClipboardEnabled(on) => ("clipboard", "enabled", on.into()),
        Setting::ClipboardMaxItems(n) => (
            "clipboard",
            "max_items",
            (n.clamp(1, MAX_CLIP_ITEMS) as i64).into(),
        ),
        Setting::UpdateChecks(on) => ("updates", "check", on.into()),
        Setting::WindowManager(on) => ("window_manager", "enabled", on.into()),
    };
    let section = doc
        .as_table_mut()
        .entry(table)
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .ok_or_else(|| format!("[{table}] in config.toml is not a table, so it was not changed"))?;
    match section.get_mut(key) {
        // Keep any comment after the old value.
        Some(Item::Value(old)) => {
            let decor = old.decor().clone();
            *old = value;
            *old.decor_mut() = decor;
        }
        _ => {
            section.insert(key, Item::Value(value));
        }
    }
    let edited = doc.to_string();
    // The launcher must still be able to read what is written.
    toml::from_str::<Config>(&edited)
        .map_err(|e| format!("config.toml was not changed: {}", e.message()))?;
    Ok(edited)
}

/// Writes `text` to a temporary file beside `path`'s target and renames
/// it over the target, so a reader never sees half a file.
fn write_replacing(path: &Path, text: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = std::fs::write(&temp, text)
        .and_then(|()| match std::fs::metadata(&target) {
            Ok(meta) => std::fs::set_permissions(&temp, meta.permissions()),
            Err(_) => Ok(()),
        })
        .and_then(|()| std::fs::rename(&temp, &target));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

impl Config {
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
    fn window_management_is_opt_in_and_its_settings_do_not_enable_clipboard_history() {
        assert!(!Config::parse(TEMPLATE).config.window_manager.enabled);
        let edited = apply_setting(TEMPLATE, Setting::WindowManager(true)).expect("setting");
        let loaded = Config::parse(&edited);
        assert!(loaded.problems.is_empty());
        assert!(loaded.config.window_manager.enabled);
        assert!(!loaded.config.clipboard.enabled);
        assert!(loaded.config.updates.check);
        let broken = "[updates]\ncheck = false\n[window_manager]\nenabled = true\n[[window_manager.rules]]\napp_id = '['\n";
        let loaded = Config::parse(broken);
        assert!(!loaded.problems.is_empty());
        assert!(!loaded.config.window_manager.enabled);
        assert!(!loaded.config.updates.check);
    }

    #[test]
    fn template_parses_to_the_defaults() {
        let loaded = Config::parse(TEMPLATE);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn update_checks_are_on_unless_turned_off() {
        assert!(Config::parse("").config.updates.check);
        assert!(
            !Config::parse("[updates]\ncheck = false\n")
                .config
                .updates
                .check
        );
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
    fn settings_change_only_their_value() {
        let text = TEMPLATE.replace("max_items = 200", "max_items = 200 # mine");
        let on = apply_setting(&text, Setting::ClipboardEnabled(true)).unwrap();
        assert_eq!(on, text.replacen("enabled = false", "enabled = true", 1));
        let fewer = apply_setting(&on, Setting::ClipboardMaxItems(50)).unwrap();
        // The comment after the value stays.
        assert!(fewer.contains("max_items = 50 # mine"), "{fewer}");
        let off = apply_setting(&fewer, Setting::UpdateChecks(false)).unwrap();
        assert_eq!(off, fewer.replace("check = true", "check = false"));
        let config = Config::parse(&off).config;
        assert!(config.clipboard.enabled);
        assert_eq!(config.clipboard.max_items, 50);
        assert!(!config.updates.check);
    }

    #[test]
    fn settings_add_missing_tables_and_keep_snippets() {
        let text = "# my notes\n[[snippets]]\nname = \"Sig\"\ntext = \"Hi\"\n";
        let edited = apply_setting(text, Setting::ClipboardEnabled(true)).unwrap();
        assert!(edited.starts_with(text), "{edited}");
        let config = Config::parse(&edited).config;
        assert!(config.clipboard.enabled);
        assert_eq!(config.snippets.len(), 1);

        let inline = "clipboard = { max_items = 9 }\n";
        let inline = apply_setting(inline, Setting::ClipboardEnabled(true)).unwrap();
        let config = Config::parse(&inline).config;
        assert!(config.clipboard.enabled);
        assert_eq!(config.clipboard.max_items, 9);
    }

    #[test]
    fn settings_refuse_broken_files() {
        assert!(apply_setting("[[snippets]\nname = ", Setting::UpdateChecks(false)).is_err());
        assert!(apply_setting("clipboard = 3\n", Setting::ClipboardEnabled(true)).is_err());
        let huge = apply_setting("", Setting::ClipboardMaxItems(1_000_000)).unwrap();
        assert_eq!(
            Config::parse(&huge).config.clipboard.max_items,
            MAX_CLIP_ITEMS
        );
    }

    #[test]
    fn save_setting_writes_the_template_or_edits_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::save_setting(&path, Setting::ClipboardEnabled(true)).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            TEMPLATE.replacen("enabled = false", "enabled = true", 1)
        );

        // A dotfiles-style link: the linked file changes, the link stays.
        let real = dir.path().join("dotfiles.toml");
        std::fs::write(&real, "[updates]\ncheck = true\n").unwrap();
        let link = dir.path().join("linked.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        Config::save_setting(&link, Setting::UpdateChecks(false)).unwrap();
        let link_type = std::fs::symlink_metadata(&link).unwrap().file_type();
        assert!(link_type.is_symlink());
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "[updates]\ncheck = false\n"
        );

        // A broken file is left as it is.
        std::fs::write(&path, "[[snippets]\n").unwrap();
        assert!(Config::save_setting(&path, Setting::ClipboardEnabled(false)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[snippets]\n");
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
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
