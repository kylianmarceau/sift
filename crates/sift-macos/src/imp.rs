use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AllocAnyThread, MainThreadMarker, Message, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBitmapImageFileType, NSBitmapImageRep,
    NSCompositingOperation, NSDeviceRGBColorSpace, NSEvent, NSGraphicsContext, NSModalResponse,
    NSOpenPanel, NSPasteboard, NSPasteboardTypeString, NSRunningApplication, NSScreen, NSView,
    NSWindow, NSWindowButton, NSWindowTitleVisibility, NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{
    NSArray, NSDate, NSDateFormatter, NSDictionary, NSError, NSPoint, NSRect, NSSize, NSString,
    NSURL,
};

use crate::{LaunchError, validate_app_bundle};

/// Renders the icon Finder shows for `path` (any file, folder or bundle)
/// into a `px`×`px` PNG.
///
/// Drawing into a fixed-size bitmap makes AppKit pick the best
/// representation for that size, instead of encoding the 1024 px master
/// image. A 64 px icon is a few kilobytes.
pub fn icon_png(path: &Path, px: u32) -> Option<Vec<u8>> {
    let path = NSString::from_str(path.to_str()?);
    let image = NSWorkspace::sharedWorkspace().iconForFile(&path);
    let side = px as isize;
    // SAFETY: a null `planes` pointer asks AppKit to allocate the pixel
    // buffer; the remaining arguments describe a standard 8-bit RGBA layout.
    let bitmap = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            side,
            side,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }?;
    let size = NSSize::new(px as f64, px as f64);
    bitmap.setSize(size);

    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    image.drawInRect_fromRect_operation_fraction(
        NSRect::new(NSPoint::new(0.0, 0.0), size),
        NSRect::ZERO,
        NSCompositingOperation::Copy,
        1.0,
    );
    context.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();

    // SAFETY: an empty properties dictionary is valid for PNG encoding.
    let data = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(data.to_vec())
}

/// Launches the application bundle at `path`, or brings it to the front if
/// it is already running, using `NSWorkspace`.
///
/// Validation errors are returned immediately. Launch failures reported by
/// macOS arrive asynchronously (on a background queue) through
/// `on_failure`.
pub fn open_application(
    path: &Path,
    on_failure: impl Fn(LaunchError) + Send + Sync + 'static,
) -> Result<(), LaunchError> {
    validate_app_bundle(path)?;
    let path_str = path
        .to_str()
        .ok_or_else(|| LaunchError::NotAnApp(path.to_path_buf()))?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
    let configuration = NSWorkspaceOpenConfiguration::configuration();
    configuration.setActivates(true);

    let owned = path.to_path_buf();
    let completion = RcBlock::new(
        move |_app: *mut NSRunningApplication, error: *mut NSError| {
            // SAFETY: AppKit passes either null or a valid NSError for the
            // duration of the callback.
            if let Some(error) = unsafe { error.as_ref() } {
                let reason = error.localizedDescription().to_string();
                tracing::warn!(path = %owned.display(), %reason, "launch failed");
                on_failure(LaunchError::Failed {
                    path: owned.clone(),
                    reason,
                });
            }
        },
    );
    NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
        &url,
        &configuration,
        Some(&completion),
    );
    Ok(())
}

/// Opens a file or folder with its default application, as a double-click
/// in Finder would. Missing paths fail immediately; failures AppKit reports
/// later (no application for the file type, for example) go to
/// `on_failure` on a background queue.
pub fn open_path(
    path: &Path,
    on_failure: impl Fn(LaunchError) + Send + Sync + 'static,
) -> Result<(), LaunchError> {
    let url = file_url(path)?;
    open_with_default_app(&url, path.to_path_buf(), on_failure);
    Ok(())
}

/// Opens a text file in its default editor, or in TextEdit if no
/// application is registered for its type (as for `.toml` on a fresh
/// system).
pub fn open_text_file(
    path: &Path,
    on_failure: impl Fn(LaunchError) + Send + Sync + 'static,
) -> Result<(), LaunchError> {
    let url = file_url(path)?;
    let workspace = NSWorkspace::sharedWorkspace();
    if workspace.URLForApplicationToOpenURL(&url).is_some() {
        open_with_default_app(&url, path.to_path_buf(), on_failure);
        return Ok(());
    }
    let text_edit = file_url(Path::new(TEXT_EDIT))?;
    let configuration = NSWorkspaceOpenConfiguration::configuration();
    configuration.setActivates(true);
    let completion = failure_block(path.to_path_buf(), on_failure);
    workspace.openURLs_withApplicationAtURL_configuration_completionHandler(
        &NSArray::from_retained_slice(&[url]),
        &text_edit,
        &configuration,
        Some(&completion),
    );
    Ok(())
}

const TEXT_EDIT: &str = "/System/Applications/TextEdit.app";

