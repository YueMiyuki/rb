#!/usr/bin/env python3
"""Risuko: Node frontend and Rust build. Crates come from the local cargo and rb registries.

The Rust build is `--offline --locked`, so a missing crate is an error instead of a download.
The Vite cache is cleared before each `pnpm pack:renderer`.
"""
import json, os, shutil, subprocess, sys, time
from pathlib import Path

ROOT = Path("/Users/yue/projects/ff/risuko")
TAURI = ROOT / "src-tauri"
RB = Path("/Users/yue/projects/rust-build/target/release/rb")


def df_used_kb(vol):
    subprocess.run(["sync"])
    return int(subprocess.check_output(["df", "-k", vol], text=True).splitlines()[1].split()[2])


def run(cmd, cwd, env):
    start = time.perf_counter()
    p = subprocess.run(cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    elapsed = time.perf_counter() - start
    if p.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed:\n{p.stdout[-2500:]}")
    return elapsed


def fetch_deps():
    """Not timed. Retries until the lockfile's crates are in the local cargo registry."""
    env = {k: v for k, v in os.environ.items() if k not in ("CARGO_HOME", "CARGO_TARGET_DIR", "RUSTC_WRAPPER", "RUSTFLAGS")}
    env.update(CARGO_HTTP_MULTIPLEXING="false", CARGO_HTTP_TIMEOUT="180", CARGO_NET_RETRY="10")
    for attempt in range(1, 9):
        print(f"fetch attempt {attempt}", flush=True)
        p = subprocess.run(["cargo", "fetch", "--locked"], cwd=TAURI, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        if p.returncode == 0:
            print("fetch ok", flush=True)
            return
        print(p.stdout[-800:], flush=True)
        time.sleep(min(5 * attempt, 30))
    sys.exit("cargo fetch failed after retries")


def main():
    fetch_deps()
    rows = []
    for tool in ("cargo", "rb"):
        vol = f"/Volumes/rbx-{tool}"
        target = f"{vol}/dl-target"
        store = f"{vol}/dl-store"
        shutil.rmtree(target, ignore_errors=True)
        shutil.rmtree(store, ignore_errors=True)
        base = df_used_kb(vol)
        shutil.rmtree(ROOT / "node_modules/.vite", ignore_errors=True)
        env = {k: v for k, v in os.environ.items() if not k.startswith("RB_") and k not in ("RUSTC_WRAPPER", "RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_HOME")}
        env.update(CI="true", CARGO_TARGET_DIR=target, RB_STORE_DIR=store)
        node = run(["pnpm", "pack:renderer"], ROOT, env)
        rust = run(([str(RB)] if tool == "rb" else ["cargo"]) + ["build", "--locked", "--offline"], TAURI, env)
        row = {"tool": tool, "node_s": round(node, 2), "rust_s": round(rust, 2), "total_s": round(node + rust, 2),
               "disk_mb": (df_used_kb(vol) - base) // 1024}
        rows.append(row)
        print(json.dumps(row), flush=True)
    c, b = rows
    print("\n| | node | rust, offline | total | disk |")
    print("| --- | --- | --- | --- | --- |")
    print(f"| cargo | {c['node_s']:.1f} s | {c['rust_s']:.1f} s | {c['total_s']:.1f} s | {c['disk_mb']} MB |")
    print(f"| rb | {b['node_s']:.1f} s | {b['rust_s']:.1f} s | {b['total_s']:.1f} s | {b['disk_mb']} MB |")


if __name__ == "__main__":
    main()
