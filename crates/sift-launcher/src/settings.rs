//! The settings window: which folders the file search covers.
//!
//! It shows each indexed folder with its state and size, and sends changes
//! to the index service. Folders are chosen with the standard macOS folder
//! picker, which is also how macOS grants access to protected locations
//! such as Documents.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{
    AnyElement, App, Bounds, Context, Entity, FocusHandle, Focusable, FontWeight, Global,
    SharedString, Subscription, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowKind,
    WindowOptions, actions, div, prelude::*, px, size, svg,
};
use sift_files::{Command, RootState, RootStatus};
use sift_indexer::IndexOptions;

use crate::launcher::{IndexStatusChanged, Launcher};
use crate::theme::Theme;

actions!(settings, [CloseSettings]);

/// The open settings window, so a second request focuses it instead of
/// opening another.
struct OpenSettings(WindowHandle<Settings>);

impl Global for OpenSettings {}

/// Whether the settings window is open.
pub fn is_open(cx: &App) -> bool {
    cx.try_global::<OpenSettings>().is_some_and(|open| {
        let id = open.0.window_id();
        cx.windows().iter().any(|w| w.window_id() == id)
    })
}

/// Opens the settings window, or brings the open one to the front.
///
/// Must not run inside an update of the launcher: the new window draws
/// immediately, and drawing reads the launcher (see `Launcher::run_command`).
pub fn open(launcher: Entity<Launcher>, cx: &mut App) {
    // Activation re-enters GPUI through AppKit callbacks, so it runs after
    // the current update (see `Panel::set_height` for the same issue).
    let activate = || {
        sift_macos::activate_app();
    };
    if let Some(open) = cx.try_global::<OpenSettings>().map(|open| open.0)
        && open
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        cx.spawn(async move |_| activate()).detach();
        return;
    }
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Sift Settings".into()),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(680.), px(540.)),
            cx,
        ))),
        kind: WindowKind::Normal,
        focus: true,
        show: true,
        is_resizable: true,
        app_id: Some("io.github.kylianmarceau.sift".into()),
        ..Default::default()
    };
    match cx.open_window(options, |window, cx| {
        cx.new(|cx| Settings::new(launcher, window, cx))
    }) {
        Ok(handle) => {
            tracing::info!("settings window opened");
            cx.set_global(OpenSettings(handle));
            cx.spawn(async move |_| activate()).detach();
        }
        Err(error) => tracing::error!(%error, "cannot open the settings window"),
    }
}

/// Results of the (asynchronous) folder picker.
enum Picked {
    AddRoot(PathBuf),
    Exclude { root: PathBuf, path: PathBuf },
}

