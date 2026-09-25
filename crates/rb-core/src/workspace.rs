//! Find the workspace, import or rewrite `Cargo.lock`, and load only the packages this build reaches.

use crate::cfgexpr::PlatformCfg;
use crate::features::{self, EngineOpts, FeatureInfo, Platforms, Request};
use crate::git::Git;
use crate::lockfile;
use crate::manifest::{self, CRATES_IO, DepDecl, DepKind, Package, RegistryLookup, WsContext, clean_path, load_package, read_toml};
use crate::registry::{Download, Registry, parallel};
use crate::resolve::{self, Graph, Node, Patches, PkgId, Prefs, SourceId, Sources};
use crate::shell::Shell;
use anyhow::{Context, Result, bail};
use semver::Version;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub use crate::manifest::{Target, TargetKind};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Workspace,
    Path,
    Registry(String),
    Git(String),
}

impl Source {
    /// Path or workspace: warnings on, incremental on, identity is a content hash.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Workspace | Self::Path)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolverVersion {
    V1,
    V2,
    V3,
}

/// A package we can build. Graph nodes we never needed stay as placeholders.
#[derive(Clone, Debug)]
pub struct Pkg {
    pub id: String,
    /// Registry and git packages: spec plus checksum or commit. Doesn't change.
    pub identity: String,
    pub name: String,
    pub version: Version,
    pub manifest_path: PathBuf,
    pub root: PathBuf,
    pub source: Source,
    pub is_member: bool,
    pub loaded: bool,
    pub links: Option<String>,
    pub edition: String,
    pub targets: Vec<Target>,
    pub declared_features: Vec<String>,
    pub proc_macro: bool,
    pub authors: Vec<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub rust_version: Option<String>,
    pub readme: Option<String>,
    pub default_run: Option<String>,
    pub deps: Vec<DepDecl>,
    /// One resolved node per `deps` entry. None if resolution didn't include it.
    pub dep_targets: Vec<Option<usize>>,
    pub lints: Option<toml::Table>,
}

impl Pkg {
    pub fn lib(&self) -> Option<usize> {
        self.targets.iter().position(|t| t.kind == TargetKind::Lib)
    }

    pub fn build_script(&self) -> Option<usize> {
        self.targets.iter().position(|t| t.kind == TargetKind::BuildScript)
    }

    pub fn display(&self) -> String {
        match &self.source {
            Source::Workspace | Source::Path => format!("{} v{} ({})", self.name, self.version, self.root.display()),
            _ => format!("{} v{}", self.name, self.version),
        }
    }
}

pub struct LoadOptions<'a> {
    pub manifest_path: Option<&'a Path>,
    pub locked: bool,
    pub offline: bool,
    pub ignore_rust_version: bool,
    /// `[source] replace-with` a directory: `(source id, vendor dir)`.
    pub vendor: Option<(String, PathBuf)>,
}

pub struct Env<'a> {
    pub home: &'a Path,
    pub rustc_version: Option<Version>,
    /// `[registries.<name>]`: name to source id.
    pub registries: BTreeMap<String, String>,
    /// `$CARGO_HOME/registry/cache`, so we can reuse `.crate` files already on disk.
    pub download_mirror: Option<PathBuf>,
    pub shell: &'a Shell,
}

pub struct Workspace {
    pub root: PathBuf,
    pub root_manifest_path: PathBuf,
    pub root_manifest: toml::Table,
    pub current_manifest: PathBuf,
    pub target_dir: PathBuf,
    pub resolver: ResolverVersion,
    pub graph: Graph,
    pub members: Vec<usize>,
    pub default_members: Vec<usize>,
    pub pkgs: Vec<Pkg>,
    /// What build scripts and rustc see as `CARGO`. That's rb.
    pub cargo: PathBuf,
    registry: Registry,
    git: Git,
    registries: BTreeMap<String, String>,
    patches: Patches,
    locals: usize,
    locked: bool,
    ignore_rust_version: bool,
    lock_version: u32,
    rustc_version: Option<Version>,
}

