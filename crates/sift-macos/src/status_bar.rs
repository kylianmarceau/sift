// A native, keyboard-accessible status menu. AppKit callbacks only enqueue
// actions; the launcher handles them after menu tracking has finished.
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSBezierPath, NSCellImagePosition, NSColor, NSFont, NSFontAttributeName,
    NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem, NSStringDrawing,
};
use objc2_foundation::{NSBundle, NSDictionary, NSObject, NSPoint, NSRect, NSSize, NSString};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub title: String,
    pub action: Option<String>,
    pub checked: bool,
    pub children: Vec<Item>,
    pub symbol: Option<String>,
    pub key: String,
}
impl Item {
    pub fn label(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            action: None,
            checked: false,
            children: vec![],
            symbol: None,
            key: String::new(),
        }
    }
    pub fn separator() -> Self {
        Self::label("")
    }
}

struct Ivars {
    callback: Rc<dyn Fn(String)>,
    tracking: Cell<bool>,
    items: RefCell<Vec<Item>>,
}
define_class!(
    // SAFETY: NSObject has no subclassing requirements. All use is main-thread only.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SiftStatusMenuTarget"]
    #[ivars = Ivars]
    struct Target;
    impl Target {
        #[unsafe(method(selectItem:))]
        fn select(&self, item: &NSMenuItem) {
            if let Some(value) = item.representedObject()
                && let Some(value) = value.downcast_ref::<NSString>() {
                (self.ivars().callback)(value.to_string());
            }
        }
    }
    unsafe impl NSObjectProtocol for Target {}
    unsafe impl NSMenuDelegate for Target {
        #[unsafe(method(menuNeedsUpdate:))]
        fn needs_update(&self, menu: &NSMenu) {
            if !self.ivars().tracking.get() {
                menu.removeAllItems();
                fill_menu(menu, &self.ivars().items.borrow(), self, self.mtm());
            }
        }
        #[unsafe(method(menuWillOpen:))]
        fn will_open(&self, _menu: &NSMenu) { self.ivars().tracking.set(true); }
        #[unsafe(method(menuDidClose:))]
        fn did_close(&self, _menu: &NSMenu) {
            self.ivars().tracking.set(false);
            // Rebuild deferred state on the next GPUI turn, never in AppKit's tracking callback.
            (self.ivars().callback)(String::new());
        }
    }
);

