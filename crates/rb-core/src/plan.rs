//! Invocations and content-addressed keys. A key hashes the work, never the absolute path.

use crate::config::RbConfig;
use crate::invocation::{Invocation, Normalizer, SELF_NAME16};
use crate::layout::Layout;
use crate::lints::Lints;
use crate::profile::Lto;
use crate::shell::Shell;
use crate::srchash::SourceHasher;
use crate::unit::{CompileKind, Mode, Unit, UnitGraph};
use crate::workspace::{Source, TargetKind, Workspace};
use anyhow::{Context, Result};
use rb_toolchain::{CrossToolchain, Rustc, TargetInfo};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct KindInfo {
    /// `host`, or the requested target, under `target/rb/<profile>/`.
    pub name: String,
    pub triple: String,
    /// rustc `--target`. None for host units.
    pub target_flag: Option<String>,
    pub info: Arc<TargetInfo>,
    /// Config and env rustflags. Also `CARGO_ENCODED_RUSTFLAGS`.
    pub user_rustflags: Vec<String>,
    pub linker: Option<PathBuf>,
    pub cross: CrossToolchain,
    pub deps_dir: PathBuf,
    pub build_dir: PathBuf,
    pub incremental_dir: PathBuf,
    pub artifact_dir: PathBuf,
    /// `target.<triple>.runner`, qemu or wine or whatever.
    pub runner: Option<Vec<String>>,
}

pub struct Ctx<'a> {
    pub ws: &'a Workspace,
    pub cfg: &'a RbConfig,
    pub rustc: &'a Rustc,
    pub shell: &'a Shell,
    pub layout: Layout,
    pub host: KindInfo,
    pub targets: Vec<KindInfo>,
    pub check: bool,
    pub jobs: usize,
    pub primary: HashSet<usize>,
    pub env_config: Vec<(String, String)>,
    pub norm: Normalizer,
    pub wrapper: Option<PathBuf>,
    /// OS and host C toolchain. Build scripts can probe either, so it goes in their keys.
    pub host_fingerprint: String,
    pub cache_build_scripts: bool,
    pub no_cache_build_scripts: HashSet<String>,
    pub remap_deps: bool,
    pub codegen_backend: Option<String>,
    pub hasher: SourceHasher,
    pub store_enabled: bool,
    pub predictions: Predictions,
    pub extra_rustc: &'a [String],
    pub std_rustc: &'a [String],
    /// `--extern` that replaces sysroot `core` (`-Zbuild-std=core`).
    pub std_externs: &'a [String],
    pub extra_rustdoc: &'a [String],
}

impl Ctx<'_> {
    pub fn kind(&self, k: CompileKind) -> &KindInfo {
        match k {
            CompileKind::Host => &self.host,
            CompileKind::Target(i) => &self.targets[i],
        }
    }

    pub fn expand(&self, s: &str) -> String {
        s.replace("{TARGET}", &self.layout.target_dir.to_string_lossy())
            .replace("{WS}", &self.ws.root.to_string_lossy())
    }
}

/// What a unit turned out to depend on.
/// Outside the package, the current values go into dependents' keys. Inside, they *are* the unit.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Prediction {
    pub env: BTreeSet<String>,
    pub files: BTreeSet<String>,
    /// Package-relative. A directory means `rerun-if-changed` on that tree.
    #[serde(default)]
    pub sources: BTreeSet<String>,
    /// Latest run said `rerun-if-*`, so the rest of the package no longer counts. Not a union.
    #[serde(default)]
    pub rerun_declared: Option<bool>,
}

/// Keyed by unit name, not by content key. The union across versions stays valid, and upstream changes don't move it.
pub struct Predictions {
    dir: PathBuf,
    /// Grew this build, so dependents' keys moved.
    changed: std::sync::atomic::AtomicBool,
}

