//! The launcher view: search field, result list and footer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::unbounded;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, FontWeight, Hsla, Image, ImageFormat,
    RenderImage, SharedString, Subscription, Task, Window, actions, div, img, prelude::*, px,
    relative, svg,
};
use sift_apps::{AppCatalog, AppProvider, Location};
use sift_calc::CalculatorProvider;
use sift_core::{Action, Provider, QueryClock, ResultItem, ResultKind, Search, Selection};
use sift_files::{FileIndex, FileProvider, FileQuery, IndexService, ServiceConfig, Status};
use sift_macos::a11y::{Urgency, announce};
use sift_macos::{LaunchError, Panel, QuickLook};

use crate::clipboard;
use crate::commands::{self, CommandProvider};
use crate::icons::{IconCache, IconKey, icon_for};
use crate::input::{Changed, SearchInput};
use crate::speech;
use crate::theme::{Theme, tile_color};
use crate::updates::{self, UpdateState};
use crate::user::{Reload, UserData, expand_placeholders};

actions!(
    launcher,
    [
        SelectNext,
        SelectPrevious,
        Confirm,
        RevealSelected,
        QuickLookSelected,
        ToggleFavorite,
        ForgetSelected,
        Dismiss
    ]
);

pub const WIDTH: f32 = 720.;
const HEADER_HEIGHT: f32 = 56.;
const ROW_HEIGHT: f32 = 42.;
const LIST_PADDING: f32 = 6.;
/// The label above the results ("Results", "Favorites and Recents").
const SECTION_HEIGHT: f32 = 26.;
const MESSAGE_HEIGHT: f32 = 56.;
const FOOTER_HEIGHT: f32 = 38.;
const DIVIDER: f32 = 1.;
/// Left edge shared by the search icon, section labels and result icons,
/// so icons sit in one column and text in another.
const INSET: f32 = 18.;
/// Highlighted rows are inset from the window edge by this much.
const ROW_MARGIN: f32 = 8.;
const ICON_SIZE: f32 = 28.;
/// Space between an icon and its text.
const ICON_GAP: f32 = 12.;
/// Rows visible at once; more results scroll with the selection.
pub const VISIBLE_ROWS: usize = 8;
/// Results fetched per query.
const RESULT_LIMIT: usize = 40;
const ICON_PX: u32 = 64;
/// How often the clipboard's change counter is read while clipboard
/// history is enabled.
const CLIPBOARD_POLL: Duration = Duration::from_millis(750);
/// How long to wait, after losing focus while a preview is open, before
/// deciding whether the user left the launcher.
const QUICK_LOOK_SETTLE: Duration = Duration::from_millis(150);
/// VoiceOver hears the result summary once typing pauses this long, so it
/// does not talk over every keystroke.
const SPEECH_PAUSE: Duration = Duration::from_millis(400);
/// Refresh the application list at most this often, when the launcher opens.
const CATALOG_MAX_AGE_SECS: u64 = 60;