fn find_manifest(cwd: &Path) -> Result<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let m = d.join("Cargo.toml");
        if m.is_file() {
            return Ok(m);
        }
        dir = d.parent();
    }
    bail!("could not find `Cargo.toml` in `{}` or any parent directory", cwd.display())
}

fn glob_dirs(root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in patterns {
        let pattern = root.join(p);
        let s = pattern.to_string_lossy();
        if s.contains(['*', '?', '[']) {
            for entry in glob::glob(&s).with_context(|| format!("invalid workspace glob `{p}`"))? {
                let d = entry?;
                if d.join("Cargo.toml").is_file() {
                    out.push(clean_path(&d));
                }
            }
        } else {
            out.push(clean_path(&pattern));
        }
    }
    Ok(out)
}

fn strings(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn find_root(current: &Path) -> Result<PathBuf> {
    let t = read_toml(current)?;
    if t.contains_key("workspace") {
        return Ok(current.to_owned());
    }
    let dir = current.parent().unwrap();
    if let Some(ws) = t.get("package").and_then(|p| p.get("workspace")).and_then(|w| w.as_str()) {
        return Ok(clean_path(&dir.join(ws).join("Cargo.toml")));
    }
    let mut up = dir.parent();
    while let Some(d) = up {
        let m = d.join("Cargo.toml");
        if m.is_file()
            && let Some(w) = read_toml(&m)?.get("workspace").and_then(|w| w.as_table())
        {
            let excluded = strings(w.get("exclude")).iter().any(|e| dir.starts_with(clean_path(&d.join(e))));
            if !excluded && glob_dirs(d, &strings(w.get("members")))?.iter().any(|m| m == dir) {
                return Ok(m);
            }
        }
        up = d.parent();
    }
    Ok(current.to_owned())
}

pub fn locate_manifests(cwd: &Path, manifest_path: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
    let current = match manifest_path {
        Some(p) => std::path::absolute(p)?,
        None => find_manifest(cwd)?,
    };
    let root = find_root(&current)?;
    Ok((current, root))
}

pub fn workspace_root(cwd: &Path, manifest_path: Option<&Path>) -> Result<PathBuf> {
    let current = match manifest_path {
        Some(p) => std::path::absolute(p)?,
        None => find_manifest(cwd)?,
    };
    Ok(find_root(&current)?.parent().unwrap().to_owned())
}

fn detect_resolver(root_manifest: &toml::Table, root_edition: Option<&str>) -> ResolverVersion {
    let explicit = root_manifest
        .get("workspace")
        .and_then(|w| w.get("resolver"))
        .or_else(|| root_manifest.get("package").and_then(|p| p.get("resolver")))
        .and_then(|r| r.as_str());
    match explicit {
        Some("1") => ResolverVersion::V1,
        Some("2") => ResolverVersion::V2,
        Some("3") => ResolverVersion::V3,
        _ => match root_edition {
            Some("2024") => ResolverVersion::V3,
            Some("2021") => ResolverVersion::V2,
            _ => ResolverVersion::V1,
        },
    }
}

fn lookup(registries: &BTreeMap<String, String>) -> impl Fn(&str) -> Result<String> + '_ {
    |name: &str| {
        registries
            .get(name)
            .cloned()
            .with_context(|| format!("registry `{name}` is not configured in `.cargo/config.toml`"))
    }
}

fn load_local(root: &Path, root_manifest: &toml::Table, registries: &BTreeMap<String, String>, dir: &Path) -> Result<Package> {
    let manifest = dir.join("Cargo.toml");
    let lookup = lookup(registries);
    match root_manifest.get("workspace").and_then(|w| w.as_table()) {
        Some(table) if dir.starts_with(root) => {
            let ctx = WsContext { root, table };
            manifest::parse_package(&manifest, &read_toml(&manifest)?, Some(&ctx), &lookup)
        }
        _ => load_package(&manifest, None, &lookup),
    }
}

