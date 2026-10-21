//! Filesystem change notifications from macOS FSEvents.
//!
//! FSEvents reports *directories* whose contents changed, not individual
//! file operations, and may coalesce many changes into one event. That is
//! exactly what the incremental indexer needs (`sift_indexer::Change`):
//! it re-lists each reported directory and compares it with the index.
//!
//! Every event carries a system-wide, monotonically increasing event id.
//! Saving the last applied id lets a later process ask for everything that
//! happened since (`since` in [`Watcher::start`]), so changes made while
//! sift was not running are not missed. The ids belong to a per-volume
//! event store identified by [`event_store_uuid`]; if that UUID changes,
//! saved ids are meaningless and the index must be rescanned.
//!
//! Events are delivered on a private dispatch queue, never on the main
//! thread.

use std::ffi::{CStr, c_char, c_void};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFUUID};
use objc2_core_services::{
    ConstFSEventStreamRef, FSEventStreamContext, FSEventStreamCreate, FSEventStreamEventFlags,
    FSEventStreamEventId, FSEventStreamFlushSync, FSEventStreamGetLatestEventId,
    FSEventStreamInvalidate, FSEventStreamRef, FSEventStreamRelease, FSEventStreamSetDispatchQueue,
    FSEventStreamStart, FSEventStreamStop, FSEventsCopyUUIDForDevice, FSEventsGetCurrentEventId,
    kFSEventStreamCreateFlagNoDefer, kFSEventStreamCreateFlagWatchRoot,
    kFSEventStreamEventIdSinceNow,
};

/// What FSEvents says about an event, beyond its path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags(pub u32);

impl Flags {
    const MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
    const USER_DROPPED: u32 = 0x0000_0002;
    const KERNEL_DROPPED: u32 = 0x0000_0004;
    const EVENT_IDS_WRAPPED: u32 = 0x0000_0008;
    const HISTORY_DONE: u32 = 0x0000_0010;
    const ROOT_CHANGED: u32 = 0x0000_0020;
    const MOUNT: u32 = 0x0000_0040;
    const UNMOUNT: u32 = 0x0000_0080;

    fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }

    /// Changes below the path were coalesced or lost: rescan the whole
    /// subtree, not just the directory.
    pub fn must_scan_subdirs(self) -> bool {
        self.has(Self::MUST_SCAN_SUBDIRS)
    }

    /// Events were dropped (by this process falling behind, or by the
    /// kernel). FSEvents also sets `must_scan_subdirs` on these.
    pub fn dropped(self) -> bool {
        self.has(Self::USER_DROPPED) || self.has(Self::KERNEL_DROPPED)
    }

    /// The 64-bit event id counter wrapped. Saved ids are no longer
    /// comparable; rescan everything.
    pub fn ids_wrapped(self) -> bool {
        self.has(Self::EVENT_IDS_WRAPPED)
    }

    /// Marks the end of the replayed history requested with `since`.
    /// Carries no path change.
    pub fn history_done(self) -> bool {
        self.has(Self::HISTORY_DONE)
    }

    /// A watched path itself (or one of its parents) was moved, deleted or
    /// recreated.
    pub fn root_changed(self) -> bool {
        self.has(Self::ROOT_CHANGED)
    }

    pub fn mount(self) -> bool {
        self.has(Self::MOUNT)
    }

    pub fn unmount(self) -> bool {
        self.has(Self::UNMOUNT)
    }
}

/// One FSEvents event: the directory whose contents changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub path: PathBuf,
    pub id: u64,
    pub flags: Flags,
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("no paths to watch")]
    NoPaths,
    #[error("FSEvents could not create an event stream")]
    Create,
    #[error("FSEvents could not start the event stream")]
    Start,
}

/// The id of the most recent filesystem event on this Mac. Read it before a
/// full scan and save it with the scan: replaying from it later covers
/// every change made during and after the scan.
pub fn current_event_id() -> u64 {
    // SAFETY: takes no arguments and only reads system state.
    unsafe { FSEventsGetCurrentEventId() }
}

/// UUID of the FSEvents event store of the volume `path` is on. Event ids
/// saved for that volume are valid only while this stays the same.
/// `None` if the volume has no event store (for example some network or
/// read-only volumes).
pub fn event_store_uuid(path: &Path) -> Option<String> {
    let dev = std::fs::metadata(path).ok()?.dev();
    // SAFETY: plain C call taking a device number; returns a +1 retained
    // CFUUID or NULL, which the binding wraps.
    let uuid: CFRetained<CFUUID> = unsafe { FSEventsCopyUUIDForDevice(dev as libc::dev_t) }?;
    CFUUID::new_string(None, Some(&uuid)).map(|s| s.to_string())
}

type Handler = Box<dyn Fn(Vec<Event>) + Send + Sync + 'static>;

/// A running FSEvents stream. Events are delivered to the handler on a
/// background dispatch queue, in batches, at most once per `latency`.
/// Dropping the watcher stops the stream; no callback runs afterwards.
pub struct Watcher {
    stream: FSEventStreamRef,
    queue: DispatchRetained<DispatchQueue>,
    handler: *mut Handler,
}

