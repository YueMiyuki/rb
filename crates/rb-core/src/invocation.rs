//! Arguments and env split into hashed (changes the output) and unhashed (UI, jobserver, absolute paths).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Placeholder for the unit's name id until we've computed it. `-C metadata`, file names, directories.
pub const SELF_NAME: &str = "__RB_SELF_NAME__";
pub const SELF_NAME16: &str = "__RB_SELF_NAME16__";

#[derive(Clone, Debug, Default)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<(String, bool)>,
    pub env: BTreeMap<String, (String, bool)>,
    pub cwd: PathBuf,
    pub path_prepend: Vec<PathBuf>,
}

/// Swap machine-specific path prefixes for stable tokens before hashing. Same inputs, different worktree, same key.
pub struct Normalizer {
    rules: Vec<(String, &'static str)>,
}

impl Normalizer {
    pub fn new(target_dir: &Path, ws_root: &Path) -> Self {
        let mut rules = Vec::new();
        // Tools report the path we gave them, or the symlink-resolved one. `/tmp` is `/private/tmp` on macOS. Same token either way.
        for (dir, token) in [(target_dir, "{TARGET}"), (ws_root, "{WS}")] {
            rules.push((dir.to_string_lossy().into_owned(), token));
            if let Ok(c) = std::fs::canonicalize(dir)
                && c != dir
            {
                rules.push((c.to_string_lossy().into_owned(), token));
            }
        }
        // Longest prefix first. The target dir usually lives inside the workspace.
        rules.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        Self { rules }
    }

    pub fn apply(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (from, to) in &self.rules {
            if out.contains(from.as_str()) {
                out = out.replace(from.as_str(), to);
            }
        }
        out
    }
}

impl Invocation {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            cwd: cwd.into(),
            ..Default::default()
        }
    }

    pub fn arg(&mut self, a: impl Into<String>) -> &mut Self {
        self.args.push((a.into(), true));
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(&mut self, it: I) -> &mut Self {
        for a in it {
            self.arg(a);
        }
        self
    }

    pub fn arg_unhashed(&mut self, a: impl Into<String>) -> &mut Self {
        self.args.push((a.into(), false));
        self
    }

    pub fn env(&mut self, k: impl Into<String>, v: impl Into<String>) -> &mut Self {
        self.env.insert(k.into(), (v.into(), true));
        self
    }

    pub fn env_unhashed(&mut self, k: impl Into<String>, v: impl Into<String>) -> &mut Self {
        self.env.insert(k.into(), (v.into(), false));
        self
    }

    pub fn get_env(&self, k: &str) -> Option<&str> {
        self.env.get(k).map(|(v, _)| v.as_str())
    }

    pub fn hash_into(&self, h: &mut blake3::Hasher, norm: &Normalizer) {
        h.update(b"program\0");
        h.update(norm.apply(&self.program.to_string_lossy()).as_bytes());
        h.update(b"\0cwd\0");
        h.update(norm.apply(&self.cwd.to_string_lossy()).as_bytes());
        for (a, hashed) in &self.args {
            if *hashed {
                h.update(b"\0a\0");
                h.update(norm.apply(a).as_bytes());
            }
        }
        for (k, (v, hashed)) in &self.env {
            if *hashed {
                h.update(b"\0e\0");
                h.update(k.as_bytes());
                h.update(b"=");
                h.update(norm.apply(v).as_bytes());
            }
        }
    }

    pub fn substitute_name(&mut self, name: &str) {
        let name16 = &name[..16];
        let sub = |s: &mut String| {
            if s.contains("__RB_SELF_NAME") {
                *s = s.replace(SELF_NAME16, name16).replace(SELF_NAME, name);
            }
        };
        for (a, _) in &mut self.args {
            sub(a);
        }
        for (v, _) in self.env.values_mut() {
            sub(v);
        }
        let mut p = self.program.to_string_lossy().into_owned();
        sub(&mut p);
        self.program = PathBuf::from(p);
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(self.args.iter().map(|(a, _)| a)).current_dir(&self.cwd);
        for (k, (v, _)) in &self.env {
            cmd.env(k, v);
        }
        if !self.path_prepend.is_empty() {
            let path = std::env::var_os("PATH").unwrap_or_default();
            if let Ok(joined) = std::env::join_paths(self.path_prepend.iter().cloned().chain(std::env::split_paths(&path))) {
                cmd.env("PATH", joined);
            }
        }
        cmd
    }

    pub fn display(&self, with_env: bool) -> String {
        let q = |s: &str| shell_words::quote(s).into_owned();
        let mut parts = Vec::new();
        if with_env {
            for (k, (v, _)) in &self.env {
                parts.push(format!("{k}={}", q(v)));
            }
        }
        parts.push(q(&self.program.to_string_lossy()));
        parts.extend(self.args.iter().map(|(a, _)| q(a)));
        parts.join(" ")
    }
}
