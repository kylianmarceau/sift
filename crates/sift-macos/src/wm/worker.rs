use super::ffi::*;
use super::{AppInfo, Event};
use sift_wm::{Effect, Rect, Window, WindowId};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::ffi::{CString, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct Report {
    pub pid: i32,
    pub windows: Vec<Window>,
    /// Presence is independent of metadata: a timed-out frame read must not
    /// erase a live window's original recovery position from the model.
    pub present_ids: Vec<WindowId>,
    pub focused: Option<WindowId>,
    pub failure: Option<String>,
}

pub enum Task {
    Apply(Vec<Effect>),
    Refresh { focus: bool, hidden: bool },
    Restore(Vec<(WindowId, Rect)>),
    Stop,
}

#[derive(Default)]
struct Pending {
    frames: BTreeMap<WindowId, Rect>,
    forced: HashSet<WindowId>,
    focus: Option<WindowId>,
    raise: Option<WindowId>,
    actions: Vec<Effect>,
    refresh: bool,
    refresh_focus: bool,
    app_hidden: Option<bool>,
    stop: bool,
}

/// Only run-loop pointers cross threads; access is serialized with removal.
/// All AX references remain exclusively owned by Context on the worker thread.
struct Wake {
    run_loop: usize,
    source: usize,
}

struct Signals {
    wake: Arc<Mutex<Option<Wake>>>,
    dirty: AtomicBool,
    focus_dirty: AtomicBool,
}

impl Signals {
    fn signal(&self) {
        let wake = self.wake.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(wake) = &*wake {
            // SAFETY: the lock synchronizes signalling with run-loop teardown.
            unsafe {
                CFRunLoopSourceSignal(wake.source as Ref);
                CFRunLoopWakeUp(wake.run_loop as Ref);
            }
        }
    }
}

struct Dispatch {
    context: RefCell<Context>,
    signals: Arc<Signals>,
}

pub struct Worker {
    pub identity: (u64, u64),
    pending: Arc<Mutex<Pending>>,
    wake: Arc<Mutex<Option<Wake>>>,
    pub latest: Arc<Mutex<Option<Report>>>,
    done: mpsc::Receiver<()>,
}

impl Worker {
    pub fn start(app: AppInfo, events: mpsc::SyncSender<Event>) -> std::io::Result<Self> {
        let identity = process_identity(app.pid).ok_or_else(|| {
            std::io::Error::other("cannot identify the application's process lifetime")
        })?;
        let pending = Arc::new(Mutex::new(Pending::default()));
        let input = pending.clone();
        let wake = Arc::new(Mutex::new(None));
        let ready = wake.clone();
        let latest = Arc::new(Mutex::new(None));
        let output = latest.clone();
        let (done_tx, done) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("sift-ax-{}", app.pid))
            .spawn(move || {
                objc2::rc::autoreleasepool(|_| {
                    let context = Box::new(Context {
                        app,
                        app_element: None,
                        observer: None,
                        windows: BTreeMap::new(),
                        observed: HashSet::new(),
                        frames: BTreeMap::new(),
                        last_attempt: BTreeMap::new(),
                        pending: input,
                        events,
                        output,
                        signals: Arc::new(Signals {
                            wake: ready,
                            dirty: AtomicBool::new(true),
                            focus_dirty: AtomicBool::new(true),
                        }),
                        stopping: false,
                    });
                    run(context);
                });
                let _ = done_tx.send(());
            })?;
        Ok(Self {
            identity,
            pending,
            wake,
            latest,
            done,
        })
    }

    pub fn send(&self, task: Task) {
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            match task {
                Task::Stop => pending.stop = true,
                Task::Refresh { focus, hidden } => {
                    pending.refresh = true;
                    pending.refresh_focus |= focus;
                    pending.app_hidden = Some(hidden);
                }
                Task::Restore(frames) => {
                    for (id, frame) in frames {
                        pending.frames.insert(id, frame);
                        pending.forced.insert(id);
                    }
                    pending.focus = None;
                    pending.raise = None;
                }
                Task::Apply(effects) => {
                    for effect in effects {
                        match effect {
                            Effect::Frame(id, frame) => {
                                pending.frames.insert(id, frame);
                            }
                            Effect::Focus(id) => pending.focus = Some(id),
                            Effect::Raise(id) => pending.raise = Some(id),
                            effect => {
                                if pending.actions.len() < 32 {
                                    pending.actions.push(effect);
                                }
                            }
                        }
                    }
                }
            }
        }
        let guard = self.wake.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(wake) = &*guard {
            // SAFETY: the lock keeps both CF objects alive until signalled.
            unsafe {
                CFRunLoopSourceSignal(wake.source as Ref);
                CFRunLoopWakeUp(wake.run_loop as Ref);
            }
        }
    }

    pub fn wait_stopped(&self, deadline: Instant) {
        let _ = self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
    }
}

