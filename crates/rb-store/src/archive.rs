//! A directory packed as one zstd tar. Incremental caches can't live in the store, but they compress well.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub fn pack(dir: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = crate::link::sibling_tmp(dst);
    let packed = (|| -> Result<()> {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
        let mut enc = zstd::stream::Encoder::new(fs::File::create(&tmp)?, 3)?;
        enc.multithread(threads)?;
        let mut tar = tar::Builder::new(enc);
        tar.follow_symlinks(false);
        tar.append_dir_all(".", dir)?;
        tar.into_inner()?.finish()?;
        Ok(())
    })();
    if let Err(e) = packed {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to archive {}", dir.display()));
    }
    fs::rename(&tmp, dst)?;
    fs::remove_dir_all(dir)?;
    Ok(())
}

pub fn unpack(archive: &Path, dir: &Path) -> Result<()> {
    let tmp = crate::link::sibling_tmp(dir);
    let unpacked = (|| -> Result<()> {
        let dec = zstd::stream::Decoder::new(fs::File::open(archive)?)?;
        tar::Archive::new(dec).unpack(&tmp)?;
        Ok(())
    })();
    if let Err(e) = unpacked {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e).with_context(|| format!("failed to restore {}", archive.display()));
    }
    fs::rename(&tmp, dir)?;
    let _ = fs::remove_file(archive);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("inc/app-0123456789abcdef");
        std::fs::create_dir_all(dir.join("s-1")).unwrap();
        std::fs::write(dir.join("s-1/query-cache.bin"), b"cache".repeat(1000)).unwrap();
        let archive = t.path().join("archive/app-0123456789abcdef.tar.zst");
        super::pack(&dir, &archive).unwrap();
        assert!(!dir.exists() && archive.is_file());
        super::unpack(&archive, &dir).unwrap();
        assert_eq!(std::fs::read(dir.join("s-1/query-cache.bin")).unwrap(), b"cache".repeat(1000));
        assert!(!archive.exists());
    }
}
