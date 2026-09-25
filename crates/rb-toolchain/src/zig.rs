//! Zig as the C compiler and linker for Linux and Windows-GNU. Small `sh` wrappers call back into `rb __zig`.

use crate::{sh_quote, write_script};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_VERSION: &str = "0.16.0";

/// Bump when the wrappers change. It's part of the cross-toolchain fingerprint.
pub const WRAPPER_VERSION: u32 = 5;

#[derive(Clone, Debug)]
pub struct Zig {
    pub path: PathBuf,
    pub version: (u32, u32, u32),
}

fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.trim().split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

fn probe(path: &Path) -> Option<Zig> {
    let out = Command::new(path).arg("version").output().ok()?;
    let version = parse_version(&String::from_utf8_lossy(&out.stdout))?;
    Some(Zig {
        path: path.to_owned(),
        version,
    })
}

fn host_key() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => bail!("no zig download for host arch {other}"),
    };
    let os = match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        other => bail!("no zig download for host OS {other}"),
    };
    Ok(format!("{arch}-{os}"))
}

fn install_dir(home: &Path, version: &str) -> PathBuf {
    home.join("zig").join(version)
}

pub fn find(home: &Path, version: &str) -> Option<Zig> {
    if let Some(p) = std::env::var_os("RB_ZIG") {
        return probe(Path::new(&p));
    }
    let local = install_dir(home, version).join(format!("zig{}", std::env::consts::EXE_SUFFIX));
    if local.is_file() {
        return probe(&local);
    }
    probe(Path::new("zig")).filter(|z| z.version >= (0, 13, 0))
}

pub fn install(home: &Path, version: &str) -> Result<Zig> {
    let index: serde_json::Value = serde_json::from_str(&crate::download::get_string("https://ziglang.org/download/index.json")?)?;
    let entry = &index[version][host_key()?];
    let url = entry["tarball"]
        .as_str()
        .with_context(|| format!("zig {version} is not published for this host"))?;
    let sha = entry["shasum"].as_str();
    let dl = home.join("zig").join(".dl").join(url.rsplit('/').next().unwrap());
    crate::download::download(url, &dl, sha, &format!("zig {version}"))?;

    let staging = home.join("zig").join(format!(".staging-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let file = std::fs::File::open(&dl)?;
    tar::Archive::new(xz2::read::XzDecoder::new(file))
        .unpack(&staging)
        .context("failed to unpack zig")?;
    let top = std::fs::read_dir(&staging)?
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .context("zig archive has no top-level directory")?
        .path();
    let dest = install_dir(home, version);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&top, &dest)?;
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&dl);
    probe(&dest.join(format!("zig{}", std::env::consts::EXE_SUFFIX))).context("installed zig does not run")
}

pub fn find_or_install(home: &Path, version: &str) -> Result<Zig> {
    match find(home, version) {
        Some(z) => Ok(z),
        None => install(home, version),
    }
}

pub fn zig_target(triple: &str, glibc: Option<&str>) -> Result<(String, Option<&'static str>)> {
    let parts: Vec<&str> = triple.split('-').collect();
    let arch = parts[0];
    let env = parts.last().copied().unwrap_or("");
    let (zarch, mcpu) = match arch {
        "x86_64" => ("x86_64", None),
        "aarch64" => ("aarch64", None),
        "i686" => ("x86", Some("pentium4")),
        "i586" => ("x86", Some("pentium")),
        "armv7" => ("arm", Some("generic+v7a+vfp3-d32+thumb2-neon")),
        "arm" if env.ends_with("hf") => ("arm", Some("generic+v6+strict_align+vfp2-d32")),
        "arm" => ("arm", Some("generic+v6+strict_align")),
        "riscv64gc" => ("riscv64", Some("generic_rv64+m+a+f+d+c")),
        "s390x" => ("s390x", Some("z10-vector")),
        "powerpc64le" | "loongarch64" => (arch, None),
        other => bail!("zig cross-compilation is not supported for architecture `{other}`"),
    };
    let target = if triple.contains("-linux-") {
        // Unsuffixed `linux-gnu` means bookworm-era glibc. GTK wants `GLIBC_2.34`. Zig's default is older, and the link fails. `.2.17` still pins.
        let suffix = if env.starts_with("gnu") {
            format!(".{}", glibc.unwrap_or("2.38"))
        } else {
            String::new()
        };
        format!("{zarch}-linux-{env}{suffix}")
    } else if triple.contains("-windows-") {
        // zig's windows-gnu is llvm-mingw, so gnullvm maps onto it too.
        format!("{zarch}-windows-gnu")
    } else {
        bail!("zig cross-compilation is not supported for `{triple}`")
    };
    Ok((target, mcpu))
}