/// Native IDs and PIDs may be recycled. Recovery must match the exact process
/// lifetime, including across restarts of the computer.
pub(super) fn process_identity(pid: i32) -> Option<(u64, u64)> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if bytes != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    (info.pbi_pid == pid as u32 && info.pbi_start_tvsec != 0)
        .then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.send(Task::Stop);
    }
}

struct Context {
    app: AppInfo,
    app_element: Option<Owned>,
    observer: Option<Owned>,
    windows: BTreeMap<WindowId, Owned>,
    observed: HashSet<WindowId>,
    frames: BTreeMap<WindowId, Rect>,
    last_attempt: BTreeMap<WindowId, (Rect, Instant)>,
    pending: Arc<Mutex<Pending>>,
    events: mpsc::SyncSender<Event>,
    output: Arc<Mutex<Option<Report>>>,
    signals: Arc<Signals>,
    stopping: bool,
}

fn run(mut context: Box<Context>) {
    // All CF and AX objects are owned for this run loop's lifetime. Observer
    // callbacks reference separate atomic signals, never the mutable AX state.
    unsafe {
        context.app_element = Owned::new(AXUIElementCreateApplication(context.app.pid));
        let Some(element) = &context.app_element else {
            return;
        };
        AXUIElementSetMessagingTimeout(element.0, 0.05);
        let element = element.0;
        let mut observer = std::ptr::null();
        if AXObserverCreate(context.app.pid, notification, &mut observer) == 0 {
            context.observer = Owned::new(observer);
        }
        let signals = context.signals.clone();
        let dispatch = Box::new(Dispatch {
            context: RefCell::new(*context),
            signals: signals.clone(),
        });
        let ptr = (&*dispatch as *const Dispatch).cast_mut().cast::<c_void>();
        let run_loop = CFRunLoopGetCurrent();
        let mut source_context = SourceContext {
            version: 0,
            info: ptr,
            retain: None,
            release: None,
            copy_description: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(perform),
        };
        let Some(source) = Owned::new(CFRunLoopSourceCreate(
            std::ptr::null(),
            0,
            &mut source_context,
        )) else {
            return;
        };
        CFRunLoopAddSource(run_loop, source.0, kCFRunLoopDefaultMode);
        if !observer.is_null() {
            CFRunLoopAddSource(
                run_loop,
                AXObserverGetRunLoopSource(observer),
                kCFRunLoopDefaultMode,
            );
            for name in [
                "AXWindowCreated",
                "AXFocusedWindowChanged",
                "AXMainWindowChanged",
                "AXApplicationActivated",
            ] {
                let key = Owned::string(name);
                AXObserverAddNotification(
                    observer,
                    element,
                    key.0,
                    Arc::as_ptr(&signals).cast_mut().cast(),
                );
            }
        }
        *signals.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(Wake {
            run_loop: run_loop as usize,
            source: source.0 as usize,
        });
        CFRunLoopSourceSignal(source.0);
        // AXWindows can transiently return an empty array for an application
        // that already owns native windows, without sending WindowCreated
        // after the initial handshake. Retry only during bounded startup;
        // established workers block indefinitely awaiting notifications.
        for delay in [0.05, 0.2, 1.0] {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, delay, false);
            let (stopping, ready) = {
                let state = dispatch.context.borrow();
                (state.stopping, !state.frames.is_empty())
            };
            if stopping || ready {
                break;
            }
            signals.dirty.store(true, Ordering::Relaxed);
            CFRunLoopSourceSignal(source.0);
        }
        let stopping = dispatch.context.borrow().stopping;
        if !stopping {
            CFRunLoopRun();
        }
        *signals.wake.lock().unwrap_or_else(|e| e.into_inner()) = None;
        CFRunLoopRemoveSource(run_loop, source.0, kCFRunLoopDefaultMode);
        if !observer.is_null() {
            CFRunLoopRemoveSource(
                run_loop,
                AXObserverGetRunLoopSource(observer),
                kCFRunLoopDefaultMode,
            );
        }
    }
}

