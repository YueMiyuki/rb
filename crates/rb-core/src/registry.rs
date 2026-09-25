//! Sparse index, checksummed `.crate` downloads, extraction into rb's own source dir.

use crate::cfgexpr::PlatformExpr;
use crate::manifest::{CRATES_IO, DepDecl, DepKind, DepSource, features_with_implicit};
use anyhow::{Context, Result, bail};
use semver::{Version, VersionReq};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub name: String,
    pub vers: Version,
    pub deps: Vec<DepDecl>,
    pub cksum: String,
    pub features: BTreeMap<String, Vec<String>>,
    pub yanked: bool,
    pub links: Option<String>,
    pub rust_version: Option<Version>,
}

#[derive(Deserialize)]
struct RawDep {
    name: String,
    req: String,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    optional: bool,
    #[serde(default = "yes")]
    default_features: bool,
    target: Option<String>,
    kind: Option<String>,
    registry: Option<String>,
    package: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct RawEntry {
    name: String,
    vers: String,
    #[serde(default)]
    deps: Vec<RawDep>,
    cksum: String,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    features2: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    yanked: bool,
    links: Option<String>,
    rust_version: Option<String>,
}

fn source_for_index_url(url: &str) -> String {
    let u = url.trim_end_matches('/');
    if u == "https://github.com/rust-lang/crates.io-index" || u == "https://index.crates.io" || u == "sparse+https://index.crates.io" {
        CRATES_IO.to_owned()
    } else if u.starts_with("sparse+") || u.starts_with("registry+") {
        format!("{u}/")
    } else {
        format!("registry+{u}")
    }
}

fn parse_entry(line: &str, source: &str) -> Result<IndexEntry> {
    let raw: RawEntry = serde_json::from_str(line)?;
    let mut deps = Vec::with_capacity(raw.deps.len());
    for d in raw.deps {
        let (name, rename) = match d.package {
            Some(p) => (p, Some(d.name)),
            None => (d.name, None),
        };
        deps.push(DepDecl {
            req: VersionReq::parse(&d.req).with_context(|| format!("invalid requirement `{}` for {name}", d.req))?,
            source: DepSource::Registry(d.registry.as_deref().map(source_for_index_url).unwrap_or_else(|| source.to_owned())),
            kind: match d.kind.as_deref() {
                Some("dev") => DepKind::Dev,
                Some("build") => DepKind::Build,
                _ => DepKind::Normal,
            },
            optional: d.optional,
            default_features: d.default_features,
            features: d.features,
            platform: d.target.as_deref().map(PlatformExpr::parse).transpose()?,
            name,
            rename,
        });
    }
    let mut features = raw.features;
    features.extend(raw.features2);
    let features = features_with_implicit(features, &deps);
    Ok(IndexEntry {
        vers: Version::parse(&raw.vers)?,
        name: raw.name,
        deps,
        cksum: raw.cksum,
        features,
        yanked: raw.yanked,
        links: raw.links,
        rust_version: raw.rust_version.and_then(|v| {
            let v = if v.matches('.').count() == 1 { format!("{v}.0") } else { v };
            Version::parse(&v).ok()
        }),
    })
}

/// `se/rd/serde`.
pub fn index_path(name: &str) -> String {
    let n = name.to_lowercase();
    match n.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[..1]),
        _ => format!("{}/{}/{n}", &n[0..2], &n[2..4]),
    }
}

pub fn source_dir_name(source: &str) -> String {
    if source == CRATES_IO {
        return "index.crates.io".into();
    }
    let host: String = source
        .split("://")
        .nth(1)
        .unwrap_or(source)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' })
        .collect();
    format!("{}-{}", host.trim_matches('-'), &blake3::hash(source.as_bytes()).to_hex()[..12])
}

type EntryCache = HashMap<(String, String), Arc<Vec<IndexEntry>>>;

pub struct Registry {
    root: PathBuf,
    offline: bool,
    /// `$CARGO_HOME/registry/cache`. Reuse a `.crate` after the checksum checks out.
    mirror: Option<PathBuf>,
    agent: ureq::Agent,
    entries: Mutex<EntryCache>,
    dl: Mutex<HashMap<String, String>>,
    /// Source id a cargo vendor directory replaces.
    vendor: Mutex<HashMap<String, PathBuf>>,
    /// Git index checkout for this process, by source id.
    git_index: Mutex<HashMap<String, PathBuf>>,
}

