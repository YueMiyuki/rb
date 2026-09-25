//! Load, plan, restore what the store has, compile the rest, uplift.
use crate::cfgexpr::PlatformCfg;
use crate::config::{CargoConfig, RbConfig};
use crate::exec::{Engine, Record};
use crate::features::Platforms;
use crate::invocation::Normalizer;
use crate::layout::Layout;
use crate::plan::{self, Ctx, KindInfo};
use crate::profile::Profiles;
use crate::shell::Shell;
use crate::srchash::SourceHasher;
use crate::timings::{TimingRow, Timings};
use crate::unit::{self, Command, CompileKind, GraphInputs, Mode, RootSelection, TargetFilter, Unit};
use crate::workspace::{Env, FeatureRequest, LoadOptions, TargetKind, Workspace};
use anyhow::{Context, Result, anyhow, bail};
use rb_store::{LinkMode, Store};
use rb_toolchain::{CrossToolchain, Rustc, TargetInfo, TargetRequest};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct BuildOptions {
    pub command: Command,
    pub filter: TargetFilter,
    pub packages: Vec<String>,
    pub workspace: bool,
    pub exclude: Vec<String>,
    /// Program or test-harness args.
    pub args: Vec<String>,
    pub no_run: bool,
    pub no_fail_fast: bool,
    pub open: bool,
    pub release: bool,
    pub profile: Option<String>,
    pub targets: Vec<String>,
    pub jobs: Option<usize>,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub locked: bool,
    pub frozen: bool,
    pub offline: bool,
    pub target_dir: Option<PathBuf>,
    pub manifest_path: Option<PathBuf>,
    pub timings: bool,
    pub unit_graph: bool,
    pub no_store: bool,
    pub link_mode: Option<LinkMode>,
    pub no_cache_build_scripts: bool,
    pub no_provision: bool,
    pub message_format: crate::shell::MessageFormat,
    pub ignore_rust_version: bool,
    pub keep_going: bool,
    pub extra_rustc: Vec<String>,
    /// Extra rustc args on every crate, including std when we build it.
    pub std_rustc: Vec<String>,
    pub extra_rustdoc: Vec<String>,
    pub unstable: Vec<String>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            command: Command::Build,
            filter: TargetFilter::default(),
            packages: Vec::new(),
            workspace: false,
            exclude: Vec::new(),
            args: Vec::new(),
            no_run: false,
            no_fail_fast: false,
            open: false,
            release: false,
            profile: None,
            targets: Vec::new(),
            jobs: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            locked: false,
            frozen: false,
            offline: false,
            target_dir: None,
            manifest_path: None,
            timings: false,
            unit_graph: false,
            no_store: false,
            link_mode: None,
            no_cache_build_scripts: false,
            no_provision: false,
            message_format: crate::shell::MessageFormat::Human,
            ignore_rust_version: false,
            keep_going: false,
            extra_rustc: Vec::new(),
            std_rustc: Vec::new(),
            extra_rustdoc: Vec::new(),
            unstable: Vec::new(),
        }
    }
}

struct Unstable {
    json_target_spec: bool,
    /// `-Zbuild-std=core`. compiler_builtins comes along.
    build_core: bool,
}

fn parse_unstable(flags: &[String]) -> Result<Unstable> {
    let mut json_target_spec = false;
    let mut build_core = false;
    for flag in flags {
        if flag == "json-target-spec" {
            json_target_spec = true;
            continue;
        }
        if flag == "build-std" || flag.starts_with("build-std=") {
            let crates: Vec<&str> = flag
                .strip_prefix("build-std=")
                .map(|rest| rest.split(',').map(str::trim).filter(|s| !s.is_empty()).collect())
                .unwrap_or_else(|| vec!["std", "panic_unwind"]);
            if crates.iter().any(|c| *c != "core" && *c != "compiler_builtins") {
                bail!("`-Zbuild-std` for {} is not supported by rb yet (`core` is)", crates.join(","));
            }
            build_core = true;
            continue;
        }
        bail!("unknown `-Z` flag specified: {flag}");
    }
    Ok(Unstable { json_target_spec, build_core })
}

#[derive(Debug)]
pub struct AlreadyReported;

impl std::fmt::Display for AlreadyReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("build failed")
    }
}

impl std::error::Error for AlreadyReported {}

#[derive(Default, Serialize, Deserialize)]
pub struct ProjectState {
    pub units: HashMap<String, Record>,
    /// Name id → key that currently owns that name's files. Fresh only while it owns the slot.
    #[serde(default)]
    pub slots: HashMap<String, String>,
    /// Name id → last time a build needed it. Older than `target.keep-unused-days` gets swept.
    #[serde(default)]
    pub used: HashMap<String, u64>,
    /// Name id → which crate it's a variant of. `target.keep-variants` older ones stay.
    #[serde(default)]
    pub groups: HashMap<String, String>,
    /// In this build's graph. Not swept.
    #[serde(skip)]
    touched: HashSet<String>,
    /// Name id → store key the last edit replaced. One revert is a hit; the one before that goes.
    #[serde(default)]
    prev: HashMap<String, String>,
    /// Keys to drop: older than `prev`, plus variants swept out of target/.
    #[serde(skip)]
    superseded: Vec<String>,
    /// Last time fresh units were marked used in the store. Daily, so the cap drops experiments first.
    #[serde(default)]
    pub store_touched: u64,
}

impl ProjectState {
    fn record(&mut self, p: &plan::Planned, rec: Record) {
        self.slots.insert(p.name.clone(), p.key.clone());
        self.units.insert(p.key.clone(), rec);
        self.touch(p);
    }

    fn touch(&mut self, p: &plan::Planned) {
        self.used.insert(p.name.clone(), rb_store::now_secs());
        self.touched.insert(p.name.clone());
        let dir = match p.out_dir.file_name() {
            Some(f) if f.to_string_lossy().contains(p.name16()) => p.out_dir.parent().unwrap_or(&p.out_dir),
            _ => &p.out_dir,
        };
        self.groups
            .insert(p.name.clone(), format!("{} {} {}", p.crate_name, p.descr, dir.display()));
    }

    fn prune(&mut self) {
        let owners: HashSet<&String> = self.slots.values().collect();
        self.units.retain(|k, _| owners.contains(k));
    }

    /// Drop unused variants past `cutoff` or past `keep_variants` per crate. Returns live name prefixes.
    fn sweep_state(&mut self, cutoff: u64, keep_variants: usize) -> HashSet<String> {
        let mut older: HashMap<&str, Vec<(u64, &String)>> = HashMap::new();
        for n in self.slots.keys().filter(|n| !self.touched.contains(*n)) {
            let group = self.groups.get(n).map_or(n.as_str(), |g| g.as_str());
            older.entry(group).or_default().push((self.used.get(n).copied().unwrap_or(0), n));
        }
        let mut stale: Vec<String> = Vec::new();
        for mut variants in older.into_values() {
            variants.sort_by(|a, b| b.cmp(a));
            for (i, (used, n)) in variants.into_iter().enumerate() {
                if used < cutoff || i >= keep_variants {
                    stale.push(n.clone());
                }
            }
        }
        for n in &stale {
            if let Some(k) = self.slots.remove(n) {
                self.superseded.push(k);
            }
            self.used.remove(n);
            self.groups.remove(n);
            if let Some(k) = self.prev.remove(n) {
                self.superseded.push(k);
            }
        }
        self.prune();
        self.slots.keys().map(|n| n[..16].to_owned()).collect()
    }
}

/// Compressed incremental caches of swept variants, so one that comes back can resume.
pub const INCREMENTAL_ARCHIVE: &str = "incremental-archive";
const ARCHIVES_PER_CRATE: usize = 3;

/// Delete `target/rb` files no live slot owns. Incremental dirs are archived, not deleted.
fn sweep_target(layout: &Layout, live16: &HashSet<String>, archive_incremental: bool) -> (usize, u64) {
    fn name16_of(file_name: &str) -> Option<&str> {
        file_name.match_indices('-').find_map(|(i, _)| {
            let h = file_name.get(i + 1..i + 17)?;
            (h.bytes().all(|b| b.is_ascii_hexdigit()) && !file_name.as_bytes().get(i + 17).is_some_and(|b| b.is_ascii_hexdigit()))
                .then_some(h)
        })
    }
    fn size_of(path: &Path) -> u64 {
        if path.is_dir() {
            walkdir::WalkDir::new(path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        } else {
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        }
    }
    let (mut removed, mut bytes) = (0, 0);
    let mut to_archive: Vec<(PathBuf, PathBuf)> = Vec::new();
    let profiles = std::fs::read_dir(layout.rb_dir()).into_iter().flatten().filter_map(|e| e.ok());
    for profile in profiles.filter(|e| e.file_name() != ".state" && e.path().is_dir()) {
        for kind in std::fs::read_dir(profile.path()).into_iter().flatten().filter_map(|e| e.ok()) {
            let archive_dir = kind.path().join(INCREMENTAL_ARCHIVE);
            for sub in ["deps", "build", "incremental"] {
                let dir = kind.path().join(sub);
                for e in std::fs::read_dir(&dir).into_iter().flatten().filter_map(|e| e.ok()) {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let live = match name16_of(&name) {
                        Some(h) => live16.contains(h),
                        None => sub != "incremental",
                    };
                    if live {
                        continue;
                    }
                    bytes += size_of(&e.path());
                    removed += 1;
                    if archive_incremental && sub == "incremental" && name16_of(&name).is_some() && e.path().is_dir() {
                        let pending = archive_dir.join(format!("{name}.pending"));
                        let _ = std::fs::create_dir_all(&archive_dir);
                        if std::fs::rename(e.path(), &pending).is_ok() {
                            to_archive.push((pending, archive_dir.join(format!("{name}.tar.zst"))));
                        }
                        continue;
                    }
                    let _ = if e.path().is_dir() {
                        std::fs::remove_dir_all(e.path())
                    } else {
                        std::fs::remove_file(e.path())
                    };
                }
            }
        }
    }
    // Packed later by `rb __compact`. This build just moves the dirs aside.
    let _ = &to_archive;
    for archive_dir in to_archive.iter().filter_map(|(_, a)| a.parent()).collect::<HashSet<_>>() {
        let mut by_crate: HashMap<String, Vec<(std::time::SystemTime, PathBuf)>> = HashMap::new();
        for e in std::fs::read_dir(archive_dir).into_iter().flatten().filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(h) = name16_of(&name) else { continue };
            if live16.contains(h) {
                let restored = archive_dir.with_file_name("incremental").join(name.trim_end_matches(".tar.zst"));
                if restored.is_dir() {
                    let _ = std::fs::remove_file(e.path());
                }
                continue;
            }
            let krate = name.split_at(name.find(h).unwrap_or(0).saturating_sub(1)).0.to_owned();
            let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            by_crate.entry(krate).or_default().push((mtime, e.path()));
        }
        for mut archives in by_crate.into_values() {
            archives.sort_by_key(|a| Reverse(a.0));
            for (_, path) in archives.into_iter().skip(ARCHIVES_PER_CRATE) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    (removed, bytes)
}

fn load_state(path: &Path) -> ProjectState {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, st: &ProjectState) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(st)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn release_semver(release: &str) -> Option<semver::Version> {
    semver::Version::parse(release.split('-').next()?).ok()
}

fn cargo_download_cache(cfg: &RbConfig) -> Option<PathBuf> {
    if !cfg.reuse_cargo_downloads {
        return None;
    }
    Some(crate::config::cargo_home().join("registry").join("cache")).filter(|p| p.is_dir())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Fresh,
    Cached,
    Miss,
}

struct Phases<'a> {
    shell: &'a Shell,
    last: std::cell::Cell<Instant>,
}

impl Phases<'_> {
    fn mark(&self, name: &str) {
        if self.shell.verbosity() >= crate::shell::Verbosity::VeryVerbose {
            let now = Instant::now();
            self.shell
                .note(format!("phase {name}: {:.1} ms", (now - self.last.get()).as_secs_f64() * 1000.0));
            self.last.set(now);
        }
    }
}

pub struct UpdateOptions {
    pub specs: Vec<String>,
    pub precise: Option<String>,
    pub recursive: bool,
    pub dry_run: bool,
    pub manifest_path: Option<PathBuf>,
    pub locked: bool,
    pub offline: bool,
    pub ignore_rust_version: bool,
}

pub struct AddOptions {
    pub deps: Vec<String>,
    pub path: Option<PathBuf>,
    pub git: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub optional: bool,
    pub dev: bool,
    pub build: bool,
    pub rename: Option<String>,
    pub dry_run: bool,
    pub package: Option<String>,
    pub manifest_path: Option<PathBuf>,
    pub locked: bool,
    pub offline: bool,
    pub ignore_rust_version: bool,
}