impl Predictions {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            changed: Default::default(),
        }
    }

    pub fn changed(&self) -> bool {
        self.changed.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(&name[..2]).join(format!("{name}.json"))
    }

    pub fn load(&self, name: &str) -> Option<Prediction> {
        std::fs::read(self.path(name)).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    pub fn record(&self, name: &str, add: &Prediction) {
        let mut cur = self.load(name).unwrap_or_default();
        let before = cur.clone();
        cur.env.extend(add.env.iter().cloned());
        cur.files.extend(add.files.iter().cloned());
        cur.sources.extend(add.sources.iter().cloned());
        if add.rerun_declared.is_some() {
            cur.rerun_declared = add.rerun_declared;
        }
        if cur == before {
            return;
        }
        self.changed.store(true, std::sync::atomic::Ordering::Relaxed);
        let path = self.path(name);
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec(&cur).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Planned {
    /// Changes when anything that affects the outputs changes.
    pub key: String,
    /// Identity without source contents. Stable across edits, so incremental and `OUT_DIR` survive.
    pub name: String,
    /// Current values of predicted inputs. Empty if there are none.
    pub input_digest: String,
    pub inv: Invocation,
    /// Deps dir, or `build/<pkg>-<key16>` for a build script.
    pub out_dir: PathBuf,
    pub crate_name: String,
    pub descr: &'static str,
    pub bin_name: Option<String>,
    /// This package's build-script run, if this unit compiles.
    pub own_run: Option<usize>,
    /// Build-script runs whose `-L` paths apply.
    pub to_link: Vec<usize>,
    pub lib_reqs: Vec<usize>,
    /// Needs full upstream rlibs, not just metadata.
    pub needs_link: bool,
    pub emits_meta: bool,
    pub cacheable: bool,
}

impl Planned {
    pub fn name16(&self) -> &str {
        &self.name[..16]
    }

    pub fn out_dir_path(&self) -> PathBuf {
        self.out_dir.join("out")
    }
}

fn output_file(info: &TargetInfo, crate_type: &str, crate_name: &str, name16: &str) -> Option<String> {
    let ft = info.file_type(crate_type)?;
    Some(format!("{}{}-{name16}{}", ft.prefix, crate_name, ft.suffix))
}

/// What rustc writes. Not the dep-info, not the `.o` files.
pub fn artifact_names(info: &TargetInfo, inv: &Invocation, crate_name: &str, name16: &str, emits_meta: bool) -> Vec<String> {
    let mut names = Vec::new();
    let mut args = inv.args.iter().map(|(a, _)| a.as_str());
    while let Some(a) = args.next() {
        if a == "--crate-type"
            && let Some(ct) = args.next()
            && let Some(file) = output_file(info, ct, crate_name, name16)
        {
            names.push(file);
        }
    }
    if emits_meta {
        names.push(format!("lib{crate_name}-{name16}.rmeta"));
    }
    names
}

/// What downstream LTO needs from this unit. Cargo's `lto::generate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LtoMode {
    /// This unit links with `-C lto`.
    Run,
    /// Only an LTO link consumes it, so bitcode is enough and rustc skips machine code.
    OnlyBitcode,
    /// Both LTO and normal links. rustc's default.
    ObjectAndBitcode,
    /// Nothing downstream wants bitcode.
    OnlyObject,
    /// `lto = "off"`.
    Off,
}

fn lto_crate_types(u: &Unit, ws: &Workspace) -> Vec<String> {
    if u.mode.is_test() {
        vec!["bin".into()]
    } else {
        u.target(ws).crate_types.clone()
    }
}

fn needs_object(crate_types: &[String]) -> bool {
    crate_types
        .iter()
        .any(|c| matches!(c.as_str(), "bin" | "staticlib" | "cdylib" | "dylib" | "proc-macro"))
}

fn lto_when_needs_object(crate_types: &[String]) -> LtoMode {
    // rustc can't LTO a dylib. A pure dylib needs no bitcode.
    if crate_types.iter().all(|c| c == "dylib") {
        LtoMode::OnlyObject
    } else {
        LtoMode::ObjectAndBitcode
    }
}

fn host_only_target(u: &Unit, ws: &Workspace) -> bool {
    let t = u.target(ws);
    t.kind == TargetKind::BuildScript || t.is_proc_macro()
}

pub fn lto_modes(ws: &Workspace, g: &UnitGraph) -> Vec<LtoMode> {
    let mut map: Vec<Option<LtoMode>> = vec![None; g.units.len()];
    for &r in g.roots.iter().chain(&g.aux_roots) {
        let u = &g.units[r];
        let types = lto_crate_types(u, ws);
        let root = match u.profile.lto {
            Lto::Default => LtoMode::OnlyObject,
            Lto::Off => LtoMode::Off,
            _ if host_only_target(u, ws) => LtoMode::OnlyObject,
            _ if needs_object(&types) => lto_when_needs_object(&types),
            _ => LtoMode::OnlyBitcode,
        };
        propagate_lto(ws, g, r, root, &mut map);
    }
    map.into_iter().map(|m| m.unwrap_or(LtoMode::OnlyObject)).collect()
}

fn propagate_lto(ws: &Workspace, g: &UnitGraph, ui: usize, parent: LtoMode, map: &mut [Option<LtoMode>]) {
    let u = &g.units[ui];
    let types = lto_crate_types(u, ws);
    let lto = if host_only_target(u, ws) {
        LtoMode::OnlyObject
    } else if types.iter().all(|c| matches!(c.as_str(), "bin" | "staticlib" | "cdylib")) {
        // A linked artifact isn't embedded in its parent, so it picks its own LTO.
        match u.profile.lto {
            Lto::Default => LtoMode::OnlyObject,
            Lto::Off => LtoMode::Off,
            _ => LtoMode::Run,
        }
    } else {
        match (parent, needs_object(&types)) {
            (LtoMode::Run, false) => LtoMode::OnlyBitcode,
            (LtoMode::Run | LtoMode::OnlyBitcode, true) => lto_when_needs_object(&types),
            (LtoMode::Off, _) => LtoMode::Off,
            _ => parent,
        }
    };
    // Shared by consumers that disagree: take the union.
    let merged = match map[ui] {
        None => lto,
        Some(prev) => {
            let m = match (lto, prev) {
                (a, b) if a == b => a,
                (LtoMode::Run, _) | (_, LtoMode::Run) => LtoMode::Run,
                (LtoMode::Off, _) | (_, LtoMode::Off) => LtoMode::Off,
                _ => LtoMode::ObjectAndBitcode,
            };
            if m == prev {
                return;
            }
            m
        }
    };
    map[ui] = Some(merged);
    for d in &u.deps {
        propagate_lto(ws, g, d.unit, merged, map);
    }
}

pub fn topo_order(units: &[Unit]) -> Vec<usize> {
    fn visit(u: usize, units: &[Unit], seen: &mut [u8], out: &mut Vec<usize>) {
        if seen[u] != 0 {
            return;
        }
        seen[u] = 1;
        for d in &units[u].deps {
            visit(d.unit, units, seen, out);
        }
        seen[u] = 2;
        out.push(u);
    }
    let mut seen = vec![0u8; units.len()];
    let mut out = Vec::with_capacity(units.len());
    for u in 0..units.len() {
        visit(u, units, &mut seen, &mut out);
    }
    out
}

/// Whole package, minus nested packages and `target/`. Registry and git sources are just their id.
pub fn source_identity(ctx: &Ctx<'_>, pkg: usize) -> Result<String> {
    let p = &ctx.ws.pkgs[pkg];
    Ok(match &p.source {
        Source::Registry(_) | Source::Git(_) => p.identity.clone(),
        Source::Workspace | Source::Path => format!(
            "path:{}",
            ctx.hasher
                .tree_hash(&p.root)
                .with_context(|| format!("failed to hash sources of {}", p.name))?
        ),
    })
}

/// Cargo's fingerprint. Known dep-info files, or a build script's `rerun-if-changed` paths.
/// First build, or a script that never said what it reads: the whole package.
pub fn unit_identity(ctx: &Ctx<'_>, u: &Unit, prediction: Option<&Prediction>) -> Result<String> {
    let p = &ctx.ws.pkgs[u.pkg];
    if !p.source.is_local() {
        return Ok(p.identity.clone());
    }
    // Hashed even when unused. An unchanged tree later proves the file-level ids match what we built.
    ctx.hasher
        .tree_hash(&p.root)
        .with_context(|| format!("failed to hash sources of {}", p.name))?;
    let fine_grained = match (u.mode, prediction) {
        (Mode::RunCustomBuild, Some(pr)) => pr.rerun_declared == Some(true),
        (_, Some(pr)) => !pr.sources.is_empty(),
        (_, None) => false,
    };
    if !fine_grained {
        return source_identity(ctx, u.pkg);
    }
    let mut h = blake3::Hasher::new_derive_key("rb 2026 unit sources v1");
    for rel in &prediction.unwrap().sources {
        h.update(rel.as_bytes());
        h.update(b"\0");
        // A directory hashes as a tree. A missing file hashes as absent.
        if let Some(hash) = ctx.hasher.file_hash(&p.root.join(rel))? {
            h.update(hash.as_bytes());
        }
        h.update(b"\n");
    }
    Ok(format!("files:{}", h.finalize().to_hex()))
}

/// Name id ignores file contents. Local packages are name, version, and normalized path.
fn name_identity(ctx: &Ctx<'_>, pkg: usize) -> String {
    let p = &ctx.ws.pkgs[pkg];
    match &p.source {
        Source::Registry(_) | Source::Git(_) => p.identity.clone(),
        Source::Workspace | Source::Path => format!("path:{}@{}:{}", p.name, p.version, ctx.norm.apply(&p.root.to_string_lossy())),
    }
}

fn cfg_env(info: &TargetInfo, debug_assertions: bool) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in &info.cfg {
        if k == "debug_assertions" || k == "feature" {
            continue;
        }
        let entry = map.entry(format!("CARGO_CFG_{}", k.to_uppercase())).or_default();
        if let Some(v) = v {
            entry.push(v.clone());
        }
    }
    if debug_assertions {
        map.insert("CARGO_CFG_DEBUG_ASSERTIONS".into(), Vec::new());
    }
    map.into_iter().map(|(k, v)| (k, v.join(","))).collect()
}