fn spec_names(specs: &[String], lock: Option<&lockfile::Lockfile>) -> Result<HashSet<String>> {
    let pkgs = lock.map(|l| l.packages.as_slice()).unwrap_or_default();
    let mut names = HashSet::new();
    for spec in specs {
        let (name, ver) = match spec.split_once(['@', ':']) {
            Some((n, v)) => (n, Some(v)),
            None => (spec.as_str(), None),
        };
        let hit = pkgs
            .iter()
            .any(|p| p.name == name && ver.is_none_or(|v| p.version.to_string().starts_with(v)));
        if !hit {
            bail!("package ID specification `{spec}` did not match any packages");
        }
        names.insert(name.to_owned());
    }
    Ok(names)
}

fn transitive_names(roots: &HashSet<String>, lock: Option<&lockfile::Lockfile>) -> HashSet<String> {
    let mut names = roots.clone();
    let Some(lock) = lock else { return names };
    loop {
        let before = names.len();
        for p in &lock.packages {
            if names.contains(&p.name) {
                for d in &p.dependencies {
                    names.insert(d.name.clone());
                }
            }
        }
        if names.len() == before {
            break;
        }
    }
    names
}

fn report_lock_changes(shell: &Shell, old: Option<&lockfile::Lockfile>, new: &lockfile::Lockfile) {
    let key = |p: &lockfile::LockPkg| (p.name.clone(), p.version.to_string(), p.source.clone());
    let old_set: HashSet<_> = old.map(|l| l.packages.iter().map(key).collect()).unwrap_or_default();
    let new_set: HashSet<_> = new.packages.iter().map(key).collect();
    let mut removed: Vec<_> = old_set.difference(&new_set).cloned().collect();
    let mut added: Vec<_> = new_set.difference(&old_set).cloned().collect();
    removed.sort();
    added.sort();
    for (name, version, _) in &removed {
        shell.status("Removing", format!("{name} v{version}"));
    }
    for (name, version, _) in &added {
        shell.status("Adding", format!("{name} v{version}"));
    }
}

impl Workspace {
    fn load_local(&self, dir: &Path) -> Result<Package> {
        load_local(&self.root, &self.root_manifest, &self.registries, dir)
    }

    fn sources<'a>(&'a self, lookup: RegistryLookup<'a>, load_path: &'a dyn Fn(&Path) -> Result<Package>, shell: &'a Shell) -> Sources<'a> {
        Sources {
            registry: &self.registry,
            git: &self.git,
            registries: lookup,
            patches: &self.patches,
            rustc_version: self.rustc_version.clone(),
            msrv_aware: self.resolver >= ResolverVersion::V3 && !self.ignore_rust_version,
            shell,
            load_path,
        }
    }

