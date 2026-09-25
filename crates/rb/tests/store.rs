//! Store behavior, through the real `rb` binary, on small offline projects.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }

    fn rb(&self, project: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rb"))
            .args(args)
            .current_dir(project)
            .env("RB_HOME", self.root.join("home"))
            .env("RB_STORE_DIR", self.root.join("store"))
            .env("RB_COMPACT", "sync")
            .env_remove("RUSTC_WRAPPER")
            .envs(envs.iter().copied())
            .output()
            .unwrap()
    }

    fn build(&self, project: &Path, envs: &[(&str, &str)]) -> String {
        let out = self.rb(project, &["build", "--color", "never"], envs);
        let text = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "rb build failed in {}:\n{text}", project.display());
        text
    }
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn package(dir: &Path, name: &str, main: &str) {
    write(
        &dir.join("Cargo.toml"),
        &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
    );
    write(&dir.join("src/main.rs"), main);
}

fn run(bin: &Path) -> String {
    let out = Command::new(bin).output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn stats(log: &str) -> (usize, usize, usize) {
    let line = log
        .lines()
        .find(|l| l.contains("Finished"))
        .unwrap_or_else(|| panic!("no Finished line:\n{log}"));
    let nums: Vec<usize> = line
        .rsplit('(')
        .next()
        .unwrap()
        .split(',')
        .map(|p| p.trim().split(' ').next().unwrap().parse().unwrap())
        .collect();
    (nums[0], nums[1], nums[2])
}

#[test]
fn fresh_cached_and_cross_project() {
    let env = Env::new();
    let a = env.root.join("a");
    package(&a, "demo", "fn main() { println!(\"v1\"); }\n");
    assert_eq!(stats(&env.build(&a, &[])), (1, 0, 0));
    assert_eq!(stats(&env.build(&a, &[])), (0, 0, 1), "second build must be fresh");
    std::fs::remove_dir_all(a.join("target")).unwrap();
    assert_eq!(
        stats(&env.build(&a, &[])),
        (0, 1, 0),
        "rebuild after rm -rf target comes from the store"
    );
    assert_eq!(run(&a.join("target/debug/demo")), "v1");

    let b = env.root.join("elsewhere/b");
    package(&b, "demo", "fn main() { println!(\"v1\"); }\n");
    assert_eq!(
        stats(&env.build(&b, &[])),
        (0, 1, 0),
        "identical sources in another directory share artifacts"
    );
    write(&b.join("src/main.rs"), "fn main() { println!(\"v2\"); }\n");
    assert_eq!(stats(&env.build(&b, &[])), (1, 0, 0));
    assert_eq!(run(&b.join("target/debug/demo")), "v2");
    assert_eq!(run(&a.join("target/debug/demo")), "v1", "other projects are unaffected");
}

/// Local units are keyed on the files they read (cargo's dep-info fingerprint), not on the whole
/// package: editing one binary rebuilds that binary only, and a build script with
/// `rerun-if-changed` reruns only when those files change
#[test]
fn edits_rebuild_only_units_that_read_them() {
    let env = Env::new();
    let p = env.root.join("p");
    write(
        &p.join("Cargo.toml"),
        "[package]\nname = \"multi\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n[workspace]\n",
    );
    write(
        &p.join("build.rs"),
        "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); println!(\"cargo:rustc-env=BS=v1\"); }\n",
    );
    write(&p.join("src/lib.rs"), "pub fn v() -> u32 { 1 }\n");
    write(
        &p.join("src/bin/a.rs"),
        "fn main() { println!(\"a {} {}\", multi::v(), env!(\"BS\")); }\n",
    );
    write(&p.join("src/bin/b.rs"), "fn main() { println!(\"b {}\", multi::v()); }\n");
    let compiled = |log: &str| stats(log).0;
    assert_eq!(compiled(&env.build(&p, &[])), 5, "build script, its run, lib, a, b");
    assert_eq!(
        compiled(&env.build(&p, &[])),
        0,
        "the first build's artifacts are re-keyed to file-level keys"
    );

    write(
        &p.join("src/bin/a.rs"),
        "fn main() { println!(\"a2 {} {}\", multi::v(), env!(\"BS\")); }\n",
    );
    assert_eq!(compiled(&env.build(&p, &[])), 1, "only `a` reads src/bin/a.rs");
    assert_eq!(run(&p.join("target/debug/a")), "a2 1 v1");

    write(&p.join("README.md"), "# unrelated\n");
    write(&p.join("notes.txt"), "unrelated\n");
    assert_eq!(compiled(&env.build(&p, &[])), 0, "files nothing reads don't matter");

    write(&p.join("src/lib.rs"), "pub fn v() -> u32 { 2 }\n");
    assert_eq!(compiled(&env.build(&p, &[])), 3, "lib and both binaries");

    write(
        &p.join("build.rs"),
        "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); println!(\"cargo:rustc-env=BS=v2\"); }\n",
    );
    assert_eq!(
        compiled(&env.build(&p, &[])),
        5,
        "the script reruns and everything downstream follows"
    );
    assert_eq!(run(&p.join("target/debug/a")), "a2 2 v2");
}

/// Generated files referenced by their symlink-resolved path (as tauri's codegen does) are
/// recognized as build outputs, so a fresh target dir still gets store hits
#[cfg(unix)]
#[test]
fn symlinked_target_dir_keeps_store_hits() {
    let env = Env::new();
    let p = env.root.join("p");
    write(
        &p.join("Cargo.toml"),
        "[package]\nname = \"gen\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    );
    write(
        &p.join("build.rs"),
        r#"fn main() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("msg.txt"), "generated").unwrap();
    let real = std::fs::canonicalize(out.join("msg.txt")).unwrap();
    println!("cargo:rustc-env=GEN_FILE={}", real.display());
}
"#,
    );
    write(
        &p.join("src/main.rs"),
        "fn main() { println!(\"{}\", include_str!(env!(\"GEN_FILE\"))); }\n",
    );
    let real = env.root.join("real-target");
    std::fs::create_dir_all(&real).unwrap();
    let link = env.root.join("target-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let build = || {
        let out = env.rb(&p, &["build", "--color", "never", "--target-dir", link.to_str().unwrap()], &[]);
        let log = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{log}");
        stats(&log)
    };
    assert_eq!(build(), (3, 0, 0));
    std::fs::remove_dir_all(&real).unwrap();
    std::fs::create_dir_all(&real).unwrap();
    assert_eq!(build(), (0, 1, 0), "the fresh target dir is served from the store");
    assert_eq!(run(&link.join("debug/gen")), "generated");
}

#[test]
fn env_dependencies_are_discovered_inputs() {
    let env = Env::new();
    let p = env.root.join("p");
    package(
        &p,
        "envy",
        "fn main() { println!(\"{}\", option_env!(\"RB_TEST_FLAVOR\").unwrap_or(\"none\")); }\n",
    );
    let bin = p.join("target/debug/envy");
    env.build(&p, &[("RB_TEST_FLAVOR", "vanilla")]);
    assert_eq!(run(&bin), "vanilla");
    assert_eq!(
        stats(&env.build(&p, &[("RB_TEST_FLAVOR", "mint")])).0,
        1,
        "changed env! value must rebuild"
    );
    assert_eq!(run(&bin), "mint");
    assert_eq!(
        stats(&env.build(&p, &[("RB_TEST_FLAVOR", "vanilla")])),
        (0, 1, 0),
        "earlier variant is restored from the store"
    );
    assert_eq!(run(&bin), "vanilla");
}

#[test]
fn files_outside_the_package_are_tracked() {
    let env = Env::new();
    let ws = env.root.join("ws");
    write(&ws.join("shared.txt"), "first");
    write(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n");
    write(
        &ws.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &ws.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", include_str!(\"../../shared.txt\")); }\n",
    );
    env.build(&ws, &[]);
    assert_eq!(run(&ws.join("target/debug/app")), "first");
    write(&ws.join("shared.txt"), "second");
    assert_eq!(stats(&env.build(&ws, &[])).0, 1);
    assert_eq!(run(&ws.join("target/debug/app")), "second");
}

/// Workspace with `gen` (lib + build script) and `app` (bin printing `gen::DATA`)
fn build_script_project(dir: &Path) {
    write(
        &dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"gen\", \"app\"]\nresolver = \"2\"\n",
    );
    write(
        &dir.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ngen = { path = \"../gen\" }\n",
    );
    write(&dir.join("app/src/main.rs"), "fn main() { println!(\"{}\", gen::DATA); }\n");
    write(
        &dir.join("gen/Cargo.toml"),
        "[package]\nname = \"gen\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &dir.join("gen/build.rs"),
        r#"fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let flavor = std::env::var("RB_GEN_FLAVOR").unwrap_or_else(|_| "plain".into());
    // Generated code that refers to OUT_DIR by absolute path must survive relocation
    std::fs::write(format!("{out}/data.txt"), &flavor).unwrap();
    std::fs::write(format!("{out}/gen.rs"), format!("pub const DATA: &str = include_str!({:?});", format!("{out}/data.txt"))).unwrap();
    println!("cargo::rerun-if-env-changed=RB_GEN_FLAVOR");
    println!("cargo::rerun-if-changed=build.rs");
}
"#,
    );
    write(&dir.join("gen/src/lib.rs"), "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n");
}

#[test]
fn build_script_outputs_relocate_between_projects() {
    let env = Env::new();
    let a = env.root.join("a");
    build_script_project(&a);
    env.build(&a, &[("RB_GEN_FLAVOR", "salty")]);
    assert_eq!(run(&a.join("target/debug/app")), "salty");
    // A second build learns which env vars the build script depends on
    env.build(&a, &[("RB_GEN_FLAVOR", "salty")]);

    // Same `gen` elsewhere, with the original project deleted first: the build script run is
    // restored into a new OUT_DIR, and the absolute path baked into gen.rs must be rewritten
    let b = env.root.join("b");
    build_script_project(&b);
    write(&b.join("app/src/main.rs"), "fn main() { println!(\"b:{}\", gen::DATA); }\n");
    std::fs::remove_dir_all(&a).unwrap();
    let log = env.build(&b, &[("RB_GEN_FLAVOR", "salty")]);
    assert!(log.contains("Cached gen"), "{log}");
    assert_eq!(run(&b.join("target/debug/app")), "b:salty");
    let gen_rs = std::fs::read_dir(b.join("target/rb/debug/host/build"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("out/gen.rs"))
        .find(|p| p.exists())
        .unwrap();
    let text = std::fs::read_to_string(gen_rs).unwrap();
    assert!(
        text.contains(&*b.to_string_lossy()) && !text.contains(&*a.to_string_lossy()),
        "{text}"
    );

    // Changing a `rerun-if-env-changed` variable reruns the script and rebuilds its dependents
    assert!(stats(&env.build(&b, &[("RB_GEN_FLAVOR", "sweet")])).0 >= 2);
    assert_eq!(run(&b.join("target/debug/app")), "b:sweet");
}

#[test]
fn build_script_cache_opt_out() {
    let env = Env::new();
    let a = env.root.join("a");
    build_script_project(&a);
    env.build(&a, &[]);
    std::fs::remove_dir_all(a.join("target")).unwrap();
    let out = env.rb(&a, &["build", "--color", "never", "--no-cache-build-scripts"], &[]);
    assert!(out.status.success());
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(log.contains("Compiling gen"), "build script must rerun:\n{log}");
    assert_eq!(run(&a.join("target/debug/app")), "plain");
}

#[test]
fn option_env_changes_propagate_to_dependents() {
    let env = Env::new();
    let ws = env.root.join("ws");
    write(
        &ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"lib\", \"app\"]\nresolver = \"2\"\n",
    );
    write(
        &ws.join("lib/Cargo.toml"),
        "[package]\nname = \"flavor\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &ws.join("lib/src/lib.rs"),
        "pub const FLAVOR: &str = match option_env!(\"RB_TEST_LIB_FLAVOR\") { Some(v) => v, None => \"none\" };\n",
    );
    write(
        &ws.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nflavor = { path = \"../lib\" }\n",
    );
    write(&ws.join("app/src/main.rs"), "fn main() { println!(\"{}\", flavor::FLAVOR); }\n");
    let bin = ws.join("target/debug/app");
    env.build(&ws, &[("RB_TEST_LIB_FLAVOR", "a")]);
    env.build(&ws, &[("RB_TEST_LIB_FLAVOR", "a")]);
    assert_eq!(run(&bin), "a");
    env.build(&ws, &[("RB_TEST_LIB_FLAVOR", "b")]);
    assert_eq!(run(&bin), "b", "the dependent binary must not keep the stale constant");
}

#[test]
fn link_modes_and_immutability() {
    for mode in ["hardlink", "copy", "symlink", "reflink"] {
        let env = Env::new();
        let a = env.root.join("a");
        package(&a, "linked", "fn main() { println!(\"ok\"); }\n");
        env.build(&a, &[]);
        std::fs::remove_dir_all(a.join("target")).unwrap();
        assert_eq!(stats(&env.build(&a, &[("RB_LINK_MODE", mode)])).1, 1, "{mode}");
        let bin = a.join("target/debug/linked");
        assert_eq!(run(&bin), "ok", "{mode}");
        if mode == "hardlink" {
            let err = std::fs::OpenOptions::new().write(true).open(&bin);
            assert!(
                err.is_err(),
                "hardlinked artifacts must be read-only so the store cannot be corrupted"
            );
        }
    }
}

#[test]
fn concurrent_builds_share_work() {
    let env = Env::new();
    let dirs: Vec<PathBuf> = (0..3).map(|i| env.root.join(format!("wt{i}"))).collect();
    for d in &dirs {
        package(d, "par", "fn main() { println!(\"same\"); }\n");
    }
    let children: Vec<_> = dirs
        .iter()
        .map(|d| {
            Command::new(env!("CARGO_BIN_EXE_rb"))
                .args(["build", "--color", "never"])
                .current_dir(d)
                .env("RB_HOME", env.root.join("home"))
                .env("RB_STORE_DIR", env.root.join("store"))
                .env("RB_COMPACT", "sync")
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut compiled = 0;
    for c in children {
        let out = c.wait_with_output().unwrap();
        let log = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{log}");
        compiled += stats(&log).0;
    }
    assert_eq!(compiled, 1, "the per-unit lock makes the other worktrees reuse the first build");
    for d in &dirs {
        assert_eq!(run(&d.join("target/debug/par")), "same");
    }
}

#[test]
fn gc_never_breaks_materialized_projects() {
    let env = Env::new();
    let a = env.root.join("a");
    package(&a, "keep", "fn main() { println!(\"alive\"); }\n");
    env.build(&a, &[]);
    let out = env.rb(&a, &["store", "gc", "--max-age-days", "0"], &[]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Units used just now are younger than 0 days only if clocks disagree; force removal by size
    let out = env.rb(&a, &["store", "gc", "--max-size", "0"], &[]);
    assert!(out.status.success());
    assert_eq!(run(&a.join("target/debug/keep")), "alive");
    let status = env.rb(&a, &["store", "status"], &[]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("units       0"));
    assert_eq!(stats(&env.build(&a, &[])), (0, 0, 1), "project state survives GC");
    let verify = env.rb(&a, &["store", "verify"], &[]);
    assert!(verify.status.success());
}

fn disk_kib(path: &Path) -> u64 {
    let out = Command::new("du").args(["-sk", path.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn file_count(dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .map(|e| if e.path().is_dir() { file_count(&e.path()) } else { 1 })
        .sum()
}

/// Changing flags leaves a new incremental session and a new `deps/` copy behind in cargo.
/// rb keeps the latest variant plus one older one per crate, so `target/` stays about two builds
#[test]
fn target_stays_much_smaller_than_cargo_across_rebuilds() {
    let env = Env::new();
    let cargo_proj = env.root.join("cargo-proj");
    let rb_proj = env.root.join("rb-proj");
    let src = "fn main() { println!(\"ok\"); }\n";
    package(&cargo_proj, "grow", src);
    package(&rb_proj, "grow", src);
    let mut cargo_secs = Vec::new();
    let mut rb_secs = Vec::new();
    for i in 0..6 {
        let flag = format!("--cfg=rbdisk{i}");
        let start = std::time::Instant::now();
        let c = Command::new("cargo")
            .args(["build", "-q"])
            .current_dir(&cargo_proj)
            .env("RUSTFLAGS", &flag)
            .env_remove("RUSTC_WRAPPER")
            .output()
            .unwrap();
        cargo_secs.push(start.elapsed().as_secs_f64());
        assert!(c.status.success(), "cargo:\n{}", String::from_utf8_lossy(&c.stderr));
        let start = std::time::Instant::now();
        let r = env.rb(&rb_proj, &["build", "-q"], &[("RUSTFLAGS", flag.as_str())]);
        rb_secs.push(start.elapsed().as_secs_f64());
        assert!(r.status.success(), "rb:\n{}", String::from_utf8_lossy(&r.stderr));
        assert_eq!(run(&rb_proj.join("target/debug/grow")), "ok");
    }
    let cargo_kb = disk_kib(&cargo_proj.join("target"));
    let rb_kb = disk_kib(&rb_proj.join("target"));
    let cargo_files = file_count(&cargo_proj.join("target"));
    let rb_files = file_count(&rb_proj.join("target"));
    eprintln!("disk KiB cargo={cargo_kb} rb={rb_kb} files cargo={cargo_files} rb={rb_files}");
    eprintln!("seconds cargo={cargo_secs:?} rb={rb_secs:?}");
    assert!(rb_files < cargo_files, "rb left {rb_files} files, cargo left {cargo_files}");
    assert!(
        rb_kb * 2 < cargo_kb,
        "rb target is {rb_kb} KiB, cargo is {cargo_kb} KiB; rb must be well under half"
    );

    // Swept variants keep their incremental cache as an archive, restored when they come back
    let host = rb_proj.join("target/rb/debug/host");
    let archives = || -> Vec<String> {
        std::fs::read_dir(host.join("incremental-archive"))
            .map(|r| r.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default()
    };
    let before = archives();
    assert!(!before.is_empty(), "swept incremental dirs are archived");
    // Coming back restores the binary from the store without running rustc; the edit after
    // it finds the variant's incremental cache again
    for main in ["fn main() { println!(\"ok\"); }\n", "fn main() { println!(\"edited\"); }\n"] {
        std::fs::write(rb_proj.join("src/main.rs"), main).unwrap();
        let r = env.rb(&rb_proj, &["build", "-q"], &[("RUSTFLAGS", "--cfg=rbdisk2")]);
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    }
    assert_eq!(run(&rb_proj.join("target/debug/grow")), "edited");
    let after = archives();
    let restored: Vec<&String> = before
        .iter()
        .filter(|a| !after.contains(a) && host.join("incremental").join(a.trim_end_matches(".tar.zst")).is_dir())
        .collect();
    assert_eq!(
        restored.len(),
        1,
        "one archive went back into incremental/: {before:?} -> {after:?}"
    );
}
