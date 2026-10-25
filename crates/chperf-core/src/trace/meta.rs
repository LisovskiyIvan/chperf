//! Trace-level metadata, the top-level file shape, main-thread detection and
//! directory listing helpers.

use super::TraceEvent;
use super::parse::ParseStats;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize, serde::Serialize, Clone, Debug, PartialEq)]
pub struct FrameInfo {
    pub id: String,
    pub parent_id: Option<String>,
    pub process_id: u64,
    pub url: String,
    pub name: String,
    pub is_main_frame: bool,
}

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
    /// Extracted frame tree from TracingStartedInBrowser
    #[serde(skip)]
    pub frames: Vec<FrameInfo>,
}

#[derive(Deserialize)]
pub struct TraceFile {
    #[serde(rename = "traceEvents")]
    pub trace_events: Vec<TraceEvent>,
    #[serde(default)]
    pub metadata: Option<TraceMetadata>,
    /// Parse counters (parsed/skipped/malformed). Skipped by serde: only
    /// `parse_trace` fills them; deserialized traces default to zeros.
    /// `parsed` counts raw events as read (unpaired `B`/`E` included);
    /// synthetic `X` events appended by `normalize_be_pairs` are not counted.
    ///
    /// The file is self-contained: every event owns its strings, so dropping
    /// the source buffer after parsing frees all input memory — holding a
    /// `TraceFile` retains only the events, and repeated `parse_trace`
    /// calls accumulate no leaked buffers.
    #[serde(skip)]
    pub stats: ParseStats,
}

/// Metadata events (`thread_name`/`process_name`/…, cat `__metadata`) carry
/// `ts` from process start — often far before the actual session. They must be
/// excluded from time-base computations, or every "ms from trace start" window
/// lands in dead time.
pub fn is_metadata_event(e: &TraceEvent) -> bool {
    matches!(
        e.name.as_str(),
        "thread_name" | "process_name" | "thread_sort_index" | "process_sort_index"
    ) || e.cat.as_deref() == Some("__metadata")
}

/// Threads that carry RunTasks but are never "the main thread" for jank
/// analysis. `Chrome_IOThread` regularly blocks for >500ms and, when its
/// events come first in the file, used to win the long-task heuristic below —
/// reporting IO/GPU/compositor busy time as main-thread busy.
/// Matching is case-insensitive; lanes are excluded only when the name
/// clearly identifies them (none of these match real main threads).
fn is_known_non_main(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("iothread")
        || lower == "crgpumain"
        || lower.contains("compositor") // VizCompositorThread, renderer CompositorTileWorker
        || lower.starts_with("threadpool")
        || lower.contains("worker") // DedicatedWorker, RasterWorker, TileWorker, …
        || lower.contains("audio") // AudioThread, AudioOutput
        || lower == "memoryinfra"
        || lower.contains("inprocgpu") // Chrome_InProcGpuThread
        || lower.contains("gpu") // GPU Process
        || lower.contains("media") // Media pipeline threads
}

