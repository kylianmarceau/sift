//! The launcher view: search field, result list and footer.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use futures::channel::mpsc::unbounded;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Image, ImageFormat, SharedString,
    Subscription, Task, Window, actions, div, img, prelude::*, px,
};
use sift_apps::{AppCatalog, AppProvider, Location};
use sift_calc::CalculatorProvider;
use sift_core::{Action, Provider, QueryClock, ResultItem, ResultKind, Search, Selection};
use sift_files::{FileIndex, FileProvider, IndexService, ServiceConfig, Status};
use sift_macos::{LaunchError, Panel};

use crate::commands::{self, CommandProvider};
use crate::icons::{IconKey, icon_for};
use crate::input::{Changed, SearchInput};
use crate::theme::Theme;

actions!(
    launcher,
    [SelectNext, SelectPrevious, Confirm, RevealSelected, Dismiss]
);

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
    apps: Option<Arc<AppProvider>>,
    files: Arc<FileProvider>,
    file_index: Arc<FileIndex>,
    /// Keeps the file index current; `None` if it could not start.
    service: Option<Arc<IndexService>>,
    index_status: Status,
    app_count: usize,
    catalog_loaded_at: Option<Instant>,
    catalog_task: Option<Task<()>>,
    clock: Arc<QueryClock>,
    query: String,
    results: Vec<ResultItem>,
    selection: Selection,
    scroll_top: usize,
    icons: HashMap<IconKey, Option<Arc<Image>>>,
    /// Icons being rendered in the background.
    icon_jobs: HashSet<IconKey>,
    panel: Option<Panel>,
    message: Option<SharedString>,
    theme: Theme,
    /// When the hotkey was pressed, until the resulting frame is rendered.
    show_requested_at: Option<Instant>,
    search_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Launcher {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::for_appearance(window.appearance());
        let input = cx.new(|cx| {
            let mut input = SearchInput::new("Search apps and files…", cx);
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
            // GPUI's layout size must follow every native resize; if it
            // doesn't, part of the window is never drawn.
            cx.observe_window_bounds(window, |_, window, _| {
                tracing::debug!(height = f32::from(window.viewport_size().height), bounds = ?window.bounds(), scale = window.scale_factor(), "viewport");
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                this.theme = Theme::for_appearance(window.appearance());
                let theme = this.theme;
                this.input.update(cx, |input, _| apply_theme(input, &theme));
                cx.notify();
            }),
        ];
        // SIFT_INDEX_DIR overrides the location (development and tests).
        let index_dir = std::env::var_os("SIFT_INDEX_DIR")
            .map(PathBuf::from)
            .or_else(sift_search::default_index_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let file_index = Arc::new(FileIndex::open(&index_dir));
        let files = Arc::new(FileProvider::new(file_index.clone()));
        let (status_tx, mut status_rx) = unbounded::<Status>();
        let service = IndexService::start(ServiceConfig::new(&index_dir), file_index.clone(), {
            move |status: &Status| {
                let _ = status_tx.unbounded_send(status.clone());
            }
        })
        .map_err(|error| tracing::error!(%error, "cannot start the file index service"))
        .ok()
        .map(Arc::new);
        cx.spawn(async move |this, cx| {
            while let Some(status) = status_rx.next().await {
                if this
                    .update(cx, |this, cx| this.set_index_status(status, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            input,
            search: Search::default(),
            apps: None,
            files,
            file_index,
            service,
            index_status: Status::default(),
            app_count: 0,
            catalog_loaded_at: None,
            catalog_task: None,
            clock: Arc::new(QueryClock::new()),
            query: String::new(),
            results: Vec::new(),
            selection: Selection::default(),
            scroll_top: 0,
            icons: HashMap::new(),
            icon_jobs: HashSet::new(),
            panel: None,
            message: None,
            theme,
            show_requested_at: None,
            search_task: None,
            _subscriptions: subscriptions,
        };
        this.rebuild_search();
        this.refresh_catalog(cx);
        this
    }

    /// Providers in priority order: within a match tier, apps first, then
    /// commands, then files.
    fn rebuild_search(&mut self) {
        let mut providers: Vec<Arc<dyn Provider>> = vec![Arc::new(CalculatorProvider)];
        if let Some(apps) = &self.apps {
            providers.push(apps.clone());
        }
        providers.push(Arc::new(CommandProvider));
        providers.push(self.files.clone());
        self.search = Search::new(providers);
    }

    pub fn index_status(&self) -> &Status {
        &self.index_status
    }

    pub fn index_service(&self) -> Option<Arc<IndexService>> {
        self.service.clone()
    }

    fn set_index_status(&mut self, status: Status, cx: &mut Context<Self>) {
        let changed = status.last_update.map(|u| u.at)
            != self.index_status.last_update.map(|u| u.at)
            || status.roots.len() != self.index_status.roots.len();
        self.index_status = status;
        // New files may match the current query.
        if changed && !self.query.trim().is_empty() {
            self.run_query(self.query.clone(), cx);
        }
        cx.emit(IndexStatusChanged);
        cx.notify();
    }

    /// Connects the native window once GPUI has created it.
    pub fn attach_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        panel.configure();
        self.panel = Some(panel);
        self.resize(cx);
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
        self.resize(cx);
        if let Some(panel) = self.panel.clone() {
            panel.show();
            // Draw the first frame as soon as this update returns, rather
            // than when GPUI's display link restarts (see `Panel::draw_now`).
            cx.spawn(async move |_, _| panel.draw_now()).detach();
        }
        window.focus(&self.input.focus_handle(cx));
        if let Some(service) = &self.service {
            service.send(sift_files::Command::Refresh);
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
                .map(|app| (app.path.clone(), sift_macos::icon_png(&app.path, ICON_PX)))
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
                this.apps = Some(Arc::new(AppProvider::new(catalog)));
                this.rebuild_search();
                for (path, png) in icons {
                    this.icons.insert(IconKey::Path(path), png.map(png_image));
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
        self.resize(cx);
        cx.notify();
    }

    fn height(&self) -> f32 {
        window_height(self.results.len(), self.shows_message())
    }

    fn shows_message(&self) -> bool {
        self.message.is_some() || (self.results.is_empty() && !self.query.trim().is_empty())
    }

    /// Fits the window to the current content.
    ///
    /// The native resize runs in a separate foreground task, after the
    /// current GPUI update has returned. Resizing in place would lose
    /// GPUI's resize notification (see `Panel::set_height`), and GPUI would
    /// keep drawing at the old size, leaving the rest of the window black.
    /// GPUI's own `Window::resize` defers the same way, but it grows the
    /// window upwards.
    fn resize(&self, cx: &mut Context<Self>) {
        let Some(panel) = self.panel.clone() else {
            return;
        };
        let height = self.height() as f64;
        cx.spawn(async move |_, _| panel.set_height(height))
            .detach();
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

    fn reveal(&mut self, _: &RevealSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self
            .selected()
            .and_then(|item| item.action.path())
            .map(|p| p.to_path_buf())
        else {
            return;
        };
        match sift_macos::reveal_in_finder(&path) {
            Ok(()) => self.hide(cx),
            Err(error) => self.show_error(error, cx),
        }
    }

    fn selected(&self) -> Option<&ResultItem> {
        self.selection.index().and_then(|i| self.results.get(i))
    }

    fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.selected().map(|item| item.action.clone()) else {
            return;
        };
        let started = Instant::now();
        let failures = crate::launch_failures();
        let report = move |error| {
            let _ = failures.unbounded_send(error);
        };
        let result = match &action {
            Action::OpenApplication(path) => sift_macos::open_application(path, report),
            Action::OpenPath(path) => sift_macos::open_path(path, report),
            Action::OpenUrl(url) => sift_macos::open_url(url, report),
            Action::CopyText(text) => sift_macos::copy_text(text),
            Action::Shell(command) => crate::shell::spawn(command),
            Action::Run(command) => {
                self.run_command(command, window, cx);
                return;
            }
        };
        match result {
            Ok(()) => {
                tracing::info!(
                    path = %action.path().map(|p| p.display().to_string()).unwrap_or_default(),
                    ms = started.elapsed().as_secs_f64() * 1000.,
                    "open requested"
                );
                self.hide(cx);
            }
            Err(error) => self.show_error(error, cx),
        }
    }

    fn run_command(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            commands::SETTINGS => {
                self.hide(cx);
                crate::settings::open(cx.entity(), window, cx);
            }
            commands::QUIT => cx.quit(),
            other => tracing::warn!(command = other, "unknown command"),
        }
    }

    fn show_error(&mut self, error: LaunchError, cx: &mut Context<Self>) {
        self.message = Some(error.to_string().into());
        self.resize(cx);
        cx.notify();
    }

    /// The icon for `item`, if rendered. Missing icons are rendered on the
    /// background executor and the view refreshes when they arrive.
    fn icon(&mut self, item: &ResultItem, cx: &mut Context<Self>) -> Option<Arc<Image>> {
        let (key, path) = icon_for(item)?;
        if let Some(icon) = self.icons.get(&key) {
            return icon.clone();
        }
        if self.icon_jobs.insert(key.clone()) {
            let render = cx.background_spawn(async move { sift_macos::icon_png(&path, ICON_PX) });
            cx.spawn(async move |this, cx| {
                let png = render.await;
                this.update(cx, |this, cx| {
                    this.icon_jobs.remove(&key);
                    this.icons.insert(key, png.map(png_image));
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        None
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
        let glyph = glyph(item.kind);
        let item = item.clone();
        let icon = self.icon(&item, cx);

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
                None if glyph.is_some() => div()
                    .size(px(30.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.))
                    .bg(theme.key_background)
                    .text_size(px(15.))
                    .children(glyph)
                    .into_any_element(),
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
        let indexing = self
            .index_status
            .roots
            .iter()
            .any(|r| matches!(r.state, sift_files::RootState::Indexing { .. }));
        let left: SharedString = if self.app_count == 0 {
            "Loading applications…".into()
        } else if indexing {
            "Indexing files…".into()
        } else if self.results.is_empty() {
            format!(
                "{} apps · {} files and folders",
                self.app_count,
                thousands(self.file_index.len())
            )
            .into()
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
        let selected = self.selected();
        let can_reveal = selected.is_some_and(|i| i.action.path().is_some());
        let open_label = match selected.map(|i| &i.action) {
            Some(Action::Run(_) | Action::Shell(_)) => "Run",
            Some(Action::CopyText(_)) => "Copy",
            _ => "Open",
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
                    .child(hint("↵", open_label))
                    .when(can_reveal, |row| row.child(hint("⌘↵", "Show in Finder")))
                    .child(hint("esc", "Close")),
            )
            .into_any_element()
    }
}

/// Emitted when the file index status changes (the settings window
/// listens).
pub struct IndexStatusChanged;

/// The symbol drawn in place of an icon for results that have no file.
fn glyph(kind: ResultKind) -> Option<&'static str> {
    match kind {
        ResultKind::Command => Some("⌘"),
        ResultKind::Calculator => Some("="),
        ResultKind::Snippet => Some("¶"),
        ResultKind::Clipboard => Some("”"),
        ResultKind::Application | ResultKind::File | ResultKind::Folder => None,
    }
}

impl gpui::EventEmitter<IndexStatusChanged> for Launcher {}

fn png_image(bytes: Vec<u8>) -> Arc<Image> {
    Arc::new(Image::from_bytes(ImageFormat::Png, bytes))
}

fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
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
        // Measured here, inside the first frame after a hotkey press, rather
        // than in an `on_next_frame` callback, which GPUI runs before it
        // starts drawing. Layout, paint and GPU presentation still follow.
        if let Some(at) = self.show_requested_at.take() {
            tracing::info!(
                latency_ms = at.elapsed().as_secs_f64() * 1000.,
                "hotkey to first frame"
            );
        }
        let theme = self.theme;
        let divider = || div().h(px(DIVIDER)).w_full().bg(theme.divider);
        let end = (self.scroll_top + VISIBLE_ROWS).min(self.results.len());
        let rows: Vec<AnyElement> = (self.scroll_top..end)
            .map(|i| self.render_row(i, cx))
            .collect();
        let message: Option<SharedString> = self.message.clone().or_else(|| {
            self.shows_message()
                .then(|| format!("No results for “{}”", self.query.trim()).into())
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
            .on_action(cx.listener(Self::reveal))
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
    fn formats_counts_with_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(54_206), "54,206");
        assert_eq!(thousands(1_000_000), "1,000,000");
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
