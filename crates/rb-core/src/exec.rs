//! Run a unit, and move its outputs into and out of the store.

use crate::build_script::{BuildOutput, LinkArgTarget, env_name};
use crate::depinfo;
use crate::invocation::Invocation;
use crate::plan::{Ctx, Planned};
use crate::unit::{Mode, UnitGraph};
use crate::workspace::TargetKind;
use anyhow::{Context, Result, anyhow, bail};
use rb_store::{Entry, EnvInput, ExtraInputs, FileInput, LinkMode, OutputFile, Store};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const OUT_DIR_TOKEN: &str = "{OUT_DIR}";

pub struct Engine<'a> {
    pub ctx: &'a Ctx<'a>,
    pub graph: &'a UnitGraph,
    pub planned: &'a [Planned],
    pub store: Option<&'a Store>,
    /// Resolved from `auto` by probing once for this build.
    pub link_mode: LinkMode,
    pub results: Vec<OnceLock<Arc<BuildOutput>>>,
    pub jobserver: jobserver::Client,
}

/// One unit in `target/rb/.state/units.json` and in the store.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Record {
    pub outputs: Vec<PathBuf>,
    pub inputs: ExtraInputs,
    /// Files of this package the unit read, relative to the package root. Next build, that's its identity.
    #[serde(default)]
    pub sources: Vec<String>,
    /// The script said `rerun-if-*`, so the rest of the package doesn't trigger a rerun.
    #[serde(default)]
    pub rerun_declared: bool,
    /// Build-script output with `{OUT_DIR}`, `{TARGET}`, `{WS}` still in it.
    pub payload: Option<BuildOutput>,
    pub diagnostics: Vec<String>,
}

pub enum Signal {
    MetaReady,
}

pub struct Outcome {
    pub record: Record,
    pub duration: Duration,
    /// Someone else finished it while we waited on the unit lock.
    pub restored: bool,
    /// Held until `ingest`, so another build doesn't do the same work. None if there's nothing to store.
    pub lock: Option<rb_store::FileLock>,
}

impl<'a> Engine<'a> {
    pub fn target_message(&self, ui: usize) -> serde_json::Value {
        let u = &self.graph.units[ui];
        let t = u.target(self.ctx.ws);
        let kind = if t.is_proc_macro() {
            vec!["proc-macro".to_owned()]
        } else {
            vec![
                match t.kind {
                    crate::workspace::TargetKind::Lib => "lib",
                    crate::workspace::TargetKind::Bin => "bin",
                    crate::workspace::TargetKind::Example => "example",
                    crate::workspace::TargetKind::Test => "test",
                    crate::workspace::TargetKind::Bench => "bench",
                    crate::workspace::TargetKind::BuildScript => "custom-build",
                }
                .to_owned(),
            ]
        };
        serde_json::json!({
            "kind": kind,
            "crate_types": t.crate_types,
            "name": t.name,
            "src_path": t.src_path,
            "edition": t.edition,
            "doc": t.doc,
            "doctest": t.doctest,
            "test": t.test,
        })
    }

    pub fn label(&self, ui: usize) -> String {
        let u = &self.graph.units[ui];
        self.ctx.ws.pkgs[u.pkg].display()
    }

    fn short(&self, ui: usize) -> String {
        let u = &self.graph.units[ui];
        let p = &self.ctx.ws.pkgs[u.pkg];
        match &self.planned[ui].bin_name {
            Some(b) => format!("`{}` (bin \"{b}\")", p.name),
            None => format!("`{}` {}", p.name, self.planned[ui].descr),
        }
    }

    pub fn expand(&self, s: &str, out_dir: Option<&Path>) -> String {
        let mut s = self.ctx.expand(s);
        if let Some(o) = out_dir {
            s = s.replace(OUT_DIR_TOKEN, &o.to_string_lossy());
        }
        s
    }