unsafe extern "C" fn notification(_observer: Ref, _element: Ref, name: Ref, ptr: MutRef) {
    if ptr.is_null() {
        return;
    }
    // AX messaging can re-enter a run loop. This context is deliberately
    // independent of Context's RefCell borrow while remote AX calls execute.
    let signals = unsafe { &*ptr.cast::<Signals>() };
    if matches!(
        string(name).as_str(),
        "AXFocusedWindowChanged" | "AXApplicationActivated"
    ) {
        signals.focus_dirty.store(true, Ordering::Relaxed);
    }
    signals.dirty.store(true, Ordering::Relaxed);
    signals.signal();
}

unsafe extern "C" fn perform(ptr: MutRef) {
    let dispatch = unsafe { &*ptr.cast::<Dispatch>() };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(mut context) = dispatch.context.try_borrow_mut() else {
            dispatch.signals.dirty.store(true, Ordering::Relaxed);
            return;
        };
        objc2::rc::autoreleasepool(|_| context.perform());
        let pending = context.pending.lock().unwrap_or_else(|e| e.into_inner());
        let pending = pending.stop
            || pending.refresh
            || !pending.frames.is_empty()
            || pending.focus.is_some()
            || pending.raise.is_some()
            || !pending.actions.is_empty();
        if !context.stopping && (pending || dispatch.signals.dirty.load(Ordering::Relaxed)) {
            dispatch.signals.signal();
        }
    }));
    if result.is_err() {
        tracing::error!("AX worker panicked; stopping its run loop");
        unsafe {
            CFRunLoopStop(CFRunLoopGetCurrent());
        }
    }
}

impl Context {
    fn publish(&self, report: Report) {
        *self.output.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
        // A full service channel is fine: it drains every worker's latest
        // report on each turn, so events coalesce rather than accumulating.
        let _ = self.events.try_send(Event::Reports);
    }

    fn perform(&mut self) {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        if let Some(hidden) = pending.app_hidden {
            self.app.hidden = hidden;
        }
        for (id, frame) in pending.frames {
            if pending.forced.contains(&id) {
                self.last_attempt.remove(&id);
            }
            self.apply(Effect::Frame(id, frame));
        }
        if let Some(id) = pending.raise {
            self.apply(Effect::Raise(id));
        }
        if let Some(id) = pending.focus {
            self.apply(Effect::Focus(id));
        }
        for effect in pending.actions {
            self.apply(effect);
        }
        if pending.stop {
            self.stopping = true;
            unsafe {
                CFRunLoopStop(CFRunLoopGetCurrent());
            }
            return;
        }
        if pending.refresh {
            self.signals.dirty.store(true, Ordering::Relaxed);
            if pending.refresh_focus {
                self.signals.focus_dirty.store(true, Ordering::Relaxed);
            }
        }
        if self.stopping || !self.signals.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        self.scan();
    }

