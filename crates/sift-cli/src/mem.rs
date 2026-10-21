//! Process memory measurement.
//!
//! "Footprint" is the figure Activity Monitor shows as Memory: dirty and
//! compressed anonymous memory owned by the process. It excludes clean,
//! file-backed pages such as the memory-mapped name snapshot, which the
//! kernel can drop and re-read at will. Resident set size (RSS) is reported
//! alongside because it does include those mapped pages.

#[derive(Debug, Clone, Copy)]
pub struct MemoryUsage {
    /// Current physical footprint in bytes.
    pub footprint: u64,
    /// Highest physical footprint over the process lifetime, in bytes.
    pub peak_footprint: u64,
    /// Peak resident set size in bytes.
    pub peak_rss: u64,
}

#[cfg(target_os = "macos")]
pub fn usage() -> Option<MemoryUsage> {
    // SAFETY: both structs are plain data that the kernel fills in; they are
    // zero-initialised and passed with the flavor matching their layout.
    unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        let rc = libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            (&raw mut info).cast::<libc::rusage_info_t>(),
        );
        if rc != 0 {
            return None;
        }
        let mut rusage: libc::rusage = std::mem::zeroed();
        // On macOS ru_maxrss is in bytes (on Linux it is kilobytes).
        let peak_rss = if libc::getrusage(libc::RUSAGE_SELF, &mut rusage) == 0 {
            rusage.ru_maxrss as u64
        } else {
            0
        };
        Some(MemoryUsage {
            footprint: info.ri_phys_footprint,
            peak_footprint: info.ri_lifetime_max_phys_footprint,
            peak_rss,
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub fn usage() -> Option<MemoryUsage> {
    None
}

pub fn describe() -> String {
    match usage() {
        Some(m) => format!(
            "footprint {} (peak {}), peak RSS {}",
            bytes(m.footprint),
            bytes(m.peak_footprint),
            bytes(m.peak_rss)
        ),
        None => "memory usage unavailable".to_string(),
    }
}

/// Formats a byte count with binary units.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
