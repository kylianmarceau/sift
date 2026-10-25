//! Minimal C ABI surface, kept here so no AX handles escape their worker.
#![allow(non_snake_case)]
use std::ffi::{c_char, c_void};

pub type Ref = *const c_void;
pub type MutRef = *mut c_void;
pub type AXError = i32;
pub type Index = isize;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}
#[repr(C)]
pub struct SourceContext {
    pub version: Index,
    pub info: MutRef,
    pub retain: Option<unsafe extern "C" fn(Ref) -> Ref>,
    pub release: Option<unsafe extern "C" fn(Ref)>,
    pub copy_description: Option<unsafe extern "C" fn(Ref) -> Ref>,
    pub equal: Option<unsafe extern "C" fn(Ref, Ref) -> u8>,
    pub hash: Option<unsafe extern "C" fn(Ref) -> usize>,
    pub schedule: Option<unsafe extern "C" fn(MutRef, Ref, Ref)>,
    pub cancel: Option<unsafe extern "C" fn(MutRef, Ref, Ref)>,
    pub perform: Option<unsafe extern "C" fn(MutRef)>,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    pub fn AXIsProcessTrusted() -> bool;
    pub fn AXIsProcessTrustedWithOptions(options: Ref) -> bool;
    pub fn AXUIElementCreateApplication(pid: i32) -> Ref;
    pub fn AXUIElementCreateSystemWide() -> Ref;
    pub fn AXUIElementCopyElementAtPosition(
        element: Ref,
        x: f32,
        y: f32,
        result: *mut Ref,
    ) -> AXError;
    pub fn AXUIElementCopyAttributeValue(element: Ref, attribute: Ref, value: *mut Ref) -> AXError;
    pub fn AXUIElementSetAttributeValue(element: Ref, attribute: Ref, value: Ref) -> AXError;
    pub fn AXUIElementPerformAction(element: Ref, action: Ref) -> AXError;
    pub fn AXUIElementSetMessagingTimeout(element: Ref, seconds: f32) -> AXError;
    pub fn AXObserverCreate(
        pid: i32,
        callback: unsafe extern "C" fn(Ref, Ref, Ref, MutRef),
        observer: *mut Ref,
    ) -> AXError;
    pub fn AXObserverAddNotification(
        observer: Ref,
        element: Ref,
        notification: Ref,
        context: MutRef,
    ) -> AXError;
    pub fn AXObserverGetRunLoopSource(observer: Ref) -> Ref;
    pub fn AXValueCreate(kind: u32, value: Ref) -> Ref;
    pub fn AXValueGetValue(value: Ref, kind: u32, out: MutRef) -> bool;
    pub fn AXValueGetType(value: Ref) -> u32;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub fn CGEventCreate(source: Ref) -> Ref;
    pub fn CGEventGetLocation(event: Ref) -> Point;
    pub fn CGWarpMouseCursorPosition(point: Point) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub static kCFRunLoopDefaultMode: Ref;
    pub static kCFBooleanTrue: Ref;
    pub static kCFBooleanFalse: Ref;
    pub fn CFRelease(value: Ref);
    pub fn CFRetain(value: Ref) -> Ref;
    pub fn CFEqual(a: Ref, b: Ref) -> bool;
    pub fn CFGetTypeID(value: Ref) -> usize;
    pub fn CFArrayGetTypeID() -> usize;
    pub fn CFArrayGetCount(array: Ref) -> Index;
    pub fn CFArrayGetValueAtIndex(array: Ref, index: Index) -> Ref;
    pub fn CFStringGetTypeID() -> usize;
    pub fn CFStringCreateWithCString(allocator: Ref, string: *const c_char, encoding: u32) -> Ref;
    pub fn CFStringGetLength(string: Ref) -> Index;
    pub fn CFStringGetMaximumSizeForEncoding(length: Index, encoding: u32) -> Index;
    pub fn CFStringGetCString(string: Ref, buffer: *mut c_char, size: Index, encoding: u32)
    -> bool;
    pub fn CFRunLoopGetCurrent() -> Ref;
    pub fn CFRunLoopRun();
    pub fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source: bool) -> i32;
    pub fn CFRunLoopStop(run_loop: Ref);
    pub fn CFRunLoopWakeUp(run_loop: Ref);
    pub fn CFRunLoopSourceCreate(allocator: Ref, order: Index, context: *mut SourceContext) -> Ref;
    pub fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: Ref);
    pub fn CFRunLoopRemoveSource(run_loop: Ref, source: Ref, mode: Ref);
    pub fn CFRunLoopSourceSignal(source: Ref);
}

/// An owned +1 CF object. Never Send/Sync: AX objects stay on one app worker.
pub struct Owned(pub Ref, std::marker::PhantomData<std::rc::Rc<()>>);
impl Owned {
    pub unsafe fn new(value: Ref) -> Option<Self> {
        (!value.is_null()).then_some(Self(value, std::marker::PhantomData))
    }
    pub fn string(text: &str) -> Self {
        let text = std::ffi::CString::new(text).expect("static AX keys contain no NUL");
        // SAFETY: CString is NUL terminated and UTF-8. CF copies its contents.
        let value =
            unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), 0x08000100) };
        Self(value, std::marker::PhantomData)
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

pub fn string(value: Ref) -> String {
    if value.is_null() {
        return String::new();
    }
    // SAFETY: callers supply a retained CF value; check type before reading.
    unsafe {
        if CFGetTypeID(value) != CFStringGetTypeID() {
            return String::new();
        }
        let length = CFStringGetMaximumSizeForEncoding(CFStringGetLength(value), 0x08000100);
        if !(0..=65_536).contains(&length) {
            return String::new();
        }
        let mut bytes = vec![0u8; length as usize + 1];
        if !CFStringGetCString(
            value,
            bytes.as_mut_ptr().cast(),
            bytes.len() as isize,
            0x08000100,
        ) {
            return String::new();
        }
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }
}