pub fn pkg_env(inv: &mut Invocation, ctx: &Ctx<'_>, pkg: usize) {
    let p = &ctx.ws.pkgs[pkg];
    let v = &p.version;
    inv.env_unhashed("CARGO", ctx.ws.cargo.to_string_lossy());
    inv.env("CARGO_MANIFEST_DIR", p.root.to_string_lossy());
    inv.env("CARGO_MANIFEST_PATH", p.manifest_path.to_string_lossy());
    inv.env("CARGO_PKG_NAME", &p.name);
    inv.env("CARGO_PKG_VERSION", v.to_string());
    inv.env("CARGO_PKG_VERSION_MAJOR", v.major.to_string());
    inv.env("CARGO_PKG_VERSION_MINOR", v.minor.to_string());
    inv.env("CARGO_PKG_VERSION_PATCH", v.patch.to_string());
    inv.env("CARGO_PKG_VERSION_PRE", v.pre.to_string());
    // Unhashed on purpose. A description or README change shouldn't rebuild the world; `env!` shows up in dep-info.
    inv.env_unhashed("CARGO_PKG_AUTHORS", p.authors.join(":"));
    inv.env_unhashed("CARGO_PKG_DESCRIPTION", p.description.clone().unwrap_or_default());
    inv.env_unhashed("CARGO_PKG_HOMEPAGE", p.homepage.clone().unwrap_or_default());
    inv.env_unhashed("CARGO_PKG_REPOSITORY", p.repository.clone().unwrap_or_default());
    inv.env_unhashed("CARGO_PKG_LICENSE", p.license.clone().unwrap_or_default());
    inv.env_unhashed("CARGO_PKG_LICENSE_FILE", p.license_file.clone().unwrap_or_default());
    inv.env_unhashed("CARGO_PKG_RUST_VERSION", p.rust_version.clone().unwrap_or_default());
    inv.env_unhashed("CARGO_PKG_README", p.readme.clone().unwrap_or_default());
    for (k, v) in &ctx.env_config {
        inv.env(k, v);
    }
}

