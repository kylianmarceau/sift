//! The launcher view: search field, result list and footer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Image, ImageFormat, SharedString,
    Subscription, Task, Window, actions, div, img, prelude::*, px,
};
use sift_apps::{AppCatalog, AppProvider, Location};
use sift_core::{Action, QueryClock, ResultItem, Search, Selection};
use sift_macos::{LaunchError, Panel};

use crate::input::{Changed, SearchInput};
use crate::theme::Theme;

actions!(launcher, [SelectNext, SelectPrevious, Confirm, Dismiss]);

pub const WIDTH: f32 = 720.;
const HEADER_HEIGHT: f32 = 58.;
const ROW_HEIGHT: f32 = 46.;
const LIST_PADDING: f32 = 6.;
const MESSAGE_HEIGHT: f32 = 56.;
const FOOTER_HEIGHT: f32 = 34.;
const DIVIDER: f32 = 1.;
/// Rows visible at once; more results scroll with the selection.
pub const VISIBLE_ROWS: usize = 8;
/// Results fetched per query.
const RESULT_LIMIT: usize = 40;
const ICON_PX: u32 = 64;
/// Refresh the application list at most this often, when the launcher opens.
const CATALOG_MAX_AGE_SECS: u64 = 60;

/// Total window height for a given body. Must match the layout in
/// `render` exactly, since the native window is sized from it.
pub fn window_height(rows: usize, message: bool) -> f32 {
    let body = if rows > 0 {
        DIVIDER + 2. * LIST_PADDING + rows.min(VISIBLE_ROWS) as f32 * ROW_HEIGHT
    } else if message {
        DIVIDER + MESSAGE_HEIGHT
    } else {
        0.
    };
    HEADER_HEIGHT + body + DIVIDER + FOOTER_HEIGHT
}

/// First visible row after moving the selection to `selected`, scrolling
/// as little as possible.
pub fn scroll_to_show(top: usize, selected: usize, visible: usize) -> usize {
    if selected < top {
        selected
    } else if selected >= top + visible {
        selected + 1 - visible
    } else {
        top
    }
}

