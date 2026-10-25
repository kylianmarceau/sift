//! Native window manager. Main-thread AppKit notifications feed a model worker;
//! each target application has its own AX run loop with a 50 ms message timeout.
//! Nothing is started until explicitly enabled. No AeroSpace executable needed.
mod ffi;
mod hooks;
mod mouse;
mod service;
mod worker;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, msg_send};
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication, NSScreen, NSWorkspace};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObjectProtocol, NSOperationQueue, NSString,
};
use sift_wm::{Command, Config, Monitor, Rect, Snapshot};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, mpsc};

pub use service::Controller;

type Observation = (
    Retained<NSNotificationCenter>,
    Retained<ProtocolObject<dyn NSObjectProtocol>>,
);

#[derive(Debug, Clone)]
pub(crate) struct AppInfo {
    pid: i32,
    name: String,
    bundle: String,
    hidden: bool,
}

pub(crate) enum Event {
    Applications(Vec<AppInfo>, Option<i32>, bool),
    Activated(Option<i32>, Option<AppInfo>),
    Refresh,
    Monitors(Vec<Monitor>),
    Reports,
    Command(Command, Option<mpsc::Sender<sift_wm::protocol::Reply>>),
    Config(Box<Config>),
    Hover(u32),
    MouseHold(bool),
    DragEnd(Option<mouse::Drag>),
    Shutdown,
}

/// Owned by the launcher on the main thread. Dropping unregisters AppKit
/// observers and requests restoration without waiting on a hung application.
pub struct Runtime {
    controller: Controller,
    observations: Vec<Observation>,
    mouse: Option<mouse::Monitor>,
}

impl Runtime {
    pub fn start(
        config: Config,
        support: &Path,
        config_path: &Path,
        publish: impl Fn(Arc<Snapshot>) + Send + 'static,
    ) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("window manager must start on the main thread")?;
        if !trusted() {
            return Err("Allow Sift in Privacy & Security → Device Control and Data Access (macOS 27), or Accessibility (older macOS), then enable Window Management again.".into());
        }
        config.validate().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::sync_channel(256);
        let controller = service::start(
            config,
            support,
            config_path,
            monitors(mtm),
            applications(),
            (tx.clone(), rx),
            publish,
        )?;
        let mut observations = Vec::new();
        let workspace_center = NSWorkspace::sharedWorkspace().notificationCenter();
        for name in [
            "NSWorkspaceDidLaunchApplicationNotification",
            "NSWorkspaceDidTerminateApplicationNotification",
            "NSWorkspaceDidActivateApplicationNotification",
            "NSWorkspaceDidUnhideApplicationNotification",
            "NSWorkspaceDidHideApplicationNotification",
            "NSWorkspaceDidWakeNotification",
            "NSWorkspaceActiveSpaceDidChangeNotification",
        ] {
            let sender = tx.clone();
            let block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
                if name == "NSWorkspaceDidActivateApplicationNotification" {
                    // Showing Sift activates our own app. Avoid enumerating
                    // every application's Launch Services metadata on that
                    // latency-sensitive main-thread notification.
                    let app = NSWorkspace::sharedWorkspace().frontmostApplication();
                    let pid = app.as_ref().map(|app| app.processIdentifier());
                    let info = app.as_ref().and_then(|app| app_info(app));
                    let _ = sender.try_send(Event::Activated(pid, info));
                    return;
                }
                let (apps, frontmost) = applications();
                let _ = sender.try_send(Event::Applications(apps, frontmost, true));
            });
            // SAFETY: delivery is explicitly on the main queue; only plain
            // owned app metadata crosses into the model service.
            let token = unsafe {
                workspace_center.addObserverForName_object_queue_usingBlock(
                    Some(&NSString::from_str(name)),
                    None,
                    Some(&NSOperationQueue::mainQueue()),
                    &block,
                )
            };
            observations.push((workspace_center.clone(), token));
        }
        let center = NSNotificationCenter::defaultCenter();
        let block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
            if let Some(mtm) = MainThreadMarker::new() {
                let _ = tx.try_send(Event::Monitors(monitors(mtm)));
            }
        });
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(&NSString::from_str(
                    "NSApplicationDidChangeScreenParametersNotification",
                )),
                None,
                Some(&NSOperationQueue::mainQueue()),
                &block,
            )
        };
        observations.push((center, token));
        let mut runtime = Self {
            controller,
            observations,
            mouse: None,
        };
        runtime.sync_mouse_monitor()?;
        Ok(runtime)
    }

    pub fn controller(&self) -> Controller {
        self.controller.clone()
    }

    pub fn sync_mouse_monitor(&mut self) -> Result<(), String> {
        MainThreadMarker::new().ok_or("mouse monitor must be updated on the main thread")?;
        let config = self.controller.config().mouse;
        let enabled = self.controller.snapshot().enabled
            && (config.focus_follows_mouse || config.drag_to_rearrange);
        if enabled
            && self
                .mouse
                .as_ref()
                .is_some_and(|monitor| monitor.config == config)
        {
            return Ok(());
        }
        self.mouse = None;
        if enabled {
            self.mouse = Some(mouse::Monitor::start(self.controller.clone(), config)?);
        }
        Ok(())
    }

    /// Event-driven recovery on explicit use, also picks up windows whose
    /// application omitted a notification. Never called per keystroke.
    pub fn refresh(&self) {
        // Cached application ownership is maintained by workspace events.
        // Refresh AX metadata asynchronously without doing native enumeration
        // on the launcher's hotkey-to-first-frame path.
        let _ = self.controller.tx.try_send(Event::Refresh);
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.mouse = None;
        for (center, token) in self.observations.drain(..) {
            let observer: &objc2::runtime::AnyObject = (*token).as_ref();
            unsafe {
                center.removeObserver(observer);
            }
        }
        self.controller.shutdown();
    }
}