fn collect_to_link(u: usize, units: &[Unit], ws: &Workspace, memo: &mut HashMap<usize, Vec<usize>>) -> Vec<usize> {
    if let Some(v) = memo.get(&u) {
        return v.clone();
    }
    let mut out = Vec::new();
    for d in &units[u].deps {
        let du = &units[d.unit];
        match du.mode {
            Mode::RunCustomBuild => out.push(d.unit),
            _ if du.target(ws).is_proc_macro() => {}
            _ if d.extern_name.is_some() => out.extend(collect_to_link(d.unit, units, ws, memo)),
            _ => {}
        }
    }
    out.sort_unstable();
    out.dedup();
    memo.insert(u, out.clone());
    out
}

fn collect_lib_reqs(u: usize, units: &[Unit], ws: &Workspace, memo: &mut HashMap<usize, Vec<usize>>) -> Vec<usize> {
    if let Some(v) = memo.get(&u) {
        return v.clone();
    }
    let mut out = Vec::new();
    for d in &units[u].deps {
        if d.extern_name.is_none() {
            continue;
        }
        out.push(d.unit);
        if !units[d.unit].target(ws).is_proc_macro() {
            out.extend(collect_lib_reqs(d.unit, units, ws, memo));
        }
    }
    out.sort_unstable();
    out.dedup();
    memo.insert(u, out.clone());
    out
}

pub fn plan(ctx: &Ctx<'_>, graph: &UnitGraph) -> Result<Vec<Planned>> {
    let units = &graph.units;
    let order = topo_order(units);
    let lto = lto_modes(ctx.ws, graph);
    let target_dir = &ctx.layout.target_dir;
    let target_roots: Vec<PathBuf> = std::iter::once(target_dir.clone())
        .chain(std::fs::canonicalize(target_dir).ok())
        .collect();
    let mut planned: Vec<Option<Planned>> = vec![None; units.len()];
    let mut lints_cache: HashMap<usize, Lints> = HashMap::new();
    let mut to_link_memo = HashMap::new();
    let mut lib_memo = HashMap::new();
    for &ui in &order {
        let u = &units[ui];
        let mut p = match u.mode {
            Mode::RunCustomBuild => plan_run(ctx, ui, units, &planned)?,
            _ => {
                let pkg = &ctx.ws.pkgs[u.pkg];
                let lints = lints_cache
                    .entry(u.pkg)
                    .or_insert_with(|| crate::lints::from_table(pkg.lints.as_ref()));
                if u.mode == Mode::Doc {
                    plan_doc(ctx, ui, units, &planned, lints)?
                } else {
                    plan_compile(ctx, ui, units, &planned, Some(lints), lto[ui])?
                }
            }
        };
        p.to_link = collect_to_link(ui, units, ctx.ws, &mut to_link_memo);
        p.lib_reqs = collect_lib_reqs(ui, units, ctx.ws, &mut lib_memo);

        // Name first: it picks the prediction that decides which sources the key covers.
        // The key then adds those contents and the dependencies' keys. The name does not.
        let mut inv_hash = blake3::Hasher::new();
        p.inv.hash_into(&mut inv_hash, &ctx.norm);
        let inv_hash = inv_hash.finalize();
        let mut h = blake3::Hasher::new_derive_key("rb 2026 unit key v2");
        let mut n = blake3::Hasher::new_derive_key("rb 2026 unit name v1");
        for x in [&mut h, &mut n] {
            x.update(ctx.rustc.id().as_bytes());
            x.update(format!("{:?}", u.mode).as_bytes());
            x.update(inv_hash.as_bytes());
            if let CompileKind::Target(_) = u.kind {
                x.update(ctx.kind(u.kind).cross.fingerprint.as_bytes());
            }
            if u.mode == Mode::RunCustomBuild {
                x.update(ctx.host_fingerprint.as_bytes());
            }
        }
        n.update(name_identity(ctx, u.pkg).as_bytes());
        for d in &u.deps {
            let dp = planned[d.unit].as_ref().unwrap();
            n.update(b"\0dep\0");
            n.update(d.extern_name.as_deref().unwrap_or("").as_bytes());
            n.update(dp.name.as_bytes());
        }
        let name = n.finalize().to_hex().to_string();
        let prediction = ctx.predictions.load(&name);
        h.update(unit_identity(ctx, u, prediction.as_ref())?.as_bytes());
        for d in &u.deps {
            let dp = planned[d.unit].as_ref().unwrap();
            h.update(b"\0dep\0");
            h.update(d.extern_name.as_deref().unwrap_or("").as_bytes());
            h.update(dp.key.as_bytes());
            h.update(dp.input_digest.as_bytes());
        }
        let key = h.finalize().to_hex().to_string();
        // Variants of this unit are told apart at lookup. Dependents bake the current values into their own keys.
        if let Some(pr) = prediction.as_ref().filter(|pr| !pr.env.is_empty() || !pr.files.is_empty()) {
            let mut h = blake3::Hasher::new_derive_key("rb 2026 predicted inputs v1");
            for name in &pr.env {
                let value = p.inv.get_env(name).map(str::to_owned).or_else(|| std::env::var(name).ok());
                h.update(format!("\0env\0{name}={value:?}").as_bytes());
            }
            for f in &pr.files {
                let path = PathBuf::from(ctx.expand(f));
                // A build output is not an input. Old predictions still name some, and a fresh target dir doesn't have them yet.
                if target_roots.iter().any(|r| path.starts_with(r)) {
                    continue;
                }
                let hash = ctx.hasher.file_hash(&path)?;
                h.update(format!("\0file\0{f}={hash:?}").as_bytes());
            }
            p.input_digest = h.finalize().to_hex().to_string();
        }
        // Opt out one build script and everything downstream of it stays project-local too.
        p.cacheable &= u.deps.iter().all(|d| planned[d.unit].as_ref().unwrap().cacheable);
        p.inv.substitute_name(&name);
        p.out_dir = PathBuf::from(p.out_dir.to_string_lossy().replace(SELF_NAME16, &name[..16]));
        p.key = key;
        p.name = name;
        planned[ui] = Some(p);
    }
    Ok(planned.into_iter().map(Option::unwrap).collect())
}

