//! One node per (package, target, platform, mode, features). Same rules as cargo's `unit_dependencies`.

use crate::cfgexpr::PlatformCfg;
use crate::features::FeatureInfo;
use crate::manifest::DepKind;
use crate::profile::{Profile, Profiles};
use crate::workspace::{Resolution, Source, TargetKind, Workspace, dep_enabled};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Where it compiles. No `--target` means everything is `Host`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CompileKind {
    Host,
    Target(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Build,
    Check,
    CheckTest,
    /// `--test` with a harness, `--cfg test` without.
    Test,
    Bench,
    Doc,
    RunCustomBuild,
}

impl Mode {
    pub fn is_test(self) -> bool {
        matches!(self, Self::Test | Self::Bench | Self::CheckTest)
    }

    pub fn is_check(self) -> bool {
        matches!(self, Self::Check | Self::CheckTest)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Check | Self::CheckTest => "check",
            Self::Test => "test",
            Self::Bench => "bench",
            Self::Doc => "doc",
            Self::RunCustomBuild => "run-custom-build",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Build,
    Check,
    Test,
    Bench,
    Doc { deps: bool },
    Run,
}

#[derive(Clone, Debug)]
pub struct UnitDep {
    pub unit: usize,
    /// `--extern` name for a library dependency.
    pub extern_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Unit {
    pub pkg: usize,
    pub target: usize,
    pub kind: CompileKind,
    /// Build scripts, proc-macros, and whatever they depend on.
    pub for_host: bool,
    pub mode: Mode,
    pub features: Vec<String>,
    pub profile: Profile,
    pub deps: Vec<UnitDep>,
}

impl Unit {
    pub fn target<'a>(&self, ws: &'a Workspace) -> &'a crate::workspace::Target {
        &ws.pkgs[self.pkg].targets[self.target]
    }
}

pub struct UnitGraph {
    pub units: Vec<Unit>,
    pub roots: Vec<usize>,
    /// Has to be on disk, but isn't an artifact of the command. Doctest inputs.
    pub aux_roots: Vec<usize>,
    pub doctests: Vec<Doctest>,
}

#[derive(Clone, Debug)]
pub struct Doctest {
    pub pkg: usize,
    pub lib: usize,
    pub dev_deps: Vec<UnitDep>,
}

#[derive(Default, Clone, Debug)]
pub struct TargetFilter {
    pub lib: bool,
    pub bins: bool,
    pub bin_names: Vec<String>,
    pub examples: bool,
    pub example_names: Vec<String>,
    pub tests: bool,
    pub test_names: Vec<String>,
    pub benches: bool,
    pub bench_names: Vec<String>,
    pub all_targets: bool,
    pub doc: bool,
}

impl TargetFilter {
    pub fn explicit(&self) -> bool {
        self.lib
            || self.bins
            || !self.bin_names.is_empty()
            || self.examples
            || !self.example_names.is_empty()
            || self.tests
            || !self.test_names.is_empty()
            || self.benches
            || !self.bench_names.is_empty()
            || self.all_targets
            || self.doc
    }

    pub fn needs_dev_deps(&self, command: Command) -> bool {
        matches!(command, Command::Test | Command::Bench)
            || self.all_targets
            || self.examples
            || self.tests
            || self.benches
            || !self.example_names.is_empty()
            || !self.test_names.is_empty()
            || !self.bench_names.is_empty()
    }
}

pub struct RootSelection<'a> {
    pub packages: &'a [usize],
    pub command: Command,
    pub filter: &'a TargetFilter,
}

pub struct GraphInputs<'a> {
    pub ws: &'a Workspace,
    pub res: &'a Resolution,
    pub profiles: &'a Profiles,
    pub host_platform: &'a PlatformCfg<'a>,
    pub target_platforms: &'a [PlatformCfg<'a>],
    pub host_apple: bool,
    pub target_apple: &'a [bool],
    pub no_target: bool,
    pub command: Command,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Identity {
    pkg: usize,
    target: usize,
    kind: CompileKind,
    for_host: bool,
    mode: Mode,
    feature_side_host: bool,
}