    fn scan(&mut self) {
        let Some(app) = self.app_element.as_ref().map(|a| a.0) else {
            return;
        };
        let Some(windows) = attribute(app, "AXWindows") else {
            self.publish(Report {
                pid: self.app.pid,
                failure: Some(format!(
                    "{} did not answer Accessibility; its windows were left alone",
                    self.app.name
                )),
                ..Report::default()
            });
            return;
        };
        if unsafe { CFGetTypeID(windows.0) != CFArrayGetTypeID() } {
            return;
        }
        let count = unsafe { CFArrayGetCount(windows.0) }.clamp(0, 4096);
        tracing::debug!(pid = self.app.pid, count, "AX window scan");
        let mut next = BTreeMap::new();
        let mut list = Vec::new();
        let mut present_ids = Vec::with_capacity(count as usize);
        for index in 0..count {
            if self.pending.lock().unwrap_or_else(|e| e.into_inner()).stop {
                return;
            }
            let element = unsafe { CFArrayGetValueAtIndex(windows.0, index) };
            if element.is_null() {
                continue;
            }
            let Some(id) = window_id(element).or_else(|| {
                self.windows
                    .iter()
                    .find(|(_, old)| unsafe { CFEqual(old.0, element) })
                    .map(|(id, _)| *id)
            }) else {
                tracing::debug!(pid = self.app.pid, "AX window has no native identity");
                continue;
            };
            present_ids.push(id);
            unsafe {
                AXUIElementSetMessagingTimeout(element, 0.05);
            }
            if let Some(owned) = unsafe { Owned::new(CFRetain(element)) } {
                next.insert(id, owned);
            }
            if !matches!(
                attribute(element, "AXRole").map(|v| string(v.0)).as_deref(),
                Some("AXWindow")
            ) {
                continue;
            }
            let Some(frame) = frame(element) else {
                tracing::debug!(pid = self.app.pid, id, "AX window has no usable frame");
                continue;
            };
            let title: String = attribute(element, "AXTitle")
                .map(|v| string(v.0))
                .unwrap_or_default()
                .chars()
                .take(1024)
                .collect();
            let subrole = attribute(element, "AXSubrole")
                .map(|v| string(v.0))
                .unwrap_or_default();
            let mut window = Window::new(id, self.app.pid, frame);
            window.title = title;
            window.app_name.clone_from(&self.app.name);
            window.app_id.clone_from(&self.app.bundle);
            window.minimized = boolean(element, "AXMinimized");
            window.native_fullscreen = boolean(element, "AXFullScreen");
            window.hidden = self.app.hidden;
            // Dialogs, sheets and utility windows float; application rules
            // may override this decision in the pure engine.
            window.floating = !matches!(subrole.as_str(), "AXStandardWindow" | "")
                || attribute(element, "AXCloseButton").is_none();
            list.push(window);
            self.frames.insert(id, frame);
            if self.observed.insert(id) {
                if let Some(observer) = &self.observer {
                    let ptr = Arc::as_ptr(&self.signals).cast_mut().cast();
                    for name in [
                        "AXUIElementDestroyed",
                        "AXMoved",
                        "AXResized",
                        "AXTitleChanged",
                        "AXWindowMiniaturized",
                        "AXWindowDeminiaturized",
                    ] {
                        let key = Owned::string(name);
                        unsafe {
                            AXObserverAddNotification(observer.0, element, key.0, ptr);
                        }
                    }
                }
            }
        }
        self.windows = next;
        self.observed.retain(|id| self.windows.contains_key(id));
        self.frames.retain(|id, _| self.windows.contains_key(id));
        self.last_attempt
            .retain(|id, _| self.windows.contains_key(id));
        let focused = if self.signals.focus_dirty.swap(false, Ordering::Relaxed) {
            attribute(app, "AXFocusedWindow").and_then(|w| window_id(w.0))
        } else {
            None
        };
        self.publish(Report {
            pid: self.app.pid,
            windows: list,
            present_ids,
            focused,
            failure: None,
        });
    }

