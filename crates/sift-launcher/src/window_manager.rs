//! Cached window/workspace results. Searching never calls AX, takes a backend
//! lock, or launches a process. Names are folded once per model publication.
use sift_core::{Action, Provider, ResultItem, ResultKind};
use sift_search::{NameMatcher, fold};
use sift_wm::Snapshot;
use std::sync::Arc;

pub struct WindowProvider {
    items: Vec<(ResultItem, Box<[u8]>)>,
}
impl WindowProvider {
    pub fn new(snapshot: Arc<Snapshot>) -> Self {
        let mut items = Vec::new();
        if !snapshot.enabled {
            return Self { items };
        }
        let mut counts = std::collections::BTreeMap::<&str, usize>::new();
        for w in &snapshot.windows {
            *counts.entry(&w.workspace).or_default() += 1;
            if w.minimized {
                continue;
            }
            let folded = fold(format!("{} {}", w.app_name, w.title).as_bytes()).into_boxed_slice();
            let item = ResultItem {
                id: format!("window:{}:{}", w.pid, w.id).into(),
                kind: ResultKind::Window,
                title: if w.title.is_empty() {
                    w.app_name.clone()
                } else {
                    w.title.clone()
                }
                .into(),
                subtitle: Some(format!("{} · Workspace {}", w.app_name, w.workspace).into()),
                score: 0,
                action: Action::WindowCommand(format!("focus --window-id {}", w.id).into()),
            };
            items.push((item, folded));
        }
        for w in &snapshot.workspaces {
            let title = format!("Workspace {}", w.name);
            let folded = fold(title.as_bytes()).into_boxed_slice();
            // Double quotes preserve spaces, apostrophes and escaped backslashes.
            let name = serde_json::to_string(&w.name).expect("string serialization");
            let item = ResultItem {
                id: format!("workspace:{}", w.name).into(),
                kind: ResultKind::Workspace,
                title: title.into(),
                subtitle: Some(
                    format!(
                        "{} windows",
                        counts.get(w.name.as_str()).copied().unwrap_or(0)
                    )
                    .into(),
                ),
                score: 0,
                action: Action::WindowCommand(format!("workspace {name}").into()),
            };
            items.push((item, folded));
        }
        Self { items }
    }
}

/// Whether two snapshots give the same window and workspace results.
/// Frames are not searched, so moving or resizing a window changes nothing.
pub fn same_search_content(a: &Snapshot, b: &Snapshot) -> bool {
    a.enabled == b.enabled
        && a.windows.len() == b.windows.len()
        && a.workspaces.len() == b.workspaces.len()
        && a.windows.iter().zip(&b.windows).all(|(a, b)| {
            a.id == b.id
                && a.pid == b.pid
                && a.minimized == b.minimized
                && a.workspace == b.workspace
                && a.title == b.title
                && a.app_name == b.app_name
        })
        && a.workspaces
            .iter()
            .zip(&b.workspaces)
            .all(|(a, b)| a.name == b.name)
}

/// Whether the menu and Settings would show the same thing for both:
/// search content plus focus, visible workspaces, mode and status.
pub fn same_overview(a: &Snapshot, b: &Snapshot) -> bool {
    same_search_content(a, b)
        && a.focused == b.focused
        && a.visible == b.visible
        && a.active_monitor == b.active_monitor
        && a.mode == b.mode
        && a.message == b.message
        && a.automatic_tiling == b.automatic_tiling
        && a.workspaces
            .iter()
            .zip(&b.workspaces)
            .all(|(a, b)| a.monitor == b.monitor)
}

impl Provider for WindowProvider {
    fn name(&self) -> &'static str {
        "windows"
    }
    fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
        let query = query.trim();
        let (needle, only_windows, only_workspaces) = if let Some(rest) = query.strip_prefix("win ")
        {
            (rest, true, false)
        } else if query == "win" {
            ("", true, false)
        } else if let Some(rest) = query.strip_prefix("ws ") {
            (rest, false, true)
        } else if query == "ws" {
            ("", false, true)
        } else {
            (query, false, false)
        };
        let matcher = NameMatcher::new(needle);
        if matcher.is_none() && !only_windows && !only_workspaces {
            return Vec::new();
        }
        let mut results: Vec<_> = self
            .items
            .iter()
            .filter(|(item, _)| {
                (!only_windows || item.kind == ResultKind::Window)
                    && (!only_workspaces || item.kind == ResultKind::Workspace)
            })
            .filter_map(|(item, folded)| {
                let score = matcher
                    .as_ref()
                    .map_or(Some(1u64 << 48), |m| m.score(folded).map(|(_, s)| s))?;
                let mut item = item.clone();
                item.score = score;
                Some(item)
            })
            .collect();
        results.sort_by(|a, b| b.score.cmp(&a.score).then(a.title.cmp(&b.title)));
        results.truncate(limit);
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_a_window_does_not_change_search_content() {
        let mut w = sift_wm::Window::new(1, 2, sift_wm::Rect::new(0., 0., 100., 100.));
        w.title = "Notes".into();
        let a = Snapshot {
            enabled: true,
            windows: vec![w],
            ..Default::default()
        };
        let mut b = a.clone();
        b.windows[0].frame = sift_wm::Rect::new(50., 50., 300., 200.);
        b.revision += 1;
        assert!(same_search_content(&a, &b));
        b.windows[0].title = "Notes — edited".into();
        assert!(!same_search_content(&a, &b));
        let mut c = a.clone();
        c.windows[0].workspace = "2".into();
        assert!(!same_search_content(&a, &c));
        let mut d = a.clone();
        d.enabled = false;
        assert!(!same_search_content(&a, &d));
        assert!(same_overview(&a, &b) == same_search_content(&a, &b));
        let mut e = a.clone();
        e.windows[0].frame = sift_wm::Rect::new(9., 9., 9., 9.);
        assert!(same_overview(&a, &e));
        e.focused = Some(1);
        assert!(!same_overview(&a, &e));
    }
    #[test]
    fn scoped_queries_only_search_cached_window_state() {
        let mut snapshot = Snapshot {
            enabled: true,
            ..Snapshot::default()
        };
        let mut w = sift_wm::Window::new(10, 2, sift_wm::Rect::new(0., 0., 100., 100.));
        w.app_name = "Editor".into();
        w.title = "Sift project".into();
        w.workspace = "code".into();
        snapshot.windows.push(w);
        let provider = WindowProvider::new(Arc::new(snapshot));
        assert_eq!(
            provider.search("win sift", 10)[0].title.as_ref(),
            "Sift project"
        );
        assert_eq!(provider.search("win", 10).len(), 1);
        assert!(provider.search("ws", 10).is_empty());
        assert!(provider.search("", 10).is_empty());
    }
}
