//! MSVC CRT and Windows SDK, via the `xwin` crate.

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const LICENSE_URL: &str = "https://go.microsoft.com/fwlink/?LinkId=2086102";

/// The xwin architecture directory name for a Rust `*-pc-windows-msvc` triple
pub fn arch_dir(triple: &str) -> Result<&'static str> {
    Ok(match triple.split('-').next().unwrap_or("") {
        "x86_64" => "x86_64",
        "aarch64" | "arm64ec" => "aarch64",
        "i686" | "i586" => "x86",
        "thumbv7a" => "aarch",
        other => bail!("unsupported MSVC architecture `{other}`"),
    })
}

fn splat_dir(home: &Path) -> PathBuf {
    home.join("xwin").join("splat")
}

fn marker(home: &Path) -> PathBuf {
    splat_dir(home).join(".rb-arches")
}

fn installed_arches(home: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(marker(home))
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The splat root if it already contains `arch`
pub fn find(home: &Path, arch: &str) -> Option<PathBuf> {
    installed_arches(home).contains(arch).then(|| splat_dir(home))
}

/// One download, splatted for every architecture under `<home>/xwin/splat`.
pub fn ensure(home: &Path, wanted: &[&str], accept_license: bool) -> Result<PathBuf> {
    let mut arches = installed_arches(home);
    if wanted.iter().all(|a| arches.contains(*a)) {
        return Ok(splat_dir(home));
    }
    if !accept_license {
        bail!(
            "targeting MSVC requires the Microsoft CRT and Windows SDK, which are subject to the license at {LICENSE_URL}\n\
             Re-run with `rb target add <triple> --accept-license` (or set RB_ACCEPT_MSVC_LICENSE=1) to download them."
        );
    }
    arches.extend(wanted.iter().map(|a| (*a).to_owned()));

    let cache = home.join("xwin").join("cache");
    let output = splat_dir(home);
    let cache_utf8 = xwin::PathBuf::from_path_buf(cache.clone()).map_err(|p| anyhow::anyhow!("non UTF-8 path {}", p.display()))?;
    let output_utf8 = xwin::PathBuf::from_path_buf(output.clone()).map_err(|p| anyhow::anyhow!("non UTF-8 path {}", p.display()))?;

    eprintln!(
        "  Downloading MSVC CRT + Windows SDK for {} (license: {LICENSE_URL})",
        arches.iter().cloned().collect::<Vec<_>>().join(", ")
    );
    let agent = xwin::ureq::Agent::new_with_defaults();
    let ctx = Arc::new(xwin::Ctx::with_dir(cache_utf8, xwin::util::ProgressTarget::Stderr, agent, 3)?);
    let hidden = indicatif::ProgressBar::hidden;
    let manifest = xwin::manifest::get_manifest(&ctx, 17, "release", hidden())?;
    let pkg_manifest = xwin::manifest::get_package_manifest(&ctx, &manifest, hidden())?;

    let mut arch_mask = 0u32;
    for a in &arches {
        let parsed: xwin::Arch = a.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
        arch_mask |= parsed as u32;
    }
    let variants = xwin::Variant::Desktop as u32;
    let pruned = xwin::prune_pkg_list(&pkg_manifest, arch_mask, variants, false, false, None, None)?;

    let mp = indicatif::MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::stderr());
    let style = indicatif::ProgressStyle::default_bar()
        .template("  {prefix:>28} [{elapsed}] {wide_bar} {bytes}/{total_bytes}")
        .unwrap();
    let work: Vec<_> = pruned
        .payloads
        .into_iter()
        .map(|p| {
            let pb = mp.add(
                indicatif::ProgressBar::new(0)
                    .with_style(style.clone())
                    .with_prefix(p.filename.to_string()),
            );
            xwin::WorkItem {
                payload: Arc::new(p),
                progress: pb,
            }
        })
        .collect();

    let _ = std::fs::remove_dir_all(&output);
    let op = xwin::Ops::Splat(xwin::SplatConfig {
        include_debug_libs: false,
        include_debug_symbols: false,
        enable_symlinks: true,
        preserve_ms_arch_notation: false,
        use_winsysroot_style: false,
        copy: false,
        map: None,
        output: output_utf8,
    });
    let packages = pkg_manifest.packages;
    let (crt, sdk, vcr) = (pruned.crt_version, pruned.sdk_version, pruned.vcr_version);
    std::thread::spawn(move || ctx.execute(packages, work, crt, sdk, vcr, arch_mask, variants, op))
        .join()
        .map_err(|_| anyhow::anyhow!("xwin panicked"))?
        .context("xwin splat failed")?;

    // The splat is the part we keep. Drop the download caches.
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::write(marker(home), arches.iter().cloned().collect::<Vec<_>>().join("\n"))?;
    Ok(output)
}
