//! rustc dep-info: files it read, and `env!` lookups.

use std::path::{Path, PathBuf};

#[derive(Debug, Default, PartialEq)]
pub struct DepInfo {
    pub files: Vec<PathBuf>,
    pub env: Vec<(String, Option<String>)>,
}

fn unescape_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn split_paths(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                cur.push(' ');
                chars.next();
            }
            ' ' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn parse_str(text: &str, cwd: &Path) -> DepInfo {
    let mut info = DepInfo::default();
    let mut first_rule = true;
    for line in text.lines() {
        if let Some(env) = line.strip_prefix("# env-dep:") {
            match env.split_once('=') {
                Some((k, v)) => info.env.push((k.to_owned(), Some(unescape_env(v)))),
                None => info.env.push((env.to_owned(), None)),
            }
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() || !first_rule {
            continue;
        }
        // The first rule lists every input; later rules repeat subsets
        if let Some(idx) = line.find(": ").or_else(|| line.strip_suffix(':').map(|l| l.len())) {
            let deps = line.get(idx + 1..).unwrap_or("");
            for p in split_paths(deps) {
                let path = PathBuf::from(&p);
                info.files.push(if path.is_absolute() { path } else { cwd.join(path) });
            }
            first_rule = false;
        }
    }
    info
}

pub fn parse(path: &Path, cwd: &Path) -> std::io::Result<DepInfo> {
    Ok(parse_str(&std::fs::read_to_string(path)?, cwd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules_and_env() {
        let text = "/t/deps/libfoo-1.rmeta: src/lib.rs /abs/my\\ file.txt\n\n/t/deps/foo-1.d: src/lib.rs\n\nsrc/lib.rs:\n\n# env-dep:CARGO_PKG_NAME=foo\n# env-dep:MULTI=a\\nb\n# env-dep:UNSET\n";
        let d = parse_str(text, Path::new("/ws"));
        assert_eq!(d.files, vec![PathBuf::from("/ws/src/lib.rs"), PathBuf::from("/abs/my file.txt")]);
        assert_eq!(d.env[1], ("MULTI".into(), Some("a\nb".into())));
        assert_eq!(d.env[2], ("UNSET".into(), None));
    }
}
