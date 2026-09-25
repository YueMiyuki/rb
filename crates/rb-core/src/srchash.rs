//! Content hashes for path packages. An mtime/size/inode index so unchanged files aren't read again.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Stamp {
    mtime_ns: i128,
    size: u64,
    ino: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    stamp: Stamp,
    hash: String,
}

pub struct SourceHasher {
    path: PathBuf,
    index: Mutex<HashMap<PathBuf, Entry>>,
    trees: Mutex<HashMap<PathBuf, String>>,
    dirty: AtomicBool,
    exclude: Vec<PathBuf>,
}

fn stamp(meta: &std::fs::Metadata) -> Stamp {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let ino = 0;
    Stamp {
        mtime_ns,
        size: meta.len(),
        ino,
    }
}

impl SourceHasher {
    /// `exclude`: directories never hashed (the target dir)
    pub fn load(state_dir: &Path, exclude: Vec<PathBuf>) -> Self {
        let path = state_dir.join("hash-index.json");
        let index = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path,
            index: Mutex::new(index),
            trees: Mutex::default(),
            dirty: AtomicBool::new(false),
            exclude,
        }
    }

    /// blake3 of a file's content, `None` if it does not exist
    pub fn file_hash(&self, path: &Path) -> Result<Option<String>> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if meta.is_dir() {
            return self.tree_hash(path).map(Some);
        }
        let st = stamp(&meta);
        if let Some(e) = self.index.lock().unwrap().get(path)
            && e.stamp == st
        {
            return Ok(Some(e.hash.clone()));
        }
        let hash = rb_store::hash_file(path)?;
        self.index.lock().unwrap().insert(
            path.to_owned(),
            Entry {
                stamp: st,
                hash: hash.clone(),
            },
        );
        self.dirty.store(true, Ordering::Relaxed);
        Ok(Some(hash))
    }

    pub fn tree_hash(&self, root: &Path) -> Result<String> {
        if let Some(h) = self.trees.lock().unwrap().get(root) {
            return Ok(h.clone());
        }
        let exclude = self.exclude.clone();
        let root_owned = root.to_owned();
        let walker = ignore::WalkBuilder::new(root)
            .require_git(false)
            .filter_entry(move |e| {
                let p = e.path();
                if e.file_type().is_some_and(|t| t.is_dir()) && p != root_owned {
                    if e.file_name() == "target" || exclude.iter().any(|x| p.starts_with(x)) {
                        return false;
                    }
                    if p.join("Cargo.toml").is_file() {
                        return false;
                    }
                }
                true
            })
            .build();
        let mut files: Vec<PathBuf> = walker
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.into_path())
            .collect();
        files.sort();
        let mut h = blake3::Hasher::new();
        for f in &files {
            let rel = f.strip_prefix(root).unwrap_or(f);
            h.update(rel.to_string_lossy().as_bytes());
            h.update(b"\0");
            if let Some(fh) = self.file_hash(f)? {
                h.update(fh.as_bytes());
            }
            h.update(b"\n");
        }
        let out = h.finalize().to_hex().to_string();
        self.trees.lock().unwrap().insert(root.to_owned(), out.clone());
        Ok(out)
    }

    /// Recomputed, not memoized. True if `root` still hashes the way it did earlier this run.
    pub fn tree_unchanged(&self, root: &Path) -> Result<bool> {
        let Some(before) = self.trees.lock().unwrap().remove(root) else {
            return Ok(true);
        };
        Ok(self.tree_hash(root)? == before)
    }

    pub fn save(&self) {
        if !self.dirty.load(Ordering::Relaxed) {
            return;
        }
        let index = self.index.lock().unwrap();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(&*index) {
            let tmp = self.path.with_extension("tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }
}
