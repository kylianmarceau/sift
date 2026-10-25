//! Stable permission-bearing app launcher for the separately rebuilt native check.
//! Keep this bundle unchanged when iterating on the backend/helper executable.
#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::{NSDate, NSRunLoop};
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    let executable = std::env::current_exe().expect("runner path");
    let helper = executable
        .ancestors()
        .nth(4)
        .expect("debug bundle location")
        .join("examples/wm_native_check");
    let mut child = std::process::Command::new(helper)
        .spawn()
        .expect("build wm_native_check first");
    loop {
        if let Some(status) = child.try_wait().expect("native check status") {
            std::process::exit(status.code().unwrap_or(1));
        }
        // Test harness only: keep AppKit responsive while its child verifies
        // the production event-driven backend. No production polling added.
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
