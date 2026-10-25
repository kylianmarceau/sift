//! Three disposable windows for wm_native_check. Never manages other apps.
#[cfg(target_os = "macos")]
fn main() {
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask, NSScreen,
        NSWindow, NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};
    let directory = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let resources = std::env::current_exe()
                .expect("executable")
                .parent()
                .expect("MacOS")
                .parent()
                .expect("Contents")
                .join("Resources/fixture-directory");
            std::path::PathBuf::from(std::fs::read_to_string(resources).expect("fixture directory"))
        });
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();
    let mut windows = Vec::new();
    let create = |index: usize| {
        // SAFETY: called on the main thread with a valid frame and style.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(
                    NSPoint::new(100. + index as f64 * 50., 150. + index as f64 * 40.),
                    NSSize::new(400., 300.),
                ),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setTitle(&NSString::from_str(&format!("Sift WM Test {index}")));
        window.makeKeyAndOrderFront(None);
        window
    };
    for index in 0..3 {
        windows.push(create(index));
    }
    let height = NSScreen::screens(mtm)
        .firstObject()
        .expect("screen")
        .frame()
        .size
        .height;
    let frames = |windows: &[objc2::rc::Retained<NSWindow>]| {
        windows.iter().filter(|w| w.isVisible() || w.isMiniaturized()).map(|w| {
            let r = w.frame();
            serde_json::json!({"id": w.windowNumber(), "frame": {"x": r.origin.x, "y": height-r.origin.y-r.size.height,
                "width":r.size.width, "height":r.size.height}, "minimized":w.isMiniaturized(),
                "focused":w.isKeyWindow() && app.isActive()})
        }).collect::<Vec<_>>()
    };
    std::fs::write(
        directory.join("original.json"),
        serde_json::to_vec(&frames(&windows)).expect("json"),
    )
    .expect("original frames");
    loop {
        // NSRunLoop alone services AX requests but never dispatches queued
        // WindowServer activation/key-window events. Exercise a real AppKit
        // event pump so focus assertions reflect what the native app sees.
        while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&NSDate::distantPast()),
            unsafe { NSDefaultRunLoopMode },
            true,
        ) {
            app.sendEvent(&event);
        }
        app.updateWindows();
        if let Ok(command) = std::fs::read_to_string(directory.join("fixture-command")) {
            let _ = std::fs::remove_file(directory.join("fixture-command"));
            match command.trim() {
                "quit" => break,
                "minimize" => {
                    if let Some(w) = windows.first() {
                        w.miniaturize(None);
                    }
                }
                "restore" => {
                    if let Some(w) = windows.first() {
                        w.deminiaturize(None);
                    }
                }
                "close" => {
                    if !windows.is_empty() {
                        windows.remove(0).close();
                    }
                }
                "add" => windows.push(create(windows.len() + 10)),
                "reopen" => windows.push(create(0)),
                "nudge" => {
                    if let Some(window) = windows.first() {
                        let frame = window.frame();
                        window.setFrameOrigin(NSPoint::new(frame.origin.x + 25., frame.origin.y));
                    }
                }
                "stall" => {
                    std::fs::write(directory.join("stalled"), "ready").expect("stall marker");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                _ => {}
            }
        }
        let temp = directory.join("actual.tmp");
        std::fs::write(&temp, serde_json::to_vec(&frames(&windows)).expect("json"))
            .expect("write frames");
        std::fs::rename(temp, directory.join("actual.json")).expect("publish frames");
        if let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&NSDate::dateWithTimeIntervalSinceNow(0.05)),
            unsafe { NSDefaultRunLoopMode },
            true,
        ) {
            app.sendEvent(&event);
        }
    }
    for window in windows {
        window.close();
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {}
