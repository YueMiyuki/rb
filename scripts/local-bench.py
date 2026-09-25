#!/usr/bin/env python3
"""Build-speed and disk comparison on local Rust fixtures. No Node/Tauri frontend.

Prints the compiler's own Finished time (not process startup) and the real disk growth of
each tool's volume, store included for rb.
"""
import json, os, re, shutil, subprocess, sys, time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RB = ROOT / "target/release/rb"
FIX = ROOT / "tests/compat/fixtures"
PROJECTS = {
    "workspace": (FIX / "workspace", "app/src/main.rs"),
    "tokio-app": (FIX / "tokio-app", "src/main.rs"),
}


def df_used_kb(vol):
    subprocess.run(["sync"])
    line = subprocess.check_output(["df", "-k", vol], text=True).splitlines()[1].split()
    return int(line[2])


def finished_secs(text):
    m = re.search(r"Finished .* in (?:(\d+)m )?([\d.]+)s", text)
    if not m:
        return None
    return int(m.group(1) or 0) * 60 + float(m.group(2))


def build(tool, cwd, vol, label):
    target = f"{vol}/local-{label}"
    env = {k: v for k, v in os.environ.items() if k not in ("RUSTC_WRAPPER", "RUSTFLAGS", "CARGO_TARGET_DIR") and not k.startswith("RB_")}
    env.update(CI="true", CARGO_TARGET_DIR=target, RB_HOME=f"/tmp/rbx/local-home-{tool}", RB_STORE_DIR=f"{vol}/local-store")
    if tool == "rb":
        cmd = [str(RB), "build", "--timings"]
    else:
        cmd = ["cargo", "build"]
    if (Path(cwd) / "Cargo.lock").is_file():
        cmd.append("--locked")
    start = time.perf_counter()
    p = subprocess.run(cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    wall = time.perf_counter() - start
    if p.returncode != 0:
        sys.exit(f"{tool} {label} failed:\n{p.stdout[-2000:]}")
    while tool == "rb" and subprocess.run(["pgrep", "-f", "rb __compact"], capture_output=True).returncode == 0:
        time.sleep(0.2)
    compile = finished_secs(p.stdout)
    m = re.search(r"([\d.]+)s summed unit time", p.stdout)
    if tool == "rb" and m:
        compile = float(m.group(1))
    return {"tool": tool, "step": label, "wall": round(wall, 2), "compile": compile, "disk_mb": None}


def main():
    rows = []
    for name, (src, leaf) in PROJECTS.items():
        for tool in ("cargo", "rb"):
            vol = f"/Volumes/rbx-{tool}"
            proj = Path(f"/tmp/rbx/local-{name}-{tool}")
            shutil.rmtree(proj, ignore_errors=True)
            shutil.copytree(src, proj, symlinks=True)
            shutil.rmtree(f"{vol}/local-{name}", ignore_errors=True)
            if tool == "rb":
                shutil.rmtree(f"{vol}/local-store", ignore_errors=True)
                shutil.rmtree(f"/tmp/rbx/local-home-{tool}", ignore_errors=True)
            if tool == "rb":
                # Download the index and crates once, outside the timed cold build
                build(tool, proj, vol, name)
                shutil.rmtree(f"{vol}/local-{name}", ignore_errors=True)
                shutil.rmtree(f"{vol}/local-store", ignore_errors=True)
            base = df_used_kb(vol)
            text = (proj / leaf).read_text()
            for step, edit in (
                ("cold", None),
                ("no-op", None),
                ("edit 1", "\n#[allow(dead_code)]\nfn bench_edit_1() -> u32 { 1 }\n"),
                ("edit 2", "\n#[allow(dead_code)]\nfn bench_edit_2() -> u32 { 2 }\n"),
                ("revert", "ORIG"),
            ):
                if edit == "ORIG":
                    (proj / leaf).write_text(text)
                elif edit:
                    (proj / leaf).write_text((proj / leaf).read_text() + edit)
                row = build(tool, proj, vol, name)
                row["project"] = name
                row["step"] = step
                row["disk_mb"] = (df_used_kb(vol) - base) // 1024
                rows.append(row)
                print(json.dumps(row), flush=True)
    print("\n| project | step | cargo | rb | cargo disk | rb disk |")
    print("| --- | --- | --- | --- | --- | --- |")
    by = {(r["project"], r["step"], r["tool"]): r for r in rows}
    lost = False
    for name in PROJECTS:
        for step in ("cold", "no-op", "edit 1", "edit 2", "revert"):
            c, b = by[(name, step, "cargo")], by[(name, step, "rb")]
            mark = ""
            if b["wall"] >= c["wall"]:
                mark = "  slower"
                lost = True
            print(f"| {name} | {step} | {c['wall']:.2f} s | {b['wall']:.2f} s | {c['disk_mb']} MB | {b['disk_mb']} MB |{mark}")
    if lost:
        sys.exit(1)


if __name__ == "__main__":
    main()