struct Builder<'a> {
    i: &'a GraphInputs<'a>,
    units: Vec<Unit>,
    memo: HashMap<Identity, usize>,
    doc_deps: bool,
}

static EMPTY: std::sync::LazyLock<FeatureInfo> = std::sync::LazyLock::new(FeatureInfo::default);

impl<'a> Builder<'a> {
    fn features(&self, pkg: usize, host_side: bool) -> &'a FeatureInfo {
        self.i.res.info(pkg, host_side).unwrap_or(&EMPTY)
    }

    fn platform(&self, kind: CompileKind) -> &'a PlatformCfg<'a> {
        match kind {
            CompileKind::Host => self.i.host_platform,
            CompileKind::Target(t) => &self.i.target_platforms[t],
        }
    }

    fn apple(&self, kind: CompileKind) -> bool {
        match kind {
            CompileKind::Host => self.i.host_apple,
            CompileKind::Target(t) => self.i.target_apple[t],
        }
    }

    fn intern(&mut self, id: Identity, profile: Profile) -> (usize, bool) {
        if let Some(&u) = self.memo.get(&id) {
            return (u, false);
        }
        let features = self.features(id.pkg, id.feature_side_host).named.iter().cloned().collect();
        let idx = self.units.len();
        self.units.push(Unit {
            pkg: id.pkg,
            target: id.target,
            kind: id.kind,
            for_host: id.for_host,
            mode: id.mode,
            features,
            profile,
            deps: Vec::new(),
        });
        self.memo.insert(id, idx);
        (idx, true)
    }

    fn unit(&mut self, pkg: usize, target: usize, kind: CompileKind, for_host: bool, mode: Mode) -> Result<usize> {
        let ws = self.i.ws;
        let p = &ws.pkgs[pkg];
        let t = &p.targets[target];
        let mut profile = self.i.profiles.get(p, for_host, self.apple(kind));
        if mode.is_test() {
            profile.panic = "unwind".into();
        }
        let (u, new) = self.intern(
            Identity {
                pkg,
                target,
                kind,
                for_host,
                mode,
                feature_side_host: for_host,
            },
            profile,
        );
        if !new {
            return Ok(u);
        }
        let dev = mode.is_test() || matches!(t.kind, TargetKind::Example | TargetKind::Test | TargetKind::Bench);
        let mut deps = self.lib_deps(pkg, kind, for_host, DepKind::Normal, mode)?;
        if dev {
            deps.extend(self.lib_deps(pkg, kind, for_host, DepKind::Dev, mode)?);
        }
        // `rb doc` documents each dependency before its dependents, so cross-crate links resolve.
        if mode == Mode::Doc && self.doc_deps {
            let libs: Vec<Unit> = deps.iter().map(|d| self.units[d.unit].clone()).collect();
            for du in libs {
                if ws.pkgs[du.pkg].targets[du.target].doc {
                    let d = self.unit(du.pkg, du.target, du.kind, du.for_host, Mode::Doc)?;
                    deps.push(UnitDep {
                        unit: d,
                        extern_name: None,
                    });
                }
            }
        }
        if p.build_script().is_some() {
            deps.push(UnitDep {
                unit: self.run_unit(pkg, kind, for_host)?,
                extern_name: None,
            });
        }
        let is_lib_under_test = t.kind == TargetKind::Lib;
        if !is_lib_under_test && let Some(lib) = p.lib() {
            let lib_mode = if mode.is_check() || mode == Mode::Doc {
                Mode::Check
            } else {
                Mode::Build
            };
            let lib_host = for_host || p.proc_macro;
            let lib_kind = if p.proc_macro { CompileKind::Host } else { kind };
            let lib_unit = self.unit(pkg, lib, lib_kind, lib_host, if p.proc_macro { Mode::Build } else { lib_mode })?;
            deps.push(UnitDep {
                unit: lib_unit,
                extern_name: Some(p.targets[lib].crate_name()),
            });
            if mode == Mode::Doc {
                let lib_doc = self.unit(pkg, lib, lib_kind, lib_host, Mode::Doc)?;
                deps.push(UnitDep {
                    unit: lib_doc,
                    extern_name: None,
                });
            }
        }
        // Integration tests and benches can launch the package's bins.
        if matches!(t.kind, TargetKind::Test | TargetKind::Bench) && matches!(mode, Mode::Test | Mode::Bench | Mode::Build) {
            for (bi, bt) in p.targets.iter().enumerate() {
                if bt.kind == TargetKind::Bin && self.required_features_ok(pkg, bi, for_host).is_ok() {
                    let b = self.unit(pkg, bi, kind, for_host, Mode::Build)?;
                    deps.push(UnitDep {
                        unit: b,
                        extern_name: None,
                    });
                }
            }
        }
        self.units[u].deps = deps;
        Ok(u)
    }

    /// Metadata only, unless the parent is a proc-macro. Those have to be executable.
    fn lib_unit(&mut self, pkg: usize, kind: CompileKind, for_host: bool, parent: Mode) -> Result<Option<usize>> {
        let p = &self.i.ws.pkgs[pkg];
        let Some(lib) = p.lib() else { return Ok(None) };
        let (kind, for_host) = if p.proc_macro {
            (CompileKind::Host, true)
        } else {
            (kind, for_host)
        };
        let check = !p.proc_macro && (parent.is_check() || parent == Mode::Doc);
        self.unit(pkg, lib, kind, for_host, if check { Mode::Check } else { Mode::Build })
            .map(Some)
    }

    fn enabled_deps(&self, pkg: usize, kind: CompileKind, for_host: bool, dep_kind: DepKind) -> Vec<(usize, usize)> {
        let fi = self.features(pkg, for_host);
        let platform = self.platform(kind);
        let p = &self.i.ws.pkgs[pkg];
        p.deps
            .iter()
            .enumerate()
            .filter(|(_, dep)| dep.kind == dep_kind && dep_enabled(dep, fi, platform))
            .filter_map(|(i, _)| p.dep_targets[i].map(|t| (i, t)))
            .filter(|&(_, t)| self.i.ws.pkgs[t].loaded)
            .collect()
    }

    fn lib_deps(&mut self, pkg: usize, kind: CompileKind, for_host: bool, dep_kind: DepKind, parent: Mode) -> Result<Vec<UnitDep>> {
        let ws = self.i.ws;
        let mut out = Vec::new();
        for (i, t) in self.enabled_deps(pkg, kind, for_host, dep_kind) {
            let dep = &ws.pkgs[pkg].deps[i];
            let tp = &ws.pkgs[t];
            if let Some(u) = self.lib_unit(t, kind, for_host, parent)? {
                let name = dep
                    .rename
                    .as_ref()
                    .map(|r| r.replace('-', "_"))
                    .unwrap_or_else(|| tp.targets[tp.lib().unwrap()].crate_name());
                out.push(UnitDep {
                    unit: u,
                    extern_name: Some(name),
                });
            }
        }
        Ok(out)
    }

    fn run_unit(&mut self, pkg: usize, kind: CompileKind, for_host: bool) -> Result<usize> {
        let p = &self.i.ws.pkgs[pkg];
        let bs = p.build_script().unwrap();
        let profile = self.i.profiles.get(p, for_host, self.apple(kind));
        let id = Identity {
            pkg,
            target: bs,
            kind,
            for_host,
            mode: Mode::RunCustomBuild,
            feature_side_host: for_host,
        };
        let (u, new) = self.intern(id, profile);
        if !new {
            return Ok(u);
        }
        let mut deps = vec![UnitDep {
            unit: self.build_script_unit(pkg, for_host)?,
            extern_name: None,
        }];
        // `DEP_<links>_*` comes from direct dependencies that declare `links`.
        for (_, t) in self.enabled_deps(pkg, kind, for_host, DepKind::Normal) {
            let dp = &self.i.ws.pkgs[t];
            if dp.links.is_some() && dp.build_script().is_some() && !dp.proc_macro && dp.lib().is_some() {
                let r = self.run_unit(t, kind, for_host)?;
                deps.push(UnitDep {
                    unit: r,
                    extern_name: None,
                });
            }
        }
        self.units[u].deps = deps;
        Ok(u)
    }

    fn build_script_unit(&mut self, pkg: usize, owner_host_side: bool) -> Result<usize> {
        let p = &self.i.ws.pkgs[pkg];
        let bs = p.build_script().unwrap();
        let profile = self.i.profiles.get(p, true, self.i.host_apple);
        // Host and target owners with the same features share one compiled script.
        // Cargo keys units by features, not by which side asked.
        let (h, t) = (self.features(pkg, true), self.features(pkg, false));
        let owner_host_side = owner_host_side && (h.named != t.named || h.optional_deps != t.optional_deps);
        let id = Identity {
            pkg,
            target: bs,
            kind: CompileKind::Host,
            for_host: true,
            mode: Mode::Build,
            feature_side_host: owner_host_side,
        };
        let (u, new) = self.intern(id, profile);
        if !new {
            return Ok(u);
        }
        let fi = self.features(pkg, owner_host_side);
        let mut deps = Vec::new();
        for (i, dep) in p.deps.iter().enumerate() {
            if dep.kind != DepKind::Build || !dep_enabled(dep, fi, self.i.host_platform) {
                continue;
            }
            let Some(t) = p.dep_targets[i] else { continue };
            let tp = &self.i.ws.pkgs[t];
            if let Some(lib) = tp.lib() {
                let du = self.unit(t, lib, CompileKind::Host, true, Mode::Build)?;
                let name = dep
                    .rename
                    .as_ref()
                    .map(|r| r.replace('-', "_"))
                    .unwrap_or_else(|| tp.targets[lib].crate_name());
                deps.push(UnitDep {
                    unit: du,
                    extern_name: Some(name),
                });
            }
        }
        self.units[u].deps = deps;
        Ok(u)
    }

    fn required_features_ok(&self, pkg: usize, target: usize, for_host: bool) -> Result<(), Vec<String>> {
        let t = &self.i.ws.pkgs[pkg].targets[target];
        let fi = self.features(pkg, for_host);
        let missing: Vec<String> = t
            .required_features
            .iter()
            .filter(|f| {
                let base = f.split('/').next().unwrap();
                !fi.named.contains(base) && !fi.optional_deps.contains(base)
            })
            .cloned()
            .collect();
        if missing.is_empty() { Ok(()) } else { Err(missing) }
    }
}