pub fn add(opts: &AddOptions, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    if opts.dev && opts.build {
        bail!("cannot specify both --dev and --build");
    }
    if opts.deps.is_empty() && opts.path.is_none() && opts.git.is_none() {
        bail!("a dependency to add is required");
    }
    if opts.deps.len() > 1 && (opts.path.is_some() || opts.git.is_some()) {
        bail!("--path and --git apply to one dependency");
    }
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    let offline = opts.offline || cargo_cfg.offline()?;
    let env = Env {
        home: &cfg.home,
        rustc_version: release_semver(&rustc.release),
        registries: cargo_cfg.registries(),
        download_mirror: cargo_download_cache(cfg),
        shell,
    };
    let load = LoadOptions {
        manifest_path: opts.manifest_path.as_deref(),
        locked: false,
        offline,
        ignore_rust_version: opts.ignore_rust_version,
        vendor: None,
    };
    let mut ws = Workspace::load(&load, &cwd, &env)?;
    ws.finalize();
    let manifest = if let Some(spec) = &opts.package {
        let ids = ws.select(std::slice::from_ref(spec), false, &[])?;
        ws.pkgs[ids[0]].manifest_path.clone()
    } else {
        let current = std::fs::canonicalize(&ws.current_manifest).unwrap_or_else(|_| ws.current_manifest.clone());
        let members: Vec<_> = ws.pkgs.iter().filter(|p| p.is_member).collect();
        members
            .iter()
            .copied()
            .find(|p| std::fs::canonicalize(&p.manifest_path).ok().as_ref() == Some(&current))
            .or_else(|| if members.len() == 1 { Some(members[0]) } else { None })
            .map(|p| p.manifest_path.clone())
            .context("could not determine which package to modify; use `-p <package>`")?
    };
    let section = if opts.dev {
        "dev-dependencies"
    } else if opts.build {
        "build-dependencies"
    } else {
        "dependencies"
    };
    let (key, package, version) = if let Some(path) = &opts.path {
        let manifest_path = if path.join("Cargo.toml").is_file() {
            path.join("Cargo.toml")
        } else {
            path.clone()
        };
        let table = crate::manifest::read_toml(&manifest_path)?;
        let name = table
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .with_context(|| format!("{} has no package name", manifest_path.display()))?
            .to_owned();
        (opts.rename.clone().unwrap_or_else(|| name.clone()), name, None)
    } else if opts.git.is_some() {
        let dep = opts.deps.first().context("--git requires a package name")?;
        let (name, version) = split_dep(dep);
        (opts.rename.clone().unwrap_or_else(|| name.clone()), name, version)
    } else {
        let dep = opts.deps.first().context("a dependency to add is required")?;
        let (name, version) = split_dep(dep);
        let version = match version {
            Some(v) => Some(v),
            None => Some(ws.crates_io_req(&name)?),
        };
        (opts.rename.clone().unwrap_or_else(|| name.clone()), name, version)
    };
    let line = dep_line(opts, &key, &package, version.as_deref(), manifest.parent().unwrap());
    let old = std::fs::read_to_string(&manifest).with_context(|| format!("failed to read {}", manifest.display()))?;
    let new = crate::manifest::upsert_dependency(&old, section, &key, &line);
    shell.status("Adding", format!("{key} to {section}"));
    if opts.dry_run {
        return Ok(());
    }
    std::fs::write(&manifest, &new).with_context(|| format!("failed to write {}", manifest.display()))?;
    let locked = LoadOptions {
        locked: opts.locked,
        ..load
    };
    if let Err(err) = Workspace::load(&locked, &cwd, &env) {
        let _ = std::fs::write(&manifest, old);
        return Err(err);
    }
    Ok(())
}

fn split_dep(dep: &str) -> (String, Option<String>) {
    match dep.split_once('@') {
        Some((name, req)) => (name.to_owned(), Some(req.to_owned())),
        None => (dep.to_owned(), None),
    }
}

fn dep_line(opts: &AddOptions, key: &str, package: &str, version: Option<&str>, manifest_dir: &Path) -> String {
    let mut fields = Vec::new();
    if key != package {
        fields.push(format!("package = \"{package}\""));
    }
    if let Some(version) = version {
        fields.push(format!("version = \"{version}\""));
    }
    if let Some(path) = &opts.path {
        let dir = path.parent().filter(|_| path.ends_with("Cargo.toml")).unwrap_or(path);
        fields.push(format!("path = \"{}\"", relative_path(manifest_dir, dir)));
    }
    if let Some(git) = &opts.git {
        fields.push(format!("git = \"{git}\""));
    }
    if let Some(branch) = &opts.branch {
        fields.push(format!("branch = \"{branch}\""));
    }
    if let Some(tag) = &opts.tag {
        fields.push(format!("tag = \"{tag}\""));
    }
    if let Some(rev) = &opts.rev {
        fields.push(format!("rev = \"{rev}\""));
    }
    let features: Vec<String> = opts
        .features
        .iter()
        .flat_map(|s| s.split([',', ' ']))
        .filter(|s| !s.is_empty())
        .map(|s| format!("\"{s}\""))
        .collect();
    if !features.is_empty() {
        fields.push(format!("features = [{}]", features.join(", ")));
    }
    if opts.no_default_features {
        fields.push("default-features = false".into());
    }
    if opts.optional {
        fields.push("optional = true".into());
    }
    if fields.len() == 1
        && key == package
        && let Some(version) = version
    {
        format!("{key} = \"{version}\"")
    } else {
        format!("{key} = {{ {} }}", fields.join(", "))
    }
}

fn relative_path(from_dir: &Path, to: &Path) -> String {
    if to.is_relative() {
        return to.display().to_string();
    }
    let from = std::fs::canonicalize(from_dir).unwrap_or_else(|_| from_dir.to_owned());
    let to = std::fs::canonicalize(to).unwrap_or_else(|_| to.to_owned());
    let mut common = 0;
    for (a, b) in from.components().zip(to.components()) {
        if a == b {
            common += 1;
        } else {
            break;
        }
    }
    let mut out = PathBuf::new();
    for _ in 0..from.components().count().saturating_sub(common) {
        out.push("..");
    }
    for c in to.components().skip(common) {
        out.push(c.as_os_str());
    }
    out.display().to_string()
}

pub struct RemoveOptions {
    pub deps: Vec<String>,
    pub dev: bool,
    pub build: bool,
    pub dry_run: bool,
    pub package: Option<String>,
    pub manifest_path: Option<PathBuf>,
    pub locked: bool,
    pub offline: bool,
}

pub fn remove(opts: &RemoveOptions, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    if opts.dev && opts.build {
        bail!("cannot specify both --dev and --build");
    }
    let section = if opts.dev {
        "dev-dependencies"
    } else if opts.build {
        "build-dependencies"
    } else {
        "dependencies"
    };
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    let offline = opts.offline || cargo_cfg.offline()?;
    let env = Env {
        home: &cfg.home,
        rustc_version: release_semver(&rustc.release),
        registries: cargo_cfg.registries(),
        download_mirror: cargo_download_cache(cfg),
        shell,
    };
    let load = LoadOptions {
        manifest_path: opts.manifest_path.as_deref(),
        locked: false,
        offline,
        ignore_rust_version: false,
        vendor: None,
    };
    let mut ws = Workspace::load(&load, &cwd, &env)?;
    ws.finalize();
    let manifest = if let Some(spec) = &opts.package {
        let ids = ws.select(std::slice::from_ref(spec), false, &[])?;
        ws.pkgs[ids[0]].manifest_path.clone()
    } else {
        let current = std::fs::canonicalize(&ws.current_manifest).unwrap_or_else(|_| ws.current_manifest.clone());
        let members: Vec<_> = ws.pkgs.iter().filter(|p| p.is_member).collect();
        members
            .iter()
            .copied()
            .find(|p| std::fs::canonicalize(&p.manifest_path).ok().as_ref() == Some(&current))
            .or_else(|| if members.len() == 1 { Some(members[0]) } else { None })
            .map(|p| p.manifest_path.clone())
            .context("could not determine which package to modify; use `-p <package>`")?
    };
    let old = std::fs::read_to_string(&manifest).with_context(|| format!("failed to read {}", manifest.display()))?;
    let mut text = old.clone();
    for dep in &opts.deps {
        text = crate::manifest::remove_dependency(&text, section, dep)
            .with_context(|| format!("no dependency named `{dep}` to remove from {section}"))?;
        shell.status("Removing", format!("{dep} from {section}"));
    }
    if opts.dry_run {
        return Ok(());
    }
    std::fs::write(&manifest, &text).with_context(|| format!("failed to write {}", manifest.display()))?;
    let locked = LoadOptions {
        locked: opts.locked,
        ..load
    };
    if let Err(err) = Workspace::load(&locked, &cwd, &env) {
        let _ = std::fs::write(&manifest, old);
        return Err(err);
    }
    Ok(())
}

pub struct InstallOptions {
    pub name: Option<String>,
    pub version: Option<String>,
    pub path: Option<PathBuf>,
    pub git: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
    pub bin: Vec<String>,
    pub force: bool,
    pub root: Option<PathBuf>,
    pub list: bool,
    pub manifest_path: Option<PathBuf>,
    pub locked: bool,
    pub offline: bool,
    pub jobs: Option<usize>,
}

/// Release build, binaries copied to `root/bin`. Defaults to `~/.cargo`, same PATH as cargo.
pub fn install(opts: &InstallOptions, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    let root = opts.root.clone().unwrap_or_else(cargo_home);
    if opts.list {
        return list_installed(&root, shell);
    }
    if opts.path.is_some() && (opts.name.is_some() || opts.git.is_some()) {
        bail!("--path cannot be combined with a crate name or --git");
    }
    let manifest = if let Some(url) = &opts.git {
        let reference = match (&opts.branch, &opts.tag, &opts.rev) {
            (Some(b), None, None) => crate::manifest::GitRef::Branch(b.clone()),
            (None, Some(t), None) => crate::manifest::GitRef::Tag(t.clone()),
            (None, None, Some(r)) => crate::manifest::GitRef::Rev(r.clone()),
            (None, None, None) => crate::manifest::GitRef::DefaultBranch,
            _ => bail!("--branch, --tag, and --rev cannot be combined"),
        };
        let git = crate::git::Git::new(cfg.home.join("git"), opts.offline);
        shell.status("Updating", format!("git repository `{url}`"));
        let commit = git.resolve(url, &reference)?;
        let checkout = git.checkout(url, &commit)?;
        if let Some(name) = &opts.name {
            crate::git::find_package(&checkout, name)?
        } else {
            let root_manifest = checkout.join("Cargo.toml");
            let table = crate::manifest::read_toml(&root_manifest).ok();
            if table.as_ref().and_then(|t| t.get("package")).is_some() {
                root_manifest
            } else {
                bail!("the git repository has no root package; pass the package name");
            }
        }
    } else if let Some(name) = &opts.name {
        let registry = crate::registry::Registry::new(cfg.home.join("registry"), opts.offline, cargo_download_cache(cfg));
        let entries = registry.entries(crate::manifest::CRATES_IO, name)?;
        let chosen = crate::registry::select_version(&entries, opts.version.as_deref())?;
        shell.status("Downloading", format!("{name} v{}", chosen.vers));
        let (dir, _) = registry.ensure(&crate::registry::Download {
            source: crate::manifest::CRATES_IO,
            name,
            version: &chosen.vers,
            checksum: Some(&chosen.cksum),
        })?;
        dir.join("Cargo.toml")
    } else {
        match &opts.manifest_path {
            Some(p) => p.clone(),
            None => {
                let dir = opts.path.clone().unwrap_or(std::env::current_dir()?);
                if dir.ends_with("Cargo.toml") { dir } else { dir.join("Cargo.toml") }
            }
        }
    };
    if !manifest.is_file() {
        bail!("manifest path `{}` does not exist", manifest.display());
    }
    let target = cfg.home.join("install-target");
    let mut build = BuildOptions {
        command: Command::Build,
        release: true,
        manifest_path: Some(manifest.clone()),
        target_dir: Some(target.clone()),
        locked: opts.locked,
        offline: opts.offline,
        jobs: opts.jobs,
        filter: crate::unit::TargetFilter {
            bins: opts.bin.is_empty(),
            bin_names: opts.bin.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    if opts.offline {
        build.offline = true;
    }
    run(&build, cfg, shell)?;
    let release = target.join("release");
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let mut copied = Vec::new();
    for entry in std::fs::read_dir(&release).with_context(|| format!("failed to read {}", release.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains('.') && !name.ends_with(".exe") {
            continue;
        }
        let stem = name.trim_end_matches(".exe");
        if !opts.bin.is_empty() && !opts.bin.iter().any(|b| b == stem) {
            continue;
        }
        let dest = bin_dir.join(&name);
        if dest.exists() && !opts.force {
            bail!(
                "binary `{}` already exists in {}; pass --force to overwrite",
                name,
                bin_dir.display()
            );
        }
        std::fs::copy(&path, &dest).with_context(|| format!("failed to install {}", dest.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&dest)?.permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&dest, perm)?;
        }
        shell.status("Installed", format!("{} to {}", stem, dest.display()));
        copied.push(stem.to_owned());
    }
    if copied.is_empty() {
        bail!("no binaries to install");
    }
    record_install(&root, &manifest, &copied)?;
    Ok(())
}

fn cargo_home() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO_HOME") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".cargo")
}

fn installs_path(root: &Path) -> PathBuf {
    root.join(".rb-install.json")
}

fn record_install(root: &Path, manifest: &Path, bins: &[String]) -> Result<()> {
    let table = crate::manifest::read_toml(manifest)?;
    let pkg = table.get("package");
    let name = pkg.and_then(|p| p.get("name")).and_then(|n| n.as_str()).unwrap_or("").to_owned();
    let version = pkg.and_then(|p| p.get("version")).and_then(|n| n.as_str()).unwrap_or("").to_owned();
    let path = installs_path(root);
    let mut list: Vec<serde_json::Value> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    list.retain(|v| v["name"] != name);
    list.push(serde_json::json!({ "name": name, "version": version, "bins": bins }));
    std::fs::write(path, serde_json::to_vec_pretty(&list)?)?;
    Ok(())
}

fn list_installed(root: &Path, shell: &Shell) -> Result<()> {
    let path = installs_path(root);
    let list: Vec<serde_json::Value> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if list.is_empty() {
        shell.status("Installed", "nothing");
        return Ok(());
    }
    for v in list {
        shell.status(
            "Installed",
            format!(
                "{} v{} ({})",
                v["name"].as_str().unwrap_or(""),
                v["version"].as_str().unwrap_or(""),
                v["bins"]
            ),
        );
    }
    Ok(())
}

pub fn uninstall(name: &str, root: Option<&Path>, shell: &Shell) -> Result<()> {
    let root = root.map(Path::to_path_buf).unwrap_or_else(cargo_home);
    let path = installs_path(&root);
    let mut list: Vec<serde_json::Value> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let Some(pos) = list.iter().position(|v| v["name"] == name) else {
        bail!("package `{name}` is not installed");
    };
    let entry = list.remove(pos);
    let bins = entry["bins"].as_array().cloned().unwrap_or_default();
    for bin in &bins {
        let Some(bin) = bin.as_str() else { continue };
        let dest = root.join("bin").join(bin);
        if dest.is_file() {
            std::fs::remove_file(&dest).with_context(|| format!("failed to remove {}", dest.display()))?;
            shell.status("Removing", dest.display().to_string());
        }
    }
    if list.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        std::fs::write(&path, serde_json::to_vec_pretty(&list)?)?;
    }
    Ok(())
}