/// Total window height for a given body. Must match the layout in
/// `render` exactly, since the native window is sized from it.
pub fn window_height(rows: usize, message: bool) -> f32 {
    let body = if rows > 0 {
        DIVIDER + 2. * LIST_PADDING + SECTION_HEIGHT + rows.min(VISIBLE_ROWS) as f32 * ROW_HEIGHT
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
    /// Files only, for queries with filters (`ext:pdf`, `in:projects`).
    file_search: Search,
    /// Clipboard history only, for `clip …` queries.
    clip_search: Search,
    window_search: Search,
    window_manager: Option<sift_macos::wm::Runtime>,
    window_snapshot: Arc<sift_wm::Snapshot>,
    window_config: sift_wm::Config,
    window_error: Option<String>,
    /// Watches the clipboard while clipboard history is enabled.
    clipboard_task: Option<(sift_macos::ClipboardWatcher, Task<()>)>,
    apps: Option<Arc<AppProvider>>,
    files: Arc<FileProvider>,
    file_index: Arc<FileIndex>,
    /// Keeps the file index current; `None` if it could not start.
    service: Option<Arc<IndexService>>,
    index_status: Status,
    /// Config file, recents and favorites.
    user: UserData,
    app_count: usize,
    catalog_loaded_at: Option<Instant>,
    catalog_task: Option<Task<()>>,
    clock: Arc<QueryClock>,
    query: String,
    results: Vec<ResultItem>,
    selection: Selection,
    scroll_top: usize,
    icons: IconCache<Arc<RenderImage>>,
    /// Icons being rendered in the background.
    icon_jobs: HashSet<IconKey>,
    panel: Option<Panel>,
    /// Previews of the selected file (⌘Y). All calls that show, move or
    /// close the preview run outside GPUI updates, since they change which
    /// window has focus.
    quick_look: Option<Rc<QuickLook>>,
    message: Option<SharedString>,
    /// The message is information (an update check), not a problem.
    message_info: bool,
    theme: Theme,
    /// What is known about a newer release (see `updates.rs`).
    update: UpdateState,
    last_update_check: updates::LastCheck,
    /// When an automatic check last failed; retried an hour later.
    update_failed_at: Option<Instant>,
    update_task: Option<Task<()>>,
    /// VoiceOver was running when the panel was last shown.
    voice_over: bool,
    /// The last summary announced to VoiceOver (see `announce_changes`).
    spoken: Option<String>,
    speech_task: Option<Task<()>>,
    /// When the hotkey was pressed, until the resulting frame is rendered.
    show_requested_at: Option<Instant>,
    /// Timings for the built-in UI benchmark (`SIFT_UI_BENCH`).
    bench: Option<crate::uibench::Recorder>,
    search_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Where `state.db`, the session marker and (unless it is in the XDG
/// config directory, see [`config_file`]) `config.toml` live:
/// `~/Library/Application Support/sift`, or `SIFT_CONFIG_DIR`.
pub fn user_dir() -> PathBuf {
    std::env::var_os("SIFT_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(sift_state::support_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `config.toml`: in `SIFT_CONFIG_DIR` when that is set, otherwise in the
/// XDG config directory or [`user_dir`] (see [`sift_state::config_file`]).
pub fn config_file() -> PathBuf {
    match std::env::var_os("SIFT_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir).join("config.toml"),
        None => sift_state::config_file(&user_dir()),
    }
}

/// What `main` knows at startup.
pub struct Startup {
    /// The previous run crashed or was killed.
    pub unclean_exit: bool,
}

impl Launcher {
    pub fn new(startup: Startup, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::for_appearance(window.appearance());
        let input = cx.new(|cx| {
            let mut input = SearchInput::new("Search apps, files and commands…", cx);
            apply_theme(&mut input, &theme);
            input
        });
        let subscriptions = vec![
            cx.subscribe(&input, |this, _, _: &Changed, cx| this.query_changed(cx)),
            // Clicking another app (the panel resigning key) dismisses
            // sift, like Spotlight.
            cx.observe_window_activation(window, |this, window, cx| {
                // The UI benchmark keeps the panel open: another app taking
                // focus mid-run would otherwise end the measurement.
                if window.is_window_active() || !this.is_visible() || this.bench.is_some() {
                    return;
                }
                if this.quick_look.as_ref().is_some_and(|q| q.is_visible()) {
                    // The preview panel takes focus for a moment when it
                    // opens (and hands it back); decide once that settles.
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(QUICK_LOOK_SETTLE).await;
                        this.update(cx, |this, cx| {
                            if this.is_visible() && !this.has_focus() {
                                this.hide(cx);
                            }
                        })
                        .ok();
                    })
                    .detach();
                } else {
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
        // A newer release seen by an earlier check is shown without asking
        // the network again.
        let last_update_check = updates::load(&user_dir());
        let update = match &last_update_check.latest {
            Some(latest) if updates::is_newer(latest, updates::CURRENT) => {
                UpdateState::Available(latest.clone())
            }
            _ => UpdateState::Unknown,
        };
        let files = Arc::new(FileProvider::new(file_index.clone()));
        let (status_tx, mut status_rx) = unbounded::<Status>();
        // After a crash, check the index page by page before using it.
        let service_config = ServiceConfig {
            verify: startup.unclean_exit,
            ..ServiceConfig::new(&index_dir)
        };
        let service = IndexService::start(service_config, file_index.clone(), {
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

        let mut user = UserData::open(&user_dir(), config_file());
        let mut notices: Vec<String> = Vec::new();
        if startup.unclean_exit {
            let log = crate::logging::log_path()
                .map(|p| sift_files::abbreviate_home(&p, std::env::home_dir().as_deref()))
                .unwrap_or_else(|| "the log".into());
            notices.push(format!(
                "Sift quit unexpectedly last time. Details are in {log}."
            ));
        }
        notices.extend(user.take_notice());

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            input,
            search: Search::default(),
            file_search: Search::default(),
            clip_search: Search::default(),
            window_search: Search::default(),
            window_manager: None,
            window_snapshot: Arc::default(),
            window_config: sift_wm::Config::default(),
            window_error: None,
            clipboard_task: None,
            apps: None,
            files,
            file_index,
            service,
            index_status: Status::default(),
            user,
            app_count: 0,
            catalog_loaded_at: None,
            catalog_task: None,
            clock: Arc::new(QueryClock::new()),
            query: String::new(),
            results: Vec::new(),
            selection: Selection::default(),
            scroll_top: 0,
            icons: IconCache::new(IconCache::<Arc<RenderImage>>::LIMIT),
            icon_jobs: HashSet::new(),
            panel: None,
            quick_look: None,
            message: None,
            message_info: false,
            update,
            last_update_check,
            update_failed_at: None,
            update_task: None,
            theme,
            voice_over: false,
            spoken: None,
            speech_task: None,
            show_requested_at: None,
            bench: None,
            search_task: None,
            _subscriptions: subscriptions,
        };
        if !notices.is_empty() {
            this.message_info = false;
            this.message = Some(notices.join(" ").into());
        }
        this.rebuild_search();
        this.configure_windows(cx);
        this.watch_clipboard(cx);
        this.refresh_catalog(cx);
        this.run_query(String::new(), cx);
        this
    }

    /// Providers in priority order: within a match tier, apps first, then
    /// built-in and custom commands, snippets, then files. Usage boosts
    /// (favorites, frequent and recent use) order results within a tier.
    fn rebuild_search(&mut self) {
        let windows: Option<Arc<dyn Provider>> = self.window_manager.as_ref().map(|_| {
            Arc::new(crate::window_manager::WindowProvider::new(
                self.window_snapshot.clone(),
            )) as Arc<dyn Provider>
        });
        let mut providers: Vec<Arc<dyn Provider>> = vec![Arc::new(CalculatorProvider)];
        if let Some(apps) = &self.apps {
            providers.push(apps.clone());
        }
        if let Some(windows) = &windows {
            providers.push(windows.clone());
        }
        providers.push(Arc::new(CommandProvider));
        providers.push(self.user.custom_commands());
        providers.push(self.user.snippets());
        providers.push(self.files.clone());
        self.search = Search::new(providers).with_boosts(self.user.boosts());
        self.file_search = Search::new(vec![self.files.clone() as Arc<dyn Provider>])
            .with_boosts(self.user.boosts());
        self.clip_search = Search::new(vec![self.user.clipboard() as Arc<dyn Provider>]);
        self.window_search = if let Some(windows) = windows {
            Search::new(vec![windows])
        } else {
            Search::default()
        };
    }

    /// Whether applications and the file index have loaded (UI benchmark).
    pub fn bench_ready(&self) -> bool {
        self.app_count > 0 && !self.file_index.is_empty()
    }

    pub fn start_bench(&mut self) {
        self.bench = Some(crate::uibench::Recorder::default());
    }

    pub fn bench(&self) -> Option<&crate::uibench::Recorder> {
        self.bench.as_ref()
    }

    /// Types `text` into the search field, as the UI benchmark's keystroke.
    pub fn bench_type(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(bench) = &mut self.bench {
            bench.typed(text);
        }
        self.input
            .update(cx, |input, cx| input.set_text(text.to_owned(), cx));
    }

    /// Picks up edits to config.toml (one `stat` when nothing changed).
    pub fn reload_config(&mut self, cx: &mut Context<Self>) {
        if let Reload::Reloaded(problems) = self.user.reload_config() {
            self.configure_windows(cx);
            self.rebuild_search();
            self.watch_clipboard(cx);
            cx.emit(UserDataChanged);
            if let Some(first) = problems.first() {
                let more = match problems.len() {
                    1 => String::new(),
                    n => format!(" (and {} more)", n - 1),
                };
                self.message_info = false;
                self.message = Some(format!("{first}{more}").into());
            }
            self.run_query(self.query.clone(), cx);
        }
    }

    pub fn index_status(&self) -> &Status {
        &self.index_status
    }

    pub fn index_service(&self) -> Option<Arc<IndexService>> {
        self.service.clone()
    }

    fn set_index_status(&mut self, status: Status, cx: &mut Context<Self>) {
        // A notice from the index service that the user did not ask for
        // (a damaged index rebuilt, an internal error recovered): show it
        // in the panel, unless the settings window is where it came from.
        if status.message.is_some()
            && status.message != self.index_status.message
            && !crate::settings::is_open(cx)
        {
            self.message_info = false;
            self.message = status.message.clone().map(Into::into);
            self.resize(cx);
        }
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
        self.quick_look = Some(Rc::new(panel.quick_look()));
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

    /// Starts or stops watching the clipboard to match the config. While
    /// on, a background thread reads the change counter every
    /// `CLIPBOARD_POLL`; the UI wakes and reads the contents only after a
    /// change. What was on the clipboard before
    /// watching started is not recorded.
    fn watch_clipboard(&mut self, cx: &mut Context<Self>) {
        if !self.user.clipboard_enabled() {
            if self.clipboard_task.take().is_some() {
                tracing::info!("clipboard history off");
            }
            return;
        }
        if self.clipboard_task.is_some() {
            return;
        }
        let (tx, mut rx) = unbounded::<()>();
        let watcher = match sift_macos::ClipboardWatcher::start(CLIPBOARD_POLL, move || {
            let _ = tx.unbounded_send(());
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::error!(%error, "cannot watch the clipboard");
                return;
            }
        };
        tracing::info!("clipboard history on");
        // Woken only when something was copied; reads the text on the main
        // thread.
        let task = cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                let text = sift_macos::clipboard_text();
                if this
                    .update(cx, |this, cx| this.clipboard_changed(text, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.clipboard_task = Some((watcher, task));
    }

    fn clipboard_changed(&mut self, text: sift_macos::ClipboardText, cx: &mut Context<Self>) {
        match text {
            sift_macos::ClipboardText::Text(text) => {
                if self.user.record_clipboard(&text) {
                    self.rebuild_search();
                    cx.emit(UserDataChanged);
                    if clipboard::scope(&self.query).is_some() {
                        self.run_query(self.query.clone(), cx);
                    }
                }
            }
            sift_macos::ClipboardText::Private => {
                tracing::debug!("clipboard content marked private; not kept");
            }
            sift_macos::ClipboardText::None => {}
        }
    }

    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_config(cx);
        if self.window_config.enabled && self.window_manager.is_none() {
            self.configure_windows(cx);
        }
        if let Some(runtime) = &self.window_manager {
            runtime.refresh();
        }
        self.resize(cx);
        if let Some(panel) = self.panel.clone() {
            panel.show();
            // Draw the first frame as soon as this update returns, rather
            // than when GPUI's display link restarts (see `Panel::draw_now`).
            cx.spawn(async move |_, _| panel.draw_now()).detach();
        }
        window.focus(&self.input.focus_handle(cx));
        // Accessibility settings can change while the panel is hidden.
        // SIFT_ANNOUNCE=1 announces as if VoiceOver were on (for testing
        // with Accessibility Inspector, or the UI bench).
        self.voice_over = sift_macos::a11y::voice_over_enabled()
            || std::env::var_os("SIFT_ANNOUNCE").is_some_and(|v| v == "1");
        self.spoken = None;
        let theme = Theme::for_appearance(window.appearance());
        if theme != self.theme {
            self.theme = theme;
            self.input.update(cx, |input, _| apply_theme(input, &theme));
        }
        if let Some(service) = &self.service {
            service.send(sift_files::Command::Refresh);
        }
        self.check_for_updates(false, cx);
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
        self.close_quick_look(cx);
        if let Some(panel) = &self.panel {
            panel.hide();
        }
        self.message = None;
        self.speech_task = None;
        self.input.update(cx, |input, cx| input.clear(cx));
    }

    /// Shows a launch failure reported asynchronously by macOS.
    pub fn launch_failed(
        &mut self,
        error: LaunchError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.message_info = false;
        self.message = Some(error.to_string().into());
        self.show(window, cx);
    }

    fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        if self.catalog_task.is_some() {
            return;
        }
        let started = Instant::now();
        let decode = icon_decoder(cx);
        let discover = cx.background_spawn(async move {
            let catalog = AppCatalog::discover(&Location::defaults());
            let icons: Vec<(PathBuf, Option<Arc<RenderImage>>)> = catalog
                .apps()
                .iter()
                .map(|app| (app.path.clone(), render_icon(&app.path, &decode)))
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
                let mut dropped = this.icons.release_kept();
                for (path, icon) in icons {
                    dropped.extend(this.icons.insert(IconKey::Path(path), icon, true));
                }
                free_textures(dropped, cx);
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
        if query.trim().is_empty() {
            // Favorites and recents: a few rows from SQLite, read in place.
            self.search_task = None;
            let home = self.user.home();
            self.set_results(home, same_query, cx);
            return;
        }
        let search = if query == "win"
            || query.starts_with("win ")
            || query == "ws"
            || query.starts_with("ws ")
        {
            self.window_search.clone()
        } else if self.in_clipboard_scope(&query) {
            self.clip_search.clone()
        } else if FileQuery::parse(&query) == Ok(None) {
            self.search.clone()
        } else {
            self.file_search.clone()
        };
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
        if let Some(bench) = &mut self.bench {
            bench.results(&self.query, self.results.len());
        }
        if let Some(selected) = self.selection.index() {
            self.scroll_top = scroll_to_show(self.scroll_top, selected, VISIBLE_ROWS);
        }
        self.follow_quick_look(cx);
        self.resize(cx);
        cx.notify();
        self.draw_soon(cx);
    }

    /// Draws the next frame as soon as the current update returns, instead
    /// of at the next display-link tick (up to one refresh interval later,
    /// 16.7 ms at 60 Hz). Used when new results arrive, the moment that
    /// decides how fast typing feels.
    fn draw_soon(&self, cx: &mut Context<Self>) {
        if let Some(panel) = self.panel.clone()
            && panel.is_visible()
        {
            cx.spawn(async move |_, _| panel.draw_now()).detach();
        }
    }

    /// What VoiceOver should know about the panel: the message shown, or
    /// the result count and the highlighted result.
    fn summary(&self) -> Option<String> {
        if let Some(message) = &self.message {
            return Some(message.to_string());
        }
        if self.results.is_empty() && !self.query.trim().is_empty() {
            return Some(self.empty_message());
        }
        speech::results(&self.results, self.selection.index())
    }

    /// Announces the summary when it changed, after typing pauses.
    fn announce_changes(&mut self, cx: &mut Context<Self>) {
        let summary = self.summary();
        if summary == self.spoken {
            return;
        }
        self.spoken = summary.clone();
        self.speech_task = summary.map(|text| {
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(SPEECH_PAUSE).await;
                speak(&text, Urgency::Polite);
            })
        });
    }

    fn height(&self) -> f32 {
        window_height(self.visible_rows(), self.shows_message())
    }

    /// What to say when a query finds nothing.
    fn empty_message(&self) -> String {
        if self.in_clipboard_scope(&self.query) {
            return if self.user.clipboard().len() == 0 {
                "Nothing copied since clipboard history was turned on".into()
            } else {
                "No copied text matches".into()
            };
        }
        FileQuery::problem(&self.query)
            .unwrap_or_else(|| format!("No results for “{}”", self.query.trim()))
    }

    /// Result rows on screen: none while a message is shown.
    fn visible_rows(&self) -> usize {
        if self.message.is_some() {
            0
        } else {
            self.results.len()
        }
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
            if self.voice_over
                && let Some(item) = self.results.get(selected)
            {
                let text = speech::selection(item, selected, self.results.len());
                // The highlight moved: say so now, and drop a pending summary.
                self.spoken = self.summary();
                self.speech_task =
                    Some(cx.spawn(async move |_, _| speak(&text, Urgency::Interrupt)));
            }
        }
        self.follow_quick_look(cx);
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_selected(cx);
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        // Esc closes the preview first, then the launcher.
        if self.quick_look.as_ref().is_some_and(|q| q.is_visible()) {
            self.close_quick_look(cx);
        } else {
            self.hide(cx);
        }
    }

    /// Whether the launcher or its preview has keyboard focus.
    fn has_focus(&self) -> bool {
        self.panel.as_ref().is_some_and(Panel::is_key)
            || self.quick_look.as_ref().is_some_and(|q| q.is_key())
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.selected()
            .and_then(|item| item.action.path())
            .map(Path::to_path_buf)
    }

    fn toggle_quick_look(&mut self, _: &QuickLookSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(quick_look) = self.quick_look.clone() else {
            return;
        };
        let path = self.selected_path();
        cx.spawn(async move |_, _| {
            if quick_look.is_visible() {
                quick_look.close();
            } else if let Some(path) = path {
                quick_look.show(&path);
            }
        })
        .detach();
    }

    /// Keeps an open preview on the selected item.
    fn follow_quick_look(&self, cx: &mut Context<Self>) {
        let Some(quick_look) = self.quick_look.clone() else {
            return;
        };
        if !quick_look.is_visible() {
            return;
        }
        let path = self.selected_path();
        cx.spawn(async move |_, _| quick_look.follow(path.as_deref()))
            .detach();
    }

    fn close_quick_look(&self, cx: &mut Context<Self>) {
        let Some(quick_look) = self.quick_look.clone() else {
            return;
        };
        if quick_look.is_visible() {
            cx.spawn(async move |_, _| quick_look.close()).detach();
        }
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

    fn activate_selected(&mut self, cx: &mut Context<Self>) {
        if self.message.take().is_some() {
            // Enter dismisses an error first; the list is hidden under it.
            self.resize(cx);
            cx.notify();
            return;
        }
        let Some(item) = self.selected().cloned() else {
            return;
        };
        let action = match (&item.kind, &item.action) {
            (ResultKind::Snippet, Action::CopyText(text)) => Action::CopyText(
                expand_placeholders(
                    text,
                    &sift_macos::format_local_now("yyyy-MM-dd"),
                    &sift_macos::format_local_now("HH:mm"),
                )
                .into(),
            ),
            (_, action) => action.clone(),
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
            Action::WindowCommand(text) => {
                let command = sift_wm::parse(text).map_err(|e| LaunchError::Command(e.to_string()));
                match (command, self.window_manager.as_ref()) {
                    (Ok(command), Some(runtime)) => {
                        // Hide before focusing another app, outside this GPUI
                        // update, so the launcher cannot steal focus back.
                        let controller = runtime.controller();
                        self.hide(cx);
                        cx.spawn(async move |_, _| {
                            let _ = controller.execute(command);
                        })
                        .detach();
                        return;
                    }
                    (Err(e), _) => Err(e),
                    (_, None) => Err(LaunchError::Command("Window Management is off".into())),
                }
            }
            Action::Run(command) => {
                self.record_use(&item);
                self.run_command(command, cx);
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
                self.record_use(&item);
                self.hide(cx);
            }
            Err(error) => self.show_error(error, cx),
        }
    }

    fn in_clipboard_scope(&self, query: &str) -> bool {
        self.user.clipboard_enabled() && clipboard::scope(query).is_some()
    }

    fn record_use(&mut self, item: &ResultItem) {
        self.user.record_use(item);
        self.rebuild_search();
    }

    fn toggle_favorite(&mut self, _: &ToggleFavorite, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.selected().cloned() else {
            return;
        };
        if self.user.toggle_favorite(&item).is_some() {
            self.rebuild_search();
            // Re-rank; the selection stays on the same item.
            self.run_query(self.query.clone(), cx);
        }
    }

    /// Removes the selected item from recents and favorites.
    fn forget_selected(&mut self, _: &ForgetSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected().map(|item| item.id.clone()) else {
            return;
        };
        self.user.forget(&id);
        self.rebuild_search();
        self.run_query(self.query.clone(), cx);
    }

    /// Writes one option to config.toml (from the settings window) and
    /// applies it at once: clipboard watching starts or stops now rather
    /// than when the panel next opens.
    pub fn change_setting(
        &mut self,
        setting: sift_state::Setting,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        sift_state::Config::save_setting(self.user.config_path(), setting)?;
        tracing::info!(?setting, "setting changed");
        self.reload_config(cx);
        Ok(())
    }

    pub fn clipboard_enabled(&self) -> bool {
        self.user.clipboard_enabled()
    }

    pub fn clipboard_max_items(&self) -> usize {
        self.user.clipboard_max_items()
    }

    /// Entries kept in the clipboard history (0 while it is off).
    pub fn clipboard_count(&self) -> usize {
        self.user.clipboard().len()
    }

    pub fn clear_clipboard(&mut self, cx: &mut Context<Self>) {
        self.user.clear_clipboard();
        self.rebuild_search();
        cx.emit(UserDataChanged);
    }

    fn edit_config(&mut self, cx: &mut Context<Self>) {
        match self.open_config_file() {
            Ok(()) => self.hide(cx),
            Err(error) => self.show_error(error, cx),
        }
    }

    /// Opens config.toml in a text editor, writing the commented template
    /// first if there is no file yet.
    pub fn open_config_file(&self) -> Result<(), LaunchError> {
        let path = self.user.config_path().to_path_buf();
        let failures = crate::launch_failures();
        sift_state::Config::ensure_exists(&path)
            .map_err(|e| LaunchError::Failed {
                path: path.clone(),
                reason: e.to_string(),
            })
            .and_then(|()| {
                sift_macos::open_text_file(&path, move |error| {
                    let _ = failures.unbounded_send(error);
                })
            })
    }

    /// Runs a built-in command (`commands::ALL`), as choosing it does.
    pub fn run_command(&mut self, command: &str, cx: &mut Context<Self>) {
        match command {
            commands::EDIT_CONFIG => self.edit_config(cx),
            commands::CLIPBOARD => {
                if self.user.clipboard_enabled() {
                    self.input
                        .update(cx, |input, cx| input.set_text("clip ", cx));
                } else {
                    self.message_info = false;
                    self.message =
                        Some("Clipboard history is off. Turn it on in Sift Settings.".into());
                    self.resize(cx);
                    cx.notify();
                }
            }
            commands::CLEAR_CLIPBOARD => {
                self.clear_clipboard(cx);
                self.hide(cx);
            }
            commands::SETTINGS => {
                self.hide(cx);
                // Opening a window draws it at once, and the settings view
                // reads the launcher while drawing. Inside this update that
                // read panics, so open it once the update has returned.
                let launcher = cx.entity();
                cx.defer(move |cx| crate::settings::open(launcher, cx));
            }
            commands::CHECK_UPDATES => match &self.update {
                UpdateState::Available(_) => self.open_release_page(cx),
                _ => self.check_for_updates(true, cx),
            },
            commands::QUIT => cx.quit(),
            commands::WINDOWS => {
                self.input
                    .update(cx, |input, cx| input.set_text("win ", cx));
            }
            commands::WORKSPACES => {
                self.input.update(cx, |input, cx| input.set_text("ws ", cx));
            }
            other => tracing::warn!(command = other, "unknown command"),
        }
    }

    pub fn update_state(&self) -> &UpdateState {
        &self.update
    }

    fn configure_windows(&mut self, cx: &mut Context<Self>) {
        let config = self.user.window_manager_config().clone();
        if !config.enabled {
            self.window_manager = None;
            self.window_snapshot = Arc::default();
            self.window_error = None;
        } else if let Err(error) = validate_window_hotkeys(&config) {
            self.window_error = Some(error);
            cx.emit(WindowManagerChanged);
            return;
        } else if let Some(runtime) = &self.window_manager {
            self.window_error = None;
            if config != self.window_config {
                if let Err(error) = runtime.controller().configure(config.clone()) {
                    self.window_error = Some(error);
                }
            }
        } else {
            // Publish only a wake signal. Readers take the latest snapshot,
            // so a busy UI never accumulates a history of window geometries.
            let (tx, mut rx) = futures::channel::mpsc::channel::<()>(1);
            let tx = std::sync::Mutex::new(tx);
            match sift_macos::wm::Runtime::start(
                config.clone(),
                &user_dir(),
                self.user.config_path(),
                move |_snapshot| {
                    let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).try_send(());
                },
            ) {
                Ok(runtime) => {
                    self.window_snapshot = runtime.controller().snapshot();
                    let controller = runtime.controller();
                    self.window_manager = Some(runtime);
                    self.window_error = None;
                    cx.spawn(async move |this, cx| {
                        while let Some(()) = rx.next().await {
                            let snapshot = controller.snapshot();
                            if this
                                .update(cx, |this, cx| {
                                    if !this.window_manager.as_ref().is_some_and(|runtime| {
                                        runtime.controller().same_instance(&controller)
                                    }) {
                                        return;
                                    }
                                    if this.window_manager.as_ref().is_some_and(|runtime| {
                                        *runtime.controller().config() != this.window_config
                                    }) {
                                        this.reload_config(cx);
                                    }
                                    this.window_snapshot = snapshot;
                                    this.sync_window_hotkeys(cx);
                                    this.rebuild_search();
                                    if this.is_visible() && !this.query.trim().is_empty() {
                                        this.run_query(this.query.clone(), cx);
                                    }
                                    cx.emit(WindowManagerChanged);
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
                    .detach();
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot enable window management");
                    self.window_error = Some(error);
                }
            }
        }
        self.window_config = config;
        self.sync_window_hotkeys(cx);
        self.rebuild_search();
        cx.emit(WindowManagerChanged);
    }

    pub fn sync_window_hotkeys(&mut self, cx: &mut Context<Self>) {
        if let Some(runtime) = &mut self.window_manager {
            if let Err(error) = runtime.sync_mouse_monitor() {
                self.window_error = Some(error);
            }
        }
        if cx.try_global::<crate::hotkey::Hotkeys>().is_none() {
            return;
        }
        let controller = self.window_manager.as_ref().map(|r| r.controller());
        let mode = if self.window_snapshot.mode.is_empty() {
            "main"
        } else {
            &self.window_snapshot.mode
        };
        if let Err(error) = cx.global_mut::<crate::hotkey::Hotkeys>().configure_windows(
            &self.window_config,
            mode,
            controller,
        ) {
            self.window_error = Some(error);
        }
    }

    pub fn window_management_enabled(&self) -> bool {
        self.user.window_manager_config().enabled
    }
    pub fn automatic_tiling_enabled(&self) -> bool {
        self.user.window_manager_config().automatic_tiling
    }
    pub fn window_management_status(&self) -> String {
        if let Some(error) = &self.window_error {
            return error.clone();
        }
        if let Some(message) = &self.window_snapshot.message {
            return message.clone();
        }
        if self.window_manager.is_some() {
            if !self.window_snapshot.automatic_tiling {
                return format!(
                    "Manual control · {} windows · {} mode{}",
                    self.window_snapshot.windows.len(),
                    self.window_snapshot.mode,
                    if self.window_snapshot.enabled {
                        ""
                    } else {
                        " · paused"
                    }
                );
            }
            format!(
                "{} windows · {} workspaces · {} mode{}",
                self.window_snapshot.windows.len(),
                self.window_snapshot.workspaces.len(),
                self.window_snapshot.mode,
                if self.window_snapshot.enabled {
                    ""
                } else {
                    " · paused"
                }
            )
        } else {
            "Window shortcuts and manual actions. Automatic tiling is off by default.".into()
        }
    }

    pub fn request_window_permission(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            sift_macos::wm::request_accessibility();
            let _ = this.update(cx, |this, cx| this.configure_windows(cx));
        })
        .detach();
    }

    pub fn retry_window_management(&mut self, cx: &mut Context<Self>) {
        if self.window_config.enabled && self.window_manager.is_none() {
            self.configure_windows(cx);
        }
    }

    pub fn stop_window_management(&mut self) -> Option<sift_macos::wm::Controller> {
        let runtime = self.window_manager.take()?;
        let controller = runtime.controller();
        drop(runtime);
        Some(controller)
    }

    pub fn update_checks_enabled(&self) -> bool {
        self.user.update_checks_enabled()
    }

    /// Shows information (not a problem) in place of the results.
    fn show_info(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        if !self.is_visible() {
            // Asked from the settings window, which shows the result.
            return;
        }
        self.message = Some(text.into());
        self.message_info = true;
        self.resize(cx);
        cx.notify();
    }

    pub fn open_release_page(&mut self, cx: &mut Context<Self>) {
        let failures = crate::launch_failures();
        let opened = sift_macos::open_url(updates::RELEASES_PAGE, move |error| {
            let _ = failures.unbounded_send(error);
        });
        match opened {
            Ok(()) => self.hide(cx),
            Err(error) => self.show_error(error, cx),
        }
    }

    /// Asks GitHub for the latest release, in the background. Automatic
    /// checks (`explicit` false) run only when enabled in the configuration
    /// and a day after the last one; an explicit check (the command or the
    /// settings button) always runs and reports its result.
    pub fn check_for_updates(&mut self, explicit: bool, cx: &mut Context<Self>) {
        if self.update_task.is_some() {
            return;
        }
        if !explicit {
            let failed_recently = self
                .update_failed_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(60 * 60));
            if !self.user.update_checks_enabled()
                || failed_recently
                || !updates::due(&self.last_update_check, sift_state::now())
            {
                return;
            }
        }
        let previous = std::mem::replace(&mut self.update, UpdateState::Checking);
        if explicit {
            self.show_info("Checking for updates…", cx);
        }
        let fetch = cx.background_spawn(async { updates::fetch_latest() });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let result = fetch.await;
            this.update(cx, |this, cx| {
                this.update_task = None;
                this.finish_update_check(result, previous, explicit, cx);
            })
            .ok();
        }));
        cx.emit(UpdateStateChanged);
    }

    fn finish_update_check(
        &mut self,
        result: Result<Option<String>, String>,
        previous: UpdateState,
        explicit: bool,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(latest) => {
                self.update_failed_at = None;
                self.last_update_check = updates::LastCheck {
                    at: sift_state::now(),
                    latest: latest.clone(),
                };
                updates::save(&user_dir(), &self.last_update_check);
                match latest.filter(|v| updates::is_newer(v, updates::CURRENT)) {
                    Some(version) => {
                        tracing::info!(version, "a newer release is available");
                        if explicit {
                            self.show_info(
                                format!(
                                    "Sift {version} is available. Choose Check for Updates \
                                     again to open the download page."
                                ),
                                cx,
                            );
                        }
                        self.update = UpdateState::Available(version);
                    }
                    None => {
                        tracing::info!("no newer release");
                        self.update = UpdateState::UpToDate;
                        if explicit {
                            self.show_info(
                                format!("Sift {} is the latest version.", updates::CURRENT),
                                cx,
                            );
                        }
                    }
                }
            }
            Err(reason) => {
                tracing::warn!(%reason, "cannot check for updates");
                self.update_failed_at = Some(Instant::now());
                if explicit {
                    if self.is_visible() {
                        self.message_info = false;
                        self.message =
                            Some(format!("Could not check for updates: {reason}").into());
                        self.resize(cx);
                    }
                    self.update = UpdateState::Failed(reason);
                } else {
                    // A failed automatic check is not worth a message;
                    // keep what was known.
                    self.update = previous;
                }
            }
        }
        cx.emit(UpdateStateChanged);
        cx.notify();
    }

    fn show_error(&mut self, error: LaunchError, cx: &mut Context<Self>) {
        self.message_info = false;
        self.message = Some(error.to_string().into());
        self.resize(cx);
        cx.notify();
    }

    /// The icon for `item`, if rendered. Missing icons are rendered on the
    /// background executor and the view refreshes when they arrive.
    fn icon(&mut self, item: &ResultItem, cx: &mut Context<Self>) -> Option<Arc<RenderImage>> {
        let (key, path) = icon_for(item)?;
        if let Some(icon) = self.icons.get(&key) {
            return icon.cloned();
        }
        if self.icon_jobs.insert(key.clone()) {
            let decode = icon_decoder(cx);
            let render = cx.background_spawn(async move { render_icon(&path, &decode) });
            cx.spawn(async move |this, cx| {
                let icon = render.await;
                this.update(cx, |this, cx| {
                    this.icon_jobs.remove(&key);
                    let rendered = icon.is_some();
                    let dropped = this.icons.insert(key, icon, false);
                    tracing::debug!(
                        rendered,
                        cached = this.icons.len(),
                        evicted = dropped.len(),
                        "icon ready"
                    );
                    free_textures(dropped, cx);
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
        // Shares the result's strings rather than copying them every frame.
        let title = SharedString::new(item.title.clone());
        let subtitle = item.subtitle.clone().map(SharedString::new);
        let favorite = self.user.is_favorite(&item.id);
        let kind = item.kind;
        let item = item.clone();
        let icon = self.icon(&item, cx);

        div()
            .id(("result", index))
            .h(px(ROW_HEIGHT))
            .mx(px(ROW_MARGIN))
            .px(px(INSET - ROW_MARGIN))
            .flex()
            .items_center()
            .gap(px(ICON_GAP))
            .rounded(px(8.))
            .when(selected, |row| row.bg(theme.selected))
            .when(!selected, |row| row.hover(|style| style.bg(theme.hover)))
            .when_some(theme.outline, |row, outline| {
                // Every row gets the border so that rows keep one size.
                row.border_1().border_color(if selected {
                    outline
                } else {
                    gpui::transparent_black()
                })
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selection.select(index);
                this.activate_selected(cx);
            }))
            .child(result_icon(kind, icon))
            .child(
                // Title, then the folder or detail in quieter text on the
                // same line; the detail gives way first when space runs out.
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_none()
                            .max_w(relative(0.75))
                            .truncate()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(title),
                    )
                    .when_some(subtitle, |line, subtitle| {
                        line.child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_size(px(13.))
                                .text_color(theme.muted)
                                .child(subtitle),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .text_color(theme.placeholder)
                    .when(favorite, |accessory| {
                        accessory.child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.favorite)
                                .child("★"),
                        )
                    })
                    .child(kind.label()),
            )
            .into_any_element()
    }

    /// What the list shows, above the first row.
    fn section_label(&self) -> &'static str {
        if self.in_clipboard_scope(&self.query) {
            "Clipboard History"
        } else if self.query.trim().is_empty() {
            "Favorites and Recents"
        } else {
            "Results"
        }
    }

    fn render_section(&self) -> AnyElement {
        let theme = self.theme;
        let position = format!(
            "{} of {}",
            self.selection.index().map_or(0, |i| i + 1),
            self.results.len()
        );
        div()
            .h(px(SECTION_HEIGHT))
            .px(px(INSET))
            .flex()
            .items_center()
            .justify_between()
            .text_size(px(11.))
            .text_color(theme.placeholder)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.section_label()),
            )
            .child(position)
            .into_any_element()
    }

    /// The status at the left of the footer, with the colour of its dot.
    fn status_line(&self) -> (Hsla, SharedString) {
        let theme = self.theme;
        let roots = &self.index_status.roots;
        if self.app_count == 0 {
            (theme.accent, "Loading applications…".into())
        } else if roots
            .iter()
            .any(|r| matches!(r.state, sift_files::RootState::Indexing { .. }))
        {
            (theme.accent, "Indexing files…".into())
        } else if roots.iter().any(|r| {
            matches!(
                r.state,
                sift_files::RootState::Unavailable | sift_files::RootState::Failed(_)
            )
        }) {
            (
                theme.error,
                "A search folder is unavailable · see Sift Settings".into(),
            )
        } else if let UpdateState::Available(version) = &self.update {
            (
                theme.accent,
                format!("Sift {version} is available · search “update”").into(),
            )
        } else {
            // From the service's status, not `FileIndex::len`: that takes
            // the index lock, which a background search holds, and this
            // runs on every frame.
            let files: u64 = roots.iter().map(|r| r.entries).sum();
            (
                theme.success,
                format!(
                    "{} apps · {} files and folders",
                    self.app_count,
                    thousands(files as usize)
                )
                .into(),
            )
        }
    }

    fn render_footer(&self) -> AnyElement {
        let theme = self.theme;
        let (dot, status) = self.status_line();
        let selected = self.selected();
        let can_reveal = selected.is_some_and(|i| i.action.path().is_some());
        let favorite_label = selected
            .filter(|i| {
                !matches!(
                    i.kind,
                    ResultKind::Calculator
                        | ResultKind::Clipboard
                        | ResultKind::Window
                        | ResultKind::Workspace
                )
            })
            .map(|i| {
                if self.user.is_favorite(&i.id) {
                    "Unfavorite"
                } else {
                    "Favorite"
                }
            });
        let open_label = match selected.map(|i| &i.action) {
            Some(Action::Run(_) | Action::Shell(_)) => "Run",
            Some(Action::CopyText(_)) => "Copy",
            Some(Action::WindowCommand(_)) => "Switch",
            _ => "Open",
        };
        let hint = |label: &'static str, keys: &'static [&'static str], primary: bool| {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .text_color(if primary { theme.text } else { theme.muted })
                        .when(primary, |label| label.font_weight(FontWeight::MEDIUM))
                        .child(label),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(3.))
                        .children(keys.iter().map(|k| key_cap(k, &theme))),
                )
        };
        let actions = match selected {
            Some(_) => div()
                .flex()
                .items_center()
                .gap(px(14.))
                .child(hint(open_label, &["↵"], true))
                .child(div().w(px(1.)).h(px(14.)).bg(theme.divider))
                .when(can_reveal, |row| {
                    row.child(hint("Show in Finder", &["⌘", "↵"], false))
                        .child(hint("Quick Look", &["⌘", "Y"], false))
                })
                .when_some(favorite_label, |row, label| {
                    row.child(hint(label, &["⌘", "D"], false))
                })
                .when(!can_reveal && favorite_label.is_none(), |row| {
                    row.child(hint("Close", &["esc"], false))
                }),
            None => div().child(hint("Close", &["esc"], false)),
        };
        div()
            .h(px(FOOTER_HEIGHT))
            .pl(px(INSET))
            .pr(px(10.))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(16.))
            .bg(theme.surface)
            .text_size(px(12.))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_center()
                    .gap(px(7.))
                    .text_color(theme.muted)
                    .child(div().flex_none().size(px(6.)).rounded_full().bg(dot))
                    .child(div().min_w(px(0.)).truncate().child(status)),
            )
            .child(actions.flex_none())
            .into_any_element()
    }
}

/// A keyboard key, as drawn in the footer.
fn key_cap(label: &'static str, theme: &Theme) -> AnyElement {
    div()
        .min_w(px(18.))
        .h(px(18.))
        .px(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .bg(theme.key_background)
        .text_color(theme.key_text)
        .text_size(px(11.))
        .font_weight(FontWeight::NORMAL)
        .child(label)
        .into_any_element()
}

/// A result's icon: the file or app icon once rendered, or a coloured tile
/// with a symbol for results that have no file.
fn result_icon(kind: ResultKind, icon: Option<Arc<RenderImage>>) -> AnyElement {
    if let Some(image) = icon {
        return img(image)
            .flex_none()
            .size(px(ICON_SIZE))
            .into_any_element();
    }
    let symbol = match kind {
        ResultKind::Command => Some("⌘"),
        ResultKind::Calculator => Some("="),
        ResultKind::Snippet => Some("¶"),
        ResultKind::Clipboard => None,
        ResultKind::Window => Some("▣"),
        ResultKind::Workspace => Some("▦"),
        // Its icon is still being rendered: keep the space.
        ResultKind::Application | ResultKind::File | ResultKind::Folder => {
            return div().flex_none().size(px(ICON_SIZE)).into_any_element();
        }
    };
    let white = gpui::white();
    div()
        .flex_none()
        .size(px(ICON_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .bg(tile_color(kind))
        .text_color(white)
        .text_size(px(15.))
        .font_weight(FontWeight::SEMIBOLD)
        .map(|tile| match symbol {
            Some(symbol) => tile.child(symbol),
            None => tile.child(
                svg()
                    .path(crate::assets::CLIPBOARD)
                    .size(px(16.))
                    .text_color(white),
            ),
        })
        .into_any_element()
}

fn validate_window_hotkeys(config: &sift_wm::Config) -> Result<(), String> {
    for (name, mode) in &config.bindings {
        let mut seen = std::collections::HashSet::new();
        for shortcut in mode.keys() {
            let key = crate::hotkey::window_mode_hotkey(shortcut, name)?;
            if !seen.insert(key.id()) {
                return Err(format!("Duplicate window shortcut: {shortcut}"));
            }
        }
    }
    Ok(())
}

/// Emitted when the file index status changes (the settings window
/// listens).
pub struct IndexStatusChanged;

/// Emitted when an update check starts or ends (the settings window
/// listens).
pub struct UpdateStateChanged;
pub struct WindowManagerChanged;
impl gpui::EventEmitter<WindowManagerChanged> for Launcher {}

impl gpui::EventEmitter<UpdateStateChanged> for Launcher {}

impl gpui::EventEmitter<IndexStatusChanged> for Launcher {}

/// Emitted when config.toml is loaded again or the clipboard history
/// changes (the settings window listens).
pub struct UserDataChanged;

impl gpui::EventEmitter<UserDataChanged> for Launcher {}

/// Decodes an icon's PNG into the form GPUI draws (BGRA pixels). Doing it
/// here, off the main thread, also means the launcher owns every decoded
/// icon and can free its texture when the icon is evicted.
fn icon_decoder(cx: &App) -> impl Fn(Vec<u8>) -> Option<Arc<RenderImage>> + Send + 'static {
    let renderer = cx.svg_renderer();
    move |png| {
        Image::from_bytes(ImageFormat::Png, png)
            .to_image_data(renderer.clone())
            .inspect_err(|error| tracing::debug!(%error, "cannot decode an icon"))
            .ok()
    }
}

/// Renders `path`'s icon (background thread).
fn render_icon(
    path: &Path,
    decode: &impl Fn(Vec<u8>) -> Option<Arc<RenderImage>>,
) -> Option<Arc<RenderImage>> {
    decode(sift_macos::icon_png(path, ICON_PX)?)
}

fn speak(text: &str, urgency: Urgency) {
    tracing::debug!(text, ?urgency, "announce");
    announce(text, urgency);
}

/// Removes evicted icons from the windows' texture atlases.
fn free_textures(images: Vec<Arc<RenderImage>>, cx: &mut App) {
    for image in images {
        cx.drop_image(image, None);
    }
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
            if let Some(bench) = &mut self.bench {
                bench.shown(at.elapsed());
            }
        }
        if let Some(bench) = &mut self.bench {
            bench.frame();
        }
        if self.voice_over && self.is_visible() {
            self.announce_changes(cx);
        }
        let theme = self.theme;
        let divider = || div().flex_none().h(px(DIVIDER)).w_full().bg(theme.divider);
        let end = (self.scroll_top + VISIBLE_ROWS).min(self.results.len());
        let rows: Vec<AnyElement> = (self.scroll_top..end)
            .map(|i| self.render_row(i, cx))
            .collect();
        let message: Option<SharedString> = self
            .message
            .clone()
            .or_else(|| self.shows_message().then(|| self.empty_message().into()));
        let message_color = if self.message.is_some() && !self.message_info {
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
            .on_action(cx.listener(Self::toggle_favorite))
            .on_action(cx.listener(Self::toggle_quick_look))
            .on_action(cx.listener(Self::forget_selected))
            .on_action(cx.listener(Self::dismiss))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.text)
            .child(
                div()
                    .h(px(HEADER_HEIGHT))
                    .px(px(INSET))
                    .flex()
                    .items_center()
                    .gap(px(ICON_GAP))
                    .child(
                        // Centred over the result icons below it.
                        div()
                            .flex_none()
                            .size(px(ICON_SIZE))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                svg()
                                    .path(crate::assets::SEARCH)
                                    .size(px(18.))
                                    .text_color(theme.placeholder),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(19.))
                            .line_height(px(24.))
                            .child(self.input.clone()),
                    ),
            )
            .when(self.message.is_none() && !rows.is_empty(), |root| {
                root.child(divider()).child(
                    div()
                        .py(px(LIST_PADDING))
                        .flex()
                        .flex_col()
                        .child(self.render_section())
                        .children(rows),
                )
            })
            .when(self.visible_rows() == 0, |root| {
                root.when_some(message, |root, message| {
                    root.child(divider()).child(
                        div()
                            .h(px(MESSAGE_HEIGHT))
                            .px(px(INSET))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(13.))
                            .text_color(message_color)
                            .child(div().text_center().line_clamp(2).child(message)),
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