fn select_targets(ws: &Workspace, pkg: usize, command: Command, f: &TargetFilter) -> Vec<(usize, Mode, bool)> {
    let p = &ws.pkgs[pkg];
    let explicit = f.explicit();
    let mut out = Vec::new();
    for (ti, t) in p.targets.iter().enumerate() {
        let named = match t.kind {
            TargetKind::Bin => f.bin_names.contains(&t.name),
            TargetKind::Example => f.example_names.contains(&t.name),
            TargetKind::Test => f.test_names.contains(&t.name),
            TargetKind::Bench => f.bench_names.contains(&t.name),
            _ => false,
        };
        let flagged = match t.kind {
            TargetKind::Lib => f.lib,
            TargetKind::Bin => f.bins,
            TargetKind::Example => f.examples,
            TargetKind::Test => f.tests,
            TargetKind::Bench => f.benches,
            TargetKind::BuildScript => false,
        } || (f.all_targets && t.kind != TargetKind::BuildScript);
        let default = match (command, t.kind) {
            (_, TargetKind::BuildScript) => false,
            (Command::Build | Command::Check, k) => matches!(k, TargetKind::Lib | TargetKind::Bin),
            (Command::Test, TargetKind::Example) => true,
            (Command::Test, _) => t.test,
            (Command::Bench, _) => t.bench,
            // A bin named like the lib would overwrite the lib's docs. Cargo skips it.
            (Command::Doc { .. }, TargetKind::Bin) => t.doc && !p.targets.iter().any(|l| l.kind == TargetKind::Lib && l.name == t.name),
            (Command::Doc { .. }, k) => t.doc && k == TargetKind::Lib,
            (Command::Run, _) => false,
        };
        let wanted = if explicit { named || flagged } else { default };
        if !wanted {
            continue;
        }
        // Test and bench targets are always test builds, harness or not.
        // `test` and `bench` also test everything else they select, except examples that didn't ask for it.
        let test_target = matches!(t.kind, TargetKind::Test | TargetKind::Bench);
        let mode = match command {
            Command::Build | Command::Run if test_target => Mode::Test,
            Command::Build | Command::Run => Mode::Build,
            Command::Check if test_target => Mode::CheckTest,
            Command::Check => Mode::Check,
            Command::Test if t.kind == TargetKind::Example && !t.test => Mode::Build,
            Command::Test => Mode::Test,
            Command::Bench if t.kind == TargetKind::Example && !t.bench => Mode::Build,
            Command::Bench => Mode::Bench,
            Command::Doc { .. } => Mode::Doc,
        };
        out.push((ti, mode, named));
    }
    out
}

