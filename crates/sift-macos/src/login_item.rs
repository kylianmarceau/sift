//! Open at Login, through `SMAppService` (macOS 13 and later).
//!
//! Registering the main app adds Sift to System Settings › General ›
//! Login Items, where the user can see and remove it. It only works for an
//! app bundle; a bare development binary reports [`Status::Unavailable`].
//! Nothing here runs unless the user turns the option on in Settings.
//!
//! The class is looked up at runtime and called with `msg_send!`, which
//! avoids another binding crate for three methods.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, NSObject};

// Makes the SMAppService class available to the runtime lookup below.
#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

/// Whether Sift opens at login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    On,
    Off,
    /// Registered, but the user must allow it in System Settings ›
    /// General › Login Items.
    NeedsApproval,
    /// Not running from an app bundle, or the system refused to say.
    Unavailable,
}

fn main_app_service() -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(c"SMAppService")?;
    // SAFETY: +[SMAppService mainAppService] takes no arguments and returns
    // a non-null, autoreleased SMAppService.
    unsafe { msg_send![class, mainAppService] }
}

/// The current state.
pub fn status() -> Status {
    let Some(service) = main_app_service() else {
        return Status::Unavailable;
    };
    // SAFETY: -[SMAppService status] returns an SMAppServiceStatus
    // (NSInteger).
    let status: isize = unsafe { msg_send![&service, status] };
    tracing::debug!(status, "SMAppService status");
    match status {
        0 => Status::Off,
        1 => Status::On,
        2 => Status::NeedsApproval,
        // SMAppServiceStatusNotFound: also what an app bundle reports before
        // it was ever registered, so only a bare binary is unavailable.
        3 if running_from_app_bundle() => Status::Off,
        _ => Status::Unavailable,
    }
}

fn running_from_app_bundle() -> bool {
    objc2_foundation::NSBundle::mainBundle()
        .bundlePath()
        .to_string()
        .ends_with(".app")
}

/// Turns Open at Login on or off. Returns the new state, or the system's
/// explanation if it refused.
pub fn set(on: bool) -> Result<Status, String> {
    let service = main_app_service().ok_or("Open at Login needs macOS 13 or later")?;
    // SAFETY: both methods take an NSError out-parameter and return BOOL;
    // `error: _` makes objc2 turn NO into the returned error.
    let result: Result<(), Retained<NSObject>> = unsafe {
        if on {
            msg_send![&service, registerAndReturnError: _]
        } else {
            msg_send![&service, unregisterAndReturnError: _]
        }
    };
    match result {
        Ok(()) => Ok(status()),
        Err(error) => {
            // SAFETY: the error is an NSError; localizedDescription returns
            // an NSString.
            let text: Option<Retained<objc2_foundation::NSString>> =
                unsafe { msg_send![&error, localizedDescription] };
            Err(text.map_or_else(|| "the system refused".to_string(), |t| t.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only reads the state: registering would change this Mac's login
    /// items. A test binary is not an app bundle.
    #[test]
    fn a_bare_binary_reports_unavailable_or_off() {
        // The framework is linked, so the class exists on macOS 13+.
        assert!(AnyClass::get(c"SMAppService").is_some());
        let status = status();
        eprintln!("login item status of the test binary: {status:?}");
        assert!(matches!(status, Status::Unavailable | Status::Off));
    }
}
