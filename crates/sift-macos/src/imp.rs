use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AllocAnyThread, MainThreadMarker, Message, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBitmapImageFileType, NSBitmapImageRep,
    NSCompositingOperation, NSDeviceRGBColorSpace, NSEvent, NSGraphicsContext,
    NSRunningApplication, NSScreen, NSView, NSWindow, NSWindowButton, NSWindowTitleVisibility,
    NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{NSDictionary, NSError, NSPoint, NSRect, NSSize, NSString, NSURL};

use crate::{LaunchError, validate_app_bundle};

/// Renders the icon Finder shows for `path` (any file, folder or bundle)
/// into a `px`×`px` PNG.
///
/// Drawing into a fixed-size bitmap makes AppKit pick the best
/// representation for that size, instead of encoding the 1024 px master
/// image. A 64 px icon is a few kilobytes.
pub fn app_icon_png(path: &Path, px: u32) -> Option<Vec<u8>> {
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