pub struct Download<'a> {
    pub source: &'a str,
    pub name: &'a str,
    pub version: &'a Version,
    pub checksum: Option<&'a str>,
}

impl Registry {
    pub fn new(root: PathBuf, offline: bool, mirror: Option<PathBuf>) -> Self {
        Self {
            root,
            offline,
            mirror,
            agent: ureq::Agent::new_with_defaults(),
            entries: Mutex::default(),
            dl: Mutex::default(),
            vendor: Mutex::default(),
            git_index: Mutex::default(),
        }
    }

    pub fn set_vendor(&self, source: impl Into<String>, dir: PathBuf) {
        self.vendor.lock().unwrap().insert(source.into(), dir);
    }

    fn index_base(source: &str) -> Result<String> {
        if source.trim_end_matches('/') == CRATES_IO {
            return Ok("https://index.crates.io/".into());
        }
        match source.strip_prefix("sparse+") {
            Some(url) => Ok(if url.ends_with('/') { url.to_owned() } else { format!("{url}/") }),
            None => bail!("registry `{source}` has no sparse index"),
        }
    }

    /// `registry+<git url>`, except crates.io, which stays sparse.
    fn git_index_url(source: &str) -> Option<String> {
        let source = source.trim_end_matches('/');
        if source == CRATES_IO || source.starts_with("sparse+") {
            return None;
        }
        source.strip_prefix("registry+").filter(|u| !u.is_empty()).map(str::to_owned)
    }

    fn git_checkout(&self, source: &str) -> Result<PathBuf> {
        let mut guard = self.git_index.lock().unwrap();
        if let Some(dir) = guard.get(source) {
            return Ok(dir.clone());
        }
        let url = Self::git_index_url(source).with_context(|| format!("registry `{source}` is not a git index"))?;
        let home = self.root.parent().unwrap_or(&self.root);
        let git = crate::git::Git::new(home.join("git"), self.offline);
        let commit = git.resolve(&url, &crate::manifest::GitRef::DefaultBranch)?;
        let dir = git.checkout(&url, &commit)?;
        guard.insert(source.to_owned(), dir.clone());
        Ok(dir)
    }