pub struct MenuBar {
    item: Retained<NSStatusItem>,
    target: Retained<Target>,
    applied: RefCell<Option<(String, String, Vec<Item>)>>,
    mtm: MainThreadMarker,
}
impl MenuBar {
    pub fn new(callback: impl Fn(String) + 'static) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let target = Target::alloc(mtm).set_ivars(Ivars {
            callback: Rc::new(callback),
            tracking: Cell::new(false),
            items: RefCell::new(vec![]),
        });
        // SAFETY: NSObject's designated initializer.
        let target: Retained<Target> = unsafe { msg_send![super(target), init] };
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
        item.setAutosaveName(Some(&NSString::from_str("SiftMenuBar")));
        if let Some(button) = item.button(mtm) {
            // The same sieve glyph as packaging/icon.svg: three pills narrow
            // toward one small square result. Coordinates are the app icon's
            // 1024 pt glyph scaled to an 18 pt template image, which the menu
            // bar recolors for light and dark appearances.
            let drawing = RcBlock::new(|_: NSRect| {
                NSColor::blackColor().setFill();
                for (x, y, w, h, r) in [
                    (1.0, 13.4, 16.0, 3.3, 1.65), // svg 212,250 · 600x124 r62
                    (2.9, 9.3, 12.2, 2.9, 1.45),  // svg 284,421 · 456x108 r54
                    (5.3, 5.4, 7.5, 2.6, 1.3),    // svg 372,576 · 280x96 r48
                    (7.5, 1.3, 2.9, 2.9, 0.5),    // svg 457,719 · 110x110 r20
                ] {
                    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)),
                        r,
                        r,
                    );
                    path.fill();
                }
                true.into()
            });
            let icon = NSImage::imageWithSize_flipped_drawingHandler(
                NSSize::new(18., 18.),
                false,
                &drawing,
            );
            icon.setTemplate(true);
            button.setImage(Some(&icon));
            button.setImagePosition(NSCellImagePosition::ImageLeft);
            button.setAccessibilityLabel(Some(&NSString::from_str("Sift")));
        }
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        item.setMenu(Some(&menu));
        Some(Self {
            item,
            target,
            applied: RefCell::new(None),
            mtm,
        })
    }
    /// Menu contents are immutable during tracking, so live window events cannot
    /// change a menu item's target underneath the pointer.
    pub fn update(&self, title: String, tooltip: String, items: Vec<Item>) {
        if self.target.ivars().tracking.get() {
            return;
        }
        let next = (title, tooltip, items);
        if self.applied.borrow().as_ref() == Some(&next) {
            return;
        }
        if let Some(button) = self.item.button(self.mtm) {
            button.setTitle(&NSString::from_str(&next.0));
            button.setToolTip(Some(&NSString::from_str(&next.1)));
        }
        *self.target.ivars().items.borrow_mut() = next.2.clone();
        *self.applied.borrow_mut() = Some(next);
    }
}
impl Drop for MenuBar {
    fn drop(&mut self) {
        self.item.setMenu(None);
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}
fn build_menu(items: &[Item], target: &Target, mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    fill_menu(&menu, items, target, mtm);
    menu
}
fn fill_menu(menu: &NSMenu, items: &[Item], target: &Target, mtm: MainThreadMarker) {
    for entry in items {
        if entry.title.is_empty() {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            continue;
        }
        // SAFETY: selectItem: is implemented above with AppKit's action signature.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&bounded_title(&entry.title)),
                entry.action.as_ref().map(|_| sel!(selectItem:)),
                &NSString::from_str(&entry.key),
            )
        };
        item.setEnabled(entry.action.is_some() || !entry.children.is_empty());
        item.setState(if entry.checked { 1 } else { 0 });
        // Keep full context accessible without allowing long errors, app names,
        // or paths to stretch the entire native menu across the screen.
        item.setToolTip(Some(&NSString::from_str(&entry.title)));
        if let Some(action) = &entry.action {
            // SAFETY: represented objects are retained NSStrings. MenuBar
            // retains the target longer than the installed menu.
            unsafe {
                item.setRepresentedObject(Some(&NSString::from_str(action)));
                item.setTarget(Some(target));
            }
        }
        if !entry.children.is_empty() {
            item.setSubmenu(Some(&build_menu(&entry.children, target, mtm)));
        }
        if let Some(symbol) = &entry.symbol {
            let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str(symbol),
                None,
            );
            item.setImage(image.as_deref());
        }
        menu.addItem(&item);
    }
}

const MAX_TITLE_WIDTH: f64 = 300.;
fn bounded_title(title: &str) -> String {
    let clean: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let font = NSFont::menuFontOfSize(0.);
    // SAFETY: NSFontAttributeName maps to an NSFont, the documented value type.
    let attributes =
        NSDictionary::from_slices(&[unsafe { NSFontAttributeName }], &[&*font as &AnyObject]);
    let fits = |text: &str| {
        // SAFETY: the attributes dictionary contains only the font attribute.
        unsafe {
            NSString::from_str(text)
                .sizeWithAttributes(Some(&attributes))
                .width
                <= MAX_TITLE_WIDTH
        }
    };
    if fits(&clean) {
        return clean;
    }
    let chars: Vec<_> = clean.chars().collect();
    let (mut low, mut high) = (0, chars.len());
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let candidate = format!("{}…", chars[..mid].iter().collect::<String>());
        if fits(&candidate) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    format!("{}…", chars[..low].iter().collect::<String>())
}

/// The packaged build number distinguishes preview binaries with the same version.
pub fn build_number() -> Option<String> {
    let value = NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&NSString::from_str("CFBundleVersion"))?;
    value.downcast_ref::<NSString>().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn refuses_appkit_off_the_main_thread() {
        assert!(super::MenuBar::new(|_| {}).is_none());
    }
}