pub struct Settings {
    launcher: Entity<Launcher>,
    focus_handle: FocusHandle,
    picked: UnboundedSender<Picked>,
    message: Option<SharedString>,
    home: Option<PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl Settings {
    fn new(launcher: Entity<Launcher>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (picked, mut picks) = unbounded::<Picked>();
        cx.spawn(async move |this, cx| {
            while let Some(pick) = picks.next().await {
                if this
                    .update(cx, |this, cx| this.on_picked(pick, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        Self {
            _subscriptions: vec![
                cx.subscribe(&launcher, |_, _, _: &IndexStatusChanged, cx| cx.notify()),
                cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
            ],
            launcher,
            focus_handle,
            picked,
            message: None,
            home: std::env::home_dir(),
        }
    }

    fn send(&self, command: Command, cx: &App) {
        match self.launcher.read(cx).index_service() {
            Some(service) => service.send(command),
            None => tracing::error!("the file index service is not running"),
        }
    }

    fn pick_folder(
        &self,
        prompt: &'static str,
        message: &'static str,
        start: Option<PathBuf>,
        cx: &mut Context<Self>,
        wrap: impl Fn(PathBuf) -> Picked + 'static,
    ) {
        let tx = self.picked.clone();
        cx.spawn(async move |_, _| {
            sift_macos::choose_folder(prompt, message, start.as_deref(), move |path| {
                if let Some(path) = path {
                    let _ = tx.unbounded_send(wrap(path));
                }
            });
        })
        .detach();
    }

    fn add_folder(&mut self, cx: &mut Context<Self>) {
        let start = self.home.clone();
        self.pick_folder(
            "Add",
            "Choose a folder for Sift to search. Only names and dates are indexed.",
            start,
            cx,
            Picked::AddRoot,
        );
    }

    fn exclude_folder(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let start = Some(root.clone());
        self.pick_folder(
            "Exclude",
            "Choose a folder inside this location that Sift should skip.",
            start,
            cx,
            move |path| Picked::Exclude {
                root: root.clone(),
                path,
            },
        );
    }

    fn on_picked(&mut self, pick: Picked, cx: &mut Context<Self>) {
        self.message = None;
        match pick {
            Picked::AddRoot(path) => {
                let path = path.canonicalize().unwrap_or(path);
                if sift_indexer::is_broad_root(&path) {
                    self.message = Some(
                        "Sift does not index your whole home folder or the startup disk, which \
                         would include system and app data. Choose specific folders, such as \
                         Documents or Projects."
                            .into(),
                    );
                } else {
                    self.send(
                        Command::AddRoot {
                            path,
                            options: IndexOptions::default(),
                        },
                        cx,
                    );
                }
            }
            Picked::Exclude { root, path } => {
                let path = path.canonicalize().unwrap_or(path);
                if !path.starts_with(&root) || path == root {
                    self.message =
                        Some(format!("Choose a folder inside {}.", self.display(&root)).into());
                } else if let Some(mut options) = self.options_of(&root, cx) {
                    if !options.exclude_paths.contains(&path) {
                        options.exclude_paths.push(path);
                        self.send(
                            Command::SetOptions {
                                path: root,
                                options,
                            },
                            cx,
                        );
                    }
                }
            }
        }
        cx.notify();
    }

    fn options_of(&self, root: &Path, cx: &App) -> Option<IndexOptions> {
        self.launcher
            .read(cx)
            .index_status()
            .roots
            .iter()
            .find(|r| r.path == root)
            .map(|r| r.options.clone())
    }

    fn display(&self, path: &Path) -> String {
        match self.home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
            Some(rest) if rest.as_os_str().is_empty() => "~".into(),
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        }
    }

    fn close(&mut self, _: &CloseSettings, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    fn render_root(
        &self,
        index: usize,
        root: &RootStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (state, state_color) = match &root.state {
            RootState::Watching => ("Up to date".to_string(), theme.success),
            RootState::Indexing { entries } => {
                (format!("Indexing… {}", thousands(*entries)), theme.accent)
            }
            RootState::Unavailable => ("Not available".to_string(), theme.error),
            RootState::Failed(_) => ("Failed".to_string(), theme.error),
        };
        let problem = match &root.state {
            RootState::Unavailable => Some("Moved, deleted or not mounted.".to_string()),
            RootState::Failed(reason) => Some(reason.clone()),
            _ => None,
        };
        let details = format!(
            "{} items · indexed {}{}",
            thousands(root.entries),
            ago(root.indexed_at),
            if root.errors > 0 {
                format!(" · {} unreadable", thousands(root.errors))
            } else {
                String::new()
            }
        );
        let skip_deps = root.options.skip_dependency_dirs;
        let path = root.path.clone();

        let reindex = {
            let path = path.clone();
            button(("reindex", index), "Re-index", Kind::Secondary, theme).on_click(
                cx.listener(move |this, _, _, cx| this.send(Command::Reindex(path.clone()), cx)),
            )
        };
        let exclude = {
            let path = path.clone();
            button(
                ("exclude", index),
                "Exclude Folder…",
                Kind::Secondary,
                theme,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.exclude_folder(path.clone(), cx)))
        };
        let toggle = {
            let path = path.clone();
            let options = root.options.clone();
            div()
                .id(("deps", index))
                .flex()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .child(switch(skip_deps, theme))
                .child(
                    div()
                        .text_size(px(12.))
                        .child("Skip dependency and cache folders"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    let mut options = options.clone();
                    options.skip_dependency_dirs = !options.skip_dependency_dirs;
                    this.send(
                        Command::SetOptions {
                            path: path.clone(),
                            options,
                        },
                        cx,
                    );
                }))
        };
        let remove = {
            let path = path.clone();
            button(("remove", index), "Remove", Kind::Destructive, theme).on_click(
                cx.listener(move |this, _, _, cx| this.send(Command::RemoveRoot(path.clone()), cx)),
            )
        };

        let exclusions: Vec<AnyElement> = root
            .options
            .exclude_paths
            .iter()
            .enumerate()
            .map(|(i, excluded)| {
                let mut options = root.options.clone();
                options.exclude_paths.retain(|p| p != excluded);
                let path = path.clone();
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(px(12.))
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(theme.muted)
                            .child(format!("Skipped: {}", self.display(excluded))),
                    )
                    .child(
                        button(
                            ("unexclude", index * 1000 + i),
                            "Include Again",
                            Kind::Link,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send(
                                Command::SetOptions {
                                    path: path.clone(),
                                    options: options.clone(),
                                },
                                cx,
                            )
                        })),
                    )
                    .into_any_element()
            })
            .collect();

        let hairline = || div().h(px(1.)).bg(theme.divider);
        div()
            .p(px(14.))
            .rounded(px(10.))
            .border_1()
            .border_color(theme.divider)
            .bg(theme.card)
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_none()
                            .size(px(32.))
                            .rounded(px(8.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(theme.accent.opacity(0.14))
                            .child(
                                svg()
                                    .path(crate::assets::FOLDER)
                                    .size(px(17.))
                                    .text_color(theme.accent),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(2.))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(self.display(&root.path)),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(theme.muted)
                                    .child(problem.unwrap_or(details)),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(theme.muted)
                            .child(div().size(px(7.)).rounded_full().bg(state_color))
                            .child(state),
                    ),
            )
            .when(!exclusions.is_empty(), |card| {
                card.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .pl(px(44.))
                        .children(exclusions),
                )
            })
            .child(hairline())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(toggle)
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(reindex)
                            .child(exclude)
                            .child(remove),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for Settings {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        let status = self.launcher.read(cx).index_status().clone();
        let roots: Vec<AnyElement> = status
            .roots
            .iter()
            .enumerate()
            .map(|(i, root)| self.render_root(i, root, &theme, cx))
            .collect();
        let message = self
            .message
            .clone()
            .or_else(|| status.message.clone().map(Into::into));
        let mut facts = vec![format!("Index size {}", bytes(status.index_bytes))];
        if let Some(update) = status.last_update {
            facts.push(format!(
                "last change searchable in {} ms",
                update.event_to_searchable.as_millis()
            ));
        }
        let count = match status.roots.len() {
            0 => String::new(),
            1 => "1 folder".to_string(),
            n => format!("{n} folders"),
        };

        div()
            .key_context("Settings")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close))
            .size_full()
            .bg(theme.background)
            .text_color(theme.text)
            .text_size(px(13.))
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(PAGE_INSET))
                    .pt(px(20.))
                    .pb(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("File Search"),
                    )
                    .child(div().max_w(px(540.)).text_color(theme.muted).child(
                        "Sift finds files and folders by name in these locations and keeps up \
                         with changes as they happen. Only names, sizes and dates are stored, \
                         on this Mac; file contents are never read.",
                    )),
            )
            .child(
                div()
                    .px(px(PAGE_INSET))
                    .pb(px(8.))
                    .flex()
                    .justify_between()
                    .text_size(px(11.))
                    .text_color(theme.placeholder)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Locations"))
                    .child(count),
            )
            .child(
                div()
                    .id("roots")
                    .flex_grow()
                    .overflow_y_scroll()
                    .px(px(PAGE_INSET))
                    .pb(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .when(roots.is_empty(), |list| {
                        list.child(
                            div()
                                .p(px(20.))
                                .rounded(px(10.))
                                .border_1()
                                .border_color(theme.divider)
                                .bg(theme.card)
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap(px(6.))
                                .text_center()
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("No folders yet"),
                                )
                                .child(div().text_color(theme.muted).child(
                                    "Add the folders you want to search, such as Documents \
                                     or a projects folder.",
                                )),
                        )
                    })
                    .children(roots),
            )
            .when_some(message, |page, message| {
                page.child(
                    div().px(px(PAGE_INSET)).pb(px(12.)).child(
                        div()
                            .px(px(12.))
                            .py(px(8.))
                            .rounded(px(8.))
                            .bg(theme.error.opacity(0.12))
                            .text_color(theme.error)
                            .child(message),
                    ),
                )
            })
            .child(div().h(px(1.)).bg(theme.divider))
            .child(
                div()
                    .h(px(52.))
                    .px(px(PAGE_INSET))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .bg(theme.surface)
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(px(12.))
                            .text_color(theme.muted)
                            .child(facts.join(" · ")),
                    )
                    .child(
                        button("add", "Add Folder…", Kind::Primary, &theme)
                            .on_click(cx.listener(|this, _, _, cx| this.add_folder(cx))),
                    ),
            )
    }
}

