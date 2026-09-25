//! Content-addressed store. Default root is `~/.rb/store/v1`.
//!
//! ```text
//! cas/<ab>/<blake3>      blob, 0444, or 0555 if it's executable
//! cas/<ab>/<blake3>.zst  same blob, compressed while nothing has it materialized
//! units/<ab>/<key>.json  outputs, discovered inputs, diagnostics to replay
//! locks/<ab>/<key>.lock  one build of this unit at a time
//! tmp/                   staged here, then renamed into cas/
//! store.lock             shared by builds, exclusive for GC
//! ```

pub mod archive;
pub mod cas;
pub mod gc;
pub mod link;
pub mod lock;
pub mod manifest;

pub use cas::{Store, hash_bytes, hash_file};
pub use link::{LinkKind, LinkMode};
pub use lock::FileLock;
pub use manifest::{Entry, EnvInput, ExtraInputs, FileInput, OutputFile, UnitManifest};

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

pub fn par_for_each<T: Sync>(items: &[T], f: impl Fn(&T) + Sync) {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(items.len());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                while let Some(item) = items.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                    f(item);
                }
            });
        }
    });
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
