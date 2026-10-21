#!/usr/bin/env python3
"""Writes the licence notices of every crate compiled into the launcher.

    scripts/third-party-licenses.py OUT.txt

The MIT and Apache licences of Sift's dependencies require their notices
to travel with the binary, so the app bundle includes this file. Only
normal dependencies of sift-launcher for Apple Silicon are listed: build
scripts, procedural macros and dev-dependencies do not end up in the app.
"""

import json
import os
import subprocess
import sys

LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "unlicense")


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    out_path = sys.argv[1]
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked",
             "--filter-platform", "aarch64-apple-darwin",
             "--manifest-path", os.path.join(root, "Cargo.toml")],
            check=True, capture_output=True, text=True,
        ).stdout
    )
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    workspace = set(meta["workspace_members"])

    def is_proc_macro(pkg):
        return any("proc-macro" in t["kind"] for t in pkg["targets"])

    start = next(i for i in workspace if packages[i]["name"] == "sift-launcher")
    seen, stack = set(), [start]
    while stack:
        node = nodes[stack.pop()]
        for dep in node["deps"]:
            normal = any(k["kind"] is None for k in dep["dep_kinds"])
            pkg = packages[dep["pkg"]]
            if normal and not is_proc_macro(pkg) and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                stack.append(dep["pkg"])

    third_party = sorted(
        (packages[i] for i in seen if i not in workspace),
        key=lambda p: (p["name"], p["version"]),
    )
    # Licence texts are mostly identical (MIT, Apache-2.0); print each once.
    texts = {}  # normalised text -> label
    by_spdx = {}  # licence file kind -> a label holding that standard text
    entries, missing = [], []
    for pkg in third_party:
        directory = os.path.dirname(pkg["manifest_path"])
        files = sorted(
            f for f in os.listdir(directory)
            if f.lower().startswith(LICENSE_PREFIXES)
            and os.path.isfile(os.path.join(directory, f))
        )
        refs = []
        for name in files:
            with open(os.path.join(directory, name), encoding="utf-8", errors="replace") as f:
                text = f.read().strip()
            label = texts.setdefault(text, f"Text {len(texts) + 1}")
            refs.append(f"{name}: {label}")
            lower = name.lower()
            for kind in ("apache", "mit", "zlib", "bsd"):
                if kind in lower:
                    by_spdx.setdefault(kind, label)
        if not files:
            missing.append(pkg)
        entries.append((pkg, refs))

    def standard(spdx):
        hits = [label for kind, label in by_spdx.items() if kind in spdx.lower()]
        return ", ".join(hits) if hits else "the licence named above"

    with open(out_path, "w", encoding="utf-8") as out:
        out.write("Sift includes the following third-party software.\n")
        out.write(f"{len(third_party)} crates, each with its licence. Licence texts\n")
        out.write("shared by several crates are printed once at the end.\n\n")
        for pkg, refs in entries:
            out.write(f"{pkg['name']} {pkg['version']}\n")
            out.write(f"  Licence: {pkg.get('license') or 'see licence file'}\n")
            if pkg.get("repository"):
                out.write(f"  Source: {pkg['repository']}\n")
            for ref in refs:
                out.write(f"  {ref}\n")
            if not refs:
                authors = ", ".join(pkg.get("authors") or []) or "the authors"
                out.write(f"  Copyright: {authors}\n")
                out.write(f"  No licence file in the published crate; the standard text\n")
                out.write(f"  is {standard(pkg.get('license') or '')}.\n")
            out.write("\n")
        for text, label in texts.items():
            out.write("=" * 78 + f"\n{label}\n" + "=" * 78 + f"\n{text}\n\n")
    print(f"wrote {out_path}: {len(third_party)} crates, {len(texts)} distinct licence texts, "
          f"{len(missing)} crates without a licence file")
    return 0


if __name__ == "__main__":
    sys.exit(main())
