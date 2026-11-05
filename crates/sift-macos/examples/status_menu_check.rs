//! Main-thread native smoke check, without opening a menu or sending UI input.
//! Run: cargo run -p sift-macos --example status_menu_check
//! Set SIFT_STATUS_PNG_DIR to also save the menu bar indicators as PNGs.
#[cfg(target_os = "macos")]
mod check {
    // Exercise the implementation on AppKit's main thread, including its private
    // target/delegate lifecycle, without adding test hooks to the public API.
    include!("../src/status_bar.rs");

    fn save_png(image: &NSImage, path: &std::path::Path) {
        use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
        // Render at 4x on a light background, as a template image appears
        // in a light menu bar.
        let size = image.size();
        let scale = 4.;
        let image = objc2::Message::retain(image);
        let canvas = NSImage::imageWithSize_flipped_drawingHandler(
            NSSize::new(size.width * scale + 16., size.height * scale + 16.),
            false,
            &RcBlock::new(move |rect: NSRect| {
                NSColor::colorWithWhite_alpha(0.93, 1.).setFill();
                NSBezierPath::fillRect(rect);
                image.drawInRect(NSRect::new(
                    NSPoint::new(8., 8.),
                    NSSize::new(size.width * scale, size.height * scale),
                ));
                true.into()
            }),
        );
        let tiff = canvas.TIFFRepresentation().expect("tiff");
        let rep = NSBitmapImageRep::imageRepWithData(&tiff).expect("bitmap");
        // SAFETY: an empty properties dictionary.
        let png = unsafe {
            rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
        }
        .expect("png");
        std::fs::write(path, png.to_vec()).expect("write png");
    }

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("main thread");
        let _app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        let _ = build_number();
        let selected = Rc::new(RefCell::new(Vec::<String>::new()));
        let events = selected.clone();
        let bar =
            MenuBar::new(move |action| events.borrow_mut().push(action)).expect("status item");
        let button = bar.item.button(mtm).expect("button");
        assert!(button.image().expect("glyph").isTemplate());
        let glyph_width = button.image().expect("glyph").size().width;

        let mut workspace = Item::label("11");
        workspace.detail = Some("Editor, Browser".into());
        workspace.action = Some("workspace-11".into());
        workspace.checked = true;
        workspace.key = "1".into();
        workspace.modifiers = OPTION;
        let mut app = Item::label("Editor");
        app.children = vec![Item::label("Workspace 1")];
        let mut group = Item::label("Assign Apps");
        group.children = vec![Item::header("All Apps"), app];
        let mut quit = Item::label("Quit Sift");
        quit.action = Some("quit".into());
        quit.key = "q".into();
        let mut open = Item::label("Open Sift");
        open.symbol = Some("magnifyingglass".into());
        open.action = Some("open".into());
        let labels = |current: &str| {
            ["1", "2", "11", "web"]
                .map(|n| (n.to_owned(), n == current))
                .to_vec()
        };
        let indicator = Indicator::Workspaces {
            labels: labels("11"),
            paused: false,
        };
        bar.update(
            indicator.clone(),
            "Sift · workspace 11".into(),
            vec![
                open,
                Item::separator(),
                Item::header("Workspaces"),
                workspace,
                group,
                quit,
            ],
        );
        let menu = bar.item.menu(mtm).expect("menu");
        bar.target.needs_update(sel!(menuNeedsUpdate:), &menu);
        assert_eq!(menu.numberOfItems(), 6);
        let image = button.image().expect("workspace indicator");
        assert!(image.isTemplate());
        assert!(image.size().width > glyph_width);
        assert_eq!(
            button.accessibilityLabel().expect("label").to_string(),
            "Sift, workspace 11"
        );
        let header = menu.itemAtIndex(2).expect("header");
        assert!(header.isSectionHeader() || !header.isEnabled());
        assert!(header.action().is_none());
        let workspace = menu.itemAtIndex(3).expect("workspace item");
        assert_eq!(workspace.state(), 1);
        // The plain title, which VoiceOver reads, includes the detail.
        assert_eq!(workspace.title().to_string(), "11   Editor, Browser");
        assert!(
            workspace
                .attributedTitle()
                .expect("detail text")
                .string()
                .to_string()
                .contains("Editor, Browser")
        );
        assert_eq!(workspace.keyEquivalent().to_string(), "1");
        assert_eq!(
            workspace.keyEquivalentModifierMask(),
            NSEventModifierFlags::Option
        );
        assert!(workspace.toolTip().is_none());
        bar.target.select(sel!(selectItem:), &workspace);
        assert_eq!(&*selected.borrow(), &["workspace-11"]);
        let quit = menu.itemAtIndex(5).expect("quit item");
        assert_eq!(quit.keyEquivalent().to_string(), "q");
        assert_eq!(
            quit.keyEquivalentModifierMask(),
            NSEventModifierFlags::Command
        );
        assert!(menu.itemAtIndex(0).expect("first item").image().is_some());

