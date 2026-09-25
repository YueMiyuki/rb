//! `rb config get`, matching `cargo config get`.

use crate::config::cargo_home;
use crate::shell::Shell;
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
enum Def {
    File(String),
    Env(String),
    Cli,
}

impl Def {
    fn display(&self) -> String {
        match self {
            Self::File(p) => p.clone(),
            Self::Env(k) => format!("environment variable `{k}`"),
            Self::Cli => "--config cli option".into(),
        }
    }
}

#[derive(Clone, Debug)]
enum Val {
    Bool(bool, Def),
    Int(i64, Def),
    Str(String, Def),
    List(Vec<Val>, Def),
    Table(BTreeMap<String, Val>, Def),
}

impl Val {
    fn def(&self) -> &Def {
        match self {
            Self::Bool(_, d) | Self::Int(_, d) | Self::Str(_, d) | Self::List(_, d) | Self::Table(_, d) => d,
        }
    }

    fn desc(&self) -> &'static str {
        match self {
            Self::Bool(_, _) => "boolean",
            Self::Int(_, _) => "integer",
            Self::Str(_, _) => "string",
            Self::List(_, _) => "array",
            Self::Table(_, _) => "table",
        }
    }
}

pub struct GetOptions<'a> {
    pub key: Option<&'a str>,
    pub format: &'a str,
    pub show_origin: bool,
    pub merged: &'a str,
    pub unstable: &'a [String],
    pub cli_config: &'a [String],
}

pub fn config_get(opts: &GetOptions<'_>, shell: &Shell) -> Result<()> {
    let nightly = rustc_is_nightly();
    if !nightly && !opts.unstable.is_empty() {
        shell.error(
            "the `-Z` flag is only accepted on the nightly channel of Cargo, but this is the `stable` channel\n\
See https://doc.rust-lang.org/book/appendix-07-nightly-rust.html for more information about Rust release channels.",
        );
        std::process::exit(101);
    }
    if !nightly {
        shell.error(
            "the `cargo config` command is unstable, and only available on the nightly channel of Cargo, but this is the `stable` channel\n\
See https://doc.rust-lang.org/book/appendix-07-nightly-rust.html for more information about Rust release channels.\n\
See https://github.com/rust-lang/cargo/issues/9301 for more information about the `cargo config` command.",
        );
        std::process::exit(101);
    }
    if !opts.unstable.iter().any(|f| f == "unstable-options") {
        shell.error(
            "the `cargo config` command is unstable, pass `-Z unstable-options` to enable it\n\
See https://github.com/rust-lang/cargo/issues/9301 for more information about the `cargo config` command.",
        );
        std::process::exit(101);
    }
    match opts.format {
        "toml" | "json" | "json-value" => {}
        other => bail!("unknown config format `{other}`"),
    }
    let merged = match opts.merged {
        "yes" => true,
        "no" => false,
        other => bail!("unknown --merged `{other}`"),
    };
    if opts.show_origin && opts.format != "toml" {
        bail!("the `{}` format does not support --show-origin, try the `toml` format instead", opts.format);
    }
    let parts = key_parts(opts.key.unwrap_or(""));
    if opts.key == Some("") {
        bail!("config value `` is not set");
    }
    let cwd = std::env::current_dir()?;
    if merged {
        let root = merge_all(&cwd, opts.cli_config)?;
        let cv = match lookup(&root, &parts)? {
            Some(v) => apply_env(v, &parts),
            None => env_leaf(&parts).ok_or_else(|| anyhow::anyhow!("config value `{}` is not set", opts.key.unwrap_or("")))?,
        };
        match opts.format {
            "toml" => print_toml(opts.show_origin, &parts, &cv),
            "json" => println!("{}", serde_json::to_string(&to_json(&parts, &cv, true)).unwrap()),
            "json-value" => println!("{}", serde_json::to_string(&to_json(&parts, &cv, false)).unwrap()),
            _ => unreachable!(),
        }
        if let Some(env) = maybe_env(&parts, &cv) {
            print_env_note(opts.format, &env);
        }
    } else {
        if opts.format != "toml" {
            bail!("the `{}` format does not support --merged=no, try the `toml` format instead", opts.format);
        }
        print_unmerged(&cwd, opts.cli_config, opts.show_origin, &parts)?;
    }
    Ok(())
}

fn rustc_is_nightly() -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Ok(out) = std::process::Command::new("rustc").arg("-vV").current_dir(cwd).output() else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains("nightly") || text.contains("-dev")
}