pub fn build(inputs: &GraphInputs<'_>, sel: &RootSelection<'_>) -> Result<UnitGraph> {
    let ws = inputs.ws;
    let doc_deps = inputs.command == Command::Doc { deps: true };
    let mut b = Builder {
        i: inputs,
        units: Vec::new(),
        memo: HashMap::new(),
        doc_deps,
    };
    let kinds: Vec<CompileKind> = if inputs.no_target {
        vec![CompileKind::Host]
    } else {
        (0..inputs.target_platforms.len()).map(CompileKind::Target).collect()
    };
    let f = sel.filter;
    for (names, kind, what) in [
        (&f.bin_names, TargetKind::Bin, "bin"),
        (&f.example_names, TargetKind::Example, "example"),
        (&f.test_names, TargetKind::Test, "test"),
        (&f.bench_names, TargetKind::Bench, "bench"),
    ] {
        for name in names {
            if !sel
                .packages
                .iter()
                .any(|&p| ws.pkgs[p].targets.iter().any(|t| t.kind == kind && &t.name == name))
            {
                bail!("no {what} target named `{name}` in the selected packages");
            }
        }
    }
    let mut roots = Vec::new();
    for &pkg in sel.packages {
        let p = &ws.pkgs[pkg];
        for (ti, mode, named) in select_targets(ws, pkg, inputs.command, f) {
            let t = &p.targets[ti];
            for &requested in &kinds {
                let (kind, for_host) = if t.is_proc_macro() {
                    (CompileKind::Host, true)
                } else {
                    (requested, false)
                };
                if t.kind != TargetKind::Lib
                    && let Err(missing) = b.required_features_ok(pkg, ti, for_host)
                {
                    if named {
                        bail!(
                            "target `{}` in package `{}` requires the features: {}\nConsider enabling them by passing, e.g., `--features=\"{}\"`",
                            t.name,
                            p.name,
                            missing.join(", "),
                            missing.join(" ")
                        );
                    }
                    continue;
                }
                let u = b.unit(pkg, ti, kind, for_host, mode)?;
                if t.is_proc_macro() {
                    // A proc-macro you asked for keeps the normal profile, debuginfo included.
                    // Resolved for the requested target, then the unit moves to the host.
                    // In build mode, dependents share that unit.
                    let mut prof = inputs.profiles.get(p, false, b.apple(requested));
                    prof.panic = "unwind".into();
                    b.units[u].profile = prof;
                }
                if !roots.contains(&u) {
                    roots.push(u);
                }
            }
        }
    }
    // Doctests link the lib and the package's dev-dependencies.
    let mut aux_roots = Vec::new();
    let mut doctests = Vec::new();
    if inputs.command == Command::Test && (!f.explicit() || f.doc) {
        for &pkg in sel.packages {
            let p = &ws.pkgs[pkg];
            let Some(lib) = p.lib() else { continue };
            if !p.targets[lib].doctest {
                continue;
            }
            // Doctests run on the host, or the first requested target. Proc-macros are host-only.
            let (kind, for_host) = if p.proc_macro {
                (CompileKind::Host, true)
            } else {
                (kinds[0], false)
            };
            let lib_unit = b.unit(pkg, lib, kind, for_host, Mode::Build)?;
            let dev_deps = b.lib_deps(pkg, kind, for_host, DepKind::Dev, Mode::Build)?;
            aux_roots.push(lib_unit);
            aux_roots.extend(dev_deps.iter().map(|d| d.unit));
            doctests.push(Doctest {
                pkg,
                lib: lib_unit,
                dev_deps,
            });
        }
    }
    let mut g = UnitGraph {
        units: b.units,
        roots,
        aux_roots,
        doctests,
    };
    if doc_deps {
        remove_duplicate_docs(&mut g, ws, inputs.no_target);
    }
    if inputs.no_target {
        share_host_units(&mut g, ws);
    }
    Ok(g)
}