    fn normalize(&self, s: &str, out_dir: Option<&Path>) -> String {
        let s = match out_dir {
            Some(o) => s.replace(&*o.to_string_lossy(), OUT_DIR_TOKEN),
            None => s.to_owned(),
        };
        self.ctx.norm.apply(&s)
    }

    pub fn inputs_hold(&self, ui: usize, inputs: &ExtraInputs) -> bool {
        let inv = &self.planned[ui].inv;
        for e in &inputs.env {
            let now = inv.get_env(&e.name).map(str::to_owned).or_else(|| std::env::var(&e.name).ok());
            if now != e.value {
                return false;
            }
        }
        for f in &inputs.files {
            let path = PathBuf::from(self.expand(&f.path, None));
            match self.ctx.hasher.file_hash(&path) {
                Ok(h) if h == f.hash => {}
                _ => return false,
            }
        }
        true
    }

    pub fn build_output_for(&self, ui: usize) -> Option<Arc<BuildOutput>> {
        self.results[ui].get().cloned()
    }

    pub fn set_build_output(&self, ui: usize, payload: &BuildOutput) {
        let out_dir = self.planned[ui].out_dir_path();
        let expanded = payload.map_strings(|s| self.expand(s, Some(&out_dir)));
        let _ = self.results[ui].set(Arc::new(expanded));
    }

