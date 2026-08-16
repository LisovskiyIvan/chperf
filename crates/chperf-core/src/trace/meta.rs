//! Trace-level metadata, the top-level file shape, main-thread detection and
//! directory listing helpers.

use super::TraceEvent;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Clone)]
pub struct TraceMetadata {
    #[serde(rename = "cpuThrottling", default)]
    pub cpu_throttling: Option<f64>,
    #[allow(dead_code)]
    #[serde(default)]
    pub source: Option<String>,
    #[serde(rename = "startTime", default)]
    pub start_time: Option<String>,
    #[serde(rename = "networkThrottling", default)]
    pub network_throttling: Option<String>,
    #[serde(rename = "hardwareConcurrency", default)]
    pub hardware_concurrency: Option<u32>,
    #[serde(rename = "hostDPR", default)]
    pub host_dpr: Option<f64>,
    /// Extracted from TracingStartedInBrowser (not in JSON metadata)
    #[serde(skip)]
    pub page_url: Option<String>,
}

#[derive(Deserialize)]
pub struct TraceFile {
    #[serde(rename = "traceEvents")]
    pub trace_events: Vec<TraceEvent>,
    #[serde(default)]
    pub metadata: Option<TraceMetadata>,
}

/// Metadata events (`thread_name`/`process_name`/…, cat `__metadata`) carry
/// `ts` from process start — often far before the actual session. They must be
/// excluded from time-base computations, or every "ms from trace start" window
/// lands in dead time.
pub fn is_metadata_event(e: &TraceEvent) -> bool {
    matches!(
        e.name,
        "thread_name" | "process_name" | "thread_sort_index" | "process_sort_index"
    ) || e.cat.as_deref() == Some("__metadata")
}

/// Detect main thread: first RunTask with dur > 500ms
pub fn detect_main_thread(events: &[TraceEvent]) -> u64 {
    for e in events {
        if e.name == "RunTask" && e.ph == b'X'
            && let Some(dur) = e.dur
                && dur > 500_000.0 {
                    return e.tid;
                }
    }
    // Fallback: tid with most RunTask events
    let mut counts: rustc_hash::FxHashMap<u64, usize> = rustc_hash::FxHashMap::default();
    for e in events {
        if e.name == "RunTask" && e.ph == b'X' {
            *counts.entry(e.tid).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(tid, _)| tid)
        .unwrap_or(0)
}

/// Stable stem for a trace file: strips both `.json` and `.json.gz`.
/// `Trace-20260731T180758.json.gz` -> `Trace-20260731T180758`.
pub fn trace_stem(path: &Path) -> String {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let name = name.strip_suffix(".json").unwrap_or(name);
    name.to_string()
}

/// Scan a directory for Chrome trace files (`*.json`, `*.json.gz`).
/// When both `.json` and `.json.gz` exist for the same stem, keep only the
/// `.json.gz` (smaller I/O). Sorted by name.
pub fn list_traces(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut by_stem: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let is_gz = name.ends_with(".json.gz");
        let is_plain = !is_gz && name.ends_with(".json");
        if !is_gz && !is_plain {
            continue;
        }
        let stem = trace_stem(&path);
        match by_stem.get(&stem) {
            // Prefer .gz: replace a plain entry when we meet its gz twin.
            Some(existing) if existing.extension().and_then(|e| e.to_str()) == Some("gz") => {}
            _ => {
                by_stem.insert(stem, path);
            }
        }
    }
    Ok(by_stem.into_values().collect())
}
