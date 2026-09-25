//! `~/.rb/config.toml`, and cargo's config stack.

use crate::cfgexpr::{PlatformCfg, PlatformExpr};
use crate::profile::TomlProfiles;
use anyhow::{Context, Result, bail};
use rb_store::LinkMode;
use rb_toolchain::Toolchains;
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

thread_local! {
    static CLI_CONFIG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// This process's `--config`. Every `CargoConfig::load` applies it.
pub fn set_cli_config(specs: Vec<String>) {
    CLI_CONFIG.with(|c| *c.borrow_mut() = specs);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Toggle {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone)]
pub struct RbConfig {
    pub home: PathBuf,
    pub store_dir: PathBuf,
    /// `false` is a normal per-project build, no global store.
    pub store_enabled: bool,
    pub link_mode: LinkMode,
    /// Cap, enforced after builds, least recently used first. `None` means no cap.
    pub store_max_size: Option<u64>,
    pub gc_max_age_days: u64,
    /// Unused `target/rb` variants older than this are deleted. They come back from the store.
    pub target_keep_days: u64,
    /// Older variants kept per crate, besides the ones this build used. `0` means a revert recompiles.
    pub target_keep_variants: usize,
    /// Remap registry and git paths in debuginfo so the artifact doesn't care where the sources lived.
    pub remap_deps_paths: bool,
    /// Nightly `-Zthreads` for units on the critical path.
    pub parallel_frontend: Toggle,
    /// Dev-profile codegen backend on nightly. `cranelift`, for instance.
    pub codegen_backend: Option<String>,
    pub zig_version: String,
    pub accept_msvc_license: bool,
    pub cache_build_scripts: bool,
    /// Reuse `$CARGO_HOME/registry/cache` after checking the checksum, instead of downloading again.
    pub reuse_cargo_downloads: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct FileConfig {
    store: StoreFile,
    speed: SpeedFile,
    cross: CrossFile,
    build_scripts: BuildScriptsFile,
    registry: RegistryFile,
    target: TargetFile,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct TargetFile {
    keep_unused_days: Option<u64>,
    keep_variants: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct RegistryFile {
    reuse_cargo_downloads: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct StoreFile {
    dir: Option<PathBuf>,
    enabled: Option<bool>,
    link_mode: Option<LinkMode>,
    max_size: Option<String>,
    max_age_days: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct SpeedFile {
    remap_deps_paths: Option<bool>,
    parallel_frontend: Option<Toggle>,
    codegen_backend: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct CrossFile {
    zig_version: Option<String>,
    accept_msvc_license: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct BuildScriptsFile {
    cache: Option<bool>,
}

/// `20G`, `512M`, `1.5T`, or a bare byte count.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => {
            let mult: u64 = match s[i..].to_ascii_uppercase().trim_end_matches("IB").trim_end_matches('B') {
                "" => 1,
                "K" => 1 << 10,
                "M" => 1 << 20,
                "G" => 1 << 30,
                "T" => 1 << 40,
                other => anyhow::bail!("unknown size unit `{other}` in `{s}`"),
            };
            (&s[..i], mult)
        }
        None => (s, 1),
    };
    let n: f64 = num.trim().parse().with_context(|| format!("invalid size `{s}`"))?;
    Ok((n * mult as f64) as u64)
}

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

pub fn home_dir() -> PathBuf {
    #[allow(deprecated)]
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

impl RbConfig {
    pub fn load() -> Result<Self> {
        let home = std::env::var_os("RB_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".rb"));
        let path = home.join("config.toml");
        let file: FileConfig = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("invalid {}", path.display()))?,
            Err(_) => FileConfig::default(),
        };
        let link_mode = match std::env::var("RB_LINK_MODE") {
            Ok(v) => v.parse().map_err(anyhow::Error::msg)?,
            Err(_) => file.store.link_mode.unwrap_or_default(),
        };
        // `"unlimited"` turns the cap off. Otherwise least recently used goes first, after the build.
        let store_max_size = match std::env::var("RB_STORE_MAX_SIZE").ok().or(file.store.max_size) {
            Some(s) if matches!(s.trim(), "unlimited" | "none" | "0") => None,
            Some(s) => Some(parse_size(&s)?),
            None => Some(20 << 30),
        };
        Ok(Self {
            store_dir: std::env::var_os("RB_STORE_DIR")
                .map(PathBuf::from)
                .or(file.store.dir)
                .unwrap_or_else(|| home.join("store")),
            store_enabled: env_bool("RB_NO_STORE").map(|b| !b).or(file.store.enabled).unwrap_or(true),
            link_mode,
            store_max_size,
            gc_max_age_days: file.store.max_age_days.unwrap_or(30),
            target_keep_days: file.target.keep_unused_days.unwrap_or(7),
            // 1 keeps the previous variant so a revert is a store hit. 0 deletes it.
            target_keep_variants: file.target.keep_variants.unwrap_or(1),
            remap_deps_paths: file.speed.remap_deps_paths.unwrap_or(false),
            parallel_frontend: file.speed.parallel_frontend.unwrap_or_default(),
            codegen_backend: std::env::var("RB_CODEGEN_BACKEND").ok().or(file.speed.codegen_backend),
            zig_version: file
                .cross
                .zig_version
                .unwrap_or_else(|| rb_toolchain::zig::DEFAULT_VERSION.to_owned()),
            accept_msvc_license: env_bool("RB_ACCEPT_MSVC_LICENSE")
                .or(file.cross.accept_msvc_license)
                .unwrap_or(false),
            cache_build_scripts: file.build_scripts.cache.unwrap_or(true),
            reuse_cargo_downloads: env_bool("RB_REUSE_CARGO_DOWNLOADS")
                .or(file.registry.reuse_cargo_downloads)
                .unwrap_or(true),
            home,
        })
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.home.join("cache")
    }

    pub fn toolchains(&self) -> Result<Toolchains> {
        Ok(Toolchains {
            home: self.home.join("toolchains"),
            rb_exe: std::env::current_exe().context("cannot locate the rb executable")?,
            zig_version: self.zig_version.clone(),
            accept_msvc_license: self.accept_msvc_license,
        })
    }

    pub fn record_msvc_license_acceptance(&self) -> Result<()> {
        let path = self.home.join("config.toml");
        let mut doc: toml::Table = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        let cross = doc.entry("cross").or_insert_with(|| toml::Value::Table(Default::default()));
        if let toml::Value::Table(t) = cross {
            t.insert("accept-msvc-license".into(), toml::Value::Boolean(true));
        }
        std::fs::create_dir_all(&self.home)?;
        std::fs::write(&path, toml::to_string_pretty(&doc)?)?;
        Ok(())
    }
}

struct ConfigFile {
    path: PathBuf,
    /// Directory that contains `.cargo`. Relative paths resolve against it.
    base: PathBuf,
    table: toml::Table,
}

/// Cwd up to the root, then `$CARGO_HOME`. Nearer files win scalars and come last in lists. `CARGO_*` overrides the files.
pub struct CargoConfig {
    /// Highest priority first.
    files: Vec<ConfigFile>,
    /// `[profile]` tables, lowest priority first.
    pub profiles: Vec<TomlProfiles>,
    /// Someone set `[unstable] build-std`. We don't support that here.
    pub build_std: bool,
}

pub fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cargo"))
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn target_env(triple: &str, key: &str) -> String {
    format!("CARGO_TARGET_{}_{key}", triple.to_uppercase().replace(['-', '.'], "_"))
}

/// A bare name is a `PATH` lookup. A relative path with a separator is relative to the config file.
fn program_path(value: &str, base: &Path) -> PathBuf {
    let p = Path::new(value);
    if p.is_relative() && value.contains(['/', '\\']) {
        base.join(p)
    } else {
        p.to_path_buf()
    }
}

fn string_list(v: &toml::Value, what: &str) -> Result<Vec<String>> {
    match v {
        toml::Value::String(s) => Ok(s.split_whitespace().map(str::to_owned).collect()),
        toml::Value::Array(a) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("`{what}` must be a list of strings"))
            })
            .collect(),
        _ => bail!("`{what}` must be a string or a list of strings"),
    }
}

impl CargoConfig {
    pub fn load(cwd: &Path) -> Result<Self> {
        let mut cfg = Self::load_from(cwd, &cargo_home())?;
        let specs = CLI_CONFIG.with(|c| c.borrow().clone());
        cfg.push_cli_config(cwd, &specs)?;
        Ok(cfg)
    }

    fn load_from(cwd: &Path, home: &Path) -> Result<Self> {
        let mut paths = Vec::new();
        for dir in cwd.ancestors() {
            let dot = dir.join(".cargo");
            // Extension-less `.cargo/config` wins when both exist, same as cargo.
            if let Some(p) = [dot.join("config"), dot.join("config.toml")].into_iter().find(|p| p.is_file()) {
                paths.push(p);
            }
        }
        if let Some(p) = [home.join("config"), home.join("config.toml")].into_iter().find(|p| p.is_file())
            && !paths.iter().any(|q| same_file(q, &p))
        {
            paths.push(p);
        }
        let mut files = Vec::new();
        for path in paths {
            let text = std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
            let table: toml::Table = toml::from_str(&text).with_context(|| format!("could not parse {}", path.display()))?;
            let base = path.parent().and_then(Path::parent).unwrap_or(Path::new("/")).to_path_buf();
            files.push(ConfigFile { path, base, table });
        }
        let mut profiles = Vec::new();
        let mut build_std = false;
        for f in files.iter().rev() {
            if let Some(p) = f.table.get("profile") {
                profiles.push(
                    p.clone()
                        .try_into::<TomlProfiles>()
                        .with_context(|| format!("invalid [profile] in {}", f.path.display()))?,
                );
            }
            build_std |= f.table.get("unstable").and_then(|u| u.get("build-std")).is_some();
        }
        Ok(Self {
            files,
            profiles,
            build_std,
        })
    }

    fn get(&self, path: &[&str]) -> Option<(&toml::Value, &Path)> {
        self.files.iter().find_map(|f| {
            let mut v = f.table.get(path[0])?;
            for key in &path[1..] {
                v = v.get(key)?;
            }
            Some((v, f.base.as_path()))
        })
    }

    fn get_str(&self, path: &[&str]) -> Result<Option<(&str, &Path)>> {
        match self.get(path) {
            None => Ok(None),
            Some((toml::Value::String(s), base)) => Ok(Some((s, base))),
            Some(_) => bail!("`{}` in cargo configuration must be a string", path.join(".")),
        }
    }

    /// Lists join lowest priority first, then the env var.
    fn joined_list(&self, path: &[&str], env: Option<&str>) -> Result<Option<Vec<String>>> {
        let mut out: Option<Vec<String>> = None;
        for f in self.files.iter().rev() {
            let mut v = f.table.get(path[0]);
            for key in &path[1..] {
                v = v.and_then(|v| v.get(key));
            }
            if let Some(v) = v {
                out.get_or_insert_with(Vec::new).extend(string_list(v, &path.join("."))?);
            }
        }
        if let Some(e) = env.and_then(env_nonempty) {
            out.get_or_insert_with(Vec::new).extend(e.split_whitespace().map(str::to_owned));
        }
        Ok(out)
    }

    fn cfg_targets<'s>(&'s self, platform: &PlatformCfg<'_>) -> Result<Vec<(&'s toml::Value, &'s Path)>> {
        let mut keys: BTreeMap<&str, Vec<(&toml::Value, &Path)>> = BTreeMap::new();
        for f in self.files.iter().rev() {
            let Some(targets) = f.table.get("target").and_then(|t| t.as_table()) else {
                continue;
            };
            for (key, value) in targets.iter().filter(|(k, _)| k.starts_with("cfg(")) {
                keys.entry(key).or_default().push((value, &f.base));
            }
        }
        let mut out = Vec::new();
        for (key, values) in keys {
            if PlatformExpr::parse(key)
                .with_context(|| format!("invalid `[target.'{key}']` in cargo configuration"))?
                .matches(platform)
            {
                out.extend(values);
            }
        }
        Ok(out)
    }

    pub fn has_cfg_targets(&self) -> bool {
        self.files.iter().any(|f| {
            f.table
                .get("target")
                .and_then(|t| t.as_table())
                .is_some_and(|t| t.keys().any(|k| k.starts_with("cfg(")))
        })
    }

    pub fn rustc(&self) -> Result<Option<PathBuf>> {
        if let Some(v) = env_nonempty("RUSTC").or_else(|| env_nonempty("CARGO_BUILD_RUSTC")) {
            return Ok(Some(PathBuf::from(v)));
        }
        Ok(self.get_str(&["build", "rustc"])?.map(|(v, base)| program_path(v, base)))
    }

    /// Empty `RUSTC_WRAPPER` turns a configured wrapper off.
    pub fn rustc_wrapper(&self) -> Result<Option<PathBuf>> {
        for name in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
            if let Ok(v) = std::env::var(name) {
                return Ok(Some(PathBuf::from(v)).filter(|p| !p.as_os_str().is_empty()));
            }
        }
        Ok(self
            .get_str(&["build", "rustc-wrapper"])?
            .filter(|(v, _)| !v.is_empty())
            .map(|(v, base)| program_path(v, base)))
    }

    pub fn build_targets(&self) -> Result<Vec<String>> {
        if let Some(v) = env_nonempty("CARGO_BUILD_TARGET") {
            return Ok(vec![v]);
        }
        match self.get(&["build", "target"]) {
            None => Ok(Vec::new()),
            Some((v, _)) => string_list(v, "build.target"),
        }
    }

    /// `CARGO_TARGET_DIR` is relative to cwd. A config path is relative to the file that set it.
    pub fn target_dir(&self, cwd: &Path) -> Result<Option<PathBuf>> {
        if let Some(v) = env_nonempty("CARGO_TARGET_DIR").or_else(|| env_nonempty("CARGO_BUILD_TARGET_DIR")) {
            return Ok(Some(cwd.join(v)));
        }
        Ok(self.get_str(&["build", "target-dir"])?.map(|(v, base)| base.join(v)))
    }

    /// Negative counts down from the number of CPUs.
    pub fn jobs(&self) -> Result<Option<usize>> {
        let n = match env_nonempty("CARGO_BUILD_JOBS") {
            Some(v) if v == "default" => return Ok(None),
            Some(v) => v.parse::<i64>().with_context(|| format!("invalid CARGO_BUILD_JOBS `{v}`"))?,
            None => match self.get(&["build", "jobs"]) {
                None => return Ok(None),
                Some((toml::Value::Integer(i), _)) => *i,
                Some((toml::Value::String(s), _)) if s == "default" => return Ok(None),
                Some(_) => bail!("`build.jobs` must be an integer"),
            },
        };
        let cpus = std::thread::available_parallelism().map(|c| c.get() as i64).unwrap_or(1);
        match n {
            0 => bail!("jobs may not be 0"),
            n if n < 0 => Ok(Some((cpus + n).max(1) as usize)),
            n => Ok(Some(n as usize)),
        }
    }

    fn get_bool(&self, path: &[&str], env: &str) -> Result<Option<bool>> {
        if let Some(v) = env_nonempty(env) {
            return Ok(Some(matches!(v.as_str(), "true" | "1")));
        }
        match self.get(path) {
            None => Ok(None),
            Some((toml::Value::Boolean(b), _)) => Ok(Some(*b)),
            Some(_) => bail!("`{}` in cargo configuration must be a boolean", path.join(".")),
        }
    }

    pub fn incremental(&self) -> Result<Option<bool>> {
        self.get_bool(&["build", "incremental"], "CARGO_BUILD_INCREMENTAL")
    }

    pub fn offline(&self) -> Result<bool> {
        Ok(self.get_bool(&["net", "offline"], "CARGO_NET_OFFLINE")?.unwrap_or(false))
    }

    pub fn registries(&self) -> BTreeMap<String, String> {
        let mut names: Vec<String> = self
            .files
            .iter()
            .filter_map(|f| f.table.get("registries")?.as_table())
            .flat_map(|t| t.keys().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .filter_map(|name| {
                let env = format!("CARGO_REGISTRIES_{}_INDEX", name.to_uppercase().replace('-', "_"));
                let index = env_nonempty(&env).or_else(|| self.get_str(&["registries", &name, "index"]).ok()??.0.to_owned().into())?;
                let index = index.trim_end_matches('/');
                let id = if index.starts_with("sparse+") {
                    format!("{index}/")
                } else {
                    format!("registry+{index}")
                };
                Some((name, id))
            })
            .collect()
    }

    /// `[source] replace-with` a directory: `(registry source id, dir)`. Anything else is an error.
    pub fn vendor_dir(&self) -> Result<Option<(String, PathBuf)>> {
        let Some((name, spec, _)) = self
            .files
            .iter()
            .filter_map(|f| f.table.get("source")?.as_table().map(|t| (t, f.base.as_path())))
            .flat_map(|(t, base)| t.iter().map(move |(n, s)| (n, s, base)))
            .find(|(_, s, _)| s.get("replace-with").is_some())
        else {
            return Ok(None);
        };
        let with = spec
            .get("replace-with")
            .and_then(|v| v.as_str())
            .with_context(|| format!("`source.{name}.replace-with` must be a string"))?;
        let Some((dir, base)) = self.get_str(&["source", with, "directory"])? else {
            bail!("source replacement `{name}` -> `{with}` is not a directory source");
        };
        let source_id = if name == "crates-io" {
            crate::manifest::CRATES_IO.to_owned()
        } else {
            self.registries().get(name).cloned().unwrap_or_else(|| name.clone())
        };
        let path = if Path::new(dir).is_absolute() {
            PathBuf::from(dir)
        } else {
            base.join(dir)
        };
        Ok(Some((source_id, path)))
    }

    pub fn replaced_source(&self) -> Option<String> {
        self.files
            .iter()
            .filter_map(|f| f.table.get("source")?.as_table())
            .flat_map(|t| t.iter())
            .find(|(_, s)| s.get("replace-with").is_some())
            .map(|(name, _)| name.clone())
    }

    /// `CARGO_ENCODED_RUSTFLAGS`, then `RUSTFLAGS`, then `target.<triple>.rustflags` plus matching `cfg()` tables, then `build.rustflags`.
    pub fn rustflags(&self, triple: &str, platform: Option<&PlatformCfg<'_>>) -> Result<Vec<String>> {
        if let Ok(e) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
            return Ok(if e.is_empty() {
                Vec::new()
            } else {
                e.split('\x1f').map(str::to_owned).collect()
            });
        }
        if let Ok(r) = std::env::var("RUSTFLAGS") {
            return Ok(r.split_whitespace().map(str::to_owned).collect());
        }
        let mut target = self.joined_list(&["target", triple, "rustflags"], Some(&target_env(triple, "RUSTFLAGS")))?;
        if let Some(p) = platform {
            for (t, _) in self.cfg_targets(p)? {
                if let Some(f) = t.get("rustflags") {
                    target
                        .get_or_insert_with(Vec::new)
                        .extend(string_list(f, "target.<cfg>.rustflags")?);
                }
            }
        }
        if let Some(t) = target {
            return Ok(t);
        }
        Ok(self
            .joined_list(&["build", "rustflags"], Some("CARGO_BUILD_RUSTFLAGS"))?
            .unwrap_or_default())
    }

    pub fn linker(&self, triple: &str, platform: Option<&PlatformCfg<'_>>) -> Result<Option<PathBuf>> {
        if let Some(v) = env_nonempty(&target_env(triple, "LINKER")) {
            return Ok(Some(PathBuf::from(v)));
        }
        if let Some((v, base)) = self.get_str(&["target", triple, "linker"])? {
            return Ok(Some(program_path(v, base)));
        }
        let Some(p) = platform else { return Ok(None) };
        for (t, base) in self.cfg_targets(p)?.into_iter().rev() {
            if let Some(l) = t.get("linker").and_then(|l| l.as_str()) {
                return Ok(Some(program_path(l, base)));
            }
        }
        Ok(None)
    }

    pub fn runner(&self, triple: &str, platform: Option<&PlatformCfg<'_>>) -> Result<Option<Vec<String>>> {
        let resolve = |mut v: Vec<String>, base: &Path| -> Option<Vec<String>> {
            let program = program_path(v.first()?, base).to_string_lossy().into_owned();
            v[0] = program;
            Some(v)
        };
        if let Some(v) = env_nonempty(&target_env(triple, "RUNNER")) {
            return Ok(Some(v.split_whitespace().map(str::to_owned).collect()).filter(|v: &Vec<String>| !v.is_empty()));
        }
        if let Some((v, base)) = self.get(&["target", triple, "runner"]) {
            return Ok(resolve(string_list(v, "target.<triple>.runner")?, base));
        }
        let Some(p) = platform else { return Ok(None) };
        for (t, base) in self.cfg_targets(p)?.into_iter().rev() {
            if let Some(r) = t.get("runner") {
                return Ok(resolve(string_list(r, "target.<cfg>.runner")?, base));
            }
        }
        Ok(None)
    }

    /// `[env]` for rustc and build scripts. An existing variable wins unless `force`.
    pub fn env_vars(&self) -> Result<Vec<(String, String)>> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for f in self.files.iter().rev() {
            let Some(env) = f.table.get("env").and_then(|e| e.as_table()) else {
                continue;
            };
            for (key, v) in env {
                let (value, force, relative) = match v {
                    toml::Value::String(s) => (s.clone(), false, false),
                    toml::Value::Table(t) => (
                        t.get("value")
                            .and_then(|v| v.as_str())
                            .with_context(|| format!("`env.{key}` needs a `value`"))?
                            .to_owned(),
                        t.get("force").and_then(|v| v.as_bool()).unwrap_or(false),
                        t.get("relative").and_then(|v| v.as_bool()).unwrap_or(false),
                    ),
                    _ => bail!("`env.{key}` in {} must be a string or a table", f.path.display()),
                };
                if !force && std::env::var_os(key).is_some() {
                    out.remove(key);
                    continue;
                }
                let value = if relative {
                    f.base.join(value).to_string_lossy().into_owned()
                } else {
                    value
                };
                out.insert(key.clone(), value);
            }
        }
        Ok(out.into_iter().collect())
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

impl CargoConfig {
    /// `--config KEY=VALUE` and `--config path`. Later entries win, ahead of `.cargo/config.toml`.
    pub fn push_cli_config(&mut self, cwd: &Path, specs: &[String]) -> Result<()> {
        if specs.is_empty() {
            return Ok(());
        }
        let mut table = toml::Table::new();
        for spec in specs {
            if let Some((key, raw)) = split_config_set(spec) {
                let value = if raw == "true" {
                    toml::Value::Boolean(true)
                } else if raw == "false" {
                    toml::Value::Boolean(false)
                } else if let Ok(n) = raw.parse::<i64>() {
                    toml::Value::Integer(n)
                } else {
                    toml::from_str::<toml::Value>(raw).unwrap_or_else(|_| toml::Value::String(raw.to_owned()))
                };
                insert_dotted(&mut table, &dotted_parts(key), value);
            } else {
                let path = cwd.join(spec);
                let path = if path.is_file() { path } else { PathBuf::from(spec) };
                if !path.is_file() {
                    bail!("--config `{spec}` is not KEY=VALUE or a config file");
                }
                let text = std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
                let extra: toml::Table = toml::from_str(&text).with_context(|| format!("could not parse {}", path.display()))?;
                for (k, v) in extra {
                    table.insert(k, v);
                }
            }
        }
        if table.get("unstable").and_then(|u| u.get("build-std")).is_some() {
            self.build_std = true;
        }
        self.files.insert(
            0,
            ConfigFile {
                path: cwd.join("--config"),
                base: cwd.to_path_buf(),
                table,
            },
        );
        Ok(())
    }
}

fn split_config_set(spec: &str) -> Option<(&str, &str)> {
    let eq = spec.find('=')?;
    let (key, raw) = spec.split_at(eq);
    if key.is_empty() || key.contains('/') || key.contains('\\') {
        return None;
    }
    Some((key, &raw[1..]))
}

fn dotted_parts(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in key.chars() {
        match c {
            '\'' | '"' => quoted = !quoted,
            '.' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn insert_dotted(table: &mut toml::Table, parts: &[String], value: toml::Value) {
    if parts.is_empty() {
        return;
    }
    let mut cur = table;
    for key in &parts[..parts.len() - 1] {
        let entry = cur.entry(key).or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        cur = entry.as_table_mut().unwrap();
    }
    cur.insert(parts[parts.len() - 1].clone(), value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("20G").unwrap(), 20 << 30);
        assert_eq!(parse_size("512MiB").unwrap(), 512 << 20);
        assert_eq!(parse_size("1024").unwrap(), 1024);
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn cli_config_overrides_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cargo")).unwrap();
        std::fs::write(tmp.path().join(".cargo/config.toml"), "[build]\njobs = 2\n[net]\noffline = false\n").unwrap();
        let mut c = CargoConfig::load(tmp.path()).unwrap();
        c.push_cli_config(tmp.path(), &["build.jobs=8".into(), "net.offline=true".into()])
            .unwrap();
        assert_eq!(c.jobs().unwrap(), Some(8));
        assert!(c.offline().unwrap());
        set_cli_config(vec!["build.jobs=4".into()]);
        let loaded = CargoConfig::load(tmp.path()).unwrap();
        assert_eq!(loaded.jobs().unwrap(), Some(4));
        set_cli_config(Vec::new());
    }

    #[test]
    fn cargo_config_hierarchy() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, ws) = (tmp.path().join("home"), tmp.path().join("ws"));
        let member = ws.join("member");
        std::fs::create_dir_all(&member).unwrap();
        write(
            &home.join("config.toml"),
            r#"
            [build]
            jobs = 3
            rustflags = ["-Chome"]
            [registries.corp]
            index = "sparse+https://corp.example/index/"
            [env]
            FROM_HOME = "h"
            "#,
        );
        write(
            &ws.join(".cargo/config.toml"),
            r#"
            [build]
            target-dir = "out"
            rustflags = "-Cws -Cmore"
            [target.x86_64-unknown-linux-gnu]
            linker = "tools/cc"
            rustflags = ["-Ctriple"]
            [target.'cfg(target_os = "linux")']
            rustflags = ["-Clinux"]
            runner = ["qemu-x86_64", "-L", "/sysroot"]
            [env]
            REL = { value = "data", relative = true }
            "#,
        );
        write(&member.join(".cargo/config.toml"), "[build]\njobs = 5\ntarget = [\"a\", \"b\"]\n");

        let c = CargoConfig::load_from(&member, &home).unwrap();
        assert_eq!(c.files.len(), 3);
        assert_eq!(c.jobs().unwrap(), Some(5), "nearest file wins");
        assert_eq!(c.build_targets().unwrap(), ["a", "b"]);
        assert_eq!(
            c.target_dir(&member).unwrap(),
            Some(ws.join("out")),
            "relative to the config's base"
        );
        assert_eq!(c.registries()["corp"], "sparse+https://corp.example/index/");

        // Lists join lowest priority first. A target table replaces `build.rustflags`.
        assert_eq!(c.rustflags("aarch64-apple-darwin", None).unwrap(), ["-Chome", "-Cws", "-Cmore"]);
        let cfg = vec![("target_os".to_owned(), Some("linux".to_owned()))];
        let linux = PlatformCfg {
            triple: "x86_64-unknown-linux-gnu",
            cfg: &cfg,
        };
        assert_eq!(c.rustflags("x86_64-unknown-linux-gnu", None).unwrap(), ["-Ctriple"]);
        assert_eq!(
            c.rustflags("x86_64-unknown-linux-gnu", Some(&linux)).unwrap(),
            ["-Ctriple", "-Clinux"]
        );
        assert!(c.has_cfg_targets());

        assert_eq!(c.linker("x86_64-unknown-linux-gnu", None).unwrap(), Some(ws.join("tools/cc")));
        assert_eq!(
            c.runner("x86_64-unknown-linux-gnu", Some(&linux)).unwrap().unwrap(),
            ["qemu-x86_64", "-L", "/sysroot"]
        );
        assert_eq!(c.runner("x86_64-unknown-linux-gnu", None).unwrap(), None);

        let env: BTreeMap<String, String> = c.env_vars().unwrap().into_iter().collect();
        assert_eq!(env["FROM_HOME"], "h");
        assert_eq!(env["REL"], ws.join("data").to_string_lossy());
    }

    #[test]
    fn legacy_name_and_negative_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("p/.cargo/config"), "[build]\njobs = -1\n");
        write(&tmp.path().join("p/.cargo/config.toml"), "[build]\njobs = 99\n");
        let c = CargoConfig::load_from(&tmp.path().join("p"), &tmp.path().join("nohome")).unwrap();
        let cpus = std::thread::available_parallelism().unwrap().get();
        assert_eq!(c.jobs().unwrap(), Some((cpus as i64 - 1).max(1) as usize));
    }
}