    pub fn load(opts: &LoadOptions<'_>, cwd: &Path, env: &Env<'_>) -> Result<Self> {
        let current_manifest = match opts.manifest_path {
            Some(p) => std::path::absolute(p)?,
            None => find_manifest(cwd)?,
        };
        let root_manifest_path = find_root(&current_manifest)?;
        let root = root_manifest_path.parent().unwrap().to_owned();
        let root_manifest = read_toml(&root_manifest_path)?;
        let ws_table = root_manifest.get("workspace").and_then(|w| w.as_table()).cloned();
        // The root package's edition picks the default resolver, including `edition.workspace = true`.
        let root_edition = match root_manifest.get("package").and_then(|p| p.get("edition")) {
            Some(toml::Value::Table(t)) if t.get("workspace").and_then(|w| w.as_bool()) == Some(true) => ws_table
                .as_ref()
                .and_then(|w| w.get("package"))
                .and_then(|p| p.get("edition"))
                .and_then(|e| e.as_str())
                .map(str::to_owned),
            Some(e) => e.as_str().map(str::to_owned),
            None => None,
        };
        let mut ws = Workspace {
            target_dir: root.join("target"),
            resolver: detect_resolver(&root_manifest, root_edition.as_deref()),
            graph: Graph::default(),
            members: Vec::new(),
            default_members: Vec::new(),
            pkgs: Vec::new(),
            cargo: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rb")),
            registry: {
                let registry = Registry::new(env.home.join("registry"), opts.offline, env.download_mirror.clone());
                if let Some((source, dir)) = &opts.vendor {
                    registry.set_vendor(source.clone(), dir.clone());
                }
                registry
            },
            git: Git::new(env.home.join("git"), opts.offline),
            registries: env.registries.clone(),
            patches: Patches::new(),
            locals: 0,
            locked: opts.locked,
            ignore_rust_version: opts.ignore_rust_version,
            lock_version: 4,
            rustc_version: env.rustc_version.clone(),
            root_manifest_path,
            root_manifest,
            current_manifest,
            root,
        };

        // Listed globs minus excludes, the root package, then path deps that live inside the root.
        let mut member_dirs: Vec<PathBuf> = Vec::new();
        let excludes: Vec<PathBuf> = ws_table
            .as_ref()
            .map(|w| strings(w.get("exclude")))
            .unwrap_or_default()
            .iter()
            .map(|e| clean_path(&ws.root.join(e)))
            .collect();
        if ws.root_manifest.contains_key("package") {
            member_dirs.push(ws.root.clone());
        }
        if let Some(w) = &ws_table {
            for d in glob_dirs(&ws.root, &strings(w.get("members")))? {
                if !excludes.iter().any(|e| d.starts_with(e)) && !member_dirs.contains(&d) {
                    member_dirs.push(d);
                }
            }
        }
        let mut queue = member_dirs.clone();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        while let Some(dir) = queue.pop() {
            if !seen.insert(dir.clone()) {
                continue;
            }
            let pkg = ws
                .load_local(&dir)
                .with_context(|| format!("failed to load manifest at {}", dir.join("Cargo.toml").display()))?;
            for d in &pkg.deps {
                if let manifest::DepSource::Path(p) = &d.source {
                    let inside = ws_table.is_some() && p.starts_with(&ws.root) && !excludes.iter().any(|e| p.starts_with(e));
                    if inside && !member_dirs.contains(p) {
                        member_dirs.push(p.clone());
                    }
                    queue.push(p.clone());
                }
            }
            let mut node = Node::new(PkgId {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                source: SourceId::Path(dir.clone()),
            });
            node.is_member = member_dirs.contains(&dir);
            node.package = Some(pkg);
            ws.graph.add(node);
        }
        // A path dep found later may have turned into a member.
        for n in &mut ws.graph.nodes {
            if let SourceId::Path(p) = &n.id.source {
                n.is_member = member_dirs.contains(p);
            }
        }
        ws.locals = ws.graph.nodes.len();
        ws.members = (0..ws.locals).filter(|&i| ws.graph.nodes[i].is_member).collect();
        ws.default_members = match ws_table.as_ref().map(|w| strings(w.get("default-members"))) {
            Some(dm) if !dm.is_empty() => {
                let dirs = glob_dirs(&ws.root, &dm)?;
                ws.members
                    .iter()
                    .copied()
                    .filter(|&m| matches!(&ws.graph.nodes[m].id.source, SourceId::Path(p) if dirs.contains(p)))
                    .collect()
            }
            _ if ws.root_manifest.contains_key("package") => ws.graph.path_node(&ws.root).into_iter().collect(),
            _ => ws.members.clone(),
        };

        if let Some(patch) = ws.root_manifest.get("patch").and_then(|p| p.as_table()).cloned() {
            let registries = ws.registries.clone();
            let lookup = lookup(&registries);
            for (src, entries) in &patch {
                let source = match src.as_str() {
                    "crates-io" | "https://github.com/rust-lang/crates.io-index" => CRATES_IO.to_owned(),
                    s if s.contains("://") => s.trim_end_matches('/').to_owned(),
                    name => lookup(name)?,
                };
                let mut decls = Vec::new();
                for (name, v) in entries.as_table().into_iter().flatten() {
                    decls.push(manifest::parse_dep(name, v, DepKind::Normal, None, &ws.root, None, &lookup)?);
                }
                ws.patches.insert(source, decls);
            }
        }

        // The old form of `[patch]`: `"name:version" = { path = "..." }`.
        if let Some(replace) = ws.root_manifest.get("replace").and_then(|p| p.as_table()) {
            let registries = ws.registries.clone();
            let lookup = lookup(&registries);
            let mut decls = Vec::new();
            for (spec, value) in replace {
                let spec = spec.rsplit('#').next().unwrap_or(spec);
                let (name, ver) = spec
                    .split_once(':')
                    .with_context(|| format!("`[replace]` key `{spec}` must be `name:version`"))?;
                let mut value = value.clone();
                if let Some(table) = value.as_table_mut()
                    && !table.contains_key("version")
                {
                    table.insert("version".into(), toml::Value::String(ver.to_owned()));
                }
                decls.push(manifest::parse_dep(name, &value, DepKind::Normal, None, &ws.root, None, &lookup)?);
            }
            ws.patches.entry(CRATES_IO.to_owned()).or_default().extend(decls);
        }

        // Use Cargo.lock when it covers every local dependency. Otherwise re-resolve.
        let lock_path = ws.root.join("Cargo.lock");
        let lock = lockfile::read(&lock_path)?;
        if let Some(l) = &lock {
            ws.lock_version = l.version;
            ws.graph.import_lock(l)?;
            ws.graph.mark_lock_patches(&ws.patches);
        }
        if lock.is_none() || !ws.graph.locals_complete() {
            ws.relock(lock.as_ref(), env.shell)?;
        }
        Ok(ws)
    }

