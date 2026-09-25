//! Diff cargo and rb. Fresh target dirs, a throwaway store, `-v`.
//! Hashes, out-dirs, and UI flags are stripped. Externs are compared by name.
//! `--unit-graph` also diffs cargo's unit graph, on nightly.
//!
//! `rb-compat [--rb PATH] [--command build|check|test|bench|doc] [--unit-graph] [--release] [--target T]... [--strict] [PROJECT...]`
//! No projects means every directory under `tests/compat/fixtures`.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Opts {
    rb: PathBuf,
    unit_graph: bool,
    strict: bool,
    /// `build`, `check`, `test`, `bench` or `doc`
    command: String,
    build_args: Vec<String>,
    projects: Vec<PathBuf>,
}

impl Opts {
    /// Arguments for the compared command (tests are only built, not run)
    fn command_args(&self) -> Vec<String> {
        let mut v = vec![self.command.clone()];
        if matches!(self.command.as_str(), "test" | "bench") {
            v.push("--no-run".into());
        }
        v
    }
}

fn parse_opts() -> Result<Opts> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut o = Opts {
        rb: root.join("../../target/release/rb"),
        unit_graph: false,
        strict: false,
        command: "build".into(),
        build_args: Vec::new(),
        projects: Vec::new(),
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rb" => o.rb = PathBuf::from(args.next().context("--rb needs a path")?),
            "--unit-graph" => o.unit_graph = true,
            "--strict" => o.strict = true,
            "--command" => o.command = args.next().context("--command needs build|check|test|bench|doc")?,
            "--release" => o.build_args.push(a),
            "--target" | "--features" | "-p" | "--profile" => {
                o.build_args.push(a);
                o.build_args.push(args.next().context("missing value")?);
            }
            _ => o.projects.push(std::path::absolute(&a)?),
        }
    }
    if o.projects.is_empty() {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(root.join("fixtures"))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        o.projects = dirs;
    }
    o.rb = std::path::absolute(&o.rb)?;
    Ok(o)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let name = e.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let dst = to.join(&name);
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dst)?;
        } else {
            std::fs::copy(e.path(), dst)?;
        }
    }
    Ok(())
}

/// Commands printed as ``Running `...` `` by `-v`
fn running_lines(log: &str) -> Vec<Vec<String>> {
    log.lines()
        .filter_map(|l| l.trim_start().strip_prefix("Running `"))
        .filter_map(|l| l.rfind('`').map(|i| &l[..i]))
        .filter_map(|cmd| shell_words::split(cmd).ok())
        .map(|toks| {
            toks.into_iter()
                .skip_while(|t| !t.starts_with('-') && t.contains('=') && !t.contains('/'))
                .collect()
        })
        .collect()
}

const PAIRED: &[&str] = &[
    "-C",
    "--cfg",
    "--check-cfg",
    "--crate-type",
    "--extern",
    "-L",
    "-l",
    "--target",
    "--cap-lints",
    "--crate-name",
    "--out-dir",
    "--edition",
    "-o",
    "--crate-version",
];

fn strip_hash(component: &str) -> String {
    match component.rsplit_once('-') {
        Some((name, hash)) if hash.len() >= 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => name.to_owned(),
        _ => component.to_owned(),
    }
}

fn normalize_path(p: &str) -> String {
    match p.rfind("/build/") {
        Some(i) => {
            let rest = &p[i + "/build/".len()..];
            let mut parts = rest.splitn(2, '/');
            let first = strip_hash(parts.next().unwrap_or(""));
            match parts.next() {
                Some(tail) => format!("{{BUILD}}/{first}/{tail}"),
                None => format!("{{BUILD}}/{first}"),
            }
        }
        None => p.to_owned(),
    }
}

/// cargo extracts crates to `$CARGO_HOME/registry/src/<index-dir>/`, rb to
/// `~/.rb/registry/src/<index-dir>/`; compare them as one location
fn normalize_registry(s: &str) -> String {
    match s.find("/registry/src/") {
        Some(i) => {
            let start = s[..i].rfind(['=', ' ']).map(|p| p + 1).unwrap_or(0);
            let rest = &s[i + "/registry/src/".len()..];
            let after = rest.find('/').map(|p| &rest[p..]).unwrap_or("");
            format!("{}{{REGISTRY}}{after}", &s[..start])
        }
        None => s.to_owned(),
    }
}

#[derive(Debug, Default)]
struct Unit {
    args: BTreeMap<String, usize>,
}