pub fn update(opts: &UpdateOptions, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    let offline = opts.offline || cargo_cfg.offline()?;
    let mut ws = Workspace::load(
        &LoadOptions {
            manifest_path: opts.manifest_path.as_deref(),
            locked: false,
            offline,
            ignore_rust_version: opts.ignore_rust_version,
            vendor: None,
        },
        &cwd,
        &Env {
            home: &cfg.home,
            rustc_version: release_semver(&rustc.release),
            registries: cargo_cfg.registries(),
            download_mirror: cargo_download_cache(cfg),
            shell,
        },
    )?;
    ws.set_locked(opts.locked);
    ws.update(&opts.specs, opts.precise.as_deref(), opts.recursive, opts.dry_run, shell)
}

pub struct MetadataOptions {
    pub format_version: u32,
    pub no_deps: bool,
    pub manifest_path: Option<PathBuf>,
    pub locked: bool,
    pub offline: bool,
}

fn tree_label(ws: &Workspace, n: usize) -> String {
    let id = &ws.graph.nodes[n].id;
    match &id.source {
        crate::resolve::SourceId::Path(p) => format!("{} v{} ({})", id.name, id.version, p.display()),
        _ => format!("{} v{}", id.name, id.version),
    }
}

fn tree_lib(ws: &Workspace, n: usize) -> String {
    let Some(pkg) = &ws.graph.nodes[n].package else {
        return String::new();
    };
    pkg.targets
        .iter()
        .find(|t| t.kind == TargetKind::Lib)
        .map(|t| t.crate_name())
        .unwrap_or_default()
}

fn tree_meta(ws: &Workspace, n: usize, license: bool) -> String {
    let Some(pkg) = &ws.graph.nodes[n].package else {
        return String::new();
    };
    if license { pkg.license.clone() } else { pkg.repository.clone() }.unwrap_or_default()
}

fn validate_tree_format(format: &str) -> Result<()> {
    let mut rest = format;
    while let Some(start) = rest.find('{') {
        let Some(rel) = rest[start + 1..].find('}') else {
            bail!("tree format `{format}` not valid");
        };
        let key = &rest[start + 1..start + 1 + rel];
        if !matches!(key, "p" | "lib" | "l" | "r" | "f") {
            bail!("tree format `{format}` not valid\n\nCaused by:\n  unsupported pattern `{key}`");
        }
        rest = &rest[start + 1 + rel + 1..];
    }
    Ok(())
}

fn format_node(ws: &Workspace, n: usize, format: &str, feats: &HashMap<usize, String>) -> String {
    let mut out = String::new();
    let mut rest = format;
    while let Some(start) = rest.find('{') {
        let rel = rest[start + 1..].find('}').expect("format validated");
        out.push_str(&rest[..start]);
        let key = &rest[start + 1..start + 1 + rel];
        let value = match key {
            "p" => tree_label(ws, n),
            "lib" => tree_lib(ws, n),
            "l" => tree_meta(ws, n, true),
            "r" => tree_meta(ws, n, false),
            "f" => feats.get(&n).cloned().unwrap_or_default(),
            _ => unreachable!("format validated"),
        };
        out.push_str(&value);
        rest = &rest[start + 1 + rel + 1..];
    }
    out.push_str(rest);
    out
}

/// What cargo prints for `{f}`: target features, or host features if that's all there is.
fn tree_features(ws: &mut Workspace, rustc: &Rustc, roots: &[usize], shell: &Shell) -> Result<HashMap<usize, String>> {
    let info = rustc.target_info(None, &[])?;
    let platforms = Platforms {
        host: PlatformCfg {
            triple: &rustc.host,
            cfg: &info.cfg,
        },
        targets: vec![PlatformCfg {
            triple: &rustc.host,
            cfg: &info.cfg,
        }],
    };
    let res = ws.resolve_features(
        roots,
        &FeatureRequest {
            features: &[],
            all_features: false,
            no_default_features: false,
        },
        &platforms,
        &HashSet::new(),
        shell,
    )?;
    let mut map = HashMap::new();
    for n in 0..ws.graph.nodes.len() {
        let Some(fi) = res.info(n, false).or_else(|| res.info(n, true)) else {
            continue;
        };
        map.insert(n, fi.named.iter().cloned().collect::<Vec<_>>().join(","));
    }
    Ok(map)
}

#[allow(clippy::too_many_arguments)]
pub fn tree(
    spec: Option<&str>,
    depth: Option<usize>,
    edges: &str,
    invert: Option<&str>,
    prefix_mode: &str,
    format: &str,
    ascii: bool,
    manifest_path: Option<&Path>,
    locked: bool,
    offline: bool,
    cfg: &RbConfig,
    shell: &Shell,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    let mut ws = Workspace::load(
        &LoadOptions {
            manifest_path,
            locked,
            offline: offline || cargo_cfg.offline()?,
            ignore_rust_version: false,
            vendor: None,
        },
        &cwd,
        &Env {
            home: &cfg.home,
            rustc_version: release_semver(&rustc.release),
            registries: cargo_cfg.registries(),
            download_mirror: cargo_download_cache(cfg),
            shell,
        },
    )?;
    let specs: Vec<String> = spec.map(str::to_owned).into_iter().collect();
    let ids = if let Some(name) = invert {
        let found: Vec<usize> = ws
            .graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.id.name == name)
            .map(|(i, _)| i)
            .collect();
        if found.is_empty() {
            bail!("package `{name}` is not in the dependency graph");
        }
        found
    } else {
        let ids = ws.select(&specs, false, &[])?;
        if ids.len() != 1 {
            bail!("could not determine which package to show; use `-p <package>`");
        }
        ids
    };
    let kinds: Vec<crate::manifest::DepKind> = match edges {
        "normal" => vec![crate::manifest::DepKind::Normal],
        "dev" => vec![crate::manifest::DepKind::Dev],
        "build" => vec![crate::manifest::DepKind::Build],
        "all" => vec![
            crate::manifest::DepKind::Normal,
            crate::manifest::DepKind::Dev,
            crate::manifest::DepKind::Build,
        ],
        other => bail!("unknown --edges `{other}`"),
    };
    if !matches!(prefix_mode, "indent" | "depth" | "none") {
        bail!("unknown --prefix `{prefix_mode}`");
    }
    validate_tree_format(format)?;
    let feats = if format.contains("{f}") {
        let roots = if invert.is_some() {
            ws.default_members.clone()
        } else {
            ids.clone()
        };
        tree_features(&mut ws, &rustc, &roots, shell)?
    } else {
        HashMap::new()
    };
    let (tee, elbow, bar, space) = if ascii {
        ("|-- ", "`-- ", "|   ", "    ")
    } else {
        ("├── ", "└── ", "│   ", "    ")
    };
    let mut seen = HashSet::new();
    fn kids(ws: &Workspace, n: usize, kinds: &[crate::manifest::DepKind]) -> Vec<usize> {
        let node = &ws.graph.nodes[n];
        let decls: Vec<&crate::manifest::DepDecl> = node.deps().iter().filter(|d| kinds.contains(&d.kind)).collect();
        if decls.is_empty() && node.package.is_none() && node.summary.is_none() {
            return node.edges.clone();
        }
        let mut out = Vec::new();
        for d in decls {
            let hit = node
                .edges
                .iter()
                .copied()
                .filter(|&e| ws.graph.nodes[e].id.name == d.name && d.req.matches(&ws.graph.nodes[e].id.version))
                .max_by(|a, b| ws.graph.nodes[*a].id.version.cmp(&ws.graph.nodes[*b].id.version));
            if let Some(e) = hit {
                out.push(e);
            }
        }
        out
    }
    fn parents(ws: &Workspace, child: usize, kinds: &[crate::manifest::DepKind]) -> Vec<usize> {
        (0..ws.graph.nodes.len()).filter(|&i| kids(ws, i, kinds).contains(&child)).collect()
    }
    #[allow(clippy::too_many_arguments)]
    fn walk(
        ws: &Workspace,
        n: usize,
        prefix: &str,
        depth: usize,
        max_depth: Option<usize>,
        kinds: &[crate::manifest::DepKind],
        inverted: bool,
        mode: &str,
        format: &str,
        feats: &HashMap<usize, String>,
        tee: &str,
        elbow: &str,
        bar: &str,
        space: &str,
        seen: &mut HashSet<usize>,
    ) {
        let children = if max_depth.is_some_and(|m| depth >= m) {
            Vec::new()
        } else if inverted {
            parents(ws, n, kinds)
        } else {
            kids(ws, n, kinds)
        };
        for (i, child) in children.iter().copied().enumerate() {
            let last = i + 1 == children.len();
            let repeated = !seen.insert(child);
            let text = format!("{}{}", format_node(ws, child, format, feats), if repeated { " (*)" } else { "" });
            match mode {
                "none" => println!("{text}"),
                "depth" => println!("{depth}{text}"),
                _ => {
                    let branch = if last { elbow } else { tee };
                    println!("{prefix}{branch}{text}");
                }
            }
            if !repeated {
                let next = format!("{prefix}{}", if last { space } else { bar });
                walk(
                    ws,
                    child,
                    &next,
                    depth + 1,
                    max_depth,
                    kinds,
                    inverted,
                    mode,
                    format,
                    feats,
                    tee,
                    elbow,
                    bar,
                    space,
                    seen,
                );
            }
        }
    }
    let inverted = invert.is_some();
    for (i, id) in ids.iter().copied().enumerate() {
        if i > 0 {
            println!();
        }
        let root_label = format_node(&ws, id, format, &feats);
        match prefix_mode {
            "depth" => println!("0{root_label}"),
            _ => println!("{root_label}"),
        }
        seen.insert(id);
        walk(
            &ws,
            id,
            "",
            1,
            depth,
            &kinds,
            inverted,
            prefix_mode,
            format,
            &feats,
            tee,
            elbow,
            bar,
            space,
            &mut seen,
        );
    }
    Ok(())
}

pub fn pkgid(spec: Option<&str>, manifest_path: Option<&Path>, locked: bool, offline: bool, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    let ws = Workspace::load(
        &LoadOptions {
            manifest_path,
            locked,
            offline: offline || cargo_cfg.offline()?,
            ignore_rust_version: false,
            vendor: None,
        },
        &cwd,
        &Env {
            home: &cfg.home,
            rustc_version: release_semver(&rustc.release),
            registries: cargo_cfg.registries(),
            download_mirror: cargo_download_cache(cfg),
            shell,
        },
    )?;
    let specs: Vec<String> = spec.map(str::to_owned).into_iter().collect();
    let ids = ws.select(&specs, false, &[])?;
    if ids.len() != 1 {
        bail!("could not determine which package to print; use `-p <package>`");
    }
    println!("{}", ws.graph.nodes[ids[0]].id.spec());
    Ok(())
}

pub fn vendor(
    dir: Option<&Path>,
    no_delete: bool,
    manifest_path: Option<&Path>,
    locked: bool,
    offline: bool,
    cfg: &RbConfig,
    shell: &Shell,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    let mut ws = Workspace::load(
        &LoadOptions {
            manifest_path,
            locked,
            offline: offline || cargo_cfg.offline()?,
            ignore_rust_version: false,
            vendor: None,
        },
        &cwd,
        &Env {
            home: &cfg.home,
            rustc_version: release_semver(&rustc.release),
            registries: cargo_cfg.registries(),
            download_mirror: cargo_download_cache(cfg),
            shell,
        },
    )?;
    ws.fetch(shell)?;
    let dest = dir.map(Path::to_path_buf).unwrap_or_else(|| cwd.join("vendor"));
    if dest.exists() && !no_delete {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::create_dir_all(&dest)?;
    for node in &ws.graph.nodes {
        let remote = !matches!(node.id.source, crate::resolve::SourceId::Path(_));
        let Some(pkg) = node.package.as_ref().filter(|_| remote) else {
            continue;
        };
        let crate_dir = dest.join(format!("{}-{}", node.id.name, node.id.version));
        copy_vendor(&pkg.root, &crate_dir)?;
        let mut files = serde_json::Map::new();
        for file in walk_files(&crate_dir) {
            let rel = file.strip_prefix(&crate_dir).unwrap().to_string_lossy().replace('\\', "/");
            if rel == ".cargo-checksum.json" {
                continue;
            }
            files.insert(rel, serde_json::Value::String(rb_toolchain::download::sha256_file(&file)?));
        }
        let checksum = serde_json::json!({
            "files": files,
            "package": node.checksum.clone().unwrap_or_default(),
        });
        std::fs::write(crate_dir.join(".cargo-checksum.json"), serde_json::to_vec_pretty(&checksum)?)?;
        shell.status("Vendored", format!("{} v{}", node.id.name, node.id.version));
    }
    println!(
        "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"{}\"\n",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("vendor")
    );
    Ok(())
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&current) else { continue };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name != "target" && !name.to_string_lossy().starts_with('.') {
                    stack.push(path);
                }
            } else if name != ".rb-ok" {
                out.push(path);
            }
        }
    }
    out
}

fn copy_vendor(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for file in walk_files(from) {
        let rel = file.strip_prefix(from).unwrap();
        let dest = to.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&file, &dest).with_context(|| format!("failed to copy {}", file.display()))?;
    }
    Ok(())
}

pub fn fetch(manifest_path: Option<&Path>, locked: bool, offline: bool, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    let mut ws = Workspace::load(
        &LoadOptions {
            manifest_path,
            locked,
            offline: offline || cargo_cfg.offline()?,
            ignore_rust_version: false,
            vendor: None,
        },
        &cwd,
        &Env {
            home: &cfg.home,
            rustc_version: release_semver(&rustc.release),
            registries: cargo_cfg.registries(),
            download_mirror: cargo_download_cache(cfg),
            shell,
        },
    )?;
    let n = ws.fetch(shell)?;
    shell.status("Fetched", format!("{n} package{}", if n == 1 { "" } else { "s" }));
    Ok(())
}

