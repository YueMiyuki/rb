//! Where things land under `target/`.
//!
//! ```text
//! target/<profile>/                  final artifacts, no --target
//! target/<triple>/<profile>/         final artifacts for --target
//! target/rb/<profile>/<kind>/deps/   compiled units, linked into the store
//! target/rb/<profile>/<kind>/build/  build-script binaries and OUT_DIR
//! target/rb/<profile>/<kind>/incremental/
//! target/rb/.state/                  freshness, source hashes
//! ```
//! `<kind>` is `host` or the triple.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Layout {
    pub target_dir: PathBuf,
    pub profile_dir: String,
}

impl Layout {
    pub fn new(target_dir: &Path, profile_dir: &str) -> Self {
        Self {
            target_dir: target_dir.to_owned(),
            profile_dir: profile_dir.to_owned(),
        }
    }

    pub fn rb_dir(&self) -> PathBuf {
        self.target_dir.join("rb")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.rb_dir().join(".state")
    }

    pub fn kind_dir(&self, kind_name: &str) -> PathBuf {
        self.rb_dir().join(&self.profile_dir).join(kind_name)
    }

    pub fn deps(&self, kind_name: &str) -> PathBuf {
        self.kind_dir(kind_name).join("deps")
    }

    pub fn build(&self, kind_name: &str) -> PathBuf {
        self.kind_dir(kind_name).join("build")
    }

    pub fn incremental(&self, kind_name: &str) -> PathBuf {
        self.kind_dir(kind_name).join("incremental")
    }

    pub fn artifact_dir(&self, triple: Option<&str>) -> PathBuf {
        match triple {
            Some(t) => self.target_dir.join(t).join(&self.profile_dir),
            None => self.target_dir.join(&self.profile_dir),
        }
    }

    pub fn doc_dir(&self, triple: Option<&str>) -> PathBuf {
        match triple {
            Some(t) => self.target_dir.join(t).join("doc"),
            None => self.target_dir.join("doc"),
        }
    }

    pub fn lock_path(&self) -> PathBuf {
        self.rb_dir().join(".lock")
    }
}
