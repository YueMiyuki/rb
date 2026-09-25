//! `rb info`, matching `cargo info`.

use crate::config::RbConfig;
use crate::lockfile;
use crate::manifest::{self, CRATES_IO, DepKind, Package, clean_path};
use crate::registry::{self, Registry};
use crate::shell::Shell;
use anyhow::{Context, Result, bail};
use semver::{Version, VersionReq};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
struct PartialVer(Vec<u64>);

impl PartialVer {
    fn matches(&self, v: &Version) -> bool {
        let got = [v.major, v.minor, v.patch];
        self.0.iter().zip(got).all(|(w, g)| *w == g)
    }
}

pub struct InfoOptions<'a> {
    pub spec: &'a str,
    pub offline: bool,
    pub locked: bool,
    pub frozen: bool,
    pub verbose: bool,
}

/// Print package information the way `cargo info` does.
pub fn info(opts: &InfoOptions<'_>, cfg: &RbConfig, shell: &Shell) -> Result<()> {
    let (name, partial) = parse_spec(opts.spec)?;
    let cwd = std::env::current_dir()?;
    let ws = workspace_root(&cwd);
    if ws.is_none() && (opts.locked || opts.frozen) {
        let flag = if opts.frozen { "--frozen" } else { "--locked" };
        bail!("the option `{flag}` can only be used within a workspace");
    }
    if let Some(root) = &ws
        && let Some(pkg) = member_match(root, &name, partial.as_ref())?
    {
        let from = format!("./{}", rel_path(&cwd, &pkg.root));
        print!("{}", render(&pkg, None, Some(&from), false, opts.verbose));
        return Ok(());
    }
    let locked_version = ws.as_ref().and_then(|root| lock_version(root, &name, partial.as_ref()));
    let suggest_tree = locked_version.is_some();
    let offline = opts.offline || crate::config::CargoConfig::load(&cwd)?.offline()?;
    let registry = Registry::new(
        cfg.home.join("registry"),
        offline,
        cargo_cache(cfg),
    );
    let (pkg, latest) = registry_package(&registry, opts.spec, &name, partial.as_ref(), locked_version, shell)?;
    print!("{}", render(&pkg, latest.as_ref(), None, true, opts.verbose));
    if suggest_tree {
        shell.note(format!(
            "to see how you depend on {}, run `cargo tree --invert {}@{}`",
            pkg.name, pkg.name, pkg.version
        ));
    }
    Ok(())
}

fn cargo_cache(cfg: &RbConfig) -> Option<PathBuf> {
    if !cfg.reuse_cargo_downloads {
        return None;
    }
    Some(crate::config::cargo_home().join("registry").join("cache")).filter(|p| p.is_dir())
}

fn parse_spec(spec: &str) -> Result<(String, Option<PartialVer>)> {
    let (name, ver) = match spec.split_once('@') {
        Some((name, ver)) => (name, Some(ver)),
        None => (spec, None),
    };
    if let Some(c) = name.chars().find(|c| !c.is_alphanumeric() && *c != '-' && *c != '_') {
        bail!(
            "invalid package ID specification: `{spec}`\n\nCaused by:\n  invalid character `{c}` in package name: `{name}`, characters must be Unicode XID characters (numbers, `-`, `_`, or most letters)"
        );
    }
    if name.is_empty() {
        bail!("invalid package ID specification: `{spec}`");
    }
    let partial = match ver {
        Some(v) => match parse_partial(v) {
            Ok(p) => Some(p),
            Err(e) => bail!("invalid package ID specification: `{spec}`\n\nCaused by:\n  {e}"),
        },
        None => None,
    };
    Ok((name.to_owned(), partial))
}

fn parse_partial(v: &str) -> Result<PartialVer> {
    if v.chars().any(|c| matches!(c, '^' | '=' | '~' | '>' | '<' | '*')) {
        bail!("unexpected version requirement, expected a version like \"1.32\"");
    }
    let parts: Vec<&str> = v.split('.').collect();
    if parts.is_empty()
        || parts.len() > 3
        || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        if v.contains("://") || v.contains('-') {
            bail!("unexpected prerelease field, expected a version like \"1.32\"");
        }
        bail!("expected a version like \"1.32\"");
    }
    Ok(PartialVer(parts.iter().map(|p| p.parse().unwrap()).collect()))
}

fn workspace_root(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let manifest = d.join("Cargo.toml");
        if manifest.is_file()
            && let Ok((current, root)) = crate::workspace::locate_manifests(d, None)
        {
            let _ = current;
            return Some(root.parent()?.to_owned());
        }
        dir = d.parent();
    }
    None
}