fn program(ctx: &Ctx<'_>, inv: &mut Invocation) {
    if let Some(w) = &ctx.wrapper {
        inv.program = w.clone();
        inv.arg_unhashed(ctx.rustc.path.to_string_lossy());
    }
}

fn plan_compile(
    ctx: &Ctx<'_>,
    ui: usize,
    units: &[Unit],
    planned: &[Option<Planned>],
    lints: Option<&Lints>,
    lto: LtoMode,
) -> Result<Planned> {
    let u = &units[ui];
    let ws = ctx.ws;
    let p = &ws.pkgs[u.pkg];
    let t = &p.targets[u.target];
    let k = ctx.kind(u.kind);
    let prof = &u.profile;
    let crate_name = t.crate_name();
    let is_bs = t.kind == TargetKind::BuildScript;
    let local = p.source.is_local();
    let (src, cwd) = match t.src_path.strip_prefix(&ws.root) {
        Ok(rel) if local => (rel.to_path_buf(), ws.root.clone()),
        _ => (t.src_path.clone(), p.root.clone()),
    };
    let mut inv = Invocation::new(&ctx.rustc.path, cwd);
    program(ctx, &mut inv);
    inv.arg("--crate-name").arg(&crate_name);
    inv.arg(format!("--edition={}", t.edition));
    inv.arg(src.to_string_lossy());
    if local && ctx.shell.is_verbose() {
        inv.arg_unhashed("--verbose");
    }
    inv.arg_unhashed("--error-format=json");
    inv.arg_unhashed(format!("--json={}", ctx.shell.rustc_json()));
    let test = u.mode.is_test();
    let crate_types: Vec<String> = match t.kind {
        _ if test => vec!["bin".into()],
        TargetKind::Lib | TargetKind::Example => t.crate_types.clone(),
        _ => vec!["bin".into()],
    };
    if test && t.harness {
        inv.arg("--test");
    } else if test {
        inv.arg("--cfg").arg("test");
    } else {
        for ct in &crate_types {
            inv.arg("--crate-type").arg(ct);
        }
    }
    let check = u.mode.is_check();
    let produces_rlib = !test && crate_types.iter().any(|c| c == "lib" || c == "rlib");
    // Cargo skips the metadata file when there's also a cdylib or staticlib.
    // Writing it made risuko's first incremental rebuild 8.5s against cargo's 6s.
    let metadata_emit = produces_rlib && crate_types.iter().all(|c| c == "lib" || c == "rlib");
    let linkable = crate_types
        .iter()
        .any(|c| matches!(c.as_str(), "bin" | "dylib" | "cdylib" | "staticlib"));
    let needs_link = !check && (linkable || t.is_proc_macro());
    let emits_meta = check || metadata_emit;
    inv.arg(format!(
        "--emit={}",
        if check {
            "dep-info,metadata"
        } else if metadata_emit {
            "dep-info,metadata,link"
        } else {
            "dep-info,link"
        }
    ));
    if prof.opt_level != "0" {
        inv.arg("-C").arg(format!("opt-level={}", prof.opt_level));
    }
    if prof.panic == "abort" && !u.for_host {
        inv.arg("-C").arg("panic=abort");
    }
    match lto {
        LtoMode::Run => {
            inv.arg("-C").arg(match prof.lto {
                Lto::Thin => "lto=thin",
                Lto::Fat => "lto=fat",
                _ => "lto",
            });
        }
        LtoMode::Off => {
            inv.arg("-C").arg("lto=off");
        }
        LtoMode::ObjectAndBitcode => {}
        LtoMode::OnlyBitcode => {
            inv.arg("-C").arg("linker-plugin-lto");
        }
        LtoMode::OnlyObject => {
            inv.arg("-C").arg("embed-bitcode=no");
        }
    }
    if let Some(cgu) = prof.codegen_units {
        inv.arg("-C").arg(format!("codegen-units={cgu}"));
    }
    if prof.debuginfo_on() {
        inv.arg("-C").arg(format!("debuginfo={}", prof.debuginfo));
        if let Some(split) = &prof.split_debuginfo {
            let supported = k.info.is_apple() || k.info.cfg_value("target_os") == Some("linux") || split == "packed";
            if supported {
                inv.arg("-C").arg(format!("split-debuginfo={split}"));
            }
        }
    }
    let opt0 = prof.opt_level == "0";
    if !opt0 {
        if prof.debug_assertions {
            inv.args(["-C", "debug-assertions=on"]);
            if !prof.overflow_checks {
                inv.args(["-C", "overflow-checks=off"]);
            }
        } else if prof.overflow_checks {
            inv.args(["-C", "overflow-checks=on"]);
        }
    } else if !prof.debug_assertions {
        inv.args(["-C", "debug-assertions=off"]);
        if prof.overflow_checks {
            inv.args(["-C", "overflow-checks=on"]);
        }
    } else if !prof.overflow_checks {
        inv.args(["-C", "overflow-checks=off"]);
    }
    if let Some(strip) = &prof.strip {
        inv.arg("-C").arg(format!("strip={strip}"));
    }
    if prof.rpath {
        inv.args(["-C", "rpath"]);
    }
    if t.is_proc_macro() || crate_types.iter().any(|c| c == "dylib") {
        inv.args(["-C", "prefer-dynamic"]);
    }
    let backend = prof
        .codegen_backend
        .clone()
        .or_else(|| ctx.codegen_backend.clone().filter(|_| ctx.rustc.nightly && prof.name == "dev"));
    if let Some(b) = backend.filter(|b| b != "llvm") {
        inv.arg(format!("-Zcodegen-backend={b}"));
    }
    for f in &u.features {
        inv.arg("--cfg").arg(format!("feature=\"{f}\""));
    }
    if let Some(l) = lints {
        inv.args(l.flags.iter().cloned());
    }
    inv.arg("--check-cfg").arg("cfg(docsrs,test)");
    let mut declared = p.declared_features.clone();
    declared.sort();
    let values: Vec<String> = declared.iter().map(|f| format!("\"{f}\"")).collect();
    inv.arg("--check-cfg").arg(format!("cfg(feature, values({}))", values.join(", ")));
    if let Some(l) = lints {
        for cc in &l.check_cfg {
            inv.arg("--check-cfg").arg(cc);
        }
    }
    // cargo's `-C metadata` is a u64. The full name is 64 hex chars and lands in every symbol.
    inv.arg("-C").arg(format!("metadata={SELF_NAME16}"));
    inv.arg("-C").arg(format!("extra-filename=-{SELF_NAME16}"));
    let out_dir = if is_bs {
        k.build_dir.join(format!("{}-{SELF_NAME16}", p.name))
    } else {
        k.deps_dir.clone()
    };
    inv.arg_unhashed("--out-dir").arg_unhashed(out_dir.to_string_lossy());
    if prof.incremental && local {
        // One dir per unit, so a variant nobody uses can be swept.
        let dir = k.incremental_dir.join(format!("{crate_name}-{SELF_NAME16}"));
        inv.arg_unhashed("-C").arg_unhashed(format!("incremental={}", dir.display()));
    }
    if let Some(tf) = &k.target_flag {
        inv.arg("--target").arg(tf);
        if tf.ends_with(".json") {
            inv.arg("-Zunstable-options");
        }
    }
    inv.arg_unhashed("-L").arg_unhashed(format!("dependency={}", k.deps_dir.display()));
    if u.kind != CompileKind::Host {
        inv.arg_unhashed("-L")
            .arg_unhashed(format!("dependency={}", ctx.host.deps_dir.display()));
    }
    let mut externs: Vec<(String, String)> = Vec::new();
    let mut bin_exes: Vec<(String, String)> = Vec::new();
    for d in &u.deps {
        let du = &units[d.unit];
        let dp = planned[d.unit].as_ref().unwrap();
        match &d.extern_name {
            Some(name) => {
                let path = dep_output_path(ctx, du, dp, !needs_link && dp.emits_meta)
                    .with_context(|| format!("while resolving `--extern {name}`"))?;
                externs.push((name.clone(), path.to_string_lossy().into_owned()));
            }
            None if du.mode == Mode::Build && du.target(ws).kind == TargetKind::Bin => {
                bin_exes.push((du.target(ws).name.clone(), uplifted_bin(ctx, du).to_string_lossy().into_owned()));
            }
            None => {}
        }
    }
    externs.sort();
    for (name, path) in externs {
        inv.arg("--extern").arg(format!("{name}={path}"));
    }
    if t.is_proc_macro() {
        inv.arg("--extern").arg("proc_macro");
    }
    if !local {
        inv.arg("--cap-lints").arg("allow");
        if ctx.remap_deps
            && let Some(parent) = p.root.parent()
        {
            inv.arg(format!("--remap-path-prefix={}=/rust/deps", parent.display()));
        }
    }
    if let Some(l) = &k.linker {
        inv.arg("-C").arg(format!("linker={}", l.display()));
    }
    inv.args(k.user_rustflags.iter().cloned());
    inv.args(ctx.std_rustc.iter().cloned());
    inv.args(k.cross.rustflags.iter().cloned());
    inv.args(prof.rustflags.iter().cloned());
    inv.path_prepend = k.cross.path_prepend.clone();
    for (key, value) in &k.cross.rustc_env {
        inv.env_unhashed(key, value);
    }

    pkg_env(&mut inv, ctx, u.pkg);
    inv.env("CARGO_CRATE_NAME", &crate_name);
    if t.kind == TargetKind::Bin {
        inv.env("CARGO_BIN_NAME", &t.name);
    }
    for (name, path) in bin_exes {
        inv.env(format!("CARGO_BIN_EXE_{name}"), path);
    }
    if matches!(t.kind, TargetKind::Test | TargetKind::Bench) {
        inv.env("CARGO_TARGET_TMPDIR", ctx.layout.target_dir.join("tmp").to_string_lossy());
    }
    if ctx.primary.contains(&u.pkg) && p.is_member {
        inv.env_unhashed("CARGO_PRIMARY_PACKAGE", "1");
    }
    if !is_bs && ctx.primary.contains(&u.pkg) {
        for arg in ctx.extra_rustc {
            inv.arg(arg);
        }
    }
    if !is_bs && !t.is_proc_macro() {
        inv.args(ctx.std_externs.iter().cloned());
    }
    let own_run = u
        .deps
        .iter()
        .map(|d| d.unit)
        .find(|&d| units[d].mode == Mode::RunCustomBuild && units[d].pkg == u.pkg);
    if let Some(r) = own_run {
        let rp = planned[r].as_ref().unwrap();
        inv.env_unhashed("OUT_DIR", rp.out_dir_path().to_string_lossy());
    }
    Ok(Planned {
        key: String::new(),
        name: String::new(),
        input_digest: String::new(),
        inv,
        out_dir,
        crate_name,
        descr: match (t.kind, test) {
            (TargetKind::Lib, true) => "(lib test)",
            (TargetKind::Bin, true) => "(bin test)",
            (TargetKind::Lib, false) if t.is_proc_macro() => "(proc-macro)",
            (TargetKind::Lib, false) => "(lib)",
            (TargetKind::Bin, false) => "(bin)",
            (TargetKind::Example, _) => "(example)",
            (TargetKind::Test, _) => "(test)",
            (TargetKind::Bench, _) => "(bench)",
            (TargetKind::BuildScript, _) => "(build script)",
        },
        bin_name: (matches!(t.kind, TargetKind::Bin | TargetKind::Example) && !test).then(|| t.name.clone()),
        own_run,
        to_link: Vec::new(),
        lib_reqs: Vec::new(),
        needs_link,
        emits_meta,
        cacheable: true,
    })
}

