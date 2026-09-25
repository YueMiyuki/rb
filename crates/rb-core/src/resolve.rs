//! Package graph, lock import, and resolution against registry, git, and path sources.

use crate::features::{self, EngineOpts, Outcome, Request};
use crate::git::{Git, find_package};
use crate::lockfile::{LockDep, LockPkg, Lockfile};
use crate::manifest::{DepDecl, DepKind, DepSource, GitRef, Package, RegistryLookup, load_package};
use crate::registry::Registry;
use crate::shell::Shell;
use anyhow::{Context, Result, bail};
use semver::{Version, VersionReq};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceId {
    Path(PathBuf),
    Registry(String),
    Git { url: String, reference: GitRef, commit: String },
}

impl SourceId {
    /// `source = "…"` in `Cargo.lock`. Path packages have none.
    pub fn lock_string(&self) -> Option<String> {
        match self {
            Self::Path(_) => None,
            Self::Registry(s) => Some(s.clone()),
            Self::Git { url, reference, commit } => Some(format!("git+{url}{}#{commit}", reference.query())),
        }
    }

    pub fn parse_lock(s: &str) -> Result<Self> {
        if let Some(rest) = s.strip_prefix("git+") {
            let (url_q, commit) = rest.split_once('#').with_context(|| format!("git source without a commit: {s}"))?;
            let (url, reference) = match url_q.split_once('?') {
                Some((u, q)) => {
                    let (k, v) = q.split_once('=').unwrap_or((q, ""));
                    let r = match k {
                        "branch" => GitRef::Branch(v.to_owned()),
                        "tag" => GitRef::Tag(v.to_owned()),
                        "rev" => GitRef::Rev(v.to_owned()),
                        _ => GitRef::DefaultBranch,
                    };
                    (u, r)
                }
                None => (url_q, GitRef::DefaultBranch),
            };
            return Ok(Self::Git {
                url: url.to_owned(),
                reference,
                commit: commit.to_owned(),
            });
        }
        if s.starts_with("registry+") || s.starts_with("sparse+") {
            return Ok(Self::Registry(s.to_owned()));
        }
        bail!("unsupported source `{s}` in Cargo.lock")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PkgId {
    pub name: String,
    pub version: Version,
    pub source: SourceId,
}

impl PkgId {
    /// `registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0`, or `path+file:///ws/app#0.1.0`.
    pub fn spec(&self) -> String {
        let url = match &self.source {
            SourceId::Registry(s) => s.clone(),
            SourceId::Path(p) => format!("path+file://{}", p.display()),
            SourceId::Git { url, reference, .. } => format!("git+{url}{}", reference.query()),
        };
        let base = url.split('?').next().unwrap_or(&url);
        let last = base.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        if last == self.name {
            format!("{url}#{}", self.version)
        } else {
            format!("{url}#{}@{}", self.name, self.version)
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub deps: Vec<DepDecl>,
    pub features: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: PkgId,
    pub checksum: Option<String>,
    /// Enough index data to resolve. The full manifest comes later, when we build.
    pub summary: Option<Summary>,
    pub package: Option<Package>,
    pub edges: Vec<usize>,
    pub is_member: bool,
    /// Registry source id this node stands in for, via `[patch]`.
    pub patch_for: Option<String>,
}

static NO_DEPS: Vec<DepDecl> = Vec::new();
static NO_FEATURES: BTreeMap<String, Vec<String>> = BTreeMap::new();

impl Node {
    pub fn new(id: PkgId) -> Self {
        Self {
            id,
            checksum: None,
            summary: None,
            package: None,
            edges: Vec::new(),
            is_member: false,
            patch_for: None,
        }
    }

    pub fn deps(&self) -> &[DepDecl] {
        match (&self.package, &self.summary) {
            (Some(p), _) => &p.deps,
            (None, Some(s)) => &s.deps,
            _ => &NO_DEPS,
        }
    }

    pub fn features(&self) -> &BTreeMap<String, Vec<String>> {
        match (&self.package, &self.summary) {
            (Some(p), _) => &p.features,
            (None, Some(s)) => &s.features,
            _ => &NO_FEATURES,
        }
    }

    pub fn has_data(&self) -> bool {
        self.package.is_some() || self.summary.is_some()
    }

    pub fn is_proc_macro(&self) -> bool {
        self.package.as_ref().is_some_and(|p| p.targets.iter().any(|t| t.is_proc_macro()))
    }
}

#[derive(Default, Debug)]
pub struct Graph {
    pub nodes: Vec<Node>,
    by_id: HashMap<PkgId, usize>,
}

fn same_git_url(a: &str, b: &str) -> bool {
    let n = |s: &str| s.trim_end_matches('/').trim_end_matches(".git").to_lowercase();
    n(a) == n(b)
}

/// A path or git dep with no version req accepts anything, pre-releases included.
pub fn req_accepts(dep: &DepDecl, v: &Version) -> bool {
    let registry = matches!(dep.source, DepSource::Registry(_));
    (!registry && dep.req == VersionReq::STAR) || dep.req.matches(v)
}

impl Graph {
    pub fn add(&mut self, node: Node) -> usize {
        if let Some(&i) = self.by_id.get(&node.id) {
            return i;
        }
        let i = self.nodes.len();
        self.by_id.insert(node.id.clone(), i);
        self.nodes.push(node);
        i
    }

    pub fn find(&self, id: &PkgId) -> Option<usize> {
        self.by_id.get(id).copied()
    }

    fn source_matches(&self, dep: &DepDecl, t: &Node) -> bool {
        match (&dep.source, &t.id.source) {
            (DepSource::Registry(s), SourceId::Registry(r)) => s == r,
            (DepSource::Registry(s), _) => t.patch_for.as_deref() == Some(s.as_str()),
            (DepSource::Path(p), SourceId::Path(q)) => p == q,
            (DepSource::Git { url, .. }, SourceId::Git { url: u, .. }) => same_git_url(url, u),
            (DepSource::Git { url, .. }, _) => t.patch_for.as_deref().is_some_and(|p| same_git_url(p, url)),
            _ => false,
        }
    }

    pub fn target_of(&self, n: usize, dep: &DepDecl) -> Option<usize> {
        let hit = self.nodes[n]
            .edges
            .iter()
            .copied()
            .filter(|&t| {
                let tn = &self.nodes[t];
                tn.id.name == dep.name && self.source_matches(dep, tn) && req_accepts(dep, &tn.id.version)
            })
            .max_by(|a, b| self.nodes[*a].id.version.cmp(&self.nodes[*b].id.version));
        if hit.is_some() {
            return hit;
        }
        // A path dep inside a git repo is that commit's package in the lockfile.
        if let SourceId::Git { url, commit, .. } = &self.nodes[n].id.source
            && matches!(dep.source, DepSource::Path(_))
        {
            return self.nodes.iter().position(|t| {
                t.id.name == dep.name
                    && matches!(&t.id.source, SourceId::Git { url: u, commit: c, .. } if same_git_url(u, url) && c == commit)
            });
        }
        None
    }

    pub fn path_node(&self, path: &Path) -> Option<usize> {
        self.nodes
            .iter()
            .position(|n| matches!(&n.id.source, SourceId::Path(p) if p == path))
    }

    /// Path packages have to already be in the graph.
    pub fn import_lock(&mut self, lock: &Lockfile) -> Result<()> {
        let mut idx = Vec::with_capacity(lock.packages.len());
        for p in &lock.packages {
            let n = match &p.source {
                None => self
                    .nodes
                    .iter()
                    .position(|n| matches!(n.id.source, SourceId::Path(_)) && n.id.name == p.name && n.id.version == p.version),
                Some(s) => {
                    let mut node = Node::new(PkgId {
                        name: p.name.clone(),
                        version: p.version.clone(),
                        source: SourceId::parse_lock(s)?,
                    });
                    node.checksum = p.checksum.clone();
                    Some(self.add(node))
                }
            };
            idx.push(n);
        }
        for (p, n) in lock.packages.iter().zip(&idx) {
            let Some(n) = *n else { continue };
            for d in &p.dependencies {
                let found = lock.packages.iter().position(|q| {
                    q.name == d.name
                        && d.version.as_ref().is_none_or(|v| &q.version == v)
                        && d.source.as_ref().is_none_or(|s| q.source.as_ref() == Some(s))
                });
                if let Some(t) = found.and_then(|j| idx[j])
                    && !self.nodes[n].edges.contains(&t)
                {
                    self.nodes[n].edges.push(t);
                }
            }
        }
        Ok(())
    }

    /// Lock import doesn't run the resolver. Mark `[patch]` replacements or dependents still look unlocked.
    pub fn mark_lock_patches(&mut self, patches: &Patches) {
        for (source, decls) in patches {
            for dep in decls {
                if let Some(i) = self.nodes.iter().position(|n| n.id.name == dep.name && self.source_matches(dep, n)) {
                    self.nodes[i].patch_for = Some(source.clone());
                }
            }
        }
    }

    pub fn locals_complete(&self) -> bool {
        (0..self.nodes.len())
            .filter(|&n| matches!(self.nodes[n].id.source, SourceId::Path(_)))
            .all(|n| {
                let node = &self.nodes[n];
                node.deps().iter().all(|d| {
                    let required = node.is_member || (!d.optional && d.kind != DepKind::Dev);
                    !required || self.target_of(n, d).is_some()
                })
            })
    }
}

type LockedVersions = HashMap<(String, String), Vec<(Version, Option<String>)>>;

/// Versions and commits already in a lockfile. Re-resolution prefers them.
#[derive(Default)]
pub struct Prefs {
    registry: LockedVersions,
    git: HashMap<(String, GitRef), String>,
}

impl Prefs {
    pub fn from_lock(lock: Option<&Lockfile>) -> Self {
        let mut p = Self::default();
        for pkg in lock.map(|l| l.packages.as_slice()).unwrap_or_default() {
            match pkg.source.as_deref().map(SourceId::parse_lock) {
                Some(Ok(SourceId::Registry(s))) => {
                    p.registry
                        .entry((s, pkg.name.clone()))
                        .or_default()
                        .push((pkg.version.clone(), pkg.checksum.clone()));
                }
                Some(Ok(SourceId::Git { url, reference, commit })) => {
                    p.git.insert((url, reference), commit);
                }
                _ => {}
            }
        }
        p
    }

    pub fn forget_all(&mut self) {
        self.registry.clear();
        self.git.clear();
    }

    pub fn forget(&mut self, names: &HashSet<String>, lock: Option<&Lockfile>) {
        self.registry.retain(|(_, name), _| !names.contains(name));
        for pkg in lock.map(|l| l.packages.as_slice()).unwrap_or_default() {
            if !names.contains(&pkg.name) {
                continue;
            }
            if let Some(Ok(SourceId::Git { url, reference, .. })) = pkg.source.as_deref().map(SourceId::parse_lock) {
                self.git.remove(&(url, reference));
            }
        }
    }

    /// `--precise` beats "latest".
    pub fn pin_version(&mut self, name: &str, version: Version) {
        let mut hit = false;
        for ((_, n), vs) in &mut self.registry {
            if n == name {
                *vs = vec![(version.clone(), None)];
                hit = true;
            }
        }
        if !hit {
            self.registry
                .insert((crate::manifest::CRATES_IO.into(), name.to_owned()), vec![(version, None)]);
        }
    }

    pub fn pin_git(&mut self, name: &str, commit: &str, lock: Option<&Lockfile>) {
        for pkg in lock.map(|l| l.packages.as_slice()).unwrap_or_default() {
            if pkg.name != name {
                continue;
            }
            if let Some(Ok(SourceId::Git { url, reference, .. })) = pkg.source.as_deref().map(SourceId::parse_lock) {
                self.git.insert((url, reference), commit.to_owned());
            }
        }
    }
}

pub type Patches = HashMap<String, Vec<DepDecl>>;

pub struct Sources<'a> {
    pub registry: &'a Registry,
    pub git: &'a Git,
    pub registries: RegistryLookup<'a>,
    pub patches: &'a Patches,
    pub rustc_version: Option<Version>,
    /// Resolver v3: prefer versions whose `rust-version` this rustc can build.
    pub msrv_aware: bool,
    pub shell: &'a Shell,
    pub load_path: &'a dyn Fn(&Path) -> Result<Package>,
}

fn load_git(src: &Sources<'_>, url: &str, commit: &str, name: &str) -> Result<Package> {
    let checkout = src.git.checkout(url, commit)?;
    let manifest = find_package(&checkout, name)?;
    load_package(&manifest, Some(&checkout), src.registries)
}

fn load_summary(g: &mut Graph, u: usize, src: &Sources<'_>) -> Result<()> {
    let id = g.nodes[u].id.clone();
    match &id.source {
        SourceId::Registry(s) => {
            let entries = src.registry.entries(s, &id.name)?;
            let e = entries
                .iter()
                .find(|e| e.vers == id.version)
                .with_context(|| format!("`{}@{}` is not in the registry index", id.name, id.version))?;
            let node = &mut g.nodes[u];
            node.checksum.get_or_insert_with(|| e.cksum.clone());
            node.summary = Some(Summary {
                deps: e.deps.clone(),
                features: e.features.clone(),
            });
        }
        SourceId::Git { url, commit, .. } => {
            let pkg = load_git(src, url, commit, &id.name)?;
            g.nodes[u].package = Some(pkg);
        }
        SourceId::Path(p) => {
            let pkg = (src.load_path)(p)?;
            g.nodes[u].package = Some(pkg);
        }
    }
    Ok(())
}

fn select(g: &mut Graph, n: usize, dep: &DepDecl, prefs: &Prefs, src: &Sources<'_>) -> Result<usize> {
    let parent = g.nodes[n].id.name.clone();
    match &dep.source {
        DepSource::Path(p) => {
            if let Some(t) = g.path_node(p) {
                return Ok(t);
            }
            let pkg = (src.load_path)(p).with_context(|| format!("failed to load path dependency `{}` of `{parent}`", dep.name))?;
            let mut node = Node::new(PkgId {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                source: SourceId::Path(p.clone()),
            });
            node.package = Some(pkg);
            Ok(g.add(node))
        }
        DepSource::Git { url, reference } => {
            if let Some(t) = g.nodes.iter().position(|x| {
                x.id.name == dep.name
                    && matches!(&x.id.source, SourceId::Git { url: u, reference: r, .. } if same_git_url(u, url) && r == reference)
            }) {
                return Ok(t);
            }
            let commit = match prefs.git.get(&(url.clone(), reference.clone())) {
                Some(c) => c.clone(),
                None => {
                    src.shell.status("Updating", format!("git repository `{url}`"));
                    src.git.resolve(url, reference)?
                }
            };
            let pkg = load_git(src, url, &commit, &dep.name)?;
            let mut node = Node::new(PkgId {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                source: SourceId::Git {
                    url: url.clone(),
                    reference: reference.clone(),
                    commit,
                },
            });
            node.package = Some(pkg);
            Ok(g.add(node))
        }
        DepSource::Registry(s) => {
            if let Some(patch) = src.patches.get(s).and_then(|v| v.iter().find(|p| p.name == dep.name)) {
                let mut pd = patch.clone();
                pd.kind = dep.kind;
                let t = select(g, n, &pd, prefs, src)?;
                if dep.req.matches(&g.nodes[t].id.version) {
                    g.nodes[t].patch_for = Some(s.clone());
                    return Ok(t);
                }
                src.shell.warn(format!(
                    "patch for `{}` ({}) does not match `{}` required by `{parent}`",
                    dep.name, g.nodes[t].id.version, dep.req
                ));
            }
            let existing = (0..g.nodes.len())
                .filter(|&t| {
                    let x = &g.nodes[t];
                    x.id.name == dep.name && x.id.source == SourceId::Registry(s.clone()) && dep.req.matches(&x.id.version)
                })
                .max_by(|a, b| g.nodes[*a].id.version.cmp(&g.nodes[*b].id.version));
            if let Some(t) = existing {
                return Ok(t);
            }
            if let Some((v, sum)) = prefs
                .registry
                .get(&(s.clone(), dep.name.clone()))
                .and_then(|vs| vs.iter().filter(|(v, _)| dep.req.matches(v)).max_by(|a, b| a.0.cmp(&b.0)))
            {
                let mut node = Node::new(PkgId {
                    name: dep.name.clone(),
                    version: v.clone(),
                    source: SourceId::Registry(s.clone()),
                });
                node.checksum = sum.clone();
                return Ok(g.add(node));
            }
            let entries = src.registry.entries(s, &dep.name)?;
            let candidates: Vec<_> = entries.iter().filter(|e| !e.yanked && dep.req.matches(&e.vers)).collect();
            let compatible: Vec<_> = match (&src.rustc_version, src.msrv_aware) {
                (Some(rv), true) => candidates
                    .iter()
                    .copied()
                    .filter(|e| e.rust_version.as_ref().is_none_or(|m| m <= rv))
                    .collect(),
                _ => Vec::new(),
            };
            let pool = if compatible.is_empty() { &candidates } else { &compatible };
            let best = pool.iter().max_by(|a, b| a.vers.cmp(&b.vers)).with_context(|| {
                format!(
                    "failed to select a version for `{}` matching `{}` (required by `{parent}`)",
                    dep.name, dep.req
                )
            })?;
            let mut node = Node::new(PkgId {
                name: dep.name.clone(),
                version: best.vers.clone(),
                source: SourceId::Registry(s.clone()),
            });
            node.checksum = Some(best.cksum.clone());
            node.summary = Some(Summary {
                deps: best.deps.clone(),
                features: best.features.clone(),
            });
            Ok(g.add(node))
        }
    }
}

/// What `Cargo.lock` records: every reachable dep, all features, dev-dependencies, every platform.
pub fn resolve_lock_mode(g: &mut Graph, members: &[usize], prefs: &Prefs, src: &Sources<'_>) -> Result<Outcome> {
    let dev_for: HashSet<usize> = members.iter().copied().collect();
    let reqs: Vec<Request> = members
        .iter()
        .map(|&m| Request {
            node: m,
            features: Vec::new(),
            all_features: true,
            default: true,
        })
        .collect();
    let mut announced = false;
    for _ in 0..10_000 {
        let opts = EngineOpts {
            two_sides: false,
            platforms: None,
            dev_for: &dev_for,
            needs_package: false,
            weak_activates: true,
        };
        let out = features::resolve(g, &reqs, &opts)?;
        if out.missing.is_empty() && out.unloaded.is_empty() {
            return Ok(out);
        }
        let mut names: Vec<(String, String)> = Vec::new();
        for &(n, i) in &out.missing {
            let d = &g.nodes[n].deps()[i];
            if let DepSource::Registry(s) = &d.source {
                names.push((s.clone(), d.name.clone()));
            }
        }
        for &u in &out.unloaded {
            if let SourceId::Registry(s) = &g.nodes[u].id.source {
                names.push((s.clone(), g.nodes[u].id.name.clone()));
            }
        }
        names.sort();
        names.dedup();
        if !names.is_empty() {
            if !announced {
                src.shell.status("Updating", "crates.io index");
                announced = true;
            }
            src.registry.prefetch(&names);
        }
        for &u in &out.unloaded {
            load_summary(g, u, src)?;
        }
        for &(n, i) in &out.missing {
            let dep = g.nodes[n].deps()[i].clone();
            let t = select(g, n, &dep, prefs, src)?;
            if !g.nodes[n].edges.contains(&t) {
                g.nodes[n].edges.push(t);
            }
            if g.target_of(n, &dep).is_none() {
                bail!(
                    "`{}` {} does not satisfy `{}` required by `{}`",
                    g.nodes[t].id.name,
                    g.nodes[t].id.version,
                    dep.req,
                    g.nodes[n].id.name
                );
            }
        }
    }
    bail!("dependency resolution did not converge")
}

pub fn to_lockfile(g: &Graph, out: &Outcome, members: &[usize], version: u32) -> Lockfile {
    let mut nodes: BTreeSet<usize> = members.iter().copied().collect();
    nodes.extend(out.info.keys().map(|(n, _)| *n));
    for &(a, b) in &out.edges {
        nodes.insert(a);
        nodes.insert(b);
    }
    let packages = nodes
        .into_iter()
        .map(|n| {
            let node = &g.nodes[n];
            let dependencies = out
                .edges
                .iter()
                .filter(|(a, _)| *a == n)
                .map(|&(_, t)| {
                    let tn = &g.nodes[t];
                    LockDep {
                        name: tn.id.name.clone(),
                        version: Some(tn.id.version.clone()),
                        source: tn.id.source.lock_string(),
                    }
                })
                .collect();
            LockPkg {
                name: node.id.name.clone(),
                version: node.id.version.clone(),
                source: node.id.source.lock_string(),
                checksum: matches!(node.id.source, SourceId::Registry(_))
                    .then(|| node.checksum.clone())
                    .flatten(),
                dependencies,
            }
        })
        .collect();
    Lockfile { version, packages }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_match_cargo() {
        let reg = PkgId {
            name: "serde".into(),
            version: Version::new(1, 0, 0),
            source: SourceId::Registry(crate::manifest::CRATES_IO.into()),
        };
        assert_eq!(reg.spec(), "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0");
        let path = PkgId {
            name: "app".into(),
            version: Version::new(0, 1, 0),
            source: SourceId::Path("/ws/app".into()),
        };
        assert_eq!(path.spec(), "path+file:///ws/app#0.1.0");
        let other = PkgId {
            name: "core-lib".into(),
            version: Version::new(0, 2, 0),
            source: SourceId::Path("/ws/core".into()),
        };
        assert_eq!(other.spec(), "path+file:///ws/core#core-lib@0.2.0");
        let git = SourceId::parse_lock("git+https://github.com/a/b?branch=main#abc123").unwrap();
        assert_eq!(git.lock_string().unwrap(), "git+https://github.com/a/b?branch=main#abc123");
    }

    #[test]
    fn path_dep_inside_patched_git_uses_same_commit() {
        let url = "https://github.com/kane50613/fontations";
        let commit = "80db8b0ccde357fd65ceb5c5243eb2de2754ee83";
        let git = |name: &str| {
            Node::new(PkgId {
                name: name.into(),
                version: Version::new(0, 1, 0),
                source: SourceId::Git {
                    url: url.into(),
                    reference: GitRef::Rev(commit.into()),
                    commit: commit.into(),
                },
            })
        };
        let mut g = Graph::default();
        let skrifa = g.add(git("skrifa"));
        let fonts = g.add(git("read-fonts"));
        let dep = DepDecl {
            name: "read-fonts".into(),
            rename: None,
            req: VersionReq::STAR,
            source: DepSource::Path(PathBuf::from("read-fonts")),
            kind: DepKind::Normal,
            optional: false,
            default_features: true,
            features: Vec::new(),
            platform: None,
        };
        assert_eq!(g.target_of(skrifa, &dep), Some(fonts));
        let mut patches = Patches::new();
        patches.insert(
            crate::manifest::CRATES_IO.into(),
            vec![DepDecl {
                name: "skrifa".into(),
                rename: None,
                req: VersionReq::STAR,
                source: DepSource::Git {
                    url: url.into(),
                    reference: GitRef::Rev(commit.into()),
                },
                kind: DepKind::Normal,
                optional: false,
                default_features: true,
                features: Vec::new(),
                platform: None,
            }],
        );
        g.mark_lock_patches(&patches);
        assert_eq!(g.nodes[skrifa].patch_for.as_deref(), Some(crate::manifest::CRATES_IO));
    }
}
