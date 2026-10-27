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