fn key_parts(key: &str) -> Vec<String> {
    if key.is_empty() {
        return Vec::new();
    }
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

fn env_key(parts: &[String]) -> String {
    let mut s = String::from("CARGO");
    for p in parts {
        s.push('_');
        s.push_str(&p.to_uppercase().replace('-', "_"));
    }
    s
}

fn from_toml(v: &toml::Value, def: &Def) -> Val {
    match v {
        toml::Value::Boolean(b) => Val::Bool(*b, def.clone()),
        toml::Value::Integer(i) => Val::Int(*i, def.clone()),
        toml::Value::String(s) => Val::Str(s.clone(), def.clone()),
        toml::Value::Float(f) => Val::Str(f.to_string(), def.clone()),
        toml::Value::Datetime(d) => Val::Str(d.to_string(), def.clone()),
        toml::Value::Array(a) => Val::List(a.iter().map(|x| from_toml(x, def)).collect(), def.clone()),
        toml::Value::Table(t) => {
            let mut map = BTreeMap::new();
            for (k, v) in t {
                map.insert(k.clone(), from_toml(v, def));
            }
            Val::Table(map, def.clone())
        }
    }
}

fn merge(low: Val, high: Val) -> Val {
    match (low, high) {
        (Val::Table(mut a, def), Val::Table(b, _)) => {
            for (k, v) in b {
                match a.remove(&k) {
                    Some(existing) => {
                        a.insert(k, merge(existing, v));
                    }
                    None => {
                        a.insert(k, v);
                    }
                }
            }
            Val::Table(a, def)
        }
        (Val::List(mut a, def), Val::List(b, _)) => {
            a.extend(b);
            Val::List(a, def)
        }
        (_, high) => high,
    }
}

struct Layer {
    def: Def,
    table: toml::Table,
}

fn discovered_layers(cwd: &Path) -> Result<Vec<Layer>> {
    let mut paths = Vec::new();
    for dir in cwd.ancestors() {
        let dot = dir.join(".cargo");
        if let Some(p) = [dot.join("config"), dot.join("config.toml")].into_iter().find(|p| p.is_file()) {
            paths.push(p);
        }
    }
    let home = cargo_home();
    if let Some(p) = [home.join("config"), home.join("config.toml")].into_iter().find(|p| p.is_file())
        && !paths.iter().any(|q| same_file(q, &p))
    {
        paths.push(p);
    }
    let mut layers = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)?;
        let table: toml::Table = toml::from_str(&text)?;
        let shown = path.canonicalize().unwrap_or(path).display().to_string();
        layers.push(Layer { def: Def::File(shown), table });
    }
    Ok(layers)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.len() == y.len() && a == b || std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok(),
        _ => false,
    }
}

fn cli_layers(cwd: &Path, specs: &[String]) -> Result<(Option<Layer>, Vec<Layer>)> {
    let mut inline = toml::Table::new();
    let mut files = Vec::new();
    for spec in specs {
        if let Some((key, raw)) = split_set(spec) {
            let value = parse_cli_value(raw);
            insert_dotted(&mut inline, &key_parts(key), value);
        } else {
            let path = if Path::new(spec).is_absolute() { PathBuf::from(spec) } else { cwd.join(spec) };
            if !path.is_file() {
                bail!("--config `{spec}` is not KEY=VALUE or a config file");
            }
            let text = std::fs::read_to_string(&path)?;
            let table: toml::Table = toml::from_str(&text)?;
            files.push(Layer {
                def: Def::File(path.display().to_string()),
                table,
            });
        }
    }
    let inline = if inline.is_empty() {
        None
    } else {
        Some(Layer { def: Def::Cli, table: inline })
    };
    Ok((inline, files))
}

fn split_set(spec: &str) -> Option<(&str, &str)> {
    let eq = spec.find('=')?;
    let (key, raw) = spec.split_at(eq);
    if key.is_empty() || key.contains('/') || key.contains('\\') {
        return None;
    }
    Some((key, &raw[1..]))
}

fn parse_cli_value(raw: &str) -> toml::Value {
    if raw == "true" {
        toml::Value::Boolean(true)
    } else if raw == "false" {
        toml::Value::Boolean(false)
    } else if let Ok(n) = raw.parse::<i64>() {
        toml::Value::Integer(n)
    } else {
        toml::from_str::<toml::Value>(raw).unwrap_or_else(|_| toml::Value::String(raw.to_owned()))
    }
}