/// rustdoc keys output by crate name. Drop non-root docs that would collide: host-side when there's no `--target`, then older versions.
fn remove_duplicate_docs(g: &mut UnitGraph, ws: &Workspace, no_target: bool) {
    let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, u) in g.units.iter().enumerate() {
        if u.mode == Mode::Doc {
            by_name.entry(u.target(ws).crate_name()).or_default().push(i);
        }
    }
    let mut removed = HashSet::new();
    for (_, mut units) in by_name {
        if units.len() == 1 {
            continue;
        }
        let mut remove = |units: Vec<usize>, cond: &dyn Fn(usize) -> bool| -> Vec<usize> {
            let (gone, kept): (Vec<usize>, Vec<usize>) = units.into_iter().partition(|&u| cond(u) && !g.roots.contains(&u));
            removed.extend(gone);
            kept
        };
        if no_target {
            units = remove(units, &|u| g.units[u].kind == CompileKind::Host);
            if units.len() <= 1 {
                continue;
            }
        }
        let mut by_source: BTreeMap<(&str, &Source, CompileKind), Vec<usize>> = BTreeMap::new();
        for u in units {
            let p = &ws.pkgs[g.units[u].pkg];
            by_source.entry((p.name.as_str(), &p.source, g.units[u].kind)).or_default().push(u);
        }
        for (_, units) in by_source {
            let newest = units.iter().map(|&u| &ws.pkgs[g.units[u].pkg].version).max().unwrap().clone();
            remove(units, &|u| ws.pkgs[g.units[u].pkg].version < newest);
        }
    }
    for u in &mut g.units {
        u.deps.retain(|d| !removed.contains(&d.unit));
    }
    prune(g);
}