    pub fn materialize(&self, ui: usize, entry: &Entry) -> Result<Record> {
        let store = self.store.context("store disabled")?;
        let p = &self.planned[ui];
        let is_run = self.graph.units[ui].mode == Mode::RunCustomBuild;
        let base = if is_run { p.out_dir_path() } else { p.out_dir.clone() };
        if is_run {
            let _ = std::fs::remove_dir_all(&p.out_dir);
        }
        std::fs::create_dir_all(&base)?;
        for d in &entry.dirs {
            std::fs::create_dir_all(base.join(d))?;
        }
        let mut outputs = Vec::new();
        for o in &entry.outputs {
            let dst = base.join(&o.name);
            if o.templated {
                let text = String::from_utf8(store.read_blob(&o.blob)?)?;
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dst, text.replace(OUT_DIR_TOKEN, &base.to_string_lossy()))?;
            } else {
                store.materialize(&o.blob, &dst, self.link_mode, o.exec)?;
            }
            outputs.push(dst);
        }
        for (rel, target) in &entry.symlinks {
            let dst = base.join(rel);
            let _ = std::fs::remove_file(&dst);
            #[cfg(unix)]
            std::os::unix::fs::symlink(self.expand(target, Some(&base)), &dst)?;
        }
        let payload: Option<BuildOutput> = match &entry.payload {
            Some(v) => Some(serde_json::from_value(v.clone())?),
            None => None,
        };
        if let Some(pl) = &payload {
            self.set_build_output(ui, pl);
        }
        Ok(Record {
            outputs,
            inputs: entry.inputs.clone(),
            payload,
            diagnostics: entry.diagnostics.clone(),
            ..Default::default()
        })
    }

    pub fn lookup(&self, ui: usize) -> Result<Option<Entry>> {
        let Some(store) = self.store else { return Ok(None) };
        if !self.planned[ui].cacheable {
            return Ok(None);
        }
        let Some(mut m) = store.load_manifest(&self.planned[ui].key)? else {
            return Ok(None);
        };
        let Some(idx) = m
            .entries
            .iter()
            .position(|e| e.outputs.iter().all(|o| store.has_blob(&o.blob)) && self.inputs_hold(ui, &e.inputs))
        else {
            return Ok(None);
        };
        let now = rb_store::now_secs();
        if now.saturating_sub(m.entries[idx].last_used) > 3600 {
            m.entries[idx].last_used = now;
            let _ = store.save_manifest(&m);
        }
        Ok(Some(m.entries[idx].clone()))
    }

    /// Fresh units skip `lookup`, so mark them used or the cap evicts daily deps before stale experiments.
    pub fn touch_store(&self, ui: usize) {
        let Some(store) = self.store else { return };
        let Ok(Some(mut m)) = store.load_manifest(&self.planned[ui].key) else {
            return;
        };
        let now = rb_store::now_secs();
        let mut changed = false;
        for e in m.entries.iter_mut().filter(|e| now.saturating_sub(e.last_used) > 3600) {
            e.last_used = now;
            changed = true;
        }
        if changed {
            let _ = store.save_manifest(&m);
        }
    }

    fn ingest(&self, ui: usize, record: &Record, duration: Duration) -> Result<()> {
        let Some(store) = self.store else { return Ok(()) };
        let p = &self.planned[ui];
        if !p.cacheable {
            return Ok(());
        }
        let is_run = self.graph.units[ui].mode == Mode::RunCustomBuild;
        let mut entry = Entry {
            label: format!("{} {}", self.label(ui), p.descr),
            created: rb_store::now_secs(),
            last_used: rb_store::now_secs(),
            inputs: record.inputs.clone(),
            diagnostics: record.diagnostics.clone(),
            duration_ms: duration.as_millis() as u64,
            ..Default::default()
        };
        if is_run {
            let base = p.out_dir_path();
            let base_s = base.to_string_lossy().into_owned();
            // A generator may also write the symlink-resolved spelling of OUT_DIR.
            let base_c = std::fs::canonicalize(&base)
                .ok()
                .map(|c| c.to_string_lossy().into_owned())
                .filter(|c| *c != base_s);
            for e in walkdir::WalkDir::new(&base).min_depth(1).into_iter().filter_map(|e| e.ok()) {
                let rel = e.path().strip_prefix(&base).unwrap().to_string_lossy().into_owned();
                let ft = e.file_type();
                if ft.is_dir() {
                    entry.dirs.push(rel);
                } else if ft.is_symlink() {
                    let target = std::fs::read_link(e.path())?;
                    entry.symlinks.push((rel, self.normalize(&target.to_string_lossy(), Some(&base))));
                } else if ft.is_file() {
                    // Rewriting only works on text. Objects and archives stay as they are. Big files aren't worth scanning.
                    let text = (e.metadata().map(|m| m.len()).unwrap_or(u64::MAX) <= 32 << 20)
                        .then(|| std::fs::read(e.path()))
                        .transpose()?
                        .and_then(|b| String::from_utf8(b).ok());
                    let templated = text
                        .as_deref()
                        .is_some_and(|t| t.contains(&base_s) || base_c.as_ref().is_some_and(|c| t.contains(c.as_str())));
                    let (blob, size, exec) = if templated {
                        let mut text = text.unwrap();
                        if let Some(c) = &base_c {
                            text = text.replace(c.as_str(), OUT_DIR_TOKEN);
                        }
                        let text = text.replace(&base_s, OUT_DIR_TOKEN);
                        (store.ingest_bytes(text.as_bytes(), false)?, text.len() as u64, false)
                    } else {
                        store.ingest_file(e.path())?
                    };
                    entry.outputs.push(OutputFile {
                        name: rel,
                        blob,
                        exec,
                        size,
                        templated,
                    });
                }
            }
            entry.payload = record.payload.as_ref().map(serde_json::to_value).transpose()?;
        } else {
            for path in &record.outputs {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let (blob, size, exec) = store.ingest_file(path)?;
                entry.outputs.push(OutputFile {
                    name,
                    blob,
                    exec,
                    size,
                    templated: false,
                });
            }
        }
        let mut m = store.load_manifest(&p.key)?.unwrap_or_else(|| rb_store::UnitManifest {
            key: p.key.clone(),
            entries: vec![],
        });
        m.upsert(entry);
        store.save_manifest(&m)?;
        Ok(())
    }

    const DEFER_INGEST_BYTES: u64 = 8 << 20;

    /// Big outputs get hashed by `rb __compact` after this process exits.
    pub fn defer_ingest(&self, ui: usize, record: &Record, duration: Duration) -> bool {
        let Some(store) = self.store else { return false };
        let p = &self.planned[ui];
        if !p.cacheable || self.graph.units[ui].mode == Mode::RunCustomBuild {
            return false;
        }
        let bytes = record
            .outputs
            .iter()
            .try_fold(0u64, |n, path| std::fs::metadata(path).map(|m| n.saturating_add(m.len())));
        if bytes.is_ok_and(|n| n < Self::DEFER_INGEST_BYTES) {
            return false;
        }
        let job = serde_json::json!({
            "key": p.key,
            "label": format!("{} {}", self.label(ui), p.descr),
            "duration_ms": duration.as_millis() as u64,
            "outputs": record.outputs,
            "inputs": record.inputs,
            "diagnostics": record.diagnostics,
        });
        store.write_queue("ingest", &serde_json::to_vec(&job).unwrap_or_default()).is_ok()
    }

    /// `meta` fires when a pipelined rlib's metadata is ready. Storing waits, so dependents don't.
    pub fn execute(&self, ui: usize, threads: Option<usize>, meta: &dyn Fn()) -> Result<Outcome> {
        let lock = match (self.store, self.planned[ui].cacheable) {
            (Some(store), true) => Some(store.unit_lock(&self.planned[ui].key)?),
            _ => None,
        };
        // Another worktree may have produced this while we waited.
        if let Some(entry) = self.lookup(ui)? {
            let record = self.materialize(ui, &entry)?;
            return Ok(Outcome {
                record,
                duration: Duration::ZERO,
                restored: true,
                lock: None,
            });
        }
        let start = Instant::now();
        let record = match self.graph.units[ui].mode {
            Mode::RunCustomBuild => self.run_build_script(ui)?,
            _ => self.compile(ui, threads, meta)?,
        };
        let duration = start.elapsed();
        if self.graph.units[ui].mode == Mode::RunCustomBuild {
            self.remember_prediction(ui, &record);
        }
        Ok(Outcome {
            record,
            duration,
            restored: false,
            lock,
        })
    }

    pub fn store_outputs(&self, ui: usize, record: &Record, duration: Duration, lock: Option<rb_store::FileLock>) -> Result<()> {
        let r = self
            .ingest(ui, record, duration)
            .with_context(|| format!("failed to store {}", self.short(ui)));
        drop(lock);
        r
    }

    fn output_names(&self, ui: usize) -> Vec<String> {
        let p = &self.planned[ui];
        let u = &self.graph.units[ui];
        crate::plan::artifact_names(&self.ctx.kind(u.kind).info, &p.inv, &p.crate_name, p.name16(), p.emits_meta)
    }

    fn compile(&self, ui: usize, threads: Option<usize>, meta: &dyn Fn()) -> Result<Record> {
        let ctx = self.ctx;
        let u = &self.graph.units[ui];
        let p = &self.planned[ui];
        let pkg = &ctx.ws.pkgs[u.pkg];
        let t = &pkg.targets[u.target];
        let is_doc = u.mode == Mode::Doc;
        let mut inv: Invocation = p.inv.clone();
        if let Some(r) = p.own_run {
            let out = self
                .build_output_for(r)
                .ok_or_else(|| anyhow!("missing build script output for {}", pkg.name))?;
            for c in &out.cfgs {
                inv.arg_unhashed("--cfg").arg_unhashed(c);
            }
            for c in &out.check_cfgs {
                inv.arg_unhashed("--check-cfg").arg_unhashed(c);
            }
            for (k, v) in &out.env {
                inv.env_unhashed(k, v);
            }
            let pass_l = (t.kind == TargetKind::Lib || pkg.lib().is_none()) && !is_doc;
            if pass_l {
                for l in &out.link_libs {
                    inv.arg_unhashed("-l").arg_unhashed(l);
                }
            }
            let is_cdylib = t.crate_types.iter().any(|c| c == "cdylib");
            let test = u.mode.is_test();
            for (which, arg) in out.link_args.iter().filter(|_| !is_doc) {
                let applies = match which {
                    LinkArgTarget::All => t.is_executable() || is_cdylib || test,
                    LinkArgTarget::Bins => t.kind == TargetKind::Bin,
                    LinkArgTarget::Bin(b) => t.kind == TargetKind::Bin && b == &t.name,
                    LinkArgTarget::Cdylib => is_cdylib && !test,
                    LinkArgTarget::Tests => t.kind == TargetKind::Test,
                    LinkArgTarget::Examples => t.kind == TargetKind::Example,
                    LinkArgTarget::Benches => t.kind == TargetKind::Bench,
                };
                if applies {
                    inv.arg_unhashed("-C").arg_unhashed(format!("link-arg={arg}"));
                }
            }
        }
        for &r in p.to_link.iter().filter(|_| !is_doc) {
            if let Some(out) = self.build_output_for(r) {
                for s in &out.link_search {
                    inv.arg_unhashed("-L").arg_unhashed(s);
                }
            }
        }
        if let Some(n) = threads {
            inv.arg_unhashed(format!("-Zthreads={n}"));
        }

        // Don't write through an inode the store might share. Unlink known artifacts only.
        // Scanning deps/ (thousands of files) on every unit used to hold a job slot.
        if is_doc {
            // rustdoc owns the shared doc tree.
        } else if t.kind == TargetKind::BuildScript {
            let _ = std::fs::remove_dir_all(&p.out_dir);
        } else {
            for name in self.output_names(ui) {
                let _ = std::fs::remove_file(p.out_dir.join(name));
            }
        }
        std::fs::create_dir_all(&p.out_dir)?;
        // A variant that comes back gets its archived incremental cache back.
        if let Some(dir) = inv.args.iter().find_map(|(a, _)| a.strip_prefix("incremental=")).map(PathBuf::from)
            && !dir.exists()
            && let (Some(parent), Some(name)) = (dir.parent().and_then(|p| p.parent()), dir.file_name())
        {
            let archive_dir = parent.join(crate::build::INCREMENTAL_ARCHIVE);
            let pending = archive_dir.join(format!("{}.pending", name.to_string_lossy()));
            if pending.is_dir() {
                let _ = std::fs::rename(&pending, &dir);
            } else {
                let archive = archive_dir.join(format!("{}.tar.zst", name.to_string_lossy()));
                if archive.is_file() && rb_store::archive::unpack(&archive, &dir).is_err() {
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }

        if ctx.shell.is_verbose() {
            ctx.shell.status(
                "Running",
                format!("`{}`", inv.display(ctx.shell.verbosity() >= crate::shell::Verbosity::VeryVerbose)),
            );
        }
        let mut cmd = inv.command();
        self.jobserver.configure(&mut cmd);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("could not execute process `{}`", inv.program.display()))?;
        let mut stdout = child.stdout.take().unwrap();
        let out_thread = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stdout.read_to_string(&mut s);
            s
        });
        let local = pkg.source.is_local();
        let (mut warnings, mut errors) = (0usize, 0usize);
        let mut diagnostics = Vec::new();
        for line in BufReader::new(child.stderr.take().unwrap()).lines() {
            let line = line?;
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
                ctx.shell.raw(&line);
                continue;
            };
            match msg["$message_type"].as_str() {
                Some("artifact") => {
                    if msg["emit"] == "metadata" {
                        meta();
                    }
                }
                Some("diagnostic") => {
                    let level = msg["level"].as_str().unwrap_or("");
                    let text = msg["message"].as_str().unwrap_or("");
                    let summary =
                        text.starts_with("aborting due to") || text.ends_with("warning emitted") || text.ends_with("warnings emitted");
                    if summary {
                        continue;
                    }
                    match level {
                        "warning" => warnings += 1,
                        "error" => errors += 1,
                        _ => {}
                    }
                    if ctx.shell.format().is_json() {
                        ctx.shell.json_line(&serde_json::json!({
                            "reason": "compiler-message",
                            "package_id": pkg.id,
                            "manifest_path": pkg.manifest_path,
                            "target": self.target_message(ui),
                            "message": &msg,
                        }));
                    }
                    if let Some(rendered) = msg["rendered"].as_str()
                        && ctx.shell.format().render()
                    {
                        ctx.shell.raw(rendered);
                        if local && level == "warning" {
                            diagnostics.push(rendered.to_owned());
                        }
                    }
                }
                _ => {}
            }
        }
        let status = child.wait()?;
        let stdout_text = out_thread.join().unwrap_or_default();
        if !stdout_text.is_empty() {
            print!("{stdout_text}");
        }
        if !status.success() {
            if warnings > 0 && local {
                ctx.shell.warn(format!(
                    "{} generated {warnings} warning{}",
                    self.short(ui),
                    if warnings == 1 { "" } else { "s" }
                ));
            }
            let why = match errors {
                0 => String::new(),
                1 => " due to 1 previous error".into(),
                n => format!(" due to {n} previous errors"),
            };
            bail!("could not compile {}{why}", self.short(ui));
        }
        if warnings > 0 && local {
            ctx.shell.warn(format!(
                "{} generated {warnings} warning{}",
                self.short(ui),
                if warnings == 1 { "" } else { "s" }
            ));
        }

        let mut outputs = Vec::new();
        if is_doc {
            let index = p.out_dir.join(&p.crate_name).join("index.html");
            if index.is_file() {
                outputs.push(index);
            }
            return Ok(Record {
                outputs,
                diagnostics,
                ..Default::default()
            });
        }
        for name in self.output_names(ui) {
            let path = p.out_dir.join(name);
            if path.is_file() {
                outputs.push(path);
            }
        }
        outputs.sort();
        Ok(Record {
            outputs,
            diagnostics,
            ..Default::default()
        })
    }

    /// Dep-info, after dependents are released. Cargo doesn't hold a job token for this either.
    pub fn attach_discovered(&self, ui: usize, record: &mut Record) -> Result<()> {
        let (inputs, sources) = self.discovered_inputs(ui, &self.planned[ui].inv)?;
        record.inputs = inputs;
        record.sources = sources;
        self.remember_prediction(ui, record);
        Ok(())
    }

    fn remember_prediction(&self, ui: usize, record: &Record) {
        if self.ctx.ws.pkgs[self.graph.units[ui].pkg].source.is_local() || !record.inputs.is_empty() {
            let pr = crate::plan::Prediction {
                env: record.inputs.env.iter().map(|e| e.name.clone()).collect(),
                files: record.inputs.files.iter().map(|f| f.path.clone()).collect(),
                sources: record.sources.iter().cloned().collect(),
                rerun_declared: Some(record.rerun_declared),
            };
            self.ctx.predictions.record(&self.planned[ui].name, &pr);
        }
    }

    /// Outside the package: checked on every lookup. Inside: the unit's identity next build.
    fn discovered_inputs(&self, ui: usize, inv: &Invocation) -> Result<(ExtraInputs, Vec<String>)> {
        let ctx = self.ctx;
        let u = &self.graph.units[ui];
        let p = &self.planned[ui];
        let pkg = &ctx.ws.pkgs[u.pkg];
        let dep_file = p.out_dir.join(format!("{}-{}.d", p.crate_name, p.name16()));
        let Ok(info) = depinfo::parse(&dep_file, &inv.cwd) else {
            return Ok(Default::default());
        };
        let own_out = p.own_run.map(|r| self.planned[r].out_dir_path());
        let generated = with_canonical([Some(ctx.layout.target_dir.clone()), Some(ctx.rustc.sysroot.clone()), own_out]);
        let pkg_roots = with_canonical([Some(pkg.root.clone())]);
        let mut files = Vec::new();
        let mut sources = Vec::new();
        for f in info.files {
            let f = lexical_clean(&f);
            if is_under(&f, &generated) {
                continue;
            }
            if let Some(rel) = relative_to(&f, &pkg_roots) {
                sources.push(rel);
                continue;
            }
            let hash = ctx.hasher.file_hash(&f)?;
            files.push(FileInput {
                path: ctx.norm.apply(&f.to_string_lossy()),
                hash,
            });
        }
        let dynamic: Vec<&str> = p
            .own_run
            .and_then(|r| self.results[r].get())
            .map(|o| o.env.iter().map(|(k, _)| k.as_str()).collect())
            .unwrap_or_default();
        let mut env = Vec::new();
        for (name, value) in info.env {
            // The invocation hash already covers values it set. Unhashed package metadata is tracked here if a crate reads it.
            let derived = matches!(
                name.as_str(),
                "CARGO_CRATE_NAME" | "CARGO_BIN_NAME" | "CARGO_PRIMARY_PACKAGE" | "OUT_DIR" | "CARGO_MANIFEST_LINKS"
            ) || dynamic.contains(&name.as_str());
            let hashed = inv.env.get(&name).is_some_and(|(_, h)| *h);
            let path_like = matches!(name.as_str(), "CARGO_MANIFEST_DIR" | "CARGO_MANIFEST_PATH");
            if derived || (hashed && !path_like) {
                continue;
            }
            env.push(EnvInput { name, value });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files.dedup();
        env.sort_by(|a, b| a.name.cmp(&b.name));
        env.dedup();
        sources.sort();
        sources.dedup();
        Ok((ExtraInputs { files, env }, sources))
    }

    fn run_build_script(&self, ui: usize) -> Result<Record> {
        let ctx = self.ctx;
        let u = &self.graph.units[ui];
        let p = &self.planned[ui];
        let pkg = &ctx.ws.pkgs[u.pkg];
        let mut inv = p.inv.clone();
        for d in u.deps.iter().filter(|d| self.graph.units[d.unit].mode == Mode::RunCustomBuild) {
            let dep = &self.graph.units[d.unit];
            let links = ctx.ws.pkgs[dep.pkg].links.as_deref().unwrap_or_default();
            if let Some(out) = self.build_output_for(d.unit) {
                for (k, v) in &out.metadata {
                    inv.env_unhashed(format!("DEP_{}_{}", env_name(links), env_name(k)), v);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&p.out_dir);
        let out_dir = p.out_dir_path();
        std::fs::create_dir_all(&out_dir)?;
        if ctx.shell.is_verbose() {
            ctx.shell.status(
                "Running",
                format!("`{}`", inv.display(ctx.shell.verbosity() >= crate::shell::Verbosity::VeryVerbose)),
            );
        }
        let mut cmd = inv.command();
        self.jobserver.configure(&mut cmd);
        let output = cmd
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run custom build command for `{}`", pkg.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        std::fs::write(p.out_dir.join("output"), &stdout)?;
        std::fs::write(p.out_dir.join("stderr"), &stderr)?;
        let parsed = BuildOutput::parse(&stdout);
        let local = pkg.source.is_local();
        if local || ctx.shell.verbosity() >= crate::shell::Verbosity::VeryVerbose {
            for w in &parsed.warnings {
                ctx.shell.warn(format!("{}@{}: {w}", pkg.name, pkg.version));
            }
        }
        if !output.status.success() || !parsed.errors.is_empty() {
            let mut msg = format!(
                "failed to run custom build command for `{}`\n\nCaused by:\n  process didn't exit successfully: `{}` ({})",
                pkg.display(),
                inv.program.display(),
                output.status
            );
            for e in &parsed.errors {
                msg.push_str(&format!("\n  error: {e}"));
            }
            msg.push_str("\n  --- stdout\n");
            msg.push_str(&indent(&stdout));
            msg.push_str("\n  --- stderr\n");
            msg.push_str(&indent(&stderr));
            bail!(msg);
        }

        let mut inputs = ExtraInputs::default();
        for var in &parsed.rerun_if_env_changed {
            let value = inv.get_env(var).map(str::to_owned).or_else(|| std::env::var(var).ok());
            inputs.env.push(EnvInput { name: var.clone(), value });
        }
        let generated = with_canonical([Some(ctx.layout.target_dir.clone())]);
        let pkg_roots = with_canonical([Some(pkg.root.clone())]);
        let mut sources = Vec::new();
        for path in &parsed.rerun_if_changed {
            let abs = lexical_clean(&pkg.root.join(path));
            if is_under(&abs, &generated) {
                continue;
            }
            if let Some(rel) = relative_to(&abs, &pkg_roots) {
                sources.push(rel);
                continue;
            }
            let hash = ctx.hasher.file_hash(&abs)?;
            inputs.files.push(FileInput {
                path: ctx.norm.apply(&abs.to_string_lossy()),
                hash,
            });
        }
        sources.sort();
        sources.dedup();
        // Once a script says what it depends on, only that triggers a rerun.
        let rerun_declared = !parsed.rerun_if_changed.is_empty() || !parsed.rerun_if_env_changed.is_empty();
        let normalized = parsed.map_strings(|s| self.normalize(s, Some(&out_dir)));
        self.set_build_output(ui, &normalized);
        Ok(Record {
            inputs,
            sources,
            rerun_declared,
            payload: Some(normalized),
            ..Default::default()
        })
    }
}

fn relative_to(path: &Path, roots: &[PathBuf]) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok();
    roots
        .iter()
        .find_map(|r| {
            path.strip_prefix(r)
                .ok()
                .or_else(|| canonical.as_ref().and_then(|c| c.strip_prefix(r).ok()))
        })
        .map(|rel| rel.to_string_lossy().into_owned())
}

fn with_canonical<const N: usize>(roots: [Option<PathBuf>; N]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for r in roots.into_iter().flatten() {
        if let Ok(c) = std::fs::canonicalize(&r)
            && c != r
        {
            out.push(c);
        }
        out.push(r);
    }
    out
}

fn is_under(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| path.starts_with(r)) || std::fs::canonicalize(path).is_ok_and(|c| roots.iter().any(|r| c.starts_with(r)))
}