fn insert_dotted(table: &mut toml::Table, parts: &[String], value: toml::Value) {
    if parts.is_empty() {
        return;
    }
    if parts.len() == 1 {
        table.insert(parts[0].clone(), value);
        return;
    }
    let entry = table.entry(parts[0].clone()).or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(t) = entry {
        insert_dotted(t, &parts[1..], value);
    }
}

/// Lowest priority first: home and distant files, then nearer files, then `--config` files, then inline `--config`.
fn layers_low_to_high(cwd: &Path, cli: &[String]) -> Result<Vec<Layer>> {
    let mut found = discovered_layers(cwd)?;
    found.reverse();
    let (inline, files) = cli_layers(cwd, cli)?;
    found.extend(files);
    if let Some(inline) = inline {
        found.push(inline);
    }
    Ok(found)
}

fn merge_all(cwd: &Path, cli: &[String]) -> Result<Val> {
    let mut acc = Val::Table(BTreeMap::new(), Def::Cli);
    for layer in layers_low_to_high(cwd, cli)? {
        let high = from_toml(&toml::Value::Table(layer.table), &layer.def);
        acc = merge(acc, high);
    }
    Ok(acc)
}

fn lookup(root: &Val, parts: &[String]) -> Result<Option<Val>> {
    let mut cur = root.clone();
    let mut so_far: Vec<&str> = Vec::new();
    for part in parts {
        match cur {
            Val::Table(map, _) => {
                match map.get(part) {
                    Some(v) => {
                        so_far.push(part);
                        cur = v.clone();
                    }
                    None => return Ok(None),
                }
            }
            other => bail!(
                "expected table for configuration key `{}`, but found {} in {}",
                so_far.join("."),
                other.desc(),
                other.def().display()
            ),
        }
    }
    if parts.is_empty() {
        return Ok(Some(cur));
    }
    Ok(Some(cur))
}

fn parse_env(raw: &str, def: Def) -> Val {
    if raw == "true" {
        Val::Bool(true, def)
    } else if raw == "false" {
        Val::Bool(false, def)
    } else if let Ok(n) = raw.parse::<i64>() {
        Val::Int(n, def)
    } else {
        Val::Str(raw.to_owned(), def)
    }
}

fn env_leaf(parts: &[String]) -> Option<Val> {
    if parts.is_empty() {
        return None;
    }
    let name = env_key(parts);
    let raw = std::env::var(&name).ok().filter(|s| !s.is_empty())?;
    Some(parse_env(&raw, Def::Env(name)))
}

fn apply_env(val: Val, parts: &[String]) -> Val {
    if parts.is_empty() || matches!(val, Val::Table(_, _)) {
        return val;
    }
    let name = env_key(parts);
    let Ok(raw) = std::env::var(&name) else { return val };
    if raw.is_empty() {
        return val;
    }
    match val {
        Val::List(mut items, def) => {
            for part in raw.split_whitespace() {
                items.push(Val::Str(part.to_owned(), Def::Env(name.clone())));
            }
            Val::List(items, def)
        }
        _ => parse_env(&raw, Def::Env(name)),
    }
}

fn env_snapshot() -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars().filter(|(_, v)| !v.is_empty()).collect();
    env.entry("CARGO_HOME".into()).or_insert_with(|| cargo_home().display().to_string());
    env
}

fn maybe_env(parts: &[String], cv: &Val) -> Option<Vec<(String, String)>> {
    if !matches!(cv, Val::Table(_, _)) {
        return None;
    }
    let prefix = format!("{}_", env_key(parts));
    let env: Vec<_> = env_snapshot().into_iter().filter(|(k, _)| k.starts_with(&prefix)).collect();
    if env.is_empty() { None } else { Some(env) }
}

fn print_env_note(format: &str, env: &[(String, String)]) {
    if format == "toml" {
        println!("# The following environment variables may affect the loaded values.");
        for (k, v) in env {
            println!("# {k}={}", shell_escape(v));
        }
    } else {
        eprintln!("note: The following environment variables may affect the loaded values.");
        for (k, v) in env {
            eprintln!("{k}={}", shell_escape(v));
        }
    }
}

fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let safe = s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '@' | '%' | '+' | '=' | ':' | ','));
    if safe { s.to_owned() } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

