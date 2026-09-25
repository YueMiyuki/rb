use crate::link::{self, LinkKind, LinkMode};
use crate::lock::FileLock;
use crate::manifest::UnitManifest;
use anyhow::{Context, Result};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static QUEUE_SEQ: AtomicU64 = AtomicU64::new(0);

pub const STORE_VERSION: &str = "v1";

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    /// Bytes this handle wrote. The cap sees growth without walking `cas/` after every build.
    ingested: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// One core, memory-mapped. Hashing on every core used to run beside rustc and lose to cargo.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_mmap(path)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(unix)]
fn is_exec(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(path, perms)
}

/// Proc-macros and other libs rustc loads into itself. macOS checks code once per inode, so a fresh copy costs seconds. Hardlink, never compress.
pub fn is_loadable(name: &str) -> bool {
    [".dylib", ".so", ".dll"].iter().any(|s| name.ends_with(s))
}

fn shard(digest: &str) -> &str {
    &digest[..2.min(digest.len())]
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.join(STORE_VERSION);
        for sub in ["cas", "units", "locks", "tmp"] {
            fs::create_dir_all(root.join(sub)).with_context(|| format!("failed to create store directory {}", root.join(sub).display()))?;
        }
        Ok(Self {
            root,
            ingested: Default::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn blob_path(&self, digest: &str) -> PathBuf {
        self.root.join("cas").join(shard(digest)).join(digest)
    }

    pub fn manifest_path(&self, key: &str) -> PathBuf {
        self.root.join("units").join(shard(key)).join(format!("{key}.json"))
    }

    /// Compressed form, for a blob no project has materialized. Expanded on the first link.
    pub fn zst_path(&self, digest: &str) -> PathBuf {
        let mut p = self.blob_path(digest).into_os_string();
        p.push(".zst");
        PathBuf::from(p)
    }

    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).is_file() || self.zst_path(digest).is_file()
    }

    fn expand(&self, digest: &str) -> Result<()> {
        let zst = self.zst_path(digest);
        if self.blob_path(digest).is_file() || !zst.is_file() {
            return Ok(());
        }
        let exec = is_exec(&fs::metadata(&zst)?);
        let tmp = link::sibling_tmp(&self.tmp_dir().join(digest));
        let expanded = (|| -> io::Result<()> {
            let mut out = fs::File::create(&tmp)?;
            zstd::stream::copy_decode(fs::File::open(&zst)?, &mut out)
        })();
        if let Err(e) = expanded {
            let _ = fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("failed to expand {}", zst.display()));
        }
        self.finish_blob(&tmp, digest, exec)?;
        let _ = fs::remove_file(&zst);
        Ok(())
    }

    /// Compress when it saves at least a tenth. Caller holds `gc_lock`, because builds link the plain blob.
    pub fn compress(&self, digest: &str) -> Result<u64> {
        let plain = self.blob_path(digest);
        let Ok(meta) = fs::metadata(&plain) else { return Ok(0) };
        let tmp = link::sibling_tmp(&self.tmp_dir().join(format!("{digest}.zst")));
        let threads = if meta.len() > 64 << 20 {
            std::thread::available_parallelism().map_or(1, |n| n.get()) as u32
        } else {
            0
        };
        let packed = (|| -> io::Result<u64> {
            let mut enc = zstd::stream::Encoder::new(fs::File::create(&tmp)?, 3)?;
            enc.multithread(threads)?;
            io::copy(&mut fs::File::open(&plain)?, &mut enc)?;
            enc.finish()?;
            Ok(fs::metadata(&tmp)?.len())
        })();
        let packed = match packed {
            Ok(n) if n <= meta.len() / 10 * 9 => n,
            other => {
                let _ = fs::remove_file(&tmp);
                return other.map(|_| 0).with_context(|| format!("failed to compress {}", plain.display()));
            }
        };
        set_mode(&tmp, if is_exec(&meta) { 0o555 } else { 0o444 })?;
        fs::rename(&tmp, self.zst_path(digest))?;
        fs::remove_file(&plain)?;
        Ok(meta.len() - packed)
    }

    fn finish_blob(&self, tmp: &Path, digest: &str, exec: bool) -> Result<()> {
        set_mode(tmp, if exec { 0o555 } else { 0o444 })?;
        let dst = self.blob_path(digest);
        fs::create_dir_all(dst.parent().unwrap())?;
        if dst.exists() {
            let _ = fs::remove_file(tmp);
            return Ok(());
        }
        fs::rename(tmp, &dst).with_context(|| format!("failed to move blob into {}", dst.display()))?;
        let len = fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
        self.ingested.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Reflink, then hardlink. `(digest, size, executable)`.
    pub fn ingest_file(&self, src: &Path) -> Result<(String, u64, bool)> {
        let meta = fs::metadata(src).with_context(|| format!("failed to stat {}", src.display()))?;
        let exec = is_exec(&meta);
        let digest = hash_file(src).with_context(|| format!("failed to hash {}", src.display()))?;
        if !self.has_blob(&digest) {
            let tmp = link::sibling_tmp(&self.tmp_dir().join(&digest));
            let mode = if src.file_name().is_some_and(|n| is_loadable(&n.to_string_lossy())) {
                LinkMode::Hardlink
            } else {
                LinkMode::Auto
            };
            link::place(src, &tmp, mode, false).with_context(|| format!("failed to stage {} into the store", src.display()))?;
            self.finish_blob(&tmp, &digest, exec)?;
        }
        Ok((digest, meta.len(), exec))
    }

    pub fn ingest_bytes(&self, bytes: &[u8], exec: bool) -> Result<String> {
        let digest = hash_bytes(bytes);
        if !self.has_blob(&digest) {
            let tmp = link::sibling_tmp(&self.tmp_dir().join(&digest));
            fs::write(&tmp, bytes)?;
            self.finish_blob(&tmp, &digest, exec)?;
        }
        Ok(digest)
    }

    pub fn read_blob(&self, digest: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(digest);
        match fs::read(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && self.zst_path(digest).is_file() => {
                zstd::decode_all(fs::File::open(self.zst_path(digest))?).context("failed to expand store blob")
            }
            r => r.with_context(|| format!("missing store blob {}", path.display())),
        }
    }

    pub fn materialize(&self, digest: &str, dst: &Path, mode: LinkMode, exec: bool) -> Result<LinkKind> {
        self.expand(digest)?;
        let blob = self.blob_path(digest);
        let loadable = dst.file_name().is_some_and(|n| is_loadable(&n.to_string_lossy()));
        let mode = if loadable && matches!(mode, LinkMode::Auto | LinkMode::Reflink) {
            LinkMode::Hardlink
        } else {
            mode
        };
        // Compaction can compress the blob between expand and link.
        let kind = match link::replace(&blob, dst, mode, true) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.expand(digest)?;
                link::replace(&blob, dst, mode, true)
            }
            r => r,
        }
        .with_context(|| format!("failed to link {} -> {}", blob.display(), dst.display()))?;
        if matches!(kind, LinkKind::Reflink | LinkKind::Copy) {
            // Copies can be writable. The blob stays read-only.
            set_mode(dst, if exec { 0o755 } else { 0o644 })?;
        }
        Ok(kind)
    }

    pub fn load_manifest(&self, key: &str) -> Result<Option<UnitManifest>> {
        let path = self.manifest_path(key);
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(m) => Ok(Some(m)),
                // Torn or not ours. Miss, and the next build rewrites it.
                Err(_) => Ok(None),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub fn save_manifest(&self, manifest: &UnitManifest) -> Result<()> {
        let path = self.manifest_path(&manifest.key);
        fs::create_dir_all(path.parent().unwrap())?;
        let tmp = link::sibling_tmp(&path);
        fs::write(&tmp, serde_json::to_vec(manifest)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn alias(&self, from: &str, to: &str) -> Result<()> {
        let Some(src) = self.load_manifest(from)? else { return Ok(()) };
        let mut dst = self.load_manifest(to)?.unwrap_or_else(|| UnitManifest {
            key: to.to_owned(),
            entries: Vec::new(),
        });
        for entry in src.entries.into_iter().rev() {
            dst.upsert(entry);
        }
        self.save_manifest(&dst)
    }

    pub fn remove_manifest(&self, key: &str) -> Result<()> {
        match fs::remove_file(self.manifest_path(key)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    pub fn unit_lock(&self, key: &str) -> Result<FileLock> {
        let path = self.root.join("locks").join(shard(key)).join(format!("{key}.lock"));
        Ok(FileLock::exclusive(&path)?)
    }

    /// Last full count plus what this handle added. Recounted hourly. Other processes can under-count until then. The cap is soft.
    pub fn approx_bytes(&self) -> Result<u64> {
        let added = self.ingested.swap(0, std::sync::atomic::Ordering::Relaxed);
        let path = self.root.join("size.json");
        let cached = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| Some((v["bytes"].as_u64()?, v["at"].as_u64()?)))
            .filter(|(_, at)| crate::now_secs().saturating_sub(*at) < 3600);
        let bytes = match cached {
            Some((bytes, at)) => {
                let bytes = bytes + added;
                self.write_size(bytes, at)?;
                bytes
            }
            None => {
                let bytes = walkdir::WalkDir::new(self.root.join("cas"))
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum();
                self.set_approx_bytes(bytes)?;
                bytes
            }
        };
        Ok(bytes)
    }

    pub fn set_approx_bytes(&self, bytes: u64) -> Result<()> {
        self.ingested.store(0, std::sync::atomic::Ordering::Relaxed);
        self.write_size(bytes, crate::now_secs())
    }

    fn write_size(&self, bytes: u64, counted_at: u64) -> Result<()> {
        let path = self.root.join("size.json");
        let tmp = link::sibling_tmp(&path);
        fs::write(&tmp, serde_json::json!({ "bytes": bytes, "at": counted_at }).to_string())?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Shared for the whole build, so GC can't run at the same time.
    pub fn build_lock(&self) -> Result<FileLock> {
        Ok(FileLock::shared(&self.root.join("store.lock"))?)
    }

    pub fn gc_lock(&self) -> Result<FileLock> {
        Ok(FileLock::exclusive(&self.root.join("store.lock"))?)
    }

    pub fn try_gc_lock(&self) -> Result<Option<FileLock>> {
        Ok(FileLock::try_exclusive(&self.root.join("store.lock"))?)
    }

    /// None if a compaction is already running.
    pub fn try_compact_lock(&self) -> Result<Option<FileLock>> {
        Ok(FileLock::try_exclusive(&self.root.join("compact.lock"))?)
    }

    pub fn queue_removal(&self, keys: &[String]) -> Result<()> {
        self.write_queue("rm", &serde_json::to_vec(keys)?)
    }

    pub fn write_queue(&self, prefix: &str, bytes: &[u8]) -> Result<()> {
        let dir = self.root.join("queue");
        fs::create_dir_all(&dir)?;
        let n = QUEUE_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{prefix}-{}-{n}.json", std::process::id()));
        let tmp = link::sibling_tmp(&path);
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn queued_removals(&self) -> (std::collections::HashSet<String>, Vec<PathBuf>) {
        let mut keys = std::collections::HashSet::new();
        let mut files = Vec::new();
        for e in fs::read_dir(self.root.join("queue")).into_iter().flatten().filter_map(|e| e.ok()) {
            if e.file_name().to_string_lossy().starts_with("ingest-") {
                continue;
            }
            if let Ok(list) = fs::read(e.path()).map(|b| serde_json::from_slice::<Vec<String>>(&b).unwrap_or_default()) {
                keys.extend(list);
                files.push(e.path());
            }
        }
        (keys, files)
    }

    /// Left behind by builds that didn't want to hash big outputs while rustc was running.
    pub fn ingest_jobs(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut jobs = Vec::new();
        for e in fs::read_dir(self.root.join("queue")).into_iter().flatten().filter_map(|e| e.ok()) {
            if e.file_name().to_string_lossy().starts_with("ingest-")
                && let Ok(b) = fs::read(e.path())
            {
                jobs.push((e.path(), b));
            }
        }
        jobs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_dedupes_and_blobs_are_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("store")).unwrap();
        let a = dir.path().join("a.rlib");
        let b = dir.path().join("b.rlib");
        fs::write(&a, b"same bytes").unwrap();
        fs::write(&b, b"same bytes").unwrap();
        let (da, _, _) = store.ingest_file(&a).unwrap();
        let (db, _, _) = store.ingest_file(&b).unwrap();
        assert_eq!(da, db);
        let blob = store.blob_path(&da);
        assert!(fs::metadata(&blob).unwrap().permissions().readonly());
        assert!(fs::OpenOptions::new().write(true).open(&blob).is_err(), "blob must not be writable");

        let out = dir.path().join("proj/target/libx.rlib");
        store.materialize(&da, &out, LinkMode::Auto, false).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"same bytes");
    }

    #[test]
    fn compressed_blobs_expand_on_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("store")).unwrap();
        let bytes = b"debuginfo ".repeat(10_000);
        let d = store.ingest_bytes(&bytes, true).unwrap();
        assert!(store.compress(&d).unwrap() > 0);
        assert!(!store.blob_path(&d).exists() && store.zst_path(&d).is_file() && store.has_blob(&d));
        assert_eq!(store.read_blob(&d).unwrap(), bytes);
        let out = dir.path().join("proj/target/app");
        store.materialize(&d, &out, LinkMode::Auto, true).unwrap();
        assert_eq!(fs::read(&out).unwrap(), bytes);
        assert!(store.blob_path(&d).is_file() && !store.zst_path(&d).exists());
        assert_eq!(hash_file(&store.blob_path(&d)).unwrap(), d);
        assert!(is_exec(&fs::metadata(store.blob_path(&d)).unwrap()));
    }
}
