// A native, keyboard-accessible status menu. AppKit callbacks only enqueue
// actions; the launcher handles them after menu tracking has finished.
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    AllocAnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class,
    msg_send, sel,
};
use objc2_app_kit::{
    NSAccessibility, NSBezierPath, NSCellImagePosition, NSColor, NSCompositingOperation,
    NSEventModifierFlags, NSFont, NSFontAttributeName, NSFontWeightBold,
    NSForegroundColorAttributeName, NSGraphicsContext, NSImage, NSMenu, NSMenuDelegate, NSMenuItem,
    NSStatusBar, NSStatusItem, NSStringDrawing,
};
use objc2_foundation::{
    NSAttributedString, NSBundle, NSDictionary, NSMutableAttributedString, NSObject, NSPoint,
    NSRect, NSSize, NSString,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

/// Modifier keys for an item's key equivalent.
pub const COMMAND: u8 = 1;
pub const OPTION: u8 = 2;
pub const CONTROL: u8 = 4;
pub const SHIFT: u8 = 8;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    pub title: String,
    /// Secondary text shown after the title in a dimmer color.
    pub detail: Option<String>,
    pub action: Option<String>,
    pub checked: bool,
    /// A section header (a small, dimmed, non-interactive title).
    pub header: bool,
    pub children: Vec<Item>,
    pub symbol: Option<String>,
    pub key: String,
    /// `COMMAND | OPTION | …`; zero with a key means Command.
    pub modifiers: u8,
}
impl Item {
    pub fn label(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }
    pub fn header(title: impl Into<String>) -> Self {
        Self {
            header: true,
            ..Self::label(title)
        }
    }
    pub fn separator() -> Self {
        Self::label("")
    }
    pub fn is_separator(&self) -> bool {
        self.title.is_empty() && !self.header
    }
}

/// What the menu bar shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Indicator {
    /// Sift's sieve glyph.
    Glyph,
    /// Workspace labels: the current one in a filled badge, others with
    /// windows as plain dimmed labels. A paused manager outlines the badge.
    Workspaces {
        labels: Vec<(String, bool)>,
        paused: bool,
    },
}

struct Ivars {
    callback: Rc<dyn Fn(String)>,
    tracking: Cell<bool>,
    items: RefCell<Vec<Item>>,
    root: RefCell<Option<Retained<NSMenu>>>,
    /// Submenus are filled when first opened, so a large tree (every
    /// installed app) costs nothing until someone looks at it. Each entry
    /// is a submenu, its index path into `items`, and whether it is filled.
    lazy: RefCell<Vec<LazyMenu>>,
}
type LazyMenu = (Retained<NSMenu>, Vec<usize>, Cell<bool>);
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
            let ivars = self.ivars();
            let is_root = ivars.root.borrow().as_deref().is_some_and(|root| std::ptr::eq(root, menu));
            if is_root {
                if !ivars.tracking.get() {
                    menu.removeAllItems();
                    ivars.lazy.borrow_mut().clear();
                    fill_menu(menu, &ivars.items.borrow(), &[], self, self.mtm());
                }
                return;
            }
            let path = ivars.lazy.borrow().iter().find_map(|(sub, path, filled)| {
                (std::ptr::eq(&**sub, menu) && !filled.replace(true)).then(|| path.clone())
            });
            if let Some(path) = path {
                let items = ivars.items.borrow();
                if let Some(children) = children_at(&items, &path) {
                    fill_menu(menu, children, &path, self, self.mtm());
                }
            }
        }
        #[unsafe(method(menuWillOpen:))]
        fn will_open(&self, menu: &NSMenu) {
            if self.ivars().root.borrow().as_deref().is_some_and(|root| std::ptr::eq(root, menu)) {
                self.ivars().tracking.set(true);
            }
        }
        #[unsafe(method(menuDidClose:))]
        fn did_close(&self, menu: &NSMenu) {
            if !self.ivars().root.borrow().as_deref().is_some_and(|root| std::ptr::eq(root, menu)) {
                return;
            }
            self.ivars().tracking.set(false);
            // Rebuild deferred state on the next GPUI turn, never in AppKit's tracking callback.
            (self.ivars().callback)(String::new());
        }
    }
);

fn children_at<'a>(items: &'a [Item], path: &[usize]) -> Option<&'a [Item]> {
    let (first, rest) = path.split_first()?;
    let item = items.get(*first)?;
    if rest.is_empty() {
        Some(&item.children)
    } else {
        children_at(&item.children, rest)
    }
}

