//! Main-thread native smoke check, without opening a menu or sending UI input.
//! Run: cargo run -p sift-macos --example status_menu_check
#[cfg(target_os = "macos")]
mod check {
    // Exercise the implementation on AppKit's main thread, including its private
    // target/delegate lifecycle, without adding test hooks to the public API.
    include!("../src/status_bar.rs");

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("main thread");
        let _app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        let _ = build_number();
        let selected = Rc::new(RefCell::new(Vec::<String>::new()));
        let events = selected.clone();
        let bar =
            MenuBar::new(move |action| events.borrow_mut().push(action)).expect("status item");
        let mut workspace = Item::label("11 — Editor, Browser");
        workspace.action = Some("workspace-11".into());
        workspace.checked = true;
        let mut group = Item::label("Workspaces");
        group.children = vec![workspace];
        let mut quit = Item::label("Quit Sift");
        quit.action = Some("quit".into());
        quit.key = "q".into();
        let mut open = Item::label("Open Sift");
        open.symbol = Some("magnifyingglass".into());
        open.action = Some("open".into());
        bar.update(
            " 11".into(),
            "Sift · workspace 11".into(),
            vec![open, Item::separator(), group, quit],
        );
        let menu = bar.item.menu(mtm).expect("menu");
        bar.target.needs_update(sel!(menuNeedsUpdate:), &menu);
        assert_eq!(menu.numberOfItems(), 4);
        let button = bar.item.button(mtm).expect("button");
        assert_eq!(button.title().to_string(), " 11");
        assert!(button.image().expect("template icon").isTemplate());
        let workspaces = menu
            .itemAtIndex(2)
            .expect("workspace parent")
            .submenu()
            .expect("workspace submenu");
        let workspace = workspaces.itemAtIndex(0).expect("workspace item");
        assert_eq!(workspace.state(), 1);
        assert_eq!(workspace.title().to_string(), "11 — Editor, Browser");
        bar.target.select(sel!(selectItem:), &workspace);
        assert_eq!(&*selected.borrow(), &["workspace-11"]);
        assert_eq!(
            menu.itemAtIndex(3)
                .expect("quit item")
                .keyEquivalent()
                .to_string(),
            "q"
        );
        assert!(menu.itemAtIndex(0).expect("first item").image().is_some());

        bar.target.will_open(sel!(menuWillOpen:), &menu);
        bar.update(
            " 2".into(),
            "Sift".into(),
            vec![Item::label("New contents")],
        );
        assert_eq!(button.title().to_string(), " 11");
        assert_eq!(menu.numberOfItems(), 4);
        bar.target.did_close(sel!(menuDidClose:), &menu);
        assert_eq!(selected.borrow().last().expect("close event"), "");
        bar.update(
            " 2".into(),
            "Sift".into(),
            vec![Item::label("New contents")],
        );
        bar.target.needs_update(sel!(menuNeedsUpdate:), &menu);
        assert_eq!(button.title().to_string(), " 2");
        assert_eq!(menu.numberOfItems(), 1);
        assert!(!menu.itemAtIndex(0).expect("first item").isEnabled());
        let long = format!(
            "Permission detail: {}",
            "very wide 窗口 workspace title ".repeat(12)
        );
        bar.update("".into(), "Sift".into(), vec![Item::label(&long)]);
        bar.target.needs_update(sel!(menuNeedsUpdate:), &menu);
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
        let item = bar.item.clone();
        drop(bar);
        assert!(item.menu(mtm).is_none());
        println!(
            "PASS: native status icon, nested menus, checkmarks, symbols, shortcuts, action dispatch, tracking freeze, deferred updates, bounded menu width, full tooltips, teardown"
        );
    }
}
fn main() {
    #[cfg(target_os = "macos")]
    check::run();
}
