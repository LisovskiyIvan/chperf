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
    ) || e.cat == Some("__metadata")
}

/// Threads that carry RunTasks but are never "the main thread" for jank
/// analysis. `Chrome_IOThread` regularly blocks for >500ms and, when its
/// events come first in the file, used to win the long-task heuristic below —
/// reporting IO/GPU/compositor busy time as main-thread busy.
fn is_known_non_main(name: &str) -> bool {
    name.contains("IOThread")
        || name == "CrGpuMain"
        || name.contains("Compositor") // VizCompositorThread, renderer Compositor
        || name.starts_with("ThreadPool")
        || name.contains("Worker") // DedicatedWorker/ServiceWorker threads
        || name == "AudioThread"
        || name == "MemoryInfra"
}

/// Detection priority for thread names seen in Chrome traces; 0 = no opinion.
/// The renderer main is where scroll/jank/script live, so it outranks the
/// browser main.
fn main_name_priority(name: &str) -> u8 {
    match name {
        "CrRendererMain" | "RendererMain" => 3,
        "Renderer" => 2, // legacy renderer main
        "CrBrowserMain" | "Main" => 1,
        _ => 0,
    }
}

/// Detect the main thread for busy/long-task analysis.
///
/// 1. A thread with RunTask activity whose `thread_name` metadata marks it as
///    a main thread (`CrRendererMain` > `Renderer` > `CrBrowserMain`/`Main`);
///    ties broken by RunTask count, then lower tid.
/// 2. First RunTask > 500ms on a thread not ruled out by `is_known_non_main`.
/// 3. Most RunTask events, non-main threads excluded first, then unrestricted
///    for nameless traces.
pub fn detect_main_thread(events: &[TraceEvent]) -> u64 {
    // One pass: per-thread name (from `thread_name` metadata) + RunTask count.
    let mut threads: rustc_hash::FxHashMap<u64, (Option<&str>, usize)> =
        rustc_hash::FxHashMap::default();
    for e in events {
        if e.name == "thread_name" {
            let name = e
                .args_value()
                .and_then(|a| a.get("name"))
                .and_then(|v| v.as_str());
            threads.entry(e.tid).or_default().0 = name;
        } else if e.name == "RunTask" && e.ph == b'X' {
            threads.entry(e.tid).or_default().1 += 1;
        }
    }

    // 1. Named main thread with RunTask activity.
    if let Some((tid, _)) = threads
        .iter()
        .filter(|(_, (name, n))| *n > 0 && (*name).is_some_and(|n| main_name_priority(n) > 0))
        .max_by_key(|(tid, (name, n))| {
            (main_name_priority((*name).unwrap()), *n, std::cmp::Reverse(*tid))
        })
    {
        return *tid;
    }

    // 2. First long RunTask on a thread that can't be ruled out by name.
    for e in events {
        if e.name == "RunTask"
            && e.ph == b'X'
            && let Some(dur) = e.dur
            && dur > 500_000.0
            && !threads
                .get(&e.tid)
                .and_then(|(name, _)| *name)
                .is_some_and(is_known_non_main)
        {
            return e.tid;
        }
    }

    // 3. Most RunTask events, non-main threads excluded, then unrestricted.
    for exclude_known in [true, false] {
        if let Some((tid, _)) = threads
            .iter()
            .filter(|(_, (name, n))| {
                *n > 0 && !(exclude_known && (*name).is_some_and(is_known_non_main))
            })
            .max_by_key(|(tid, (_, n))| (*n, std::cmp::Reverse(*tid)))
        {
            return *tid;
        }
    }
    0
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

#[cfg(test)]
mod tests {
    use super::detect_main_thread;
    use crate::trace::{TraceEvent, intern_name};

    fn ev(name: &str, ph: u8, tid: u64, dur: Option<f64>, args: Option<String>) -> TraceEvent {
        TraceEvent {
            name: intern_name(name),
            id: 0,
            has_id: false,
            ph,
            ts: 0.0,
            dur,
            tid,
            pid: 1,
            cat: None,
            args: args.map(|s| s.into_boxed_str()),
            args_cache: std::sync::OnceLock::new(),
        }
    }

    fn thread_name(tid: u64, name: &str) -> TraceEvent {
        ev("thread_name", b'M', tid, None, Some(format!(r#"{{"name":"{name}"}}"#)))
    }

    /// Regression: Chrome_IOThread's long RunTask appearing first in the file
    /// used to win detection; the named renderer main must take precedence.
    #[test]
    fn renderer_name_beats_io_thread_long_task() {
        let events = vec![
            thread_name(74_465, "Chrome_IOThread"),
            thread_name(74_480, "CrGpuMain"),
            thread_name(104_561, "CrRendererMain"),
            ev("RunTask", b'X', 74_465, Some(600_000.0), None),
            ev("RunTask", b'X', 104_561, Some(16_700.0), None),
            ev("RunTask", b'X', 104_561, Some(30_000.0), None),
        ];
        assert_eq!(detect_main_thread(&events), 104_561);
    }

    #[test]
    fn unnamed_long_task_skips_known_non_main_threads() {
        let events = vec![
            thread_name(100, "Chrome_IOThread"),
            ev("RunTask", b'X', 100, Some(600_000.0), None),
            ev("RunTask", b'X', 300, Some(550_000.0), None),
            ev("RunTask", b'X', 300, Some(100.0), None),
        ];
        assert_eq!(detect_main_thread(&events), 300);
    }

    #[test]
    fn renderer_outranks_browser_main_count_breaks_ties() {
        let events = vec![
            thread_name(1, "CrBrowserMain"),
            thread_name(2, "Renderer"),
            thread_name(3, "Renderer"),
            ev("RunTask", b'X', 1, Some(100.0), None),
            ev("RunTask", b'X', 3, Some(100.0), None),
            ev("RunTask", b'X', 3, Some(100.0), None),
            ev("RunTask", b'X', 2, Some(100.0), None),
        ];
        assert_eq!(detect_main_thread(&events), 3);
    }

    #[test]
    fn named_main_without_runtasks_is_ignored() {
        let events = vec![
            thread_name(5, "CrRendererMain"),
            thread_name(6, "ThreadPoolForegroundWorker"),
            ev("RunTask", b'X', 6, Some(600_000.0), None),
            ev("RunTask", b'X', 7, Some(10.0), None),
        ];
        assert_eq!(detect_main_thread(&events), 7);
    }

    #[test]
    fn nameless_trace_keeps_long_task_then_most_runtasks() {
        let events = vec![
            ev("RunTask", b'X', 1, Some(100.0), None),
            ev("RunTask", b'X', 2, Some(100.0), None),
            ev("RunTask", b'X', 2, Some(100.0), None),
        ];
        assert_eq!(detect_main_thread(&events), 2);
    }
}
