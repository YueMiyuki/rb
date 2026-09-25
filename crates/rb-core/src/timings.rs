//! Per-unit durations, for the critical path and `--timings`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Timings {
    path: PathBuf,
    map: HashMap<String, u64>,
    dirty: bool,
}

impl Timings {
    pub fn load(home: &Path) -> Self {
        let path = home.join("timings.json");
        let map = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, map, dirty: false }
    }

    pub fn get(&self, label: &str) -> Option<u64> {
        self.map.get(label).copied()
    }

    /// Exponential moving average so one noisy build does not dominate
    pub fn record(&mut self, label: &str, ms: u64) {
        let v = match self.map.get(label) {
            Some(&old) => (old + ms) / 2,
            None => ms,
        };
        self.map.insert(label.to_owned(), v);
        self.dirty = true;
    }

    pub fn save(&self) {
        if !self.dirty {
            return;
        }
        if let Some(p) = self.path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let tmp = self.path.with_extension("tmp");
        if let Ok(b) = serde_json::to_vec(&self.map)
            && std::fs::write(&tmp, b).is_ok()
        {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

pub struct TimingRow {
    pub label: String,
    pub ms: u64,
}

pub fn print_summary(rows: &mut Vec<TimingRow>, wall_ms: u64, jobs: usize) {
    let notes: Vec<String> = rows
        .iter()
        .filter(|r| r.ms == 0 && r.label.starts_with('('))
        .map(|r| r.label.clone())
        .collect();
    rows.retain(|r| !(r.ms == 0 && r.label.starts_with('(')));
    rows.sort_by_key(|r| std::cmp::Reverse(r.ms));
    let cpu: u64 = rows.iter().map(|r| r.ms).sum();
    let par = if wall_ms > 0 { cpu as f64 / wall_ms as f64 } else { 0.0 };
    eprintln!();
    eprintln!(
        "Timings: {} units executed, {:.2}s wall, {:.2}s summed unit time, average parallelism {par:.1}x of {jobs} jobs",
        rows.len(),
        wall_ms as f64 / 1000.0,
        cpu as f64 / 1000.0
    );
    for n in notes {
        eprintln!("  {n}");
    }
    for r in rows.iter().take(15) {
        eprintln!("  {:>8.2}s  {}", r.ms as f64 / 1000.0, r.label);
    }
}

pub fn write_html(target_dir: &Path, rows: &[TimingRow], wall_ms: u64) {
    let dir = target_dir.join("cargo-timings");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let max = rows.iter().map(|r| r.ms).max().unwrap_or(1).max(1);
    let mut body = String::new();
    for r in rows {
        let width = (r.ms as f64 / max as f64) * 100.0;
        let label = html_escape(&r.label);
        body.push_str(&format!(
            "<tr><td class=\"t\">{:.2}s</td><td><div class=\"bar\" style=\"width:{width:.1}%\"></div></td><td>{label}</td></tr>\n",
            r.ms as f64 / 1000.0
        ));
    }
    let page = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>rb timings</title><style>\
body{{font:14px sans-serif;margin:24px;background:#111;color:#eee}}\
table{{border-collapse:collapse;width:100%}} td{{padding:4px 8px;vertical-align:middle}}\
.t{{text-align:right;font-variant-numeric:tabular-nums;width:6em}}\
.bar{{height:12px;background:#3dd68c;border-radius:2px}}\
</style></head><body><h1>Build timings</h1><p>Wall {:.2}s, {} units.</p><table>{body}</table></body></html>",
        wall_ms as f64 / 1000.0,
        rows.len()
    );
    let path = dir.join("cargo-timings.html");
    if std::fs::write(&path, page).is_ok() {
        eprintln!("  report: {}", path.display());
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
