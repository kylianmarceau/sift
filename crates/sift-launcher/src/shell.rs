//! Runs custom shell commands from the config file.

use std::process::{Command, Stdio};

use sift_macos::LaunchError;

/// Bytes of a failing command's stderr kept in the log.
const STDERR_LOG_BYTES: usize = 500;

/// Starts `command` with `/bin/sh -c` in the home folder, with no input,
/// and returns at once. A background thread waits for it and logs the exit
/// status (and stderr, when it fails). The command line comes only from
/// the user's own config file.
pub fn spawn(command: &str) -> Result<(), LaunchError> {
    let mut child = Command::new("/bin/sh");
    child
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(home) = std::env::home_dir() {
        child.current_dir(home);
    }
    let child = child
        .spawn()
        .map_err(|e| LaunchError::Command(e.to_string()))?;
    let label = command.to_owned();
    std::thread::Builder::new()
        .name("sift-shell".into())
        .spawn(move || match child.wait_with_output() {
            Ok(output) if output.status.success() => {
                tracing::info!(command = %label, "command finished");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr: String = stderr.chars().take(STDERR_LOG_BYTES).collect();
                tracing::warn!(command = %label, status = %output.status, %stderr, "command failed");
            }
            Err(error) => tracing::warn!(command = %label, %error, "command wait failed"),
        })
        .map_err(|e| LaunchError::Command(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_the_command_in_the_background() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("ran");
        spawn(&format!("touch '{}'", marker.display())).expect("spawn");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !marker.exists() {
            assert!(std::time::Instant::now() < deadline, "command did not run");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