#[derive(Clone, Debug)]
pub struct Wrappers {
    pub dir: PathBuf,
    /// Host-triplet-prefixed tools (`x86_64-unknown-linux-musl-ar`, ...) that autotools, make
    /// and cmake based build scripts look up on `PATH`
    pub bin_dir: PathBuf,
    pub cc: PathBuf,
    pub cxx: PathBuf,
    pub ar: PathBuf,
    pub ranlib: PathBuf,
    pub cmake_toolchain: PathBuf,
}

/// embed-resource runs `*-w64-mingw32-windres --input --output --include-dir`. zig has `rc`, not windres.
fn windres_script(zig: &Path, arch: &str) -> String {
    let zigp = sh_quote(&zig.to_string_lossy());
    format!(
        r#"if [ "$1" = "-V" ]; then echo "GNU windres (rb) 1"; exit 0; fi
input= output=
includes=
while [ $# -gt 0 ]; do
  case "$1" in
    --input) input=$2; shift 2;;
    --output) output=$2; shift 2;;
    --include-dir|-I) includes="$includes /i $2"; shift 2;;
    --output-format|--output-format=coff) shift;;
    -D) includes="$includes /d $2"; shift 2;;
    -D*) includes="$includes /d ${{1#-D}}"; shift;;
    *) shift;;
  esac
done
exec {zigp} rc /:target {arch} /:output-format coff /fo "$output" $includes -- "$input"
"#
    )
}

pub fn write_wrappers(root: &Path, rb_exe: &Path, zig: &Zig, triple: &str, glibc: Option<&str>) -> Result<Wrappers> {
    let (target, mcpu) = zig_target(triple, glibc)?;
    let dir = root.join(format!("zig-{target}"));
    let rb = sh_quote(&rb_exe.to_string_lossy());
    let zigp = sh_quote(&zig.path.to_string_lossy());
    let mut cc_args = format!("-target {target} -g -fno-sanitize=all");
    if let Some(mcpu) = mcpu {
        cc_args.push_str(&format!(" -mcpu={mcpu}"));
    }
    // Musl links static by default, and the Alpine sysroot only ships shared GTK libs
    if triple.contains("-musl") {
        cc_args.push_str(" -dynamic");
    }
    let tool = |name: &str, extra: &str| format!("exec {rb} __zig {zigp} {name} {extra} \"$@\"");
    let w = Wrappers {
        cc: dir.join("cc"),
        cxx: dir.join("c++"),
        ar: dir.join("ar"),
        ranlib: dir.join("ranlib"),
        cmake_toolchain: dir.join("toolchain.cmake"),
        bin_dir: dir.join("bin"),
        dir: dir.clone(),
    };
    write_script(&w.cc, &tool("cc", &cc_args))?;
    write_script(&w.cxx, &tool("c++", &cc_args))?;
    write_script(&w.ar, &tool("ar", ""))?;
    write_script(&w.ranlib, &tool("ranlib", ""))?;
    write_script(&dir.join("lib"), &tool("lib", ""))?;
    let arch = triple.split('-').next().unwrap();
    let mut prefixes = vec![triple.to_owned(), target.split('.').next().unwrap().to_owned()];
    if triple.contains("-windows-") {
        let mingw = format!("{}-w64-mingw32", if triple.starts_with("i686") { "i686" } else { arch });
        write_script(&dir.join("dlltool"), &tool("dlltool", ""))?;
        write_script(&w.bin_dir.join(format!("{mingw}-dlltool")), &tool("dlltool", ""))?;
        let windres = windres_script(&zig.path, arch);
        write_script(&w.bin_dir.join("windres"), &windres)?;
        write_script(&w.bin_dir.join(format!("{mingw}-windres")), &windres)?;
        prefixes.push(mingw);
    }
    for p in &prefixes {
        for (name, t, args) in [
            ("ar", "ar", ""),
            ("ranlib", "ranlib", ""),
            ("objcopy", "objcopy", ""),
            ("cc", "cc", cc_args.as_str()),
            ("gcc", "cc", cc_args.as_str()),
            ("c++", "c++", cc_args.as_str()),
            ("g++", "c++", cc_args.as_str()),
        ] {
            write_script(&w.bin_dir.join(format!("{p}-{name}")), &tool(t, args))?;
        }
    }
    let system = if triple.contains("-windows-") { "Windows" } else { "Linux" };
    let processor = match target.split('-').next().unwrap() {
        "x86" => "i686",
        other => other,
    };
    let cmake = format!(
        "set(CMAKE_SYSTEM_NAME {system})\nset(CMAKE_SYSTEM_PROCESSOR {processor})\n\
         set(CMAKE_C_COMPILER \"{cc}\")\nset(CMAKE_CXX_COMPILER \"{cxx}\")\n\
         set(CMAKE_AR \"{ar}\")\nset(CMAKE_RANLIB \"{ranlib}\")\n",
        cc = w.cc.display(),
        cxx = w.cxx.display(),
        ar = w.ar.display(),
        ranlib = w.ranlib.display()
    );
    if std::fs::read_to_string(&w.cmake_toolchain).ok().as_deref() != Some(cmake.as_str()) {
        std::fs::write(&w.cmake_toolchain, cmake)?;
    }
    Ok(w)
}

