//! `doctor`, `target`, `store`.

use anyhow::Result;
use rb_core::config::RbConfig;
use rb_core::shell::Shell;
use rb_toolchain::{Rustc, TargetRequest, Toolchains, binfmt};
use std::process::Command;

pub const COMMON_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-pc-windows-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

pub use rb_store::human_bytes;

pub struct TargetStatus {
    pub std: bool,
    pub toolchain: Result<String, String>,
}

pub fn target_status(rustc: &Rustc, tc: &Toolchains, triple: &str) -> TargetStatus {
    let req = TargetRequest::parse(triple);
    TargetStatus {
        std: rustc.has_std(&req.triple),
        toolchain: tc.setup(rustc, &req, false).map(|t| t.description).map_err(|e| e.to_string()),
    }
}

pub fn print_target_list(rustc: &Rustc, tc: &Toolchains) {
    println!("{:<30} {:<5} toolchain", "target", "std");
    for t in COMMON_TARGETS {
        let st = target_status(rustc, tc, t);
        let tool = match &st.toolchain {
            Ok(d) => d.clone(),
            Err(_) => "not provisioned (rb target add)".into(),
        };
        let host = if *t == rustc.host { " (host)" } else { "" };
        println!("{:<30} {:<5} {tool}{host}", t, if st.std { "yes" } else { "no" });
    }
}

pub fn smoke(rustc: &Rustc, tc: &Toolchains, triple: Option<&str>) -> Result<String> {
    let dir = std::env::temp_dir().join(format!("rb-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let src = dir.join("main.rs");
    std::fs::write(&src, "fn main() { println!(\"hello from rb\"); }\n")?;
    let out = dir.join(format!("hello-{}", triple.unwrap_or("host")));
    let mut cmd = Command::new(&rustc.path);
    cmd.arg(&src).args(["--edition", "2021", "--crate-name", "hello", "-o"]).arg(&out);
    if let Some(t) = triple {
        let req = TargetRequest::parse(t);
        let cross = tc.setup(rustc, &req, false)?;
        cmd.args(["--target", &req.triple]);
        if let Some(l) = &cross.linker {
            cmd.arg("-C").arg(format!("linker={}", l.display()));
        }
        cmd.args(&cross.rustflags);
        cmd.envs(cross.rustc_env.iter().map(|(k, v)| (k, v)));
        if !cross.path_prepend.is_empty() {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let joined = std::env::join_paths(cross.path_prepend.iter().cloned().chain(std::env::split_paths(&path)))?;
            cmd.env("PATH", joined);
        }
    }
    let output = cmd.current_dir(&dir).output()?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr));
    }
    let produced = if out.exists() { out.clone() } else { out.with_extension("exe") };
    let desc = binfmt::describe(&produced).unwrap_or_else(|| "unknown format".into());
    let _ = std::fs::remove_dir_all(&dir);
    Ok(desc)
}

pub fn doctor(shell: &Shell, cfg: &RbConfig, smoke_test: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let rustc = Rustc::detect(None, &cwd, &cfg.cache_dir())?;
    let tc = cfg.toolchains()?;
    println!("rb {}", env!("CARGO_PKG_VERSION"));
    println!("rustc     {} ({})", rustc.release, rustc.path.display());
    println!("host      {}", rustc.host);
    println!(
        "channel   {}",
        if rustc.nightly {
            "nightly (parallel frontend available)"
        } else {
            "stable"
        }
    );
    println!("rb home   {}", cfg.home.display());
    let store_state = if cfg.store_enabled { "enabled" } else { "disabled" };
    println!("store     {} ({store_state}, link mode {})", cfg.store_dir.display(), cfg.link_mode);
    let (probe_dir, where_) = if cwd.join("Cargo.toml").is_file() {
        (cwd.join("target").join("rb"), "./target")
    } else {
        (std::env::temp_dir().join("rb-link-probe"), "the temp dir")
    };
    let probe = rb_store::link::probe(&cfg.store_dir.join("v1").join("tmp"), &probe_dir);
    println!("links     store -> {where_} resolves to `{probe}`");
    match rb_toolchain::zig::find(&tc.home, &tc.zig_version) {
        Some(z) => println!("zig       {}.{}.{} ({})", z.version.0, z.version.1, z.version.2, z.path.display()),
        None => println!("zig       not installed (auto-provisioned for linux / windows-gnu targets)"),
    }
    #[cfg(feature = "msvc")]
    {
        let arches: Vec<&str> = ["x86_64", "aarch64", "x86"]
            .into_iter()
            .filter(|a| rb_toolchain::xwin::find(&tc.home, a).is_some())
            .collect();
        if arches.is_empty() {
            println!("msvc      CRT/SDK not installed (rb target add <triple>-pc-windows-msvc --accept-license)");
        } else {
            println!("msvc      CRT/SDK for {}", arches.join(", "));
        }
    }
    println!();
    print_target_list(&rustc, &tc);
    if smoke_test {
        println!();
        let ready: Vec<&str> = COMMON_TARGETS
            .iter()
            .copied()
            .filter(|t| *t != rustc.host)
            .filter(|t| {
                let s = target_status(&rustc, &tc, t);
                s.std && s.toolchain.is_ok()
            })
            .collect();
        match smoke(&rustc, &tc, None) {
            Ok(d) => shell.status("Linked", format!("{} -> {d}", rustc.host)),
            Err(e) => shell.error(format!("host smoke test failed: {e}")),
        }
        for t in ready {
            match smoke(&rustc, &tc, Some(t)) {
                Ok(d) => shell.status("Linked", format!("{t} -> {d}")),
                Err(e) => shell.error(format!("{t}: {e}")),
            }
        }
    }
    Ok(())
}

pub fn target_add(shell: &Shell, cfg: &RbConfig, targets: &[String], accept_license: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let rustc = Rustc::detect(None, &cwd, &cfg.cache_dir())?;
    let mut tc = cfg.toolchains()?;
    if accept_license {
        tc.accept_msvc_license = true;
    }
    #[cfg(feature = "msvc")]
    {
        let arches: Vec<&str> = targets
            .iter()
            .filter(|t| t.ends_with("-windows-msvc"))
            .filter_map(|t| rb_toolchain::xwin::arch_dir(t).ok())
            .collect();
        if !arches.is_empty() {
            rb_toolchain::xwin::ensure(&tc.home, &arches, tc.accept_msvc_license)?;
        }
    }
    for t in targets {
        let req = TargetRequest::parse(t);
        if !rustc.has_std(&req.triple) {
            shell.status("Installing", format!("rust-std for {}", req.triple));
            rb_toolchain::target::rustup_add_target(&req.triple, &cwd)?;
        }
        let cross = tc.setup(&rustc, &req, true)?;
        if accept_license && req.triple.ends_with("-windows-msvc") {
            cfg.record_msvc_license_acceptance()?;
        }
        shell.status("Ready", format!("{} ({})", req.display(), cross.description));
    }
    Ok(())
}