fn toml_scalar(v: &Val) -> String {
    match v {
        Val::Bool(b, _) => b.to_string(),
        Val::Int(i, _) => i.to_string(),
        Val::Str(s, _) => toml::Value::String(s.clone()).to_string(),
        Val::List(items, _) => {
            let parts: Vec<_> = items.iter().map(toml_scalar).collect();
            format!("[{}]", parts.join(", "))
        }
        Val::Table(_, _) => "{}".into(),
    }
}

fn print_toml(show_origin: bool, key: &[String], cv: &Val) {
    let name = key.join(".");
    let origin = |d: &Def| if show_origin { format!(" # {}", d.display()) } else { String::new() };
    match cv {
        Val::Table(map, _) => {
            for (k, v) in map {
                let mut sub = key.to_vec();
                sub.push(k.clone());
                print_toml(show_origin, &sub, v);
            }
        }
        Val::List(items, _) if show_origin => {
            println!("{name} = [");
            for item in items {
                println!("    {}, # {}", toml_scalar(item), item.def().display());
            }
            println!("]");
        }
        other => println!("{name} = {}{}", toml_scalar(other), origin(other.def())),
    }
}

fn to_json(key: &[String], cv: &Val, include_key: bool) -> serde_json::Value {
    let value = cv_json(cv);
    if key.is_empty() || !include_key {
        return value;
    }
    let mut root = serde_json::json!({});
    let mut table = &mut root;
    let (last, prefix) = key.split_last().unwrap();
    for part in prefix {
        table[part] = serde_json::json!({});
        table = table.get_mut(part).unwrap();
    }
    table[last] = value;
    root
}

fn cv_json(cv: &Val) -> serde_json::Value {
    match cv {
        Val::Bool(b, _) => serde_json::json!(b),
        Val::Int(i, _) => serde_json::json!(i),
        Val::Str(s, _) => serde_json::json!(s),
        Val::List(items, _) => serde_json::Value::Array(items.iter().map(cv_json).collect()),
        Val::Table(map, _) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                obj.insert(k.clone(), cv_json(v));
            }
            serde_json::Value::Object(obj)
        }
    }
}

fn print_unmerged(cwd: &Path, cli: &[String], show_origin: bool, parts: &[String]) -> Result<()> {
    let layers = layers_low_to_high(cwd, cli)?;
    // Cargo prints cli args, then env, then files from highest priority to lowest.
    let mut cli_layer = None;
    let mut files = Vec::new();
    for layer in layers {
        if matches!(layer.def, Def::Cli) {
            cli_layer = Some(layer);
        } else {
            files.push(layer);
        }
    }
    files.reverse();
    if let Some(layer) = cli_layer {
        let cv = from_toml(&toml::Value::Table(layer.table), &layer.def);
        if let Some(cv) = trim(&cv, parts)? {
            println!("# {}", cv.def().display());
            print_toml(show_origin, &[], &cv);
            println!();
        }
    }
    let prefix = env_key(parts);
    let env: Vec<_> = env_snapshot().into_iter().filter(|(k, _)| k.starts_with(&prefix)).collect();
    if !env.is_empty() {
        println!("# Environment variables");
        for (k, v) in &env {
            println!("# {k}={}", shell_escape(v));
        }
        println!();
    }
    for layer in files {
        let cv = from_toml(&toml::Value::Table(layer.table), &layer.def);
        if let Some(cv) = trim(&cv, parts)? {
            println!("# {}", cv.def().display());
            print_toml(show_origin, &[], &cv);
            println!();
        }
    }
    Ok(())
}

fn trim(cv: &Val, parts: &[String]) -> Result<Option<Val>> {
    nest_trim(cv, parts, &[])
}

fn nest_trim(cv: &Val, parts: &[String], so_far: &[String]) -> Result<Option<Val>> {
    if parts.is_empty() {
        return match cv {
            Val::Table(map, _) if map.is_empty() => Ok(None),
            other => Ok(Some(other.clone())),
        };
    }
    match cv {
        Val::Table(map, def) => {
            let part = &parts[0];
            let Some(child) = map.get(part) else { return Ok(None) };
            let mut next = so_far.to_vec();
            next.push(part.clone());
            let Some(trimmed) = nest_trim(child, &parts[1..], &next)? else { return Ok(None) };
            let mut kept = BTreeMap::new();
            kept.insert(part.clone(), trimmed);
            Ok(Some(Val::Table(kept, def.clone())))
        }
        other => bail!(
            "expected table for configuration key `{}`, but found {} in {}",
            so_far.join("."),
            other.desc(),
            other.def().display()
        ),
    }
}
