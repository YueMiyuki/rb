use crate::cas::{Store, hash_file};
use crate::manifest::UnitManifest;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
fn nlink(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(not(unix))]
fn nlink(_meta: &fs::Metadata) -> u64 {
    1
}

fn digest_of(file_name: &str) -> &str {
    file_name.strip_suffix(".zst").unwrap_or(file_name)
}

fn files_under(dir: &Path) -> impl Iterator<Item = walkdir::DirEntry> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
}

fn load_all(store: &Store) -> Vec<(PathBuf, UnitManifest)> {
    files_under(&store.root().join("units"))
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let m: UnitManifest = serde_json::from_slice(&fs::read(e.path()).ok()?).ok()?;
            Some((e.path().to_owned(), m))
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct StoreStatus {
    pub units: usize,
    pub entries: usize,
    pub blobs: usize,
    /// Compressed because nothing has them materialized.
    pub compressed_blobs: usize,
    pub blob_bytes: u64,
    /// Not hardlinked into any project.
    pub exclusive_bytes: u64,
    /// Hardlinked into a project. Freed only when the project drops them too.
    pub shared_bytes: u64,
    pub oldest_use: Option<u64>,
}

pub fn status(store: &Store) -> Result<StoreStatus> {
    let mut s = StoreStatus::default();
    for (_, m) in load_all(store) {
        s.units += 1;
        s.entries += m.entries.len();
        let lu = m.last_used();
        s.oldest_use = Some(s.oldest_use.map_or(lu, |o: u64| o.min(lu)));
    }
    for e in files_under(&store.root().join("cas")) {
        if let Ok(meta) = e.metadata() {
            s.blobs += 1;
            s.compressed_blobs += usize::from(e.file_name().to_string_lossy().ends_with(".zst"));
            s.blob_bytes += meta.len();
            if nlink(&meta) > 1 {
                s.shared_bytes += meta.len();
            } else {
                s.exclusive_bytes += meta.len();
            }
        }
    }
    Ok(s)
}

#[derive(Debug, Default, Clone)]
pub struct GcOptions {
    pub max_size: Option<u64>,
    pub max_age_secs: Option<u64>,
    pub dry_run: bool,
    /// Survive no matter what. Symlinked into a live project, for example.
    pub pinned: HashSet<String>,
    /// Drop these even if they're new and the store is under the cap.
    pub remove: HashSet<String>,
    /// Blobs only referenced by units outside this set get compressed.
    pub hot: Option<HashSet<String>>,
    /// A build may be running. Don't touch a blob whose manifest isn't written yet, unless it's an orphan older than an hour.
    pub concurrent: bool,
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub units_removed: usize,
    pub units_kept: usize,
    pub blobs_removed: usize,
    pub bytes_freed: u64,
    pub bytes_kept: u64,
    pub blobs_compressed: usize,
    pub bytes_compressed: u64,
}

/// Smaller than this stays plain. Compressing it barely saves anything and costs a file.
const MIN_COMPRESS: u64 = 64 << 10;

/// Caller holds `gc_lock`.
pub fn gc(store: &Store, opts: &GcOptions) -> Result<GcReport> {
    let now = crate::now_secs();
    let mut report = GcReport::default();
    let mut units = load_all(store);
    units.sort_by_key(|(_, m)| m.last_used());

    let blob_size = |d: &str| {
        fs::metadata(store.blob_path(d))
            .or_else(|_| fs::metadata(store.zst_path(d)))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let mut refs: HashMap<String, usize> = HashMap::new();
    for (_, m) in &units {
        for e in &m.entries {
            for o in &e.outputs {
                *refs.entry(o.blob.clone()).or_default() += 1;
            }
        }
    }
    let mut live_bytes: u64 = refs.keys().map(|d| blob_size(d)).sum();

    let mut remove: Vec<usize> = Vec::new();
    for (i, (_, m)) in units.iter().enumerate() {
        if opts.pinned.contains(&m.key) {
            continue;
        }
        let too_old = opts.max_age_secs.is_some_and(|age| now.saturating_sub(m.last_used()) > age);
        let too_big = opts.max_size.is_some_and(|max| live_bytes > max);
        // Anything a live project doesn't list is a variant this build already replaced.
        // Compressing it left a second copy: reflinks share blocks until the plain file becomes a .zst, and df counts both.
        let unused = opts.hot.as_ref().is_some_and(|hot| !hot.contains(&m.key));
        if !(opts.remove.contains(&m.key) || too_old || too_big || unused) {
            continue;
        }
        remove.push(i);
        for e in &m.entries {
            for o in &e.outputs {
                let c = refs.get_mut(&o.blob).unwrap();
                *c -= 1;
                if *c == 0 {
                    live_bytes = live_bytes.saturating_sub(blob_size(&o.blob));
                }
            }
        }
    }
    let removed: HashSet<usize> = remove.iter().copied().collect();
    for &i in &remove {
        report.units_removed += 1;
        if !opts.dry_run {
            let _ = fs::remove_file(&units[i].0);
        }
    }
    report.units_kept = units.len() - removed.len();
    let removed_blobs: HashSet<&str> = remove
        .iter()
        .flat_map(|&i| units[i].1.entries.iter().flat_map(|e| &e.outputs))
        .map(|o| o.blob.as_str())
        .collect();

    let live: HashSet<&str> = refs.iter().filter(|(_, c)| **c > 0).map(|(d, _)| d.as_str()).collect();
    for e in files_under(&store.root().join("cas")) {
        let name = e.file_name().to_string_lossy();
        let Ok(meta) = e.metadata() else { continue };
        let recent = || {
            meta.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age.as_secs() < 3600)
        };
        if live.contains(digest_of(&name)) || (opts.concurrent && !removed_blobs.contains(digest_of(&name)) && recent()) {
            report.bytes_kept += meta.len();
            continue;
        }
        report.blobs_removed += 1;
        if nlink(&meta) <= 1 {
            report.bytes_freed += meta.len();
        }
        if !opts.dry_run {
            let _ = fs::remove_file(e.path());
        }
    }

    // Cold blobs live only here, so they can be compressed. A hardlink means a project still shares the blocks.
    if let Some(hot) = opts.hot.as_ref().filter(|_| !opts.dry_run) {
        let mut plain_needed: HashSet<&str> = HashSet::new();
        let mut cold: HashSet<&str> = HashSet::new();
        for (_, (_, m)) in units.iter().enumerate().filter(|(i, _)| !removed.contains(i)) {
            let keep_plain = hot.contains(&m.key) || opts.pinned.contains(&m.key);
            for o in m.entries.iter().flat_map(|e| &e.outputs) {
                if keep_plain || crate::cas::is_loadable(&o.name) {
                    plain_needed.insert(&o.blob);
                } else if o.size >= MIN_COMPRESS {
                    cold.insert(&o.blob);
                }
            }
        }
        let todo: Vec<&str> = cold
            .difference(&plain_needed)
            .copied()
            .filter(|d| fs::metadata(store.blob_path(d)).is_ok_and(|m| nlink(&m) <= 1))
            .collect();
        let saved = std::sync::Mutex::new((0usize, 0u64));
        crate::par_for_each(&todo, |d| {
            if let Ok(n) = store.compress(d)
                && n > 0
            {
                let mut s = saved.lock().unwrap();
                s.0 += 1;
                s.1 += n;
            }
        });
        (report.blobs_compressed, report.bytes_compressed) = saved.into_inner().unwrap();
        report.bytes_kept = report.bytes_kept.saturating_sub(report.bytes_compressed);
    }

    // Leftovers from a crashed build.
    for e in files_under(&store.tmp_dir()) {
        let stale = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 24 * 3600);
        if stale && !opts.dry_run {
            let _ = fs::remove_file(e.path());
        }
    }
    Ok(report)
}

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub blobs_checked: usize,
    pub corrupt_blobs: Vec<String>,
    pub broken_units: Vec<String>,
}