/// Left and right margin of the settings page.
const PAGE_INSET: f32 = 24.;

#[derive(Clone, Copy)]
enum Kind {
    /// The one main action on the page.
    Primary,
    Secondary,
    /// Removes something.
    Destructive,
    /// A small text-only action inside a line.
    Link,
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    kind: Kind,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label);
    let (bg, hover, text) = (theme.key_background, theme.selected, theme.text);
    match kind {
        Kind::Primary => base
            .h(px(28.))
            .px(px(14.))
            .bg(theme.accent)
            .text_color(theme.on_accent)
            .hover(|style| style.opacity(0.88)),
        Kind::Secondary => base
            .h(px(26.))
            .px(px(10.))
            .bg(bg)
            .text_color(text)
            .hover(move |style| style.bg(hover)),
        Kind::Destructive => base
            .h(px(26.))
            .px(px(10.))
            .bg(bg)
            .text_color(theme.error)
            .hover(move |style| style.bg(hover)),
        Kind::Link => base
            .px(px(4.))
            .text_color(theme.accent)
            .hover(|style| style.opacity(0.75)),
    }
}

/// An on/off switch, drawn without animation.
fn switch(on: bool, theme: &Theme) -> AnyElement {
    div()
        .flex_none()
        .w(px(30.))
        .h(px(18.))
        .p(px(2.))
        .rounded_full()
        .flex()
        .items_center()
        .when(on, |track| track.justify_end().bg(theme.accent))
        .when(!on, |track| track.justify_start().bg(theme.selected))
        .child(
            div()
                .size(px(14.))
                .rounded_full()
                .bg(gpui::white())
                .shadow_sm(),
        )
        .into_any_element()
}

fn thousands(n: u64) -> String {
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

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.),
        n => format!("{n} bytes"),
    }
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
fn ago(unix: i64) -> String {
    if unix <= 0 {
        return "never".into();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(unix);
    let secs = (now - unix).max(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_and_ages() {
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(2 * 1024 * 1024 + 100), "2.0 MB");
        assert_eq!(thousands(54_206), "54,206");
        assert_eq!(ago(0), "never");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 600), "10 min ago");
        assert_eq!(ago(now - 3 * 86400), "3 days ago");
    }
}
