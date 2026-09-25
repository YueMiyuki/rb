//! Find rustc, and provision zig or xwin when the host compiler can't target it.

pub mod binfmt;
pub mod download;
pub mod mt;
pub mod rustc_info;
pub mod target;
#[cfg(feature = "msvc")]
pub mod xwin;
pub mod zig;

pub use rustc_info::{Rustc, TargetInfo};
pub use target::{CrossToolchain, Strategy, TargetRequest, Toolchains};

/// Single-quote `s` for POSIX `sh`
pub(crate) fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:@+,".contains(&b)) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Write an executable `sh` script, only touching the file when its content changes
pub(crate) fn write_script(path: &std::path::Path, body: &str) -> anyhow::Result<()> {
    let content = format!("#!/bin/sh\n{body}\n");
    if std::fs::read_to_string(path).ok().as_deref() == Some(content.as_str()) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("rb-tmp");
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