    fn cached_get(&self, source: &str, rel: &str) -> Result<Option<Vec<u8>>> {
        if Self::git_index_url(source).is_some() {
            let path = self.git_checkout(source)?.join(rel);
            return Ok(path.is_file().then(|| std::fs::read(&path)).transpose()?);
        }
        let file = self.root.join("index").join(source_dir_name(source)).join(rel);
        let etag_file = file.with_extension("etag");
        let cached = std::fs::read(&file).ok();
        if self.offline {
            return Ok(cached);
        }
        let url = format!("{}{rel}", Self::index_base(source)?);
        let mut req = self.agent.get(&url);
        if cached.is_some()
            && let Ok(etag) = std::fs::read_to_string(&etag_file)
        {
            req = req.header("If-None-Match", etag.trim());
        }
        let resp = match req.config().http_status_as_error(false).build().call() {
            Ok(r) => r,
            Err(e) if cached.is_some() => {
                let _ = e;
                return Ok(cached);
            }
            Err(e) => return Err(e).with_context(|| format!("failed to fetch {url}")),
        };
        match resp.status().as_u16() {
            304 => Ok(cached),
            200 => {
                let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_owned);
                let mut body = Vec::new();
                resp.into_body().into_reader().read_to_end(&mut body)?;
                std::fs::create_dir_all(file.parent().unwrap())?;
                let tmp = file.with_extension(format!("tmp{}", std::process::id()));
                std::fs::write(&tmp, &body)?;
                std::fs::rename(&tmp, &file)?;
                match etag {
                    Some(e) => std::fs::write(&etag_file, e)?,
                    None => {
                        let _ = std::fs::remove_file(&etag_file);
                    }
                }
                Ok(Some(body))
            }
            404 | 410 | 451 => Ok(None),
            s => bail!("fetching {url} failed with HTTP {s}"),
        }
    }

    pub fn entries(&self, source: &str, name: &str) -> Result<Arc<Vec<IndexEntry>>> {
        let key = (source.to_owned(), name.to_lowercase());
        if let Some(e) = self.entries.lock().unwrap().get(&key) {
            return Ok(e.clone());
        }
        let body = self.cached_get(source, &index_path(name))?.with_context(|| {
            if self.offline {
                format!("no cached index entry for `{name}` while offline")
            } else {
                format!("no crate named `{name}` in registry {source}")
            }
        })?;
        let text = String::from_utf8_lossy(&body);
        let parsed: Vec<IndexEntry> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| parse_entry(l, source).ok())
            .collect();
        let parsed = Arc::new(parsed);
        self.entries.lock().unwrap().insert(key, parsed.clone());
        Ok(parsed)
    }

    /// Errors show up later, on `entries`.
    pub fn prefetch(&self, items: &[(String, String)]) {
        let todo: Vec<&(String, String)> = {
            let have = self.entries.lock().unwrap();
            items
                .iter()
                .filter(|(s, n)| !have.contains_key(&(s.clone(), n.to_lowercase())))
                .collect()
        };
        parallel(&todo, 16, |(s, n)| {
            let _ = self.entries(s, n);
        });
    }

    fn dl_template(&self, source: &str) -> Result<String> {
        if let Some(t) = self.dl.lock().unwrap().get(source) {
            return Ok(t.clone());
        }
        let body = self.cached_get(source, "config.json")?.context("registry has no config.json")?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        let dl = v["dl"].as_str().context("registry config.json has no `dl`")?.to_owned();
        self.dl.lock().unwrap().insert(source.to_owned(), dl.clone());
        Ok(dl)
    }

    pub fn src_dir(&self, source: &str, name: &str, version: &Version) -> PathBuf {
        self.root
            .join("src")
            .join(source_dir_name(source))
            .join(format!("{name}-{version}"))
    }

    fn mirror_copy(&self, name: &str, version: &Version, checksum: &str, dest: &Path) -> bool {
        let Some(mirror) = &self.mirror else { return false };
        let Ok(rd) = std::fs::read_dir(mirror) else { return false };
        for d in rd.filter_map(|e| e.ok()) {
            let candidate = d.path().join(format!("{name}-{version}.crate"));
            if candidate.is_file()
                && rb_toolchain::download::sha256_file(&candidate).is_ok_and(|h| h.eq_ignore_ascii_case(checksum))
                && std::fs::create_dir_all(dest.parent().unwrap()).is_ok()
                && rb_store::link::place(&candidate, dest, rb_store::LinkMode::Reflink, false).is_ok()
            {
                return true;
            }
        }
        false
    }

    /// Extract `name@version` if it isn't already. Returns the package root.
    pub fn ensure(&self, d: &Download<'_>) -> Result<(PathBuf, bool)> {
        if let Some(root) = self.vendor.lock().unwrap().get(d.source).cloned() {
            let pkg = root.join(format!("{}-{}", d.name, d.version));
            if !pkg.join("Cargo.toml").is_file() {
                bail!("vendor directory {} has no {}-{}", root.display(), d.name, d.version);
            }
            if let Some(want) = d.checksum {
                let ck = pkg.join(".cargo-checksum.json");
                if ck.is_file() {
                    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&ck)?)?;
                    if let Some(got) = value.get("package").and_then(|p| p.as_str())
                        && !got.eq_ignore_ascii_case(want)
                    {
                        bail!("checksum mismatch for `{}-{}` in {}", d.name, d.version, root.display());
                    }
                }
            }
            return Ok((pkg, false));
        }
        let dir = self.src_dir(d.source, d.name, d.version);
        if dir.join(".rb-ok").is_file() {
            return Ok((dir, false));
        }
        let archive = self
            .root
            .join("cache")
            .join(source_dir_name(d.source))
            .join(format!("{}-{}.crate", d.name, d.version));
        let mut downloaded = false;
        if !archive.is_file() {
            let reused = d.checksum.is_some_and(|c| self.mirror_copy(d.name, d.version, c, &archive));
            if !reused {
                if self.offline {
                    bail!("`{}@{}` is not downloaded and rb is offline", d.name, d.version);
                }
                let tpl = self.dl_template(d.source)?;
                let url = if ["{crate}", "{version}", "{prefix}", "{lowerprefix}", "{sha256-checksum}"]
                    .iter()
                    .any(|m| tpl.contains(m))
                {
                    let prefix = index_path(d.name);
                    let prefix = prefix.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                    tpl.replace("{crate}", d.name)
                        .replace("{version}", &d.version.to_string())
                        .replace("{prefix}", prefix)
                        .replace("{lowerprefix}", &prefix.to_lowercase())
                        .replace("{sha256-checksum}", d.checksum.unwrap_or(""))
                } else {
                    format!("{}/{}/{}/download", tpl.trim_end_matches('/'), d.name, d.version)
                };
                rb_toolchain::download::fetch_verified(&self.agent, &url, &archive, d.checksum)?;
                downloaded = true;
            }
        } else if let Some(c) = d.checksum {
            let got = rb_toolchain::download::sha256_file(&archive)?;
            if !got.eq_ignore_ascii_case(c) {
                let _ = std::fs::remove_file(&archive);
                bail!("cached {} has checksum {got}, expected {c}", archive.display());
            }
        }
        let parent = dir.parent().unwrap();
        std::fs::create_dir_all(parent)?;
        let staging = parent.join(format!(".staging-{}-{}-{}", d.name, d.version, std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let file = std::fs::File::open(&archive)?;
        tar::Archive::new(flate2::read::GzDecoder::new(file))
            .unpack(&staging)
            .with_context(|| format!("failed to unpack {}", archive.display()))?;
        let top = staging.join(format!("{}-{}", d.name, d.version));
        if !top.join("Cargo.toml").is_file() {
            bail!("{} does not contain {}-{}/Cargo.toml", archive.display(), d.name, d.version);
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&top, &dir)?;
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::write(dir.join(".rb-ok"), "")?;
        Ok((dir, downloaded))
    }
}

