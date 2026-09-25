//! Built-in defaults, `inherits`, then manifest, config, and `CARGO_PROFILE_*`. Host units get `build-override`.

use crate::workspace::Pkg;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type TomlProfiles = BTreeMap<String, TomlProfile>;

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum StringOrBool {
    Bool(bool),
    String(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum StringOrInt {
    Int(i64),
    String(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum DebugValue {
    Bool(bool),
    Int(i64),
    String(String),
}

/// One layer: manifest, a config file, or `CARGO_PROFILE_*`. Unknown keys are ignored.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlProfile {
    opt_level: Option<StringOrInt>,
    debug: Option<DebugValue>,
    debug_assertions: Option<bool>,
    overflow_checks: Option<bool>,
    lto: Option<StringOrBool>,
    panic: Option<String>,
    codegen_units: Option<u32>,
    incremental: Option<bool>,
    rpath: Option<bool>,
    strip: Option<StringOrBool>,
    split_debuginfo: Option<String>,
    codegen_backend: Option<String>,
    rustflags: Option<Vec<String>>,
    inherits: Option<String>,
    dir_name: Option<String>,
    build_override: Option<Box<TomlProfile>>,
    /// `"*"` for every non-member, or `name` / `name@version`.
    package: Option<BTreeMap<String, TomlProfile>>,
}

macro_rules! take {
    ($self:ident, $other:ident, $($field:ident),*) => {
        $(if $other.$field.is_some() {
            $self.$field = $other.$field.clone();
        })*
    };
}

impl TomlProfile {
    fn merge(&mut self, other: &TomlProfile) {
        take!(
            self,
            other,
            opt_level,
            debug,
            debug_assertions,
            overflow_checks,
            lto,
            panic,
            codegen_units,
            incremental,
            rpath,
            strip,
            split_debuginfo,
            codegen_backend,
            rustflags,
            inherits,
            dir_name
        );
        if let Some(b) = &other.build_override {
            self.build_override.get_or_insert_with(Default::default).merge(b);
        }
        if let Some(pk) = &other.package {
            let mine = self.package.get_or_insert_with(Default::default);
            for (spec, p) in pk {
                mine.entry(spec.clone()).or_default().merge(p);
            }
        }
    }
}

fn debuginfo_value(d: &DebugValue) -> String {
    match d {
        DebugValue::Bool(true) => "2".into(),
        DebugValue::Bool(false) => "0".into(),
        DebugValue::Int(i) => i.to_string(),
        DebugValue::String(s) => match s.as_str() {
            "none" => "0".into(),
            "limited" => "1".into(),
            "full" => "2".into(),
            other => other.to_owned(),
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lto {
    /// `lto = false`. Thin-local. Dependencies don't need bitcode.
    Default,
    Off,
    Thin,
    Fat,
    /// `lto = true`. Same as fat.
    True,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Profile {
    pub name: String,
    /// `dev` or `release`. What the `PROFILE` env var says.
    pub root: String,
    pub opt_level: String,
    pub debuginfo: String,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub lto: Lto,
    pub panic: String,
    pub codegen_units: Option<u32>,
    pub incremental: bool,
    pub rpath: bool,
    pub strip: Option<String>,
    pub split_debuginfo: Option<String>,
    pub codegen_backend: Option<String>,
    pub rustflags: Vec<String>,
}

impl Profile {
    fn builtin(name: &str) -> Self {
        let release = name == "release";
        Self {
            name: name.to_owned(),
            root: if release { "release" } else { "dev" }.to_owned(),
            opt_level: if release { "3" } else { "0" }.to_owned(),
            debuginfo: if release { "0" } else { "2" }.to_owned(),
            debug_assertions: !release,
            overflow_checks: !release,
            lto: Lto::Default,
            panic: "unwind".into(),
            codegen_units: None,
            incremental: !release,
            rpath: false,
            strip: None,
            split_debuginfo: None,
            codegen_backend: None,
            rustflags: Vec::new(),
        }
    }

    pub fn debuginfo_on(&self) -> bool {
        self.debuginfo != "0"
    }

    fn apply(&mut self, t: &TomlProfile) {
        if let Some(o) = &t.opt_level {
            self.opt_level = match o {
                StringOrInt::Int(i) => i.to_string(),
                StringOrInt::String(s) => s.clone(),
            };
        }
        if let Some(d) = &t.debug {
            self.debuginfo = debuginfo_value(d);
        }
        if let Some(v) = t.debug_assertions {
            self.debug_assertions = v;
        }
        if let Some(v) = t.overflow_checks {
            self.overflow_checks = v;
        }
        if let Some(l) = &t.lto {
            self.lto = match l {
                StringOrBool::Bool(false) => Lto::Default,
                StringOrBool::Bool(true) => Lto::True,
                StringOrBool::String(s) => match s.as_str() {
                    "off" => Lto::Off,
                    "thin" => Lto::Thin,
                    "fat" => Lto::Fat,
                    _ => Lto::Default,
                },
            };
        }
        if let Some(p) = &t.panic {
            self.panic = p.clone();
        }
        if let Some(c) = t.codegen_units {
            self.codegen_units = Some(c);
        }
        if let Some(i) = t.incremental {
            self.incremental = i;
        }
        if let Some(r) = t.rpath {
            self.rpath = r;
        }
        if let Some(s) = &t.strip {
            self.strip = Some(match s {
                StringOrBool::Bool(true) => "symbols".into(),
                StringOrBool::Bool(false) => "none".into(),
                StringOrBool::String(s) => s.clone(),
            });
        }
        if let Some(s) = &t.split_debuginfo {
            self.split_debuginfo = Some(s.clone());
        }
        if let Some(b) = &t.codegen_backend {
            self.codegen_backend = Some(b.clone());
        }
        if let Some(f) = &t.rustflags {
            self.rustflags.extend(f.iter().cloned());
        }
    }
}

pub struct Profiles {
    pub requested: String,
    pub dir_name: String,
    base: Profile,
    chain: Vec<TomlProfile>,
    incremental_override: Option<bool>,
    /// Host units get no debuginfo, but only when there's no `--target`. Explicit targets keep the profile's.
    pub weaken_host_debuginfo: bool,
}

fn env_profile(name: &str) -> Result<Option<TomlProfile>> {
    let prefix = format!("CARGO_PROFILE_{}_", name.to_uppercase().replace('-', "_"));
    let mut table = toml::Table::new();
    let mut build_override = toml::Table::new();
    for (k, v) in std::env::vars() {
        let Some(rest) = k.strip_prefix(&prefix) else { continue };
        let (target, key) = match rest.strip_prefix("BUILD_OVERRIDE_") {
            Some(key) => (&mut build_override, key),
            None => (&mut table, rest),
        };
        let value = if let Ok(b) = v.parse::<bool>() {
            toml::Value::Boolean(b)
        } else if let Ok(i) = v.parse::<i64>() {
            toml::Value::Integer(i)
        } else {
            toml::Value::String(v.clone())
        };
        target.insert(key.to_lowercase().replace('_', "-"), value);
    }
    if !build_override.is_empty() {
        table.insert("build-override".into(), toml::Value::Table(build_override));
    }
    if table.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        toml::Value::Table(table)
            .try_into()
            .with_context(|| format!("invalid {prefix}* environment variables"))?,
    ))
}

/// `name`, `name@version`, or the older `name:version`.
fn spec_matches(spec: &str, pkg: &Pkg) -> bool {
    let (name, version) = match spec.split_once(['@', ':']) {
        Some((n, v)) => (n, Some(v)),
        None => (spec, None),
    };
    spec != "*" && name == pkg.name && version.is_none_or(|v| v == pkg.version.to_string())
}

impl Profiles {
    pub fn new(root_manifest: &toml::Table, config: &[TomlProfiles], requested: &str, incremental_override: Option<bool>) -> Result<Self> {
        let manifest: Option<TomlProfiles> = match root_manifest.get("profile") {
            Some(p) => Some(p.clone().try_into().context("invalid [profile] in the workspace manifest")?),
            None => None,
        };
        let layer = |name: &str| -> Result<TomlProfile> {
            let mut t = TomlProfile::default();
            if let Some(m) = manifest.as_ref().and_then(|m| m.get(name)) {
                t.merge(m);
            }
            for c in config {
                if let Some(p) = c.get(name) {
                    t.merge(p);
                }
            }
            if let Some(e) = env_profile(name)? {
                t.merge(&e);
            }
            Ok(t)
        };
        let mut chain = Vec::new();
        let mut name = requested.to_owned();
        let mut seen = Vec::new();
        let base_name = loop {
            if seen.contains(&name) {
                bail!("profile inheritance loop detected with profile `{name}`");
            }
            seen.push(name.clone());
            let t = layer(&name)?;
            let inherits = t.inherits.clone();
            chain.insert(0, t);
            match name.as_str() {
                "dev" | "release" => break name,
                "test" => name = "dev".into(),
                "bench" => name = "release".into(),
                _ => match inherits {
                    Some(parent) => name = parent,
                    None => bail!(
                        "profile `{name}` is missing an `inherits` directive (`inherits` is required for all profiles except `dev` or `release`)"
                    ),
                },
            }
        };
        let mut base = Profile::builtin(&base_name);
        base.name = requested.to_owned();
        let dir_name = match requested {
            "dev" | "test" => "debug".to_owned(),
            "bench" => "release".to_owned(),
            other => chain.last().and_then(|t| t.dir_name.clone()).unwrap_or_else(|| other.to_owned()),
        };
        Ok(Self {
            requested: requested.to_owned(),
            dir_name,
            base,
            chain,
            incremental_override,
            weaken_host_debuginfo: true,
        })
    }

    /// Before per-package and host tweaks. Cargo's `Finished` line.
    pub fn base_profile(&self) -> Profile {
        let mut p = self.base.clone();
        for t in &self.chain {
            p.apply(t);
        }
        p
    }

    /// `for_host` is build scripts, proc-macros, and their deps.
    /// `apple` defaults split-debuginfo to unpacked.
    pub fn get(&self, pkg: &Pkg, for_host: bool, apple: bool) -> Profile {
        let mut p = self.base.clone();
        for t in &self.chain {
            p.apply(t);
        }
        let debug_off_in_profile = !p.debuginfo_on();
        if for_host {
            p.opt_level = "0".into();
            p.codegen_units = None;
            if self.weaken_host_debuginfo {
                p.debuginfo = "0".into();
            }
            for t in &self.chain {
                if let Some(bo) = &t.build_override {
                    p.apply(bo);
                }
            }
            // Build scripts and proc-macros have to unwind.
            p.panic = "unwind".into();
        }
        for t in &self.chain {
            let Some(pk) = &t.package else { continue };
            if !pkg.is_member
                && let Some(all) = pk.get("*")
            {
                p.apply(all);
            }
            for (spec, o) in pk {
                if spec_matches(spec, pkg) {
                    p.apply(o);
                }
            }
        }
        if let Some(i) = self.incremental_override {
            p.incremental = i;
        }
        if p.strip.is_none() && debug_off_in_profile && !p.debuginfo_on() {
            p.strip = Some("debuginfo".into());
        }
        if p.strip.as_deref() == Some("none") {
            p.strip = None;
        }
        if apple && p.split_debuginfo.is_none() && p.debuginfo_on() {
            p.split_debuginfo = Some("unpacked".into());
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Source;

    fn pkg(member: bool) -> Pkg {
        Pkg {
            id: "x".into(),
            identity: "x".into(),
            name: "x".into(),
            version: semver::Version::new(1, 0, 0),
            manifest_path: "/x/Cargo.toml".into(),
            root: "/x".into(),
            source: if member { Source::Workspace } else { Source::Registry("r".into()) },
            is_member: member,
            loaded: true,
            links: None,
            edition: "2021".into(),
            targets: vec![],
            declared_features: vec![],
            proc_macro: false,
            authors: vec![],
            description: None,
            homepage: None,
            repository: None,
            license: None,
            license_file: None,
            rust_version: None,
            readme: None,
            default_run: None,
            deps: vec![],
            dep_targets: vec![],
            lints: None,
        }
    }

    #[test]
    fn release_and_overrides() {
        let manifest: toml::Table = toml::from_str(
            r#"
            [profile.release]
            lto = "thin"
            [profile.release.package."*"]
            opt-level = 2
            [profile.dist]
            inherits = "release"
            strip = true
            "#,
        )
        .unwrap();
        let r = Profiles::new(&manifest, &[], "release", None).unwrap();
        let member = r.get(&pkg(true), false, true);
        assert_eq!((member.opt_level.as_str(), member.lto), ("3", Lto::Thin));
        assert_eq!(member.strip.as_deref(), Some("debuginfo"));
        assert_eq!(r.get(&pkg(false), false, true).opt_level, "2");
        let host = r.get(&pkg(true), true, true);
        assert_eq!(host.opt_level, "0");

        let specs: toml::Table = toml::from_str(
            r#"
            [profile.dev]
            debug = "line-tables-only"
            opt-level = "s"
            [profile.dev.package."x@1.0.0"]
            debug = "limited"
            [profile.dev.package."x@2.0.0"]
            opt-level = 3
            "#,
        )
        .unwrap();
        let dev = Profiles::new(&specs, &[], "dev", None).unwrap().get(&pkg(true), false, false);
        assert_eq!((dev.debuginfo.as_str(), dev.opt_level.as_str()), ("1", "s"));

        let d = Profiles::new(&manifest, &[], "dist", None).unwrap();
        assert_eq!(d.dir_name, "dist");
        assert_eq!(d.get(&pkg(true), false, true).strip.as_deref(), Some("symbols"));

        let dev = Profiles::new(&manifest, &[], "dev", None).unwrap();
        assert_eq!(dev.dir_name, "debug");
        let p = dev.get(&pkg(true), false, true);
        assert_eq!(p.split_debuginfo.as_deref(), Some("unpacked"));
        assert!(p.incremental && p.debug_assertions && p.strip.is_none());
    }
}
