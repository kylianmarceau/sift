//! What VoiceOver says about the launcher.
//!
//! GPUI exposes no accessibility tree, so the launcher announces changes
//! itself (see `sift_macos::a11y`): the highlighted result when it moves,
//! and, once typing pauses, how many results there are and which is first.

use sift_core::{ResultItem, ResultKind};

/// Longer titles (clipboard text, for example) are cut short when spoken.
const MAX_SPOKEN_TITLE: usize = 120;

/// "report.pdf, file, in Documents": the containing folder by name only,
/// since a whole path takes long to hear.
pub fn describe(item: &ResultItem) -> String {
    let title = shorten(&item.title);
    let kind = item.kind.label().to_lowercase();
    let folder = item
        .subtitle
        .as_deref()
        .and_then(|path| path.trim_end_matches('/').rsplit('/').next())
        .filter(|name| !name.is_empty());
    match (folder, item.kind) {
        (Some(folder), ResultKind::File | ResultKind::Folder) => {
            format!("{title}, {kind}, in {}", shorten(folder))
        }
        _ => format!("{title}, {kind}"),
    }
}

/// Spoken when the highlight moves: "report.pdf, file, in Documents,
/// 2 of 8".
pub fn selection(item: &ResultItem, index: usize, total: usize) -> String {
    format!("{}, {} of {total}", describe(item), index + 1)
}

/// Spoken when typing pauses: "8 results. Safari, application".
pub fn results(results: &[ResultItem], selected: Option<usize>) -> Option<String> {
    let item = results.get(selected?)?;
    let count = match results.len() {
        1 => "1 result".to_string(),
        n => format!("{n} results"),
    };
    Some(format!("{count}. {}", describe(item)))
}

fn shorten(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_SPOKEN_TITLE) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sift_core::Action;

    fn item(kind: ResultKind, title: &str, subtitle: Option<&str>) -> ResultItem {
        ResultItem {
            id: title.into(),
            kind,
            title: title.into(),
            subtitle: subtitle.map(Into::into),
            score: 0,
            action: Action::Run("x"),
        }
    }

    #[test]
    fn files_are_described_with_their_folder() {
        let file = item(ResultKind::File, "report.pdf", Some("~/Work/Documents"));
        assert_eq!(describe(&file), "report.pdf, file, in Documents");
        assert_eq!(
            selection(&file, 1, 8),
            "report.pdf, file, in Documents, 2 of 8"
        );
        let top = item(ResultKind::Folder, "Users", Some("/"));
        assert_eq!(describe(&top), "Users, folder");
        let app = item(ResultKind::Application, "Safari", Some("/Applications"));
        assert_eq!(describe(&app), "Safari, application");
    }

    #[test]
    fn results_name_the_count_and_the_highlighted_item() {
        let list = [
            item(ResultKind::Application, "Safari", None),
            item(ResultKind::Command, "Quit Sift", None),
        ];
        assert_eq!(
            results(&list, Some(0)).as_deref(),
            Some("2 results. Safari, application")
        );
        assert_eq!(
            results(&list[..1], Some(0)).as_deref(),
            Some("1 result. Safari, application")
        );
        assert_eq!(results(&list, None), None);
        assert_eq!(results(&[], Some(0)), None);
    }

    #[test]
    fn long_titles_are_cut_short() {
        let long = "x".repeat(500);
        let spoken = describe(&item(ResultKind::Clipboard, &long, None));
        assert!(spoken.starts_with(&"x".repeat(MAX_SPOKEN_TITLE)));
        assert!(spoken.contains("…, clipboard"));
    }
}