fn member_match(root: &Path, name: &str, partial: Option<&PartialVer>) -> Result<Option<Package>> {
    let table = manifest::read_toml(&root.join("Cargo.toml"))?;
    let mut dirs = vec![root.to_owned()];
    if let Some(ws) = table.get("workspace").and_then(|w| w.as_table()) {
        let patterns = ws
            .get("members")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default();
        dirs.clear();
        for p in patterns {
            let pattern = root.join(&p);
            let s = pattern.to_string_lossy();
            if s.contains(['*', '?', '[']) {
                for entry in glob::glob(&s).with_context(|| format!("invalid workspace glob `{p}`"))? {
                    let d = entry?;
                    if d.join("Cargo.toml").is_file() {
                        dirs.push(clean_path(&d));
                    }
                }
            } else if pattern.join("Cargo.toml").is_file() {
                dirs.push(clean_path(&pattern));
            }
        }
    }
    for dir in dirs {
        let path = dir.join("Cargo.toml");
        let pkg = manifest::load_package(&path, None, &|_| Ok(CRATES_IO.to_owned()))?;
        if pkg.name == name && partial.is_none_or(|p| p.matches(&pkg.version)) {
            return Ok(Some(pkg));
        }
    }
    Ok(None)
}

fn lock_version(root: &Path, name: &str, partial: Option<&PartialVer>) -> Option<Version> {
    let lock = lockfile::read(&root.join("Cargo.lock")).ok()??;
    lock.packages
        .into_iter()
        .filter(|p| p.name == name && p.source.as_deref().is_some_and(|s| s.contains("crates.io")))
        .filter(|p| partial.is_none_or(|v| v.matches(&p.version)))
        .map(|p| p.version)
        .max()
}