/// What `--extern` points at: `.rmeta` when that's enough, otherwise the rlib or proc-macro dylib.
pub fn dep_output_path(ctx: &Ctx<'_>, du: &Unit, dp: &Planned, rmeta_ok: bool) -> Result<PathBuf> {
    let dt = du.target(ctx.ws);
    let dk = ctx.kind(du.kind);
    let file = if dt.is_proc_macro() {
        output_file(&ctx.host.info, "proc-macro", &dp.crate_name, dp.name16())
    } else if rmeta_ok || du.mode.is_check() {
        Some(format!("lib{}-{}.rmeta", dp.crate_name, dp.name16()))
    } else if dt.crate_types.iter().any(|c| c == "lib" || c == "rlib") {
        output_file(&dk.info, "rlib", &dp.crate_name, dp.name16())
    } else {
        output_file(&dk.info, "dylib", &dp.crate_name, dp.name16())
    };
    let file = file.with_context(|| format!("target {} cannot produce the crate type of `{}`", dk.triple, dp.crate_name))?;
    Ok(dk.deps_dir.join(file))
}

/// Where the bin lands after uplift. Also what `CARGO_BIN_EXE_<name>` points at.
pub fn uplifted_bin(ctx: &Ctx<'_>, u: &Unit) -> PathBuf {
    let k = ctx.kind(u.kind);
    let suffix = k.info.file_type("bin").map(|f| f.suffix.clone()).unwrap_or_default();
    k.artifact_dir.join(format!("{}{suffix}", u.target(ctx.ws).name))
}