struct ZigTarget {
    arch: String,
    musl: bool,
    windows_gnu: bool,
}

impl ZigTarget {
    fn parse(target: &str) -> Self {
        let arch = target.split('-').next().unwrap_or("").to_owned();
        Self {
            musl: target.contains("musl"),
            windows_gnu: target.contains("windows-gnu"),
            arch,
        }
    }
    fn is_arm(&self) -> bool {
        self.arch.starts_with("arm")
    }
    fn is_x86(&self) -> bool {
        self.arch == "x86"
    }
}

enum Filtered {
    Keep(Vec<String>),
    Skip,
    SkipWithNext,
}

/// zig's lld allows 65535 exports. One symbol in the def file stops it exporting every object symbol.
fn cap_gnu_def(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    let mut first = None;
    let mut n = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with(';') || t == "EXPORTS" || t.starts_with("LIBRARY") {
            continue;
        }
        n += 1;
        if first.is_none() {
            first = Some(t.to_owned());
        }
    }
    if n <= 65535 {
        return false;
    }
    let Some(sym) = first else { return false };
    std::fs::write(path, format!("EXPORTS\n{sym}\n")).is_ok()
}

thread_local! {
    static CAPPED_DEF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `zig cc` rejects `/exclude-all-symbols`. Ask zig for the link line and run `zig lld-link`, which accepts it.
fn relink_without_auto_export(zig: &Path, tool: &str, args: &[String]) -> Result<i32> {
    let out = Command::new(zig).arg(tool).arg("-v").args(args).output()?;
    if out.status.success() {
        return Ok(0);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("too many exported symbols")
        && let Some(line) = err.lines().find(|l| l.starts_with("lld-link "))
        && let Ok(mut lld) = shell_words::split(line)
        && !lld.is_empty()
    {
        lld.remove(0);
        lld.push("/exclude-all-symbols".into());
        let st = Command::new(zig).arg("lld-link").args(&lld).status()?;
        return Ok(st.code().unwrap_or(1));
    }
    eprint!("{}", String::from_utf8_lossy(&out.stdout));
    eprint!("{err}");
    Ok(out.status.code().unwrap_or(1))
}

fn filter_arg(arg: &str, t: &ZigTarget, zig: (u32, u32, u32)) -> Filtered {
    use Filtered::*;
    let ge = |maj, min| (zig.0, zig.1) >= (maj, min);
    if arg == "-lgcc_s" {
        return Keep(vec!["-lunwind".into()]);
    }
    if arg.starts_with("--target=") || (arg.starts_with("-B") && arg.contains("gcc-ld")) || arg == "-fno-rtlib-defaultlib" {
        return Skip;
    }
    if arg.starts_with("-e") && arg.len() > 2 && !arg.starts_with("-export") {
        return Keep(vec![format!("-Wl,--entry={}", &arg[2..])]);
    }
    if (t.is_arm() || t.windows_gnu) && arg.ends_with(".rlib") && arg.contains("libcompiler_builtins-") {
        return Skip;
    }
    if t.windows_gnu {
        if arg == "-lgcc_eh" && (!ge(0, 14) || t.is_x86()) {
            return Keep(vec!["-lc++".into()]);
        }
        if (arg.ends_with("rsbegin.o") || arg.ends_with("rsend.o")) && t.is_x86() {
            return Skip;
        }
        if arg == "-Wl,-Bdynamic" && ge(0, 11) {
            return Keep(vec!["-Wl,-search_paths_first".into()]);
        }
        if matches!(arg, "-lwindows" | "-l:libpthread.a" | "-lgcc" | "-lmsvcrt")
            || matches!(
                arg,
                "-Wl,--disable-auto-image-base" | "-Wl,--dynamicbase" | "-Wl,--large-address-aware"
            )
        {
            return Skip;
        }
        // rustc's list.def names every symbol. Dropping it makes lld export every object symbol and overflow the same 65535 cap. One symbol in the def stops that.
        if let Some(path) = arg
            .strip_prefix("-Wl,")
            .filter(|p| p.ends_with("/list.def") || p.ends_with("\\list.def"))
        {
            let _ = cap_gnu_def(Path::new(path));
            // dllexport directives in the objects count even when the def file is short
            CAPPED_DEF.with(|c| c.set(true));
            // `-Wl,file.def` is a linker arg zig rejects. A bare `.def` is an input file it accepts.
            return Keep(vec![path.to_owned()]);
        }
    } else if matches!(
        arg,
        "-Wl,--no-undefined-version" | "-Wl,-znostart-stop-gc" | "-Wl,--fix-cortex-a53-843419"
    ) || arg.starts_with("-Wl,-plugin-opt")
    {
        return Skip;
    }
    if t.musl {
        // zig links its own musl crt objects and libc. `-static` would require .a copies of GTK.
        if (arg.ends_with(".o") && arg.contains("self-contained") && arg.contains("crt"))
            || arg == "-Wl,-melf_i386"
            || arg == "-lc"
            || arg == "-static"
            || arg == "-Wl,-static"
            || arg == "-Wl,-Bstatic"
        {
            return Skip;
        }
    }
    if arg.starts_with("-Wp,") && !["-Wp,-MD", "-Wp,-MMD", "-Wp,-MT"].iter().any(|p| arg.starts_with(p)) {
        return Skip;
    }
    if let Some(march) = arg.strip_prefix("-march=") {
        if t.is_arm() || t.is_x86() {
            return Skip;
        }
        if t.arch == "riscv64" {
            return Keep(vec!["-march=generic_rv64".into()]);
        }
        if march.starts_with("armv") && t.arch == "aarch64" {
            let features = march.find('+').map(|p| &march[p..]).unwrap_or("");
            let mut out = vec![format!("-mcpu=generic{features}")];
            if features.contains("+crypto") {
                out.extend(["-Xassembler".to_owned(), arg.to_owned()]);
            }
            return Keep(out);
        }
    }
    if !ge(0, 16) {
        if arg == "-Wl,-exported_symbols_list" || arg == "-Wl,--dynamic-list" {
            return SkipWithNext;
        }
        if arg.starts_with("-Wl,-exported_symbols_list,") || arg.starts_with("-Wl,--dynamic-list,") {
            return Skip;
        }
    }
    Keep(vec![arg.to_owned()])
}

fn filter_args(args: impl IntoIterator<Item = String>, t: &ZigTarget, zig: (u32, u32, u32)) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        match filter_arg(&arg, t, zig) {
            Filtered::Keep(v) => out.extend(v),
            Filtered::Skip => {}
            Filtered::SkipWithNext => skip_next = true,
        }
    }
    out
}