fn registry_package(
    registry: &Registry,
    spec: &str,
    requested: &str,
    partial: Option<&PartialVer>,
    locked: Option<Version>,
    shell: &Shell,
) -> Result<(Package, Option<Version>)> {
    let mut found = None;
    for candidate in lookup_names(requested) {
        match registry.entries(CRATES_IO, &candidate) {
            Ok(entries) if !entries.is_empty() => {
                found = Some(entries);
                break;
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    let entries = found.with_context(|| {
        format!("could not find `{spec}` in registry `https://github.com/rust-lang/crates.io-index`")
    })?;
    let canonical = &entries[0].name;
    if canonical != requested {
        shell.warn(format!("translating `{requested}` to `{canonical}`"));
    }
    let latest = entries.iter().filter(|e| !e.yanked).map(|e| e.vers.clone()).max();
    let rustc = rustc_version();
    let chosen = if let Some(v) = locked {
        v
    } else {
        entries
            .iter()
            .filter(|e| !e.yanked && partial.is_none_or(|p| p.matches(&e.vers)))
            .max_by(|a, b| {
                let am = a.rust_version.as_ref().is_none_or(|m| rustc.as_ref().is_none_or(|r| m <= r));
                let bm = b.rust_version.as_ref().is_none_or(|m| rustc.as_ref().is_none_or(|r| m <= r));
                match (am, bm) {
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    _ => a.vers.cmp(&b.vers),
                }
            })
            .with_context(|| {
                format!("could not find `{spec}` in registry `https://github.com/rust-lang/crates.io-index`")
            })?
            .vers
            .clone()
    };
    let cksum = entries.iter().find(|e| e.vers == chosen).map(|e| e.cksum.clone());
    let (dir, _) = registry.ensure(&registry::Download {
        source: CRATES_IO,
        name: canonical,
        version: &chosen,
        checksum: cksum.as_deref(),
    })?;
    let pkg = manifest::load_package(&dir.join("Cargo.toml"), None, &|_| Ok(CRATES_IO.to_owned()))?;
    let latest = latest.filter(|v| v != &pkg.version);
    Ok((pkg, latest))
}

fn lookup_names(name: &str) -> Vec<String> {
    let mut names = vec![name.to_owned()];
    let lower = name.to_lowercase();
    if lower != name {
        names.push(lower.clone());
    }
    let hyphen = lower.replace('_', "-");
    if !names.contains(&hyphen) {
        names.push(hyphen);
    }
    names
}

fn rustc_version() -> Option<Version> {
    let out = std::process::Command::new("rustc").arg("-vV").output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let release = text.lines().find_map(|l| l.strip_prefix("release: "))?;
    Version::parse(release.split('-').next()?).ok()
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Feat {
    ByUser,
    On,
    Off,
}

fn render(pkg: &Package, latest: Option<&Version>, from: Option<&str>, crates_io: bool, verbose: bool) -> String {
    let mut out = String::new();
    out.push_str(&pkg.name);
    if !pkg.keywords.is_empty() {
        out.push(' ');
        out.push_str(&pkg.keywords.iter().map(|k| format!("#{k}")).collect::<Vec<_>>().join(" "));
    }
    out.push('\n');
    if let Some(desc) = &pkg.description {
        out.push_str(desc.trim_end());
        out.push('\n');
    }
    out.push_str(&format!("version: {}", pkg.version));
    if let Some(latest) = latest {
        out.push_str(&format!(" (latest {latest})"));
    } else if let Some(from) = from {
        out.push_str(&format!(" (from {from})"));
    }
    out.push('\n');
    out.push_str(&format!("license: {}\n", pkg.license.as_deref().unwrap_or("unknown")));
    out.push_str(&format!("rust-version: {}\n", pkg.rust_version.as_deref().unwrap_or("unknown")));
    if let Some(doc) = pkg.documentation.clone().or_else(|| {
        crates_io.then(|| format!("https://docs.rs/{}/{}", pkg.name, pkg.version))
    }) {
        out.push_str(&format!("documentation: {doc}\n"));
    }
    if let Some(home) = &pkg.homepage {
        out.push_str(&format!("homepage: {home}\n"));
    }
    if let Some(repo) = &pkg.repository {
        out.push_str(&format!("repository: {repo}\n"));
    }
    if crates_io {
        out.push_str(&format!("crates.io: https://crates.io/crates/{}/{}\n", pkg.name, pkg.version));
    }
    out.push_str(&render_features(pkg));
    if verbose {
        out.push_str(&render_deps(pkg, DepKind::Normal, "dependencies"));
        out.push_str(&render_deps(pkg, DepKind::Build, "build-dependencies"));
    }
    out
}

fn render_features(pkg: &Package) -> String {
    let margin = pkg.features.keys().map(|n| n.len()).max().unwrap_or(0);
    if margin == 0 {
        return String::new();
    }
    let mut status: Vec<(String, Feat)> = pkg
        .features
        .keys()
        .map(|n| (n.clone(), if n == "default" { Feat::ByUser } else { Feat::Off }))
        .collect();
    let mut queue = vec!["default".to_owned()];
    while let Some(current) = queue.pop() {
        let Some(values) = pkg.features.get(&current) else { continue };
        for value in values.iter().rev() {
            if value.starts_with("dep:") || value.contains('/') {
                continue;
            }
            if let Some((_, st)) = status.iter_mut().find(|(n, _)| n == value)
                && *st == Feat::Off
            {
                *st = Feat::On;
                queue.push(value.clone());
            }
        }
    }
    status.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    let mut out = String::from("features:\n");
    for (name, st) in status {
        let mark = if st == Feat::ByUser { "+" } else { " " };
        let values = pkg.features.get(&name).map(|v| v.join(", ")).unwrap_or_default();
        out.push_str(&format!(" {mark}{name:<margin$} = [{values}]\n"));
    }
    out
}

fn render_deps(pkg: &Package, kind: DepKind, header: &str) -> String {
    let mut deps: Vec<_> = pkg.deps.iter().filter(|d| d.kind == kind).collect();
    if deps.is_empty() {
        return String::new();
    }
    let activated: Vec<&str> = pkg
        .features
        .iter()
        .filter(|(n, _)| n == &"default" || feature_on(pkg, n))
        .flat_map(|(_, vs)| vs.iter().map(String::as_str))
        .collect();
    deps.sort_by(|a, b| {
        dep_status(a, &activated).cmp(&dep_status(b, &activated)).then(a.name.cmp(&b.name))
    });
    let mut out = format!("{header}:\n");
    for d in deps {
        let mark = if dep_status(d, &activated) == Feat::ByUser { "+" } else { " " };
        let (req, source) = match &d.source {
            manifest::DepSource::Registry(_) => (format!("@{}", pretty_req(&d.req)), String::new()),
            _ => (String::new(), format!(" ({})", d.name)),
        };
        let _ = source;
        out.push_str(&format!(" {mark}{}{req}\n", d.name));
    }
    out
}

fn feature_on(pkg: &Package, name: &str) -> bool {
    let mut seen = Vec::new();
    let mut queue = vec!["default"];
    while let Some(cur) = queue.pop() {
        if !seen.contains(&cur) {
            seen.push(cur);
        }
        if cur == name {
            return true;
        }
        if let Some(vs) = pkg.features.get(cur) {
            for v in vs {
                if !v.starts_with("dep:") && !v.contains('/') && !seen.contains(&v.as_str()) {
                    queue.push(v);
                }
            }
        }
    }
    false
}

fn dep_status(d: &manifest::DepDecl, activated: &[&str]) -> Feat {
    if !d.optional {
        return Feat::ByUser;
    }
    let toml_name = d.dep_name();
    let enabled = activated.iter().any(|v| {
        v.strip_prefix("dep:").is_some_and(|n| n == toml_name)
            || v.split_once('/').is_some_and(|(n, _)| n.trim_end_matches('?') == toml_name)
    });
    if enabled { Feat::On } else { Feat::Off }
}

fn pretty_req(req: &VersionReq) -> String {
    let mut rendered = req.to_string();
    if req.comparators.len() == 1 && rendered.starts_with('^') {
        rendered.remove(0);
    }
    rendered
}

fn rel_path(from_dir: &Path, to: &Path) -> String {
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
    if out.as_os_str().is_empty() {
        ".".into()
    } else {
        out.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_version_rejects_requirements() {
        assert!(parse_partial("^1").is_err());
        assert!(parse_partial("1.0.210").unwrap().matches(&Version::parse("1.0.210").unwrap()));
        assert!(parse_partial("1.0").unwrap().matches(&Version::parse("1.0.229").unwrap()));
        assert!(!parse_partial("1.0").unwrap().matches(&Version::parse("1.1.0").unwrap()));
    }
}
