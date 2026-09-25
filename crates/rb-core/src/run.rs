//! What happens after the build: run, test, bench, or open the docs.

use crate::build::{AlreadyReported, BuildOptions, ProjectState};
use crate::exec::Engine;
use crate::invocation::Invocation;
use crate::plan::{dep_output_path, pkg_env, uplifted_bin};
use crate::unit::{Command, CompileKind, Mode, Unit};
use crate::workspace::TargetKind;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command as Process;

fn executable(e: &Engine<'_>, state: &ProjectState, u: usize) -> Option<PathBuf> {
    let rec = state.units.get(&e.planned[u].key)?;
    rec.outputs
        .iter()
        .find(|o| {
            let n = o.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            !(n.ends_with(".rmeta") || n.ends_with(".rlib") || n.ends_with(".pdb") || n.ends_with(".d") || n.ends_with(".lib"))
        })
        .cloned()
}

fn relative<'a>(e: &Engine<'_>, p: &'a Path) -> std::borrow::Cow<'a, str> {
    match p.strip_prefix(&e.ctx.ws.root) {
        Ok(r) => r.to_string_lossy(),
        Err(_) => p.to_string_lossy(),
    }
}

/// The copy in `target/<profile>/deps/` when it's there. That's where the test actually runs.
fn test_executable(e: &Engine<'_>, state: &ProjectState, r: usize) -> Option<PathBuf> {
    let built = executable(e, state, r)?;
    let placed = crate::build::test_exe_dir(e, &e.graph.units[r]).join(built.file_name()?);
    Some(if placed.is_file() { placed } else { built })
}

/// What cargo puts in the environment: package metadata, `OUT_DIR`, `rustc-env`, `CARGO_BIN_EXE_*`, and a library path.
fn program_env(e: &Engine<'_>, ui: usize) -> Invocation {
    let ctx = e.ctx;
    let u = &e.graph.units[ui];
    let mut inv = Invocation::new("", "");
    pkg_env(&mut inv, ctx, u.pkg);
    if let Some(r) = e.planned[ui].own_run {
        inv.env_unhashed("OUT_DIR", e.planned[r].out_dir_path().to_string_lossy());
        for (key, value) in e.build_output_for(r).map(|o| o.env.clone()).unwrap_or_default() {
            inv.env_unhashed(key, value);
        }
    }
    if matches!(u.target(ctx.ws).kind, TargetKind::Test | TargetKind::Bench) {
        for d in &u.deps {
            let du = &e.graph.units[d.unit];
            if d.extern_name.is_none() && du.mode == Mode::Build && du.target(ctx.ws).kind == TargetKind::Bin {
                inv.env_unhashed(
                    format!("CARGO_BIN_EXE_{}", du.target(ctx.ws).name),
                    uplifted_bin(ctx, du).to_string_lossy(),
                );
            }
        }
    }
    let k = ctx.kind(u.kind);
    let var = if cfg!(target_os = "macos") {
        "DYLD_FALLBACK_LIBRARY_PATH"
    } else if cfg!(windows) {
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    let mut dirs = vec![k.deps_dir.clone(), ctx.host.deps_dir.clone(), ctx.rustc.target_libdir(&k.triple)];
    if let Some(existing) = std::env::var_os(var) {
        dirs.extend(std::env::split_paths(&existing));
    }
    if let Ok(joined) = std::env::join_paths(dirs) {
        inv.env_unhashed(var, joined.to_string_lossy());
    }
    inv
}

fn command_for(e: &Engine<'_>, ui: usize, program: &Path, cwd: &Path) -> Process {
    let k = e.ctx.kind(e.graph.units[ui].kind);
    let mut cmd = match &k.runner {
        Some(r) => {
            let mut c = Process::new(&r[0]);
            c.args(&r[1..]).arg(program);
            c
        }
        None => Process::new(program),
    };
    for (key, (value, _)) in &program_env(e, ui).env {
        cmd.env(key, value);
    }
    cmd.current_dir(cwd);
    cmd
}

pub fn run_binary(e: &Engine<'_>, state: &ProjectState, args: &[String]) -> Result<()> {
    let root = *e.graph.roots.first().context("nothing to run")?;
    let u = &e.graph.units[root];
    let t = u.target(e.ctx.ws);
    let k = e.ctx.kind(u.kind);
    let dir = if t.kind == TargetKind::Example {
        k.artifact_dir.join("examples")
    } else {
        k.artifact_dir.clone()
    };
    let exe_suffix = k.info.file_type("bin").map(|f| f.suffix.clone()).unwrap_or_default();
    let program = dir.join(format!("{}{exe_suffix}", t.name));
    let program = if program.is_file() {
        program
    } else {
        executable(e, state, root).context("the built binary is missing")?
    };
    let shown = std::iter::once(relative(e, &program).into_owned())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>();
    e.ctx.shell.status("Running", format!("`{}`", shown.join(" ")));
    let cwd = std::env::current_dir()?;
    let status = command_for(e, root, &program, &cwd)
        .args(args)
        .status()
        .with_context(|| format!("could not execute process `{}`", program.display()))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(101));
    }
    Ok(())
}

