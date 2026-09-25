# rb

rb is a Rust build tool that doesn't use cargo. It reads the same `Cargo.toml`, `Cargo.lock` and
`.cargo/config.toml`, and `rb build`, `test`, `bench`, `doc` and `run` give you what the cargo
commands would. Everything underneath is its own code: dependency resolution, the lockfile,
crates.io downloads, feature resolution, scheduling, caching. You need a Rust toolchain (`rustc`
and `rustdoc`) and nothing else.

I wrote it for three reasons: `target/` directories were eating my disk, builds felt slower than
they had to be, and cross-compiling from a Mac was a chore. Here is what came out of that.

## The store

Every compiled unit (rlibs, binaries, proc-macros, build-script runs) is stored once,
content-addressed, in `~/.rb/store`. A project's `target/` holds links into it: reflinks where the
filesystem can do copy-on-write clones, hardlinks otherwise, symlinks if you ask with
`--link-mode symlink`. This is the pnpm model applied to Rust. Build a dependency once and every
project and worktree on the machine gets it for free. `rm -rf target && rb build` takes
milliseconds. Downloaded crate sources are shared the same way, in `~/.rb/registry`.

## Speed

On ripgrep and the small fixtures, clean builds are faster than cargo, edits usually are too, and
no-op rebuilds are cache hits. A full rebuild from a new `RUSTFLAGS` on a large app still ties
cargo or loses by a few seconds. A build whose time is a single fat-LTO link is the other case
rb can't pull ahead: rustc runs that on one thread. The numbers are in [Benchmarks](#benchmarks).

## Same results as cargo

I didn't want a faster tool that builds something slightly different, so a differential harness
runs `build`, `check`, `test`, `bench` and `doc` through both tools and diffs every rustc and
rustdoc invocation plus the unit graph. They match exactly on a corpus that includes ripgrep and
this repository, cargo's per-crate LTO modes included. ripgrep built by rb is the same size as
cargo's, its code section differs by 0.005%, its searches run at the same speed, and its test
suite passes identically under `rb test`. A `Cargo.lock` that rb resolves from scratch is
byte-identical to the one cargo writes.

