//! Native Spaces awareness. While a native fullscreen app (or another
//! desktop) is the active Space, many applications leave windows on other
//! Spaces out of `AXWindows`. Those windows are still open; treating them as
//! closed loses their workspace and the frame saved before parking them.
use super::ffi::*;
use std::ffi::CString;
use std::sync::OnceLock;

type MainConnection = unsafe extern "C" fn() -> i32;
type CopySpaces = unsafe extern "C" fn(i32, i32, Ref) -> Ref;

struct SpacesApi {
    connection: MainConnection,
    copy_spaces: CopySpaces,
}

/// `SLSCopySpacesForWindows` is private, like `_AXUIElementGetWindow`, and
/// is used the same way by other macOS window managers. Resolve it at run
/// time and fall back to the public window list when it is unavailable.
fn spaces_api() -> Option<&'static SpacesApi> {
    static API: OnceLock<Option<SpacesApi>> = OnceLock::new();
    API.get_or_init(|| {
        let lookup = |handle: *mut libc::c_void, name: &str| {
            let name = CString::new(name).expect("literal");
            let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
            (!address.is_null()).then_some(address)
        };
        let path = CString::new("/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight")
            .expect("literal");
        let skylight = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        let candidates = [
            (skylight, "SLSMainConnectionID", "SLSCopySpacesForWindows"),
            (
                libc::RTLD_DEFAULT,
                "CGSMainConnectionID",
                "CGSCopySpacesForWindows",
            ),
        ];
        candidates
            .into_iter()
            .filter(|(handle, _, _)| !handle.is_null())
            .find_map(|(handle, connection, copy)| {
                let connection = lookup(handle, connection)?;
                let copy = lookup(handle, copy)?;
                // SAFETY: both symbols have these C signatures on every macOS
                // version that exports them.
                Some(unsafe {
                    SpacesApi {
                        connection: std::mem::transmute::<*mut libc::c_void, MainConnection>(
                            connection,
                        ),
                        copy_spaces: std::mem::transmute::<*mut libc::c_void, CopySpaces>(copy),
                    }
                })
            })
    })
    .as_ref()
}

/// How many Spaces hold the window, or `None` when macOS cannot say. A
/// closed window, or one an application ordered out, belongs to none.
fn space_count(id: u32) -> Option<isize> {
    let api = spaces_api()?;
    unsafe {
        let number = Owned::new(CFNumberCreate(
            std::ptr::null(),
            K_CF_NUMBER_SINT32_TYPE,
            (&id as *const u32).cast(),
        ))?;
        let values = [number.0];
        let array = Owned::new(CFArrayCreate(
            std::ptr::null(),
            values.as_ptr(),
            1,
            (&raw const kCFTypeArrayCallBacks).cast(),
        ))?;
        // 0x7: the window's Spaces of every kind (user, fullscreen, system).
        let spaces = Owned::new((api.copy_spaces)((api.connection)(), 0x7, array.0))?;
        if CFGetTypeID(spaces.0) != CFArrayGetTypeID() {
            return None;
        }
        Some(CFArrayGetCount(spaces.0))
    }
}

/// Whether the window server still has this window for `pid`, on any Space.
fn window_server_has(id: u32, pid: i32) -> bool {
    unsafe {
        let Some(list) = Owned::new(CGWindowListCopyWindowInfo(
            K_CG_WINDOW_LIST_OPTION_INCLUDING_WINDOW,
            id,
        )) else {
            return false;
        };
        if CFGetTypeID(list.0) != CFArrayGetTypeID() {
            return false;
        }
        (0..CFArrayGetCount(list.0).clamp(0, 16)).any(|index| {
            let info = CFArrayGetValueAtIndex(list.0, index);
            let number = |key: Ref| {
                let value = CFDictionaryGetValue(info, key);
                let mut out = 0i64;
                (!value.is_null()
                    && CFNumberGetValue(
                        value,
                        K_CF_NUMBER_SINT64_TYPE,
                        (&mut out as *mut i64).cast(),
                    ))
                .then_some(out)
            };
            !info.is_null()
                && number(kCGWindowNumber) == Some(i64::from(id))
                && number(kCGWindowOwnerPID) == Some(i64::from(pid))
        })
    }
}

/// A window AX did not list is still open on another Space (for example
/// behind a native fullscreen app). Closed and ordered-out windows are not.
pub(super) fn open_on_another_space(id: u32, pid: i32) -> bool {
    window_server_has(id, pid) && space_count(id).is_none_or(|count| count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_windows_are_not_reported_as_open() {
        // No process owns window 0 or a window for pid -1.
        assert!(!open_on_another_space(0, std::process::id() as i32));
        assert!(!window_server_has(u32::MAX - 1, -1));
    }

    /// Run with `--ignored` in a logged-in session: every normal window the
    /// window server lists is on some Space and is reported as open.
    #[test]
    #[ignore = "needs a logged-in window server"]
    fn live_windows_are_open_on_a_space() {
        let mut checked = 0;
        unsafe {
            let list = Owned::new(CGWindowListCopyWindowInfo(1, 0)).expect("on-screen windows");
            for index in 0..CFArrayGetCount(list.0) {
                let info = CFArrayGetValueAtIndex(list.0, index);
                let number = |key: Ref| {
                    let mut out = 0i64;
                    CFNumberGetValue(
                        CFDictionaryGetValue(info, key),
                        K_CF_NUMBER_SINT64_TYPE,
                        (&mut out as *mut i64).cast(),
                    );
                    out
                };
                let (id, pid) = (
                    number(kCGWindowNumber) as u32,
                    number(kCGWindowOwnerPID) as i32,
                );
                assert!(window_server_has(id, pid));
                assert!(!window_server_has(id, pid + 1));
                if space_count(id).is_some_and(|n| n > 0) {
                    checked += 1;
                    assert!(open_on_another_space(id, pid));
                }
            }
        }
        eprintln!("{checked} on-screen windows have a Space");
        assert!(checked > 0);
    }

    #[test]
    fn the_spaces_query_resolves_on_supported_macos() {
        assert!(spaces_api().is_some(), "SLSCopySpacesForWindows is missing");
        // A window id nobody owns lives on no Space.
        assert_eq!(space_count(u32::MAX - 1).unwrap_or(0), 0);
    }
}