fn same_except_debuginfo(a: &Profile, b: &Profile) -> bool {
    let mut a = a.clone();
    a.debuginfo = b.debuginfo.clone();
    a.split_debuginfo = b.split_debuginfo.clone();
    a.strip = b.strip.clone();
    a == *b
}

/// A host lib that matches a target lib apart from debuginfo becomes that one unit. Then drop whatever nothing reaches.
fn share_host_units(g: &mut UnitGraph, ws: &Workspace) {
    let n = g.units.len();
    let mut alias: Vec<usize> = (0..n).collect();
    type Key<'a> = (usize, usize, CompileKind, Mode, &'a [String]);
    let mut by_identity: HashMap<Key<'_>, Vec<usize>> = HashMap::new();
    for (i, u) in g.units.iter().enumerate() {
        if !u.for_host {
            by_identity
                .entry((u.pkg, u.target, u.kind, u.mode, u.features.as_slice()))
                .or_default()
                .push(i);
        }
    }
    // Dependencies first. A host unit can only become the target unit if its deps already did,
    // or the host side would see two copies of something like serde_core.
    fn deps_of<'u>(alias: &[usize], u: &'u Unit) -> Vec<(usize, Option<&'u String>)> {
        let mut d: Vec<_> = u.deps.iter().map(|d| (alias[d.unit], d.extern_name.as_ref())).collect();
        d.sort();
        d
    }
    for i in crate::plan::topo_order(&g.units) {
        let u = &g.units[i];
        let t = u.target(ws);
        let lib = u.mode == Mode::Build && t.kind == TargetKind::Lib && !t.is_proc_macro();
        if !u.for_host || !(lib || u.mode == Mode::RunCustomBuild) {
            continue;
        }
        let mine = deps_of(&alias, u);
        if let Some(&v) = by_identity
            .get(&(u.pkg, u.target, u.kind, u.mode, u.features.as_slice()))
            .and_then(|c| {
                c.iter()
                    .find(|&&v| same_except_debuginfo(&g.units[v].profile, &u.profile) && deps_of(&alias, &g.units[v]) == mine)
            })
        {
            alias[i] = v;
        }
    }
    for u in &mut g.units {
        for d in &mut u.deps {
            d.unit = alias[d.unit];
        }
    }
    for r in g.roots.iter_mut().chain(g.aux_roots.iter_mut()) {
        *r = alias[*r];
    }
    for d in &mut g.doctests {
        d.lib = alias[d.lib];
        for dep in &mut d.dev_deps {
            dep.unit = alias[dep.unit];
        }
    }
    prune(g);
}