pub fn parallel<T: Sync>(items: &[T], threads: usize, f: impl Fn(&T) + Sync) {
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.min(items.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    match items.get(i) {
                        Some(item) => f(item),
                        None => break,
                    }
                }
            });
        }
    });
}

pub struct SearchHit {
    pub name: String,
    pub version: String,
    pub description: String,
}

pub fn parse_search(body: &str) -> Result<Vec<SearchHit>> {
    let value: serde_json::Value = serde_json::from_str(body).context("invalid crates.io search response")?;
    let crates = value
        .get("crates")
        .and_then(|c| c.as_array())
        .context("search response has no crates")?;
    Ok(crates
        .iter()
        .filter_map(|c| {
            Some(SearchHit {
                name: c.get("name")?.as_str()?.to_owned(),
                version: c.get("max_version")?.as_str()?.to_owned(),
                description: c.get("description").and_then(|d| d.as_str()).unwrap_or("").replace('\n', " "),
            })
        })
        .collect())
}

/// crates.io allows 100 per page. `limit` is clamped to that.
pub fn search_crates(query: &str, limit: u32) -> Result<Vec<SearchHit>> {
    let limit = limit.clamp(1, 100);
    let url = format!("https://crates.io/api/v1/crates?q={}&per_page={limit}", urlencoding_query(query));
    let agent = ureq::Agent::new_with_defaults();
    let response = agent
        .get(&url)
        .header("User-Agent", "rb (cargo-compatible search)")
        .call()
        .with_context(|| format!("failed to search crates.io for `{query}`"))?;
    let mut body = String::new();
    response
        .into_body()
        .into_reader()
        .read_to_string(&mut body)
        .context("failed to read crates.io search response")?;
    parse_search(&body)
}

