//! Isolated on-screen verification. Requires Accessibility for this executable.
//! Build both examples, then run this one. Only the bundled fixture is managed.
#[cfg(target_os = "macos")]
#[path = "wm_native_check/launcher_bench.rs"]
mod launcher_bench;
#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace,
        NSWorkspaceOpenConfiguration,
    };
    use objc2_foundation::{NSArray, NSDate, NSError, NSRunLoop, NSString, NSURL};
    use sift_wm::{Command, Config, Rect};
    use std::time::{Duration, Instant};
    let level = if std::env::var("SIFT_WM_DEBUG").is_ok_and(|value| !value.is_empty()) {
        tracing::Level::DEBUG
    } else {
        tracing::Level::WARN
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .init();
    if !sift_macos::wm::trusted() {
        eprintln!(
            "Accessibility is not granted to wm_native_check; no windows were opened or moved."
        );
        std::process::exit(78);
    }
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    let directory = tempfile::tempdir().expect("tempdir");
    let base = directory.path();
    let bundle = base.join("Sift WM Fixture.app/Contents");
    std::fs::create_dir_all(bundle.join("MacOS")).expect("bundle");
    let binary = std::env::current_exe()
        .expect("executable")
        .parent()
        .expect("examples")
        .join("wm_fixture");
    std::fs::copy(binary, bundle.join("MacOS/Fixture")).expect("build wm_fixture first");
    std::fs::write(bundle.join("Info.plist"), r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>io.github.kylianmarceau.sift.wm-fixture</string><key>CFBundleExecutable</key><string>Fixture</string><key>CFBundleName</key><string>Sift WM Fixture</string><key>CFBundlePackageType</key><string>APPL</string></dict></plist>"#).expect("plist");
    let signed = std::process::Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(bundle.parent().expect("fixture bundle"))
        .status()
        .expect("sign fixture");
    assert!(signed.success(), "fixture signing failed");
    let configuration = NSWorkspaceOpenConfiguration::configuration();
    configuration.setArguments(&NSArray::from_retained_slice(&[NSString::from_str(
        &base.to_string_lossy(),
    )]));
    configuration.setCreatesNewApplicationInstance(true);
    // AppKit initializes its AX window list on first activation. Make the
    // fixture a normally launched app before testing background activation.
    configuration.setActivates(true);
    let launched = std::sync::Arc::new(std::sync::Mutex::new(None));
    let result = launched.clone();
    let completion =
        block2::RcBlock::new(move |app: *mut NSRunningApplication, error: *mut NSError| {
            let value = if !app.is_null() {
                Ok(unsafe { &*app }.processIdentifier())
            } else if !error.is_null() {
                Err(unsafe { &*error }.localizedDescription().to_string())
            } else {
                Err("fixture launch did not return an application".into())
            };
            *result.lock().expect("launch result") = Some(value);
        });
    NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
        &NSURL::fileURLWithPath(&NSString::from_str(
            &bundle.parent().expect("fixture bundle").to_string_lossy(),
        )),
        &configuration,
        Some(&completion),
    );
    struct Cleanup(objc2::rc::Retained<NSRunningApplication>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.forceTerminate();
        }
    }
    let spin = || {
        while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            objc2_app_kit::NSEventMask::Any,
            Some(&NSDate::distantPast()),
            unsafe { objc2_foundation::NSDefaultRunLoopMode },
            true,
        ) {
            app.sendEvent(&event);
        }
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.02));
    };
    let wait = |label: &str, condition: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "timed out: {label}"
            );
            spin();
        }
        println!("PASS {label}");
    };
    wait("fixture launched through Launch Services", &|| {
        launched.lock().expect("launch result").is_some()
    });
    let pid = launched
        .lock()
        .expect("launch result")
        .take()
        .expect("launch completion")
        .expect("fixture launch");
    let _cleanup = Cleanup(
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .expect("fixture process"),
    );
    wait("fixture ready", &|| base.join("original.json").exists());
    wait("fixture registered as a regular application", &|| {
        NSWorkspace::sharedWorkspace()
            .runningApplications()
            .iter()
            .any(|app| {
                app.processIdentifier() == pid
                    && app.activationPolicy() == NSApplicationActivationPolicy::Regular
                    && app.bundleIdentifier().is_some_and(|id| {
                        id.to_string() == "io.github.kylianmarceau.sift.wm-fixture"
                    })
            })
    });
    let originals: serde_json::Value =
        serde_json::from_slice(&std::fs::read(base.join("original.json")).expect("original"))
            .expect("json");
    let config = Config {
        enabled: true,
        include_apps: vec!["^io\\.github\\.kylianmarceau\\.sift\\.wm-fixture$".into()],
        ..Config::default()
    };
    let config_path = base.join("config.toml");
    std::fs::write(&config_path, "[window_manager]\nenabled = true\ninclude_apps = ['^io\\.github\\.kylianmarceau\\.sift\\.wm-fixture$']\n").expect("config");
    let mut runtime =
        sift_macos::wm::Runtime::start(config, base, &config_path, |_| {}).expect("runtime");
    let controller = runtime.controller();
    let wait = |label: &str, condition: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !condition() {
            if start.elapsed() >= Duration::from_secs(15) {
                eprintln!(
                    "Last model: {}",
                    serde_json::to_string_pretty(&*controller.snapshot()).expect("snapshot")
                );
                eprintln!(
                    "Last native windows: {}",
                    std::fs::read_to_string(base.join("actual.json")).unwrap_or_default()
                );
                panic!("timed out: {label}");
            }
            spin();
        }
        println!("PASS {label}");
    };
    wait("discover only fixture windows", &|| {
        controller.snapshot().windows.len() == 3
    });
    let actual = || -> serde_json::Value {
        std::fs::read(base.join("actual.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    let matches = |expected: &[(u32, Rect)]| {
        let actual = actual();
        expected.iter().all(|(id, frame)| {
            actual.as_array().is_some_and(|list| {
                list.iter().any(|w| {
                    w["id"].as_u64() == Some(*id as u64)
                        && serde_json::from_value::<Rect>(w["frame"].clone())
                            .is_ok_and(|r| r.nearly(*frame))
                })
            })
        })
    };
    let desired = || {
        controller
            .snapshot()
            .windows
            .iter()
            .map(|w| (w.id, w.frame))
            .collect::<Vec<_>>()
    };
    wait("actual native windows tiled", &|| matches(&desired()));
    use objc2::MainThreadOnly;
    use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    let control = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(20., 40.), NSSize::new(220., 80.)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        control.setReleasedWhenClosed(false);
    }
    control.setTitle(&NSString::from_str("Sift WM Check Control"));
    control.makeKeyAndOrderFront(None);
    NSRunningApplication::currentApplication()
        .activateWithOptions(objc2_app_kit::NSApplicationActivationOptions::empty());
    // Cooperative AppKit activation may ignore a background accessory app.
    // Make the disposable control window active through AX on a worker, while
    // this main thread continues serving the app's own Accessibility requests.
    std::thread::spawn(cursor::activate_self);
    wait("fixture inactive before focus test", &|| {
        !actual()
            .as_array()
            .is_some_and(|list| list.iter().any(|w| w["focused"].as_bool() == Some(true)))
    });
    let id = controller.snapshot().windows[0].id;
    controller.execute(Command::FocusId(id)).expect("focus");
    wait("focus native window", &|| {
        actual().as_array().is_some_and(|list| {
            list.iter().any(|w| {
                w["id"].as_u64() == Some(id as u64) && w["focused"].as_bool() == Some(true)
            })
        })
    });
    control.close();
    {
        let original = cursor::position();
        let _restore = cursor::Restore(original);
        controller
            .execute(sift_wm::parse("move-mouse window-center").expect("cursor command"))
            .expect("cursor move");
        wait("native cursor positioning", &|| {
            let (x, y) = controller
                .snapshot()
                .windows
                .iter()
                .find(|w| w.id == id)
                .expect("window")
                .frame
                .center();
            let point = cursor::position();
            (point.x - x).abs() < 1. && (point.y - y).abs() < 1.
        });
    }
    controller
        .execute(Command::MoveToWorkspace("2".into(), false))
        .expect("move");
    wait("move to hidden workspace", &|| {
        controller
            .snapshot()
            .windows
            .iter()
            .any(|w| w.id == id && w.workspace == "2")
    });
    controller
        .execute(Command::Workspace("2".into()))
        .expect("workspace");
    wait("workspace shows native window", &|| {
        matches(&[(
            id,
            controller
                .snapshot()
                .windows
                .iter()
                .find(|w| w.id == id)
                .expect("window")
                .frame,
        )])
    });
    // Minimize the visible window. AppKit may ignore miniaturize while the
    // target is parked almost entirely offscreen on another workspace.
    std::fs::write(base.join("fixture-command"), "minimize").expect("command");
    wait("minimize leaves tile tree", &|| {
        controller
            .snapshot()
            .windows
            .iter()
            .any(|w| w.id == id && w.minimized)
    });
    std::fs::write(base.join("fixture-command"), "restore").expect("command");
    wait("unminimize rejoins tile tree", &|| {
        controller
            .snapshot()
            .windows
            .iter()
            .any(|w| w.id == id && !w.minimized)
    });
    std::fs::write(base.join("fixture-command"), "stall").expect("command");
    wait("fixture is stalled", &|| base.join("stalled").exists());
    runtime.refresh();
    let start = Instant::now();
    let reply = sift_wm::protocol::request(base, vec!["status".into()]).expect("status");
    assert!(reply.ok);
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "slow app blocked model service"
    );
    println!("PASS stalled application does not block status");
    controller.execute(Command::Enable(false)).expect("pause");
    let originals: Vec<_> = originals
        .as_array()
        .expect("frames")
        .iter()
        .map(|v| {
            (
                v["id"].as_u64().expect("id") as u32,
                serde_json::from_value::<Rect>(v["frame"].clone()).expect("frame"),
            )
        })
        .collect();
    wait("pause restores original frames", &|| matches(&originals));
    controller.execute(Command::Enable(true)).expect("resume");
    wait("resume tiles native windows again", &|| {
        let snapshot = controller.snapshot();
        snapshot.enabled
            && matches(
                &snapshot
                    .windows
                    .iter()
                    .filter(|w| snapshot.visible.values().any(|name| name == &w.workspace))
                    .map(|w| (w.id, w.frame))
                    .collect::<Vec<_>>(),
            )
    });
    let original_config = (*controller.config()).clone();
    for _ in 0..3 {
        let mut config = original_config.clone();
        config.mouse.drag_to_rearrange = true;
        controller.configure(config).expect("enable mouse listener");
        wait("mouse configuration enabled", &|| {
            controller.config().mouse.drag_to_rearrange
        });
        runtime
            .sync_mouse_monitor()
            .expect("install native mouse listener");
        controller
            .configure(original_config.clone())
            .expect("disable mouse listener");
        wait("mouse configuration disabled", &|| {
            !controller.config().mouse.drag_to_rearrange
        });
        runtime
            .sync_mouse_monitor()
            .expect("remove native mouse listener");
    }
    drop(runtime);
    wait("shutdown restores original frames", &|| matches(&originals));
    assert!(
        controller.wait_stopped(Duration::from_secs(4)),
        "shutdown did not release ownership"
    );
    if std::env::var_os("SIFT_WM_LAUNCHER").is_some_and(|value| !value.is_empty()) {
        launcher_bench::run(base);
    }
    println!("Native window-manager checks passed.");
}

