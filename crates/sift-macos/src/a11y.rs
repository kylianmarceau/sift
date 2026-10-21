//! Accessibility: system display settings and VoiceOver announcements.
//!
//! GPUI draws the launcher into one Metal view and exposes no accessibility
//! tree, so VoiceOver cannot read the result list by itself. Instead the
//! launcher asks VoiceOver to speak what changed (the selected result, the
//! number of results, messages) through AppKit's announcement notification.
//! Nothing here needs a permission: these are the same calls any app uses to
//! read its own accessibility settings and talk to VoiceOver.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSAccessibilityAnnouncementKey, NSAccessibilityAnnouncementRequestedNotification,
    NSAccessibilityPostNotificationWithUserInfo, NSAccessibilityPriorityKey,
    NSAccessibilityPriorityLevel, NSApplication, NSColor, NSColorSpace, NSWorkspace,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString};

/// Display settings from System Settings › Accessibility › Display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DisplayOptions {
    pub increase_contrast: bool,
    pub reduce_transparency: bool,
    pub differentiate_without_color: bool,
}

/// Reads the current display settings (cheap; read them when showing).
/// Main thread only: elsewhere it returns the defaults without asking
/// AppKit (calling AppKit from other threads crashed now and then in
/// tests).
pub fn display_options() -> DisplayOptions {
    if MainThreadMarker::new().is_none() {
        return DisplayOptions::default();
    }
    let workspace = NSWorkspace::sharedWorkspace();
    DisplayOptions {
        increase_contrast: workspace.accessibilityDisplayShouldIncreaseContrast(),
        reduce_transparency: workspace.accessibilityDisplayShouldReduceTransparency(),
        differentiate_without_color: workspace
            .accessibilityDisplayShouldDifferentiateWithoutColor(),
    }
}

/// The accent colour chosen in System Settings › Appearance, as sRGB
/// components in 0–1. Main thread only (`None` elsewhere).
pub fn accent_color() -> Option<[f32; 3]> {
    MainThreadMarker::new()?;
    let color =
        NSColor::controlAccentColor().colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    Some([
        color.redComponent() as f32,
        color.greenComponent() as f32,
        color.blueComponent() as f32,
    ])
}

/// Whether VoiceOver is running. Main thread only (false elsewhere).
pub fn voice_over_enabled() -> bool {
    MainThreadMarker::new().is_some() && NSWorkspace::sharedWorkspace().isVoiceOverEnabled()
}

/// How an announcement treats speech already in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Waits for current speech (for example, the echo of a typed letter).
    Polite,
    /// Interrupts current speech (the selection moved).
    Interrupt,
}

/// Asks VoiceOver (if running) to speak `text`. Main thread only; does
/// nothing elsewhere.
pub fn announce(text: &str, urgency: Urgency) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let priority = match urgency {
        // NSAccessibilityPriorityMedium and NSAccessibilityPriorityHigh.
        Urgency::Polite => NSAccessibilityPriorityLevel(50),
        Urgency::Interrupt => NSAccessibilityPriorityLevel(90),
    };
    let message: Retained<AnyObject> = NSString::from_str(text).into();
    let priority: Retained<AnyObject> = NSNumber::new_isize(priority.0).into();
    // SAFETY: the keys are AppKit's own constants, the values have the
    // types AppKit documents for them (a string and an NSNumber holding an
    // NSAccessibilityPriorityLevel), and `app` is a live object.
    unsafe {
        let info = NSDictionary::from_retained_objects(
            &[NSAccessibilityAnnouncementKey, NSAccessibilityPriorityKey],
            &[message, priority],
        );
        NSAccessibilityPostNotificationWithUserInfo(
            &app,
            NSAccessibilityAnnouncementRequestedNotification,
            Some(&info),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_the_main_thread_nothing_reaches_appkit() {
        // Test threads are not the main thread.
        assert_eq!(display_options(), DisplayOptions::default());
        assert!(!voice_over_enabled());
        assert_eq!(accent_color(), None);
        announce("test", Urgency::Polite);
    }
}