fn urlencoding_query(query: &str) -> String {
    let mut out = String::new();
    for (i, part) in query.split_whitespace().enumerate() {
        if i > 0 {
            out.push('+');
        }
        for b in part.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    out
}

/// Highest non-yanked version. `1.2.3` is exact, `1.2` is a requirement.
pub fn select_version<'a>(entries: &'a [IndexEntry], spec: Option<&str>) -> Result<&'a IndexEntry> {
    let mut candidates: Vec<&IndexEntry> = entries.iter().filter(|e| !e.yanked).collect();
    if candidates.is_empty() {
        bail!("no available versions");
    }
    if let Some(spec) = spec {
        if let Ok(exact) = Version::parse(spec) {
            return candidates
                .into_iter()
                .find(|e| e.vers == exact)
                .with_context(|| format!("version `{spec}` was not found"));
        }
        let req = VersionReq::parse(spec).with_context(|| format!("invalid version requirement `{spec}`"))?;
        candidates.retain(|e| req.matches(&e.vers));
    }
    candidates
        .into_iter()
        .max_by(|a, b| a.vers.cmp(&b.vers))
        .context("no matching version")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_paths() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("syn"), "3/s/syn");
        assert_eq!(index_path("Serde"), "se/rd/serde");
    }

    #[test]
    fn parses_index_line() {
        let line = r#"{"name":"demo","vers":"1.2.0","deps":[{"name":"s","req":"^1","features":[],"optional":true,"default_features":true,"target":"cfg(unix)","kind":"normal","package":"serde"}],"cksum":"ab","features":{"std":[]},"features2":{"fast":["dep:s"]},"yanked":false,"rust_version":"1.70"}"#;
        let e = parse_entry(line, CRATES_IO).unwrap();
        assert_eq!(e.deps[0].name, "serde");
        assert_eq!(e.deps[0].dep_name(), "s");
        assert!(e.features.contains_key("fast") && !e.features.contains_key("s"));
        assert_eq!(e.rust_version, Some(Version::new(1, 70, 0)));
    }

    #[test]
    fn parses_search_results() {
        let body = r#"{"crates":[{"name":"serde","max_version":"1.0.210","description":"A serialization framework"}]}"#;
        let hits = parse_search(body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "serde");
        assert_eq!(hits[0].version, "1.0.210");
    }

    #[test]
    fn select_version_picks_latest_or_exact() {
        let entries = vec![
            parse_entry(
                r#"{"name":"demo","vers":"1.0.0","deps":[],"cksum":"a","features":{},"yanked":false}"#,
                CRATES_IO,
            )
            .unwrap(),
            parse_entry(
                r#"{"name":"demo","vers":"1.2.0","deps":[],"cksum":"b","features":{},"yanked":false}"#,
                CRATES_IO,
            )
            .unwrap(),
            parse_entry(
                r#"{"name":"demo","vers":"2.0.0","deps":[],"cksum":"c","features":{},"yanked":true}"#,
                CRATES_IO,
            )
            .unwrap(),
        ];
        assert_eq!(select_version(&entries, None).unwrap().vers, Version::new(1, 2, 0));
        assert_eq!(select_version(&entries, Some("1.0.0")).unwrap().vers, Version::new(1, 0, 0));
        assert_eq!(select_version(&entries, Some("^1")).unwrap().vers, Version::new(1, 2, 0));
    }

    #[test]
    fn git_index_resolves_and_unpacks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let index = root.join("index");
        let dl = root.join("dl");
        std::fs::create_dir_all(index.join("de/mo")).unwrap();
        std::fs::create_dir_all(&dl).unwrap();
        let crate_path = dl.join("demo-0.1.0.crate");
        let mut ar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
        let toml = b"[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let lib = b"pub fn hi() -> u8 { 7 }\n";
        for (name, bytes) in [("demo-0.1.0/Cargo.toml", &toml[..]), ("demo-0.1.0/src/lib.rs", &lib[..])] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            ar.append_data(&mut header, name, bytes).unwrap();
        }
        let bytes = ar.into_inner().unwrap().finish().unwrap();
        std::fs::write(&crate_path, &bytes).unwrap();
        let cksum = rb_toolchain::download::sha256_file(&crate_path).unwrap();
        let line =
            format!("{{\"name\":\"demo\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{cksum}\",\"features\":{{}},\"yanked\":false}}\n");
        std::fs::write(index.join("de/mo/demo"), line).unwrap();
        let dl_url = format!("file://{}/{{crate}}-{{version}}.crate", dl.display());
        std::fs::write(
            index.join("config.json"),
            format!("{{\"dl\":{}}}", serde_json::to_string(&dl_url).unwrap()),
        )
        .unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&index)
                .env("GIT_AUTHOR_NAME", "rb")
                .env("GIT_AUTHOR_EMAIL", "rb@example.com")
                .env("GIT_COMMITTER_NAME", "rb")
                .env("GIT_COMMITTER_EMAIL", "rb@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}\n{}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["add", "."]);
        git(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", "index"]);
        let source = format!("registry+file://{}", index.display());
        let registry = Registry::new(root.join("registry"), false, None);
        let entries = registry.entries(&source, "demo").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].vers, Version::new(0, 1, 0));
        let (dir, downloaded) = registry
            .ensure(&Download {
                source: &source,
                name: "demo",
                version: &entries[0].vers,
                checksum: Some(&entries[0].cksum),
            })
            .unwrap();
        assert!(downloaded);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "pub fn hi() -> u8 { 7 }\n"
        );
    }
}
