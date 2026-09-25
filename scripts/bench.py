#!/usr/bin/env python3
"""Compare build times of cargo and rb on one project.

Each tool works on its own copy of the project (default `target/`), and both copies receive the
same edits. Every edit is unique, so rb can't get store hits from earlier runs. Clean rb builds
start from an empty store.

Usage: scripts/bench.py PROJECT [--release] [--runs N] [--leaf FILE] [--deep FILE]
                        [--scenarios clean,noop,leaf,deep,check,revert,rmtarget] [--rb PATH]
FILE paths are relative to PROJECT. `--leaf` is a file of the final binary crate; `--deep` is a
file of a library that other workspace crates depend on. The report ends with the disk usage of
each tool's `target/` and of rb's store.
"""

import argparse
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run(cmd, cwd, env):
    start = time.perf_counter()
    r = subprocess.run(cmd, cwd=cwd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    elapsed = time.perf_counter() - start
    if r.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed in {cwd}:\n{r.stderr.decode()[-3000:]}")
    return elapsed


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("project")
    ap.add_argument("--release", action="store_true")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--leaf")
    ap.add_argument("--deep")
    ap.add_argument("--scenarios", default="clean,noop,leaf,deep,check,revert,rmtarget")
    ap.add_argument("--rb", default=os.path.join(HERE, "target/release/rb"))
    a = ap.parse_args()
    scenarios = a.scenarios.split(",")
    profile = ["--release"] if a.release else []

    tmp = tempfile.mkdtemp(prefix="rb-bench-")
    ignore = shutil.ignore_patterns("target", ".git")
    dirs = {"cargo": os.path.join(tmp, "cargo"), "rb": os.path.join(tmp, "rb")}
    for d in dirs.values():
        shutil.copytree(a.project, d, ignore=ignore, symlinks=True)
    env = dict(os.environ)
    env.pop("RUSTC_WRAPPER", None)
    store = os.path.join(tmp, "store")
    env["RB_STORE_DIR"] = store
    tools = {"cargo": ["cargo"], "rb": [a.rb]}
    results = {}

    def build(tool, cmd="build"):
        return run(tools[tool] + [cmd, "-q"] + profile, dirs[tool], env)

    def both(name, prepare, cmd="build", runs=a.runs):
        for i in range(runs):
            for tool in (["cargo", "rb"] if i % 2 == 0 else ["rb", "cargo"]):
                prepare(tool, i)
                results.setdefault(name, {}).setdefault(tool, []).append(build(tool, cmd))

    def clean(tool, _):
        shutil.rmtree(os.path.join(dirs[tool], "target"), ignore_errors=True)
        if tool == "rb":
            shutil.rmtree(store, ignore_errors=True)

    originals = {}

    def edit(rel, tag):
        def prepare(tool, i):
            path = os.path.join(dirs[tool], rel)
            originals.setdefault(path, open(path).read())
            with open(path, "a") as f:
                f.write(f"\n#[allow(dead_code)]\nfn __rb_bench_{tag}_{i}() -> u32 {{ {i} }}\n")
        return prepare

    def restore(tool, _):
        for path, text in originals.items():
            if path.startswith(dirs[tool] + os.sep):
                with open(path, "w") as f:
                    f.write(text)

    # The first (clean) build of each copy also warms rb's store for `revert`
    both("clean build", clean) if "clean" in scenarios else [build(t) for t in tools]
    if "noop" in scenarios:
        both("no-op rebuild", lambda t, i: None, runs=max(a.runs, 5))
    if "leaf" in scenarios and a.leaf:
        both(f"edit {a.leaf}", edit(a.leaf, "leaf"))
    if "deep" in scenarios and a.deep:
        both(f"edit {a.deep}", edit(a.deep, "deep"))
    if "check" in scenarios and a.deep:
        both(f"check after editing {a.deep}", edit(a.deep, "check"), cmd="check")
    if "revert" in scenarios and originals:
        both("revert all edits", restore, runs=1)
    if "rmtarget" in scenarios:
        both("rebuild after `rm -rf target`", lambda t, i: shutil.rmtree(os.path.join(dirs[t], "target")))

    print(f"\n{os.path.basename(os.path.abspath(a.project))} ({'release' if a.release else 'dev'}), median of runs")
    print("| scenario | cargo | rb | rb / cargo |")
    print("| --- | --- | --- | --- |")
    for name, r in results.items():
        c, b = statistics.median(r["cargo"]), statistics.median(r["rb"])
        print(f"| {name} | {c:.2f} s | {b:.2f} s | {b / c:.2f} |")
    # Disk: what one project's target/ costs, and what a second worktree adds (rb: links only)
    mib = lambda p: sum(f.stat().st_size for f in __import__("pathlib").Path(p).rglob("*") if f.is_file()) / 2**20
    cargo_target, rb_target, store = (mib(os.path.join(dirs["cargo"], "target")), mib(os.path.join(dirs["rb"], "target")), mib(store))
    print(f"| disk: one `target/` | {cargo_target:.0f} MiB | {rb_target:.0f} MiB (+ {store:.0f} MiB store | |")
    shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