    fn relock(&mut self, lock: Option<&lockfile::Lockfile>, shell: &Shell) -> Result<()> {
        let lock_path = self.root.join("Cargo.lock");
        if self.locked {
            bail!(
                "the lock file {} needs to be updated but --locked was passed to prevent this",
                lock_path.display()
            );
        }
        let (g, new) = self.resolve_from_locals(&Prefs::from_lock(lock), shell)?;
        self.commit_lock(&new, false, shell)?;
        self.graph = g;
        Ok(())
    }

    /// No specs: every locked crate, up to what its requirement allows.
    /// Specs: just those, and with `recursive` whatever they lock too.
    pub fn update(&mut self, specs: &[String], precise: Option<&str>, recursive: bool, dry_run: bool, shell: &Shell) -> Result<()> {
        let lock_path = self.root.join("Cargo.lock");
        let old = lockfile::read(&lock_path)?;
        if precise.is_some() && specs.len() != 1 {
            bail!("--precise requires exactly one package to update");
        }
        let mut prefs = Prefs::from_lock(old.as_ref());
        if specs.is_empty() {
            prefs.forget_all();
        } else {
            let mut names = spec_names(specs, old.as_ref())?;
            if recursive {
                names = transitive_names(&names, old.as_ref());
            }
            if let Some(precise) = precise {
                let name = names.iter().next().unwrap();
                if let Ok(version) = Version::parse(precise) {
                    prefs.pin_version(name, version);
                } else {
                    prefs.pin_git(name, precise, old.as_ref());
                }
            } else {
                prefs.forget(&names, old.as_ref());
            }
        }
        let (g, new) = self.resolve_from_locals(&prefs, shell)?;
        if let (Some(precise), [spec]) = (precise, specs) {
            let name = spec.split_once(['@', ':']).map(|(n, _)| n).unwrap_or(spec.as_str());
            let hit = Version::parse(precise)
                .ok()
                .is_some_and(|version| new.packages.iter().any(|p| p.name == name && p.version == version))
                || new.packages.iter().any(|p| {
                    p.name == name
                        && p.source
                            .as_deref()
                            .is_some_and(|s| s.ends_with(precise) || s.contains(&format!("#{precise}")))
                });
            if !hit {
                bail!("could not update `{name}` to `{precise}`");
            }
        }
        report_lock_changes(shell, old.as_ref(), &new);
        if self.locked && old.as_ref().is_none_or(|o| o.render() != new.render()) {
            bail!(
                "the lock file {} needs to be updated but --locked was passed to prevent this",
                lock_path.display()
            );
        }
        if !dry_run {
            self.commit_lock(&new, true, shell)?;
            self.graph = g;
        }
        Ok(())
    }

    fn resolve_from_locals(&self, prefs: &Prefs, shell: &Shell) -> Result<(Graph, lockfile::Lockfile)> {
        let mut g = Graph::default();
        for n in &self.graph.nodes[..self.locals] {
            let mut fresh = Node::new(n.id.clone());
            fresh.is_member = n.is_member;
            fresh.package = n.package.clone();
            g.add(fresh);
        }
        let registries = self.registries.clone();
        let lookup = lookup(&registries);
        let load_path = |p: &Path| self.load_local(p);
        let src = self.sources(&lookup, &load_path, shell);
        let out = resolve::resolve_lock_mode(&mut g, &self.members, prefs, &src)?;
        let lock = resolve::to_lockfile(&g, &out, &self.members, self.lock_version);
        Ok((g, lock))
    }

