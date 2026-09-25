//! Cargo's feature rules, including lock mode: one side, every platform, all member features.
//! Missing edges and unloaded packages come back so the caller can fetch them and run this again.

use crate::cfgexpr::PlatformCfg;
use crate::manifest::{DepDecl, DepKind};
use crate::resolve::Graph;
use anyhow::{Result, bail};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

pub struct Platforms<'a> {
    pub host: PlatformCfg<'a>,
    pub targets: Vec<PlatformCfg<'a>>,
}

pub struct EngineOpts<'a> {
    /// Resolver v2+: build deps and proc-macros get their own feature set.
    pub two_sides: bool,
    /// `None` means every platform. That's lock mode, and resolver v1.
    pub platforms: Option<&'a Platforms<'a>>,
    pub dev_for: &'a HashSet<usize>,
    /// Don't expand a package until its manifest is loaded.
    pub needs_package: bool,
    /// Lock mode still pulls a weak `dep?/feat` into the graph. The feature pass leaves it off.
    pub weak_activates: bool,
}

pub struct Request {
    pub node: usize,
    pub features: Vec<String>,
    pub all_features: bool,
    pub default: bool,
}

#[derive(Clone, Debug, Default)]
pub struct FeatureInfo {
    pub named: BTreeSet<String>,
    pub optional_deps: BTreeSet<String>,
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// Keyed by `(node, host side)`.
    pub info: HashMap<(usize, bool), FeatureInfo>,
    /// Activated deps that don't have a resolved target yet.
    pub missing: Vec<(usize, usize)>,
    /// Activated nodes whose data isn't loaded.
    pub unloaded: BTreeSet<usize>,
    pub edges: BTreeSet<(usize, usize)>,
}

enum Task {
    Base,
    Feat(String),
    Dep(String),
    DepFeat(String, String, bool),
}

struct Engine<'a> {
    g: &'a Graph,
    o: &'a EngineOpts<'a>,
    queue: VecDeque<(usize, bool, Task)>,
    named: HashMap<(usize, bool), BTreeSet<String>>,
    opt: HashMap<(usize, bool), BTreeSet<String>>,
    based: HashSet<(usize, bool)>,
    active: HashSet<(usize, bool, usize)>,
    dep_feats: HashMap<(usize, bool), Vec<(String, String)>>,
    out: Outcome,
}