fn plan_doc(ctx: &Ctx<'_>, ui: usize, units: &[Unit], planned: &[Option<Planned>], lints: &Lints) -> Result<Planned> {
    let u = &units[ui];
    let ws = ctx.ws;
    let p = &ws.pkgs[u.pkg];
    let t = &p.targets[u.target];
    let k = ctx.kind(u.kind);
    let crate_name = t.crate_name();
    let local = p.source.is_local();
    let (src, cwd) = match t.src_path.strip_prefix(&ws.root) {
        Ok(rel) if local => (rel.to_path_buf(), ws.root.clone()),
        _ => (t.src_path.clone(), p.root.clone()),
    };
    let rustdoc = ctx.rustc.path.with_file_name(format!("rustdoc{}", std::env::consts::EXE_SUFFIX));
    let mut inv = Invocation::new(rustdoc, cwd);
    inv.arg(format!("--edition={}", t.edition));
    let crate_types = if t.kind == TargetKind::Lib {
        t.crate_types.clone()
    } else {
        vec!["bin".into()]
    };
    for ct in &crate_types {
        inv.arg("--crate-type").arg(ct);
    }
    inv.arg("--crate-name").arg(&crate_name);
    inv.arg(src.to_string_lossy());
    let doc_dir = ctx.layout.doc_dir(k.target_flag.as_ref().map(|_| k.triple.as_str()));
    inv.arg_unhashed("-o").arg_unhashed(doc_dir.to_string_lossy());
    inv.arg_unhashed("--error-format=json");
    if ctx.shell.color() {
        inv.arg_unhashed("--json=diagnostic-rendered-ansi");
    }
    for f in &u.features {
        inv.arg("--cfg").arg(format!("feature=\"{f}\""));
    }
    inv.args(lints.flags.iter().cloned());
    inv.arg("--check-cfg").arg("cfg(docsrs,test)");
    let mut declared = p.declared_features.clone();
    declared.sort();
    let values: Vec<String> = declared.iter().map(|f| format!("\"{f}\"")).collect();
    inv.arg("--check-cfg").arg(format!("cfg(feature, values({}))", values.join(", ")));
    for cc in &lints.check_cfg {
        inv.arg("--check-cfg").arg(cc);
    }
    inv.arg_unhashed("-C").arg_unhashed(format!("metadata={SELF_NAME16}"));
    if let Some(tf) = &k.target_flag {
        inv.arg("--target").arg(tf);
        if tf.ends_with(".json") {
            inv.arg("-Zunstable-options");
        }
    }
    inv.arg_unhashed("-L").arg_unhashed(format!("dependency={}", k.deps_dir.display()));
    if u.kind != CompileKind::Host {
        inv.arg_unhashed("-L")
            .arg_unhashed(format!("dependency={}", ctx.host.deps_dir.display()));
    }
    let mut externs = Vec::new();
    for d in &u.deps {
        let Some(name) = &d.extern_name else { continue };
        let path = dep_output_path(ctx, &units[d.unit], planned[d.unit].as_ref().unwrap(), true)?;
        externs.push(format!("{name}={}", path.display()));
    }
    externs.sort();
    for e in externs {
        inv.arg("--extern").arg(e);
    }
    if t.is_proc_macro() {
        inv.arg("--extern").arg("proc_macro");
    }
    inv.arg("--crate-version").arg(p.version.to_string());
    if !local {
        inv.arg("--cap-lints").arg("allow");
    }
    if t.kind == TargetKind::Bin {
        inv.arg("--document-private-items").arg("-Arustdoc::private-intra-doc-links");
    }
    pkg_env(&mut inv, ctx, u.pkg);
    inv.env("CARGO_CRATE_NAME", &crate_name);
    let own_run = u
        .deps
        .iter()
        .map(|d| d.unit)
        .find(|&d| units[d].mode == Mode::RunCustomBuild && units[d].pkg == u.pkg);
    if let Some(r) = own_run {
        inv.env_unhashed("OUT_DIR", planned[r].as_ref().unwrap().out_dir_path().to_string_lossy());
    }
    if ctx.primary.contains(&u.pkg) {
        for arg in ctx.extra_rustdoc {
            inv.arg(arg);
        }
    }
    Ok(Planned {
        key: String::new(),
        name: String::new(),
        input_digest: String::new(),
        inv,
        out_dir: doc_dir,
        crate_name,
        descr: "(doc)",
        bin_name: None,
        own_run,
        to_link: Vec::new(),
        lib_reqs: Vec::new(),
        needs_link: false,
        emits_meta: false,
        // One doc tree for every crate. It stays in the project.
        cacheable: false,
    })
}