pub struct MenuBar {
    item: Retained<NSStatusItem>,
    target: Retained<Target>,
    applied: RefCell<Option<(Indicator, String, Vec<Item>)>>,
    mtm: MainThreadMarker,
}
impl MenuBar {
    pub fn new(callback: impl Fn(String) + 'static) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let target = Target::alloc(mtm).set_ivars(Ivars {
            callback: Rc::new(callback),
            tracking: Cell::new(false),
            items: RefCell::new(vec![]),
            root: RefCell::new(None),
            lazy: RefCell::new(vec![]),
        });
        // SAFETY: NSObject's designated initializer.
        let target: Retained<Target> = unsafe { msg_send![super(target), init] };
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
        item.setAutosaveName(Some(&NSString::from_str("SiftMenuBar")));
        if let Some(button) = item.button(mtm) {
            button.setImagePosition(NSCellImagePosition::ImageOnly);
        }
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        item.setMenu(Some(&menu));
        *target.ivars().root.borrow_mut() = Some(menu);
        let bar = Self {
            item,
            target,
            applied: RefCell::new(None),
            mtm,
        };
        bar.show(&Indicator::Glyph);
        Some(bar)
    }
    /// Menu contents are immutable during tracking, so live window events cannot
    /// change a menu item's target underneath the pointer.
    pub fn update(&self, indicator: Indicator, tooltip: String, items: Vec<Item>) {
        if self.target.ivars().tracking.get() {
            return;
        }
        let next = (indicator, tooltip, items);
        let previous = self.applied.borrow_mut().take();
        if previous.as_ref().is_none_or(|p| p.0 != next.0) {
            self.show(&next.0);
        }
        if let Some(button) = self.item.button(self.mtm)
            && previous.as_ref().is_none_or(|p| p.1 != next.1)
        {
            button.setToolTip(Some(&NSString::from_str(&next.1)));
        }
        if previous.as_ref().is_none_or(|p| p.2 != next.2) {
            *self.target.ivars().items.borrow_mut() = next.2.clone();
        }
        *self.applied.borrow_mut() = Some(next);
    }

    fn show(&self, indicator: &Indicator) {
        let Some(button) = self.item.button(self.mtm) else {
            return;
        };
        let (image, label) = match indicator {
            Indicator::Glyph => (glyph(), "Sift".to_owned()),
            Indicator::Workspaces { labels, paused } => {
                let current = labels
                    .iter()
                    .find(|(_, current)| *current)
                    .map(|(name, _)| name.as_str())
                    .unwrap_or("none");
                (
                    workspaces(labels, *paused),
                    format!(
                        "Sift, workspace {current}{}",
                        if *paused { ", paused" } else { "" }
                    ),
                )
            }
        };
        button.setImage(Some(&image));
        button.setAccessibilityLabel(Some(&NSString::from_str(&label)));
    }
}
impl Drop for MenuBar {
    fn drop(&mut self) {
        self.item.setMenu(None);
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

const ICON_HEIGHT: f64 = 18.;

/// The same sieve glyph as packaging/icon.svg: three pills narrow toward one
/// small square result. Coordinates are the app icon's 1024 pt glyph scaled
/// to an 18 pt template image, which the menu bar recolors for light and
/// dark appearances.
fn glyph() -> Retained<NSImage> {
    let drawing = RcBlock::new(|_: NSRect| {
        NSColor::blackColor().setFill();
        for (x, y, w, h, r) in [
            (1.0, 13.4, 16.0, 3.3, 1.65), // svg 212,250 · 600x124 r62
            (2.9, 9.3, 12.2, 2.9, 1.45),  // svg 284,421 · 456x108 r54
            (5.3, 5.4, 7.5, 2.6, 1.3),    // svg 372,576 · 280x96 r48
            (7.5, 1.3, 2.9, 2.9, 0.5),    // svg 457,719 · 110x110 r20
        ] {
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)),
                r,
                r,
            )
            .fill();
        }
        true.into()
    });
    let icon = NSImage::imageWithSize_flipped_drawingHandler(
        NSSize::new(ICON_HEIGHT, ICON_HEIGHT),
        false,
        &drawing,
    );
    icon.setTemplate(true);
    icon
}