    fn apply(&mut self, effect: Effect) {
        let id = match &effect {
            Effect::Frame(id, _)
            | Effect::Focus(id)
            | Effect::Raise(id)
            | Effect::Close(id)
            | Effect::Minimize(id) => *id,
            Effect::NativeFullscreen(id, _) => *id,
            _ => return,
        };
        let Some(window) = self.windows.get(&id).map(|w| w.0) else {
            return;
        };
        match effect {
            Effect::Frame(_, rect) => {
                if !rect.valid() || self.frames.get(&id).is_some_and(|f| f.nearly(rect)) {
                    return;
                }
                if self.last_attempt.get(&id).is_some_and(|(r, at)| {
                    r.nearly(rect) && at.elapsed() < Duration::from_millis(250)
                }) {
                    return;
                }
                self.last_attempt.insert(id, (rect, Instant::now()));
                let point = Point {
                    x: rect.x,
                    y: rect.y,
                };
                let size = Size {
                    width: rect.width,
                    height: rect.height,
                };
                // A second size set handles applications that constrain size
                // using the old monitor before accepting the new position.
                set_value(window, "AXSize", 2, &size as *const Size as Ref);
                set_value(window, "AXPosition", 1, &point as *const Point as Ref);
                set_value(window, "AXSize", 2, &size as *const Size as Ref);
                self.frames.insert(id, rect);
            }
            Effect::Focus(_) => {
                let minimized = Owned::string("AXMinimized");
                unsafe {
                    AXUIElementSetAttributeValue(window, minimized.0, kCFBooleanFalse);
                }
                // AXRaise changes stacking, but does not reliably activate an
                // inactive application. NSRunningApplication supports background
                // callers; request activation before selecting the AX window.
                if let Some(app) =
                    objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(
                        self.app.pid,
                    )
                {
                    let accepted =
                        app.activateWithOptions(
                            objc2_app_kit::NSApplicationActivationOptions::empty(),
                        );
                    tracing::debug!(
                        pid = self.app.pid,
                        accepted,
                        "application activation requested"
                    );
                }
                let key = Owned::string("AXFrontmost");
                if let Some(app) = &self.app_element {
                    let hidden = Owned::string("AXHidden");
                    unsafe {
                        AXUIElementSetAttributeValue(app.0, hidden.0, kCFBooleanFalse);
                        AXUIElementSetAttributeValue(app.0, key.0, kCFBooleanTrue);
                    }
                }
                action(window, "AXRaise");
                let main = Owned::string("AXMain");
                let focused = Owned::string("AXFocused");
                unsafe {
                    let main = AXUIElementSetAttributeValue(window, main.0, kCFBooleanTrue);
                    let focused = AXUIElementSetAttributeValue(window, focused.0, kCFBooleanTrue);
                    tracing::debug!(id, main, focused, "window focus attributes requested");
                }
            }
            Effect::Raise(_) => action(window, "AXRaise"),
            Effect::Close(_) => {
                if let Some(button) = attribute(window, "AXCloseButton") {
                    action(button.0, "AXPress");
                }
            }
            Effect::Minimize(_) => {
                let key = Owned::string("AXMinimized");
                unsafe {
                    AXUIElementSetAttributeValue(window, key.0, kCFBooleanTrue);
                }
            }
            Effect::NativeFullscreen(_, on) => {
                let key = Owned::string("AXFullScreen");
                unsafe {
                    AXUIElementSetAttributeValue(
                        window,
                        key.0,
                        if on { kCFBooleanTrue } else { kCFBooleanFalse },
                    );
                }
            }
            _ => {}
        }
    }
}