/// The current local date and time formatted with a Unicode date pattern
/// (`yyyy-MM-dd`, `HH:mm`, ...).
pub fn format_local_now(pattern: &str) -> String {
    let formatter = NSDateFormatter::new();
    formatter.setDateFormat(Some(&NSString::from_str(pattern)));
    formatter.stringFromDate(&NSDate::now()).to_string()
}

/// Opens a URL (`https:`, `mailto:`, `x-apple.systempreferences:`, ...)
/// with its default handler. Only URLs with a scheme are accepted; text
/// that does not parse fails immediately.
pub fn open_url(
    url: &str,
    on_failure: impl Fn(LaunchError) + Send + Sync + 'static,
) -> Result<(), LaunchError> {
    let invalid = || LaunchError::BadUrl(url.to_owned());
    let parsed = NSURL::URLWithString(&NSString::from_str(url)).ok_or_else(invalid)?;
    if parsed.scheme().is_none_or(|s| s.length() == 0) {
        return Err(invalid());
    }
    open_with_default_app(&parsed, PathBuf::from(url), on_failure);
    Ok(())
}

fn open_with_default_app(
    url: &NSURL,
    label: PathBuf,
    on_failure: impl Fn(LaunchError) + Send + Sync + 'static,
) {
    let configuration = NSWorkspaceOpenConfiguration::configuration();
    configuration.setActivates(true);
    let completion = failure_block(label, on_failure);
    NSWorkspace::sharedWorkspace().openURL_configuration_completionHandler(
        url,
        &configuration,
        Some(&completion),
    );
}

/// A completion handler that reports AppKit's error, if any.
fn failure_block(
    label: PathBuf,
    on_failure: impl Fn(LaunchError) + Send + Sync + 'static,
) -> RcBlock<dyn Fn(*mut NSRunningApplication, *mut NSError)> {
    RcBlock::new(
        move |_app: *mut NSRunningApplication, error: *mut NSError| {
            // SAFETY: AppKit passes either null or a valid NSError for the
            // duration of the callback.
            if let Some(error) = unsafe { error.as_ref() } {
                let reason = error.localizedDescription().to_string();
                tracing::warn!(target = %label.display(), %reason, "open failed");
                on_failure(LaunchError::Failed {
                    path: label.clone(),
                    reason,
                });
            }
        },
    )
}

/// Pasteboard types that apps use to mark content that clipboard
/// managers should not keep (see nspasteboard.org): passwords from
/// password managers, and short-lived or machine-generated content.
const PRIVATE_TYPES: &[&str] = &[
    "org.nspasteboard.ConcealedType",
    "org.nspasteboard.TransientType",
    "org.nspasteboard.AutoGeneratedType",
];

/// A counter that increases whenever any app changes the clipboard. Cheap
/// to read; poll it and read the contents only when it changes.
pub fn clipboard_change_count() -> isize {
    NSPasteboard::generalPasteboard().changeCount()
}