fn indicator_attributes(alpha: f64) -> Retained<NSDictionary<NSString, AnyObject>> {
    // SAFETY: plain constant reads.
    let font = NSFont::monospacedDigitSystemFontOfSize_weight(11., unsafe { NSFontWeightBold });
    let color = NSColor::blackColor().colorWithAlphaComponent(alpha);
    // SAFETY: NSFontAttributeName maps to an NSFont and
    // NSForegroundColorAttributeName to an NSColor, the documented types.
    unsafe {
        NSDictionary::from_slices(
            &[NSFontAttributeName, NSForegroundColorAttributeName],
            &[&*font as &AnyObject, &*color as &AnyObject],
        )
    }
}

/// Workspace labels as one template image: `[2]` for the current one,
/// with the label knocked out of a filled badge, and dimmed labels for the
/// other workspaces that have windows.
fn workspaces(labels: &[(String, bool)], paused: bool) -> Retained<NSImage> {
    const PAD: f64 = 4.;
    const BADGE: f64 = 15.;
    const GAP: f64 = 3.;
    let measure = indicator_attributes(1.);
    let mut layout = Vec::new();
    let mut x = 0.;
    for (label, current) in labels {
        // SAFETY: the attributes hold only a font and a color.
        let size = unsafe { NSString::from_str(label).sizeWithAttributes(Some(&measure)) };
        let width = if *current {
            (size.width + 2. * PAD).max(BADGE)
        } else {
            size.width + 2.
        };
        layout.push((label.clone(), *current, x, width, size));
        x += width + GAP;
    }
    let width = (x - GAP).max(BADGE);
    let drawing = RcBlock::new(move |_: NSRect| {
        let solid = indicator_attributes(1.);
        let dim = indicator_attributes(0.45);
        let context = NSGraphicsContext::currentContext();
        for (label, current, x, width, size) in &layout {
            let text = NSString::from_str(label);
            let point = NSPoint::new(
                x + (width - size.width) / 2.,
                ((ICON_HEIGHT - size.height) / 2.).round(),
            );
            if !*current {
                // SAFETY: the attributes hold only a font and a color.
                unsafe { text.drawAtPoint_withAttributes(point, Some(&dim)) };
                continue;
            }
            let badge = NSRect::new(
                NSPoint::new(*x, (ICON_HEIGHT - BADGE) / 2.),
                NSSize::new(*width, BADGE),
            );
            NSColor::blackColor().set();
            if paused {
                let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    NSRect::new(
                        NSPoint::new(badge.origin.x + 0.75, badge.origin.y + 0.75),
                        NSSize::new(badge.size.width - 1.5, badge.size.height - 1.5),
                    ),
                    3.5,
                    3.5,
                );
                path.setLineWidth(1.5);
                path.stroke();
                // SAFETY: the attributes hold only a font and a color.
                unsafe { text.drawAtPoint_withAttributes(point, Some(&solid)) };
                continue;
            }
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(badge, 4., 4.).fill();
            // Erase the label from the badge so the menu bar shows through.
            if let Some(context) = &context {
                context.saveGraphicsState();
                context.setCompositingOperation(NSCompositingOperation::DestinationOut);
                // SAFETY: the attributes hold only a font and a color.
                unsafe { text.drawAtPoint_withAttributes(point, Some(&solid)) };
                context.restoreGraphicsState();
            }
        }
        true.into()
    });
    let icon = NSImage::imageWithSize_flipped_drawingHandler(
        NSSize::new(width.ceil(), ICON_HEIGHT),
        false,
        &drawing,
    );
    icon.setTemplate(true);
    icon
}

fn modifier_flags(modifiers: u8) -> NSEventModifierFlags {
    let mut flags = NSEventModifierFlags::empty();
    for (bit, flag) in [
        (COMMAND, NSEventModifierFlags::Command),
        (OPTION, NSEventModifierFlags::Option),
        (CONTROL, NSEventModifierFlags::Control),
        (SHIFT, NSEventModifierFlags::Shift),
    ] {
        if modifiers & bit != 0 {
            flags |= flag;
        }
    }
    flags
}

