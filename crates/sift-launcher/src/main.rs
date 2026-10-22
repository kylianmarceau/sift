//! Sift: a keyboard-first macOS launcher.
//!
//! Runs as an accessory app (no Dock icon). A global shortcut (Option+Space
//! unless changed in Settings) shows a floating search panel; Escape, the hotkey again, or clicking elsewhere hides it.
//! The process, window and caches stay alive between uses, so showing the
//! panel is an order-front, not a relaunch.

mod assets;
mod clipboard;
mod commands;
mod hotkey;
mod icons;
mod input;
mod launcher;
mod logging;
mod session;
mod settings;
mod shell;
mod speech;
mod theme;
mod uibench;
mod updates;
mod user;

use std::sync::OnceLock;
use std::time::Instant;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions, actions, px, size,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use sift_macos::{LaunchError, Panel};

use crate::input as text;
use crate::launcher::{
    Confirm, Dismiss, ForgetSelected, Launcher, QuickLookSelected, RevealSelected, SelectNext,
    SelectPrevious, ToggleFavorite,
};

actions!(sift, [Quit]);

static LAUNCH_FAILURES: OnceLock<UnboundedSender<LaunchError>> = OnceLock::new();

/// Channel for launch failures, which macOS reports on a background queue.
pub fn launch_failures() -> UnboundedSender<LaunchError> {
    LAUNCH_FAILURES
        .get()
        .cloned()
        .unwrap_or_else(|| unbounded().0)
}

fn main() {
    let started = Instant::now();
    logging::init(log_level());
    let (session, unclean_exit) = session::Session::start(&launcher::user_dir());

    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            // GPUI switches the app to the regular policy while launching;
            // switch back so sift has no Dock icon even when run unbundled.
            sift_macos::use_accessory_activation_policy();
            bind_keys(cx);

            let window = match cx.open_window(window_options(cx), |window, cx| {
                cx.new(|cx| Launcher::new(launcher::Startup { unclean_exit }, window, cx))
            }) {
                Ok(window) => window,
                Err(error) => {
                    tracing::error!(%error, "cannot open the launcher window");
                    cx.quit();
                    return;
                }
            };

            let attached = window.update(cx, |launcher, window, cx| {
                let panel = window.window_handle().ok().and_then(|handle| {
                    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
                        return None;
                    };
                    // SAFETY: the view belongs to the window GPUI just created,
                    // and this closure runs on the main thread.
                    unsafe { Panel::from_ns_view(appkit.ns_view) }
                });
                match panel {
                    Some(panel) => launcher.attach_panel(panel, cx),
                    None => tracing::error!("cannot access the native window"),
                }
                // Show once at launch so starting the app visibly does something,
                // unless asked not to (for example when started at login).
                if std::env::var_os("SIFT_START_HIDDEN").is_none() {
                    launcher.show(window, cx);
                }
            });
            if let Err(error) = attached {
                tracing::error!(%error, "cannot set up the launcher window");
            }

            // Shortcut presses arrive from a Carbon event handler on the main
            // thread; forward them to a GPUI task through a channel.
            let (hotkey_tx, mut hotkey_rx) = unbounded::<Instant>();
            hotkey::install(launcher::user_dir(), hotkey_tx, cx);
            cx.spawn(async move |cx| {
                while let Some(pressed_at) = hotkey_rx.next().await {
                    let _ = window.update(cx, |launcher, window, cx| {
                        launcher.toggle(pressed_at, window, cx)
                    });
                }
            })
            .detach();

            let (failure_tx, mut failure_rx) = unbounded::<LaunchError>();
            let _ = LAUNCH_FAILURES.set(failure_tx);
            cx.spawn(async move |cx| {
                while let Some(error) = failure_rx.next().await {
                    let _ = window.update(cx, |launcher, window, cx| {
                        launcher.launch_failed(error, window, cx)
                    });
                }
            })
            .detach();

            // SIFT_RUN_COMMAND=<id> runs a built-in command shortly after
            // launch, from inside a launcher update as choosing it does (for
            // testing commands such as `settings` without a person).
            if let Ok(command) = std::env::var("SIFT_RUN_COMMAND") {
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(500))
                        .await;
                    tracing::info!(command, "running command from SIFT_RUN_COMMAND");
                    let _ = window.update(cx, |launcher, _, cx| launcher.run_command(&command, cx));
                })
                .detach();
            }

            if let Some(rounds) = std::env::var("SIFT_UI_BENCH")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
            {
                cx.spawn(async move |cx| uibench::run(window, rounds, cx).await)
                    .detach();
            }

            cx.on_action(|_: &Quit, cx| cx.quit());
            // Quitting normally removes the marker; a crash leaves it behind.
            cx.on_app_quit(move |_| {
                session.end();
                async {}
            })
            .detach();
            tracing::info!(
                ms = started.elapsed().as_secs_f64() * 1000.,
                "startup to window ready"
            );
        });
}

