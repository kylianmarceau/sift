#!/usr/bin/env python3
"""Alternate a reference launcher and this branch against the same real index.

Runs only disposable launcher processes/configurations, with clipboard history,
updates and window management disabled. Shows the normal UI benchmark on screen.
Reports keystroke/show latency, footprint, RSS, and hidden idle CPU. Build both
binaries first; create a real on-disk benchmark index with `sift bench files`
and `sift index`. This script never terminates an existing user's Sift process.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time


def stop(process):
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def cpu_time(pid):
    value = subprocess.check_output(["ps", "-o", "time=", "-p", str(pid)], text=True).strip()
    fields = value.split(":")
    return sum(float(part) * 60 ** i for i, part in enumerate(reversed(fields)))


def wait_log(process, log, marker, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        content = log.read_text(errors="replace") if log.exists() else ""
        if marker in content:
            return content
        if process.poll() is not None:
            raise RuntimeError(f"launcher exited {process.returncode}: {content[-3000:]}")
        time.sleep(0.2)
    raise RuntimeError(f"timed out waiting for {marker}: {log.read_text(errors='replace')[-3000:]}")


def environment(directory, index):
    config = directory / "config"
    config.mkdir(parents=True)
    (config / "config.toml").write_text(
        "[updates]\ncheck = false\n[clipboard]\nenabled = false\n"
        "[window_manager]\nenabled = false\n"
    )
    env = os.environ.copy()
    for name in ("SIFT_RUN_COMMAND", "SIFT_UI_BENCH", "SIFT_ANNOUNCE", "SIFT_UPDATE_URL"):
        env.pop(name, None)
    env.update(SIFT_START_HIDDEN="1", SIFT_INDEX_DIR=str(index),
               SIFT_CONFIG_DIR=str(config), SIFT_LOG_DIR=str(directory / "logs"))
    return env


def run(args, name, binary, directory):
    shutil.copytree(args.index, directory / "index")
    env = environment(directory, directory / "index")
    env["SIFT_UI_BENCH"] = str(args.rounds)
    log = directory / "launcher.log"
    with log.open("w") as output:
        process = subprocess.Popen([str(binary)], env=env, stdout=output, stderr=output)
        try:
            content = wait_log(process, log, "ui bench done", 120)
            time.sleep(3)
            rss = int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(process.pid)], text=True)) / 1024
            footprint = subprocess.run(["footprint", str(process.pid)], capture_output=True, text=True, timeout=20)
            before = cpu_time(process.pid)
            time.sleep(args.idle_seconds)
            idle = cpu_time(process.pid) - before
            summary = content[content.find("== ui bench"):]
            result = {"name": name, "binary": str(binary), "rounds": args.rounds,
                      "rss_mib": round(rss, 2), "idle_seconds": args.idle_seconds,
                      "idle_cpu_seconds": round(idle, 3), "summary": summary,
                      "footprint": "\n".join(line for line in footprint.stdout.splitlines()
                                             if re.search(r"[Ff]ootprint|phys_footprint", line))}
            print(json.dumps(result, indent=2), flush=True)
            return result
        finally:
            stop(process)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--launcher", type=Path, required=True)
    parser.add_argument("--index", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--pairs", type=int, default=2)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--idle-seconds", type=int, default=30)
    args = parser.parse_args()
    for name in ("baseline", "launcher", "index"):
        setattr(args, name, getattr(args, name).resolve(strict=True))
    if not 1 <= args.pairs <= 10 or not 1 <= args.rounds <= 20 or not 1 <= args.idle_seconds <= 60:
        parser.error("pairs 1..10, rounds 1..20, idle-seconds 1..60")
    with tempfile.TemporaryDirectory(prefix="sift-wm-ui-") as temporary:
        work = Path(temporary)
        warm = work / "warmup"
        env = environment(warm, args.index)
        log = warm / "warmup.log"
        with log.open("w") as output:
            process = subprocess.Popen([str(args.baseline)], env=env, stdout=output, stderr=output)
            try:
                wait_log(process, log, "watching for changes", 60)
                time.sleep(1)
            finally:
                stop(process)
        results = []
        for pair in range(args.pairs):
            for name, binary in (("baseline", args.baseline), ("native-wm-disabled", args.launcher)):
                results.append(run(args, f"{name}-{pair + 1}", binary, work / f"{name}-{pair}"))
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