pub fn read_manifest(manifest_path: Option<&Path>, locked: bool, offline: bool, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    let mut ws = Workspace::load(
        &LoadOptions {
            manifest_path,
            locked,
            offline: offline || cargo_cfg.offline()?,
            ignore_rust_version: false,
            vendor: None,
        },
        &cwd,
        &Env {
            home: &cfg.home,
            rustc_version: release_semver(&rustc.release),
            registries: cargo_cfg.registries(),
            download_mirror: cargo_download_cache(cfg),
            shell,
        },
    )?;
    ws.finalize();
    let current = std::fs::canonicalize(&ws.current_manifest).unwrap_or_else(|_| ws.current_manifest.clone());
    let members: Vec<_> = ws.pkgs.iter().filter(|p| p.is_member).collect();
    let pkg = members
        .iter()
        .copied()
        .find(|p| std::fs::canonicalize(&p.manifest_path).ok().as_ref() == Some(&current))
        .or_else(|| if members.len() == 1 { Some(members[0]) } else { None })
        .context("could not determine which package to print; use --manifest-path")?;
    let node = ws
        .graph
        .nodes
        .iter()
        .position(|n| n.id.name == pkg.name && n.id.version == pkg.version)
        .context("package is not in the graph")?;
    println!("{}", serde_json::to_string(&metadata_package(&ws, node))?);
    Ok(())
}

pub fn metadata(opts: &MetadataOptions, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    if opts.format_version != 1 {
        bail!(
            "metadata format version {} is unsupported; pass --format-version 1",
            opts.format_version
        );
    }
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    let mut ws = Workspace::load(
        &LoadOptions {
            manifest_path: opts.manifest_path.as_deref(),
            locked: opts.locked,
            offline: opts.offline || cargo_cfg.offline()?,
            ignore_rust_version: false,
            vendor: None,
        },
        &cwd,
        &Env {
            home: &cfg.home,
            rustc_version: release_semver(&rustc.release),
            registries: cargo_cfg.registries(),
            download_mirror: cargo_download_cache(cfg),
            shell,
        },
    )?;
    let members: Vec<usize> = ws.members().collect();
    let feats = if opts.no_deps {
        HashMap::new()
    } else {
        tree_features(&mut ws, &rustc, &members, shell)?
    };
    let nodes: Vec<usize> = if opts.no_deps {
        members.clone()
    } else {
        (0..ws.graph.nodes.len()).collect()
    };
    let packages: Vec<serde_json::Value> = nodes.iter().copied().map(|n| metadata_package(&ws, n)).collect();
    let member_ids: Vec<String> = members.iter().map(|&n| ws.graph.nodes[n].id.spec()).collect();
    let default_ids: Vec<String> = if ws.default_members.is_empty() {
        member_ids.clone()
    } else {
        ws.default_members.iter().map(|&n| ws.graph.nodes[n].id.spec()).collect()
    };
    let resolve = if opts.no_deps {
        serde_json::Value::Null
    } else {
        let resolve_nodes: Vec<serde_json::Value> = (0..ws.graph.nodes.len())
            .map(|n| {
                let node = &ws.graph.nodes[n];
                let deps: Vec<String> = node.edges.iter().map(|&d| ws.graph.nodes[d].id.spec()).collect();
                let mut dep_objs: Vec<serde_json::Value> = node
                    .deps()
                    .iter()
                    .filter_map(|d| {
                        let edge = node
                            .edges
                            .iter()
                            .copied()
                            .find(|&e| ws.graph.nodes[e].id.name == d.name && d.req.matches(&ws.graph.nodes[e].id.version))?;
                        let kind = match d.kind {
                            crate::manifest::DepKind::Normal => serde_json::Value::Null,
                            crate::manifest::DepKind::Dev => serde_json::json!("dev"),
                            crate::manifest::DepKind::Build => serde_json::json!("build"),
                        };
                        Some(serde_json::json!({
                            "name": d.dep_name(),
                            "pkg": ws.graph.nodes[edge].id.spec(),
                            "dep_kinds": [{ "kind": kind, "target": null }],
                        }))
                    })
                    .collect();
                if dep_objs.is_empty() {
                    dep_objs = node
                        .edges
                        .iter()
                        .map(|&e| {
                            serde_json::json!({
                                "name": ws.graph.nodes[e].id.name,
                                "pkg": ws.graph.nodes[e].id.spec(),
                                "dep_kinds": [{ "kind": null, "target": null }],
                            })
                        })
                        .collect();
                }
                let features: Vec<&str> = feats
                    .get(&n)
                    .map(|s| if s.is_empty() { Vec::new() } else { s.split(',').collect() })
                    .unwrap_or_default();
                serde_json::json!({ "id": node.id.spec(), "dependencies": deps, "deps": dep_objs, "features": features })
            })
            .collect();
        serde_json::json!({ "nodes": resolve_nodes, "root": member_ids.first().cloned() })
    };
    let doc = serde_json::json!({
        "packages": packages,
        "workspace_members": member_ids,
        "workspace_default_members": default_ids,
        "resolve": resolve,
        "target_directory": ws.target_dir,
        "build_directory": ws.target_dir,
        "version": 1,
        "workspace_root": ws.root,
        "metadata": null,
    });
    println!("{}", serde_json::to_string(&doc)?);
    Ok(())
}