pub fn verify(store: &Store, repair: bool) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();
    for e in files_under(&store.root().join("cas")) {
        report.blobs_checked += 1;
        let file = e.file_name().to_string_lossy().into_owned();
        let name = digest_of(&file).to_owned();
        let hash = if file.ends_with(".zst") {
            fs::File::open(e.path())
                .and_then(zstd::decode_all)
                .map(|b| crate::cas::hash_bytes(&b))
        } else {
            hash_file(e.path())
        };
        match hash {
            Ok(h) if h == name => {}
            _ => {
                if repair {
                    let _ = fs::remove_file(e.path());
                }
                report.corrupt_blobs.push(name);
            }
        }
    }
    let corrupt: HashSet<&str> = report.corrupt_blobs.iter().map(|s| s.as_str()).collect();
    for (path, m) in load_all(store) {
        let broken = m
            .entries
            .iter()
            .flat_map(|e| &e.outputs)
            .any(|o| corrupt.contains(o.blob.as_str()) || !store.has_blob(&o.blob));
        if broken {
            if repair {
                let _ = fs::remove_file(&path);
            }
            report.broken_units.push(m.key.clone());
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Entry, OutputFile};

    #[test]
    fn gc_removes_unreferenced_and_old() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let keep = store.ingest_bytes(b"keep", false).unwrap();
        let old = store.ingest_bytes(b"old", false).unwrap();
        let orphan = store.ingest_bytes(b"orphan", false).unwrap();
        let mk = |key: &str, blob: &str, used: u64| UnitManifest {
            key: key.into(),
            entries: vec![Entry {
                last_used: used,
                outputs: vec![OutputFile {
                    name: "f".into(),
                    blob: blob.into(),
                    exec: false,
                    size: 4,
                    templated: false,
                }],
                ..Default::default()
            }],
        };
        let now = crate::now_secs();
        store.save_manifest(&mk("aa11", &keep, now)).unwrap();
        store.save_manifest(&mk("bb22", &old, now - 100 * 86400)).unwrap();
        let r = gc(
            &store,
            &GcOptions {
                max_age_secs: Some(30 * 86400),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.units_removed, 1);
        assert!(store.has_blob(&keep));
        assert!(!store.has_blob(&old));
        assert!(!store.has_blob(&orphan));
        assert!(verify(&store, false).unwrap().broken_units.is_empty());
    }

    #[test]
    fn gc_drops_units_no_project_lists() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let live = store.ingest_bytes(b"live-bytes-pad", false).unwrap();
        let old = store.ingest_bytes(b"old-variant-pad", false).unwrap();
        let mk = |key: &str, blob: &str| UnitManifest {
            key: key.into(),
            entries: vec![Entry {
                last_used: crate::now_secs(),
                outputs: vec![OutputFile {
                    name: "lib.rlib".into(),
                    blob: blob.into(),
                    exec: false,
                    size: 80_000,
                    templated: false,
                }],
                ..Default::default()
            }],
        };
        store.save_manifest(&mk("live", &live)).unwrap();
        store.save_manifest(&mk("old", &old)).unwrap();
        let mut hot = HashSet::new();
        hot.insert("live".into());
        gc(
            &store,
            &GcOptions {
                hot: Some(hot),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(store.load_manifest("live").unwrap().is_some());
        assert!(store.load_manifest("old").unwrap().is_none());
        assert!(store.has_blob(&live));
        assert!(!store.has_blob(&old));
    }
}