fn window_options(cx: &App) -> WindowOptions {
    let bounds = Bounds::centered(
        None,
        size(px(launcher::WIDTH), px(launcher::window_height(0, false))),
        cx,
    );
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: true,
        show: false,
        // A non-activating panel above other windows, on every Space and
        // over full-screen apps.
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Opaque,
        app_id: Some("io.github.kylianmarceau.sift".into()),
        ..Default::default()
    }
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, Some("Launcher")),
        KeyBinding::new("ctrl-n", SelectNext, Some("Launcher")),
        KeyBinding::new("up", SelectPrevious, Some("Launcher")),
        KeyBinding::new("ctrl-p", SelectPrevious, Some("Launcher")),
        KeyBinding::new("enter", Confirm, Some("Launcher")),
        KeyBinding::new("cmd-enter", RevealSelected, Some("Launcher")),
        KeyBinding::new("cmd-d", ToggleFavorite, Some("Launcher")),
        KeyBinding::new("cmd-y", QuickLookSelected, Some("Launcher")),
        KeyBinding::new("ctrl-x", ForgetSelected, Some("Launcher")),
        KeyBinding::new("cmd-w", settings::CloseSettings, Some("Settings")),
        KeyBinding::new("escape", Dismiss, Some("Launcher")),
        KeyBinding::new("cmd-w", Dismiss, Some("Launcher")),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("backspace", text::Backspace, Some("SearchInput")),
        KeyBinding::new("alt-backspace", text::DeleteWordLeft, Some("SearchInput")),
        KeyBinding::new("cmd-backspace", text::DeleteToStart, Some("SearchInput")),
        KeyBinding::new("delete", text::Delete, Some("SearchInput")),
        KeyBinding::new("left", text::Left, Some("SearchInput")),
        KeyBinding::new("right", text::Right, Some("SearchInput")),
        KeyBinding::new("home", text::Home, Some("SearchInput")),
        KeyBinding::new("cmd-left", text::Home, Some("SearchInput")),
        KeyBinding::new("end", text::End, Some("SearchInput")),
        KeyBinding::new("cmd-right", text::End, Some("SearchInput")),
        KeyBinding::new("cmd-a", text::SelectAll, Some("SearchInput")),
        KeyBinding::new("cmd-v", text::Paste, Some("SearchInput")),
        KeyBinding::new("cmd-c", text::Copy, Some("SearchInput")),
        KeyBinding::new("cmd-x", text::Cut, Some("SearchInput")),
    ]);
}

/// `SIFT_LOG=debug|info|warn` (default `info`): startup, catalog and
/// hotkey-latency timings are logged at info.
fn log_level() -> tracing::Level {
    match std::env::var("SIFT_LOG").as_deref() {
        Ok("debug") => tracing::Level::DEBUG,
        Ok("warn") => tracing::Level::WARN,
        Ok("error") => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    }
}