        // Submenus stay empty until opened, then fill once, at any depth.
        bar.target.will_open(sel!(menuWillOpen:), &menu);
        let assign = menu
            .itemAtIndex(4)
            .expect("assign parent")
            .submenu()
            .expect("assign submenu");
        assert_eq!(assign.numberOfItems(), 0);
        bar.target.needs_update(sel!(menuNeedsUpdate:), &assign);
        assert_eq!(assign.numberOfItems(), 2);
        bar.target.needs_update(sel!(menuNeedsUpdate:), &assign);
        assert_eq!(assign.numberOfItems(), 2);
        let apps = assign
            .itemAtIndex(1)
            .expect("app")
            .submenu()
            .expect("app submenu");
        bar.target.needs_update(sel!(menuNeedsUpdate:), &apps);
        assert_eq!(
            apps.itemAtIndex(0).expect("workspace").title().to_string(),
            "Workspace 1"
        );
        // Opening a submenu is not the root menu closing.
        bar.target.did_close(sel!(menuDidClose:), &assign);
        assert!(bar.target.ivars().tracking.get());

        // Nothing changes underneath an open menu.
        let paused = Indicator::Workspaces {
            labels: labels("2"),
            paused: true,
        };
        bar.update(
            paused.clone(),
            "Sift".into(),
            vec![Item::label("New contents")],
        );
        assert_eq!(menu.numberOfItems(), 6);
        assert_eq!(
            button.accessibilityLabel().expect("label").to_string(),
            "Sift, workspace 11"
        );
        bar.target.did_close(sel!(menuDidClose:), &menu);
        assert_eq!(selected.borrow().last().expect("close event"), "");
        bar.update(paused, "Sift".into(), vec![Item::label("New contents")]);
        bar.target.needs_update(sel!(menuNeedsUpdate:), &menu);
        assert_eq!(
            button.accessibilityLabel().expect("label").to_string(),
            "Sift, workspace 2, paused"
        );
        assert_eq!(menu.numberOfItems(), 1);
        assert!(!menu.itemAtIndex(0).expect("first item").isEnabled());
        assert!(bar.target.ivars().lazy.borrow().is_empty());

        let long = format!(
            "Permission detail: {}",
            "very wide 窗口 workspace title ".repeat(12)
        );
        bar.update(Indicator::Glyph, "Sift".into(), vec![Item::label(&long)]);
        bar.target.needs_update(sel!(menuNeedsUpdate:), &menu);
        assert_eq!(button.image().expect("glyph").size().width, glyph_width);
        let row = menu.itemAtIndex(0).expect("long row");
        let displayed = row.title();
        assert!(displayed.to_string().ends_with('…'));
        assert_eq!(row.toolTip().expect("full description").to_string(), long);
        let font = NSFont::menuFontOfSize(0.);
        // SAFETY: the font attribute's documented value is NSFont.
        let attributes =
            NSDictionary::from_slices(&[unsafe { NSFontAttributeName }], &[&*font as &AnyObject]);
        // SAFETY: the attributes dictionary has the documented value types.
        assert!(
            unsafe { displayed.sizeWithAttributes(Some(&attributes)).width } <= MAX_TITLE_WIDTH
        );

        if let Some(dir) = std::env::var_os("SIFT_STATUS_PNG_DIR") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).expect("png directory");
            save_png(&glyph(), &dir.join("glyph.png"));
            save_png(
                &workspaces(&labels("2"), false),
                &dir.join("workspaces.png"),
            );
            save_png(
                &workspaces(&labels("web"), false),
                &dir.join("workspaces-web.png"),
            );
            save_png(
                &workspaces(&labels("2"), true),
                &dir.join("workspaces-paused.png"),
            );
            save_png(
                &workspaces(&[("1".into(), true)], false),
                &dir.join("workspace-alone.png"),
            );
            println!("saved indicators to {}", dir.display());
        }

        let item = bar.item.clone();
        drop(bar);
        assert!(item.menu(mtm).is_none());
        println!(
            "PASS: glyph and workspace indicators, section headers, detail text, option shortcuts, lazy nested menus, checkmarks, symbols, action dispatch, tracking freeze, deferred updates, bounded menu width, tooltips only when truncated, teardown"
        );
    }
}
fn main() {
    #[cfg(target_os = "macos")]
    check::run();
}