pub struct Launcher {
    focus_handle: FocusHandle,
    input: Entity<SearchInput>,
    search: Search,
    app_count: usize,
    catalog_loaded_at: Option<Instant>,
    catalog_task: Option<Task<()>>,
    clock: Arc<QueryClock>,
    query: String,
    results: Vec<ResultItem>,
    selection: Selection,
    scroll_top: usize,
    icons: HashMap<PathBuf, Option<Arc<Image>>>,
    panel: Option<Panel>,
    message: Option<SharedString>,
    theme: Theme,
    /// When the hotkey was pressed, until the resulting frame is drawn.
    show_requested_at: Option<Instant>,
    search_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Launcher {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::for_appearance(window.appearance());
        let input = cx.new(|cx| {
            let mut input = SearchInput::new("Search applications…", cx);
            apply_theme(&mut input, &theme);
            input
        });
        let subscriptions = vec![
            cx.subscribe(&input, |this, _, _: &Changed, cx| this.query_changed(cx)),
            // Clicking another app (the panel resigning key) dismisses
            // sift, like Spotlight.
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() && this.is_visible() {
                    this.hide(cx);
                }
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                this.theme = Theme::for_appearance(window.appearance());
                let theme = this.theme;
                this.input.update(cx, |input, _| apply_theme(input, &theme));
                cx.notify();
            }),
        ];
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            input,
            search: Search::default(),
            app_count: 0,
            catalog_loaded_at: None,
            catalog_task: None,
            clock: Arc::new(QueryClock::new()),
            query: String::new(),
            results: Vec::new(),
            selection: Selection::default(),
            scroll_top: 0,
            icons: HashMap::new(),
            panel: None,
            message: None,
            theme,
            show_requested_at: None,
            search_task: None,
            _subscriptions: subscriptions,
        };
        this.refresh_catalog(cx);
        this
    }

    /// Connects the native window once GPUI has created it.
    pub fn attach_panel(&mut self, panel: Panel) {
        panel.configure();
        panel.set_height(window_height(0, false) as f64);
        self.panel = Some(panel);
    }

    fn is_visible(&self) -> bool {
        self.panel.as_ref().is_some_and(Panel::is_visible)
    }

    /// Shows the launcher, or hides it if it is already showing (pressing
    /// the hotkey twice closes it, as with Spotlight).
    pub fn toggle(&mut self, requested_at: Instant, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_visible() {
            self.hide(cx);
        } else {
            self.show_requested_at = Some(requested_at);
            self.show(window, cx);
        }
    }

    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &self.panel {
            panel.set_height(self.height() as f64);
            panel.show();
        }
        window.focus(&self.input.focus_handle(cx));
        if self.show_requested_at.is_some() {
            cx.on_next_frame(window, |this, _, _| {
                if let Some(at) = this.show_requested_at.take() {
                    tracing::info!(
                        latency_ms = at.elapsed().as_secs_f64() * 1000.,
                        "hotkey to first frame"
                    );
                }
            });
        }
        let stale = self
            .catalog_loaded_at
            .is_none_or(|t| t.elapsed().as_secs() >= CATALOG_MAX_AGE_SECS);
        if stale {
            self.refresh_catalog(cx);
        }
        cx.notify();
    }

    /// Hides the panel and resets the query. The window and all caches are
    /// kept, so the next show is just an order-front.
    pub fn hide(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = &self.panel {
            panel.hide();
        }
        self.message = None;
        self.input.update(cx, |input, cx| input.clear(cx));
    }

    /// Shows a launch failure reported asynchronously by macOS.
    pub fn launch_failed(
        &mut self,
        error: LaunchError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.message = Some(error.to_string().into());
        self.show(window, cx);
        self.resize();
    }

    fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        if self.catalog_task.is_some() {
            return;
        }
        let started = Instant::now();
        let discover = cx.background_spawn(async move {
            let catalog = AppCatalog::discover(&Location::defaults());
            let icons: Vec<(PathBuf, Option<Vec<u8>>)> = catalog
                .apps()
                .iter()
                .map(|app| {
                    (
                        app.path.clone(),
                        sift_macos::app_icon_png(&app.path, ICON_PX),
                    )
                })
                .collect();
            (catalog, icons)
        });
        self.catalog_task = Some(cx.spawn(async move |this, cx| {
            let (catalog, icons) = discover.await;
            this.update(cx, |this, cx| {
                tracing::info!(
                    apps = catalog.len(),
                    ms = started.elapsed().as_secs_f64() * 1000.,
                    "application catalog loaded (with icons)"
                );
                this.app_count = catalog.len();
                this.search = Search::new(vec![Arc::new(AppProvider::new(catalog))]);
                for (path, png) in icons {
                    let image =
                        png.map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)));
                    this.icons.insert(path, image);
                }
                this.catalog_loaded_at = Some(Instant::now());
                this.catalog_task = None;
                // Re-run the current query against the fresh catalog.
                this.run_query(this.query.clone(), cx);
            })
            .ok();
        }));
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).text().to_string();
        self.message = None;
        self.run_query(query, cx);
    }

    /// Runs `query` on the background executor. Results are applied only if
    /// no newer query started meanwhile; replacing `search_task` also drops
    /// (cancels) the previous one.
    fn run_query(&mut self, query: String, cx: &mut Context<Self>) {
        let ticket = self.clock.next();
        let same_query = query == self.query;
        self.query = query.clone();
        let search = self.search.clone();
        let task = cx.background_spawn(async move { search.search(&query, RESULT_LIMIT) });
        self.search_task = Some(cx.spawn(async move |this, cx| {
            let results = task.await;
            this.update(cx, |this, cx| {
                if this.clock.is_current(ticket) {
                    this.set_results(results, same_query, cx);
                }
            })
            .ok();
        }));
    }

    fn set_results(&mut self, results: Vec<ResultItem>, same_query: bool, cx: &mut Context<Self>) {
        if same_query {
            // A refresh of the same query: keep the highlighted item.
            self.selection.reset_keeping(&self.results, &results);
        } else {
            self.selection.reset(results.len(), None);
            self.scroll_top = 0;
        }
        self.results = results;
        if let Some(selected) = self.selection.index() {
            self.scroll_top = scroll_to_show(self.scroll_top, selected, VISIBLE_ROWS);
        }
        self.resize();
        cx.notify();
    }

    fn height(&self) -> f32 {
        window_height(self.results.len(), self.shows_message())
    }

    fn shows_message(&self) -> bool {
        self.message.is_some() || (self.results.is_empty() && !self.query.trim().is_empty())
    }

    fn resize(&self) {
        if let Some(panel) = &self.panel {
            panel.set_height(self.height() as f64);
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.selection.next();
        self.follow_selection(cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.selection.previous();
        self.follow_selection(cx);
    }

    fn follow_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(selected) = self.selection.index() {
            self.scroll_top = scroll_to_show(self.scroll_top, selected, VISIBLE_ROWS);
        }
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_selected(window, cx);
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        self.hide(cx);
    }

    fn activate_selected(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.selection.index().and_then(|i| self.results.get(i)) else {
            return;
        };
        match &item.action {
            Action::OpenApplication(path) => {
                let started = Instant::now();
                let failures = crate::launch_failures();
                match sift_macos::open_application(path, move |error| {
                    let _ = failures.unbounded_send(error);
                }) {
                    Ok(()) => {
                        tracing::info!(
                            app = %path.display(),
                            ms = started.elapsed().as_secs_f64() * 1000.,
                            "launch requested"
                        );
                        self.hide(cx);
                    }
                    Err(error) => {
                        self.message = Some(error.to_string().into());
                        self.resize();
                        cx.notify();
                    }
                }
            }
            Action::OpenPath(_) => {
                // File results arrive with the file provider in Phase 2.
            }
        }
    }

    fn icon(&mut self, path: &Path) -> Option<Arc<Image>> {
        if let Some(icon) = self.icons.get(path) {
            return icon.clone();
        }
        // Not prefetched (catalog still loading): render now, once.
        let image = sift_macos::app_icon_png(path, ICON_PX)
            .map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)));
        self.icons.insert(path.to_path_buf(), image.clone());
        image
    }

    fn render_row(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let item = &self.results[index];
        let selected = self.selection.index() == Some(index);
        let title = SharedString::from(item.title.to_string());
        let subtitle = item
            .subtitle
            .as_ref()
            .map(|s| SharedString::from(s.to_string()));
        let kind = item.kind.label();
        let icon_path = match &item.action {
            Action::OpenApplication(p) | Action::OpenPath(p) => p.clone(),
        };
        let icon = self.icon(&icon_path);

        div()
            .id(("result", index))
            .h(px(ROW_HEIGHT))
            .mx(px(LIST_PADDING))
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .rounded(px(8.))
            .when(selected, |row| row.bg(theme.selected))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selection.select(index);
                this.activate_selected(window, cx);
            }))
            .child(match icon {
                Some(image) => img(image).size(px(30.)).into_any_element(),
                None => div().size(px(30.)).into_any_element(),
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_grow()
                    .overflow_hidden()
                    .child(div().text_size(px(14.)).text_color(theme.text).child(title))
                    .when_some(subtitle, |col, subtitle| {
                        col.child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.muted)
                                .truncate()
                                .child(subtitle),
                        )
                    }),
            )
            .child(div().text_size(px(12.)).text_color(theme.muted).child(kind))
            .into_any_element()
    }

    fn render_footer(&self) -> AnyElement {
        let theme = self.theme;
        let left: SharedString = if self.app_count == 0 {
            "Loading applications…".into()
        } else if self.results.is_empty() {
            format!("{} applications", self.app_count).into()
        } else {
            format!(
                "{} of {} results",
                self.selection.index().map_or(0, |i| i + 1),
                self.results.len()
            )
            .into()
        };
        let key = |label: &'static str| {
            div()
                .px(px(5.))
                .rounded(px(4.))
                .bg(theme.key_background)
                .text_color(theme.text)
                .child(label)
        };
        let hint = |keys: &'static str, label: &'static str| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(key(keys))
                .child(div().text_color(theme.muted).child(label))
        };
        div()
            .h(px(FOOTER_HEIGHT))
            .px_4()
            .flex()
            .items_center()
            .justify_between()
            .text_size(px(11.5))
            .child(div().text_color(theme.muted).child(left))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(hint("↑↓", "Navigate"))
                    .child(hint("↵", "Open"))
                    .child(hint("esc", "Close")),
            )
            .into_any_element()
    }
}