impl Engine<'_> {
    fn loaded(&self, n: usize) -> bool {
        let node = &self.g.nodes[n];
        if self.o.needs_package {
            node.package.is_some()
        } else {
            node.has_data()
        }
    }

    fn side(&self, host: bool) -> bool {
        self.o.two_sides && host
    }

    fn platform_ok(&self, dep: &DepDecl, host: bool) -> bool {
        match (&dep.platform, self.o.platforms) {
            (None, _) | (_, None) => true,
            (Some(p), Some(pl)) => {
                // Build dependencies run on the host.
                if host || dep.kind == DepKind::Build {
                    p.matches(&pl.host)
                } else {
                    pl.targets.iter().any(|t| p.matches(t))
                }
            }
        }
    }

    fn kind_ok(&self, n: usize, dep: &DepDecl) -> bool {
        dep.kind != DepKind::Dev || self.o.dev_for.contains(&n)
    }

    fn target_side(&self, t: usize, dep: &DepDecl, s: bool) -> bool {
        self.side(dep.kind == DepKind::Build || self.g.nodes[t].is_proc_macro() || s)
    }

    fn activate_edge(&mut self, n: usize, s: bool, i: usize) {
        if !self.active.insert((n, s, i)) {
            return;
        }
        let dep = &self.g.nodes[n].deps()[i];
        let Some(t) = self.g.target_of(n, dep) else {
            self.out.missing.push((n, i));
            return;
        };
        self.out.edges.insert((n, t));
        if !self.loaded(t) {
            self.out.unloaded.insert(t);
            return;
        }
        let ts = self.target_side(t, dep, s);
        self.queue.push_back((t, ts, Task::Base));
        if dep.default_features {
            self.queue.push_back((t, ts, Task::Feat("default".into())));
        }
        for f in &dep.features {
            self.queue.push_back((t, ts, Task::Feat(f.clone())));
        }
        if let Some(list) = self.dep_feats.get(&(n, s)) {
            for (name, feat) in list {
                if name == dep.dep_name() {
                    self.queue.push_back((t, ts, Task::Feat(feat.clone())));
                }
            }
        }
    }

    fn value(&mut self, n: usize, s: bool, v: &str) {
        let task = if let Some(d) = v.strip_prefix("dep:") {
            Task::Dep(d.to_owned())
        } else if let Some((a, b)) = v.split_once('/') {
            match a.strip_suffix('?') {
                Some(a) => Task::DepFeat(a.to_owned(), b.to_owned(), true),
                None => Task::DepFeat(a.to_owned(), b.to_owned(), false),
            }
        } else {
            Task::Feat(v.to_owned())
        };
        self.queue.push_back((n, s, task));
    }

    fn run(&mut self) -> Result<()> {
        while let Some((n, s, task)) = self.queue.pop_front() {
            if !self.loaded(n) {
                self.out.unloaded.insert(n);
                continue;
            }
            let node = &self.g.nodes[n];
            match task {
                Task::Base => {
                    if self.based.insert((n, s)) {
                        for (i, dep) in node.deps().iter().enumerate() {
                            if !dep.optional && self.kind_ok(n, dep) && self.platform_ok(dep, s) {
                                self.activate_edge(n, s, i);
                            }
                        }
                    }
                }
                Task::Feat(f) => {
                    let feats = node.features();
                    let Some(values) = feats.get(&f) else {
                        if f == "default" {
                            continue;
                        }
                        bail!("package `{}` does not have the feature `{f}`", node.id.name);
                    };
                    if self.named.entry((n, s)).or_default().insert(f) {
                        for v in values.clone() {
                            self.value(n, s, &v);
                        }
                    }
                }
                Task::Dep(name) => {
                    if self.opt.entry((n, s)).or_default().insert(name.clone()) {
                        for (i, dep) in node.deps().iter().enumerate() {
                            if dep.optional && dep.dep_name() == name && self.kind_ok(n, dep) && self.platform_ok(dep, s) {
                                self.activate_edge(n, s, i);
                            }
                        }
                    }
                }
                Task::DepFeat(name, feat, weak) => {
                    // `dep/feat` only activates `dep` when that dependency actually applies here.
                    let is_optional = node
                        .deps()
                        .iter()
                        .any(|d| d.optional && d.dep_name() == name && self.kind_ok(n, d) && self.platform_ok(d, s));
                    self.dep_feats.entry((n, s)).or_default().push((name.clone(), feat.clone()));
                    if (!weak || self.o.weak_activates) && is_optional {
                        self.queue.push_back((n, s, Task::Dep(name.clone())));
                        if !weak && node.features().contains_key(&name) {
                            self.queue.push_back((n, s, Task::Feat(name.clone())));
                        }
                    }
                    let active: Vec<usize> = self
                        .active
                        .iter()
                        .filter(|(an, as_, i)| *an == n && *as_ == s && node.deps()[*i].dep_name() == name)
                        .map(|(_, _, i)| *i)
                        .collect();
                    for i in active {
                        let dep = &node.deps()[i];
                        if let Some(t) = self.g.target_of(n, dep).filter(|t| self.loaded(*t)) {
                            let ts = self.target_side(t, dep, s);
                            self.queue.push_back((t, ts, Task::Feat(feat.clone())));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

pub fn resolve(g: &Graph, reqs: &[Request], o: &EngineOpts<'_>) -> Result<Outcome> {
    let mut e = Engine {
        g,
        o,
        queue: VecDeque::new(),
        named: HashMap::new(),
        opt: HashMap::new(),
        based: HashSet::new(),
        active: HashSet::new(),
        dep_feats: HashMap::new(),
        out: Outcome::default(),
    };
    for r in reqs {
        let host = e.side(e.loaded(r.node) && g.nodes[r.node].is_proc_macro());
        e.queue.push_back((r.node, host, Task::Base));
        if r.default {
            e.queue.push_back((r.node, host, Task::Feat("default".into())));
        }
        if r.all_features && e.loaded(r.node) {
            for f in g.nodes[r.node].features().keys() {
                e.queue.push_back((r.node, host, Task::Feat(f.clone())));
            }
        }
        for f in &r.features {
            e.value(r.node, host, f);
        }
    }
    e.run()?;
    for key in e.based.iter().copied() {
        let info = FeatureInfo {
            named: e.named.remove(&key).unwrap_or_default(),
            optional_deps: e.opt.remove(&key).unwrap_or_default(),
        };
        e.out.info.insert(key, info);
    }
    e.out.missing.sort_unstable();
    e.out.missing.dedup();
    Ok(e.out)
}