/// Identity of a rustc invocation plus its normalized argument multiset
fn normalize(tokens: &[String]) -> Option<(String, Unit)> {
    let program = tokens.first()?;
    let tool = if program.ends_with("rustdoc") {
        "rustdoc"
    } else if program.ends_with("rustc") {
        "rustc"
    } else {
        return None;
    };
    let mut items: Vec<String> = Vec::new();
    let mut it = tokens[1..].iter();
    while let Some(t) = it.next() {
        if PAIRED.contains(&t.as_str()) {
            let v = it.next().cloned().unwrap_or_default();
            items.push(format!("{t} {v}"));
        } else {
            items.push(t.clone());
        }
    }
    let (mut crate_name, mut src, mut target, mut types, mut feats) =
        (String::new(), String::new(), "host".to_owned(), Vec::new(), Vec::new());
    let mut unit = Unit::default();
    for item in items {
        let drop = [
            "-C metadata=",
            "-C extra-filename=",
            "--out-dir ",
            "-C incremental=",
            "-L dependency=",
            "--error-format=",
            "--json=",
            "--diagnostic-width=",
            "--color=",
            "-Zthreads=",
            "-o ",
        ]
        .iter()
        .any(|p| item.starts_with(p))
            || item == "--verbose"
            // rb's provisioned cross toolchains (zig wrappers, rust-lld, xwin libraries) have no
            // cargo equivalent; the fixtures configure no linker of their own
            || item.contains("/.rb/toolchains/")
            || item.starts_with("-C linker=")
            || item == "-Clink-arg=/ignore:4099";
        if drop {
            continue;
        }
        let item = normalize_registry(&item);
        let item = if let Some(rest) = item.strip_prefix("--extern ") {
            format!("--extern {}", rest.split('=').next().unwrap())
        } else if let Some(rest) = item.strip_prefix("-L ") {
            format!("-L {}", normalize_path(rest))
        } else if item.starts_with("--edition ") {
            item.replacen("--edition ", "--edition=", 1)
        } else {
            item
        };
        if let Some(n) = item.strip_prefix("--crate-name ") {
            crate_name = n.to_owned();
        } else if let Some(t) = item.strip_prefix("--target ") {
            target = t.to_owned();
        } else if let Some(t) = item.strip_prefix("--crate-type ") {
            types.push(t.to_owned());
        } else if let Some(f) = item.strip_prefix("--cfg feature=") {
            feats.push(f.to_owned());
        } else if !item.starts_with('-') && item.ends_with(".rs") {
            src = item.clone();
        }
        *unit.args.entry(item).or_default() += 1;
    }
    types.sort();
    feats.sort();
    let test = if unit.args.contains_key("--test") { " --test" } else { "" };
    let meta_only = unit.args.keys().any(|a| a.starts_with("--emit=") && !a.contains("link"));
    let test = if meta_only { format!("{test} (metadata)") } else { test.to_owned() };
    Some((
        format!(
            "{tool} {crate_name} [{}]{test} {target} {src} features({})",
            types.join(","),
            feats.join(",")
        ),
        unit,
    ))
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("failed to run {cmd:?}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        bail!("{cmd:?} failed:\n{text}");
    }
    Ok(text)
}

fn units_of(log: &str) -> (BTreeMap<String, Unit>, BTreeSet<String>) {
    let mut units = BTreeMap::new();
    let mut scripts = BTreeSet::new();
    for toks in running_lines(log) {
        if let Some((id, u)) = normalize(&toks) {
            units.insert(id, u);
        } else if let Some(p) = toks
            .first()
            .filter(|p| p.contains("build-script-build") || p.contains("build_script_build-"))
        {
            scripts.insert(normalize_path(Path::new(p).parent().unwrap().to_str().unwrap()));
        }
    }
    (units, scripts)
}

struct Report {
    problems: usize,
}

fn compare_argv(project: &Path, work: &Path, o: &Opts, r: &mut Report) -> Result<()> {
    let cargo_log = run(Command::new("cargo")
        .args(o.command_args())
        .args(["-v", "--target-dir"])
        .arg(work.join("cargo-target"))
        .args(&o.build_args)
        .current_dir(project))?;
    let rb_log = run(Command::new(&o.rb)
        .args(o.command_args())
        .args(["-v", "--color", "never", "--target-dir"])
        .arg(work.join("rb-target"))
        .args(&o.build_args)
        .env("RB_STORE_DIR", work.join("store"))
        .current_dir(project))?;
    let (cargo_units, cargo_scripts) = units_of(&cargo_log);
    let (rb_units, rb_scripts) = units_of(&rb_log);
    let mut same = 0;
    for (id, cu) in &cargo_units {
        match rb_units.get(id) {
            None => {
                r.problems += 1;
                println!("  - only in cargo: {id}");
            }
            Some(ru) => {
                let only_c: Vec<&String> = cu.args.iter().filter(|(k, v)| ru.args.get(*k) != Some(v)).map(|(k, _)| k).collect();
                let only_r: Vec<&String> = ru.args.iter().filter(|(k, v)| cu.args.get(*k) != Some(v)).map(|(k, _)| k).collect();
                if only_c.is_empty() && only_r.is_empty() {
                    same += 1;
                } else {
                    r.problems += 1;
                    println!("  ~ {id}\n      cargo only: {only_c:?}\n      rb only:    {only_r:?}");
                }
            }
        }
    }
    for id in rb_units.keys().filter(|k| !cargo_units.contains_key(*k)) {
        r.problems += 1;
        println!("  + only in rb: {id}");
    }
    if cargo_scripts != rb_scripts {
        r.problems += 1;
        println!("  build script runs differ:\n      cargo: {cargo_scripts:?}\n      rb:    {rb_scripts:?}");
    }
    println!(
        "  argv: {same}/{} rustc invocations identical after normalization, {} build script runs",
        cargo_units.len(),
        cargo_scripts.len()
    );
    Ok(())
}