/// Lib, bins, integration tests, examples, benches. Cargo's order.
fn test_roots(e: &Engine<'_>) -> Vec<usize> {
    let ws = e.ctx.ws;
    let mut roots: Vec<usize> = e
        .graph
        .roots
        .iter()
        .copied()
        .filter(|&r| {
            let u = &e.graph.units[r];
            let t = u.target(ws);
            matches!(u.mode, Mode::Test | Mode::Bench) || (u.mode == Mode::Build && matches!(t.kind, TargetKind::Test | TargetKind::Bench))
        })
        .collect();
    let rank = |k: TargetKind| match k {
        TargetKind::Lib => 0,
        TargetKind::Bin => 1,
        TargetKind::Test => 2,
        TargetKind::Example => 3,
        TargetKind::Bench => 4,
        TargetKind::BuildScript => 5,
    };
    roots.sort_by_key(|&r| {
        let u = &e.graph.units[r];
        let t = u.target(ws);
        (ws.pkgs[u.pkg].name.clone(), rank(t.kind), t.name.clone(), u.kind)
    });
    roots
}

fn describe(e: &Engine<'_>, u: &Unit) -> String {
    let t = u.target(e.ctx.ws);
    let root = &e.ctx.ws.pkgs[u.pkg].root;
    let src = t.src_path.strip_prefix(root).unwrap_or(&t.src_path).to_string_lossy().into_owned();
    match t.kind {
        TargetKind::Lib | TargetKind::Bin => format!("unittests {src}"),
        _ => src,
    }
}

fn rerun_hint(e: &Engine<'_>, u: &Unit) -> String {
    let p = &e.ctx.ws.pkgs[u.pkg];
    let t = u.target(e.ctx.ws);
    match t.kind {
        TargetKind::Lib => format!("-p {} --lib", p.name),
        TargetKind::Bin => format!("-p {} --bin {}", p.name, t.name),
        TargetKind::Test => format!("-p {} --test {}", p.name, t.name),
        TargetKind::Bench => format!("-p {} --bench {}", p.name, t.name),
        TargetKind::Example => format!("-p {} --example {}", p.name, t.name),
        TargetKind::BuildScript => format!("-p {}", p.name),
    }
}

pub fn list_test_executables(e: &Engine<'_>, state: &ProjectState) -> Result<()> {
    for r in test_roots(e) {
        let u = &e.graph.units[r];
        if let Some(exe) = executable(e, state, r) {
            e.ctx
                .shell
                .status("Executable", format!("{} ({})", describe(e, u), relative(e, &exe)));
        }
    }
    Ok(())
}

pub fn run_tests(e: &Engine<'_>, state: &ProjectState, opts: &BuildOptions) -> Result<()> {
    let shell = e.ctx.shell;
    let bench = opts.command == Command::Bench;
    let mut failed: Vec<String> = Vec::new();
    let fail = |failed: &mut Vec<String>, what: &str, hint: String| -> Result<()> {
        if opts.no_fail_fast {
            failed.push(hint);
            Ok(())
        } else {
            shell.error(format!("{what} failed, to rerun pass `{hint}`"));
            Err(AlreadyReported.into())
        }
    };
    for r in test_roots(e) {
        let u = &e.graph.units[r];
        let exe = test_executable(e, state, r).with_context(|| format!("test executable for `{}` is missing", describe(e, u)))?;
        shell.status("Running", format!("{} ({})", describe(e, u), relative(e, &exe)));
        let mut cmd = command_for(e, r, &exe, &e.ctx.ws.pkgs[u.pkg].root);
        cmd.args(&opts.args);
        if bench {
            cmd.arg("--bench");
        }
        let status = cmd
            .status()
            .with_context(|| format!("could not execute process `{}`", exe.display()))?;
        if !status.success() {
            fail(&mut failed, "test", rerun_hint(e, u))?;
        }
    }
    if opts.command == Command::Test {
        for d in &e.graph.doctests {
            let ok = run_doctest(e, d, &opts.args)?;
            if !ok {
                fail(&mut failed, "doctest", format!("-p {} --doc", e.ctx.ws.pkgs[d.pkg].name))?;
            }
        }
    }
    if !failed.is_empty() {
        let list: Vec<String> = failed.iter().map(|f| format!("    `{f}`")).collect();
        shell.error(format!(
            "{} target{} failed:\n{}",
            failed.len(),
            if failed.len() == 1 { "" } else { "s" },
            list.join("\n")
        ));
        return Err(AlreadyReported.into());
    }
    Ok(())
}