fn lexical_clean(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Hash outputs a build deferred, and write their manifests. False if a build holds the store lock; the queue waits.
pub fn finish_deferred_ingests(store: &rb_store::Store) -> bool {
    for (path, bytes) in store.ingest_jobs() {
        if store.try_gc_lock().ok().flatten().is_none() {
            return false;
        }
        let Ok(job) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let Some(key) = job["key"].as_str() else { continue };
        let mut entry = rb_store::Entry {
            label: job["label"].as_str().unwrap_or("").to_owned(),
            created: rb_store::now_secs(),
            last_used: rb_store::now_secs(),
            duration_ms: job["duration_ms"].as_u64().unwrap_or(0),
            inputs: serde_json::from_value(job["inputs"].clone()).unwrap_or_default(),
            diagnostics: serde_json::from_value(job["diagnostics"].clone()).unwrap_or_default(),
            ..Default::default()
        };
        let outputs: Vec<PathBuf> = serde_json::from_value(job["outputs"].clone()).unwrap_or_default();
        let mut ok = true;
        for output in &outputs {
            if store.try_gc_lock().ok().flatten().is_none() {
                return false;
            }
            match store.ingest_file(output) {
                Ok((blob, size, exec)) => entry.outputs.push(rb_store::OutputFile {
                    name: output.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    blob,
                    exec,
                    size,
                    templated: false,
                }),
                Err(_) => ok = false,
            }
        }
        if ok && !entry.outputs.is_empty() {
            let mut m = store.load_manifest(key).ok().flatten().unwrap_or_else(|| rb_store::UnitManifest {
                key: key.to_owned(),
                entries: vec![],
            });
            m.upsert(entry);
            let _ = store.save_manifest(&m);
        }
        let _ = std::fs::remove_file(path);
    }
    true
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
}