    fn commit_lock(&self, new: &lockfile::Lockfile, quiet: bool, shell: &Shell) -> Result<()> {
        let lock_path = self.root.join("Cargo.lock");
        let text = new.render();
        let old = std::fs::read_to_string(&lock_path).ok();
        if old.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        if !quiet {
            match new.packages.iter().filter(|p| p.source.is_some()).count() {
                0 => {}
                1 => shell.status("Locking", "1 package to latest compatible version"),
                n => shell.status("Locking", format!("{n} packages to latest compatible versions")),
            }
        }
        std::fs::write(&lock_path, text).with_context(|| format!("failed to write {}", lock_path.display()))?;
        Ok(())
    }

    /// The caret req cargo would write for the latest crates.io version of `name`.
    pub fn crates_io_req(&self, name: &str) -> Result<String> {
        let entries = self.registry.entries(CRATES_IO, name)?;
        let best = entries
            .iter()
            .filter(|e| !e.yanked)
            .max_by(|a, b| a.vers.cmp(&b.vers))
            .with_context(|| format!("no matching package named `{name}` found"))?;
        Ok(if best.vers.major == 0 {
            format!("0.{}", best.vers.minor)
        } else {
            best.vers.major.to_string()
        })
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    pub fn members(&self) -> impl Iterator<Item = usize> + '_ {
        self.members.iter().copied()
    }

    /// `-p`, or `--workspace` minus `--exclude`, or the package that owns the current manifest, or `default-members`.
    pub fn select(&self, packages: &[String], workspace: bool, exclude: &[String]) -> Result<Vec<usize>> {
        let nodes = &self.graph.nodes;
        let matches = |spec: &str, i: usize| {
            let id = &nodes[i].id;
            match spec.split_once(['@', ':']) {
                Some((name, ver)) => id.name == name && id.version.to_string().starts_with(ver),
                None => id.name == spec,
            }
        };
        if workspace {
            for e in exclude {
                if !self.members().any(|i| matches(e, i)) {
                    bail!("excluded package `{e}` is not a workspace member");
                }
            }
            return Ok(self.members().filter(|&i| !exclude.iter().any(|e| matches(e, i))).collect());
        }
        if !packages.is_empty() {
            let mut out = Vec::new();
            for spec in packages {
                let found: Vec<usize> = (0..nodes.len()).filter(|&i| matches(spec, i)).collect();
                match found.as_slice() {
                    [] => bail!("package ID specification `{spec}` did not match any packages"),
                    [one] => out.push(*one),
                    many => {
                        let members: Vec<usize> = many.iter().copied().filter(|&i| nodes[i].is_member).collect();
                        if members.len() == 1 {
                            out.push(members[0]);
                        } else {
                            let versions: Vec<String> = many
                                .iter()
                                .map(|&i| format!("{}@{}", nodes[i].id.name, nodes[i].id.version))
                                .collect();
                            bail!(
                                "package ID specification `{spec}` is ambiguous, candidates: {}",
                                versions.join(", ")
                            );
                        }
                    }
                }
            }
            return Ok(out);
        }
        if self.current_manifest != self.root_manifest_path
            && let Some(i) = self
                .members()
                .find(|&i| matches!(&nodes[i].id.source, SourceId::Path(p) if p.join("Cargo.toml") == self.current_manifest))
        {
            return Ok(vec![i]);
        }
        Ok(self.default_members.clone())
    }

