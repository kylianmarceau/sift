//! Checks the Quick Look wiring without a launcher: creates an off-screen
//! key panel, attaches Quick Look and asks the preview panel which
//! controller it found in the responder chain. With `--show`, the preview
//! of the given file is also shown for two seconds.
//!
//! cargo run -p sift-macos --example quicklook_check -- <file> [--show]

#[cfg(target_os = "macos")]
fn main() {
    use std::ptr::NonNull;

    use objc2::MainThreadMarker;
    use objc2::MainThreadOnly;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSPanel,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoop, NSSize};

    let mut args = std::env::args().skip(1);
    let file = std::path::PathBuf::from(args.next().expect("usage: quicklook_check <file>"));
    let show = args.any(|a| a == "--show");
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // Off-screen and non-activating, like the launcher panel.
    let frame = NSRect::new(
        NSPoint::new(-10_000.0, -10_000.0),
        NSSize::new(200.0, 100.0),
    );
    let window = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Titled | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: the window is owned by this function only.
    unsafe { window.setReleasedWhenClosed(false) };
    window.makeKeyAndOrderFront(None);
    let view = window.contentView().expect("content view");
    let ptr = NonNull::from(&*view).cast();
    // SAFETY: `view` is a live NSView and this is the main thread.
    let panel = unsafe { sift_macos::Panel::from_ns_view(ptr) }.expect("panel");
    let quick_look = panel.quick_look();

    let spin = |seconds: f64| {
        let until = NSDate::dateWithTimeIntervalSinceNow(seconds);
        NSRunLoop::currentRunLoop().runUntilDate(&until);
    };
    spin(0.2);
    println!("launcher window is key: {}", window.isKeyWindow());
    {
        use objc2::runtime::{AnyObject, Sel};
        use objc2::{msg_send, sel};
        let selector: Sel = sel!(acceptsPreviewPanelControl:);
        // SAFETY: `targetForAction:` takes a selector and returns an
        // object or nil.
        let target: Option<objc2::rc::Retained<AnyObject>> =
            unsafe { msg_send![&app, targetForAction: selector] };
        println!(
            "targetForAction: {:?}",
            target.map(|t| t.class().name().to_owned())
        );
    }
    if show {
        quick_look.show(&file);
        spin(1.0);
        println!("preview visible: {}", quick_look.is_visible());
        println!("launcher still key: {}", window.isKeyWindow());
    } else {
        quick_look.prepare(&file);
    }
    println!(
        "controller found through the responder chain: {}",
        quick_look.is_controlling()
    );
    quick_look.close();
    drop(quick_look);
    window.close();
}

#[cfg(not(target_os = "macos"))]
fn main() {}
