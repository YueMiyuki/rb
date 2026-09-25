//! Git deps via the `git` CLI. One bare db per URL, checkouts under `~/.rb/git`.

use crate::manifest::GitRef;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Git {
    root: PathBuf,
    offline: bool,
}

fn short_name(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git");
    format!("{last}-{}", &blake3::hash(url.as_bytes()).to_hex()[..12])
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("failed to run `git` (required for git dependencies)")?;
    if !out.status.success() {
        bail!(
            "`git {}` failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim_end()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

impl Git {
    pub fn new(root: PathBuf, offline: bool) -> Self {
        Self { root, offline }
    }

    fn db(&self, url: &str) -> Result<PathBuf> {
        let db = self.root.join("db").join(short_name(url));
        if !db.join("HEAD").is_file() {
            std::fs::create_dir_all(&db)?;
            let out = Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&db)
                .output()
                .context("failed to run `git init`")?;
            if !out.status.success() {
                bail!("`git init` failed: {}", String::from_utf8_lossy(&out.stderr));
            }
        }
        Ok(db)
    }

    fn fetch(&self, db: &Path, url: &str, extra: &[&str]) -> Result<()> {
        if self.offline {
            bail!("cannot fetch {url} while offline");
        }
        let mut args = vec!["-c", "protocol.file.allow=always", "fetch", "--force", "--quiet", url];
        if extra.is_empty() {
            args.extend([
                "+refs/heads/*:refs/remotes/origin/*",
                "+HEAD:refs/remotes/origin/HEAD",
                "+refs/tags/*:refs/tags/*",
            ]);
        } else {
            args.extend(extra);
        }
        git(db, &args).map(|_| ())
    }

    /// Fetch if we don't already have it.
    pub fn resolve(&self, url: &str, reference: &GitRef) -> Result<String> {
        let db = self.db(url)?;
        let rev = match reference {
            GitRef::DefaultBranch => "refs/remotes/origin/HEAD".to_owned(),
            GitRef::Branch(b) => format!("refs/remotes/origin/{b}"),
            GitRef::Tag(t) => format!("refs/tags/{t}"),
            GitRef::Rev(r) => r.clone(),
        };
        let lookup = |db: &Path| git(db, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")]);
        // Branches move, so refresh them unless we're offline. Tags and revs are fetched once.
        let refresh = matches!(reference, GitRef::DefaultBranch | GitRef::Branch(_)) && !self.offline;
        if !refresh && let Ok(c) = lookup(&db) {
            return Ok(c);
        }
        self.fetch(&db, url, &[])?;
        if let Ok(c) = lookup(&db) {
            return Ok(c);
        }
        if let GitRef::Rev(r) = reference {
            self.fetch(&db, url, &[r.as_str()])?;
            if let Ok(c) = git(&db, &["rev-parse", "--verify", "--quiet", "FETCH_HEAD^{commit}"]) {
                return Ok(c);
            }
        }
        bail!("could not find `{rev}` in {url}")
    }

    pub fn checkout(&self, url: &str, commit: &str) -> Result<PathBuf> {
        let dir = self
            .root
            .join("checkouts")
            .join(short_name(url))
            .join(&commit[..commit.len().min(12)]);
        if dir.join(".rb-ok").is_file() {
            return Ok(dir);
        }
        let db = self.db(url)?;
        if git(&db, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_err() {
            self.fetch(&db, url, &[])?;
            if git(&db, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_err() {
                self.fetch(&db, url, &[commit])?;
            }
        }
        if git(&db, &["cat-file", "-e", &format!("{commit}:.gitmodules")]).is_ok() {
            return self.checkout_with_submodules(&db, url, commit, &dir);
        }
        let staging = dir.with_extension(format!("staging{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(&db)
            .args(["archive", "--format=tar", commit])
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to run `git archive`")?;
        tar::Archive::new(child.stdout.take().unwrap())
            .unpack(&staging)
            .context("failed to unpack git archive")?;
        if !child.wait()?.success() {
            bail!("`git archive {commit}` failed for {url}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&staging, &dir)?;
        std::fs::write(dir.join(".rb-ok"), commit)?;
        Ok(dir)
    }

    /// `git archive` drops submodules. Check out a worktree and init them.
    fn checkout_with_submodules(&self, db: &Path, url: &str, commit: &str, dir: &Path) -> Result<PathBuf> {
        let staging = dir.with_extension(format!("staging{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        let clone = Command::new("git")
            .args(["clone", "--shared", "--quiet"])
            .arg(db)
            .arg(&staging)
            .output()
            .context("failed to clone git repo for submodule checkout")?;
        if !clone.status.success() {
            bail!("`git clone` failed for {url}: {}", String::from_utf8_lossy(&clone.stderr).trim());
        }
        let run = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(&staging).output()?;
            if !out.status.success() {
                bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(())
        };
        run(&["checkout", "--quiet", "--detach", commit])?;
        if let Err(err) = run(&["-c", "protocol.file.allow=always", "submodule", "update", "--init", "--recursive"]) {
            if self.offline {
                bail!("cannot fetch submodules of {url} while offline");
            }
            return Err(err);
        }
        let _ = std::fs::remove_dir_all(dir);
        std::fs::rename(&staging, dir)?;
        std::fs::write(dir.join(".rb-ok"), commit)?;
        Ok(dir.to_path_buf())
    }
}

pub fn find_package(root: &Path, name: &str) -> Result<PathBuf> {
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let m = dir.join("Cargo.toml");
        if let Ok(t) = crate::manifest::read_toml(&m)
            && t.get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str()) == Some(name)
        {
            return Ok(m);
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.filter_map(|e| e.ok()) {
                let fname = e.file_name();
                if e.path().is_dir() && fname != "target" && !fname.to_string_lossy().starts_with('.') {
                    stack.push(e.path());
                }
            }
        }
    }
    bail!("no package named `{name}` in {}", root.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::GitRef;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "rb")
            .env("GIT_AUTHOR_EMAIL", "rb@example.com")
            .env("GIT_COMMITTER_NAME", "rb")
            .env("GIT_COMMITTER_EMAIL", "rb@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}\n{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn checkout_includes_submodule_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let lib = root.join("lib");
        let parent = root.join("parent");
        std::fs::create_dir_all(lib.join("src")).unwrap();
        std::fs::write(lib.join("src/marker.txt"), "from-submodule\n").unwrap();
        git(&lib, &["init", "-q", "-b", "main"]);
        git(&lib, &["add", "."]);
        git(&lib, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "lib"]);
        std::fs::create_dir_all(&parent).unwrap();
        git(&parent, &["init", "-q", "-b", "main"]);
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                lib.to_str().unwrap(),
                "libs",
            ],
        );
        git(&parent, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "parent"]);
        let rev = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&parent)
            .output()
            .unwrap();
        let commit = String::from_utf8_lossy(&rev.stdout).trim().to_owned();
        let git_store = Git::new(root.join("store"), false);
        let url = parent.to_str().unwrap();
        git_store.resolve(url, &GitRef::Rev(commit.clone())).unwrap();
        let checkout = git_store.checkout(url, &commit).unwrap();
        let marker = std::fs::read_to_string(checkout.join("libs/src/marker.txt")).unwrap();
        assert_eq!(marker, "from-submodule\n");
    }
}
