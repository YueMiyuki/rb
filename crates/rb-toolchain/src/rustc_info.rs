use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// A resolved rustc toolchain
#[derive(Debug)]
pub struct Rustc {
    /// The real `rustc`, not the rustup proxy. The proxy costs about 10ms a call.
    pub path: PathBuf,
    pub verbose_version: String,
    pub release: String,
    pub commit_hash: String,
    pub host: String,
    pub sysroot: PathBuf,
    pub nightly: bool,
    id: String,
    cache_dir: PathBuf,
    infos: Mutex<HashMap<InfoKey, Arc<TargetInfo>>>,
}

/// `(--target, rustflags)`
type InfoKey = (Option<String>, Vec<String>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileType {
    pub prefix: String,
    pub suffix: String,
}

/// `--print cfg`, and how rustc names each crate type's output.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetInfo {
    pub triple: String,
    pub cfg: Vec<(String, Option<String>)>,
    pub file_types: BTreeMap<String, FileType>,
}

impl TargetInfo {
    pub fn cfg_value(&self, name: &str) -> Option<&str> {
        self.cfg.iter().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref())
    }

    pub fn cfg_values(&self, name: &str) -> Vec<&str> {
        self.cfg
            .iter()
            .filter(|(k, _)| k == name)
            .filter_map(|(_, v)| v.as_deref())
            .collect()
    }

    pub fn has_cfg(&self, name: &str) -> bool {
        self.cfg.iter().any(|(k, v)| k == name && v.is_none())
    }

    pub fn is_apple(&self) -> bool {
        self.cfg_value("target_vendor") == Some("apple")
    }

    pub fn is_windows(&self) -> bool {
        self.cfg_value("target_os") == Some("windows")
    }

    pub fn is_msvc(&self) -> bool {
        self.is_windows() && self.cfg_value("target_env") == Some("msvc")
    }

    pub fn file_type(&self, crate_type: &str) -> Option<&FileType> {
        let key = if crate_type == "lib" { "rlib" } else { crate_type };
        self.file_types.get(key)
    }
}

const CRATE_TYPES: [&str; 6] = ["bin", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8(out.stdout)?)
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

#[derive(Serialize, Deserialize)]
struct Detected {
    vv: String,
    sysroot: PathBuf,
    real_mtime: Option<u128>,
}

fn mtime_ns(p: &Path) -> Option<u128> {
    std::fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos())
}

/// Everything rustup consults to pick a toolchain for `cwd`
fn selection_fingerprint(program: &Path, cwd: &Path) -> String {
    let mut h = blake3::Hasher::new();
    h.update(program.as_os_str().as_encoded_bytes());
    for var in ["RUSTUP_TOOLCHAIN", "RUSTUP_HOME", "PATH"] {
        h.update(format!("\0{var}={:?}", std::env::var_os(var)).as_bytes());
    }
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        for name in ["rust-toolchain", "rust-toolchain.toml"] {
            if let Ok(c) = std::fs::read(d.join(name)) {
                h.update(d.as_os_str().as_encoded_bytes());
                h.update(&c);
            }
        }
        dir = d.parent();
    }
    let rustup_home = std::env::var_os("RUSTUP_HOME").map(PathBuf::from).or_else(|| {
        #[allow(deprecated)]
        std::env::home_dir().map(|h| h.join(".rustup"))
    });
    if let Some(settings) = rustup_home.and_then(|h| std::fs::read(h.join("settings.toml")).ok()) {
        h.update(&settings);
    }
    h.finalize().to_hex()[..24].to_owned()
}