    fn requests(&self, selected: &[usize], req: &FeatureRequest<'_>) -> Result<Vec<Request>> {
        let requested: Vec<&str> = req
            .features
            .iter()
            .flat_map(|f| f.split([',', ' ']))
            .filter(|f| !f.is_empty())
            .collect();
        let mut reqs: Vec<Request> = selected
            .iter()
            .map(|&n| Request {
                node: n,
                features: Vec::new(),
                all_features: req.all_features,
                default: !req.no_default_features,
            })
            .collect();
        for f in requested {
            let mut found = false;
            if let Some((owner, feat)) = f.split_once('/') {
                for r in &mut reqs {
                    let node = &self.graph.nodes[r.node];
                    if node.id.name == owner {
                        r.features.push(feat.to_owned());
                        found = true;
                    } else if node.deps().iter().any(|d| d.dep_name() == owner) {
                        r.features.push(f.to_owned());
                        found = true;
                    }
                }
            } else {
                for r in &mut reqs {
                    let node = &self.graph.nodes[r.node];
                    if node.features().contains_key(f) || !node.has_data() {
                        r.features.push(f.to_owned());
                        found = true;
                    }
                }
            }
            if !found {
                bail!("none of the selected packages contains the feature `{f}`");
            }
        }
        Ok(reqs)
    }

    pub fn fetch(&mut self, shell: &Shell) -> Result<usize> {
        let nodes: Vec<usize> = (0..self.graph.nodes.len())
            .filter(|&n| self.graph.nodes[n].package.is_none())
            .collect();
        let n = nodes.len();
        if n > 0 {
            self.load_packages(&nodes, shell)?;
        }
        Ok(n)
    }

    fn load_packages(&mut self, nodes: &[usize], shell: &Shell) -> Result<()> {
        let registries = self.registries.clone();
        let lookup = lookup(&registries);
        type Loaded = (usize, Result<(Package, bool)>);
        let results: Mutex<Vec<Loaded>> = Mutex::default();
        let jobs: Vec<(usize, PkgId, Option<String>)> = nodes
            .iter()
            .map(|&n| (n, self.graph.nodes[n].id.clone(), self.graph.nodes[n].checksum.clone()))
            .collect();
        parallel(&jobs, 8, |(n, id, checksum)| {
            let r = (|| -> Result<(Package, bool)> {
                match &id.source {
                    SourceId::Registry(s) => {
                        let (dir, downloaded) = self.registry.ensure(&Download {
                            source: s,
                            name: &id.name,
                            version: &id.version,
                            checksum: checksum.as_deref(),
                        })?;
                        Ok((load_package(&dir.join("Cargo.toml"), Some(&dir), &lookup)?, downloaded))
                    }
                    SourceId::Git { url, commit, .. } => {
                        let checkout = self.git.checkout(url, commit)?;
                        let manifest = crate::git::find_package(&checkout, &id.name)?;
                        Ok((load_package(&manifest, Some(&checkout), &lookup)?, false))
                    }
                    SourceId::Path(p) => Ok((self.load_local(p)?, false)),
                }
            })();
            results
                .lock()
                .unwrap()
                .push((*n, r.with_context(|| format!("failed to load `{}@{}`", id.name, id.version))));
        });
        let mut downloaded = 0;
        for (n, r) in results.into_inner().unwrap() {
            let (pkg, fresh) = r?;
            downloaded += usize::from(fresh);
            self.graph.nodes[n].package = Some(pkg);
        }
        if downloaded > 0 {
            shell.status(
                "Downloaded",
                format!("{downloaded} crate{}", if downloaded == 1 { "" } else { "s" }),
            );
        }
        Ok(())
    }

    /// Features for `selected` on `platforms`. Downloads and checkouts run in parallel.
    pub fn resolve_features(
        &mut self,
        selected: &[usize],
        req: &FeatureRequest<'_>,
        platforms: &Platforms<'_>,
        dev_for: &HashSet<usize>,
        shell: &Shell,
    ) -> Result<Resolution> {
        let two_sides = self.resolver >= ResolverVersion::V2;
        let mut relocked = false;
        loop {
            let reqs = self.requests(selected, req)?;
            let opts = EngineOpts {
                two_sides,
                platforms: two_sides.then_some(platforms),
                dev_for,
                needs_package: true,
                weak_activates: false,
            };
            let out = features::resolve(&self.graph, &reqs, &opts)?;
            if !out.missing.is_empty() {
                let (n, i) = out.missing[0];
                let what = format!(
                    "`{}` (dependency of `{}`)",
                    self.graph.nodes[n].deps()[i].name,
                    self.graph.nodes[n].id.name
                );
                if relocked || selected.iter().any(|&s| s >= self.locals) {
                    bail!("{what} is not in Cargo.lock; delete Cargo.lock to re-resolve");
                }
                let lock = lockfile::read(&self.root.join("Cargo.lock"))?;
                self.relock(lock.as_ref(), shell)?;
                relocked = true;
                continue;
            }
            if out.unloaded.is_empty() {
                return Ok(Resolution { map: out.info, two_sides });
            }
            let todo: Vec<usize> = out.unloaded.into_iter().collect();
            self.load_packages(&todo, shell)?;
        }
    }