/// Watches the clipboard's change counter on a background thread, so the
/// UI is woken only when something was copied.
///
/// The thread runs at background quality of service: macOS may run it on
/// efficiency cores and coalesce its timer with other wakeups. Dropping the
/// watcher stops the thread.
pub struct ClipboardWatcher {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardWatcher {
    /// Checks every `interval`; calls `on_change` (on the watcher thread)
    /// after each change. Changes made before starting are not reported.
    pub fn start(
        interval: std::time::Duration,
        on_change: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::Builder::new()
            .name("sift-clipboard".into())
            .spawn(move || {
                // SAFETY: sets this thread's own QoS class; no other
                // preconditions.
                unsafe {
                    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
                }
                let mut last = clipboard_change_count();
                while !stopped.load(Ordering::Relaxed) {
                    std::thread::park_timeout(interval);
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let count = clipboard_change_count();
                    if count != last {
                        last = count;
                        on_change();
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for ClipboardWatcher {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// What [`clipboard_text`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardText {
    Text(String),
    /// Marked as concealed, transient or auto-generated: not to be kept.
    Private,
    /// No plain text (an image or files, for example).
    None,
}

/// The clipboard's plain text, unless the copying app marked it private.
pub fn clipboard_text() -> ClipboardText {
    read_text(&NSPasteboard::generalPasteboard())
}

fn read_text(pasteboard: &NSPasteboard) -> ClipboardText {
    if let Some(types) = pasteboard.types() {
        let private = types
            .iter()
            .any(|t| PRIVATE_TYPES.contains(&t.to_string().as_str()));
        if private {
            return ClipboardText::Private;
        }
    }
    // SAFETY: NSPasteboardTypeString is an immutable framework constant.
    let kind = unsafe { NSPasteboardTypeString };
    match pasteboard.stringForType(kind) {
        Some(text) => ClipboardText::Text(text.to_string()),
        None => ClipboardText::None,
    }
}

/// Replaces the clipboard contents with `text` (plain text only).
pub fn copy_text(text: &str) -> Result<(), LaunchError> {
    if write_text(&NSPasteboard::generalPasteboard(), text) {
        Ok(())
    } else {
        Err(LaunchError::Clipboard)
    }
}

fn write_text(pasteboard: &NSPasteboard, text: &str) -> bool {
    pasteboard.clearContents();
    // SAFETY: NSPasteboardTypeString is an immutable framework constant.
    let kind = unsafe { NSPasteboardTypeString };
    pasteboard.setString_forType(&NSString::from_str(text), kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A private pasteboard, so tests never touch the user's clipboard.
    fn scratch() -> Retained<NSPasteboard> {
        NSPasteboard::pasteboardWithUniqueName()
    }

    #[test]
    fn reads_back_written_text() {
        let pasteboard = scratch();
        assert_eq!(read_text(&pasteboard), ClipboardText::None);
        let before = pasteboard.changeCount();
        assert!(write_text(&pasteboard, "héllo\nworld"));
        assert!(pasteboard.changeCount() > before);
        assert_eq!(
            read_text(&pasteboard),
            ClipboardText::Text("héllo\nworld".into())
        );
    }

    #[test]
    fn concealed_content_is_never_read() {
        let pasteboard = scratch();
        assert!(write_text(&pasteboard, "hunter2"));
        let marker = NSString::from_str("org.nspasteboard.ConcealedType");
        assert!(pasteboard.setString_forType(&NSString::from_str(""), &marker));
        assert_eq!(read_text(&pasteboard), ClipboardText::Private);
    }
}

/// Opens a Finder window with `path` selected.
pub fn reveal_in_finder(path: &Path) -> Result<(), LaunchError> {
    let url = file_url(path)?;
    let urls = NSArray::from_retained_slice(&[url]);
    NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&urls);
    Ok(())
}

fn file_url(path: &Path) -> Result<Retained<NSURL>, LaunchError> {
    if std::fs::symlink_metadata(path).is_err() {
        return Err(LaunchError::NotFound(path.to_path_buf()));
    }
    // APFS and HFS+ names are always UTF-8; other volumes rarely differ.
    let text = path.to_str().ok_or_else(|| LaunchError::Failed {
        path: path.to_path_buf(),
        reason: "the path is not valid UTF-8".into(),
    })?;
    Ok(NSURL::fileURLWithPath(&NSString::from_str(text)))
}

/// Shows the standard folder picker (non-modal) and calls `on_done` on the
/// main thread with the chosen folder, or `None` if cancelled. Returns false
/// if not called on the main thread.
///
/// Folders chosen here are also how macOS grants access to protected
/// locations, so picking `~/Documents` counts as the user's permission.
pub fn choose_folder(
    prompt: &str,
    message: &str,
    start_in: Option<&Path>,
    on_done: impl Fn(Option<PathBuf>) + 'static,
) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setAllowsMultipleSelection(false);
    panel.setPrompt(Some(&NSString::from_str(prompt)));
    panel.setMessage(Some(&NSString::from_str(message)));
    if let Some(dir) = start_in.and_then(|d| d.to_str()) {
        panel.setDirectoryURL(Some(&NSURL::fileURLWithPath(&NSString::from_str(dir))));
    }
    // An accessory app is not frontmost; bring the panel to the user.
    activate_app();
    let chosen = panel.clone();
    let handler = RcBlock::new(move |response: NSModalResponse| {
        // NSModalResponseOK is 1; Cancel is 0.
        let path = (response == 1)
            .then(|| chosen.URL())
            .flatten()
            .and_then(|url| url.path())
            .map(|path| PathBuf::from(path.to_string()));
        on_done(path);
    });
    panel.beginWithCompletionHandler(&handler);
    true
}

/// Makes Sift the active application, so a normal window (settings) comes
/// to the front even though Sift has no Dock icon. Main thread only.
pub fn activate_app() {
    if let Some(mtm) = MainThreadMarker::new() {
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    }
}

/// Runs the app as an accessory: no Dock icon and no menu bar, like
/// Spotlight. Bundles get the same from `LSUIElement`; this also covers
/// running the bare binary during development. Main thread only.
pub fn use_accessory_activation_policy() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    NSApplication::sharedApplication(mtm)
        .setActivationPolicy(NSApplicationActivationPolicy::Accessory)
}

/// The launcher's native window.
///
/// GPUI creates the window (a non-activating `NSPanel` for
/// `WindowKind::PopUp`); this wraps it to do what GPUI has no API for:
/// ordering it in and out without activating sift, placing it on the screen
/// the user is working on, and growing it downwards from a fixed top edge.
#[derive(Clone)]
pub struct Panel {
    window: Retained<NSWindow>,
    /// GPUI's Metal-backed view (a subview of the window's content view).
    view: Retained<NSView>,
    mtm: MainThreadMarker,
}

impl Panel {
    /// Wraps the window that owns `ns_view` (from GPUI's
    /// `raw_window_handle::AppKitWindowHandle`).
    ///
    /// # Safety
    ///
    /// `ns_view` must point to a live `NSView`, and this must run on the
    /// main thread.
    pub unsafe fn from_ns_view(ns_view: NonNull<c_void>) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        // SAFETY: guaranteed by the caller.
        let view: &NSView = unsafe { ns_view.cast::<NSView>().as_ref() };
        let window = view.window()?;
        Some(Self {
            window,
            view: view.retain(),
            mtm,
        })
    }

    /// Turns GPUI's titled panel into a plain launcher surface: no title,
    /// no traffic-light buttons, not draggable. The titled style is kept on
    /// purpose, because it gives the window macOS's native rounded corners
    /// and shadow.
    pub fn configure(&self) {
        let window = &self.window;
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        window.setTitlebarAppearsTransparent(true);
        for button in [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ] {
            if let Some(button) = window.standardWindowButton(button) {
                button.setHidden(true);
            }
        }
        window.setMovable(false);
        window.setHidesOnDeactivate(false);
    }

    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    /// Whether the panel has keyboard focus.
    pub fn is_key(&self) -> bool {
        self.window.isKeyWindow()
    }

    pub(crate) fn window(&self) -> Retained<NSWindow> {
        self.window.clone()
    }

    pub(crate) fn mtm(&self) -> MainThreadMarker {
        self.mtm
    }

    /// Places the panel horizontally centred, its top edge 28% below the
    /// top of the screen under the mouse pointer, and makes it key so
    /// typing goes to it. Because the panel is non-activating, the app the
    /// user was in stays active, and focus returns to it when the panel
    /// hides.
    pub fn show(&self) {
        if let Some(frame) = self.target_screen_frame() {
            let size = self.window.frame().size;
            let x = frame.origin.x + (frame.size.width - size.width) / 2.0;
            let top = frame.origin.y + frame.size.height * 0.72;
            self.window
                .setFrameTopLeftPoint(NSPoint::new(x.round(), top.round()));
        }
        self.window.makeKeyAndOrderFront(None);
    }

    /// Draws a frame right now instead of waiting for GPUI's display link.
    ///
    /// GPUI stops its display link while the window is hidden, and starts it
    /// again only when AppKit reports the window visible. That report is
    /// asynchronous and arrives tens of milliseconds after the window is
    /// shown. Asking the view's layer to display makes GPUI draw immediately
    /// (through its `displayLayer:` handler).
    ///
    /// Like [`Panel::set_height`], call this from a GPUI foreground task,
    /// never from inside a GPUI update or render.
    pub fn draw_now(&self) {
        // SAFETY: `-[NSView layer]` returns the backing CALayer or nil, and
        // `-[CALayer display]` takes no arguments and returns nothing.
        unsafe {
            let layer: Option<Retained<AnyObject>> = msg_send![&*self.view, layer];
            if let Some(layer) = layer {
                let _: () = msg_send![&*layer, display];
            }
        }
    }

    pub fn hide(&self) {
        self.window.orderOut(None);
    }

    /// Resizes the window to `height` points, keeping its top edge in place
    /// (AppKit's origin is the bottom-left corner, so a plain resize would
    /// grow upwards).
    ///
    /// Call this from a GPUI foreground task, never from inside a GPUI
    /// update or render. AppKit reports the new size back to GPUI
    /// synchronously, and GPUI drops that report if its app state is
    /// already borrowed. The window would then keep drawing at the old
    /// size, leaving the rest of the window black.
    pub fn set_height(&self, height: f64) {
        let frame = self.window.frame();
        if (frame.size.height - height).abs() < 0.5 {
            return;
        }
        let top = frame.origin.y + frame.size.height;
        let new = NSRect::new(
            NSPoint::new(frame.origin.x, top - height),
            NSSize::new(frame.size.width, height),
        );
        self.window.setFrame_display(new, true);
    }

    /// Visible frame (excluding menu bar and Dock) of the screen containing
    /// the mouse pointer, falling back to the main screen.
    fn target_screen_frame(&self) -> Option<NSRect> {
        let mouse = NSEvent::mouseLocation();
        let screens = NSScreen::screens(self.mtm);
        let contains = |r: NSRect| {
            mouse.x >= r.origin.x
                && mouse.x < r.origin.x + r.size.width
                && mouse.y >= r.origin.y
                && mouse.y < r.origin.y + r.size.height
        };
        screens
            .iter()
            .find(|s| contains(s.frame()))
            .or_else(|| NSScreen::mainScreen(self.mtm))
            .map(|s| s.visibleFrame())
    }
}
