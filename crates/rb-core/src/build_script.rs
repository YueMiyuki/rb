//! `cargo:` and `cargo::` lines from a build script.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkArgTarget {
    All,
    Bins,
    Bin(String),
    Cdylib,
    Tests,
    Examples,
    Benches,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildOutput {
    pub link_libs: Vec<String>,
    pub link_search: Vec<String>,
    pub link_args: Vec<(LinkArgTarget, String)>,
    pub cfgs: Vec<String>,
    pub check_cfgs: Vec<String>,
    pub env: Vec<(String, String)>,
    pub metadata: Vec<(String, String)>,
    pub rerun_if_changed: Vec<String>,
    pub rerun_if_env_changed: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl BuildOutput {
    pub fn parse(stdout: &str) -> Self {
        let mut out = Self::default();
        for line in stdout.lines() {
            let (new_syntax, rest) = if let Some(r) = line.strip_prefix("cargo::") {
                (true, r)
            } else if let Some(r) = line.strip_prefix("cargo:") {
                (false, r)
            } else {
                continue;
            };
            let Some((key, value)) = rest.split_once('=') else { continue };
            let value = value.to_owned();
            match key {
                "rustc-link-lib" => out.link_libs.push(value),
                "rustc-link-search" => out.link_search.push(value),
                "rustc-link-arg" => out.link_args.push((LinkArgTarget::All, value)),
                "rustc-link-arg-bins" => out.link_args.push((LinkArgTarget::Bins, value)),
                "rustc-link-arg-bin" => {
                    if let Some((bin, arg)) = value.split_once('=') {
                        out.link_args.push((LinkArgTarget::Bin(bin.to_owned()), arg.to_owned()));
                    }
                }
                "rustc-link-arg-cdylib" | "rustc-cdylib-link-arg" => out.link_args.push((LinkArgTarget::Cdylib, value)),
                "rustc-link-arg-tests" => out.link_args.push((LinkArgTarget::Tests, value)),
                "rustc-link-arg-examples" => out.link_args.push((LinkArgTarget::Examples, value)),
                "rustc-link-arg-benches" => out.link_args.push((LinkArgTarget::Benches, value)),
                "rustc-cfg" => out.cfgs.push(value),
                "rustc-check-cfg" => out.check_cfgs.push(value),
                "rustc-env" => {
                    if let Some((k, v)) = value.split_once('=') {
                        out.env.push((k.to_owned(), v.to_owned()));
                    }
                }
                "rustc-flags" => {
                    let mut it = value.split_whitespace();
                    while let Some(flag) = it.next() {
                        let (kind, v) = if flag.len() > 2 {
                            (&flag[..2], Some(flag[2..].to_owned()))
                        } else {
                            (flag, it.next().map(str::to_owned))
                        };
                        match (kind, v) {
                            ("-l", Some(v)) => out.link_libs.push(v),
                            ("-L", Some(v)) => out.link_search.push(v),
                            _ => {}
                        }
                    }
                }
                "warning" => out.warnings.push(value),
                "error" => out.errors.push(value),
                "rerun-if-changed" => out.rerun_if_changed.push(value),
                "rerun-if-env-changed" => out.rerun_if_env_changed.push(value),
                "metadata" if new_syntax => {
                    if let Some((k, v)) = value.split_once('=') {
                        out.metadata.push((k.to_owned(), v.to_owned()));
                    }
                }
                _ if !new_syntax => out.metadata.push((key.to_owned(), value)),
                _ => out.warnings.push(format!("unknown build script instruction `cargo::{key}`")),
            }
        }
        out
    }

    /// Apply `f` to every value that may contain a path
    pub fn map_strings(&self, f: impl Fn(&str) -> String) -> Self {
        let m = |v: &Vec<String>| v.iter().map(|s| f(s)).collect::<Vec<_>>();
        Self {
            link_libs: m(&self.link_libs),
            link_search: m(&self.link_search),
            link_args: self.link_args.iter().map(|(t, a)| (t.clone(), f(a))).collect(),
            cfgs: self.cfgs.clone(),
            check_cfgs: self.check_cfgs.clone(),
            env: self.env.iter().map(|(k, v)| (k.clone(), f(v))).collect(),
            metadata: self.metadata.iter().map(|(k, v)| (k.clone(), f(v))).collect(),
            rerun_if_changed: m(&self.rerun_if_changed),
            rerun_if_env_changed: self.rerun_if_env_changed.clone(),
            warnings: self.warnings.clone(),
            errors: self.errors.clone(),
        }
    }
}

/// `CARGO_FEATURE_<NAME>` / `DEP_<LINKS>_<KEY>` style environment names
pub fn env_name(s: &str) -> String {
    s.to_uppercase().replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_syntaxes() {
        let o = BuildOutput::parse(
            "cargo:rustc-link-lib=static=foo\ncargo::rustc-link-search=native=/x/out\ncargo:rustc-cfg=has_foo\n\
             cargo::rustc-check-cfg=cfg(has_foo)\ncargo:include=/x/out/include\ncargo::metadata=root=/x\n\
             cargo:rustc-flags=-l bar -L/y\ncargo:rustc-env=GIT=abc\ncargo:warning=careful\nnoise\n",
        );
        assert_eq!(o.link_libs, ["static=foo", "bar"]);
        assert_eq!(o.link_search, ["native=/x/out", "/y"]);
        assert_eq!(o.cfgs, ["has_foo"]);
        assert_eq!(
            o.metadata,
            [("include".into(), "/x/out/include".into()), ("root".into(), "/x".into())]
        );
        assert_eq!(o.env, [("GIT".into(), "abc".into())]);
        assert_eq!(o.warnings, ["careful"]);
    }
}