fn apply_theme(input: &mut SearchInput, theme: &Theme) {
    input.text_color = theme.text;
    input.placeholder_color = theme.placeholder;
    input.cursor_color = theme.accent;
    input.selection_color = theme.accent.opacity(0.3);
}

impl Focusable for Launcher {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Launcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let divider = || div().h(px(DIVIDER)).w_full().bg(theme.divider);
        let end = (self.scroll_top + VISIBLE_ROWS).min(self.results.len());
        let rows: Vec<AnyElement> = (self.scroll_top..end)
            .map(|i| self.render_row(i, cx))
            .collect();
        let message: Option<SharedString> = self.message.clone().or_else(|| {
            self.shows_message()
                .then(|| format!("No applications match “{}”", self.query.trim()).into())
        });
        let message_color = if self.message.is_some() {
            theme.error
        } else {
            theme.muted
        };

        div()
            .key_context("Launcher")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.text)
            .child(
                div()
                    .h(px(HEADER_HEIGHT))
                    .px_5()
                    .flex()
                    .items_center()
                    .text_size(px(20.))
                    .line_height(px(26.))
                    .child(self.input.clone()),
            )
            .when(!rows.is_empty(), |root| {
                root.child(divider())
                    .child(div().py(px(LIST_PADDING)).flex().flex_col().children(rows))
            })
            .when(self.results.is_empty(), |root| {
                root.when_some(message, |root, message| {
                    root.child(divider()).child(
                        div()
                            .h(px(MESSAGE_HEIGHT))
                            .px_5()
                            .flex()
                            .items_center()
                            .text_size(px(13.))
                            .text_color(message_color)
                            .child(message),
                    )
                })
            })
            .child(divider())
            .child(self.render_footer())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn height_grows_with_rows_up_to_the_visible_limit() {
        let compact = window_height(0, false);
        assert_eq!(compact, HEADER_HEIGHT + DIVIDER + FOOTER_HEIGHT);
        assert!(window_height(0, true) > compact);
        let one = window_height(1, false);
        let eight = window_height(8, false);
        assert_eq!(eight - one, 7. * ROW_HEIGHT);
        assert_eq!(window_height(40, false), eight);
        // Results take precedence over a message.
        assert_eq!(window_height(3, true), window_height(3, false));
    }

    #[test]
    fn scrolling_keeps_the_selection_visible() {
        assert_eq!(scroll_to_show(0, 3, 8), 0);
        assert_eq!(scroll_to_show(0, 8, 8), 1);
        assert_eq!(scroll_to_show(5, 12, 8), 5);
        assert_eq!(scroll_to_show(5, 13, 8), 6);
        assert_eq!(scroll_to_show(5, 2, 8), 2);
    }
}