    pub fn finalize(&mut self) {
        let g = &self.graph;
        self.pkgs = (0..g.nodes.len())
            .map(|n| {
                let node = &g.nodes[n];
                let id = node.id.spec();
                let (source, identity) = match &node.id.source {
                    SourceId::Path(_) if node.is_member => (Source::Workspace, id.clone()),
                    SourceId::Path(_) => (Source::Path, id.clone()),
                    SourceId::Registry(s) => (
                        Source::Registry(s.clone()),
                        format!("{id}#{}", node.checksum.clone().unwrap_or_default()),
                    ),
                    SourceId::Git { commit, .. } => (
                        Source::Git(node.id.source.lock_string().unwrap_or_default()),
                        format!("{id}#{commit}"),
                    ),
                };
                match &node.package {
                    Some(p) => Pkg {
                        id,
                        identity,
                        name: p.name.clone(),
                        version: p.version.clone(),
                        manifest_path: p.manifest_path.clone(),
                        root: p.root.clone(),
                        source,
                        is_member: node.is_member,
                        loaded: true,
                        links: p.links.clone(),
                        edition: p.edition.clone(),
                        targets: p.targets.clone(),
                        declared_features: p.features.keys().cloned().collect(),
                        proc_macro: p.targets.iter().any(|t| t.is_proc_macro()),
                        authors: p.authors.clone(),
                        description: p.description.clone(),
                        homepage: p.homepage.clone(),
                        repository: p.repository.clone(),
                        license: p.license.clone(),
                        license_file: p.license_file.clone(),
                        rust_version: p.rust_version.clone(),
                        readme: p.readme.clone(),
                        default_run: p.default_run.clone(),
                        dep_targets: p.deps.iter().map(|d| g.target_of(n, d)).collect(),
                        deps: p.deps.clone(),
                        lints: p.lints.clone(),
                    },
                    None => Pkg {
                        id,
                        identity,
                        name: node.id.name.clone(),
                        version: node.id.version.clone(),
                        manifest_path: PathBuf::new(),
                        root: PathBuf::new(),
                        source,
                        is_member: node.is_member,
                        loaded: false,
                        links: None,
                        edition: "2015".into(),
                        targets: Vec::new(),
                        declared_features: Vec::new(),
                        proc_macro: false,
                        authors: Vec::new(),
                        description: None,
                        homepage: None,
                        repository: None,
                        license: None,
                        license_file: None,
                        rust_version: None,
                        readme: None,
                        default_run: None,
                        deps: Vec::new(),
                        dep_targets: Vec::new(),
                        lints: None,
                    },
                }
            })
            .collect();
    }
}

pub struct FeatureRequest<'a> {
    pub features: &'a [String],
    pub all_features: bool,
    pub no_default_features: bool,
}

/// Feature sets, unioned across the requested targets.
#[derive(Debug, Default)]
pub struct Resolution {
    map: HashMap<(usize, bool), FeatureInfo>,
    two_sides: bool,
}

impl Resolution {
    pub fn info(&self, pkg: usize, host: bool) -> Option<&FeatureInfo> {
        self.map.get(&(pkg, self.two_sides && host))
    }
}

pub fn dep_enabled(dep: &DepDecl, fi: &FeatureInfo, platform: &PlatformCfg<'_>) -> bool {
    if dep.optional && !fi.optional_deps.contains(dep.dep_name()) {
        return false;
    }
    dep.platform.as_ref().is_none_or(|p| p.matches(platform))
}