impl Rustc {
    /// An explicit `RUSTC`, or `rustc` from `PATH` in `cwd` so `rust-toolchain.toml` applies. Cached, rechecked against the binary's mtime.
    pub fn detect(program: Option<&Path>, cwd: &Path, cache_dir: &Path) -> Result<Self> {
        let explicit = program.is_some();
        let program = program.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("rustc"));
        let cache_path = cache_dir
            .join("rustc")
            .join(format!("{}.json", selection_fingerprint(&program, cwd)));
        let cached: Option<Detected> = std::fs::read(&cache_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let detected = match cached {
            Some(d) if d.real_mtime.is_some() && d.real_mtime == mtime_ns(&d.sysroot.join("bin").join(exe("rustc"))) => d,
            _ => {
                let vv = run(Command::new(&program).arg("-vV").current_dir(cwd))?;
                let sysroot = PathBuf::from(run(Command::new(&program).args(["--print", "sysroot"]).current_dir(cwd))?.trim());
                let d = Detected {
                    real_mtime: mtime_ns(&sysroot.join("bin").join(exe("rustc"))),
                    vv,
                    sysroot,
                };
                let _ = std::fs::create_dir_all(cache_path.parent().unwrap());
                let _ = std::fs::write(&cache_path, serde_json::to_vec(&d)?);
                d
            }
        };
        let Detected { vv, sysroot, .. } = detected;
        let field = |name: &str| {
            vv.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}: ")))
                .map(str::to_owned)
                .unwrap_or_default()
        };
        let host = field("host");
        if host.is_empty() {
            bail!("unexpected `rustc -vV` output:\n{vv}");
        }
        let real = sysroot.join("bin").join(exe("rustc"));
        let path = if !explicit && real.is_file() { real } else { program };
        let release = field("release");
        Ok(Self {
            id: blake3::hash(vv.as_bytes()).to_hex()[..16].to_owned(),
            nightly: release.contains("nightly") || release.contains("-dev"),
            commit_hash: field("commit-hash"),
            release,
            host,
            sysroot,
            path,
            verbose_version: vv,
            cache_dir: cache_dir.join("target-info"),
            infos: Mutex::default(),
        })
    }

    /// Short stable identifier of this exact compiler build
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn target_libdir(&self, triple: &str) -> PathBuf {
        self.sysroot.join("lib/rustlib").join(triple).join("lib")
    }

    /// Whether the standard library for `triple` is installed
    pub fn has_std(&self, triple: &str) -> bool {
        std::fs::read_dir(self.target_libdir(triple))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().starts_with("libstd-"))
            })
            .unwrap_or(false)
    }

    /// `lib/rustlib/<host>/bin`: rust-lld, gcc-ld shims, rust-objcopy
    pub fn host_tools_dir(&self) -> PathBuf {
        self.sysroot.join("lib/rustlib").join(&self.host).join("bin")
    }

    /// The `lld-link` flavor shim shipped with every rustup toolchain
    pub fn lld_link(&self) -> PathBuf {
        self.host_tools_dir().join("gcc-ld").join(exe("lld-link"))
    }

    pub fn has_codegen_backend(&self, name: &str) -> bool {
        let dir = self.sysroot.join("lib/rustlib").join(&self.host).join("codegen-backends");
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().contains(&format!("rustc_codegen_{name}")))
            })
            .unwrap_or(false)
    }

    /// `None` is the host, with no `--target`. Cached in memory and on disk.
    pub fn target_info(&self, triple: Option<&str>, rustflags: &[String]) -> Result<Arc<TargetInfo>> {
        let mem_key = (triple.map(str::to_owned), rustflags.to_vec());
        if let Some(info) = self.infos.lock().unwrap().get(&mem_key) {
            return Ok(info.clone());
        }
        let disk_key = {
            let mut h = blake3::Hasher::new();
            h.update(self.id.as_bytes());
            h.update(triple.unwrap_or("<host>").as_bytes());
            for f in rustflags {
                h.update(b"\0");
                h.update(f.as_bytes());
            }
            h.finalize().to_hex()[..24].to_owned()
        };
        let disk_path = self.cache_dir.join(format!("{disk_key}.json"));
        let info = match std::fs::read(&disk_path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
            Some(info) => info,
            None => {
                let info = self.query_target_info(triple, rustflags)?;
                let _ = std::fs::create_dir_all(&self.cache_dir);
                let _ = std::fs::write(&disk_path, serde_json::to_vec(&info)?);
                info
            }
        };
        let info = Arc::new(info);
        self.infos.lock().unwrap().insert(mem_key, info.clone());
        Ok(info)
    }

    fn query_target_info(&self, triple: Option<&str>, rustflags: &[String]) -> Result<TargetInfo> {
        let mut cmd = Command::new(&self.path);
        cmd.args(["-", "--crate-name", "___", "--print=file-names", "--print=cfg"]);
        for ct in CRATE_TYPES {
            cmd.args(["--crate-type", ct]);
        }
        if let Some(t) = triple {
            cmd.args(["--target", t]);
        }
        cmd.args(rustflags);
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run {cmd:?}"))?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            let hint = match triple {
                Some(t) if stderr.contains("can't find crate for `core`") || stderr.contains("could not find specification") => {
                    format!("\nhint: run `rb target add {t}`")
                }
                _ => String::new(),
            };
            bail!("failed to query target info for {}:\n{stderr}{hint}", triple.unwrap_or(&self.host));
        }
        let stdout = String::from_utf8(out.stdout)?;
        let supported: Vec<&str> = CRATE_TYPES
            .iter()
            .copied()
            .filter(|ct| !stderr.contains(&format!("unsupported crate type `{ct}`")))
            .collect();
        let mut lines = stdout.lines();
        let mut file_types = BTreeMap::new();
        for ct in &supported {
            let name = lines.next().context("truncated --print=file-names output")?;
            let (prefix, suffix) = name.split_once("___").context("unexpected file name output")?;
            file_types.insert(
                (*ct).to_owned(),
                FileType {
                    prefix: prefix.to_owned(),
                    suffix: suffix.to_owned(),
                },
            );
        }
        let cfg = lines
            .map(|l| match l.split_once('=') {
                Some((k, v)) => (k.to_owned(), Some(v.trim_matches('"').to_owned())),
                None => (l.to_owned(), None),
            })
            .collect();
        Ok(TargetInfo {
            triple: triple.unwrap_or(&self.host).to_owned(),
            cfg,
            file_types,
        })
    }
}