fn header_item(title: &str, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    // Native section headers arrived in macOS 14; earlier systems get a
    // small dimmed title that looks and behaves the same.
    if NSMenuItem::class()
        .metaclass()
        .responds_to(sel!(sectionHeaderWithTitle:))
    {
        return NSMenuItem::sectionHeaderWithTitle(&NSString::from_str(title), mtm);
    }
    let item = NSMenuItem::new(mtm);
    let font = NSFont::boldSystemFontOfSize(NSFont::smallSystemFontSize());
    let color = NSColor::secondaryLabelColor();
    // SAFETY: font and color attributes with their documented value types.
    let attributes = unsafe {
        NSDictionary::from_slices(
            &[NSFontAttributeName, NSForegroundColorAttributeName],
            &[&*font as &AnyObject, &*color as &AnyObject],
        )
    };
    // SAFETY: as above.
    let text = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(title),
            Some(&attributes),
        )
    };
    item.setAttributedTitle(Some(&text));
    item.setEnabled(false);
    item
}

/// `title` in the menu font followed by `detail` in the secondary color.
fn detailed_title(title: &str, detail: &str) -> Retained<NSAttributedString> {
    let font = NSFont::menuFontOfSize(0.);
    let secondary = NSColor::secondaryLabelColor();
    // SAFETY: font and color attributes with their documented value types.
    unsafe {
        let main = NSDictionary::from_slices(&[NSFontAttributeName], &[&*font as &AnyObject]);
        let dim = NSDictionary::from_slices(
            &[NSFontAttributeName, NSForegroundColorAttributeName],
            &[&*font as &AnyObject, &*secondary as &AnyObject],
        );
        let text = NSMutableAttributedString::initWithString_attributes(
            NSMutableAttributedString::alloc(),
            &NSString::from_str(title),
            Some(&main),
        );
        text.appendAttributedString(&NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(&format!("   {detail}")),
            Some(&dim),
        ));
        Retained::into_super(text)
    }
}

fn fill_menu(
    menu: &NSMenu,
    items: &[Item],
    path: &[usize],
    target: &Target,
    mtm: MainThreadMarker,
) {
    for (index, entry) in items.iter().enumerate() {
        if entry.is_separator() {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            continue;
        }
        if entry.header {
            menu.addItem(&header_item(&entry.title, mtm));
            continue;
        }
        let title = bounded_title(&entry.title);
        let truncated = title != entry.title;
        // SAFETY: selectItem: is implemented above with AppKit's action signature.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&title),
                entry.action.as_ref().map(|_| sel!(selectItem:)),
                &NSString::from_str(&entry.key),
            )
        };
        if !entry.key.is_empty() && entry.modifiers != 0 {
            item.setKeyEquivalentModifierMask(modifier_flags(entry.modifiers));
        }
        if let Some(detail) = entry.detail.as_deref().filter(|d| !d.is_empty()) {
            item.setAttributedTitle(Some(&detailed_title(&title, &bounded_title(detail))));
        }
        item.setEnabled(entry.action.is_some() || !entry.children.is_empty());
        item.setState(if entry.checked { 1 } else { 0 });
        // Keep full context reachable without letting long errors, app
        // names or paths stretch the native menu across the screen.
        if truncated {
            item.setToolTip(Some(&NSString::from_str(&entry.title)));
        }
        if let Some(action) = &entry.action {
            // SAFETY: represented objects are retained NSStrings. MenuBar
            // retains the target longer than the installed menu.
            unsafe {
                item.setRepresentedObject(Some(&NSString::from_str(action)));
                item.setTarget(Some(target));
            }
        }
        if !entry.children.is_empty() {
            let submenu = NSMenu::new(mtm);
            submenu.setAutoenablesItems(false);
            submenu.setDelegate(Some(ProtocolObject::from_ref(target)));
            let mut child = path.to_vec();
            child.push(index);
            target
                .ivars()
                .lazy
                .borrow_mut()
                .push((submenu.clone(), child, Cell::new(false)));
            item.setSubmenu(Some(&submenu));
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
    use super::*;
    #[test]
    fn refuses_appkit_off_the_main_thread() {
        assert!(super::MenuBar::new(|_| {}).is_none());
    }
    #[test]
    fn lazy_paths_find_nested_children() {
        let mut leaf = Item::label("leaf");
        leaf.children = vec![Item::label("deep")];
        let mut middle = Item::label("middle");
        middle.children = vec![Item::label("a"), leaf];
        let items = vec![Item::label("top"), middle];
        assert_eq!(children_at(&items, &[1]).map(<[Item]>::len), Some(2));
        assert_eq!(children_at(&items, &[1, 1]).unwrap()[0].title, "deep");
        assert!(children_at(&items, &[5]).is_none());
        assert!(Item::separator().is_separator());
        assert!(!Item::header("").is_separator());
    }
}