rb is young. The harness makes me confident about what gets passed to the compiler; the resolver
and the store have been exercised on that corpus and on a Tauri desktop app, not on the whole
ecosystem. When rb hits something it can't handle it stops with an error; the list is under
[Not supported (yet)](#not-supported-yet).

## Install

```sh
cargo install --git https://github.com/YueMiyuki/rust-build.git rb
```

From a checkout: `cargo install --path crates/rb`, or, once you have rb, `rb build --release -p rb`.

## Usage

```sh
rb build                       # like cargo build (same flags, same target/<profile>/ outputs)
rb build --release --examples  # --lib --bins --examples --tests --benches --all-targets
rb check --all-targets
rb run -p app -- --flag        # rb run --example demo
rb test                        # unit, integration and doc tests; --no-run, --no-fail-fast, --doc
rb test -p core --lib filter -- --exact
rb bench
rb doc --open                  # --no-deps
rb build --target x86_64-unknown-linux-musl --target aarch64-unknown-linux-gnu.2.17 \
         --target x86_64-pc-windows-msvc --target aarch64-apple-darwin
rb build --timings             # where the time went, pipelining stats
rb clean                       # remove target/ (the store is untouched)
```

Without a `Cargo.lock`, rb resolves the graph itself: the crates.io sparse index, other sparse
registries, git and path dependencies, `[patch]`, features, and `rust-version` (MSRV-aware with
resolver 3). It writes the lockfile in cargo's format. With a lockfile present, rb uses it as it
is and only re-resolves when the manifests no longer match it; `--locked` and `--frozen` make that
an error instead. `.crate` files already sitting in `~/.cargo/registry/cache` are checksummed and
reused rather than downloaded again.

## Cross compilation

Toolchains are set up on first use. `rb target add` does it ahead of time:

```sh
rb target add x86_64-unknown-linux-gnu aarch64-unknown-linux-musl x86_64-pc-windows-gnu
rb target add x86_64-pc-windows-msvc aarch64-pc-windows-msvc --accept-license   # Microsoft CRT/SDK license
rb target list
rb doctor --smoke              # compile and link hello-world for every ready target
```

rb itself is pure Rust, but linking for another OS needs that OS's C runtime, and plenty of crates
(`cc`, `ring`, `zstd`, `jemalloc`, …) compile C code that needs a C cross compiler. macOS ships
neither for Linux nor for Windows, so rb downloads them:

- zig, a single 49 MiB download, covers Linux gnu and musl and Windows GNU. It brings clang, lld,
  glibc stubs for whichever version you pick, musl, and the mingw headers and import libraries.
- xwin's Microsoft CRT/SDK splat covers MSVC. Linking goes through rustup's `rust-lld`; clang in
  `cl` mode compiles the C.

Pure-Rust crates targeting musl could link with `rust-lld` alone. glibc targets need versioned libc
stubs, windows-gnu needs import libraries, and anything with C code needs a compiler, so those
can't.

## Store maintenance

```sh
rb store status
rb store gc --max-size 20G --max-age-days 30 [--dry-run]
rb store verify [--repair]
```

## Configuration

`.cargo/config.toml` files are read with cargo's rules. Supported keys:

- `build`: `target`, `target-dir`, `rustflags`, `rustc`, `rustc-wrapper`, `jobs`, `incremental`;
- `target.<triple>` and `target.'cfg(…)'`: `linker`, `runner`, `rustflags`;
- `[env]`, `[registries]`, `net.offline` and `[profile]`;
- the matching `CARGO_*`, `RUSTC`, `RUSTC_WRAPPER`, `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS`
  environment variables.

rb's own settings live in `~/.rb/config.toml`. Every key is optional:

```toml
[store]
dir = "/Volumes/fast/rb-store"   # default ~/.rb/store
link-mode = "auto"               # auto | reflink | hardlink | symlink | copy
max-size = "50G"                 # default 20G
max-age-days = 30

[target]
keep-variants = 1                # 0 deletes the previous build (less disk; a revert recompiles)
keep-unused-days = 7             # and for at most this long

[speed]
parallel-frontend = "auto"       # nightly -Zthreads for long units when cores are idle: auto | on | off
codegen-backend = "cranelift"    # nightly dev builds, if the component is installed
remap-deps-paths = false         # remap registry paths in debuginfo

[cross]
zig-version = "0.16.0"
accept-msvc-license = false

[build-scripts]
cache = true

[registry]
reuse-cargo-downloads = true     # verify and reuse .crate files from ~/.cargo/registry/cache
```

Environment variables: `RB_HOME`, `RB_STORE_DIR`, `RB_LINK_MODE`, `RB_NO_STORE=1`,
`RB_STORE_MAX_SIZE`, `RB_COMPACT=sync|off` (store upkeep after builds runs in the background by
default), `RB_ACCEPT_MSVC_LICENSE=1`, `RB_CODEGEN_BACKEND`, `RB_ZIG`, `RB_REUSE_CARGO_DOWNLOADS`.

If a build script depends on the machine it runs on, keep it out of the shared store:

```toml
[workspace.metadata.rb]
no-cache-build-scripts = ["openssl-sys"]
```

## Not supported (yet)

These are reported as errors:

- unstable `cargo-features`;
- artifact dependencies;
- `-Zbuild-std` except `-Zbuild-std=core` (needs nightly `rust-src`).

Git indexes (`[registries] index` without a `sparse+` URL) are checked out and read like cargo. crates.io stays on the sparse index.

`rb build -Zjson-target-spec --target foo.json` matches cargo: rustc receives the absolute spec and `-Zunstable-options`, and artifacts are written to `target/<file-stem>/`. A `[source] replace-with` that points at a directory (vendoring) and `[replace]` are applied. A git dependency whose commit contains `.gitmodules` is checked out with its submodules.

## Benchmarks

Measured on an Apple M4 Pro (14 cores), APFS, 25 Sep 2026. Each tool gets its own copy and its own
volume (`/Volumes/rbx-cargo`, `/Volumes/rbx-rb`). Time is the compiler's `Finished` line. Disk is
`df` allocated blocks from a fresh target (and, for rb, an empty store) after compaction.
`keep-variants = 0`, so rb drops the previous build instead of keeping it for a fast revert.
Builds are online and `--locked` when a lockfile exists. Network is allowed; `Finished` does not
include that download.

Local fixtures and takumi, one run each. Hello's `df` delta is under 1 MB for both tools (`du`
sees about 1.2 MB of `target/`, plus 0.5 MB of rb store).

| project | step | cargo | rb | cargo disk | rb disk |
| --- | --- | --- | --- | --- | --- |
| hello | cold | 0.51 s | 0.18 s | <1 MB | <1 MB |
| hello | no-op | 0.15 s | 0.00 s | <1 MB | <1 MB |
| hello | edit, then another | 0.28 s, 0.30 s | 0.16 s, 0.17 s | <1 MB | <1 MB |
| hello | revert | 0.29 s | 0.17 s | <1 MB | <1 MB |
| testing | cold | 1.20 s | 1.04 s | 3 MB | 3 MB |
| testing | no-op | 0.07 s | 0.00 s | 3 MB | 3 MB |
| testing | edit, then another | 0.30 s, 0.29 s | 0.23 s, 0.22 s | 5 MB | 4 MB |
| testing | revert | 0.28 s | 0.21 s | 5 MB | 4 MB |
| workspace | cold | 4.62 s | 3.20 s | 53 MB | 52 MB |
| workspace | no-op | 0.13 s | 0.01 s | 53 MB | 52 MB |
| workspace | edit, then another | 0.28 s, 0.31 s | 0.23 s, 0.17 s | 53 MB | 53 MB |
| workspace | revert | 0.28 s | 0.16 s | 53 MB | 53 MB |
| sys | cold | 2.28 s | 1.83 s | 7 MB | 7 MB |
| sys | no-op | 0.12 s | 0.00 s | 7 MB | 7 MB |
| sys | edit, then another | 0.36 s, 0.48 s | 0.17 s, 0.17 s | 8 MB | 8 MB |
| sys | revert | 0.38 s | 0.18 s | 8 MB | 8 MB |
| tokio-app | cold | 3.69 s | 3.12 s | 73 MB | 73 MB |
| tokio-app | no-op | 0.14 s | 0.00 s | 73 MB | 73 MB |
| tokio-app | edit, then another | 0.35 s, 0.36 s | 0.30 s, 0.22 s | 76 MB | 76 MB |
| tokio-app | revert | 0.35 s | 0.21 s | 76 MB | 76 MB |
| takumi | cold | 43.93 s | 48.67 s | 1927 MB | 1925 MB |
| takumi | no-op | 0.24 s | 0.04 s | 1927 MB | 1925 MB |
| takumi | edit, then another | 0.37 s, 0.42 s | 0.46 s, 0.19 s | 1931 MB | 1929 MB |
| takumi | revert | 0.40 s | 0.18 s | 1931 MB | 1929 MB |