fn run_doctest(e: &Engine<'_>, d: &crate::unit::Doctest, args: &[String]) -> Result<bool> {
    let ctx = e.ctx;
    let ws = ctx.ws;
    let units = &e.graph.units;
    let lib = &units[d.lib];
    let p = &ws.pkgs[d.pkg];
    let t = lib.target(ws);
    let k = ctx.kind(lib.kind);
    let crate_name = t.crate_name();
    ctx.shell.status("Doc-tests", &crate_name);
    let rustdoc = ctx.rustc.path.with_file_name(format!("rustdoc{}", std::env::consts::EXE_SUFFIX));
    let mut cmd = Process::new(rustdoc);
    cmd.arg(format!("--edition={}", t.edition));
    for ct in &t.crate_types {
        cmd.args(["--crate-type", ct]);
    }
    // Doctest names are relative to the workspace root, same as cargo.
    let (src, cwd) = match t.src_path.strip_prefix(&ws.root) {
        Ok(rel) if p.source.is_local() => (rel.to_path_buf(), ws.root.clone()),
        _ => (t.src_path.clone(), p.root.clone()),
    };
    cmd.args(["--crate-name", &crate_name, "--test"]).arg(&src);
    cmd.arg("--test-run-directory").arg(&p.root);
    cmd.arg("-L").arg(format!("dependency={}", k.deps_dir.display()));
    if lib.kind != CompileKind::Host {
        cmd.arg("-L").arg(format!("dependency={}", ctx.host.deps_dir.display()));
    }
    let lib_path = dep_output_path(ctx, lib, &e.planned[d.lib], false)?;
    cmd.arg("--extern").arg(format!("{crate_name}={}", lib_path.display()));
    if t.is_proc_macro() {
        cmd.args(["--extern", "proc_macro"]);
    }
    for dep in lib.deps.iter().chain(&d.dev_deps) {
        if let Some(name) = &dep.extern_name {
            let path = dep_output_path(ctx, &units[dep.unit], &e.planned[dep.unit], false)?;
            cmd.arg("--extern").arg(format!("{name}={}", path.display()));
        }
    }
    for f in &lib.features {
        cmd.arg("--cfg").arg(format!("feature=\"{f}\""));
    }
    let lp = &e.planned[d.lib];
    if let Some(out) = lp.own_run.and_then(|r| e.build_output_for(r)) {
        for c in &out.cfgs {
            cmd.args(["--cfg", c]);
        }
    }
    for &r in &lp.to_link {
        if let Some(out) = e.build_output_for(r) {
            for s in &out.link_search {
                cmd.args(["-L", s]);
            }
        }
    }
    if let Some(tf) = &k.target_flag {
        cmd.args(["--target", tf]);
        if tf.ends_with(".json") {
            cmd.arg("-Zunstable-options");
        }
    }
    if let Some(l) = &k.linker {
        cmd.arg("-C").arg(format!("linker={}", l.display()));
    }
    if ctx.shell.color() {
        cmd.arg("--color=always");
    }
    for a in args {
        cmd.args(["--test-args", a]);
    }
    for (key, (value, _)) in &program_env(e, d.lib).env {
        cmd.env(key, value);
    }
    cmd.current_dir(cwd);
    let status = cmd.status().context("could not run rustdoc --test")?;
    Ok(status.success())
}

/// Cargo's `Generated` line, and `--open` on the first one.
pub fn report_docs(e: &Engine<'_>, state: &ProjectState, open: bool) -> Result<()> {
    let indexes: Vec<PathBuf> = e
        .graph
        .roots
        .iter()
        .filter(|&&r| e.graph.units[r].mode == Mode::Doc)
        .filter_map(|&r| state.units.get(&e.planned[r].key))
        .filter_map(|rec| rec.outputs.first().cloned())
        .collect();
    let Some(index) = indexes.first() else {
        bail!("no documentation was generated")
    };
    let others = match indexes.len() - 1 {
        0 => String::new(),
        1 => " and 1 other file".to_owned(),
        n => format!(" and {n} other files"),
    };
    e.ctx.shell.status("Generated", format!("{}{others}", index.display()));
    if !open {
        return Ok(());
    }
    e.ctx.shell.status("Opening", index.display());
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let status = Process::new(opener).arg(index).status();
    if !status.is_ok_and(|s| s.success()) {
        bail!("could not open {}; open it in a browser", index.display());
    }
    Ok(())
}