/// Detection priority for thread names seen in Chrome traces; 0 = no opinion.
/// The renderer main is where scroll/jank/script live, so it outranks the
/// browser main. Matching is case-insensitive.
fn main_name_priority(name: &str) -> u8 {
    if name.eq_ignore_ascii_case("CrRendererMain") {
        4
    } else if name.eq_ignore_ascii_case("RendererMain")
        || name.eq_ignore_ascii_case("Renderer Main")
        || name.eq_ignore_ascii_case("CrRendererMainThread")
        || name.eq_ignore_ascii_case("Renderer")
    {
        3
    } else if name.eq_ignore_ascii_case("CrBrowserMain") || name.eq_ignore_ascii_case("Main") {
        1
    } else {
        0
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
    let mut threads: rustc_hash::FxHashMap<u64, (Option<String>, usize)> =
        rustc_hash::FxHashMap::default();
    for e in events {
        if e.name == "thread_name" {
            let args = e.args_value();
            let name = args
                .as_ref()
                .and_then(|a| a.get("name"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            threads.entry(e.tid).or_default().0 = name;
        } else if e.name == "RunTask" && e.ph == b'X' {
            threads.entry(e.tid).or_default().1 += 1;
        }
    }

    // 1. Named main thread with RunTask activity.
    if let Some((tid, _)) = threads
        .iter()
        .filter(|(_, (name, n))| {
            *n > 0
                && (*name)
                    .as_deref()
                    .is_some_and(|n| main_name_priority(n) > 0)
        })
        .max_by_key(|(tid, (name, n))| {
            (
                main_name_priority((*name).as_deref().unwrap()),
                *n,
                std::cmp::Reverse(*tid),
            )
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
                .and_then(|(name, _)| (*name).as_deref())
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
                *n > 0 && !(exclude_known && (*name).as_deref().is_some_and(is_known_non_main))
            })
            .max_by_key(|(tid, (_, n))| (*n, std::cmp::Reverse(*tid)))
        {
            return *tid;
        }
    }
    0
}

/// One renderer main thread: a `(pid, tid)` pair plus its `thread_name`
/// metadata (`""` when the trace carries no `thread_name` for it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RendererThread {
    pub pid: u64,
    pub tid: u64,
    pub name: String,
}

/// Per-pid / multi-renderer thread model (OOPIF, workers, browser process).
///
/// Name patterns keyed on (all matching is case-insensitive):
/// - renderer-main-ish: `CrRendererMain` (top priority), then
///   `RendererMain` / `Renderer Main` / `CrRendererMainThread` / `Renderer`
///   (see `main_name_priority`);
/// - browser main: `CrBrowserMain` / `Main`;
/// - compositor lanes: thread names containing `Compositor`
///   (`VizCompositorThread`, `CompositorTileWorker`, …);
/// - excluded lanes: everything `is_known_non_main` rejects (IOThread,
///   GPU, audio, media, workers, thread pools, …).
#[derive(Clone, Debug)]
pub struct ThreadModel {
    /// The global main thread: `tid == detect_main_thread(events)`
    /// (guaranteed consistent — an entry is added even when it is not a
    /// per-pid winner). Sentinel `{pid: 0, tid: 0, name: ""}` when the trace
    /// has no main thread.
    pub primary: RendererThread,
    /// One entry per renderer process: the pid's renderer-main-ish thread,
    /// or its majority-RunTask thread for nameless traces. Sorted by
    /// `(pid, tid)`. Pids whose winner is a browser-main name or a known
    /// non-main lane (GPU/IO/…) are not renderers and are skipped.
    pub renderers: Vec<RendererThread>,
    /// The browser main thread (`CrBrowserMain`/`Main`), if named.
    pub browser_main: Option<u64>,
    /// Tids whose thread name contains `Compositor` (any pid).
    pub compositor: Vec<u64>,
}

/// Build the multi-renderer thread model. Reuses the `detect_main_thread`
/// heuristics per pid (named renderer main wins, then most RunTasks with
/// known non-main lanes excluded, then unrestricted). `primary.tid` always
/// equals `detect_main_thread(events)`.
pub fn detect_thread_model(events: &[TraceEvent]) -> ThreadModel {
    // Per (pid, tid): pid (first seen), name (latest thread_name wins),
    // RunTask (ph X) count.
    let mut threads: rustc_hash::FxHashMap<(u64, u64), (u64, Option<String>, usize)> =
        rustc_hash::FxHashMap::default();
    for e in events {
        if e.name == "thread_name" {
            let args = e.args_value();
            let name = args
                .as_ref()
                .and_then(|a| a.get("name"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let slot = threads.entry((e.pid, e.tid)).or_insert((e.pid, None, 0));
            if name.is_some() {
                slot.1 = name;
            }
        } else if e.name == "RunTask" && e.ph == b'X' {
            threads.entry((e.pid, e.tid)).or_insert((e.pid, None, 0)).2 += 1;
        }
    }

    // Compositor lanes: any thread named *Compositor*.
    let mut compositor: Vec<u64> = threads
        .iter()
        .filter(|(_, (_, name, _))| {
            (*name)
                .as_deref()
                .is_some_and(|n| n.to_ascii_lowercase().contains("compositor"))
        })
        .map(|((_, tid), _)| *tid)
        .collect();
    compositor.sort_unstable();
    compositor.dedup();

    // Browser main: the CrBrowserMain/Main thread with the most RunTasks.
    let browser_main: Option<u64> = threads
        .iter()
        .filter(|(_, (_, name, n))| {
            *n > 0
                && (*name)
                    .as_deref()
                    .is_some_and(|x| main_name_priority(x) == 1)
        })
        .max_by_key(|((_, tid), (_, _, n))| (*n, std::cmp::Reverse(*tid)))
        .map(|((_, tid), _)| *tid);

    // One renderer per pid that carries RunTasks.
    let mut pids: Vec<u64> = threads
        .values()
        .filter(|(_, _, n)| *n > 0)
        .map(|(pid, _, _)| *pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    let mut renderers: Vec<RendererThread> = Vec::new();
    for pid in pids {
        let in_pid: Vec<((u64, u64), Option<String>, usize)> = threads
            .iter()
            .filter(|((p, _), (_, _, n))| *p == pid && *n > 0)
            .map(|(k, (_, name, n))| (*k, name.clone(), *n))
            .collect();
        if in_pid.is_empty() {
            continue;
        }
        // (a) Named renderer main in this pid.
        let mut winner: Option<((u64, u64), String, usize)> = in_pid
            .iter()
            .filter(|(_, name, _)| {
                (*name)
                    .as_deref()
                    .is_some_and(|x| main_name_priority(x) > 1)
            })
            .map(|(k, name, n)| (*k, name.clone().unwrap_or_default(), *n))
            .max_by_key(|(k, name, n)| (main_name_priority(name), *n, std::cmp::Reverse(k.1)));
        // (b) Most RunTasks, known non-main lanes excluded; (c) unrestricted.
        if winner.is_none() {
            for exclude_known in [true, false] {
                if let Some((k, name, n)) = in_pid
                    .iter()
                    .filter(|(_, name, _)| {
                        !(exclude_known && (*name).as_deref().is_some_and(is_known_non_main))
                    })
                    .map(|(k, name, n)| (*k, name.clone().unwrap_or_default(), *n))
                    .max_by_key(|(k, _, n)| (*n, std::cmp::Reverse(k.1)))
                {
                    winner = Some((k, name, n));
                    break;
                }
            }
        }
        let Some(((wpid, wtid), wname, _)) = winner else {
            continue;
        };
        // A browser-main or known-non-main winner means this pid is not a
        // renderer process (browser / GPU / IO / …).
        if main_name_priority(&wname) == 1 || is_known_non_main(&wname) {
            continue;
        }
        renderers.push(RendererThread {
            pid: wpid,
            tid: wtid,
            name: wname,
        });
    }

    // Primary: identical to detect_main_thread; ensure it is listed.
    let primary_tid = detect_main_thread(events);
    let mut primary = RendererThread {
        pid: 0,
        tid: primary_tid,
        name: String::new(),
    };
    if primary_tid != 0 {
        let mut cands: Vec<((u64, u64), Option<String>)> = threads
            .iter()
            .filter(|((_, tid), _)| *tid == primary_tid)
            .map(|(k, (pid, name, _))| ((*pid, k.1), name.clone()))
            .collect();
        cands.sort();
        if let Some(((pid, _), name)) = cands.into_iter().next() {
            primary.pid = pid;
            primary.name = name.unwrap_or_default();
        }
        if !renderers.iter().any(|r| r.tid == primary_tid) {
            renderers.push(primary.clone());
        }
    }
    renderers.sort_by_key(|r| (r.pid, r.tid));

    ThreadModel {
        primary,
        renderers,
        browser_main,
        compositor,
    }
}

/// Stable stem for a trace file: strips both `.json` and `.json.gz`
/// (case-insensitive: `Trace-….JSON.GZ` -> `Trace-…`).
/// `Trace-20260731T180758.json.gz` -> `Trace-20260731T180758`.
pub fn trace_stem(path: &Path) -> String {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if name.len() >= ".json".len()
        && name[name.len() - ".json".len()..].eq_ignore_ascii_case(".json")
    {
        name[..name.len() - ".json".len()].to_string()
    } else {
        name.to_string()
    }
}

/// Scan a directory for Chrome trace files (`*.json`, `*.json.gz`,
/// case-insensitive). Non-trace files are skipped (counted — see
/// `list_traces_with_skipped`).
pub fn list_traces(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    Ok(list_traces_with_skipped(dir)?.0)
}

/// Scan a directory for Chrome trace files (`*.json`, `*.json.gz`,
/// case-insensitive). When both `.json` and `.json.gz` exist for the same
/// stem, keep only the `.json.gz` (smaller I/O). Sorted by name. Returns the
/// traces plus the count of skipped non-trace files.
pub fn list_traces_with_skipped(
    dir: &Path,
) -> Result<(Vec<PathBuf>, usize), Box<dyn std::error::Error>> {
    let mut by_stem: std::collections::BTreeMap<String, PathBuf> =
        std::collections::BTreeMap::new();
    let mut skipped = 0usize;
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
        let lower = name.to_ascii_lowercase();
        let is_gz = lower.ends_with(".json.gz");
        let is_plain = !is_gz && lower.ends_with(".json");
        if !is_gz && !is_plain {
            skipped += 1;
            continue;
        }
        let stem = trace_stem(&path);
        match by_stem.get(&stem) {
            // Prefer .gz: replace a plain entry when we meet its gz twin.
            Some(existing)
                if existing
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("gz")) => {}
            _ => {
                by_stem.insert(stem, path);
            }
        }
    }
    Ok((by_stem.into_values().collect(), skipped))
}

/// Resolve a trace input string to trace file paths:
/// - an existing `.json` / `.json.gz` file → `vec![it]`;
/// - an existing directory → the `list_traces` listing (`.gz` preferred);
/// - a glob pattern (contains `*`, `?` or `[`) → the matching `*.json` /
///   `*.json.gz` files in one directory (**non-recursive**; the pattern's
///   directory part, or `.` when bare, is listed and file names are matched
///   with `*` / `?` / `[...]` — document this limit to callers);
/// - anything else (missing path, pattern with no matches, empty directory)
///   → `Err` with a human-readable message.
pub fn expand_trace_input(input: &str) -> Result<Vec<PathBuf>, String> {
    if input.contains(['*', '?', '[']) {
        return expand_glob_input(input);
    }
    let p = Path::new(input);
    if p.is_file() {
        let lower = input.to_ascii_lowercase();
        if lower.ends_with(".json") || lower.ends_with(".json.gz") {
            return Ok(vec![p.to_path_buf()]);
        }
        return Err(format!(
            "input '{input}' is not a trace file: expected a .json or .json.gz file, a directory, or a glob pattern"
        ));
    }
    if p.is_dir() {
        let traces = list_traces(p).map_err(|e| format!("cannot list directory '{input}': {e}"))?;
        if traces.is_empty() {
            return Err(format!(
                "no trace files (*.json, *.json.gz) found in directory '{input}'"
            ));
        }
        return Ok(traces);
    }
    Err(format!(
        "input '{input}' does not exist: expected a .json/.json.gz file, a directory, or a glob pattern"
    ))
}

/// Glob branch of `expand_trace_input` (non-recursive, see its docs).
/// `**` is accepted but treated as a single-directory match (never
/// recursive, never an error): `**` segments in the directory part are
/// dropped and a bare `**` file pattern means `*` (still filtered to trace
/// extensions). Redundant separators (`dir//*.json`) and `./` prefixes are
/// normalized away. Matches are sorted for deterministic output.
fn expand_glob_input(input: &str) -> Result<Vec<PathBuf>, String> {
    let (dir_part, file_pat) = match input.rfind('/') {
        Some(i) => (&input[..i], &input[i + 1..]),
        None => (".", input),
    };
    let absolute = input.starts_with('/');
    let dir = normalize_glob_dir(dir_part, absolute);
    let file_pat = if file_pat == "**" { "*" } else { file_pat };
    let entries =
        std::fs::read_dir(&dir).map_err(|e| format!("cannot list directory '{dir}': {e}"))?;
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot list directory '{dir}': {e}"))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let lower = name.to_ascii_lowercase();
        if !(lower.ends_with(".json") || lower.ends_with(".json.gz")) {
            continue;
        }
        if glob_match(file_pat, name) {
            out.push(path);
        }
    }
    out.sort();
    if out.is_empty() {
        return Err(format!(
            "pattern '{input}' matched no trace files (*.json, *.json.gz; non-recursive)"
        ));
    }
    Ok(out)
}

/// Normalize the directory part of a glob pattern to a single listable
/// directory (unix only): collapse redundant separators (`dir//sub` →
/// `dir/sub`), drop `.` segments (`./dir` → `dir`), and drop `**` / other
/// glob-meta segments (`dir/**` → `dir`) — the match stays non-recursive by
/// design. Falls back to `.` (relative) or `/` (absolute) when nothing
/// listable remains.
fn normalize_glob_dir(raw: &str, absolute: bool) -> String {
    let mut segs: Vec<&str> = Vec::new();
    for s in raw.split('/') {
        if s.is_empty() || s == "." || s == "**" {
            continue;
        }
        if s.contains(['*', '?', '[']) {
            continue;
        }
        segs.push(s);
    }
    if segs.is_empty() {
        return if absolute {
            "/".to_string()
        } else {
            ".".to_string()
        };
    }
    let mut d = segs.join("/");
    if absolute {
        d.insert(0, '/');
    }
    d
}

/// Minimal glob matcher for file names: `*` (any run), `?` (one char) and
/// `[...]` classes (`[abc]`, `[a-z]`, `[!abc]`). Case-sensitive (callers
/// lowercase both sides when they need otherwise).
fn glob_match(pat: &str, s: &str) -> bool {
    fn classes(p: &[u8], c: u8) -> (bool, usize) {
        // Returns (matched, pattern bytes consumed incl. brackets).
        let mut i = 1usize;
        let mut neg = false;
        if i < p.len() && (p[i] == b'!' || p[i] == b'^') {
            neg = true;
            i += 1;
        }
        let mut hit = false;
        let mut first = true;
        while i < p.len() && !(p[i] == b']' && !first) {
            if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
                if p[i] <= c && c <= p[i + 2] {
                    hit = true;
                }
                i += 3;
            } else {
                if p[i] == c {
                    hit = true;
                }
                i += 1;
            }
            first = false;
        }
        let consumed = if i < p.len() { i + 1 } else { i };
        (hit != neg, consumed)
    }
    fn rec(p: &[u8], s: &[u8]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        match p[0] {
            b'*' => {
                let mut q = 1;
                while q < p.len() && p[q] == b'*' {
                    q += 1;
                }
                for k in 0..=s.len() {
                    if rec(&p[q..], &s[k..]) {
                        return true;
                    }
                }
                false
            }
            b'?' => !s.is_empty() && rec(&p[1..], &s[1..]),
            b'[' => {
                if s.is_empty() {
                    return false;
                }
                let (hit, consumed) = classes(p, s[0]);
                hit && rec(&p[consumed.min(p.len())..], &s[1..])
            }
            b'\\' if p.len() > 1 => s.first() == Some(&p[1]) && rec(&p[2..], &s[1..]),
            c => s.first() == Some(&c) && rec(&p[1..], &s[1..]),
        }
    }
    rec(pat.as_bytes(), s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::detect_main_thread;
    use crate::trace::ArgsText;
    use crate::trace::TraceEvent;

    fn ev(name: &str, ph: u8, tid: u64, dur: Option<f64>, args: Option<String>) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph,
            ts: 0.0,
            dur,
            tid,
            pid: 1,
            cat: None,
            args: args.map(|s| ArgsText::copy(&s)),
        }
    }

    fn thread_name(tid: u64, name: &str) -> TraceEvent {
        ev(
            "thread_name",
            b'M',
            tid,
            None,
            Some(format!(r#"{{"name":"{name}"}}"#)),
        )
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

    /// Thread-name matching is case-insensitive and covers the renderer-main
    /// aliases; CrRendererMain stays the highest priority.
    #[test]
    fn main_detection_is_case_insensitive_with_aliases() {
        for alias in [
            "Renderer Main",
            "CrRendererMainThread",
            "renderer",
            "RENDERERMAIN",
        ] {
            let events = vec![
                thread_name(1, "CrBrowserMain"),
                thread_name(2, alias),
                ev("RunTask", b'X', 1, Some(100.0), None),
                ev("RunTask", b'X', 2, Some(100.0), None),
            ];
            assert_eq!(detect_main_thread(&events), 2, "alias {alias} must win");
        }
        // CrRendererMain outranks a same-count RendererMain alias.
        let events = vec![
            thread_name(1, "crrenderermain"),
            thread_name(2, "RendererMain"),
            ev("RunTask", b'X', 1, Some(100.0), None),
            ev("RunTask", b'X', 2, Some(100.0), None),
        ];
        assert_eq!(detect_main_thread(&events), 1);
    }

    /// GPU/compositor/raster/audio/media lanes never win the long-task
    /// fallback, and never shadow a real main thread.
    #[test]
    fn known_non_main_covers_gpu_audio_media_workers() {
        for lane in [
            "Chrome_InProcGpuThread",
            "VizCompositorThread",
            "CompositorTileWorker",
            "RasterWorker",
            "TileWorker",
            "GPU Process",
            "AudioOutput",
            "Media",
            "audiooutput",
        ] {
            let events = vec![
                thread_name(100, lane),
                ev("RunTask", b'X', 100, Some(600_000.0), None),
                ev("RunTask", b'X', 300, Some(550_000.0), None),
            ];
            assert_eq!(detect_main_thread(&events), 300, "lane {lane} must lose");
        }
        // Real main threads are not excluded.
        for main in ["CrRendererMain", "RendererMain", "CrBrowserMain"] {
            let events = vec![
                thread_name(1, main),
                ev("RunTask", b'X', 1, Some(600_000.0), None),
                ev("RunTask", b'X', 2, Some(100.0), None),
            ];
            assert_eq!(detect_main_thread(&events), 1, "main {main} must win");
        }
    }

    /// Extension matching is case-insensitive and non-trace files are
    /// skipped but counted.
    #[test]
    fn list_traces_matches_case_insensitively_and_counts_skipped() {
        let dir = std::env::temp_dir().join(format!("chperf-list-{}.d", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.JSON"), "{}").unwrap();
        std::fs::write(dir.join("b.Json.GZ"), "{}").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::write(dir.join("data.csv"), "x").unwrap();
        let (traces, skipped) = super::list_traces_with_skipped(&dir).unwrap();
        assert_eq!(traces.len(), 2);
        assert_eq!(skipped, 2);
        assert!(traces.iter().all(|p| {
            let n = p
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_ascii_lowercase();
            n.ends_with(".json") || n.ends_with(".json.gz")
        }));
        // The plain helper agrees.
        let plain = super::list_traces(&dir).unwrap();
        assert_eq!(plain.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(super::trace_stem(std::path::Path::new("T.JSON.GZ")), "T");
    }

    fn be(name: &str, ph: u8, tid: u64, pid: u64, dur: Option<f64>) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph,
            ts: 0.0,
            dur,
            tid,
            pid,
            cat: None,
            args: None,
        }
    }

    fn tn(pid: u64, tid: u64, name: &str) -> TraceEvent {
        TraceEvent {
            name: "thread_name".into(),
            id: 0,
            has_id: false,
            ph: b'M',
            ts: 0.0,
            dur: None,
            tid,
            pid,
            cat: None,
            args: Some(ArgsText::copy(&format!(r#"{{"name":"{name}"}}"#))),
        }
    }

    /// Multi-renderer model: named renderer + nameless OOPIF pids become
    /// renderers; browser and GPU pids don't; compositor tids are listed;
    /// primary matches detect_main_thread.
    #[test]
    fn thread_model_multi_renderer() {
        let events = vec![
            tn(1, 11, "CrRendererMain"),
            tn(1, 12, "ThreadPoolForegroundWorker"),
            tn(3, 31, "CrBrowserMain"),
            tn(4, 41, "CrGpuMain"),
            tn(1, 51, "VizCompositorThread"),
            be("RunTask", b'X', 11, 1, Some(100.0)),
            be("RunTask", b'X', 11, 1, Some(100.0)),
            be("RunTask", b'X', 12, 1, Some(100.0)),
            be("RunTask", b'X', 12, 1, Some(100.0)),
            be("RunTask", b'X', 12, 1, Some(100.0)),
            be("RunTask", b'X', 21, 2, Some(100.0)),
            be("RunTask", b'X', 21, 2, Some(100.0)),
            be("RunTask", b'X', 31, 3, Some(100.0)),
            be("RunTask", b'X', 41, 4, Some(100.0)),
        ];
        let m = super::detect_thread_model(&events);
        // Named renderer main beats the busier worker in pid 1.
        assert_eq!(m.renderers.len(), 2);
        assert_eq!(m.renderers[0].pid, 1);
        assert_eq!(m.renderers[0].tid, 11);
        assert_eq!(m.renderers[0].name, "CrRendererMain");
        // Nameless OOPIF pid is a renderer with an empty name.
        assert_eq!(m.renderers[1].pid, 2);
        assert_eq!(m.renderers[1].tid, 21);
        assert_eq!(m.renderers[1].name, "");
        assert_eq!(m.browser_main, Some(31));
        assert_eq!(m.compositor, vec![51]);
        assert_eq!(m.primary.tid, detect_main_thread(&events));
        assert_eq!(m.primary.tid, 11);
    }

    /// Primary is listed even when it is not its pid's majority winner
    /// (here: the long-task fallback beats the busier nameless sibling).
    #[test]
    fn thread_model_lists_non_majority_primary() {
        let mut events = vec![];
        for _ in 0..5 {
            events.push(be("RunTask", b'X', 11, 1, Some(10_000.0)));
        }
        events.push(be("RunTask", b'X', 12, 1, Some(600_000.0)));
        let m = super::detect_thread_model(&events);
        assert_eq!(detect_main_thread(&events), 12);
        assert_eq!(m.primary.tid, 12);
        assert!(m.renderers.iter().any(|r| r.tid == 11));
        assert!(m.renderers.iter().any(|r| r.tid == 12));
    }

    /// Compositor matching is a case-insensitive substring; browser aliases
    /// map to browser_main.
    #[test]
    fn thread_model_compositor_substring_and_browser_alias() {
        let events = vec![
            tn(1, 7, "CompositorTileWorker"),
            tn(1, 8, "compositor"),
            tn(3, 9, "Main"),
            be("RunTask", b'X', 9, 3, Some(100.0)),
            be("RunTask", b'X', 10, 5, Some(100.0)),
        ];
        let m = super::detect_thread_model(&events);
        assert_eq!(m.compositor, vec![7, 8]);
        assert_eq!(m.browser_main, Some(9));
    }

    #[test]
    fn expand_trace_input_file_dir_glob_and_errors() {
        let dir = std::env::temp_dir().join(format!("chperf-expand-{}.d", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let a = dir.join("a.json");
        let b = dir.join("b.JSON.GZ");
        std::fs::write(&a, "{}").unwrap();
        std::fs::write(&b, "{}").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::write(dir.join("sub").join("c.json"), "{}").unwrap();

        // Single file (case-insensitive .gz).
        assert_eq!(
            super::expand_trace_input(a.to_str().unwrap()),
            Ok(vec![a.clone()])
        );
        assert_eq!(
            super::expand_trace_input(b.to_str().unwrap()),
            Ok(vec![b.clone()])
        );
        // Directory: non-recursive listing, notes.txt skipped.
        assert_eq!(
            super::expand_trace_input(dir.to_str().unwrap()),
            Ok(vec![a.clone(), b.clone()])
        );
        // Non-trace file is an error.
        let txt = dir.join("notes.txt");
        assert!(super::expand_trace_input(txt.to_str().unwrap()).is_err());
        // Glob: non-recursive, trace extensions only.
        let pat = format!("{}/*.json", dir.display());
        assert_eq!(super::expand_trace_input(&pat), Ok(vec![a.clone()]));
        let star = format!("{}/*", dir.display());
        assert_eq!(super::expand_trace_input(&star), Ok(vec![a, b]));
        // Missing path and empty match are errors with a message.
        assert!(super::expand_trace_input(dir.join("nope.json").to_str().unwrap()).is_err());
        let nomatch = format!("{}/zzz*.json", dir.display());
        let err = super::expand_trace_input(&nomatch).unwrap_err();
        assert!(err.contains("matched no"), "unexpected: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Glob polish: `./` prefixes, `//` separators, `**` (single-dir, never
    /// an error), absolute patterns, and deterministic sorted output.
    #[test]
    fn expand_glob_polish_and_sorted() {
        // Unit-level: dir normalization without touching the real cwd.
        assert_eq!(super::normalize_glob_dir(".", false), ".");
        assert_eq!(super::normalize_glob_dir("./dir", false), "dir");
        assert_eq!(super::normalize_glob_dir("dir//sub", false), "dir/sub");
        assert_eq!(super::normalize_glob_dir("dir/**", false), "dir");
        assert_eq!(super::normalize_glob_dir("**", false), ".");
        assert_eq!(super::normalize_glob_dir("", true), "/");
        assert_eq!(super::normalize_glob_dir("tmp/x", true), "/tmp/x");
        assert!(super::glob_match("**", "a.json"));
        assert!(super::glob_match("*.json", "a.json"));

        let dir = std::env::temp_dir().join(format!("chperf-glob2-{}.d", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Created out of order; output must still be sorted.
        for n in ["z.json", "a.json", "m.json"] {
            std::fs::write(dir.join(n), "{}").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let exp = vec![dir.join("a.json"), dir.join("m.json"), dir.join("z.json")];

        // Absolute patterns (dir is absolute).
        let abs_star = format!("{}/*.json", dir.display());
        assert_eq!(super::expand_trace_input(&abs_star), Ok(exp.clone()));
        // Redundant separators collapse.
        let dbl = format!("{}//*.json", dir.display());
        assert_eq!(super::expand_trace_input(&dbl), Ok(exp.clone()));
        // Bare `**` matches all trace files in the single dir.
        let star2 = format!("{}/**", dir.display());
        assert_eq!(super::expand_trace_input(&star2), Ok(exp.clone()));
        // `**` in the directory part stays non-recursive, not an error.
        let mid = format!("{}/**/*.json", dir.display());
        assert_eq!(super::expand_trace_input(&mid), Ok(exp.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
