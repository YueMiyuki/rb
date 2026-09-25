use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;

pub fn download(url: &str, dest: &Path, sha256: Option<&str>, label: &str) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part = dest.with_extension("part");
    let resp = ureq::get(url).call().with_context(|| format!("failed to download {url}"))?;
    let total: Option<u64> = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let mut reader = resp.into_body().into_reader();
    let mut file = std::fs::File::create(&part)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut done: u64 = 0;
    let mut last_pct = u64::MAX;
    loop {
        let n = reader.read(&mut buf).with_context(|| format!("error while downloading {url}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        done += n as u64;
        if let Some(total) = total.filter(|t| *t > 0) {
            let pct = done * 100 / total;
            if pct / 10 != last_pct / 10 {
                eprint!("\r  Downloading {label}: {pct:>3}% of {} MiB", total >> 20);
                last_pct = pct;
            }
        }
    }
    if total.is_some() {
        eprintln!();
    }
    file.sync_all()?;
    drop(file);
    let got = hex_lower(&hasher.finalize());
    if let Some(want) = sha256
        && !want.eq_ignore_ascii_case(&got)
    {
        let _ = std::fs::remove_file(&part);
        bail!("checksum mismatch for {url}: expected {want}, got {got}");
    }
    std::fs::rename(&part, dest)?;
    Ok(())
}

/// Download without progress output; verifies the SHA-256 digest when given
pub fn fetch_verified(agent: &ureq::Agent, url: &str, dest: &Path, sha256: Option<&str>) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(path) = url.strip_prefix("file://") {
        let bytes = std::fs::read(path).with_context(|| format!("failed to read {url}"))?;
        return write_verified(&bytes, dest, sha256, url);
    }
    let resp = agent.get(url).call().with_context(|| format!("failed to download {url}"))?;
    let mut bytes = Vec::new();
    resp.into_body()
        .into_reader()
        .read_to_end(&mut bytes)
        .with_context(|| format!("error while downloading {url}"))?;
    write_verified(&bytes, dest, sha256, url)
}

fn write_verified(bytes: &[u8], dest: &Path, sha256: Option<&str>, url: &str) -> Result<()> {
    let got = hex_lower(&Sha256::digest(bytes));
    if let Some(want) = sha256
        && !want.eq_ignore_ascii_case(&got)
    {
        bail!("checksum mismatch for {url}: expected {want}, got {got}");
    }
    let part = dest.with_extension(format!("part{}", std::process::id()));
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, dest)?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex_lower(&h.finalize()))
}

pub fn get_string(url: &str) -> Result<String> {
    let resp = ureq::get(url).call().with_context(|| format!("failed to fetch {url}"))?;
    let mut s = String::new();
    resp.into_body().into_reader().read_to_string(&mut s)?;
    Ok(s)
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
