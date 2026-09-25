//! A target triple, or a `cfg(...)` checked against `rustc --print cfg`.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Cfg {
    Name(String),
    KeyValue(String, String),
    All(Vec<Cfg>),
    Any(Vec<Cfg>),
    Not(Box<Cfg>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlatformExpr {
    Triple(String),
    Cfg(Cfg),
}

#[derive(Clone, Copy)]
pub struct PlatformCfg<'a> {
    pub triple: &'a str,
    pub cfg: &'a [(String, Option<String>)],
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Result<String> {
        self.ws();
        let start = self.i;
        while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_') {
            self.i += 1;
        }
        if start == self.i {
            bail!("expected identifier at offset {start}");
        }
        Ok(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }

    fn string(&mut self) -> Result<String> {
        self.ws();
        if !self.eat(b'"') {
            bail!("expected string at offset {}", self.i);
        }
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i] != b'"' {
            self.i += 1;
        }
        let v = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        self.i += 1;
        Ok(v)
    }

    fn list(&mut self) -> Result<Vec<Cfg>> {
        if !self.eat(b'(') {
            bail!("expected `(`");
        }
        let mut out = Vec::new();
        loop {
            if self.eat(b')') {
                return Ok(out);
            }
            out.push(self.cfg()?);
            self.eat(b',');
        }
    }

    fn cfg(&mut self) -> Result<Cfg> {
        let name = self.ident()?;
        match name.as_str() {
            "all" => Ok(Cfg::All(self.list()?)),
            "any" => Ok(Cfg::Any(self.list()?)),
            "not" => {
                let mut l = self.list()?;
                if l.len() != 1 {
                    bail!("not() takes exactly one predicate");
                }
                Ok(Cfg::Not(Box::new(l.remove(0))))
            }
            _ if self.eat(b'=') => Ok(Cfg::KeyValue(name, self.string()?)),
            _ => Ok(Cfg::Name(name)),
        }
    }
}

impl PlatformExpr {
    pub fn parse(s: &str) -> Result<Self> {
        let t = s.trim();
        if let Some(inner) = t.strip_prefix("cfg(").and_then(|r| r.strip_suffix(')')) {
            let mut p = Parser { s: inner.as_bytes(), i: 0 };
            let c = p.cfg()?;
            p.ws();
            if p.i != p.s.len() {
                bail!("unexpected trailing input in `{s}`");
            }
            return Ok(Self::Cfg(c));
        }
        Ok(Self::Triple(t.to_owned()))
    }

    pub fn matches(&self, p: &PlatformCfg<'_>) -> bool {
        match self {
            Self::Triple(t) => t == p.triple,
            Self::Cfg(c) => eval(c, p.cfg),
        }
    }
}

fn eval(c: &Cfg, cfg: &[(String, Option<String>)]) -> bool {
    match c {
        Cfg::Name(n) => cfg.iter().any(|(k, v)| k == n && v.is_none()),
        Cfg::KeyValue(k, v) => cfg.iter().any(|(ck, cv)| ck == k && cv.as_deref() == Some(v.as_str())),
        Cfg::All(l) => l.iter().all(|c| eval(c, cfg)),
        Cfg::Any(l) => l.iter().any(|c| eval(c, cfg)),
        Cfg::Not(c) => !eval(c, cfg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates() {
        let cfg: Vec<(String, Option<String>)> = vec![
            ("unix".into(), None),
            ("target_os".into(), Some("linux".into())),
            ("target_arch".into(), Some("x86_64".into())),
        ];
        let p = PlatformCfg {
            triple: "x86_64-unknown-linux-gnu",
            cfg: &cfg,
        };
        let t = |s: &str| PlatformExpr::parse(s).unwrap().matches(&p);
        assert!(t("cfg(unix)"));
        assert!(!t("cfg(windows)"));
        assert!(t("cfg(all(unix, target_arch = \"x86_64\"))"));
        assert!(t("cfg(any(windows, target_os = \"linux\",))"));
        assert!(t("cfg(not(target_os = \"macos\"))"));
        assert!(t("x86_64-unknown-linux-gnu"));
        assert!(!t("aarch64-apple-darwin"));
    }
}
