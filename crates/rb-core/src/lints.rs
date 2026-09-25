//! `[lints]` as rustc flags. Inheritance is already filled in when the manifest is parsed.

#[derive(Default)]
pub struct Lints {
    pub flags: Vec<String>,
    pub check_cfg: Vec<String>,
}

/// Flags for a package's normalized `[lints]` table
pub fn from_table(table: Option<&toml::Table>) -> Lints {
    let mut out = Lints::default();
    let Some(tools) = table else { return out };
    let mut entries: Vec<(i64, String, String)> = Vec::new();
    for (tool, lints) in tools {
        let Some(lints) = lints.as_table() else { continue };
        for (name, v) in lints {
            let (level, priority) = match v {
                toml::Value::String(level) => (level.clone(), 0),
                toml::Value::Table(t) => {
                    if tool == "rust"
                        && name == "unexpected_cfgs"
                        && let Some(cc) = t.get("check-cfg").and_then(|c| c.as_array())
                    {
                        out.check_cfg.extend(cc.iter().filter_map(|c| c.as_str()).map(str::to_owned));
                    }
                    match t.get("level").and_then(|l| l.as_str()) {
                        Some(level) => (level.to_owned(), t.get("priority").and_then(|p| p.as_integer()).unwrap_or(0)),
                        None => continue,
                    }
                }
                _ => continue,
            };
            let full = if tool == "rust" { name.clone() } else { format!("{tool}::{name}") };
            entries.push((priority, full, level));
        }
    }
    entries.sort();
    out.flags = entries.into_iter().map(|(_, name, level)| format!("--{level}={name}")).collect();
    out
}