#[cfg(target_os = "macos")]
mod cursor {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Point {
        pub x: f64,
        pub y: f64,
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreate(source: *const c_void) -> *const c_void;
        fn CGEventGetLocation(event: *const c_void) -> Point;
        fn CGWarpMouseCursorPosition(point: Point) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const c_void);
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            value: *const std::ffi::c_char,
            encoding: u32,
        ) -> *const c_void;
        static kCFBooleanTrue: *const c_void;
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
        fn AXUIElementSetAttributeValue(
            element: *const c_void,
            attribute: *const c_void,
            value: *const c_void,
        ) -> i32;
        fn AXUIElementSetMessagingTimeout(element: *const c_void, seconds: f32) -> i32;
    }
    pub fn activate_self() {
        unsafe {
            let app = AXUIElementCreateApplication(std::process::id() as i32);
            let key =
                CFStringCreateWithCString(std::ptr::null(), c"AXFrontmost".as_ptr(), 0x08000100);
            assert!(!app.is_null() && !key.is_null());
            AXUIElementSetMessagingTimeout(app, 0.05);
            let result = AXUIElementSetAttributeValue(app, key, kCFBooleanTrue);
            CFRelease(key);
            CFRelease(app);
            assert_eq!(result, 0, "activate control application");
        }
    }
    pub fn position() -> Point {
        unsafe {
            let event = CGEventCreate(std::ptr::null());
            assert!(!event.is_null(), "cursor state unavailable");
            let point = CGEventGetLocation(event);
            CFRelease(event);
            point
        }
    }
    pub struct Restore(pub Point);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                CGWarpMouseCursorPosition(self.0);
            }
        }
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {}