pub fn run_wrapper(args: &[String]) -> Result<i32> {
    let [zig_path, tool, rest @ ..] = args else {
        bail!("usage: rb __zig <zig> <cc|c++|ar|ranlib|lib|dlltool> [args...]");
    };
    let zig = probe(Path::new(zig_path)).with_context(|| format!("cannot run zig at {zig_path}"))?;
    let mut cmd = Command::new(&zig.path);
    match tool.as_str() {
        "cc" | "c++" => {
            let target = rest
                .iter()
                .position(|a| a == "-target")
                .and_then(|i| rest.get(i + 1))
                .cloned()
                .unwrap_or_default();
            let t = ZigTarget::parse(&target);
            CAPPED_DEF.with(|c| c.set(false));
            let mut seen_target = false;
            let mut out: Vec<String> = Vec::with_capacity(rest.len());
            let mut iter = rest.iter().cloned();
            while let Some(arg) = iter.next() {
                // Keep our own `-target`; drop any later one rustc might add
                if arg == "-target" {
                    if seen_target {
                        iter.next();
                        continue;
                    }
                    seen_target = true;
                    out.push(arg);
                    out.extend(iter.next());
                    continue;
                }
                if let Some(path) = arg.strip_prefix('@').filter(|_| arg.ends_with("linker-arguments")) {
                    let content = std::fs::read_to_string(path)?;
                    let filtered = filter_args(content.split('\n').map(str::to_owned), &t, zig.version);
                    std::fs::write(path, filtered.join("\n"))?;
                    out.push(arg);
                    continue;
                }
                out.extend(filter_args([arg], &t, zig.version));
            }
            if t.windows_gnu && (zig.version.0, zig.version.1) >= (0, 16) {
                out.push("-lcompiler_rt".into());
            }
            if CAPPED_DEF.with(|c| c.get()) {
                return relink_without_auto_export(&zig.path, tool, &out);
            }
            cmd.arg(tool).args(out);
        }
        "ar" | "ranlib" | "lib" | "dlltool" | "objcopy" => {
            cmd.arg(tool).args(rest);
        }
        other => bail!("unknown zig tool `{other}`"),
    }
    let status = cmd.status().with_context(|| format!("failed to run {cmd:?}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_targets() {
        assert_eq!(
            zig_target("x86_64-unknown-linux-gnu", Some("2.17")).unwrap().0,
            "x86_64-linux-gnu.2.17"
        );
        assert_eq!(zig_target("x86_64-unknown-linux-gnu", None).unwrap().0, "x86_64-linux-gnu.2.38");
        assert_eq!(
            zig_target("aarch64-unknown-linux-musl", Some("2.17")).unwrap().0,
            "aarch64-linux-musl"
        );
        assert_eq!(zig_target("x86_64-pc-windows-gnu", None).unwrap().0, "x86_64-windows-gnu");
        assert!(zig_target("x86_64-apple-darwin", None).is_err());
    }

    #[test]
    fn filters_linker_args() {
        let t = ZigTarget::parse("x86_64-linux-musl");
        let out = filter_args(
            [
                "-lgcc_s",
                "/x/self-contained/crt1.o",
                "-lc",
                "-Wl,--no-undefined-version",
                "-o",
                "a",
            ]
            .map(String::from),
            &t,
            (0, 16, 0),
        );
        assert_eq!(out, ["-lunwind", "-o", "a"]);
    }

    #[test]
    fn caps_oversized_gnu_def_to_one_export() {
        let dir = std::env::temp_dir().join(format!("rb-def-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("list.def");
        let mut body = String::from("EXPORTS\n");
        for i in 0..65536 {
            body.push_str(&format!("sym{i}\n"));
        }
        std::fs::write(&path, &body).unwrap();
        let arg = format!("-Wl,{}", path.display());
        let t = ZigTarget::parse("x86_64-windows-gnu");
        let out = filter_args([arg], &t, (0, 16, 0));
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("list.def") && !out[0].starts_with("-Wl,"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "EXPORTS\nsym0\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
