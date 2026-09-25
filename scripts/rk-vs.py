"""risuko dev builds, cargo vs rb: build time and real disk footprint over an edit/variant churn.

Each tool works on its own copy of the sources, with its target dir (and rb's store) on its own
APFS disk image, so `df` of that image is the tool's exact footprint (rb's store and target
share blocks through reflinks, which `du` would count twice). Every step runs for both tools
back to back, alternating which goes first, so machine drift hits both alike.
"""
import json, os, re, shutil, subprocess, sys, time
from pathlib import Path

RB = "/Users/yue/projects/rust-build/target/release/rb"
TAG = int(time.time()) % 100000
MINOR = "src/state.rs"
MAJOR = ["risuko-http/src/lib.rs", "risuko-bt/src/lib.rs", "risuko-engine/src/lib.rs", "src/lib.rs"]
TOOLS = ["cargo", "rb"]
WALLS = {}


def major_text(rel, i):
    return {
        "risuko-http/src/lib.rs": f"\npub fn bench_major_http_{i}() -> u32 {{\n    {TAG + i}\n}}\n",
        "risuko-bt/src/lib.rs": f"\npub fn bench_major_bt_{i}() -> u32 {{\n    risuko_http::bench_major_http_{i}() + 1\n}}\n",
        "risuko-engine/src/lib.rs": f"\npub fn bench_major_engine_{i}() -> u32 {{\n    risuko_bt::bench_major_bt_{i}() + 1\n}}\n",
        "src/lib.rs": f"\n#[allow(dead_code)]\nfn bench_major_app_{i}() -> u32 {{\n    risuko_engine::bench_major_engine_{i}() + 1\n}}\n",
    }[rel]


def used_kb(vol):
    subprocess.run(["sync"])
    return int(subprocess.run(["df", "-k", vol], capture_output=True, text=True).stdout.splitlines()[1].split()[2])


class Tool:
    def __init__(self, name):
        self.name = name
        self.vol = f"/Volumes/rbx-{name}"
        self.tauri = Path(f"/tmp/rbx/rkvs-src-{name}/src-tauri")
        self.target = f"{self.vol}/target"
        self.home = f"/tmp/rbx/rkvs-home-{name}"
        for d in (self.target, f"{self.vol}/store", self.home):
            shutil.rmtree(d, ignore_errors=True)
        os.makedirs(self.home)
        self.base = used_kb(self.vol)
        self.originals = {rel: (self.tauri / rel).read_text() for rel in [MINOR, *MAJOR]}

    def append(self, rel, text):
        with open(self.tauri / rel, "a") as f:
            f.write(text)

    def revert(self):
        for rel, text in self.originals.items():
            if (self.tauri / rel).read_text() != text:
                (self.tauri / rel).write_text(text)

    def build(self, label, rustflags):
        env = {k: v for k, v in os.environ.items() if not k.startswith("RB_") and k not in ("RUSTC_WRAPPER", "RUSTFLAGS", "CARGO_TARGET_DIR")}
        env.update(CI="true", CARGO_TARGET_DIR=self.target, RB_HOME=self.home, RB_STORE_DIR=f"{self.vol}/store")
        if rustflags:
            env["RUSTFLAGS"] = rustflags
        cmd = [RB, "build", "--locked", "--timings"] if self.name == "rb" else ["cargo", "build", "--locked"]
        start = time.perf_counter()
        p = subprocess.run(cmd, cwd=self.tauri, env=env, capture_output=True, text=True)
        wall = time.perf_counter() - start
        out = p.stdout + p.stderr
        Path(f"/tmp/rbx/rkvs-{self.name}-{label.replace(' ', '_')}.log").write_text(out)
        if p.returncode != 0:
            raise SystemExit(f"{self.name} {label} failed:\n{out[-3000:]}")
        # rb's store upkeep runs detached at background priority; measure disk once it's done
        # (not counted in the build time)
        while subprocess.run(["pgrep", "-f", "rb __compact"], capture_output=True).returncode == 0:
            time.sleep(0.5)
        row = {"tool": self.name, "step": label, "wall": round(wall, 2),
               "compiled": len(re.findall(r"^\s+Compiling ", out, re.M)),
               "disk_mb": (used_kb(self.vol) - self.base) // 1024}
        WALLS.setdefault(label, {})[self.name] = row["wall"]
        print(json.dumps(row), flush=True)


def minor(i):
    return lambda t: t.append(MINOR, f"\n#[allow(dead_code)]\nfn bench_minor_{i}() -> u32 {{\n    {TAG + i}\n}}\n")


def major(i):
    def apply(t):
        for rel in MAJOR:
            t.append(rel, major_text(rel, i))
    return apply


def nothing(t):
    pass


STEPS = [("cold", nothing, None), ("no-op", nothing, None)]
STEPS += [(f"minor edit {i + 1}", minor(i), None) for i in range(3)]
STEPS += [(f"major edit {i + 1}", major(i), None) for i in range(2)]
STEPS += [("revert edits", Tool.revert, None)]
for v in ("a", "b"):
    STEPS += [(f"RUSTFLAGS variant {v}", nothing, f"--cfg rbx_{v}"),
              (f"edit under variant {v}", minor(10 + ord(v)), f"--cfg rbx_{v}"),
              (f"revert under variant {v}", Tool.revert, f"--cfg rbx_{v}")]
STEPS += [("back to default", nothing, None), ("minor edit after", minor(99), None)]
if os.environ.get("CHURN"):
    # Many metadata changes in a row (features, flags, toolchains, build vs test in real life)
    STEPS = [("cold", nothing, None)]
    for v in range(1, int(os.environ["CHURN"]) + 1):
        STEPS += [(f"variant {v}", nothing, f"--cfg rbx_churn{v}"), (f"edit under variant {v}", minor(200 + v), f"--cfg rbx_churn{v}")]

tools = [Tool(n) for n in TOOLS]
seen = {}
try:
    for i, (label, action, flags) in enumerate(STEPS):
        for t in (tools if i % 2 == 0 else tools[::-1]):
            action(t)
            t.build(label, flags)
            seen.setdefault(label, set()).add(t.name)
            if seen[label] == {"cargo", "rb"} and WALLS[label]["rb"] > WALLS[label]["cargo"]:
                raise SystemExit(f"slower: {label} cargo {WALLS[label]['cargo']}s rb {WALLS[label]['rb']}s")
    while subprocess.run(["pgrep", "-f", "rb __compact"], capture_output=True).returncode == 0:
        time.sleep(1)
    for t in tools:
        print(json.dumps({"tool": t.name, "step": "settled", "disk_mb": (used_kb(t.vol) - t.base) // 1024}), flush=True)
finally:
    for t in tools:
        t.revert()