On these trees rb is faster on every no-op and on every revert, and the disk is the same or a few
MB lower. The cold takumi compile is the exception (48.67 s against 43.93 s). The first takumi
edit is also slower (0.46 s against 0.37 s); the second edit is faster (0.19 s against 0.42 s).

Risuko's Rust crate only (`src-tauri`, frontend not built), same rules.

| scenario | cargo | rb | cargo disk | rb disk |
| --- | --- | --- | --- | --- |
| cold build | 61.00 s | 57.03 s | 7641 MB | 7591 MB |
| no-op rebuild | 5.71 s | 0.11 s | 7768 MB | 7591 MB |
| one-line edit | 5.82 s | 6.27 s | 7782 MB | 7739 MB |
| new `RUSTFLAGS` | 50.17 s | 51.84 s | 13108 MB | 7601 MB |
| revert the edit and flags | 11.24 s | 51.26 s | 13118 MB | 7602 MB |

With `keep-variants = 0`, a new `RUSTFLAGS` does not leave the old incremental sessions on disk:
cargo goes from 7.6 GB to 13.1 GB, rb stays at 7.6 GB. The revert then recompiles, so it costs
51 s instead of a store hit. Cargo's no-op is 5.71 s; rb's is 0.11 s. The one-line edit still
relinks the large staticlib, and cargo is a bit ahead there (5.82 s, rb 6.27 s).

Cross builds of that crate, online and `--locked`, each tool starting from an empty target.
Darwin disk is that target alone. The Windows rows share one rb target directory, so their disk
figures are the running total. Linux was rebuilt once `~/.rb` (where the Debian and Alpine GTK
sysroots live) was the rb home; those two times are not from an empty target.

| target | cargo | rb |
| --- | --- | --- |
| `x86_64-apple-darwin` | 64.00 s, 8286 MB | 57.86 s, 8273 MB |
| `x86_64-unknown-linux-gnu` | no `x86_64-linux-gnu-gcc` | 27.47 s |
| `aarch64-unknown-linux-musl` | | 29.80 s |
| `x86_64-pc-windows-gnu` | no `x86_64-w64-mingw32-dlltool` | 65.83 s, 12511 MB total |
| `x86_64-pc-windows-msvc` | host `cc` has no MSVC headers | 82.02 s, 21300 MB total |

An earlier `scripts/bench.py` run (medians, not repeated above) on this machine:

| scenario | ripgrep dev | ripgrep release | rust-build dev | rust-build release |
| --- | --- | --- | --- | --- |
| clean build | 4.42 s → 3.43 s | 7.35 s → 6.35 s | 25.85 s → 26.37 s | 25.34 s → 23.38 s |
| no-op rebuild | 0.18 s → 0.02 s | 0.18 s → 0.02 s | 0.32 s → 0.02 s | 0.27 s → 0.02 s |
| edit the binary crate | 0.84 s → 0.58 s | 2.94 s → 2.48 s | 0.67 s → 0.39 s | 9.11 s → 9.26 s |
| edit a deep library | 1.04 s → 0.89 s | 3.44 s → 3.02 s | 1.32 s → 1.07 s | 10.92 s → 10.66 s |
| `check` after editing it | 0.60 s → 0.35 s | | 0.67 s → 0.36 s | |
| revert the edits | 1.09 s → 0.02 s | 5.70 s → 0.02 s | 1.33 s → 0.02 s | 10.37 s → 0.02 s |
| rebuild after `rm -rf target` | 4.59 s → 0.03 s | 7.55 s → 0.03 s | 27.35 s → 0.03 s | 21.83 s → 0.03 s |

Numbers there are cargo → rb. ripgrep 15.2.0 has no LTO; this repository's release profile uses
thin LTO. One ripgrep `target/` was 313 MiB with cargo and 31 MiB with rb; two worktrees were
626 MiB against 197 MiB, store included.

## Development

```sh
cargo test --workspace
cargo build --release
cargo run -p rb-compat -- --command test --unit-graph   # differential check vs cargo (build|check|test|bench|doc)
scripts/bench.py path/to/project [--release] --leaf src/main.rs --deep crates/lib/src/lib.rs   # build times and disk vs cargo
```