fn metadata_package(ws: &Workspace, n: usize) -> serde_json::Value {
    let node = &ws.graph.nodes[n];
    let pkg = node.package.as_ref();
    let source = match &node.id.source {
        crate::resolve::SourceId::Path(_) => serde_json::Value::Null,
        other => serde_json::json!(other.lock_string()),
    };
    let targets: Vec<serde_json::Value> = pkg
        .map(|p| {
            p.targets
                .iter()
                .map(|t| {
                    let kind = if t.is_proc_macro() {
                        vec!["proc-macro".to_owned()]
                    } else {
                        vec![
                            match t.kind {
                                TargetKind::Lib => "lib",
                                TargetKind::Bin => "bin",
                                TargetKind::Example => "example",
                                TargetKind::Test => "test",
                                TargetKind::Bench => "bench",
                                TargetKind::BuildScript => "custom-build",
                            }
                            .to_owned(),
                        ]
                    };
                    let src = if t.src_path.is_absolute() {
                        t.src_path.clone()
                    } else {
                        p.root.join(&t.src_path)
                    };
                    serde_json::json!({
                        "kind": kind,
                        "crate_types": t.crate_types,
                        "name": t.name,
                        "src_path": src,
                        "edition": t.edition,
                        "doc": t.doc,
                        "doctest": t.doctest,
                        "test": t.test,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let dependencies: Vec<serde_json::Value> = pkg
        .map(|p| {
            p.deps
                .iter()
                .map(|d| {
                    let (source, path) = match &d.source {
                        crate::manifest::DepSource::Path(path) => (serde_json::Value::Null, serde_json::json!(path)),
                        crate::manifest::DepSource::Registry(s) => (serde_json::json!(s), serde_json::Value::Null),
                        crate::manifest::DepSource::Git { url, reference } => (
                            serde_json::json!(format!("git+{url}{}", reference.query())),
                            serde_json::Value::Null,
                        ),
                    };
                    let kind = match d.kind {
                        crate::manifest::DepKind::Normal => serde_json::Value::Null,
                        crate::manifest::DepKind::Dev => serde_json::json!("dev"),
                        crate::manifest::DepKind::Build => serde_json::json!("build"),
                    };
                    serde_json::json!({
                        "name": d.name,
                        "source": source,
                        "req": d.req.to_string(),
                        "kind": kind,
                        "rename": d.rename,
                        "optional": d.optional,
                        "uses_default_features": d.default_features,
                        "features": d.features,
                        "target": d.platform.as_ref().map(|p| match p {
                            crate::cfgexpr::PlatformExpr::Triple(t) => t.clone(),
                            crate::cfgexpr::PlatformExpr::Cfg(_) => "cfg(...)".to_owned(),
                        }),
                        "path": path,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let features = pkg.map(|p| serde_json::json!(p.features)).unwrap_or_else(|| serde_json::json!({}));
    serde_json::json!({
        "name": node.id.name,
        "version": node.id.version.to_string(),
        "id": node.id.spec(),
        "license": pkg.and_then(|p| p.license.clone()),
        "license_file": pkg.and_then(|p| p.license_file.clone()),
        "description": pkg.and_then(|p| p.description.clone()),
        "source": source,
        "dependencies": dependencies,
        "targets": targets,
        "features": features,
        "manifest_path": pkg.map(|p| p.manifest_path.clone()).unwrap_or_default(),
        "metadata": null,
        "publish": null,
        "authors": pkg.map(|p| p.authors.clone()).unwrap_or_default(),
        "categories": [],
        "keywords": [],
        "readme": pkg.and_then(|p| p.readme.clone()),
        "repository": pkg.and_then(|p| p.repository.clone()),
        "homepage": pkg.and_then(|p| p.homepage.clone()),
        "documentation": null,
        "edition": pkg.map(|p| p.edition.clone()).unwrap_or_default(),
        "links": pkg.and_then(|p| p.links.clone()),
        "default_run": pkg.and_then(|p| p.default_run.clone()),
        "rust_version": pkg.and_then(|p| p.rust_version.clone()),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn package(
    list: bool,
    no_verify: bool,
    allow_dirty: bool,
    manifest_path: Option<&Path>,
    locked: bool,
    offline: bool,
    cfg: &RbConfig,
    shell: &Shell,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    let mut ws = Workspace::load(
        &LoadOptions {
            manifest_path,
            locked,
            offline: offline || cargo_cfg.offline()?,
            ignore_rust_version: false,
            vendor: None,
        },
        &cwd,
        &Env {
            home: &cfg.home,
            rustc_version: release_semver(&rustc.release),
            registries: cargo_cfg.registries(),
            download_mirror: cargo_download_cache(cfg),
            shell,
        },
    )?;
    ws.finalize();
    let current = std::fs::canonicalize(&ws.current_manifest).unwrap_or_else(|_| ws.current_manifest.clone());
    let members: Vec<_> = ws.pkgs.iter().filter(|p| p.is_member).collect();
    let pkg = members
        .iter()
        .copied()
        .find(|p| std::fs::canonicalize(&p.manifest_path).ok().as_ref() == Some(&current))
        .or_else(|| if members.len() == 1 { Some(members[0]) } else { None })
        .context("could not determine which package to package; use --manifest-path")?;
    if !allow_dirty && git_dirty(&pkg.root) {
        bail!("the working directory is dirty; pass --allow-dirty to package anyway");
    }
    let manifest = crate::manifest::read_toml(&pkg.manifest_path)?;
    let patterns = |key: &str| -> Vec<String> {
        manifest
            .get("package")
            .and_then(|p| p.get(key))
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    let files = package_files(&pkg.root, &patterns("include"), &patterns("exclude"));
    if list {
        for file in &files {
            println!("{}", file.display());
        }
        return Ok(());
    }
    let out_dir = ws.target_dir.join("package");
    std::fs::create_dir_all(&out_dir)?;
    let crate_name = format!("{}-{}", pkg.name, pkg.version);
    let dest = out_dir.join(format!("{crate_name}.crate"));
    let encoder = flate2::write::GzEncoder::new(std::fs::File::create(&dest)?, flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    for file in &files {
        let mut header = tar::Header::new_gnu();
        header.set_size(file.metadata()?.len());
        header.set_mode(0o644);
        header.set_cksum();
        let rel = file.strip_prefix(&pkg.root).unwrap();
        let name = format!("{crate_name}/{}", rel.to_string_lossy().replace('\\', "/"));
        archive.append_data(&mut header, name, std::fs::File::open(file)?)?;
    }
    archive.finish()?;
    shell.status("Packaged", dest.display().to_string());
    if !no_verify {
        let verify = tempfile_dir()?;
        let decoder = flate2::read::GzDecoder::new(std::fs::File::open(&dest)?);
        tar::Archive::new(decoder).unpack(&verify)?;
        let mut opts = BuildOptions {
            command: Command::Build,
            manifest_path: Some(verify.join(&crate_name).join("Cargo.toml")),
            offline: true,
            locked: true,
            target_dir: Some(verify.join("target")),
            ..Default::default()
        };
        opts.offline = offline || cargo_cfg.offline()?;
        run(&opts, cfg, shell)?;
    }
    Ok(())
}

fn git_dirty(dir: &Path) -> bool {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output();
    match out {
        Ok(out) if out.status.success() => !out.stdout.is_empty(),
        _ => false,
    }
}

fn package_files(root: &Path, include: &[String], exclude: &[String]) -> Vec<PathBuf> {
    let matches = |rel: &str, patterns: &[String]| {
        patterns.iter().any(|p| {
            glob::Pattern::new(p)
                .ok()
                .is_some_and(|pat| pat.matches(rel) || pat.matches(rel.rsplit('/').next().unwrap_or(rel)))
        })
    };
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = entry.file_name();
            let label = name.to_string_lossy();
            if path.is_dir() {
                if label != "target" && label != ".git" {
                    stack.push(path);
                }
            } else {
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                let forced = rel == "Cargo.toml" || rel == "Cargo.lock";
                if !include.is_empty() && !forced && !matches(&rel, include) {
                    continue;
                }
                if !forced && matches(&rel, exclude) {
                    continue;
                }
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn tempfile_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("rb-package-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn scaffold(path: &Path, lib: bool, edition: Option<&str>, name: Option<&str>, vcs: Option<&str>, shell: &Shell) -> Result<()> {
    if path.join("Cargo.toml").is_file() {
        bail!("`{}` already contains a Cargo.toml", path.display());
    }
    let edition = edition.unwrap_or("2024");
    if !matches!(edition, "2015" | "2018" | "2021" | "2024") {
        bail!("invalid edition `{edition}`");
    }
    let name = name.map(str::to_owned).unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().replace('_', "-"))
            .unwrap_or_else(|| "app".into())
    });
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("invalid package name `{name}`");
    }
    std::fs::create_dir_all(path.join("src"))?;
    let manifest = format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n\n[dependencies]\n");
    std::fs::write(path.join("Cargo.toml"), manifest)?;
    if lib {
        std::fs::write(
            path.join("src/lib.rs"),
            "pub fn add(left: u64, right: u64) -> u64 {\n    left + right\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn it_works() {\n        let result = add(2, 2);\n        assert_eq!(result, 4);\n    }\n}\n",
        )?;
    } else {
        std::fs::write(path.join("src/main.rs"), "fn main() {\n    println!(\"Hello, world!\");\n}\n")?;
    }
    match vcs {
        Some("none") => {}
        Some("git") | None => {
            let out = std::process::Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(path)
                .output();
            if let Ok(out) = out
                && !out.status.success()
            {
                shell.warn(format!("git init failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
        }
        Some(other) => bail!("unsupported vcs `{other}`"),
    }
    shell.status("Created", format!("{} package `{name}`", if lib { "library" } else { "binary" }));
    Ok(())
}

/// Build `core` and `compiler_builtins` from rust-src. Returns the `--extern` args cargo would pass.
fn build_core_sysroot(opts: &BuildOptions, cfg: &RbConfig, shell: &Shell, rustc: &Rustc, target_dir: &Path, layout: &Layout) -> Result<Vec<String>> {
    if !rustc.nightly {
        bail!("`-Zbuild-std` requires a nightly compiler");
    }
    let manifest = rustc.sysroot.join("lib/rustlib/src/rust/library/compiler-builtins/compiler-builtins/Cargo.toml");
    if !manifest.is_file() {
        bail!("`-Zbuild-std` requires the `rust-src` component; run `rustup component add rust-src`");
    }
    let child = BuildOptions {
        manifest_path: Some(manifest),
        features: vec!["compiler-builtins".into()],
        target_dir: Some(target_dir.to_owned()),
        offline: opts.offline,
        jobs: opts.jobs,
        release: opts.release,
        profile: opts.profile.clone(),
        std_rustc: vec!["-Zforce-unstable-if-unmarked".into(), "--cap-lints".into(), "allow".into()],
        ..Default::default()
    };
    run(&child, cfg, shell)?;
    let deps = layout.deps("host");
    let rmeta = |prefix: &str| -> Result<PathBuf> {
        let mut hits: Vec<_> = std::fs::read_dir(&deps)
            .with_context(|| format!("failed to read {}", deps.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|x| x == "rmeta")
                    && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix))
            })
            .collect();
        hits.sort();
        hits.pop().with_context(|| format!("no {prefix} rmeta in {}", deps.display()))
    };
    let core = rmeta("libcore-")?;
    let builtins = rmeta("libcompiler_builtins-")?;
    Ok(vec![
        "-Zunstable-options".into(),
        "--extern".into(),
        format!("noprelude,nounused:compiler_builtins={}", builtins.display()),
        "--extern".into(),
        format!("noprelude,nounused:core={}", core.display()),
    ])
}

pub fn run(opts: &BuildOptions, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    shell.set_format(opts.message_format);
    let start = Instant::now();
    let started_secs = rb_store::now_secs();
    let phases = Phases {
        shell,
        last: std::cell::Cell::new(start),
    };
    let cwd = std::env::current_dir()?;
    let cargo_cfg = CargoConfig::load(&cwd)?;
    phases.mark("cargo config");
    let rustc = Rustc::detect(cargo_cfg.rustc()?.as_deref(), &cwd, &cfg.cache_dir())?;
    phases.mark("rustc detection");
    if cargo_cfg.build_std {
        bail!("`-Zbuild-std` (`[unstable] build-std`) is not supported by rb");
    }
    let vendor = cargo_cfg.vendor_dir()?;
    let offline = opts.offline || opts.frozen || cargo_cfg.offline()?;
    let mut ws = Workspace::load(
        &LoadOptions {
            manifest_path: opts.manifest_path.as_deref(),
            locked: opts.locked || opts.frozen,
            offline,
            ignore_rust_version: opts.ignore_rust_version,
            vendor: vendor.clone(),
        },
        &cwd,
        &Env {
            home: &cfg.home,
            rustc_version: release_semver(&rustc.release),
            registries: cargo_cfg.registries(),
            download_mirror: cargo_download_cache(cfg),
            shell,
        },
    )?;
    phases.mark("workspace + lockfile");
    let target_args: Vec<String> = if opts.targets.is_empty() {
        cargo_cfg.build_targets()?
    } else {
        opts.targets.clone()
    };
    let unstable = parse_unstable(&opts.unstable)?;
    let json_target_spec = unstable.json_target_spec;
    if target_args.iter().any(|t| t.ends_with(".json")) && !json_target_spec {
        bail!("`.json` target specs require -Zjson-target-spec to be added to the cargo invocation");
    }
    let wrapper = cargo_cfg.rustc_wrapper()?.filter(|w| {
        let is_sccache = w.file_stem().is_some_and(|s| s == "sccache");
        if is_sccache {
            shell.verbose_status("Skipping", "sccache as RUSTC_WRAPPER (rb's store already caches every unit)");
        }
        !is_sccache && !w.as_os_str().is_empty()
    });
    let requests: Vec<TargetRequest> = target_args
        .iter()
        .map(|t| TargetRequest::from_arg(t, &cwd))
        .collect::<Result<_>>()?;
    let no_target = requests.is_empty();

    let profile_name = match (&opts.profile, opts.release, opts.command) {
        (Some(p), _, _) => p.clone(),
        (None, _, Command::Bench) => "bench".into(),
        (None, true, _) => "release".into(),
        (None, false, Command::Test) => "test".into(),
        (None, false, _) => "dev".into(),
    };
    let incremental = std::env::var("CARGO_INCREMENTAL")
        .ok()
        .map(|v| v == "1")
        .or(cargo_cfg.incremental()?);
    let mut profiles = Profiles::new(&ws.root_manifest, &cargo_cfg.profiles, &profile_name, incremental)?;
    profiles.weaken_host_debuginfo = no_target;
    let target_dir = opts
        .target_dir
        .clone()
        .map(|p| cwd.join(p))
        .or(cargo_cfg.target_dir(&cwd)?)
        .unwrap_or_else(|| ws.target_dir.clone());
    let layout = Layout::new(&target_dir, &profiles.dir_name);
    let std_externs = if unstable.build_core {
        if !opts.targets.is_empty() {
            bail!("`-Zbuild-std` for a cross target is not supported by rb yet");
        }
        build_core_sysroot(opts, cfg, shell, &rustc, &target_dir, &layout)?
    } else {
        Vec::new()
    };

    let toolchains = cfg.toolchains()?;
    let mut crosses = Vec::new();
    for req in &requests {
        if req.spec_path.is_none() && !rustc.has_std(&req.triple) {
            if opts.no_provision {
                bail!(
                    "the standard library for `{}` is not installed; run `rb target add {}`",
                    req.triple,
                    req.display()
                );
            }
            shell.status("Installing", format!("rust-std for {}", req.triple));
            rb_toolchain::target::rustup_add_target(&req.triple, &cwd)?;
        }
        let cross = toolchains.setup(&rustc, req, !opts.no_provision)?;
        crosses.push(cross);
    }

    // cfg() rustflags depend on the target's cfg, which depends on the flags. Query twice if a table adds any.
    let flags_and_info = |triple: &str, target: Option<&str>, extra: &[String]| -> Result<(Vec<String>, Arc<TargetInfo>)> {
        let query = |flags: &[String]| {
            let mut q = flags.to_vec();
            q.extend(extra.iter().cloned());
            if target.is_some_and(|t| t.ends_with(".json")) {
                q.push("-Zunstable-options".into());
            }
            rustc.target_info(target, &q)
        };
        let flags = cargo_cfg.rustflags(triple, None)?;
        let info = query(&flags)?;
        if !cargo_cfg.has_cfg_targets() {
            return Ok((flags, info));
        }
        let with_cfg = cargo_cfg.rustflags(triple, Some(&PlatformCfg { triple, cfg: &info.cfg }))?;
        if with_cfg == flags {
            return Ok((flags, info));
        }
        let info = query(&with_cfg)?;
        Ok((with_cfg, info))
    };
    let (host_flags, host_info) = if no_target {
        flags_and_info(&rustc.host, None, &[])?
    } else {
        (Vec::new(), rustc.target_info(None, &[])?)
    };
    let kind_info = |name: String,
                     triple: &str,
                     flag: Option<String>,
                     info: Arc<TargetInfo>,
                     flags: Vec<String>,
                     cross: CrossToolchain|
     -> Result<KindInfo> {
        let platform = PlatformCfg { triple, cfg: &info.cfg };
        let linker = cargo_cfg.linker(triple, Some(&platform))?.or_else(|| cross.linker.clone());
        let runner = cargo_cfg.runner(triple, Some(&platform))?;
        Ok(KindInfo {
            runner,
            deps_dir: layout.deps(&name),
            build_dir: layout.build(&name),
            incremental_dir: layout.incremental(&name),
            artifact_dir: layout.artifact_dir(flag.as_ref().map(|_| triple)),
            name,
            triple: triple.to_owned(),
            target_flag: flag,
            info,
            user_rustflags: flags,
            linker,
            cross,
        })
    };
    let host = kind_info(
        "host".into(),
        &rustc.host,
        None,
        host_info.clone(),
        host_flags,
        CrossToolchain::default(),
    )?;
    let mut targets = Vec::new();
    for (req, cross) in requests.iter().zip(crosses) {
        let rustc_target = req.rustc_target();
        let (flags, info) = flags_and_info(&req.triple, Some(&rustc_target), &cross.rustflags)?;
        targets.push(kind_info(req.display(), &req.triple, Some(rustc_target), info, flags, cross)?);
    }

    phases.mark("toolchains + target info");
    let selected = ws.select(&opts.packages, opts.workspace, &opts.exclude)?;
    if opts.command == Command::Run && selected.len() != 1 {
        bail!("`rb run` could not determine which package to run; use `-p <package>`");
    }
    let host_cfg = PlatformCfg {
        triple: &rustc.host,
        cfg: &host_info.cfg,
    };
    let target_cfgs: Vec<PlatformCfg<'_>> = targets
        .iter()
        .map(|k| PlatformCfg {
            triple: &k.triple,
            cfg: &k.info.cfg,
        })
        .collect();
    let platforms = Platforms {
        host: PlatformCfg {
            triple: &rustc.host,
            cfg: &host_info.cfg,
        },
        targets: if no_target {
            vec![PlatformCfg {
                triple: &rustc.host,
                cfg: &host_info.cfg,
            }]
        } else {
            target_cfgs.clone()
        },
    };
    let dev_for: HashSet<usize> = if opts.filter.needs_dev_deps(opts.command) {
        selected.iter().copied().collect()
    } else {
        HashSet::new()
    };
    let res = ws.resolve_features(
        &selected,
        &FeatureRequest {
            features: &opts.features,
            all_features: opts.all_features,
            no_default_features: opts.no_default_features,
        },
        &platforms,
        &dev_for,
        shell,
    )?;
    ws.finalize();
    phases.mark("feature resolution + downloads");
    let filter = run_filter(&ws, &selected, opts)?;
    let target_apple: Vec<bool> = targets.iter().map(|k| k.info.is_apple()).collect();
    let graph = unit::build(
        &GraphInputs {
            ws: &ws,
            res: &res,
            profiles: &profiles,
            host_platform: &host_cfg,
            target_platforms: if no_target { &[] } else { &target_cfgs },
            host_apple: host_info.is_apple(),
            target_apple: &target_apple,
            no_target,
            command: opts.command,
        },
        &RootSelection {
            packages: &selected,
            command: opts.command,
            filter: &filter,
        },
    )?;
    phases.mark("unit graph");
    if opts.unit_graph {
        let names: Vec<String> = requests.iter().map(|r| r.triple.clone()).collect();
        println!("{}", serde_json::to_string_pretty(&unit::to_json(&ws, &graph, &names))?);
        return Ok(());
    }

    let jobs = opts
        .jobs
        .or(cargo_cfg.jobs()?)
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));
    let store_enabled = cfg.store_enabled && !opts.no_store;
    let ctx = Ctx {
        ws: &ws,
        cfg,
        rustc: &rustc,
        shell,
        norm: Normalizer::new(&target_dir, &ws.root),
        hasher: SourceHasher::load(&layout.state_dir(), vec![target_dir.clone()]),
        layout: layout.clone(),
        host,
        targets,
        check: opts.command == Command::Check,
        jobs,
        primary: selected.iter().copied().collect(),
        env_config: cargo_cfg.env_vars()?,
        wrapper,
        host_fingerprint: plan::host_fingerprint(&cfg.cache_dir()),
        cache_build_scripts: cfg.cache_build_scripts && !opts.no_cache_build_scripts,
        no_cache_build_scripts: no_cache_list(&ws),
        remap_deps: cfg.remap_deps_paths,
        codegen_backend: cfg.codegen_backend.clone().filter(|b| {
            let ok = b == "llvm" || (rustc.nightly && rustc.has_codegen_backend(b));
            if !ok {
                shell.warn(format!(
                    "codegen backend `{b}` is not available for {} (install it with `rustup component add rustc-codegen-{b}-preview` on nightly); using LLVM",
                    rustc.release
                ));
            }
            ok
        }),
        store_enabled,
        predictions: plan::Predictions::new(cfg.home.join("predict")),
        extra_rustc: &opts.extra_rustc,
        std_rustc: &opts.std_rustc,
        std_externs: &std_externs,
        extra_rustdoc: &opts.extra_rustdoc,
    };
    let mut graph = graph;
    let mut planned = plan::plan(&ctx, &graph)?;
    dedupe_by_key(&mut graph, &mut planned);
    phases.mark("planning + keys");
    debug_keys(&ctx, &graph, &planned, "")?;

    std::fs::create_dir_all(layout.rb_dir())?;
    // CARGO_TARGET_TMPDIR
    std::fs::create_dir_all(layout.target_dir.join("tmp"))?;
    let _target_lock = match rb_store::FileLock::try_exclusive(&layout.lock_path())? {
        Some(l) => l,
        None => {
            shell.status("Blocking", "waiting for file lock on build directory");
            rb_store::FileLock::exclusive(&layout.lock_path())?
        }
    };
    let store = if store_enabled { Some(Store::open(&cfg.store_dir)?) } else { None };
    let _store_lock = store.as_ref().map(|s| s.build_lock()).transpose()?;
    let link_mode = match opts.link_mode.unwrap_or(cfg.link_mode) {
        LinkMode::Auto => match &store {
            Some(s) => rb_store::link::probe(&s.tmp_dir(), &layout.rb_dir()).as_mode(),
            None => LinkMode::Copy,
        },
        m => m,
    };
    let engine = Engine {
        ctx: &ctx,
        graph: &graph,
        planned: &planned,
        store: store.as_ref(),
        link_mode,
        results: (0..graph.units.len()).map(|_| OnceLock::new()).collect(),
        jobserver: jobserver::Client::new(jobs).context("failed to create jobserver")?,
    };
    let result = execute(&engine, opts, &profiles, start);
    ctx.hasher.save();
    let mut state = match result {
        Ok(state) => state,
        Err(err) => {
            emit_build_finished(shell, false);
            return Err(err);
        }
    };
    emit_build_finished(shell, true);
    if ctx.predictions.changed() {
        rekey(&engine, &graph, &mut state)?;
    }
    let outcome = match opts.command {
        Command::Run => crate::run::run_binary(&engine, &state, &opts.args),
        Command::Test | Command::Bench if !opts.no_run => crate::run::run_tests(&engine, &state, opts),
        Command::Test | Command::Bench => crate::run::list_test_executables(&engine, &state),
        Command::Doc { .. } => crate::run::report_docs(&engine, &state, opts.open),
        _ => Ok(()),
    };
    drop(_store_lock);
    housekeeping(&engine, &mut state, store.as_ref(), started_secs)?;
    outcome
}

/// Sweep stale target variants and queue store cleanup. Caller has already dropped the shared store lock.
fn housekeeping(e: &Engine<'_>, state: &mut ProjectState, store: Option<&rb_store::Store>, started_secs: u64) -> Result<()> {
    let (cfg, layout, shell) = (e.ctx.cfg, &e.ctx.layout, e.ctx.shell);
    let cutoff = match cfg.target_keep_days {
        0 => u64::MAX,
        days => started_secs.saturating_sub(days * 86400),
    };
    let live16 = state.sweep_state(cutoff, cfg.target_keep_variants);
    if cfg.target_keep_variants == 0 {
        for k in std::mem::take(&mut state.prev).into_values() {
            state.superseded.push(k);
        }
    }
    let (swept, bytes) = sweep_target(layout, &live16, cfg.target_keep_variants > 0);
    if swept > 0 {
        save_state(&layout.state_dir().join("units.json"), state)?;
        shell.verbose_status(
            "Swept",
            format!("{swept} stale unit variants ({}) from target/rb", rb_store::human_bytes(bytes)),
        );
    }
    let Some(store) = store else { return Ok(()) };
    let own: HashSet<String> = state.slots.values().cloned().collect();
    let others = project_keys(cfg, Some(&layout.target_dir)).1;
    let remove: Vec<String> = std::mem::take(&mut state.superseded)
        .into_iter()
        .filter(|k| !own.contains(k) && !others.contains(k))
        .collect();
    if !remove.is_empty() {
        store.queue_removal(&remove)?;
    }
    let over_cap = cfg.store_max_size.is_some_and(|max| store.approx_bytes().is_ok_and(|b| b > max));
    if remove.is_empty() && swept == 0 && !over_cap && store.ingest_jobs().is_empty() {
        return Ok(());
    }
    // Compaction uses these keys to see what's still materialized.
    register_project(e, state);
    match std::env::var("RB_COMPACT").as_deref() {
        Ok("off") => {}
        Ok("sync") => {
            if let Some(r) = compact(cfg)? {
                shell.verbose_status(
                    "Collected",
                    format!(
                        "{} unit(s) ({}) from the store, compressed {} unused blob(s) (saved {})",
                        r.units_removed,
                        rb_store::human_bytes(r.bytes_freed),
                        r.blobs_compressed,
                        rb_store::human_bytes(r.bytes_compressed)
                    ),
                );
            }
        }
        _ => spawn_compaction(),
    }
    Ok(())
}

/// Store cleanup that can run next to other builds. `None` if one is already going.
pub fn compact(cfg: &RbConfig) -> Result<Option<rb_store::gc::GcReport>> {
    // Wait out a burst of rebuilds. Hashing right after a cold build blows rustc's incremental
    // cache, and the next one-line edit loses to cargo.
    if let Ok(idle) = std::env::var("RB_COMPACT_IDLE") {
        let secs: u64 = idle.parse().unwrap_or(0);
        if secs > 0 {
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
    }
    // Stay off the disk while a build is running. Hashing through a RUSTFLAGS rebuild lost to cargo.
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
        }
        let _ = setiopolicy_np(0, 0, 1);
    }
    let store = Store::open(&cfg.store_dir)?;
    let Some(_compacting) = store.try_compact_lock()? else {
        return Ok(None);
    };
    // Don't take the shared build lock. The next rebuild must not wait, and hashing has to see it and stop.
    if store.try_gc_lock()?.is_none() || !crate::exec::finish_deferred_ingests(&store) {
        return Ok(None);
    }
    let mut report = compact_once(cfg, &store)?;
    while !store.queued_removals().0.is_empty() {
        if store.try_gc_lock()?.is_none() {
            return Ok(Some(report));
        }
        let more = compact_once(cfg, &store)?;
        report.units_removed += more.units_removed;
        report.bytes_freed += more.bytes_freed;
        report.blobs_compressed += more.blobs_compressed;
        report.bytes_compressed += more.bytes_compressed;
        report.bytes_kept = more.bytes_kept;
    }
    Ok(Some(report))
}

fn compact_once(cfg: &RbConfig, store: &Store) -> Result<rb_store::gc::GcReport> {
    let (queued, queue_files) = store.queued_removals();
    let (symlinked, hot) = project_keys(cfg, None);
    let over_cap = cfg.store_max_size.filter(|&max| store.approx_bytes().is_ok_and(|b| b > max));
    // Live projects stay, even over the cap. Collecting down to 90% avoids a sweep after every edit.
    let mut pinned = symlinked;
    pinned.extend(hot.iter().cloned());
    let opts = rb_store::gc::GcOptions {
        max_size: over_cap.map(|max| max / 10 * 9),
        remove: queued.difference(&hot).cloned().collect(),
        pinned,
        hot: Some(hot),
        concurrent: true,
        ..Default::default()
    };
    let report = rb_store::gc::gc(store, &opts)?;
    for f in queue_files {
        let _ = std::fs::remove_file(f);
    }
    store.set_approx_bytes(report.bytes_kept)?;
    pack_pending_incremental(cfg);
    Ok(report)
}

/// Pack incremental dirs a build moved aside. The build itself doesn't wait for this.
fn pack_pending_incremental(cfg: &RbConfig) {
    let Ok(rd) = std::fs::read_dir(cfg.home.join("projects")) else {
        return;
    };
    let mut pending = Vec::new();
    for e in rd.filter_map(|e| e.ok()) {
        let Ok(doc) =
            std::fs::read(e.path()).and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).map_err(std::io::Error::other))
        else {
            continue;
        };
        let Some(target) = doc["target_dir"].as_str() else { continue };
        for dir in walkdir::WalkDir::new(Path::new(target).join("rb"))
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if dir.file_type().is_dir() && dir.file_name().to_string_lossy().ends_with(".pending") {
                let archive = dir.path().with_extension("tar.zst");
                pending.push((dir.path().to_owned(), archive));
            }
        }
    }
    rb_store::par_for_each(&pending, |(dir, archive)| {
        if rb_store::archive::pack(dir, archive).is_err() {
            let _ = std::fs::remove_dir_all(dir);
        }
    });
}

/// `rb __compact` in the background, nice'd, so the build doesn't wait. Same idea as `git gc --auto`.
fn spawn_compaction() {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = if Path::new("/usr/sbin/taskpolicy").exists() {
        let mut c = std::process::Command::new("/usr/sbin/taskpolicy");
        c.arg("-b").arg(&exe);
        c
    } else if Path::new("/usr/bin/nice").exists() {
        let mut c = std::process::Command::new("/usr/bin/nice");
        c.args(["-n", "19"]).arg(&exe);
        c
    } else {
        std::process::Command::new(&exe)
    };
    cmd.arg("__compact")
        .env("RB_COMPACT_IDLE", "15")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let _ = cmd.spawn();
}

fn run_filter(ws: &Workspace, selected: &[usize], opts: &BuildOptions) -> Result<TargetFilter> {
    if opts.command != Command::Run {
        return Ok(opts.filter.clone());
    }
    let mut f = opts.filter.clone();
    if !f.bin_names.is_empty() || !f.example_names.is_empty() {
        return Ok(f);
    }
    let p = &ws.pkgs[selected[0]];
    let bins: Vec<&str> = p
        .targets
        .iter()
        .filter(|t| t.kind == TargetKind::Bin)
        .map(|t| t.name.as_str())
        .collect();
    let chosen = match (&p.default_run, bins.as_slice()) {
        (Some(d), _) => d.clone(),
        (None, [one]) => (*one).to_owned(),
        (None, []) => bail!("a bin target must be available for `rb run`"),
        (None, many) => bail!(
            "`rb run` could not determine which binary to run. Use the `--bin` option to specify a binary, or the `default-run` manifest key.\navailable binaries: {}",
            many.join(", ")
        ),
    };
    f.bin_names = vec![chosen];
    Ok(f)
}

fn no_cache_list(ws: &Workspace) -> HashSet<String> {
    ws.root_manifest
        .get("workspace")
        .and_then(|w| w.get("metadata"))
        .and_then(|m| m.get("rb"))
        .and_then(|r| r.get("no-cache-build-scripts"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Same key, same work. Point everything at the first copy.
fn dedupe_by_key(graph: &mut unit::UnitGraph, planned: &mut [plan::Planned]) {
    let mut first: HashMap<String, usize> = HashMap::new();
    let canon: Vec<usize> = (0..planned.len())
        .map(|i| *first.entry(planned[i].key.clone()).or_insert(i))
        .collect();
    if canon.iter().enumerate().all(|(i, &c)| i == c) {
        return;
    }
    for u in &mut graph.units {
        for d in &mut u.deps {
            d.unit = canon[d.unit];
        }
    }
    for p in planned.iter_mut() {
        p.own_run = p.own_run.map(|r| canon[r]);
        for v in [&mut p.to_link, &mut p.lib_reqs] {
            for x in v.iter_mut() {
                *x = canon[*x];
            }
            v.sort_unstable();
            v.dedup();
        }
    }
    for r in graph.roots.iter_mut().chain(graph.aux_roots.iter_mut()) {
        *r = canon[*r];
    }
    for d in &mut graph.doctests {
        d.lib = canon[d.lib];
        for dep in &mut d.dev_deps {
            dep.unit = canon[dep.unit];
        }
    }
    graph.roots.dedup();
}

fn unit_timing_label(e: &Engine<'_>, ui: usize) -> String {
    let u = &e.graph.units[ui];
    let p = &e.ctx.ws.pkgs[u.pkg];
    format!(
        "{}@{} {} {:?} {}",
        p.name,
        p.version,
        p.targets[u.target].name,
        u.mode,
        e.ctx.kind(u.kind).name
    )
}

fn verb(e: &Engine<'_>, ui: usize) -> &'static str {
    if e.graph.units[ui].mode == Mode::Check {
        "Checking"
    } else {
        "Compiling"
    }
}

fn execute(e: &Engine<'_>, opts: &BuildOptions, profiles: &Profiles, start: Instant) -> Result<ProjectState> {
    let ctx = e.ctx;
    let shell = ctx.shell;
    let units = &e.graph.units;
    let state_path = ctx.layout.state_dir().join("units.json");
    let mut state = load_state(&state_path);
    let mut seen_keys = HashSet::new();
    let canon: Vec<bool> = e.planned.iter().map(|p| seen_keys.insert(p.key.as_str())).collect();
    let order: Vec<usize> = plan::topo_order(units).into_iter().filter(|&u| canon[u]).collect();

    let mut status = vec![Status::Miss; units.len()];
    let mut entries: HashMap<usize, rb_store::Entry> = HashMap::new();
    for &ui in &order {
        let key = &e.planned[ui].key;
        if let Some(rec) = state.units.get(key)
            && state.slots.get(&e.planned[ui].name) == Some(key)
            && rec.outputs.iter().all(|o| o.exists())
            && (units[ui].mode != Mode::RunCustomBuild || e.planned[ui].out_dir_path().is_dir())
            && e.inputs_hold(ui, &rec.inputs)
        {
            status[ui] = Status::Fresh;
            if let Some(pl) = &rec.payload {
                e.set_build_output(ui, pl);
            }
            continue;
        }
        if let Some(entry) = e.lookup(ui)? {
            status[ui] = Status::Cached;
            entries.insert(ui, entry);
        }
    }

    // A hit doesn't pull its dependencies. A miss does.
    let mut needed = vec![false; units.len()];
    let mut stack: Vec<usize> = e.graph.roots.iter().chain(&e.graph.aux_roots).copied().collect();
    // Doctests link the lib, so its deps and build-script outputs have to be on disk.
    for &a in &e.graph.aux_roots {
        stack.extend(e.planned[a].lib_reqs.iter().copied());
        stack.extend(e.planned[a].to_link.iter().copied());
    }
    // Bins tests launch via CARGO_BIN_EXE_*, even when the tests themselves are cached.
    for &r in &e.graph.roots {
        if units[r].mode.is_test() {
            stack.extend(units[r].deps.iter().map(|d| d.unit).filter(|&d| is_bin_build(e, d)));
        }
    }
    while let Some(u) = stack.pop() {
        if std::mem::replace(&mut needed[u], true) || status[u] != Status::Miss {
            continue;
        }
        let p = &e.planned[u];
        stack.extend(units[u].deps.iter().map(|d| d.unit));
        stack.extend(p.lib_reqs.iter().copied());
        stack.extend(p.to_link.iter().copied());
    }
    // Touch the whole graph. Sweeping a fresh unit nobody reached would throw away its incremental cache.
    for &ui in &order {
        state.touch(&e.planned[ui]);
    }
    let touch_store = rb_store::now_secs().saturating_sub(state.store_touched) > 86400;
    if touch_store {
        for &ui in order.iter().filter(|&&u| needed[u] && status[u] == Status::Fresh) {
            e.touch_store(ui);
        }
    }
    if touch_store {
        state.store_touched = rb_store::now_secs();
    }
    // Drop slot ownership before rewriting files. A crash must not leave the old record in charge.
    let mut released = false;
    for &ui in order.iter().filter(|&&u| needed[u] && status[u] != Status::Fresh) {
        let p = &e.planned[ui];
        if let Some(old) = state.slots.remove(&p.name) {
            released = true;
            if old != p.key {
                // keep-variants = 0: drop the old store entry. A revert recompiles.
                if e.ctx.cfg.target_keep_variants == 0 {
                    state.superseded.push(old);
                } else if let Some(older) = state.prev.insert(p.name.clone(), old) {
                    state.superseded.push(older);
                }
            }
        }
    }
    if released {
        save_state(&state_path, &state)?;
    }

    let cached: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&u| needed[u] && status[u] == Status::Cached)
        .collect();
    let mut printed: HashSet<(usize, CompileKind)> = HashSet::new();
    let restored = restore_parallel(e, &cached, &entries)?;
    for (ui, rec) in restored {
        let u = &units[ui];
        if printed.insert((u.pkg, u.kind)) {
            shell.status_alt("Cached", e.label(ui));
        }
        replay(e, ui, &rec);
        state.record(&e.planned[ui], rec);
    }
    for &ui in &order {
        if needed[ui] && status[ui] == Status::Fresh {
            let u = &units[ui];
            if printed.insert((u.pkg, u.kind)) {
                shell.verbose_status("Fresh", e.label(ui));
            }
            if let Some(rec) = state.units.get(&e.planned[ui].key) {
                replay(e, ui, rec);
            }
        }
    }

    let misses: Vec<usize> = order.iter().copied().filter(|&u| needed[u] && status[u] == Status::Miss).collect();
    let mut timings = Timings::load(&ctx.cfg.home);
    let mut rows = Vec::new();
    let mut printed_compiling = HashSet::new();
    let outcome = schedule(
        e,
        &misses,
        &mut timings,
        &mut state,
        &mut rows,
        &mut printed_compiling,
        opts.keep_going,
    );
    timings.save();
    state.prune();
    save_state(&state_path, &state)?;
    let restored_after_wait = outcome?;

    // Same set cargo uplifts: requested units, every bin (including test bins), and dylibs.
    let mut uplifted: HashMap<usize, Vec<PathBuf>> = HashMap::new();
    for (i, u) in units.iter().enumerate().filter(|(i, _)| needed[*i]) {
        let root = e.graph.roots.contains(&i);
        let dylib = u.target(e.ctx.ws).crate_types.iter().any(|c| c == "dylib");
        if (u.mode == Mode::Build && (root || is_bin_build(e, i) || dylib)) || (root && u.mode.is_test()) {
            uplifted.insert(i, uplift(e, i, &state)?);
        }
    }
    if shell.format().is_json() {
        for (i, _) in units
            .iter()
            .enumerate()
            .filter(|(i, u)| needed[*i] && u.mode != Mode::RunCustomBuild && u.mode != Mode::Doc)
        {
            emit_compiler_artifact(e, i, &state, status.get(i) == Some(&Status::Fresh), uplifted.get(&i));
        }
    }
    register_project(e, &state);
    shell.clear_progress();
    let prof = profiles.base_profile();
    let desc = format!(
        "[{}{}]",
        if prof.opt_level == "0" { "unoptimized" } else { "optimized" },
        if prof.debuginfo_on() { " + debuginfo" } else { "" }
    );
    let fresh = needed.iter().zip(&status).filter(|(n, s)| **n && **s == Status::Fresh).count();
    shell.status(
        "Finished",
        format!(
            "`{}` profile {desc} target(s) in {:.2}s ({} compiled, {} cached, {} fresh)",
            profiles.requested,
            start.elapsed().as_secs_f64(),
            misses.len() - restored_after_wait,
            cached.len() + restored_after_wait,
            fresh
        ),
    );
    if opts.timings {
        let wall = start.elapsed().as_millis() as u64;
        crate::timings::print_summary(&mut rows, wall, ctx.jobs);
        crate::timings::write_html(&ctx.layout.target_dir, &rows, wall);
    }
    Ok(state)
}

fn replay(e: &Engine<'_>, ui: usize, rec: &Record) {
    for d in &rec.diagnostics {
        e.ctx.shell.raw(d);
    }
    if !rec.diagnostics.is_empty() {
        let n = rec.diagnostics.len();
        let p = &e.ctx.ws.pkgs[e.graph.units[ui].pkg];
        e.ctx.shell.warn(format!(
            "`{}` {} generated {n} warning{}",
            p.name,
            e.planned[ui].descr,
            if n == 1 { "" } else { "s" }
        ));
    }
}

fn restore_parallel(e: &Engine<'_>, units: &[usize], entries: &HashMap<usize, rb_store::Entry>) -> Result<Vec<(usize, Record)>> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let out = std::sync::Mutex::new(Vec::new());
    let workers = e.ctx.jobs.min(units.len()).max(1);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&ui) = units.get(i) else { break };
                    let r = e.materialize(ui, &entries[&ui]);
                    out.lock().unwrap().push((ui, r));
                }
            });
        }
    });
    out.into_inner()
        .unwrap()
        .into_iter()
        .map(|(ui, r)| r.map(|rec| (ui, rec)))
        .collect()
}

enum Event {
    Token(std::io::Result<jobserver::Acquired>),
    Meta(usize),
    /// rustc exited. Dependents can start. Bookkeeping is `Done`.
    Unlocked(usize),
    Done(usize, Box<Result<crate::exec::Outcome>>),
    /// Outputs are in the store. A failure here is not fatal.
    Stored(usize, Result<()>, Duration),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    Meta,
    Full,
}

fn schedule(
    e: &Engine<'_>,
    misses: &[usize],
    timings: &mut Timings,
    state: &mut ProjectState,
    rows: &mut Vec<TimingRow>,
    printed: &mut HashSet<(usize, CompileKind)>,
    keep_going: bool,
) -> Result<usize> {
    if misses.is_empty() {
        return Ok(0);
    }
    let mut restored_after_wait = 0;
    let ctx = e.ctx;
    let shell = ctx.shell;
    let units = &e.graph.units;
    let miss_set: HashSet<usize> = misses.iter().copied().collect();

    let mut waits: HashMap<usize, usize> = HashMap::new();
    let mut dependents: HashMap<usize, Vec<(usize, Need, bool)>> = HashMap::new();
    for &u in misses {
        let p = &e.planned[u];
        let mut reqs: Vec<(usize, Need)> = Vec::new();
        match units[u].mode {
            Mode::RunCustomBuild => reqs.extend(units[u].deps.iter().map(|d| (d.unit, Need::Full))),
            _ => {
                for &l in &p.lib_reqs {
                    let meta_ok = !p.needs_link && !units[l].target(ctx.ws).is_proc_macro() && e.planned[l].emits_meta;
                    reqs.push((l, if meta_ok { Need::Meta } else { Need::Full }));
                }
                reqs.extend(p.to_link.iter().map(|&r| (r, Need::Full)));
                if let Some(r) = p.own_run {
                    reqs.push((r, Need::Full));
                }
                // Bins an integration test runs.
                for d in units[u]
                    .deps
                    .iter()
                    .filter(|d| d.extern_name.is_none() && units[d.unit].mode != Mode::RunCustomBuild)
                {
                    reqs.push((d.unit, Need::Full));
                }
            }
        }
        reqs.sort_by_key(|(d, n)| (*d, *n == Need::Full));
        reqs.dedup_by_key(|(d, _)| *d);
        let mut count = 0;
        for (d, need) in reqs {
            if miss_set.contains(&d) && d != u {
                dependents.entry(d).or_default().push((u, need, false));
                count += 1;
            }
        }
        waits.insert(u, count);
    }

    // Own time plus the longest chain after it. A first build guesses from source size.
    let mut source_bytes: HashMap<usize, u64> = HashMap::new();
    let mut estimate = |u: usize| -> u64 {
        if let Some(ms) = timings.get(&unit_timing_label(e, u)) {
            return ms;
        }
        let unit = &units[u];
        let t = unit.target(ctx.ws);
        match unit.mode {
            Mode::RunCustomBuild => 400,
            _ if t.kind == TargetKind::BuildScript => 500,
            _ => {
                let bytes = *source_bytes
                    .entry(unit.pkg)
                    .or_insert_with(|| rust_source_bytes(&ctx.ws.pkgs[unit.pkg].root));
                // ~150 source bytes per ms at opt-level 0, one core. A guess, not a measurement.
                let base = 200 + bytes / 150;
                let factor = if unit.mode.is_check() {
                    0.5
                } else if unit.profile.opt_level == "0" {
                    1.0
                } else {
                    2.0
                };
                (base as f64 * factor) as u64
            }
        }
    };
    let mut prio: HashMap<usize, u64> = HashMap::new();
    for &u in misses.iter().rev() {
        let tail = dependents
            .get(&u)
            .map(|v| v.iter().map(|(d, _, _)| prio.get(d).copied().unwrap_or(0)).max().unwrap_or(0))
            .unwrap_or(0);
        prio.insert(u, estimate(u) + tail);
    }
    let est: HashMap<usize, u64> = misses.iter().map(|&u| (u, estimate(u))).collect();

    let mut ready: BinaryHeap<(u64, Reverse<usize>)> = misses.iter().filter(|u| waits[u] == 0).map(|&u| (prio[&u], Reverse(u))).collect();
    let (tx, rx) = crossbeam_channel::unbounded::<Event>();
    let token_tx = tx.clone();
    let helper = e
        .jobserver
        .clone()
        .into_helper_thread(move |t| {
            let _ = token_tx.send(Event::Token(t));
        })
        .context("failed to start jobserver helper")?;
    let mut tokens: Vec<jobserver::Acquired> = Vec::new();
    let mut requested = 0usize;
    let mut running: HashMap<usize, String> = HashMap::new();
    let mut storing = 0usize;
    let mut store_time = (Duration::ZERO, Duration::ZERO);
    let mut meta_seen: HashSet<usize> = HashSet::new();
    let mut failed = false;
    let mut done = 0usize;
    let total = misses.len();

    let mut pipelined = 0usize;
    let satisfy = |d: usize,
                   full: bool,
                   dependents: &mut HashMap<usize, Vec<(usize, Need, bool)>>,
                   waits: &mut HashMap<usize, usize>,
                   ready: &mut BinaryHeap<(u64, Reverse<usize>)>|
     -> usize {
        let mut released = 0;
        if let Some(list) = dependents.get_mut(&d) {
            for (u, need, sat) in list.iter_mut() {
                if !*sat && (full || *need == Need::Meta) {
                    *sat = true;
                    let w = waits.get_mut(u).unwrap();
                    *w -= 1;
                    if *w == 0 {
                        ready.push((prio[u], Reverse(*u)));
                        released += 1;
                    }
                }
            }
        }
        released
    };

    std::thread::scope(|s| -> Result<()> {
        loop {
            while (!failed || keep_going) && !ready.is_empty() && !tokens.is_empty() {
                let Reverse(u) = ready.pop().unwrap().1;
                let token = tokens.pop().unwrap();
                let unit = &units[u];
                if printed.insert((unit.pkg, unit.kind)) {
                    shell.status(verb(e, u), e.label(u));
                }
                let threads = parallel_frontend(e, u, est[&u], running.len() + ready.len());
                running.insert(u, ctx.ws.pkgs[unit.pkg].name.clone());
                let tx = tx.clone();
                s.spawn(move || {
                    let meta_tx = tx.clone();
                    let meta = move || {
                        let _ = meta_tx.send(Event::Meta(u));
                    };
                    let mut r = e.execute(u, threads, &meta);
                    drop(token);
                    // The job token is for rustc. Dep-info can wait, same as cargo.
                    if r.is_ok() {
                        let _ = tx.send(Event::Unlocked(u));
                    }
                    if let Ok(out) = r.as_mut()
                        && !out.restored
                        && e.graph.units[u].mode != Mode::RunCustomBuild
                        && let Err(err) = e.attach_discovered(u, &mut out.record)
                    {
                        r = Err(err);
                    }
                    let pending = match &mut r {
                        Ok(out) if !out.restored => Some((out.record.clone(), out.duration, out.lock.take())),
                        _ => None,
                    };
                    let _ = tx.send(Event::Done(u, Box::new(r)));
                    if let Some((record, duration, lock)) = pending {
                        if e.defer_ingest(u, &record, duration) {
                            drop(lock);
                            let _ = tx.send(Event::Stored(u, Ok(()), Duration::ZERO));
                        } else {
                            let start = Instant::now();
                            let r = e.store_outputs(u, &record, duration, lock);
                            let _ = tx.send(Event::Stored(u, r, start.elapsed()));
                        }
                    }
                });
            }
            if running.is_empty() && storing == 0 && (ready.is_empty() || (failed && !keep_going)) {
                break;
            }
            let want = if failed && !keep_going { 0 } else { ready.len() };
            while requested + tokens.len() < want {
                helper.request_token();
                requested += 1;
            }
            if ready.is_empty() || (failed && !keep_going) {
                tokens.clear();
            }
            let names: Vec<String> = running.values().cloned().collect();
            shell.progress(done, total, &names);
            match rx.recv().map_err(|_| anyhow!("scheduler channel closed"))? {
                Event::Token(t) => {
                    requested = requested.saturating_sub(1);
                    tokens.push(t.context("failed to acquire jobserver token")?);
                }
                Event::Meta(u) => {
                    if meta_seen.insert(u) {
                        pipelined += satisfy(u, false, &mut dependents, &mut waits, &mut ready);
                    }
                }
                Event::Unlocked(u) => {
                    satisfy(u, true, &mut dependents, &mut waits, &mut ready);
                }
                Event::Stored(u, r, took) => {
                    storing -= 1;
                    store_time.0 += took;
                    store_time.1 = store_time.1.max(took);
                    if let Err(err) = r {
                        shell.warn(format!("{err:#} (the build itself succeeded; {} stays project-local)", e.label(u)));
                    }
                }
                Event::Done(u, r) => {
                    running.remove(&u);
                    done += 1;
                    match *r {
                        Ok(out) => {
                            if out.restored {
                                restored_after_wait += 1;
                            } else {
                                storing += 1;
                                let ms = out.duration.as_millis() as u64;
                                timings.record(&unit_timing_label(e, u), ms);
                                rows.push(TimingRow {
                                    label: format!("{} {}", e.label(u), e.planned[u].descr),
                                    ms,
                                });
                            }
                            state.record(&e.planned[u], out.record);
                        }
                        Err(err) => {
                            shell.clear_progress();
                            shell.error(format!("{err:#}"));
                            failed = true;
                        }
                    }
                }
            }
        }
        Ok(())
    })?;
    shell.clear_progress();
    drop(helper);
    if failed {
        return Err(AlreadyReported.into());
    }
    if !rows.is_empty() {
        rows.push(TimingRow {
            label: format!("(pipelining: {pipelined} units started as soon as upstream metadata was ready)"),
            ms: 0,
        });
        rows.push(TimingRow {
            label: format!(
                "(store: {:.2}s spent storing outputs off the critical path, longest {:.2}s)",
                store_time.0.as_secs_f64(),
                store_time.1.as_secs_f64()
            ),
            ms: 0,
        });
    }
    Ok(restored_after_wait)
}

/// `-Zthreads` on long nightly units when other cores are sitting idle.
fn parallel_frontend(e: &Engine<'_>, u: usize, est_ms: u64, busy: usize) -> Option<usize> {
    use crate::config::Toggle;
    let ctx = e.ctx;
    if !ctx.rustc.nightly || ctx.cfg.parallel_frontend == Toggle::Off || e.graph.units[u].mode == Mode::RunCustomBuild {
        return None;
    }
    let long = est_ms >= 1000 || ctx.cfg.parallel_frontend == Toggle::On;
    // A warm incremental session is faster on one thread. Empty session (cold, or new RUSTFLAGS) still goes parallel.
    let session = e.planned[u].inv.args.iter().find_map(|(a, _)| a.strip_prefix("incremental="));
    if session.is_some_and(|p| Path::new(p).is_dir()) && ctx.cfg.parallel_frontend != Toggle::On {
        return None;
    }
    // A full rebuild sat around 9 of 14 jobs. Give a long crate the cores nothing else is using.
    let spare = ctx.jobs.saturating_sub(busy);
    (long && spare >= 2).then(|| spare.clamp(2, 8))
}

/// First-time discovered inputs change dependents' keys. Move records to those keys so the next build doesn't redo the graph.
fn rekey(e: &Engine<'_>, graph: &unit::UnitGraph, state: &mut ProjectState) -> Result<()> {
    // An edit changes a local package and this is a no-op. Replanning just to notice that
    // was a second full plan on the first incremental rebuild.
    let mut seen = HashSet::new();
    for u in &graph.units {
        let pkg = &e.ctx.ws.pkgs[u.pkg];
        if !pkg.source.is_local() || !seen.insert(pkg.root.clone()) {
            continue;
        }
        if !e.ctx.hasher.tree_unchanged(&pkg.root).unwrap_or(false) {
            return Ok(());
        }
    }
    let replanned = plan::plan(e.ctx, graph)?;
    debug_keys(e.ctx, graph, &replanned, ".rekey")?;
    let mut trees: HashMap<PathBuf, bool> = seen.into_iter().map(|root| (root, true)).collect();
    for (ui, (old, new)) in e.planned.iter().zip(&replanned).enumerate() {
        if old.key == new.key {
            continue;
        }
        // Identity shrinks from the whole package to the files rustc read. Only valid if nothing changed mid-build.
        let p = &e.ctx.ws.pkgs[graph.units[ui].pkg];
        if p.source.is_local() {
            let unchanged = *trees
                .entry(p.root.clone())
                .or_insert_with(|| e.ctx.hasher.tree_unchanged(&p.root).unwrap_or(false));
            if !unchanged {
                return Ok(());
            }
        }
        if old.input_digest != new.input_digest {
            match state.units.get(&old.key) {
                Some(rec) if e.inputs_hold(ui, &rec.inputs) => {}
                _ => return Ok(()),
            }
        }
    }
    for (old, new) in e.planned.iter().zip(&replanned) {
        if old.key == new.key || state.slots.get(&old.name) != Some(&old.key) {
            continue;
        }
        let Some(rec) = state.units.get(&old.key).cloned() else { continue };
        if let Some(store) = e.store
            && old.cacheable
        {
            store.alias(&old.key, &new.key)?;
        }
        state.units.insert(new.key.clone(), rec);
        state.slots.insert(new.name.clone(), new.key.clone());
    }
    save_state(&e.ctx.layout.state_dir().join("units.json"), state)
}

/// `RB_DEBUG_KEYS=<file>` dumps every unit's key inputs, for diffing a surprise rebuild.
fn debug_keys(ctx: &Ctx<'_>, graph: &unit::UnitGraph, planned: &[plan::Planned], suffix: &str) -> Result<()> {
    let Some(path) = std::env::var_os("RB_DEBUG_KEYS") else {
        return Ok(());
    };
    let rows: Vec<serde_json::Value> = graph
        .units
        .iter()
        .zip(planned)
        .map(|(u, p)| {
            serde_json::json!({ "unit": format!("{} {} {:?}", ctx.ws.pkgs[u.pkg].display(), p.descr, u.kind), "key": p.key,
                "name": p.name, "input_digest": p.input_digest,
                "source": plan::unit_identity(ctx, u, ctx.predictions.load(&p.name).as_ref()).unwrap_or_default() })
        })
        .collect();
    let mut path = path;
    path.push(suffix);
    std::fs::write(path, serde_json::to_string_pretty(&rows)?)?;
    Ok(())
}

fn rust_source_bytes(root: &Path) -> u64 {
    let root_owned = root.to_owned();
    ignore::WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(move |e| {
            let p = e.path();
            !(e.file_type().is_some_and(|t| t.is_dir()) && p != root_owned && (e.file_name() == "target" || p.join("Cargo.toml").is_file()))
        })
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn is_bin_build(e: &Engine<'_>, u: usize) -> bool {
    let u = &e.graph.units[u];
    u.mode == Mode::Build && u.target(e.ctx.ws).kind == TargetKind::Bin
}

/// `target/<profile>/deps/`. Tests look for sibling bins relative to `current_exe()`.
pub fn test_exe_dir(e: &Engine<'_>, u: &Unit) -> PathBuf {
    e.ctx.kind(u.kind).artifact_dir.join("deps")
}

fn emit_build_finished(shell: &Shell, success: bool) {
    if shell.format().is_json() {
        shell.json_line(&serde_json::json!({ "reason": "build-finished", "success": success }));
    }
}

fn emit_compiler_artifact(e: &Engine<'_>, ui: usize, state: &ProjectState, fresh: bool, uplifted: Option<&Vec<PathBuf>>) {
    let u = &e.graph.units[ui];
    let p = &e.planned[ui];
    let pkg = &e.ctx.ws.pkgs[u.pkg];
    let filenames: Vec<String> = if let Some(paths) = uplifted {
        paths.iter().map(|p| p.display().to_string()).collect()
    } else {
        state
            .units
            .get(&p.key)
            .map(|rec| {
                rec.outputs
                    .iter()
                    .filter(|o| o.extension().is_none_or(|x| x != "d"))
                    .map(|o| o.display().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    if filenames.is_empty() {
        return;
    }
    let executable = u.target(e.ctx.ws).is_executable().then(|| filenames.first().cloned()).flatten();
    let debuginfo = match u.profile.debuginfo.parse::<u32>() {
        Ok(n) => serde_json::json!(n),
        Err(_) => serde_json::json!(u.profile.debuginfo),
    };
    let mut features = u.features.clone();
    features.sort();
    e.ctx.shell.json_line(&serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": pkg.id,
        "manifest_path": pkg.manifest_path,
        "target": e.target_message(ui),
        "profile": {
            "opt_level": u.profile.opt_level,
            "debuginfo": debuginfo,
            "debug_assertions": u.profile.debug_assertions,
            "overflow_checks": u.profile.overflow_checks,
            "test": u.mode.is_test(),
        },
        "features": features,
        "filenames": filenames,
        "executable": executable,
        "fresh": fresh,
    }));
}

fn uplift(e: &Engine<'_>, r: usize, state: &ProjectState) -> Result<Vec<PathBuf>> {
    let u = &e.graph.units[r];
    let p = &e.planned[r];
    let Some(rec) = state.units.get(&p.key) else {
        return Ok(Vec::new());
    };
    let k = e.ctx.kind(u.kind);
    let test = u.mode.is_test();
    let dir = if test {
        test_exe_dir(e, u)
    } else if u.target(e.ctx.ws).kind == TargetKind::Example {
        k.artifact_dir.join("examples")
    } else {
        k.artifact_dir.clone()
    };
    std::fs::create_dir_all(&dir)?;
    let suffix = format!("-{}", p.name16());
    let mut placed = Vec::new();
    for out in &rec.outputs {
        let name = out.file_name().unwrap().to_string_lossy();
        if name.ends_with(".rmeta") || (test && name.ends_with(".d")) {
            continue;
        }
        if test {
            let dest = dir.join(&*name);
            rb_store::link::replace(out, &dest, LinkMode::Auto, false).with_context(|| format!("failed to place {}", out.display()))?;
            placed.push(dest);
            continue;
        }
        let mut up = name.replace(&suffix, "");
        if let Some(bin) = &p.bin_name
            && let Some(rest) = up.strip_prefix(&p.crate_name)
            && (rest.is_empty() || rest == std::env::consts::EXE_SUFFIX || rest == ".exe" || rest == ".wasm")
        {
            up = format!("{bin}{rest}");
        }
        let dest = dir.join(&up);
        rb_store::link::replace(out, &dest, LinkMode::Auto, false).with_context(|| format!("failed to uplift {}", out.display()))?;
        placed.push(dest);
    }
    Ok(placed)
}

fn register_project(e: &Engine<'_>, state: &ProjectState) {
    if e.store.is_none() {
        return;
    }
    let dir = e.ctx.cfg.home.join("projects");
    let target = e.ctx.layout.target_dir.to_string_lossy().into_owned();
    let id = &blake3::hash(target.as_bytes()).to_hex()[..16];
    let doc = serde_json::json!({
        "target_dir": target,
        "last_used": rb_store::now_secs(),
        "link_mode": e.link_mode.to_string(),
        "keys": state.units.keys().collect::<Vec<_>>(),
    });
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(format!("{id}.json")), doc.to_string());
}

pub fn pinned_keys(cfg: &RbConfig) -> HashSet<String> {
    project_keys(cfg, None).0
}

/// Keys in other live projects: `(symlink mode, any mode)`. Drops registrations whose target dir is gone.
pub fn project_keys(cfg: &RbConfig, skip: Option<&Path>) -> (HashSet<String>, HashSet<String>) {
    let (mut symlinked, mut all) = (HashSet::new(), HashSet::new());
    let Ok(rd) = std::fs::read_dir(cfg.home.join("projects")) else {
        return (symlinked, all);
    };
    for e in rd.filter_map(|e| e.ok()) {
        let Some(doc) = std::fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        else {
            continue;
        };
        let Some(target) = doc["target_dir"].as_str().map(Path::new) else {
            continue;
        };
        if !target.is_dir() {
            let _ = std::fs::remove_file(e.path());
            continue;
        }
        if skip == Some(target) {
            continue;
        }
        let keys = doc["keys"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|k| k.as_str())
            .map(str::to_owned);
        if doc["link_mode"] == "symlink" {
            let keys: Vec<String> = keys.collect();
            symlinked.extend(keys.iter().cloned());
            all.extend(keys);
        } else {
            all.extend(keys);
        }
    }
    (symlinked, all)
}

pub fn clean(manifest_path: Option<&Path>, target_dir: Option<&Path>, shell: &Shell) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let dir = match target_dir {
        Some(d) => d.to_owned(),
        None => {
            let root = crate::workspace::workspace_root(&cwd, manifest_path)?;
            let cargo_cfg = CargoConfig::load(&cwd)?;
            cargo_cfg.target_dir(&cwd)?.unwrap_or_else(|| root.join("target"))
        }
    };
    if dir.exists() {
        let size = dir_size(&dir);
        std::fs::remove_dir_all(&dir).with_context(|| format!("failed to remove {}", dir.display()))?;
        shell.status(
            "Removed",
            format!(
                "{} ({:.1} MiB; the global store is untouched)",
                dir.display(),
                size as f64 / (1 << 20) as f64
            ),
        );
    }
    Ok(())
}

pub fn dir_size(dir: &Path) -> u64 {
    walk_size(dir)
}

fn walk_size(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    for e in rd.filter_map(|e| e.ok()) {
        match e.file_type() {
            Ok(t) if t.is_dir() => total += walk_size(&e.path()),
            Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => {}
        }
    }
    total
}
