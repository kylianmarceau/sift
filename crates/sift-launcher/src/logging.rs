//! Logging to stderr and to `~/Library/Logs/Sift/sift.log`.
//!
//! An app started from Finder has nowhere visible for stderr, so warnings,
//! errors and crash reports would be lost. The file keeps them, within
//! limits: it is rotated to `sift.log.1` at startup when it passed 1 MiB,
//! and one session writes at most 4 MiB. A panic hook records the panic
//! and its backtrace before the process ends. Logs stay on this Mac.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const FILE_NAME: &str = "sift.log";
const ROTATE_AT: u64 = 1024 * 1024;
const SESSION_LIMIT: u64 = 4 * 1024 * 1024;

/// `~/Library/Logs/Sift`, or `SIFT_LOG_DIR`.
pub fn log_dir() -> Option<PathBuf> {
    std::env::var_os("SIFT_LOG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join("Library/Logs/Sift")))
}

/// The log file's path, for messages to the user.
pub fn log_path() -> Option<PathBuf> {
    log_dir().map(|d| d.join(FILE_NAME))
}

struct LogFile {
    file: Mutex<Option<(File, u64)>>,
}

struct LogWriter<'a>(&'a LogFile);

impl Write for LogWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.0.file.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((file, written)) = guard.as_mut() {
            if *written < SESSION_LIMIT {
                file.write_all(buf)?;
                *written += buf.len() as u64;
                if *written >= SESSION_LIMIT {
                    let _ = file.write_all(b"(log limit for this session reached)\n");
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut guard = self.0.file.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some((file, _)) => file.flush(),
            None => Ok(()),
        }
    }
}

/// The log file, shared by every write for the life of the process.
#[derive(Clone, Copy)]
struct SharedLog(&'static LogFile);

impl<'a> MakeWriter<'a> for SharedLog {
    type Writer = LogWriter<'static>;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.0)
    }
}

fn open_file() -> Option<File> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(FILE_NAME);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_AT) {
        let _ = std::fs::rename(&path, dir.join(format!("{FILE_NAME}.1")));
    }
    File::options().create(true).append(true).open(path).ok()
}

/// Sets up logging (stderr at `stderr_level`, the file at INFO) and the
/// panic hook.
pub fn init(stderr_level: tracing::Level) {
    let file: &'static LogFile = Box::leak(Box::new(LogFile {
        file: Mutex::new(open_file().map(|f| (f, 0))),
    }));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .without_time()
                .with_filter(LevelFilter::from_level(stderr_level)),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(SharedLog(file))
                .with_ansi(false)
                .with_filter(LevelFilter::INFO),
        )
        .init();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(panic = %info, %backtrace, "Sift crashed");
        default_hook(info);
    }));
}