type GraphUnit = (String, String, String, String, String, String);

fn graph_units(v: &serde_json::Value) -> (BTreeSet<GraphUnit>, BTreeSet<(GraphUnit, GraphUnit)>) {
    let units = v["units"].as_array().cloned().unwrap_or_default();
    let key = |u: &serde_json::Value| -> GraphUnit {
        let feats: Vec<String> = u["features"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str())
            .map(str::to_owned)
            .collect();
        let kinds: Vec<String> = u["target"]["kind"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str())
            .map(str::to_owned)
            .collect();
        (
            u["pkg_id"].as_str().unwrap_or("").to_owned(),
            u["target"]["name"].as_str().unwrap_or("").to_owned(),
            kinds.join(","),
            u["platform"].as_str().unwrap_or("host").to_owned(),
            u["mode"].as_str().unwrap_or("").to_owned(),
            feats.join(","),
        )
    };
    let keys: Vec<GraphUnit> = units.iter().map(key).collect();
    // rb runs doctests after the build instead of modelling them as units
    let kept = |i: usize| units[i]["mode"] != "doctest";
    let mut edges = BTreeSet::new();
    for (i, u) in units.iter().enumerate().filter(|(i, _)| kept(*i)) {
        for d in u["dependencies"].as_array().into_iter().flatten() {
            if let Some(j) = d["index"].as_u64() {
                edges.insert((keys[i].clone(), keys[j as usize].clone()));
            }
        }
    }
    (
        keys.iter().enumerate().filter(|(i, _)| kept(*i)).map(|(_, k)| k.clone()).collect(),
        edges,
    )
}

fn compare_unit_graph(project: &Path, work: &Path, o: &Opts, r: &mut Report) -> Result<()> {
    let cargo = run(Command::new("cargo")
        .arg("+nightly")
        .args(o.command_args())
        .args(["--unit-graph", "-Zunstable-options", "--target-dir"])
        .arg(work.join("cargo-target"))
        .args(&o.build_args)
        .current_dir(project))?;
    let rb = run(Command::new(&o.rb)
        .args(o.command_args())
        .args(["--unit-graph", "--target-dir"])
        .arg(work.join("rb-target"))
        .args(&o.build_args)
        .env("RB_STORE_DIR", work.join("store"))
        .current_dir(project))?;
    let (cu, ce) = graph_units(&serde_json::from_str(&cargo)?);
    let (ru, re) = graph_units(&serde_json::from_str(&rb)?);
    for u in cu.difference(&ru) {
        r.problems += 1;
        println!("  - unit only in cargo: {u:?}");
    }
    for u in ru.difference(&cu) {
        r.problems += 1;
        println!("  + unit only in rb: {u:?}");
    }
    let edge_diff = ce.symmetric_difference(&re).count();
    if edge_diff > 0 {
        r.problems += 1;
        println!("  {edge_diff} dependency edges differ");
    }
    println!(
        "  unit graph: {} cargo units, {} rb units, {} common",
        cu.len(),
        ru.len(),
        cu.intersection(&ru).count()
    );
    Ok(())
}

fn main() -> Result<()> {
    let o = parse_opts()?;
    if !o.rb.is_file() {
        bail!(
            "rb binary not found at {} (build it with `cargo build --release` or pass --rb)",
            o.rb.display()
        );
    }
    let mut report = Report { problems: 0 };
    for project in &o.projects {
        let work = tempfile::tempdir()?;
        let src = work.path().join("src");
        // Copy fixtures out of the repo so its rust-toolchain.toml and target dir don't apply
        let dir = if project.starts_with(env!("CARGO_MANIFEST_DIR")) {
            copy_dir(project, &src)?;
            src
        } else {
            project.clone()
        };
        println!("== {}", project.display());
        if let Err(e) = compare_argv(&dir, work.path(), &o, &mut report) {
            report.problems += 1;
            println!("  build failed: {e:#}");
        }
        if o.unit_graph
            && let Err(e) = compare_unit_graph(&dir, work.path(), &o, &mut report)
        {
            report.problems += 1;
            println!("  unit graph failed: {e:#}");
        }
    }
    println!("\n{} difference(s)", report.problems);
    if o.strict && report.problems > 0 {
        std::process::exit(1);
    }
    Ok(())
}