// SAFETY: the stream is only used through `&self`/`Drop` with functions
// that Apple documents as callable from any thread once the stream is
// scheduled on a dispatch queue; the handler is `Send + Sync`.
unsafe impl Send for Watcher {}

impl Watcher {
    /// Watches `paths` (and everything below them).
    ///
    /// `since`: replay events after this id before live events start, or
    /// `None` to start from now. Replayed history ends with an event whose
    /// [`Flags::history_done`] is set.
    ///
    /// Paths are passed to FSEvents as strings; a path that is not valid
    /// UTF-8 is converted lossily (APFS and HFS+ names are always UTF-8).
    pub fn start(
        paths: &[PathBuf],
        since: Option<u64>,
        latency: Duration,
        handler: impl Fn(Vec<Event>) + Send + Sync + 'static,
    ) -> Result<Self, WatchError> {
        if paths.is_empty() {
            return Err(WatchError::NoPaths);
        }
        let strings: Vec<CFRetained<CFString>> = paths
            .iter()
            .map(|p| CFString::from_str(&p.to_string_lossy()))
            .collect();
        let refs: Vec<&CFString> = strings.iter().map(|s| &**s).collect();
        let array = CFArray::from_objects(&refs);

        let handler: *mut Handler = Box::into_raw(Box::new(Box::new(handler)));
        let mut context = FSEventStreamContext {
            version: 0,
            info: handler.cast(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        // Directory-level events (no FileEvents flag): one event per changed
        // directory, which is what the indexer consumes. NoDefer delivers
        // the first change of a quiet period immediately; WatchRoot reports
        // the watched folders themselves being moved or deleted.
        let flags = kFSEventStreamCreateFlagNoDefer | kFSEventStreamCreateFlagWatchRoot;
        // SAFETY: `array` holds CFStrings; `context` is copied by FSEvents;
        // `handler` stays alive until `Drop` after the stream is
        // invalidated.
        let stream = unsafe {
            FSEventStreamCreate(
                None,
                Some(callback),
                &mut context,
                array.as_opaque(),
                since.unwrap_or(kFSEventStreamEventIdSinceNow),
                latency.as_secs_f64(),
                flags,
            )
        };
        if stream.is_null() {
            // SAFETY: created above and not shared with FSEvents.
            drop(unsafe { Box::from_raw(handler) });
            return Err(WatchError::Create);
        }

        let queue = DispatchQueue::new("io.github.kylianmarceau.sift.fsevents", None);
        // SAFETY: `stream` is a valid, not yet scheduled stream.
        let started = unsafe {
            FSEventStreamSetDispatchQueue(stream, Some(&queue));
            FSEventStreamStart(stream)
        };
        let watcher = Self {
            stream,
            queue,
            handler,
        };
        if !started {
            // Drop releases the stream and the handler.
            return Err(WatchError::Start);
        }
        Ok(watcher)
    }

    /// Blocks until all events that happened before this call have been
    /// delivered to the handler.
    pub fn flush(&self) {
        // SAFETY: the stream is valid and started.
        unsafe { FSEventStreamFlushSync(self.stream) }
    }

    /// The id of the last event delivered to the handler.
    pub fn latest_event_id(&self) -> u64 {
        // SAFETY: the stream is valid.
        unsafe { FSEventStreamGetLatestEventId(self.stream.cast_const()) }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: the stream is valid; stopping an unstarted stream is
        // allowed. After invalidation FSEvents schedules no new callbacks,
        // and the empty synchronous block waits out one that is already
        // running on the queue, so freeing the handler afterwards is safe.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
        }
        self.queue.exec_sync(|| {});
        // SAFETY: allocated in `start`, no longer referenced by FSEvents.
        drop(unsafe { Box::from_raw(self.handler) });
    }
}

unsafe extern "C-unwind" fn callback(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: NonNull<c_void>,
    flags: NonNull<FSEventStreamEventFlags>,
    ids: NonNull<FSEventStreamEventId>,
) {
    // SAFETY: FSEvents passes `count` C strings (no UseCFTypes flag),
    // `count` flags and `count` ids, valid for the duration of the call;
    // `info` is the handler pointer from `start`.
    let (handler, events) = unsafe {
        let handler = &*(info as *const Handler);
        let paths = std::slice::from_raw_parts(paths.as_ptr() as *const *const c_char, count);
        let flags = std::slice::from_raw_parts(flags.as_ptr(), count);
        let ids = std::slice::from_raw_parts(ids.as_ptr(), count);
        let events: Vec<Event> = (0..count)
            .map(|i| Event {
                path: path_from_c(paths[i]),
                id: ids[i],
                flags: Flags(flags[i]),
            })
            .collect();
        (handler, events)
    };
    // A panic must not unwind into FSEvents' dispatch machinery.
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(events))).is_err() {
        tracing::error!("filesystem event handler panicked");
    }
}

/// # Safety
/// `ptr` must be a valid NUL-terminated string.
unsafe fn path_from_c(ptr: *const c_char) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: guaranteed by the caller.
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    // FSEvents reports directory paths with a trailing slash.
    let trimmed = match bytes {
        [rest @ .., b'/'] if !rest.is_empty() => rest,
        other => other,
    };
    PathBuf::from(std::ffi::OsStr::from_bytes(trimmed))
}
