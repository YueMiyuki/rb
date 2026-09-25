//! `Cargo.toml`: inheritance, deps, implicit features, target discovery.

use crate::cfgexpr::PlatformExpr;
use anyhow::{Context, Result, bail};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

/// crates.io, and the id its sparse index uses too.
pub const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DepKind {
    Normal,
    Build,
    Dev,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GitRef {
    DefaultBranch,
    Branch(String),
    Tag(String),
    Rev(String),
}

impl GitRef {
    /// `?branch=main` and friends, as they show up in source ids.
    pub fn query(&self) -> String {
        match self {
            Self::DefaultBranch => String::new(),
            Self::Branch(b) => format!("?branch={b}"),
            Self::Tag(t) => format!("?tag={t}"),
            Self::Rev(r) => format!("?rev={r}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DepSource {
    Registry(String),
    Path(PathBuf),
    Git { url: String, reference: GitRef },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DepDecl {
    pub name: String,
    /// Manifest key when it isn't the package name. `foo = { package = "bar" }`.
    pub rename: Option<String>,
    pub req: VersionReq,
    pub source: DepSource,
    pub kind: DepKind,
    pub optional: bool,
    pub default_features: bool,
    pub features: Vec<String>,
    pub platform: Option<PlatformExpr>,
}

impl DepDecl {
    /// What features call it: `dep:<name>`, `<name>/feat`.
    pub fn dep_name(&self) -> &str {
        self.rename.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TargetKind {
    Lib,
    Bin,
    Example,
    Test,
    Bench,
    BuildScript,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    pub kind: TargetKind,
    pub crate_types: Vec<String>,
    pub src_path: PathBuf,
    pub edition: String,
    pub required_features: Vec<String>,
    /// On for `test`, `bench`, and `doc`. Also whether doctests run.
    pub test: bool,
    pub bench: bool,
    pub doc: bool,
    pub doctest: bool,
    pub harness: bool,
}

impl Target {
    pub fn crate_name(&self) -> String {
        self.name.replace('-', "_")
    }

    pub fn is_proc_macro(&self) -> bool {
        self.crate_types.iter().any(|c| c == "proc-macro")
    }

    pub fn is_executable(&self) -> bool {
        matches!(
            self.kind,
            TargetKind::Bin | TargetKind::Test | TargetKind::Bench | TargetKind::BuildScript
        ) || (self.kind == TargetKind::Example && self.crate_types.iter().any(|c| c == "bin"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: Version,
    pub edition: String,
    pub manifest_path: PathBuf,
    pub root: PathBuf,
    pub links: Option<String>,
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
    pub features: BTreeMap<String, Vec<String>>,
    pub targets: Vec<Target>,
    /// `[lints]`, with `workspace = true` already filled in.
    pub lints: Option<toml::Table>,
}

pub fn read_toml(path: &Path) -> Result<toml::Table> {
    let text = std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn clean_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The `[workspace]` this package inherits from.
pub struct WsContext<'a> {
    pub root: &'a Path,
    pub table: &'a toml::Table,
}

pub type RegistryLookup<'a> = &'a dyn Fn(&str) -> Result<String>;

fn get<'a>(t: &'a toml::Table, keys: &[&str]) -> Option<&'a toml::Value> {
    keys.iter().find_map(|k| t.get(*k))
}

fn is_inherit(v: &toml::Value) -> bool {
    v.get("workspace").and_then(|w| w.as_bool()) == Some(true)
}

fn inherited<'a>(pkg: &'a toml::Table, key: &str, ws: Option<&'a WsContext<'_>>) -> Result<Option<&'a toml::Value>> {
    match pkg.get(key) {
        Some(v) if is_inherit(v) => {
            let ws = ws.with_context(|| format!("`{key}.workspace = true` but the package is not in a workspace"))?;
            let v = ws
                .table
                .get("package")
                .and_then(|p| p.get(key))
                .with_context(|| format!("`{key}` is inherited but `workspace.package.{key}` is not set"))?;
            Ok(Some(v))
        }
        other => Ok(other),
    }
}

fn str_field(pkg: &toml::Table, key: &str, ws: Option<&WsContext<'_>>) -> Result<Option<String>> {
    Ok(inherited(pkg, key, ws)?.and_then(|v| v.as_str()).map(str::to_owned))
}

fn strings(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn bool_or(t: &toml::Table, keys: &[&str], default: bool) -> bool {
    get(t, keys).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn parse_req(s: &str, ctx: &str) -> Result<VersionReq> {
    VersionReq::parse(s.trim()).with_context(|| format!("invalid version requirement `{s}` for {ctx}"))
}

/// One dependency entry. `root` is where relative paths resolve.
#[allow(clippy::too_many_arguments)]
pub fn parse_dep(
    key: &str,
    v: &toml::Value,
    kind: DepKind,
    platform: Option<&PlatformExpr>,
    root: &Path,
    ws: Option<&WsContext<'_>>,
    registries: RegistryLookup<'_>,
) -> Result<DepDecl> {
    let mut decl = DepDecl {
        name: key.to_owned(),
        rename: None,
        req: VersionReq::STAR,
        source: DepSource::Registry(CRATES_IO.to_owned()),
        kind,
        optional: false,
        default_features: true,
        features: Vec::new(),
        platform: platform.cloned(),
    };
    let t = match v {
        toml::Value::String(req) => {
            decl.req = parse_req(req, key)?;
            return Ok(decl);
        }
        toml::Value::Table(t) => t,
        _ => bail!("invalid dependency specification for `{key}`"),
    };
    if is_inherit(v) {
        let ws = ws.with_context(|| format!("dependency `{key}` uses `workspace = true` outside a workspace"))?;
        let base = ws
            .table
            .get("dependencies")
            .and_then(|d| d.get(key))
            .with_context(|| format!("dependency `{key}` is inherited but not declared in `[workspace.dependencies]`"))?;
        let mut d = parse_dep(key, base, kind, platform, ws.root, None, registries)?;
        d.features.extend(strings(t.get("features")));
        d.optional = t.get("optional").and_then(|o| o.as_bool()).unwrap_or(false);
        return Ok(d);
    }
    if t.contains_key("artifact") {
        bail!("dependency `{key}` is an artifact dependency, which rb does not support");
    }
    if let Some(p) = t.get("package").and_then(|p| p.as_str()) {
        decl.rename = Some(key.to_owned());
        decl.name = p.to_owned();
    }
    if let Some(req) = t.get("version").and_then(|v| v.as_str()) {
        decl.req = parse_req(req, key)?;
    }
    decl.optional = bool_or(t, &["optional"], false);
    decl.default_features = bool_or(t, &["default-features", "default_features"], true);
    decl.features = strings(t.get("features"));
    if let Some(path) = t.get("path").and_then(|p| p.as_str()) {
        decl.source = DepSource::Path(clean_path(&root.join(path)));
    } else if let Some(url) = t.get("git").and_then(|g| g.as_str()) {
        let reference = if let Some(b) = t.get("branch").and_then(|v| v.as_str()) {
            GitRef::Branch(b.to_owned())
        } else if let Some(tag) = t.get("tag").and_then(|v| v.as_str()) {
            GitRef::Tag(tag.to_owned())
        } else if let Some(rev) = t.get("rev").and_then(|v| v.as_str()) {
            GitRef::Rev(rev.to_owned())
        } else {
            GitRef::DefaultBranch
        };
        decl.source = DepSource::Git {
            url: url.trim_end_matches('/').to_owned(),
            reference,
        };
    } else if let Some(reg) = t.get("registry").and_then(|r| r.as_str()) {
        decl.source = DepSource::Registry(registries(reg)?);
    } else if let Some(index) = t.get("registry-index").and_then(|r| r.as_str()) {
        // A published manifest names other registries by index URL.
        let u = index.trim_end_matches('/');
        decl.source = DepSource::Registry(
            if u == "https://github.com/rust-lang/crates.io-index" || u.ends_with("index.crates.io") {
                CRATES_IO.to_owned()
            } else if u.starts_with("sparse+") {
                format!("{u}/")
            } else {
                format!("registry+{u}")
            },
        );
    }
    Ok(decl)
}

/// Inherit from the nearest `[workspace]`, stopping at `boundary` if there is one.
pub fn load_package(path: &Path, boundary: Option<&Path>, registries: RegistryLookup<'_>) -> Result<Package> {
    let table = read_toml(path)?;
    let mut ws_root = None;
    let mut dir = path.parent();
    while let Some(d) = dir {
        let m = d.join("Cargo.toml");
        if m == path {
            if table.contains_key("workspace") {
                ws_root = Some((d.to_owned(), table.clone()));
                break;
            }
        } else if m.is_file()
            && let Ok(t) = read_toml(&m)
            && t.contains_key("workspace")
        {
            ws_root = Some((d.to_owned(), t));
            break;
        }
        if boundary.is_some_and(|b| d == b) {
            break;
        }
        dir = d.parent();
    }
    let ws_table = ws_root.as_ref().and_then(|(_, t)| t.get("workspace")).and_then(|w| w.as_table());
    let ctx = match (&ws_root, ws_table) {
        (Some((root, _)), Some(table)) => Some(WsContext { root, table }),
        _ => None,
    };
    parse_package(path, &table, ctx.as_ref(), registries)
}

fn dep_tables(t: &toml::Table) -> [(DepKind, Option<&toml::Value>); 3] {
    [
        (DepKind::Normal, t.get("dependencies")),
        (DepKind::Dev, get(t, &["dev-dependencies", "dev_dependencies"])),
        (DepKind::Build, get(t, &["build-dependencies", "build_dependencies"])),
    ]
}

pub fn parse_deps(manifest: &toml::Table, root: &Path, ws: Option<&WsContext<'_>>, registries: RegistryLookup<'_>) -> Result<Vec<DepDecl>> {
    let mut out = Vec::new();
    let mut push = |t: &toml::Table, platform: Option<&PlatformExpr>| -> Result<()> {
        for (kind, table) in dep_tables(t) {
            let Some(table) = table.and_then(|v| v.as_table()) else { continue };
            for (key, v) in table {
                out.push(parse_dep(key, v, kind, platform, root, ws, registries)?);
            }
        }
        Ok(())
    };
    push(manifest, None)?;
    if let Some(targets) = manifest.get("target").and_then(|t| t.as_table()) {
        for (spec, t) in targets {
            let platform = PlatformExpr::parse(spec).with_context(|| format!("invalid target specification `{spec}`"))?;
            if let Some(t) = t.as_table() {
                push(t, Some(&platform))?;
            }
        }
    }
    Ok(out)
}

/// Declared features, plus `name = ["dep:name"]` for optional deps no feature mentions with `dep:`.
pub fn features_with_implicit(declared: BTreeMap<String, Vec<String>>, deps: &[DepDecl]) -> BTreeMap<String, Vec<String>> {
    let mut out = declared;
    let explicit_dep: BTreeSet<&str> = out.values().flatten().filter_map(|v| v.strip_prefix("dep:")).collect();
    let mut implicit = Vec::new();
    for d in deps.iter().filter(|d| d.optional) {
        let n = d.dep_name();
        if !explicit_dep.contains(n) && !out.contains_key(n) {
            implicit.push(n.to_owned());
        }
    }
    for n in implicit {
        let v = vec![format!("dep:{n}")];
        out.insert(n, v);
    }
    out
}

struct TargetDefaults {
    kind: TargetKind,
    test: bool,
    bench: bool,
    doc: bool,
}

fn target_from_table(t: &toml::Table, d: &TargetDefaults, name: String, src_path: PathBuf, edition: &str) -> Target {
    let proc_macro = bool_or(t, &["proc-macro", "proc_macro"], false);
    let crate_types = if proc_macro {
        vec!["proc-macro".to_owned()]
    } else {
        let ct = strings(get(t, &["crate-type", "crate_type"]));
        if !ct.is_empty() {
            ct
        } else if d.kind == TargetKind::Lib {
            vec!["lib".to_owned()]
        } else {
            vec!["bin".to_owned()]
        }
    };
    Target {
        name,
        kind: d.kind,
        crate_types,
        src_path,
        edition: t.get("edition").and_then(|e| e.as_str()).unwrap_or(edition).to_owned(),
        required_features: strings(get(t, &["required-features", "required_features"])),
        test: bool_or(t, &["test"], d.test),
        bench: bool_or(t, &["bench"], d.bench),
        doc: bool_or(t, &["doc"], d.doc),
        doctest: bool_or(t, &["doctest"], d.kind == TargetKind::Lib),
        harness: bool_or(t, &["harness"], true),
    }
}

fn discover(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_file() && name.ends_with(".rs") && !name.starts_with('.') {
            out.push((name.trim_end_matches(".rs").to_owned(), p));
        } else if p.is_dir() && p.join("main.rs").is_file() {
            out.push((name, p.join("main.rs")));
        }
    }
    out.sort();
    out
}

fn discover_targets(manifest: &toml::Table, pkg: &toml::Table, root: &Path, name: &str, edition: &str) -> Result<Vec<Target>> {
    let mut targets = Vec::new();
    let auto = |key: &str| pkg.get(key).and_then(|v| v.as_bool()).unwrap_or(true);

    let lib_table = manifest.get("lib").and_then(|l| l.as_table());
    let lib_path = lib_table.and_then(|l| l.get("path")).and_then(|p| p.as_str()).map(|p| root.join(p));
    let default_lib = root.join("src/lib.rs");
    let lib_src = lib_path.or_else(|| (auto("autolib") && default_lib.is_file()).then_some(default_lib));
    if let Some(src) = lib_src {
        let empty = toml::Table::new();
        let t = lib_table.unwrap_or(&empty);
        let lib_name = t
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| name.replace('-', "_"));
        targets.push(target_from_table(
            t,
            &TargetDefaults {
                kind: TargetKind::Lib,
                test: true,
                bench: true,
                doc: true,
            },
            lib_name,
            src,
            edition,
        ));
    }

    type Section = (&'static str, &'static str, &'static str, TargetDefaults);
    let sections: [Section; 4] = [
        (
            "bin",
            "autobins",
            "src/bin",
            TargetDefaults {
                kind: TargetKind::Bin,
                test: true,
                bench: true,
                doc: true,
            },
        ),
        (
            "example",
            "autoexamples",
            "examples",
            TargetDefaults {
                kind: TargetKind::Example,
                test: false,
                bench: false,
                doc: false,
            },
        ),
        (
            "test",
            "autotests",
            "tests",
            TargetDefaults {
                kind: TargetKind::Test,
                test: true,
                bench: false,
                doc: false,
            },
        ),
        (
            "bench",
            "autobenches",
            "benches",
            TargetDefaults {
                kind: TargetKind::Bench,
                test: false,
                bench: true,
                doc: false,
            },
        ),
    ];
    for (key, auto_key, dir, defaults) in sections {
        let mut found: Vec<(String, PathBuf)> = Vec::new();
        if auto(auto_key) {
            if defaults.kind == TargetKind::Bin && root.join("src/main.rs").is_file() {
                found.push((name.to_owned(), root.join("src/main.rs")));
            }
            found.extend(discover(&root.join(dir)));
        }
        let explicit = manifest.get(key).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let mut seen = BTreeSet::new();
        for e in &explicit {
            let Some(t) = e.as_table() else { continue };
            let tname = t.get("name").and_then(|n| n.as_str()).map(str::to_owned);
            let path = match t.get("path").and_then(|p| p.as_str()) {
                Some(p) => root.join(p),
                None => {
                    let tname = tname
                        .as_deref()
                        .with_context(|| format!("a [[{key}]] target needs a `name` or `path`"))?;
                    let candidates = [
                        root.join(dir).join(format!("{tname}.rs")),
                        root.join(dir).join(tname).join("main.rs"),
                        if defaults.kind == TargetKind::Bin && tname == name {
                            root.join("src/main.rs")
                        } else {
                            PathBuf::new()
                        },
                    ];
                    candidates
                        .into_iter()
                        .find(|c| c.is_file())
                        .with_context(|| format!("can't find `{tname}` {key} target, specify its `path`"))?
                }
            };
            let tname = match tname {
                Some(n) => n,
                None => path.file_stem().unwrap().to_string_lossy().into_owned(),
            };
            seen.insert(tname.clone());
            targets.push(target_from_table(t, &defaults, tname, path, edition));
        }
        let empty = toml::Table::new();
        for (tname, path) in found {
            let dup_path = targets.iter().any(|t| t.kind == defaults.kind && t.src_path == path);
            if !seen.contains(&tname) && !dup_path {
                targets.push(target_from_table(&empty, &defaults, tname, path, edition));
            }
        }
    }

    let build = match pkg.get("build") {
        Some(toml::Value::Boolean(false)) => None,
        Some(toml::Value::String(p)) => Some(root.join(p)),
        _ => Some(root.join("build.rs")).filter(|p| p.is_file()),
    };
    if let Some(src) = build {
        // `build = "src/gen.rs"` is named `build-script-gen`.
        let stem = src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "build".into());
        targets.push(Target {
            name: format!("build-script-{stem}"),
            kind: TargetKind::BuildScript,
            crate_types: vec!["bin".into()],
            src_path: src,
            edition: edition.to_owned(),
            required_features: Vec::new(),
            test: false,
            bench: false,
            doc: false,
            doctest: false,
            harness: false,
        });
    }
    Ok(targets)
}

pub fn parse_package(path: &Path, table: &toml::Table, ws: Option<&WsContext<'_>>, registries: RegistryLookup<'_>) -> Result<Package> {
    let root = path.parent().unwrap().to_owned();
    if let Some(f) = table.get("cargo-features") {
        bail!("{} uses unstable `cargo-features = {f}`, which rb does not support", path.display());
    }
    let pkg = table
        .get("package")
        .or_else(|| table.get("project"))
        .and_then(|p| p.as_table())
        .with_context(|| format!("{} has no [package] section", path.display()))?;
    let name = pkg
        .get("name")
        .and_then(|n| n.as_str())
        .with_context(|| format!("{}: missing package name", path.display()))?
        .to_owned();
    let version = match inherited(pkg, "version", ws)?.and_then(|v| v.as_str()) {
        Some(v) => Version::parse(v).with_context(|| format!("invalid version `{v}` in {}", path.display()))?,
        None => Version::new(0, 0, 0),
    };
    let edition = str_field(pkg, "edition", ws)?.unwrap_or_else(|| "2015".into());
    let authors = strings(inherited(pkg, "authors", ws)?);
    let readme = match inherited(pkg, "readme", ws)? {
        Some(toml::Value::Boolean(false)) => None,
        Some(toml::Value::Boolean(true)) => Some("README.md".into()),
        Some(toml::Value::String(s)) => Some(s.clone()),
        _ => ["README.md", "README.txt", "README"]
            .iter()
            .find(|r| root.join(r).is_file())
            .map(|r| (*r).to_owned()),
    };
    let deps = parse_deps(table, &root, ws, registries)?;
    let declared: BTreeMap<String, Vec<String>> = table
        .get("features")
        .and_then(|f| f.as_table())
        .map(|t| t.iter().map(|(k, v)| (k.clone(), strings(Some(v)))).collect())
        .unwrap_or_default();
    let features = features_with_implicit(declared, &deps);
    let targets = discover_targets(table, pkg, &root, &name, &edition)?;
    let lints = match table.get("lints") {
        Some(v) if is_inherit(v) => ws.and_then(|w| w.table.get("lints")).and_then(|l| l.as_table()).cloned(),
        Some(v) => v.as_table().cloned(),
        None => None,
    };
    Ok(Package {
        name,
        version,
        edition,
        manifest_path: path.to_owned(),
        root,
        links: pkg.get("links").and_then(|l| l.as_str()).map(str::to_owned),
        authors,
        description: str_field(pkg, "description", ws)?,
        homepage: str_field(pkg, "homepage", ws)?,
        repository: str_field(pkg, "repository", ws)?,
        license: str_field(pkg, "license", ws)?,
        license_file: str_field(pkg, "license-file", ws)?,
        rust_version: str_field(pkg, "rust-version", ws)?,
        readme,
        default_run: pkg.get("default-run").and_then(|d| d.as_str()).map(str::to_owned),
        deps,
        features,
        targets,
        lints,
    })
}

/// `false` if that key wasn't in `section`.
pub fn remove_dependency(text: &str, section: &str, key: &str) -> Option<String> {
    let header = format!("[{section}]");
    let mut owned: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    let start = owned.iter().position(|l| l.trim() == header)?;
    let end = owned
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, l)| l.trim_start().starts_with('['))
        .map(|(i, _)| i)
        .unwrap_or(owned.len());
    let key_at = owned.iter().enumerate().take(end).skip(start + 1).find(|(_, l)| {
        let t = l.trim();
        t.starts_with(&format!("{key} ")) || t.starts_with(&format!("{key}="))
    })?;
    owned.remove(key_at.0);
    Some(owned.concat())
}

/// One dependency line, in or replaced. The rest of the file stays as it was.
pub fn upsert_dependency(text: &str, section: &str, key: &str, line: &str) -> String {
    let header = format!("[{section}]");
    let rendered = if line.ends_with('\n') {
        line.to_owned()
    } else {
        format!("{line}\n")
    };
    let mut owned: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    if owned.is_empty() && !text.is_empty() {
        owned.push(text.to_owned());
    }
    let start = owned.iter().position(|l| l.trim() == header);
    let Some(start) = start else {
        let mut out = text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&header);
        out.push('\n');
        out.push_str(&rendered);
        return out;
    };
    let end = owned
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, l)| l.trim_start().starts_with('['))
        .map(|(i, _)| i)
        .unwrap_or(owned.len());
    let key_at = owned.iter().enumerate().take(end).skip(start + 1).find(|(_, l)| {
        let t = l.trim();
        t.starts_with(&format!("{key} ")) || t.starts_with(&format!("{key}="))
    });
    if let Some((i, _)) = key_at {
        owned[i] = rendered;
    } else {
        owned.insert(end, rendered);
    }
    owned.concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn parses_targets_deps_and_inheritance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("src/lib.rs"), "");
        write(&root.join("src/main.rs"), "");
        write(&root.join("src/bin/tool.rs"), "");
        write(&root.join("tests/it.rs"), "");
        write(&root.join("examples/demo/main.rs"), "");
        write(&root.join("build.rs"), "");
        let manifest: toml::Table = toml::from_str(
            r#"
            [package]
            name = "my-pkg"
            version.workspace = true
            edition = "2021"
            [features]
            default = ["std"]
            std = []
            fast = ["dep:simd", "serde?/derive"]
            [dependencies]
            serde = { version = "1", optional = true }
            simd = { version = "0.2", optional = true, package = "simd-real" }
            local = { path = "../local" }
            shared = { workspace = true, features = ["extra"] }
            [target.'cfg(windows)'.dependencies]
            winapi = "0.3"
            [dev-dependencies]
            tempfile = "3"
            "#,
        )
        .unwrap();
        let ws_table: toml::Table =
            toml::from_str("[package]\nversion = \"1.2.3\"\n[dependencies]\nshared = { version = \"2\", features = [\"base\"] }\n")
                .unwrap();
        let ws = WsContext { root, table: &ws_table };
        let none = |_: &str| -> Result<String> { bail!("no registries") };
        let p = parse_package(&root.join("Cargo.toml"), &manifest, Some(&ws), &none).unwrap();
        assert_eq!(p.version, Version::new(1, 2, 3));
        let kinds: Vec<(TargetKind, &str)> = p.targets.iter().map(|t| (t.kind, t.name.as_str())).collect();
        assert_eq!(
            kinds,
            [
                (TargetKind::Lib, "my_pkg"),
                (TargetKind::Bin, "my-pkg"),
                (TargetKind::Bin, "tool"),
                (TargetKind::Example, "demo"),
                (TargetKind::Test, "it"),
                (TargetKind::BuildScript, "build-script-build"),
            ]
        );
        assert_eq!(p.features.get("serde"), Some(&vec!["dep:serde".to_owned()]), "implicit feature");
        assert!(!p.features.contains_key("simd"), "dep: syntax suppresses the implicit feature");
        let shared = p.deps.iter().find(|d| d.name == "shared").unwrap();
        assert_eq!(shared.features, ["base", "extra"]);
        let simd = p.deps.iter().find(|d| d.name == "simd-real").unwrap();
        assert_eq!(simd.dep_name(), "simd");
        assert!(p.deps.iter().any(|d| d.name == "winapi" && d.platform.is_some()));
        assert!(p.deps.iter().any(|d| d.name == "tempfile" && d.kind == DepKind::Dev));
        assert!(matches!(
            p.deps.iter().find(|d| d.name == "local").unwrap().source,
            DepSource::Path(_)
        ));
    }
}
