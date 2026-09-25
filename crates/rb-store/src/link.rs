use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// How a store blob lands in `target/`, and the other way around.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    /// reflink, then hardlink, then symlink, then copy.
    #[default]
    Auto,
    Reflink,
    Hardlink,
    Symlink,
    Copy,
}

impl std::str::FromStr for LinkMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "auto" => Self::Auto,
            "reflink" | "clone" => Self::Reflink,
            "hardlink" => Self::Hardlink,
            "symlink" => Self::Symlink,
            "copy" => Self::Copy,
            other => return Err(format!("unknown link mode `{other}` (expected auto|reflink|hardlink|symlink|copy)")),
        })
    }
}

impl fmt::Display for LinkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Reflink => "reflink",
            Self::Hardlink => "hardlink",
            Self::Symlink => "symlink",
            Self::Copy => "copy",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    Reflink,
    Hardlink,
    Symlink,
    Copy,
}

impl LinkKind {
    pub fn as_mode(self) -> LinkMode {
        match self {
            Self::Reflink => LinkMode::Reflink,
            Self::Hardlink => LinkMode::Hardlink,
            Self::Symlink => LinkMode::Symlink,
            Self::Copy => LinkMode::Copy,
        }
    }
}

impl fmt::Display for LinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_mode().fmt(f)
    }
}

fn try_one(src: &Path, dst: &Path, kind: LinkKind) -> io::Result<()> {
    match kind {
        LinkKind::Reflink => reflink_copy::reflink(src, dst),
        LinkKind::Hardlink => std::fs::hard_link(src, dst),
        LinkKind::Symlink => symlink(src, dst),
        LinkKind::Copy => std::fs::copy(src, dst).map(|_| ()),
    }
}

#[cfg(unix)]
fn symlink(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn symlink(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(src, dst)
}

fn chain(mode: LinkMode, allow_symlink: bool) -> &'static [LinkKind] {
    use LinkKind::*;
    match mode {
        LinkMode::Auto if allow_symlink => &[Reflink, Hardlink, Symlink, Copy],
        LinkMode::Auto => &[Reflink, Hardlink, Copy],
        LinkMode::Reflink => &[Reflink, Copy],
        LinkMode::Hardlink => &[Hardlink, Copy],
        LinkMode::Symlink if allow_symlink => &[Symlink, Copy],
        LinkMode::Symlink => &[Copy],
        LinkMode::Copy => &[Copy],
    }
}

pub fn place(src: &Path, dst: &Path, mode: LinkMode, allow_symlink: bool) -> io::Result<LinkKind> {
    let mut last_err = None;
    for &kind in chain(mode, allow_symlink) {
        match try_one(src, dst, kind) {
            Ok(()) => return Ok(kind),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::other("no link strategy available")))
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn sibling_tmp(path: &Path) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.rb-tmp-{}-{n}", std::process::id()))
}

/// Never writes through an inode that might already be the store's.
pub fn replace(src: &Path, dst: &Path, mode: LinkMode, allow_symlink: bool) -> io::Result<LinkKind> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = sibling_tmp(dst);
    let _ = std::fs::remove_file(&tmp);
    let kind = place(src, &tmp, mode, allow_symlink)?;
    if let Err(e) = std::fs::rename(&tmp, dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(kind)
}

pub fn probe(from_dir: &Path, to_dir: &Path) -> LinkKind {
    let _ = std::fs::create_dir_all(from_dir);
    let _ = std::fs::create_dir_all(to_dir);
    let src = sibling_tmp(&from_dir.join("probe-src"));
    if std::fs::write(&src, b"rb link probe").is_err() {
        return LinkKind::Copy;
    }
    let mut result = LinkKind::Copy;
    for &kind in chain(LinkMode::Auto, true) {
        let dst = sibling_tmp(&to_dir.join("probe-dst"));
        let ok = try_one(&src, &dst, kind).is_ok();
        let _ = std::fs::remove_file(&dst);
        if ok {
            result = kind;
            break;
        }
    }
    let _ = std::fs::remove_file(&src);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_is_atomic_and_breaks_hardlinks() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"one").unwrap();
        replace(&a, &b, LinkMode::Hardlink, false).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"one");
        let c = dir.path().join("c");
        std::fs::write(&c, b"two").unwrap();
        replace(&c, &b, LinkMode::Copy, false).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"one", "source of the old hardlink must be untouched");
        assert_eq!(std::fs::read(&b).unwrap(), b"two");
    }

    #[test]
    fn probe_finds_a_strategy() {
        let dir = tempfile::tempdir().unwrap();
        let kind = probe(&dir.path().join("x"), &dir.path().join("y"));
        assert_ne!(kind, LinkKind::Symlink);
    }
}
