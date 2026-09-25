#!/usr/bin/env python3
"""Risuko Rust-only steps, offline: cold, no-op, edit, RUSTFLAGS, revert, then cross builds.

Crate download is not part of the timing. Node/Vite is not run.
"""
import json, os, shutil, subprocess, sys, time
from pathlib import Path

SRC = Path("/Users/yue/projects/ff/risuko/src-tauri")
RB = Path("/Users/yue/projects/rust-build/target/release/rb")
COPY = Path("/tmp/rbx/steps-src")
CROSS = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-gnu",
    "x86_64-pc-windows-msvc",
    "x86_64-apple-darwin",
]


def df_used_kb(vol):
    subprocess.run(["sync"])
    return int(subprocess.check_output(["df", "-k", vol], text=True).splitlines()[1].split()[2])


def env_for(tool):
    vol = f"/Volumes/rbx-{tool}"
    env = {k: v for k, v in os.environ.items() if not k.startswith("RB_") and k not in ("RUSTC_WRAPPER", "RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_HOME")}
    env.update(CI="true", CARGO_TARGET_DIR=f"{vol}/steps-target", RB_STORE_DIR=f"{vol}/steps-store")
    return env


def build(tool, step, rustflags=None, targets=None):
    env = env_for(tool)
    if rustflags:
        env["RUSTFLAGS"] = rustflags
    cmd = [str(RB) if tool == "rb" else "cargo", "build", "--locked", "--offline"]
    for t in targets or []:
        cmd += ["--target", t]
    start = time.perf_counter()
    p = subprocess.run(cmd, cwd=COPY, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    wall = round(time.perf_counter() - start, 2)
    tail = p.stdout[-1500:]
    row = {"tool": tool, "step": step, "ok": p.returncode == 0, "wall": wall}
    if p.returncode != 0:
        row["error"] = tail.splitlines()[-4:]
    print(json.dumps(row), flush=True)
    Path(f"/tmp/rbx/steps-{tool}-{step.replace(' ', '_')}.log").write_text(p.stdout)
    return row


def main():
    if COPY.exists():
        shutil.rmtree(COPY)
    shutil.copytree(SRC, COPY, ignore=shutil.ignore_patterns("target"), symlinks=True)
    state = COPY / "src/state.rs"
    original = state.read_text()
    rows = []
    try:
        for tool in ("cargo", "rb"):
            vol = f"/Volumes/rbx-{tool}"
            shutil.rmtree(f"{vol}/steps-target", ignore_errors=True)
            shutil.rmtree(f"{vol}/steps-store", ignore_errors=True)
            base = df_used_kb(vol)
            for step, flags, edit in (
                ("cold", None, None),
                ("no-op", None, None),
                ("edit", None, "\n#[allow(dead_code)]\nfn bench_step_edit() -> u32 { 1 }\n"),
                ("rustflags", "--cfg rbx_bench_a", None),
                ("revert flags", None, "ORIG"),
            ):
                if edit == "ORIG":
                    state.write_text(original)
                elif edit:
                    state.write_text(state.read_text() + edit)
                rows.append(build(tool, step, flags))
            rows.append({"tool": tool, "step": "disk", "disk_mb": (df_used_kb(vol) - base) // 1024})
            print(json.dumps(rows[-1]), flush=True)
        state.write_text(original)
        # Cross: rb has the linkers. Cargo is tried on the native Intel macOS target only.
        base = df_used_kb("/Volumes/rbx-rb")
        for triple in CROSS:
            rows.append(build("rb", f"cross {triple}", targets=[triple]))
        rows.append(build("cargo", "cross x86_64-apple-darwin", targets=["x86_64-apple-darwin"]))
        rows.append({"tool": "rb", "step": "disk after cross", "disk_mb": (df_used_kb("/Volumes/rbx-rb") - base) // 1024})
        print(json.dumps(rows[-1]), flush=True)
    finally:
        state.write_text(original)

    print("\n| step | cargo | rb |")
    print("| --- | --- | --- |")
    by = {}
    for r in rows:
        by.setdefault(r["step"], {})[r["tool"]] = r
    for step in ("cold", "no-op", "edit", "rustflags", "revert flags", "disk"):
        c, b = by[step].get("cargo", {}), by[step].get("rb", {})
        def cell(r):
            if "disk_mb" in r:
                return f"{r['disk_mb']} MB"
            if not r:
                return ""
            return f"{r['wall']:.1f} s" if r.get("ok") else "failed"
        print(f"| {step} | {cell(c)} | {cell(b)} |")
    print("\n| cross target | result |")
    print("| --- | --- |")
    for r in rows:
        if r["step"].startswith("cross"):
            print(f"| {r['step'].split(' ', 1)[1]} ({r['tool']}) | {r['wall']:.1f} s |" if r.get("ok") else f"| {r['step'].split(' ', 1)[1]} ({r['tool']}) | failed |")


if __name__ == "__main__":
    main()