pub fn trusted() -> bool {
    unsafe { ffi::AXIsProcessTrusted() }
}

/// Called only after an explicit click. It opens Apple's own permission prompt.
pub fn request_accessibility() -> bool {
    if MainThreadMarker::new().is_none() {
        return false;
    }
    let key = NSString::from_str("AXTrustedCheckOptionPrompt");
    let value = objc2_foundation::NSNumber::new_bool(true);
    let options = objc2_foundation::NSDictionary::from_slices(&[&*key], &[&*value]);
    unsafe { ffi::AXIsProcessTrustedWithOptions(objc2::rc::Retained::as_ptr(&options).cast()) }
}

fn app_info(app: &NSRunningApplication) -> Option<AppInfo> {
    if app.processIdentifier() == std::process::id() as i32
        || app.isTerminated()
        || app.activationPolicy() != NSApplicationActivationPolicy::Regular
    {
        return None;
    }
    Some(AppInfo {
        hidden: app.isHidden(),
        pid: app.processIdentifier(),
        name: app
            .localizedName()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        bundle: app
            .bundleIdentifier()
            .map(|s| s.to_string())
            .unwrap_or_default(),
    })
}

fn applications() -> (Vec<AppInfo>, Option<i32>) {
    let workspace = NSWorkspace::sharedWorkspace();
    let apps = workspace
        .runningApplications()
        .iter()
        .filter_map(|a| app_info(&a))
        .collect();
    let front = workspace
        .frontmostApplication()
        .map(|a| a.processIdentifier());
    (apps, front)
}

fn monitors(mtm: MainThreadMarker) -> Vec<Monitor> {
    let screens = NSScreen::screens(mtm);
    let height = screens
        .firstObject()
        .map(|s| s.frame().size.height)
        .unwrap_or(0.);
    screens
        .iter()
        .enumerate()
        .map(|(index, screen)| {
            let convert = |r: objc2_foundation::NSRect| {
                Rect::new(
                    r.origin.x,
                    height - r.origin.y - r.size.height,
                    r.size.width,
                    r.size.height,
                )
            };
            let number = screen
                .deviceDescription()
                .objectForKey(&NSString::from_str("NSScreenNumber"));
            let id: u32 = number
                .map(|n| unsafe { msg_send![&*n, unsignedIntValue] })
                .unwrap_or(index as u32 + 1);
            Monitor {
                id,
                name: screen.localizedName().to_string(),
                frame: convert(screen.frame()),
                usable: convert(screen.visibleFrame()),
                main: index == 0,
            }
        })
        .collect()
}
