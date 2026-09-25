use crate::rustc_info::Rustc;
use crate::{sh_quote, write_script, zig};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What the user typed. A glibc suffix like `.2.17` goes to zig and is stripped for rustc.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TargetRequest {
    /// Config key, `TARGET`, and the directory under `target/`. A JSON spec uses the file stem, same as cargo.
    pub triple: String,
    pub glibc: Option<String>,
    /// Custom target JSON. rustc gets this path as `--target`.
    pub spec_path: Option<PathBuf>,
}

impl TargetRequest {
    pub fn parse(s: &str) -> Self {
        if let Some((triple, ver)) = s.split_once('.')
            && triple.contains("-linux-gnu")
            && ver.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            return Self {
                triple: triple.to_owned(),
                glibc: Some(ver.to_owned()),
                spec_path: None,
            };
        }
        Self {
            triple: s.to_owned(),
            glibc: None,
            spec_path: None,
        }
    }

    /// A builtin triple, or a `.json` target spec resolved against `cwd`.
    pub fn from_arg(s: &str, cwd: &Path) -> Result<Self> {
        if !s.ends_with(".json") {
            return Ok(Self::parse(s));
        }
        let path = if Path::new(s).is_absolute() {
            PathBuf::from(s)
        } else {
            cwd.join(s)
        };
        if !path.is_file() {
            bail!("target path `{s}` is not a valid file\n\nCaused by:\n  No such file or directory (os error 2)");
        }
        let path = path
            .canonicalize()
            .with_context(|| format!("target path `{s}` is not a valid file"))?;
        let stem = path
            .file_stem()
            .and_then(|n| n.to_str())
            .with_context(|| format!("target path `{s}` is not a valid file"))?
            .to_owned();
        Ok(Self {
            triple: stem,
            glibc: None,
            spec_path: Some(path),
        })
    }

    /// What rustc's `--target` should be. JSON specs stay an absolute path. Cargo canonicalizes them too.
    pub fn rustc_target(&self) -> String {
        match &self.spec_path {
            Some(p) => p.display().to_string(),
            None => self.triple.clone(),
        }
    }

    /// What zig or xwin should target. JSON specs prefer `llvm-target`.
    pub fn toolchain_triple(&self) -> Result<String> {
        let Some(path) = &self.spec_path else {
            return Ok(self.triple.clone());
        };
        let text = std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
        let spec: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("target spec `{}` is not valid JSON", path.display()))?;
        if let Some(t) = spec.get("llvm-target").and_then(|v| v.as_str()).filter(|t| !t.is_empty()) {
            return Ok(t.to_owned());
        }
        let part = |k: &str| spec.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let arch = part("arch").unwrap_or("unknown");
        let vendor = part("vendor").unwrap_or("unknown");
        let os = part("os").unwrap_or("none");
        Ok(match part("env") {
            Some(env) => format!("{arch}-{vendor}-{os}-{env}"),
            None => format!("{arch}-{vendor}-{os}"),
        })
    }

    pub fn display(&self) -> String {
        match &self.glibc {
            Some(v) => format!("{}.{v}", self.triple),
            None => self.triple.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Same target, Apple-to-Apple, or wasm with rust-lld.
    Native,
    /// zig cc as C compiler and linker
    Zig,
    /// xwin CRT/SDK + rustup's lld-link + clang-cl
    Msvc,
}

impl Strategy {
    pub fn for_target(host: &str, target: &str) -> Result<Self> {
        let os = |t: &str| {
            if t.contains("-apple-") {
                "apple"
            } else if t.contains("-linux-") {
                "linux"
            } else if t.contains("-windows-") {
                "windows"
            } else {
                "other"
            }
        };
        if host == target || (os(host) == "apple" && os(target) == "apple") || target.starts_with("wasm32-") {
            return Ok(Self::Native);
        }
        if target.ends_with("-windows-msvc") {
            return Ok(Self::Msvc);
        }
        if target.contains("-linux-") || target.contains("-windows-gnu") {
            return Ok(Self::Zig);
        }
        bail!("cross-compiling from {host} to {target} is not supported yet; configure `target.{target}.linker` manually")
    }
}

/// Everything needed to compile and link for one target
#[derive(Clone, Debug, Default)]
pub struct CrossToolchain {
    pub linker: Option<PathBuf>,
    pub rustflags: Vec<String>,
    /// Environment for build scripts (`CC_<triple>`, `AR_<triple>`, ...)
    pub env: Vec<(String, String)>,
    /// Prepended to `PATH` for rustc (e.g. `dlltool` for windows-gnu raw-dylib)
    pub path_prepend: Vec<PathBuf>,
    /// Extra environment for rustc (inherited by the linker it spawns)
    pub rustc_env: Vec<(String, String)>,
    /// Goes in the cache key of every unit built for this target.
    pub fingerprint: String,
    pub description: String,
}

/// Toolchain provisioning root (`~/.rb/toolchains`)
#[derive(Clone, Debug)]
pub struct Toolchains {
    pub home: PathBuf,
    /// Path of the running `rb`, which wrappers call back into
    pub rb_exe: PathBuf,
    pub zig_version: String,
    pub accept_msvc_license: bool,
}

pub(crate) fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn find_llvm_rc() -> Option<PathBuf> {
    if let Some(p) = find_in_path("llvm-rc") {
        return Some(p);
    }
    let brew = PathBuf::from("/opt/homebrew/opt/llvm/bin/llvm-rc");
    brew.is_file().then_some(brew)
}

/// `rustup target add <triple>` for the toolchain active in `cwd`
pub fn rustup_add_target(triple: &str, cwd: &Path) -> Result<()> {
    let status = Command::new("rustup")
        .args(["target", "add", triple])
        .current_dir(cwd)
        .status()
        .context("failed to run rustup; install the standard library for the target manually")?;
    if !status.success() {
        bail!("`rustup target add {triple}` failed");
    }
    Ok(())
}

fn env_suffix(triple: &str) -> String {
    triple.replace(['-', '.'], "_")
}

impl Toolchains {
    fn wrappers_root(&self) -> PathBuf {
        self.home.join("wrappers")
    }

    pub fn setup(&self, rustc: &Rustc, req: &TargetRequest, provision: bool) -> Result<CrossToolchain> {
        let tool = req.toolchain_triple()?;
        let strategy = Strategy::for_target(&rustc.host, &tool)?;
        let u = env_suffix(&req.triple);
        match strategy {
            Strategy::Native => Ok(CrossToolchain {
                description: "native".into(),
                fingerprint: "native".into(),
                ..Default::default()
            }),
            Strategy::Zig => {
                let triple = tool.as_str();
                let zig = if provision {
                    zig::find_or_install(&self.home, &self.zig_version)?
                } else {
                    zig::find(&self.home, &self.zig_version)
                        .with_context(|| format!("zig is not installed; run `rb target add {}`", req.display()))?
                };
                let w = zig::write_wrappers(&self.wrappers_root(), &self.rb_exe, &zig, triple, req.glibc.as_deref())?;
                let mut env = vec![
                    (format!("CC_{u}"), w.cc.display().to_string()),
                    (format!("CXX_{u}"), w.cxx.display().to_string()),
                    (format!("AR_{u}"), w.ar.display().to_string()),
                    (format!("RANLIB_{u}"), w.ranlib.display().to_string()),
                    (format!("CMAKE_TOOLCHAIN_FILE_{u}"), w.cmake_toolchain.display().to_string()),
                ];
                let path_prepend = vec![w.bin_dir.clone()];
                if triple.contains("-windows-") {
                    env.push(("WINAPI_NO_BUNDLED_LIBRARIES".into(), "1".into()));
                }
                if triple.contains("-linux-") {
                    // Host .pc files describe macOS frameworks. Point pkg-config at the target sysroot, and let it run during a cross build.
                    let pc = self.home.join("pkgconfig").join(triple);
                    let _ = std::fs::create_dir_all(pc.join("lib/pkgconfig"));
                    // Debian puts .pc files under usr/lib/<multiarch>/pkgconfig.
                    let multiarch = triple.replace("-unknown", "").replace("-pc", "");
                    let mut dirs: Vec<String> = [
                        pc.join(format!("usr/lib/{multiarch}/pkgconfig")),
                        pc.join("usr/lib/pkgconfig"),
                        pc.join("usr/share/pkgconfig"),
                        pc.join("lib/pkgconfig"),
                    ]
                    .into_iter()
                    .filter(|d| d.is_dir())
                    .map(|d| d.display().to_string())
                    .collect();
                    if dirs.is_empty() {
                        let fallback = pc.join("lib/pkgconfig");
                        let _ = std::fs::create_dir_all(&fallback);
                        dirs.push(fallback.display().to_string());
                    }
                    let libdir = dirs.join(":");
                    env.push(("PKG_CONFIG_ALLOW_CROSS".into(), "1".into()));
                    // Don't mix in Homebrew .pc files from the host.
                    env.push(("PKG_CONFIG_PATH".into(), "".into()));
                    env.push(("PKG_CONFIG_LIBDIR".into(), libdir));
                    env.push(("PKG_CONFIG_SYSROOT_DIR".into(), pc.display().to_string()));
                }
                let (zt, _) = zig::zig_target(triple, req.glibc.as_deref())?;
                let (a, b, c) = zig.version;
                Ok(CrossToolchain {
                    linker: Some(w.cc.clone()),
                    rustflags: Vec::new(),
                    env,
                    path_prepend,
                    rustc_env: Vec::new(),
                    fingerprint: format!(
                        "zig {a}.{b}.{c} {zt} wrappers v{} rb {}",
                        zig::WRAPPER_VERSION,
                        env!("CARGO_PKG_VERSION")
                    ),
                    description: format!("zig {a}.{b}.{c} ({zt})"),
                })
            }
            Strategy::Msvc => self.setup_msvc(rustc, req, &tool, provision),
        }
    }

    #[cfg(feature = "msvc")]
    fn setup_msvc(&self, rustc: &Rustc, req: &TargetRequest, tool: &str, provision: bool) -> Result<CrossToolchain> {
        let triple = tool;
        let u = env_suffix(&req.triple);
        let lld_link = rustc.lld_link();
        if !lld_link.is_file() {
            bail!("{} not found; the active rustc does not ship rust-lld", lld_link.display());
        }
        let arch = crate::xwin::arch_dir(triple)?;
        let splat = if provision {
            crate::xwin::ensure(&self.home, &[arch], self.accept_msvc_license)?
        } else {
            crate::xwin::find(&self.home, arch)
                .with_context(|| format!("the MSVC CRT/SDK is not installed; run `rb target add {triple} --accept-license`"))?
        };
        let dir = self.wrappers_root().join(format!("msvc-{triple}"));
        let clang = find_in_path("clang").context("clang is required to compile C code for MSVC targets")?;
        let q = |p: &Path| sh_quote(&p.to_string_lossy());
        let includes: Vec<String> = ["crt/include", "sdk/include/ucrt", "sdk/include/um", "sdk/include/shared"]
            .iter()
            .map(|sub| sh_quote(&format!("/imsvc{}", splat.join(sub).display())))
            .collect();
        write_script(
            &dir.join("clang-cl"),
            &format!(
                "exec {} --driver-mode=cl --target={triple} -Wno-unused-command-line-argument -fuse-ld=lld-link {} \"$@\"",
                q(&clang),
                includes.join(" ")
            ),
        )?;
        // Some nightlies ship rust-lld linked against libLLVM.dylib, and the rpath can't find it.
        let lib_dir = rustc.sysroot.join("lib");
        let dyld = std::env::var("DYLD_FALLBACK_LIBRARY_PATH")
            .map(|v| format!("{}:{v}", lib_dir.display()))
            .unwrap_or_else(|_| lib_dir.display().to_string());
        let rustc_env = if cfg!(target_os = "macos") {
            vec![("DYLD_FALLBACK_LIBRARY_PATH".to_owned(), dyld.clone())]
        } else {
            Vec::new()
        };
        let env_prefix = rustc_env.iter().map(|(k, v)| format!("{k}={} ", sh_quote(v))).collect::<String>();
        write_script(&dir.join("llvm-lib"), &format!("{env_prefix}exec {} /lib \"$@\"", q(&lld_link)))?;
        // lld-link wants `mt.exe` for `/MANIFEST:EMBED` when it was built without libxml2.
        write_script(&dir.join("mt.exe"), &format!("exec {} __mt \"$@\"", q(&self.rb_exe)))?;
        if let Some(rc) = find_llvm_rc() {
            let dest = dir.join("llvm-rc");
            let _ = std::fs::remove_file(&dest);
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(&rc, &dest);
        }
        let clang_cl = dir.join("clang-cl").display().to_string();
        let lib = dir.join("llvm-lib").display().to_string();
        let libdirs = [
            format!("crt/lib/{arch}"),
            format!("sdk/lib/um/{arch}"),
            format!("sdk/lib/ucrt/{arch}"),
        ];
        // rustc decides the lld flavor from the `rust-lld` file name and passes `-flavor link`.
        let rust_lld = rustc.host_tools_dir().join(format!("rust-lld{}", std::env::consts::EXE_SUFFIX));
        let mut rustflags: Vec<String> = libdirs.iter().map(|d| format!("-Lnative={}", splat.join(d).display())).collect();
        // xwin's CRT has no PDBs. LNK4099 breaks `/WX` links.
        rustflags.push("-Clink-arg=/ignore:4099".into());
        Ok(CrossToolchain {
            linker: Some(rust_lld),
            rustflags,
            env: vec![
                (format!("CC_{u}"), clang_cl.clone()),
                (format!("CXX_{u}"), clang_cl),
                (format!("AR_{u}"), lib),
            ],
            path_prepend: vec![dir.clone()],
            rustc_env,
            fingerprint: format!(
                "msvc xwin {} {} wrappers v{} rb {}",
                std::fs::read_to_string(splat.join(".rb-arches"))
                    .unwrap_or_default()
                    .replace('\n', ","),
                clang.display(),
                zig::WRAPPER_VERSION,
                env!("CARGO_PKG_VERSION")
            ),
            description: format!("xwin CRT/SDK + lld-link ({arch})"),
        })
    }

    #[cfg(not(feature = "msvc"))]
    fn setup_msvc(&self, _rustc: &Rustc, req: &TargetRequest, _tool: &str, _provision: bool) -> Result<CrossToolchain> {
        bail!("rb was built without the `msvc` feature; cannot target {}", req.triple)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_glibc_suffix() {
        let r = TargetRequest::parse("x86_64-unknown-linux-gnu.2.17");
        assert_eq!(r.triple, "x86_64-unknown-linux-gnu");
        assert_eq!(r.glibc.as_deref(), Some("2.17"));
        assert_eq!(TargetRequest::parse("aarch64-apple-darwin").glibc, None);
        assert!(TargetRequest::parse("aarch64-apple-darwin").spec_path.is_none());
    }

    #[test]
    fn json_spec_uses_file_stem_and_llvm_target() {
        let dir = std::env::temp_dir().join(format!("rb-spec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("custom-target.json");
        std::fs::write(&path, r#"{"llvm-target":"x86_64-unknown-linux-gnu","arch":"x86_64","os":"linux"}"#).unwrap();
        let req = TargetRequest::from_arg("custom-target.json", &dir).unwrap();
        assert_eq!(req.triple, "custom-target");
        assert_eq!(req.rustc_target(), path.canonicalize().unwrap().display().to_string());
        assert_eq!(req.toolchain_triple().unwrap(), "x86_64-unknown-linux-gnu");
        assert_eq!(
            Strategy::for_target("aarch64-apple-darwin", &req.toolchain_triple().unwrap()).unwrap(),
            Strategy::Zig
        );
        let err = TargetRequest::from_arg("missing.json", &dir).unwrap_err().to_string();
        assert!(err.contains("is not a valid file"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picks_strategies() {
        let host = "aarch64-apple-darwin";
        assert_eq!(Strategy::for_target(host, "x86_64-apple-darwin").unwrap(), Strategy::Native);
        assert_eq!(Strategy::for_target(host, "x86_64-unknown-linux-musl").unwrap(), Strategy::Zig);
        assert_eq!(Strategy::for_target(host, "x86_64-pc-windows-gnu").unwrap(), Strategy::Zig);
        assert_eq!(Strategy::for_target(host, "aarch64-pc-windows-msvc").unwrap(), Strategy::Msvc);
    }
}
