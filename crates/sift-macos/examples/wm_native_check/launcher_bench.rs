//! Actual GPUI launcher comparison, launched as a child of the approved test
//! app. The allowlist restricts window management to this run's fixture app.
use objc2_foundation::{NSDate, NSRunLoop};
use serde_json::{Value, json};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGTERM);
            }
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(3) {
                if self.0.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spin() {
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
}
fn ps(pid: u32, field: &str) -> String {
    let result = Command::new("/bin/ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .output()
        .expect("process statistics");
    assert!(result.status.success(), "process statistics unavailable");
    String::from_utf8_lossy(&result.stdout).trim().into()
}
fn cpu(pid: u32) -> f64 {
    ps(pid, "time=")
        .split(':')
        .rev()
        .enumerate()
        .map(|(index, part)| part.parse::<f64>().expect("CPU time") * 60f64.powi(index as i32))
        .sum()
}
fn copy_index(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("benchmark index directory");
    for entry in std::fs::read_dir(source).expect("source index") {
        let entry = entry.expect("index entry");
        if entry.file_type().expect("index file type").is_file() {
            std::fs::copy(entry.path(), destination.join(entry.file_name())).expect("copy index");
        }
    }
}

pub fn run(base: &Path) {
    // Darwin's sockaddr_un has only 104 bytes. The per-user $TMPDIR prefix
    // plus nested case/config/socket paths can exceed that before launch.
    let benchmark_directory = tempfile::Builder::new()
        .prefix("sift-wm-bench-")
        .tempdir_in("/tmp")
        .expect("short benchmark directory");
    let launcher = std::env::var_os("SIFT_WM_LAUNCHER").expect("launcher path");
    let index = std::path::PathBuf::from(std::env::var_os("SIFT_WM_INDEX").expect("index path"));
    let output = std::env::var_os("SIFT_WM_BENCH_OUTPUT").expect("benchmark output path");
    let mut results = Vec::<Value>::new();
    let automatic = std::env::var("SIFT_WM_BENCH_AUTOMATIC_TILING").as_deref() != Ok("false");
    for pair in 1..=2 {
        for enabled in [false, true] {
            let name = format!("wm-{}-{pair}", if enabled { "enabled" } else { "disabled" });
            println!("START launcher benchmark {name}");
            let directory = benchmark_directory.path().join(&name);
            let originals: Value = serde_json::from_slice(
                &std::fs::read(base.join("actual.json")).expect("current fixture frames"),
            )
            .expect("current frames JSON");
            let config = directory.join("config");
            std::fs::create_dir_all(&config).expect("benchmark configuration directory");
            std::fs::write(config.join("config.toml"), format!(
                "[updates]\ncheck = false\n[clipboard]\nenabled = false\n[window_manager]\nenabled = {enabled}\nautomatic_tiling = {automatic}\ninclude_apps = ['^io\\.github\\.kylianmarceau\\.sift\\.wm-fixture$']\n[window_manager.bindings.main]\n"
            )).expect("benchmark configuration");
            let index_copy = directory.join("index");
            copy_index(&index, &index_copy);
            let log = directory.join("launcher.log");
            let stream = std::fs::File::create(&log).expect("launcher log");
            let child = Command::new(&launcher)
                .env_remove("SIFT_RUN_COMMAND")
                .env_remove("SIFT_ANNOUNCE")
                .env("SIFT_UI_BENCH", "3")
                .env("SIFT_START_HIDDEN", "1")
                .env("SIFT_CONFIG_DIR", &config)
                .env("SIFT_INDEX_DIR", &index_copy)
                .env("SIFT_LOG_DIR", directory.join("logs"))
                .stdin(Stdio::null())
                .stdout(stream.try_clone().expect("log clone"))
                .stderr(stream)
                .spawn()
                .expect("benchmark launcher");
            let mut child = Process(child);
            let start = Instant::now();
            let content = loop {
                let content = std::fs::read_to_string(&log).unwrap_or_default();
                if content.contains("ui bench done") {
                    break content;
                }
                assert!(
                    child.0.try_wait().expect("launcher status").is_none(),
                    "launcher exited: {content}"
                );
                assert!(
                    start.elapsed() < Duration::from_secs(120),
                    "launcher benchmark timed out: {content}"
                );
                spin();
            };
            let windows = if enabled {
                let status = sift_wm::protocol::request(&config, vec!["status".into()])
                    .unwrap_or_else(|e| panic!("enabled service did not start: {e}\n{content}"));
                assert!(
                    status.ok && status.data["enabled"] == true,
                    "window manager is not enabled: {status:?}"
                );
                let count = status.data["windows"]
                    .as_array()
                    .expect("window list")
                    .len();
                assert_eq!(
                    count, 3,
                    "benchmark must actually manage the fixture windows"
                );
                count
            } else {
                assert!(
                    !sift_wm::protocol::socket_path(&config).exists(),
                    "disabled backend unexpectedly started"
                );
                0
            };
            let settle = Instant::now();
            while settle.elapsed() < Duration::from_secs(3) {
                spin();
            }
            let pid = child.0.id();
            let rss_mib = ps(pid, "rss=").parse::<f64>().expect("RSS") / 1024.;
            let footprint = Command::new("/usr/bin/footprint")
                .arg(pid.to_string())
                .output()
                .expect("physical footprint");
            let footprint = String::from_utf8_lossy(&footprint.stdout)
                .lines()
                .filter(|line| line.to_lowercase().contains("footprint"))
                .collect::<Vec<_>>()
                .join("\n");
            let before = cpu(pid);
            let idle = Instant::now();
            while idle.elapsed() < Duration::from_secs(30) {
                spin();
            }
            let idle_cpu = cpu(pid) - before;
            let summary =
                content[content.find("== ui bench").expect("benchmark summary")..].to_owned();
            let result = json!({"name": name, "enabled": enabled, "automatic_tiling": automatic, "managed_windows": windows,
                "rounds":3,"rss_mib":rss_mib,"idle_seconds":30,"idle_cpu_seconds":idle_cpu,
                "footprint":footprint,"summary":summary});
            println!(
                "RESULT {}",
                serde_json::to_string(&result).expect("benchmark result")
            );
            results.push(result);
            // Pause and wait for actual restoration before terminating this
            // disposable launcher. Never leave parked fixture windows behind.
            if enabled {
                let reply =
                    sift_wm::protocol::request(&config, vec!["enable".into(), "off".into()])
                        .expect("pause");
                assert!(reply.ok);
                let wait = Instant::now();
                loop {
                    let actual = std::fs::read(base.join("actual.json"))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
                    let restored =
                        actual
                            .as_ref()
                            .and_then(Value::as_array)
                            .is_some_and(|actual| {
                                originals.as_array().expect("original frames").iter().all(
                                    |original| {
                                        actual.iter().any(|window| {
                                            window["id"] == original["id"]
                                                && serde_json::from_value::<sift_wm::Rect>(
                                                    window["frame"].clone(),
                                                )
                                                .is_ok_and(|frame| {
                                                    serde_json::from_value::<sift_wm::Rect>(
                                                        original["frame"].clone(),
                                                    )
                                                    .is_ok_and(|original| frame.nearly(original))
                                                })
                                        })
                                    },
                                )
                            });
                    if restored {
                        break;
                    }
                    assert!(
                        wait.elapsed() < Duration::from_secs(10),
                        "benchmark pause did not restore fixture windows"
                    );
                    spin();
                }
            }
            drop(child);
            std::fs::write(
                &output,
                serde_json::to_vec_pretty(&results).expect("results JSON"),
            )
            .expect("save results");
        }
    }
}