fn action(element: Ref, name: &str) {
    let key = Owned::string(name);
    unsafe {
        AXUIElementPerformAction(element, key.0);
    }
}
fn set_value(element: Ref, name: &str, kind: u32, value: Ref) {
    let key = Owned::string(name);
    if let Some(value) = unsafe { Owned::new(AXValueCreate(kind, value)) } {
        let code = unsafe { AXUIElementSetAttributeValue(element, key.0, value.0) };
        if code != 0 {
            tracing::debug!(code, attribute = name, "window rejected a geometry change");
        }
    }
}
pub(super) fn attribute(element: Ref, name: &str) -> Option<Owned> {
    let key = Owned::string(name);
    let mut value = std::ptr::null();
    if unsafe { AXUIElementCopyAttributeValue(element, key.0, &mut value) } != 0 {
        return None;
    }
    unsafe { Owned::new(value) }
}
fn boolean(element: Ref, name: &str) -> bool {
    attribute(element, name).is_some_and(|v| unsafe { CFEqual(v.0, kCFBooleanTrue) })
}
pub(super) fn frame(element: Ref) -> Option<Rect> {
    let pos = attribute(element, "AXPosition")?;
    let size = attribute(element, "AXSize")?;
    let mut p = Point::default();
    let mut s = Size::default();
    unsafe {
        if AXValueGetType(pos.0) != 1 || AXValueGetType(size.0) != 2 {
            return None;
        }
        if !AXValueGetValue(pos.0, 1, (&mut p as *mut Point).cast())
            || !AXValueGetValue(size.0, 2, (&mut s as *mut Size).cast())
        {
            return None;
        }
    }
    let rect = Rect::new(p.x, p.y, s.width, s.height);
    rect.valid().then_some(rect)
}

/// AX has no public stable window-id accessor. Like AeroSpace, use the
/// system's AX->CG bridge. Resolve dynamically and fail closed if unavailable.
pub(super) fn window_id(element: Ref) -> Option<u32> {
    type GetId = unsafe extern "C" fn(Ref, *mut u32) -> i32;
    static FUNCTION: std::sync::OnceLock<Option<GetId>> = std::sync::OnceLock::new();
    let get = FUNCTION
        .get_or_init(|| {
            let name = CString::new("_AXUIElementGetWindow").expect("literal");
            let address = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
            if address.is_null() {
                None
            } else {
                Some(unsafe { std::mem::transmute::<*mut c_void, GetId>(address) })
            }
        })
        .as_ref()?;
    let mut id = 0;
    (unsafe { get(element, &mut id) } == 0 && id != 0).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_identity_reads_the_native_process_lifetime_and_rejects_missing_processes() {
        let pid = std::process::id() as i32;
        let identity = process_identity(pid).expect("current process lifetime");
        assert!(identity.0 > 0);
        assert_eq!(process_identity(pid), Some(identity));
        assert_eq!(process_identity(-1), None);
    }

    #[test]
    fn geometry_mailbox_coalesces_events_and_preserves_shutdown_restoration() {
        let (_, done) = mpsc::channel();
        let worker = Worker {
            identity: (1, 0),
            pending: Arc::default(),
            wake: Arc::default(),
            latest: Arc::default(),
            done,
        };
        for x in 0..10_000 {
            worker.send(Task::Apply(vec![Effect::Frame(
                1,
                Rect::new(x as f64, 0., 400., 300.),
            )]));
        }
        assert_eq!(worker.pending.lock().expect("pending").frames.len(), 1);
        worker.send(Task::Restore(vec![(1, Rect::new(100., 100., 400., 300.))]));
        worker.send(Task::Stop);
        let pending = worker.pending.lock().expect("pending");
        assert_eq!(pending.frames[&1].x, 100.);
        assert!(pending.forced.contains(&1));
        assert!(pending.stop);
    }
}