fn prune(g: &mut UnitGraph) {
    let n = g.units.len();
    let mut new_index = vec![usize::MAX; n];
    let mut order = Vec::new();
    let mut stack: Vec<usize> = g.roots.iter().chain(&g.aux_roots).copied().collect();
    while let Some(u) = stack.pop() {
        if new_index[u] != usize::MAX {
            continue;
        }
        new_index[u] = order.len();
        order.push(u);
        stack.extend(g.units[u].deps.iter().map(|d| d.unit));
    }
    let mut units: Vec<Unit> = order.iter().map(|&u| g.units[u].clone()).collect();
    for u in &mut units {
        for d in &mut u.deps {
            d.unit = new_index[d.unit];
        }
        let mut seen = std::collections::HashSet::new();
        u.deps.retain(|d| seen.insert(d.unit));
    }
    g.roots = g.roots.iter().map(|&r| new_index[r]).collect();
    g.aux_roots = g.aux_roots.iter().map(|&r| new_index[r]).collect();
    for d in &mut g.doctests {
        d.lib = new_index[d.lib];
        for dep in &mut d.dev_deps {
            dep.unit = new_index[dep.unit];
        }
    }
    g.roots.dedup();
    g.units = units;
}

/// Cargo's `--unit-graph` JSON, version 1. The differential harness diffs against this.
pub fn to_json(ws: &Workspace, g: &UnitGraph, targets: &[String]) -> serde_json::Value {
    use serde_json::json;
    let units: Vec<_> = g
        .units
        .iter()
        .map(|u| {
            let p = &ws.pkgs[u.pkg];
            let t = &p.targets[u.target];
            let kind = match t.kind {
                TargetKind::Lib => t.crate_types.clone(),
                TargetKind::Bin => vec!["bin".into()],
                TargetKind::Example => vec!["example".into()],
                TargetKind::Test => vec!["test".into()],
                TargetKind::Bench => vec!["bench".into()],
                TargetKind::BuildScript => vec!["custom-build".into()],
            };
            let platform = match u.kind {
                CompileKind::Host => serde_json::Value::Null,
                CompileKind::Target(i) => json!(targets[i]),
            };
            json!({
                "pkg_id": p.id,
                "target": { "kind": kind, "crate_types": t.crate_types, "name": t.name, "src_path": t.src_path, "edition": t.edition },
                "profile": { "name": u.profile.name, "opt_level": u.profile.opt_level,
                             "debuginfo": u.profile.debuginfo.parse::<u64>().map(serde_json::Value::from).unwrap_or_else(|_| json!(u.profile.debuginfo)),
                             "debug_assertions": u.profile.debug_assertions, "overflow_checks": u.profile.overflow_checks,
                             "panic": u.profile.panic, "incremental": u.profile.incremental, "strip": u.profile.strip,
                             "split_debuginfo": u.profile.split_debuginfo },
                "platform": platform,
                "mode": if u.mode == Mode::Bench { "test" } else { u.mode.name() },
                "features": u.features,
                "dependencies": u.deps.iter().map(|d| json!({ "index": d.unit, "extern_crate_name": d.extern_name.clone().unwrap_or_else(|| ws.pkgs[g.units[d.unit].pkg].targets[g.units[d.unit].target].crate_name()), "public": false, "noprelude": false })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "version": 1, "units": units, "roots": g.roots })
}
