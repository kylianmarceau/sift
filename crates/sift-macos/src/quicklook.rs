//! Quick Look previews of the selected file, with the standard
//! `QLPreviewPanel`.
//!
//! The panel picks its data source from the responder chain of the key
//! window. A small responder that adopts `QLPreviewPanelDataSource` is
//! inserted right after the launcher's window in that chain, and the panel
//! is ordered front without becoming key, so typing stays in the launcher.

use std::cell::RefCell;
use std::path::Path;

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSResponder, NSWindow, NSWindowDelegate};
use objc2_foundation::{NSInteger, NSNotification, NSString, NSURL};
use objc2_quick_look_ui::{QLPreviewItem, QLPreviewPanel, QLPreviewPanelDataSource};

#[derive(Default)]
struct Ivars {
    item: RefCell<Option<Retained<NSURL>>>,
    /// The launcher window, which keeps the keyboard while previewing.
    launcher: RefCell<Option<Retained<NSWindow>>>,
}

define_class!(
    // SAFETY: NSResponder has no subclassing requirements, and the class
    // does not implement Drop.
    #[unsafe(super(NSResponder))]
    #[thread_kind = MainThreadOnly]
    #[name = "SiftQuickLookController"]
    #[ivars = Ivars]
    struct Controller;

    impl Controller {
        #[unsafe(method(acceptsPreviewPanelControl:))]
        fn accepts_control(&self, _panel: Option<&QLPreviewPanel>) -> bool {
            true
        }

        #[unsafe(method(beginPreviewPanelControl:))]
        fn begin_control(&self, panel: Option<&QLPreviewPanel>) {
            if let Some(panel) = panel {
                // SAFETY: the panel keeps weak references; `QuickLook`
                // keeps this controller alive for as long as it is in use.
                unsafe {
                    panel.setDataSource(Some(ProtocolObject::from_ref(self)));
                    panel.setDelegate(Some(self));
                }
            }
        }

        #[unsafe(method(endPreviewPanelControl:))]
        fn end_control(&self, panel: Option<&QLPreviewPanel>) {
            if let Some(panel) = panel {
                // SAFETY: clearing the data source and delegate is valid.
                unsafe {
                    panel.setDataSource(None);
                    panel.setDelegate(None);
                }
            }
        }
    }

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSWindowDelegate for Controller {
        /// The preview panel makes itself key when it appears (and when
        /// clicked); give the keyboard back to the launcher so typing,
        /// arrows and Esc keep working there. Scrolling the preview still
        /// works, since scroll events go to the window under the pointer.
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            if let Some(launcher) = self.ivars().launcher.borrow().as_ref()
                && launcher.isVisible()
            {
                launcher.makeKeyWindow();
            }
        }
    }

    unsafe impl QLPreviewPanelDataSource for Controller {
        #[unsafe(method(numberOfPreviewItemsInPreviewPanel:))]
        unsafe fn number_of_items(&self, _panel: Option<&QLPreviewPanel>) -> NSInteger {
            NSInteger::from(self.ivars().item.borrow().is_some())
        }

        #[unsafe(method_id(previewPanel:previewItemAtIndex:))]
        unsafe fn item_at(
            &self,
            _panel: Option<&QLPreviewPanel>,
            index: NSInteger,
        ) -> Option<Retained<ProtocolObject<dyn QLPreviewItem>>> {
            let url = if index == 0 {
                self.ivars().item.borrow().clone()
            } else {
                None
            };
            url.map(ProtocolObject::from_retained)
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars::default());
        // SAFETY: NSResponder's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Quick Look for the launcher window.
pub struct QuickLook {
    controller: Retained<Controller>,
    window: Retained<NSWindow>,
    mtm: MainThreadMarker,
}

impl QuickLook {
    /// Inserts the preview controller into `window`'s responder chain.
    fn attach(window: Retained<NSWindow>, mtm: MainThreadMarker) -> Self {
        let controller = Controller::new(mtm);
        *controller.ivars().launcher.borrow_mut() = Some(window.clone());
        // SAFETY: `nextResponder` is not retained by AppKit. The controller
        // is owned by the returned `QuickLook`, which takes it back out of
        // the chain when dropped; whatever followed the window (normally
        // nothing) is kept after the controller.
        unsafe {
            controller.setNextResponder(window.nextResponder().as_deref());
            window.setNextResponder(Some(&controller));
        }
        Self {
            controller,
            window,
            mtm,
        }
    }

    fn panel(&self) -> Option<Retained<QLPreviewPanel>> {
        // SAFETY: called on the main thread (we hold a marker).
        unsafe { QLPreviewPanel::sharedPreviewPanel(self.mtm) }
    }

    fn existing_panel(&self) -> Option<Retained<QLPreviewPanel>> {
        // SAFETY: as above; checking first avoids creating the panel.
        if unsafe { QLPreviewPanel::sharedPreviewPanelExists(self.mtm) } {
            self.panel()
        } else {
            None
        }
    }

    pub fn is_visible(&self) -> bool {
        self.existing_panel().is_some_and(|p| p.isVisible())
    }

    /// Whether the preview panel has keyboard focus (the user clicked it).
    pub fn is_key(&self) -> bool {
        self.existing_panel().is_some_and(|p| p.isKeyWindow())
    }

    /// Shows `path` in the preview panel, above the launcher, without
    /// taking keyboard focus from it.
    pub fn show(&self, path: &Path) {
        self.set_item(Some(path));
        let Some(panel) = self.panel() else {
            return;
        };
        // The launcher's app is never active (its panel is non-activating);
        // a panel that hides on deactivation would vanish at once.
        panel.setHidesOnDeactivate(false);
        panel.setLevel(self.window.level() + 1);
        panel.orderFront(None);
        // SAFETY: main thread; asks the panel to find its controller now.
        unsafe {
            panel.updateController();
            panel.reloadData();
        }
    }

    /// Sets the item and lets the panel find its controller, without
    /// showing it (for checking the wiring; see `examples/quicklook_check`).
    pub fn prepare(&self, path: &Path) {
        self.set_item(Some(path));
        if let Some(panel) = self.panel() {
            // SAFETY: main thread.
            unsafe { panel.updateController() };
        }
    }

    /// Follows the selection while the panel is open: previews `path`, or
    /// closes the panel when the selection has no file.
    pub fn follow(&self, path: Option<&Path>) {
        if !self.is_visible() {
            return;
        }
        match path {
            Some(path) => {
                self.set_item(Some(path));
                if let Some(panel) = self.existing_panel() {
                    // SAFETY: main thread.
                    unsafe { panel.reloadData() };
                }
            }
            None => self.close(),
        }
    }

    pub fn close(&self) {
        if let Some(panel) = self.existing_panel()
            && panel.isVisible()
        {
            panel.orderOut(None);
        }
        self.set_item(None);
    }

    fn set_item(&self, path: Option<&Path>) {
        let url = path
            .and_then(Path::to_str)
            .map(|p| NSURL::fileURLWithPath(&NSString::from_str(p)));
        *self.controller.ivars().item.borrow_mut() = url;
    }

    /// Whether the panel's current data source is this controller (for
    /// checking the responder-chain wiring).
    pub fn is_controlling(&self) -> bool {
        let Some(panel) = self.existing_panel() else {
            return false;
        };
        // SAFETY: main thread.
        let current = unsafe { panel.currentController() };
        current.is_some_and(|c| same(&*c, &*self.controller))
    }
}

impl Drop for QuickLook {
    fn drop(&mut self) {
        self.close();
        // Take the controller back out of the chain before it is freed.
        // SAFETY: both responders are alive here (see `attach`).
        unsafe {
            if self
                .window
                .nextResponder()
                .is_some_and(|r| same(&*r, &*self.controller))
            {
                self.window
                    .setNextResponder(self.controller.nextResponder().as_deref());
            }
        }
    }
}

/// Whether two references point at the same Objective-C object.
fn same<A, B>(a: &A, b: &B) -> bool {
    std::ptr::eq((a as *const A).cast::<()>(), (b as *const B).cast::<()>())
}

impl crate::Panel {
    /// Quick Look support for this window.
    pub fn quick_look(&self) -> QuickLook {
        QuickLook::attach(self.window(), self.mtm())
    }
}