fn plan_run(ctx: &Ctx<'_>, ui: usize, units: &[Unit], planned: &[Option<Planned>]) -> Result<Planned> {
    let u = &units[ui];
    let p = &ctx.ws.pkgs[u.pkg];
    let k = ctx.kind(u.kind);
    let prof = &u.profile;
    let script_unit = u
        .deps
        .iter()
        .map(|d| d.unit)
        .find(|&d| units[d].mode != Mode::RunCustomBuild && units[d].pkg == u.pkg)
        .context("build script run without its compiled build script")?;
    let sp = planned[script_unit].as_ref().unwrap();
    let exe = sp
        .out_dir
        .join(format!("{}-{}{}", sp.crate_name, sp.name16(), std::env::consts::EXE_SUFFIX));
    let out_dir = k.build_dir.join(format!("{}-{SELF_NAME16}", p.name));
    let mut inv = Invocation::new(exe, p.root.clone());
    pkg_env(&mut inv, ctx, u.pkg);
    if let Some(links) = &p.links {
        inv.env("CARGO_MANIFEST_LINKS", links);
    }
    for f in &u.features {
        inv.env(format!("CARGO_FEATURE_{}", crate::build_script::env_name(f)), "1");
    }
    for (name, value) in cfg_env(&k.info, prof.debug_assertions) {
        inv.env(name, value);
    }
    inv.env_unhashed("OUT_DIR", out_dir.join("out").to_string_lossy());
    inv.env("TARGET", &k.triple);
    inv.env("HOST", &ctx.rustc.host);
    inv.env_unhashed("NUM_JOBS", ctx.jobs.to_string());
    inv.env("OPT_LEVEL", &prof.opt_level);
    inv.env("DEBUG", if prof.debuginfo_on() { "true" } else { "false" });
    inv.env("PROFILE", if prof.root == "release" { "release" } else { "debug" });
    inv.env_unhashed("RUSTC", ctx.rustc.path.to_string_lossy());
    inv.env_unhashed(
        "RUSTDOC",
        ctx.rustc
            .path
            .with_file_name(format!("rustdoc{}", std::env::consts::EXE_SUFFIX))
            .to_string_lossy(),
    );
    if let Some(l) = &k.linker {
        inv.env("RUSTC_LINKER", l.to_string_lossy());
    }
    let mut encoded = k.user_rustflags.clone();
    encoded.extend(k.cross.rustflags.iter().cloned());
    inv.env("CARGO_ENCODED_RUSTFLAGS", encoded.join("\x1f"));
    for (key, value) in &k.cross.env {
        inv.env(key, value);
    }
    inv.path_prepend = k.cross.path_prepend.clone();
    let cacheable = ctx.cache_build_scripts && !ctx.no_cache_build_scripts.contains(&p.name);
    Ok(Planned {
        key: String::new(),
        name: String::new(),
        input_digest: String::new(),
        inv,
        out_dir,
        crate_name: sp.crate_name.clone(),
        descr: "(build script run)",
        bin_name: None,
        own_run: None,
        to_link: Vec::new(),
        lib_reqs: Vec::new(),
        needs_link: false,
        emits_meta: false,
        cacheable,
    })
}

pub fn host_fingerprint(cache_dir: &Path) -> String {
    let path = cache_dir.join("host-fingerprint.txt");
    let fresh = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < 3600);
    if fresh && let Ok(s) = std::fs::read_to_string(&path) {
        return s;
    }
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").trim().to_owned())
            .unwrap_or_default()
    };
    let mut parts = vec![run("uname", &["-srm"]), run("cc", &["--version"])];
    if cfg!(target_os = "macos") {
        parts.push(run("xcrun", &["--show-sdk-version"]));
    }
    let fp = parts.join(" | ");
    let _ = std::fs::create_dir_all(cache_dir);
    let _ = std::fs::write(&path, &fp);
    fp
}
