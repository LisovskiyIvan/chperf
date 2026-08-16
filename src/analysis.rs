use crate::trace::TraceEvent;
use crate::inspect::Scope;
use std::collections::HashMap;

// ── Summary ──

#[derive(Clone)]
pub struct EventTypeStat {
    pub name: &'static str,
    pub total_time_us: f64,
    pub count: usize,
    pub avg_time_us: f64,
    pub pct_of_trace: f64, // percentage of total trace time
}

#[derive(Clone)]
pub struct SummaryResult {
    pub long_task_count: usize,
    pub long_tasks_top: Vec<f64>,
    pub total_trace_duration_us: f64,
    pub main_thread_busy_us: f64, // total RunTask time on main thread
    pub event_stats: Vec<EventTypeStat>,
}

/// Map an event name to its canonical `'static` key, or `None` if not tracked.
fn target_key(name: &str) -> Option<&'static str> {
    Some(match name {
        "RunTask" => "RunTask",
        "UpdateLayoutTree" => "UpdateLayoutTree",
        "Layout" => "Layout",
        "Paint" => "Paint",
        "FunctionCall" => "FunctionCall",
        "FireAnimationFrame" => "FireAnimationFrame",
        "Layerize" => "Layerize",
        "Commit" => "Commit",
        "HitTest" => "HitTest",
        "IntersectionObserverController::computeIntersections" => {
            "IntersectionObserverController::computeIntersections"
        }
        "MajorGC" => "MajorGC",
        "MinorGC" => "MinorGC",
        "EvaluateScript" => "EvaluateScript",
        _ => return None,
    })
}

pub fn analyze_summary(events: &[TraceEvent], main_tid: u64) -> SummaryResult {
    // Single pass over the trace: duration bounds, long tasks, busy time, stats.
    let mut long_task_durs: Vec<f64> = Vec::new();
    let mut stats_map: rustc_hash::FxHashMap<&'static str, (f64, usize)> = rustc_hash::FxHashMap::default();
    let mut main_thread_busy_us = 0.0f64;
    let mut min_ts = f64::INFINITY;
    let mut max_ts = 0.0f64;

    for e in events {
        if crate::trace::is_metadata_event(e) {
            continue;
        }
        let end = e.ts + e.dur.unwrap_or(0.0);
        if e.ts < min_ts {
            min_ts = e.ts;
        }
        if end > max_ts {
            max_ts = end;
        }
        if e.tid != main_tid || e.ph != b'X' {
            continue;
        }
        if e.name == "RunTask"
            && let Some(d) = e.dur {
                main_thread_busy_us += d;
                if d > 50_000.0 {
                    long_task_durs.push(d);
                }
            }
        if let Some(key) = target_key(e.name)
            && let Some(d) = e.dur {
                let entry = stats_map.entry(key).or_default();
                entry.0 += d;
                entry.1 += 1;
            }
    }

    long_task_durs.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let long_task_count = long_task_durs.len();
    let long_tasks_top: Vec<f64> = long_task_durs.into_iter().take(10).collect();
    let total_trace_duration_us = (max_ts - min_ts).max(0.0);

    let mut event_stats: Vec<EventTypeStat> = stats_map
        .into_iter()
        .map(|(name, (total, count))| EventTypeStat {
            name,
            total_time_us: total,
            count,
            avg_time_us: if count > 0 {
                total / count as f64
            } else {
                0.0
            },
            pct_of_trace: if total_trace_duration_us > 0.0 {
                total / total_trace_duration_us * 100.0
            } else {
                0.0
            },
        })
        .collect();
    event_stats.sort_by(|a, b| b.total_time_us.partial_cmp(&a.total_time_us).unwrap());

    SummaryResult {
        long_task_count,
        long_tasks_top,
        total_trace_duration_us,
        main_thread_busy_us,
        event_stats,
    }
}

// ── Scroll Frame Analysis ──

#[derive(Clone)]
pub struct FrameTask {
    #[allow(dead_code)]
    pub ts: f64,
    pub dur_us: f64,
    pub js_us: f64,
    pub ult_us: f64,
    pub paint_us: f64,
    pub composite_us: f64,
    pub hit_test_us: f64,
    pub layout_us: f64,
}

impl FrameTask {
    /// Returns the name of the dominant cost category
    pub fn bottleneck(&self) -> &'static str {
        let costs = [
            (self.js_us, "JS"),
            (self.ult_us, "Style"),
            (self.layout_us, "Layout"),
            (self.paint_us, "Paint"),
            (self.composite_us, "Composite"),
            (self.hit_test_us, "HitTest"),
        ];
        costs
            .iter()
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
            .map(|(_, name)| *name)
            .unwrap_or("?")
    }

    /// Returns breakdown as (label, value, color_index) sorted by value desc
    #[allow(dead_code)]
    pub fn breakdown(&self) -> Vec<(&'static str, f64)> {
        let mut v = vec![
            ("JS", self.js_us),
            ("Style", self.ult_us),
            ("Layout", self.layout_us),
            ("Paint", self.paint_us),
            ("Comp", self.composite_us),
            ("Hit", self.hit_test_us),
        ];
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        v
    }
}

#[derive(Clone)]
pub struct ScrollFramePercentiles {
    pub p50_us: f64,
    pub p90_us: f64,
    pub p99_us: f64,
}

#[derive(Clone)]
pub struct ScrollFrameResult {
    pub tasks: Vec<FrameTask>,
    pub avg: FrameTask,
    pub percentiles: ScrollFramePercentiles,
}

pub fn analyze_scroll_frames(events: &[TraceEvent], main_tid: u64) -> ScrollFrameResult {
    let mut main_x: Vec<&TraceEvent> = events
        .iter()
        .filter(|e| e.tid == main_tid && e.ph == b'X')
        .collect();
    main_x.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap());

    let run_tasks: Vec<&TraceEvent> = main_x
        .iter()
        .copied()
        .filter(|e| e.name == "RunTask" && e.dur.is_some())
        .collect();

    let mut tasks = Vec::new();

    // Single sweep: RunTasks on a thread are serial and non-nested, so each
    // main-thread event falls inside at most one task. `lo` only moves forward.
    let mut lo = 0usize;
    for rt in &run_tasks {
        let rt_ts = rt.ts;
        let rt_dur = rt.dur.unwrap();
        let rt_end = rt_ts + rt_dur;

        while lo < main_x.len() && main_x[lo].ts < rt_ts {
            lo += 1;
        }
        let mut j = lo;
        let mut has_heavy_ult = false;
        let mut has_heavy_fc = false;
        while j < main_x.len() && main_x[j].ts <= rt_end {
            let e = main_x[j];
            if e.name != "RunTask" {
                let d = e.dur.unwrap_or(0.0);
                if d > 50_000.0 {
                    if e.name == "UpdateLayoutTree" {
                        has_heavy_ult = true;
                    } else if e.name == "FunctionCall" {
                        has_heavy_fc = true;
                    }
                }
            }
            j += 1;
        }

        if !has_heavy_ult && !has_heavy_fc {
            continue;
        }

        let mut ft = FrameTask {
            ts: rt_ts,
            dur_us: rt_dur,
            js_us: 0.0,
            ult_us: 0.0,
            paint_us: 0.0,
            composite_us: 0.0,
            hit_test_us: 0.0,
            layout_us: 0.0,
        };

        j = lo;
        while j < main_x.len() && main_x[j].ts <= rt_end {
            let e = main_x[j];
            if e.name != "RunTask" {
                let d = e.dur.unwrap_or(0.0);
                if d > 0.0 && e.ts + d <= rt_end {
                    match e.name {
                        "FunctionCall" => ft.js_us += d,
                        "UpdateLayoutTree" => ft.ult_us += d,
                        "Paint" => ft.paint_us += d,
                        "Layerize" | "Commit" => ft.composite_us += d,
                        "HitTest" => ft.hit_test_us += d,
                        "Layout" => ft.layout_us += d,
                        _ => {}
                    }
                }
            }
            j += 1;
        }
        tasks.push(ft);
    }

    let n = tasks.len().max(1) as f64;
    let avg = FrameTask {
        ts: 0.0,
        dur_us: tasks.iter().map(|t| t.dur_us).sum::<f64>() / n,
        js_us: tasks.iter().map(|t| t.js_us).sum::<f64>() / n,
        ult_us: tasks.iter().map(|t| t.ult_us).sum::<f64>() / n,
        paint_us: tasks.iter().map(|t| t.paint_us).sum::<f64>() / n,
        composite_us: tasks.iter().map(|t| t.composite_us).sum::<f64>() / n,
        hit_test_us: tasks.iter().map(|t| t.hit_test_us).sum::<f64>() / n,
        layout_us: tasks.iter().map(|t| t.layout_us).sum::<f64>() / n,
    };

    // Percentiles (sorted ascending by duration)
    let percentiles = if tasks.is_empty() {
        ScrollFramePercentiles {
            p50_us: 0.0,
            p90_us: 0.0,
            p99_us: 0.0,
        }
    } else {
        let mut durs: Vec<f64> = tasks.iter().map(|t| t.dur_us).collect();
        durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let percentile = |p: f64| -> f64 {
            let idx = ((p / 100.0) * (durs.len() as f64 - 1.0)).round() as usize;
            durs[idx.min(durs.len() - 1)]
        };
        ScrollFramePercentiles {
            p50_us: percentile(50.0),
            p90_us: percentile(90.0),
            p99_us: percentile(99.0),
        }
    };

    ScrollFrameResult { tasks, avg, percentiles }
}

// ── CPU Profile ──

/// CPU-profile node id -> (function name, source URL, parent id), first-wins.
/// FxHash: u64 keys on hot scan paths; SipHash costs ~2-4x on millions of
/// per-sample lookups.
pub type ProfileNodes = rustc_hash::FxHashMap<u64, (String, String, Option<u64>)>;
/// CPU-profile node id -> sampled self-time (µs).
pub type ProfileSelfTimes = rustc_hash::FxHashMap<u64, f64>;

#[derive(Clone)]
pub struct FunctionTime {
    pub function_name: String,
    pub url: String,
    pub self_time_us: f64,
    pub source_type: SourceType,
}

#[derive(Clone, PartialEq, Eq)]
pub enum SourceType {
    Runtime,  // framework/library
    AppCode,  // user application code
    Native,   // browser internals / no URL
}

impl SourceType {
    pub fn label(&self) -> &'static str {
        match self {
            SourceType::Runtime => "runtime",
            SourceType::AppCode => "app",
            SourceType::Native => "native",
        }
    }
}

fn classify_url(url: &str) -> SourceType {
    if url.is_empty() {
        return SourceType::Native;
    }
    let lower = url.to_lowercase();
    if lower.contains("node_modules")
        || lower.contains("svelte")
        || lower.contains("react")
        || lower.contains("vue")
        || lower.contains("angular")
        || lower.contains("polyfill")
        || lower.contains("vendor")
        || lower.contains("chunk-")
        || lower.contains(".min.")
    {
        SourceType::Runtime
    } else {
        SourceType::AppCode
    }
}

#[derive(Clone)]
pub struct CpuProfileResult {
    pub functions: Vec<FunctionTime>,
    pub total_sample_time_us: f64,
    pub app_time_us: f64,
    pub runtime_time_us: f64,
    pub native_time_us: f64,
}

/// Parallel ProfileChunk scan: each thread accumulates its own node/self-time
/// maps over a slice of events, results are merged afterwards.
///
/// `reserve_threads` is the number of threads that may already be busy running
/// sibling passes; CPU chunking backs off to leave room for them.
pub fn scan_profile_chunks(
    events: &[TraceEvent],
    scope: Option<&Scope>,
    reserve_threads: usize,
) -> (ProfileNodes, ProfileSelfTimes) {
    let windows = [scope.and_then(|s| s.window)];
    let threads = scan_threads(reserve_threads);
    let (nodes, mut times) = scan_profile_chunks_core(events, scope, &windows, threads);
    (nodes, times.remove(0))
}

/// Like `scan_profile_chunks` but attributes each sample to every window it
/// falls into, returning one self-time map per window. The windows are time
/// filters only; `scope` (if any) supplies the tid/pid/cat filters.
pub fn scan_profile_chunks_windows(
    events: &[TraceEvent],
    scope: Option<&Scope>,
    windows: &[Option<(f64, f64)>],
    reserve_threads: usize,
) -> (ProfileNodes, Vec<ProfileSelfTimes>) {
    scan_profile_chunks_core(events, scope, windows, scan_threads(reserve_threads))
}

fn scan_threads(reserve_threads: usize) -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(reserve_threads).max(1))
        .unwrap_or(1)
        .max(1)
}

/// Shared scan core. `windows` may be empty (attribute nothing) or contain
/// one window per output map; a `None` window matches every sample. The
/// sequential `profile_chunk_bases` walk only runs when at least one window
/// needs sample times.
fn scan_profile_chunks_core(
    events: &[TraceEvent],
    scope: Option<&Scope>,
    windows: &[Option<(f64, f64)>],
    threads: usize,
) -> (ProfileNodes, Vec<ProfileSelfTimes>) {
    const MIN_CHUNK_WORK: usize = 64_000;
    let any_window = windows.iter().any(|w| w.is_some());
    let bases = if any_window {
        profile_chunk_bases(events)
    } else {
        HashMap::default()
    };
    if threads == 1 || events.len() < MIN_CHUNK_WORK {
        return scan_profile_chunk(events, 0, scope, &bases, windows);
    }
    let chunk = events.len().div_ceil(threads);
    let bases = &bases;
    std::thread::scope(|s| {
        let handles: Vec<_> = events
            .chunks(chunk)
            .enumerate()
            .map(|(ci, c)| s.spawn(move || scan_profile_chunk(c, ci * chunk, scope, bases, windows)))
            .collect();
        let mut nodes: ProfileNodes = ProfileNodes::default();
        let mut times: Vec<ProfileSelfTimes> =
            (0..windows.len()).map(|_| ProfileSelfTimes::default()).collect();
        for h in handles {
            let (n, t) = h.join().unwrap();
            for (id, v) in n {
                nodes.entry(id).or_insert(v);
            }
            for (wi, m) in t.into_iter().enumerate() {
                for (id, dt) in m {
                    *times[wi].entry(id).or_default() += dt;
                }
            }
        }
        (nodes, times)
    })
}

/// Sequential walk over ProfileChunks: absolute sample times are not
/// derivable from a chunk alone. `timeDeltas` are *inter-sample* gaps:
/// delta[0] = gap since the previous chunk's last sample (or the profile
/// start), delta[i] = gap between sample i−1 and sample i. Each delta is
/// therefore the weight of its sample, and the absolute time of sample i is
/// `base + sum(deltas[0..=i])`, where `base` = time of the previous chunk's
/// last sample + this chunk's first delta. The walk is anchored per process
/// on the `Profile` (ph=P) event's ts. Returns the base per event index.
///
/// With `CHPERF_CHECK` set, also verifies that reconstructed sample times
/// stay inside the trace bounds — if Chrome ever changes `timeDeltas`
/// semantics, the drift shows up here instead of silently skewing windows.
fn profile_chunk_bases(events: &[TraceEvent]) -> HashMap<usize, f64> {
    let mut starts: rustc_hash::FxHashMap<u64, f64> = rustc_hash::FxHashMap::default();
    for e in events {
        if e.name == "Profile" && e.ph == b'P' {
            starts.entry(e.pid).or_insert(e.ts);
        }
    }
    let mut bases: HashMap<usize, f64> = HashMap::new();
    let mut prev_last: rustc_hash::FxHashMap<u64, f64> = rustc_hash::FxHashMap::default(); // last sample time per pid
    // Sample-time bounds (first sample of the walk, last sample per pid) for
    // the CHPERF_CHECK sanity pass.
    let mut sample_min = f64::INFINITY;
    let mut sample_max = f64::NEG_INFINITY;
    for (idx, e) in events.iter().enumerate() {
        if e.name != "ProfileChunk" {
            continue;
        }
        let Some(td) = e
            .args_value()
            .and_then(|a| a.get("data"))
            .and_then(|d| d.get("timeDeltas"))
            .and_then(|t| t.as_array())
        else {
            continue;
        };
        let Some(first) = td.first().and_then(|v| v.as_f64()) else {
            continue;
        };
        let pl = prev_last
            .get(&e.pid)
            .copied()
            .unwrap_or_else(|| starts.get(&e.pid).copied().unwrap_or(0.0));
        let base = pl + first;
        let sum: f64 = td.iter().filter_map(|v| v.as_f64()).sum();
        prev_last.insert(e.pid, pl + sum);
        if check_enabled() {
            if base < sample_min {
                sample_min = base;
            }
            let last = pl + sum;
            if last > sample_max {
                sample_max = last;
            }
        }
        bases.insert(idx, base);
    }
    if check_enabled() {
        // Compare sample bounds against the trace's own event bounds (skipping
        // metadata events, which carry process-start timestamps).
        let mut ev_min = f64::INFINITY;
        let mut ev_max = f64::NEG_INFINITY;
        for e in events {
            if crate::trace::is_metadata_event(e) {
                continue;
            }
            let end = e.ts + e.dur.unwrap_or(0.0);
            if e.ts < ev_min {
                ev_min = e.ts;
            }
            if end > ev_max {
                ev_max = end;
            }
        }
        const SLACK_US: f64 = 30_000_000.0; // 30s: profiling may start/stop before/after UI events
        if sample_min < ev_min - SLACK_US || sample_max > ev_max + SLACK_US {
            eprintln!(
                "[CHPERF_CHECK] CPU sample times {:.3}s..{:.3}s outside trace bounds {:.3}s..{:.3}s \
                 — timeDeltas semantics may have changed",
                sample_min / 1e6,
                sample_max / 1e6,
                ev_min / 1e6,
                ev_max / 1e6,
            );
        }
    }
    bases
}

/// Cached check of the `CHPERF_CHECK` env flag.
fn check_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("CHPERF_CHECK").is_ok())
}

/// Single-threaded scan of one events slice, shared by the parallel helper.
/// `global_offset` maps slice-local indices back to event indices for the
/// chunk bases lookup.
fn scan_profile_chunk(
    events: &[TraceEvent],
    global_offset: usize,
    scope: Option<&Scope>,
    bases: &HashMap<usize, f64>,
    windows: &[Option<(f64, f64)>],
) -> (ProfileNodes, Vec<ProfileSelfTimes>) {
    let mut node_map: ProfileNodes = ProfileNodes::default();
    let mut self_times: Vec<ProfileSelfTimes> =
        (0..windows.len()).map(|_| ProfileSelfTimes::default()).collect();

    for (i, e) in events.iter().enumerate() {
        if e.name != "ProfileChunk" {
            continue;
        }
        let args = match e.args_value() {
            Some(a) => a,
            None => continue,
        };
        let data = match args.get("data") {
            Some(d) => d,
            None => continue,
        };
        let cpu_profile = match data.get("cpuProfile") {
            Some(cp) => cp,
            None => continue,
        };

        if let Some(nodes) = cpu_profile.get("nodes").and_then(|n| n.as_array()) {
            for node in nodes {
                let id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let call_frame = node.get("callFrame");
                let func_name = call_frame
                    .and_then(|cf| cf.get("functionName"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("(anonymous)")
                    .to_string();
                let url = call_frame
                    .and_then(|cf| cf.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let parent = node.get("parent").and_then(|v| v.as_u64());
                node_map.entry(id).or_insert((func_name, url, parent));
            }
        }

        if let Some(scope) = scope
            && !scope.allows_chunk(e) {
                continue;
            }
        let samples = cpu_profile.get("samples").and_then(|s| s.as_array());
        let time_deltas = data.get("timeDeltas").and_then(|t| t.as_array());

        // Each `timeDeltas[i]` is the interval its sample represents (gap to
        // the previous sample; the first is the gap from the previous
        // chunk). Sample i lives at `base + prefix(deltas[0..=i])`. The old
        // code summed deltas directly (cumulative-sum inflation, ~83x on
        // long traces) and filtered chunks by the chunk's own ts, not the
        // samples'. Windowed scopes now attribute per-sample by absolute
        // sample time. With no window in play, the tight loop skips the
        // sample-time math entirely.
        if let (Some(samples), Some(deltas)) = (samples, time_deltas) {
            let n = samples.len().min(deltas.len());
            if n == 0 {
                continue;
            }
            let windowed = windows.iter().any(|w| w.is_some());
            if !windowed {
                let t0 = &mut self_times[0];
                for k in 0..n {
                    let weight = deltas[k].as_f64().unwrap_or(0.0).max(0.0);
                    if weight > 0.0 {
                        let node_id = samples[k].as_u64().unwrap_or(0);
                        *t0.entry(node_id).or_default() += weight;
                    }
                }
                continue;
            }
            let base = bases.get(&(global_offset + i)).copied().unwrap_or(0.0);
            let mut sample_ts = base;
            for k in 0..n {
                // Weights clamp negative deltas (V8 sampling jitter); the
                // time walk uses the raw deltas so timestamps stay true.
                let weight = deltas[k].as_f64().unwrap_or(0.0).max(0.0);
                if weight > 0.0 {
                    let node_id = samples[k].as_u64().unwrap_or(0);
                    for (wi, w) in windows.iter().enumerate() {
                        if w.is_none_or(|(lo, hi)| sample_ts >= lo && sample_ts <= hi) {
                            *self_times[wi].entry(node_id).or_default() += weight;
                        }
                    }
                }
                sample_ts += deltas[k].as_f64().unwrap_or(0.0);
            }
        }
    }
    (node_map, self_times)
}

pub fn analyze_cpu_profile(events: &[TraceEvent]) -> CpuProfileResult {
    let (node_map, self_times) = scan_profile_chunks(events, None, 5);
    let node_map: HashMap<u64, (String, String)> = node_map
        .into_iter()
        .map(|(id, (n, u, _))| (id, (n, u)))
        .collect();

    let mut functions: Vec<FunctionTime> = self_times
        .into_iter()
        .map(|(id, time)| {
            let (name, url) = node_map.get(&id).cloned().unwrap_or_default();
            let source_type = classify_url(&url);
            FunctionTime {
                function_name: name,
                url,
                self_time_us: time,
                source_type,
            }
        })
        .filter(|f| f.self_time_us > 0.0)
        .collect();

    functions.sort_by(|a, b| b.self_time_us.partial_cmp(&a.self_time_us).unwrap());

    let total_sample_time_us: f64 = functions.iter().map(|f| f.self_time_us).sum();
    let app_time_us: f64 = functions
        .iter()
        .filter(|f| f.source_type == SourceType::AppCode)
        .map(|f| f.self_time_us)
        .sum();
    let runtime_time_us: f64 = functions
        .iter()
        .filter(|f| f.source_type == SourceType::Runtime)
        .map(|f| f.self_time_us)
        .sum();
    let native_time_us: f64 = functions
        .iter()
        .filter(|f| f.source_type == SourceType::Native)
        .map(|f| f.self_time_us)
        .sum();

    CpuProfileResult {
        functions,
        total_sample_time_us,
        app_time_us,
        runtime_time_us,
        native_time_us,
    }
}

// ── Style Recalc (UpdateLayoutTree) ──

#[derive(Clone)]
pub struct StyleRecalcEntry {
    pub dur_us: f64,
    pub element_count: u32,
}

#[derive(Clone)]
pub struct StyleRecalcResult {
    pub entries: Vec<StyleRecalcEntry>,
    pub avg_elements: f64,
    pub max_elements: u32,
    pub total_count: usize,
}

pub fn analyze_style_recalc(events: &[TraceEvent], main_tid: u64) -> StyleRecalcResult {
    let mut entries = Vec::new();

    for e in events {
        if e.tid != main_tid || e.name != "UpdateLayoutTree" || e.ph != b'X' {
            continue;
        }
        let element_count = e
            .args_value()
            .and_then(|a| a.get("elementCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        if element_count > 0 {
            entries.push(StyleRecalcEntry {
                dur_us: e.dur.unwrap_or(0.0),
                element_count,
            });
        }
    }

    entries.sort_by_key(|b| std::cmp::Reverse(b.element_count));

    let n = entries.len().max(1) as f64;
    let avg_elements = entries.iter().map(|e| e.element_count as f64).sum::<f64>() / n;
    let max_elements = entries.iter().map(|e| e.element_count).max().unwrap_or(0);
    let total_count = entries.len();

    StyleRecalcResult {
        entries,
        avg_elements,
        max_elements,
        total_count,
    }
}

// ── Layout Dirty Objects ──

#[derive(Clone)]
pub struct LayoutDirtyEntry {
    #[allow(dead_code)]
    pub ts: f64,
    pub dur_us: f64,
    pub dirty_count: u32,
    pub total_count: u32,
}

#[derive(Clone)]
pub struct LayoutDirtyResult {
    pub entries: Vec<LayoutDirtyEntry>,
    pub avg_dirty: f64,
    pub max_dirty: u32,
    pub avg_ratio: f64,
}

pub fn analyze_layout_dirty(events: &[TraceEvent], main_tid: u64) -> LayoutDirtyResult {
    let mut entries = Vec::new();

    for e in events {
        if e.tid != main_tid || e.name != "Layout" || e.ph != b'X' {
            continue;
        }
        let args = match e.args_value() {
            Some(a) => a,
            None => continue,
        };
        let begin_data = match args.get("beginData") {
            Some(bd) => bd,
            None => continue,
        };

        let dirty = begin_data
            .get("dirtyObjects")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let total = begin_data
            .get("totalObjects")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        entries.push(LayoutDirtyEntry {
            ts: e.ts,
            dur_us: e.dur.unwrap_or(0.0),
            dirty_count: dirty,
            total_count: total,
        });
    }

    entries.sort_by_key(|b| std::cmp::Reverse(b.dirty_count));

    let n = entries.len().max(1) as f64;
    let avg_dirty = entries.iter().map(|e| e.dirty_count as f64).sum::<f64>() / n;
    let max_dirty = entries.iter().map(|e| e.dirty_count).max().unwrap_or(0);
    let avg_ratio = entries
        .iter()
        .filter(|e| e.total_count > 0)
        .map(|e| e.dirty_count as f64 / e.total_count as f64 * 100.0)
        .sum::<f64>()
        / n;

    LayoutDirtyResult {
        entries,
        avg_dirty,
        max_dirty,
        avg_ratio,
    }
}

// ── Forced Reflow (Layout Thrashing) Detection ──

#[derive(Clone)]
pub struct ForcedReflowEntry {
    pub task_dur_us: f64,
    pub reflow_count: usize, // number of JS→Layout/ULT alternations
    pub layout_time_us: f64,
}

#[derive(Clone)]
pub struct ForcedReflowResult {
    pub entries: Vec<ForcedReflowEntry>,
    pub total_reflows: usize,
    pub total_layout_time_us: f64,
}

/// Detect forced reflow: RunTask containing alternating FunctionCall→(Layout|UpdateLayoutTree)→FunctionCall pattern
pub fn analyze_forced_reflows(events: &[TraceEvent], main_tid: u64) -> ForcedReflowResult {
    let mut main_x: Vec<&TraceEvent> = events
        .iter()
        .filter(|e| e.tid == main_tid && e.ph == b'X')
        .collect();
    main_x.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap());

    let run_tasks: Vec<&TraceEvent> = main_x
        .iter()
        .copied()
        .filter(|e| e.name == "RunTask" && e.dur.is_some())
        .collect();

    let mut entries = Vec::new();

    // Single sweep, same as scroll frames: children are in ts order already.
    let mut lo = 0usize;
    for rt in &run_tasks {
        let rt_ts = rt.ts;
        let rt_end = rt_ts + rt.dur.unwrap();

        while lo < main_x.len() && main_x[lo].ts < rt_ts {
            lo += 1;
        }

        // Look for JS→Layout/ULT alternation pattern
        let mut reflow_count = 0usize;
        let mut layout_time = 0.0f64;
        let mut last_was_js = false;
        let mut j = lo;
        while j < main_x.len() && main_x[j].ts <= rt_end {
            let c = main_x[j];
            if c.name != "RunTask" && c.ts + c.dur.unwrap_or(0.0) <= rt_end {
                match c.name {
                    "FunctionCall" => {
                        last_was_js = true;
                    }
                    "Layout" | "UpdateLayoutTree" => {
                        if last_was_js {
                            reflow_count += 1;
                            layout_time += c.dur.unwrap_or(0.0);
                        }
                        last_was_js = false;
                    }
                    _ => {}
                }
            }
            j += 1;
        }

        if reflow_count >= 2 {
            entries.push(ForcedReflowEntry {
                task_dur_us: rt.dur.unwrap(),
                reflow_count,
                layout_time_us: layout_time,
            });
        }
    }

    entries.sort_by_key(|b| std::cmp::Reverse(b.reflow_count));

    let total_reflows: usize = entries.iter().map(|e| e.reflow_count).sum();
    let total_layout_time_us: f64 = entries.iter().map(|e| e.layout_time_us).sum();

    ForcedReflowResult {
        entries,
        total_reflows,
        total_layout_time_us,
    }
}

// ── Compare ──

#[derive(Clone)]
#[allow(dead_code)]
pub struct CompareRow {
    pub event_name: String,
    pub count_a: usize,
    pub count_b: usize,
    pub avg_a_us: f64,
    pub avg_b_us: f64,
    pub diff_pct: f64,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct CpuFunctionDiff {
    pub function_name: String,
    pub url: String,
    pub source_type: SourceType,
    pub time_a_us: f64,
    pub time_b_us: f64,
    pub pct_a: f64,
    pub pct_b: f64,
    pub diff_pct: f64,
}

#[derive(Clone)]
pub struct Finding {
    pub severity: FindingSeverity,
    pub category: String,
    pub message: String,
    pub detail: String,
}

#[derive(Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum FindingSeverity {
    Improved,
    Regressed,
    Neutral,
}

#[derive(Clone)]
pub struct CompareResult {
    pub rows: Vec<CompareRow>,
    pub scroll_avg_a: Option<FrameTask>,
    pub scroll_avg_b: Option<FrameTask>,
    pub scroll_pct_a: ScrollFramePercentiles,
    pub scroll_pct_b: ScrollFramePercentiles,
    pub scroll_count_a: usize,
    pub scroll_count_b: usize,
    pub summary_a: SummaryResult,
    pub summary_b: SummaryResult,
    pub cpu_diff: Vec<CpuFunctionDiff>,
    pub layout_a: LayoutDirtyResult,
    pub layout_b: LayoutDirtyResult,
    pub style_recalc_a: StyleRecalcResult,
    pub style_recalc_b: StyleRecalcResult,
    pub findings: Vec<Finding>,
}

fn pct_diff(a: f64, b: f64) -> f64 {
    if a > 0.0 {
        (b - a) / a * 100.0
    } else {
        0.0
    }
}

#[allow(clippy::too_many_arguments)]
pub fn analyze_compare(
    summary_a: &SummaryResult,
    summary_b: &SummaryResult,
    scroll_a: &ScrollFrameResult,
    scroll_b: &ScrollFrameResult,
    cpu_a: &CpuProfileResult,
    cpu_b: &CpuProfileResult,
    layout_a: &LayoutDirtyResult,
    layout_b: &LayoutDirtyResult,
    style_recalc_a: &StyleRecalcResult,
    style_recalc_b: &StyleRecalcResult,
) -> CompareResult {
    // ── Event rows ──
    let map_a: HashMap<&str, &EventTypeStat> = summary_a
        .event_stats
        .iter()
        .map(|s| (s.name, s))
        .collect();
    let map_b: HashMap<&str, &EventTypeStat> = summary_b
        .event_stats
        .iter()
        .map(|s| (s.name, s))
        .collect();

    let mut all_names: Vec<&str> = map_a.keys().chain(map_b.keys()).copied().collect();
    all_names.sort();
    all_names.dedup();

    let mut rows: Vec<CompareRow> = all_names
        .into_iter()
        .map(|name| {
            let a = map_a.get(name);
            let b = map_b.get(name);
            let avg_a = a.map(|s| s.avg_time_us).unwrap_or(0.0);
            let avg_b = b.map(|s| s.avg_time_us).unwrap_or(0.0);
            CompareRow {
                event_name: name.to_string(),
                count_a: a.map(|s| s.count).unwrap_or(0),
                count_b: b.map(|s| s.count).unwrap_or(0),
                avg_a_us: avg_a,
                avg_b_us: avg_b,
                diff_pct: pct_diff(avg_a, avg_b),
            }
        })
        .collect();

    rows.sort_by(|a, b| b.diff_pct.abs().partial_cmp(&a.diff_pct.abs()).unwrap());

    // ── Scroll frame ──
    let scroll_avg_a = if scroll_a.tasks.is_empty() {
        None
    } else {
        Some(scroll_a.avg.clone())
    };
    let scroll_avg_b = if scroll_b.tasks.is_empty() {
        None
    } else {
        Some(scroll_b.avg.clone())
    };

    // ── CPU profile diff ──
    let total_a = cpu_a.total_sample_time_us;
    let total_b = cpu_b.total_sample_time_us;

    let mut func_map_a: rustc_hash::FxHashMap<(&str, &str), &FunctionTime> = rustc_hash::FxHashMap::default();
    for f in &cpu_a.functions {
        func_map_a
            .entry((&f.function_name, &f.url))
            .or_insert(f);
    }
    let mut func_map_b: rustc_hash::FxHashMap<(&str, &str), &FunctionTime> = rustc_hash::FxHashMap::default();
    for f in &cpu_b.functions {
        func_map_b
            .entry((&f.function_name, &f.url))
            .or_insert(f);
    }

    let mut all_funcs: Vec<(&str, &str)> = func_map_a
        .keys()
        .chain(func_map_b.keys())
        .copied()
        .collect();
    all_funcs.sort();
    all_funcs.dedup();

    let mut cpu_diff: Vec<CpuFunctionDiff> = all_funcs
        .into_iter()
        .map(|(name, url)| {
            let fa = func_map_a.get(&(name, url));
            let fb = func_map_b.get(&(name, url));
            let time_a = fa.map(|f| f.self_time_us).unwrap_or(0.0);
            let time_b = fb.map(|f| f.self_time_us).unwrap_or(0.0);
            let pct_a = if total_a > 0.0 { time_a / total_a * 100.0 } else { 0.0 };
            let pct_b = if total_b > 0.0 { time_b / total_b * 100.0 } else { 0.0 };
            let source = fa
                .map(|f| f.source_type.clone())
                .or_else(|| fb.map(|f| f.source_type.clone()))
                .unwrap_or(SourceType::Native);
            CpuFunctionDiff {
                function_name: name.to_string(),
                url: url.to_string(),
                source_type: source,
                time_a_us: time_a,
                time_b_us: time_b,
                pct_a,
                pct_b,
                diff_pct: pct_diff(time_a, time_b),
            }
        })
        .filter(|d| d.time_a_us > 0.0 || d.time_b_us > 0.0)
        .collect();

    // Sort by absolute diff in percentage points (most impactful first)
    cpu_diff.sort_by(|a, b| {
        (b.pct_b - b.pct_a)
            .abs()
            .partial_cmp(&(a.pct_b - a.pct_a).abs())
            .unwrap()
    });
    cpu_diff.truncate(30); // top 30

    // ── Findings ──
    let mut findings = Vec::new();

    // Long task comparison
    let lt_diff = pct_diff(
        summary_a.long_task_count as f64,
        summary_b.long_task_count as f64,
    );
    if lt_diff.abs() > 10.0 {
        findings.push(Finding {
            severity: if lt_diff < 0.0 {
                FindingSeverity::Improved
            } else {
                FindingSeverity::Regressed
            },
            category: "Long Tasks".to_string(),
            message: format!(
                "{} -> {} ({:+.0}%)",
                summary_a.long_task_count, summary_b.long_task_count, lt_diff
            ),
            detail: if lt_diff < 0.0 {
                "Fewer long tasks blocking the main thread".to_string()
            } else {
                "More long tasks blocking the main thread".to_string()
            },
        });
    }

    // Scroll frame duration
    if let (Some(sa), Some(sb)) = (&scroll_avg_a, &scroll_avg_b) {
        let dur_diff = pct_diff(sa.dur_us, sb.dur_us);
        if dur_diff.abs() > 5.0 {
            findings.push(Finding {
                severity: if dur_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Scroll Duration".to_string(),
                message: format!("{:+.1}% avg scroll task time", dur_diff),
                detail: format!(
                    "Bottleneck: {} -> {}",
                    sa.bottleneck(),
                    sb.bottleneck()
                ),
            });
        }

        // JS time
        let js_diff = pct_diff(sa.js_us, sb.js_us);
        if js_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if js_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "JS in Scroll".to_string(),
                message: format!("{:+.1}% JS execution time", js_diff),
                detail: String::new(),
            });
        }

        // Style/ULT time
        let ult_diff = pct_diff(sa.ult_us, sb.ult_us);
        if ult_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if ult_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Style Recalc".to_string(),
                message: format!("{:+.1}% UpdateLayoutTree time", ult_diff),
                detail: String::new(),
            });
        }

        // Layout time
        let layout_diff = pct_diff(sa.layout_us, sb.layout_us);
        if layout_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if layout_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Layout".to_string(),
                message: format!("{:+.1}% layout time", layout_diff),
                detail: String::new(),
            });
        }

        // Paint time
        let paint_diff = pct_diff(sa.paint_us, sb.paint_us);
        if paint_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if paint_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Paint".to_string(),
                message: format!("{:+.1}% paint time", paint_diff),
                detail: String::new(),
            });
        }

        // HitTest time
        let hit_diff = pct_diff(sa.hit_test_us, sb.hit_test_us);
        if hit_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if hit_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "HitTest".to_string(),
                message: format!("{:+.1}% hit test time", hit_diff),
                detail: String::new(),
            });
        }

        // Composite time
        let comp_diff = pct_diff(sa.composite_us, sb.composite_us);
        if comp_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if comp_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Composite".to_string(),
                message: format!("{:+.1}% composite time", comp_diff),
                detail: String::new(),
            });
        }
    }

    // Main thread busy
    let busy_diff = pct_diff(summary_a.main_thread_busy_us, summary_b.main_thread_busy_us);
    if busy_diff.abs() > 10.0 {
        findings.push(Finding {
            severity: if busy_diff < 0.0 {
                FindingSeverity::Improved
            } else {
                FindingSeverity::Regressed
            },
            category: "Main Thread".to_string(),
            message: format!("{:+.1}% total busy time", busy_diff),
            detail: String::new(),
        });
    }

    // Layout dirty
    let dirty_diff = pct_diff(layout_a.avg_dirty, layout_b.avg_dirty);
    if dirty_diff.abs() > 15.0 {
        findings.push(Finding {
            severity: if dirty_diff < 0.0 {
                FindingSeverity::Improved
            } else {
                FindingSeverity::Regressed
            },
            category: "Layout Dirty".to_string(),
            message: format!(
                "Avg dirty {:.0} -> {:.0} ({:+.1}%)",
                layout_a.avg_dirty, layout_b.avg_dirty, dirty_diff
            ),
            detail: String::new(),
        });
    }

    // Style recalc elements
    if style_recalc_a.total_count > 0 && style_recalc_b.total_count > 0 {
        let elem_diff = pct_diff(style_recalc_a.avg_elements, style_recalc_b.avg_elements);
        if elem_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if elem_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Style Elements".to_string(),
                message: format!(
                    "Avg {:.0} -> {:.0} ({:+.1}%)",
                    style_recalc_a.avg_elements, style_recalc_b.avg_elements, elem_diff
                ),
                detail: format!(
                    "Max {} -> {}",
                    style_recalc_a.max_elements, style_recalc_b.max_elements
                ),
            });
        }
    }

    // Sort findings: regressions first, then improvements
    findings.sort_by(|a, b| {
        let ord_a = match a.severity {
            FindingSeverity::Regressed => 0,
            FindingSeverity::Improved => 1,
            FindingSeverity::Neutral => 2,
        };
        let ord_b = match b.severity {
            FindingSeverity::Regressed => 0,
            FindingSeverity::Improved => 1,
            FindingSeverity::Neutral => 2,
        };
        ord_a.cmp(&ord_b)
    });

    CompareResult {
        rows,
        scroll_avg_a,
        scroll_avg_b,
        scroll_pct_a: scroll_a.percentiles.clone(),
        scroll_pct_b: scroll_b.percentiles.clone(),
        scroll_count_a: scroll_a.tasks.len(),
        scroll_count_b: scroll_b.tasks.len(),
        summary_a: summary_a.clone(),
        summary_b: summary_b.clone(),
        cpu_diff,
        layout_a: layout_a.clone(),
        layout_b: layout_b.clone(),
        style_recalc_a: style_recalc_a.clone(),
        style_recalc_b: style_recalc_b.clone(),
        findings,
    }
}

// ── Jank Clusters (spikes below the 50ms Long Task threshold) ──

#[derive(Clone)]
pub struct JankCluster {
    pub start_us: f64,
    pub end_us: f64,
    pub busy_us: f64, // total main-thread RunTask time in the window
    pub max_run_us: f64,
    pub max_faf_us: f64,
    pub max_gpu_us: f64,
    pub dropped_frames: usize,
    /// Top FunctionCall names by total duration in the window
    pub top_calls: Vec<(String, f64)>,
}

#[derive(Clone)]
pub struct JankResult {
    pub clusters: Vec<JankCluster>,
    pub total_dropped: usize,
    pub bucket_ms: f64,
}

/// Frame budget at 60fps (µs). Spikes above this mean dropped frames.
const FRAME_BUDGET_US: f64 = 16_667.0;

/// A single hot bucket (or any bucket containing dropped frames).
fn bucket_hot(busy: f64, max_run: f64, max_faf: f64, max_gpu: f64, dropped: u32) -> bool {
    dropped > 0
        || max_run >= FRAME_BUDGET_US
        || max_faf >= FRAME_BUDGET_US
        || max_gpu >= FRAME_BUDGET_US
        || busy >= 50_000.0
}

/// Detect jank clusters across the whole trace: 1-second-ish buckets where
/// dropped frames, ≥16.7ms spikes (RunTask/FireAnimationFrame/GPUTask) or
/// heavy main-thread busy occurred. Adjacent hot buckets merge into
/// clusters, ranked by dropped frames → worst spike → busy. For each top
/// cluster, the dominating FunctionCalls are collected (the "what happened"
/// chain). This catches spikes that the 50ms Long Task summary misses.
pub fn analyze_jank(events: &[TraceEvent], main_tid: u64, scope: Option<&Scope>) -> JankResult {
    let min_ts = events
        .iter()
        .filter(|e| !crate::trace::is_metadata_event(e))
        .map(|e| e.ts)
        .fold(f64::INFINITY, f64::min);
    let max_ts = events
        .iter()
        .fold(0.0f64, |acc, e| acc.max(e.ts + e.dur.unwrap_or(0.0)));
    let span_us = (max_ts - min_ts).max(1.0);

    let bucket_ms = (span_us / 1000.0 / 2000.0).clamp(50.0, 1000.0);
    let bucket_us = bucket_ms * 1000.0;
    let n = ((span_us / bucket_us).ceil() as usize).max(1);
    let mut busy = vec![0.0f64; n];
    let mut max_run = vec![0.0f64; n];
    let mut max_faf = vec![0.0f64; n];
    let mut max_gpu = vec![0.0f64; n];
    let mut dropped = vec![0u32; n];

    // When scoped, events outside the window are ignored entirely: hot-bucket
    // merging must not cross the window boundary either.
    let win_lo = scope.and_then(|s| s.window).map(|(lo, _)| lo);
    let win_hi = scope.and_then(|s| s.window).map(|(_, hi)| hi);

    for e in events {
        if e.ts < min_ts || e.ts > max_ts {
            continue;
        }
        if let (Some(lo), Some(hi)) = (win_lo, win_hi)
            && (e.ts < lo || e.ts > hi) {
                continue;
            }
        let b = (((e.ts - min_ts) / bucket_us) as usize).min(n - 1);
        match e.name {
            "RunTask" if e.ph == b'X' && e.tid == main_tid => {
                if let Some(d) = e.dur {
                    busy[b] += d;
                    if d > max_run[b] {
                        max_run[b] = d;
                    }
                }
            }
            "FireAnimationFrame" if e.ph == b'X' && e.tid == main_tid => {
                if let Some(d) = e.dur
                    && d > max_faf[b] {
                        max_faf[b] = d;
                    }
            }
            "GPUTask" if e.ph == b'X' => {
                if let Some(d) = e.dur
                    && d > max_gpu[b] {
                        max_gpu[b] = d;
                    }
            }
            "DroppedFrame" => {
                dropped[b] += 1;
            }
            _ => {}
        }
    }

    // Merge adjacent hot buckets into clusters.
    struct Acc {
        start: usize,
        end: usize,
        busy: f64,
        max_run: f64,
        max_faf: f64,
        max_gpu: f64,
        dropped: u32,
    }
    let mut accs: Vec<Acc> = Vec::new();
    for b in 0..n {
        let hot = bucket_hot(busy[b], max_run[b], max_faf[b], max_gpu[b], dropped[b]);
        if !hot {
            continue;
        }
        if scope.and_then(|s| s.window).is_some() {
            // Windowed: never merge across buckets outside the window.
            let b_lo = min_ts + b as f64 * bucket_us;
            let b_hi = b_lo + bucket_us;
            let in_win = win_lo.is_none_or(|w| b_hi > w) && win_hi.is_none_or(|w| b_lo < w);
            if !in_win {
                continue;
            }
        }
        match (accs.last_mut(), hot) {
            (Some(a), true) if a.end + 1 == b => {
                a.end = b;
                a.busy += busy[b];
                a.max_run = a.max_run.max(max_run[b]);
                a.max_faf = a.max_faf.max(max_faf[b]);
                a.max_gpu = a.max_gpu.max(max_gpu[b]);
                a.dropped += dropped[b];
            }
            (_, true) => accs.push(Acc {
                start: b,
                end: b,
                busy: busy[b],
                max_run: max_run[b],
                max_faf: max_faf[b],
                max_gpu: max_gpu[b],
                dropped: dropped[b],
            }),
            _ => {}
        }
    }

    let total_dropped: usize = dropped.iter().map(|&d| d as usize).sum();

    // Rank: dropped frames first, then worst spike, then busy.
    accs.sort_by(|a, b| {
        b.dropped
            .cmp(&a.dropped)
            .then_with(|| b.max_run.partial_cmp(&a.max_run).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.busy.partial_cmp(&a.busy).unwrap_or(std::cmp::Ordering::Equal))
    });
    let top = accs.into_iter().take(8).collect::<Vec<_>>();

    // Mark buckets belonging to top clusters, then collect the dominating
    // FunctionCalls per cluster in one extra pass.
    let mut in_cluster: Vec<Option<usize>> = vec![None; n];
    for (ci, a) in top.iter().enumerate() {
        in_cluster[a.start..=a.end].fill(Some(ci));
    }
    let mut calls: Vec<rustc_hash::FxHashMap<String, f64>> = (0..top.len()).map(|_| rustc_hash::FxHashMap::default()).collect();
    for e in events {
        if e.name != "FunctionCall" || e.ph != b'X' {
            continue;
        }
        if let (Some(lo), Some(hi)) = (win_lo, win_hi)
            && (e.ts < lo || e.ts > hi) {
                continue;
            }
        let b = (((e.ts - min_ts) / bucket_us) as usize).min(n - 1);
        if let Some(ci) = in_cluster[b]
            && let Some(d) = e.dur
                && let Some(name) = e
                    .args_value()
                    .and_then(|a| a.get("data"))
                    .and_then(|d| d.get("functionName"))
                    .and_then(|v| v.as_str())
                    && !name.is_empty() {
                        *calls[ci].entry(name.to_string()).or_default() += d;
                    }
    }

    let clusters: Vec<JankCluster> = top
        .into_iter()
        .enumerate()
        .map(|(ci, a)| {
            let mut top_calls: Vec<(String, f64)> =
                std::mem::take(&mut calls[ci]).into_iter().collect();
            top_calls.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
            top_calls.truncate(5);
            JankCluster {
                start_us: min_ts + a.start as f64 * bucket_us,
                end_us: min_ts + (a.end + 1) as f64 * bucket_us,
                busy_us: a.busy,
                max_run_us: a.max_run,
                max_faf_us: a.max_faf,
                max_gpu_us: a.max_gpu,
                dropped_frames: a.dropped as usize,
                top_calls,
            }
        })
        .collect();

    JankResult {
        clusters,
        total_dropped,
        bucket_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::TraceEvent;

    fn profile_event(ts: f64) -> TraceEvent {
        TraceEvent {
            name: "Profile",
            ph: b'P',
            ts,
            dur: None,
            tid: 65,
            pid: 2,
            cat: None,
            args: None,
            args_cache: std::sync::OnceLock::new(),
        }
    }

    fn chunk_event(ts: f64, nodes: &[(u64, &str, Option<u64>)], samples: &[u64], deltas: &[f64]) -> TraceEvent {
        let nodes_json: Vec<serde_json::Value> = nodes
            .iter()
            .map(|(id, name, parent)| {
                serde_json::json!({"id": id, "callFrame": {"functionName": name, "url": ""}, "parent": parent})
            })
            .collect();
        TraceEvent {
            name: "ProfileChunk",
            ph: b'P',
            ts,
            dur: None,
            tid: 65,
            pid: 2,
            cat: None,
            args: crate::trace::test_args(serde_json::json!({
                "data": {
                    "cpuProfile": {"nodes": nodes_json, "samples": samples},
                    "timeDeltas": deltas,
                }
            })),
            args_cache: std::sync::OnceLock::new(),
        }
    }

    fn plain_event(ts: f64, name: &str) -> TraceEvent {
        TraceEvent {
            name: crate::trace::intern_name(name),
            ph: b'X',
            ts,
            dur: Some(100.0),
            tid: 1,
            pid: 2,
            cat: None,
            args: None,
            args_cache: std::sync::OnceLock::new(),
        }
    }

    const NODES_ABC: [(u64, &str, Option<u64>); 3] =
        [(1, "(root)", None), (2, "shoot", Some(1)), (3, "update", Some(1))];

    /// Fixture mirroring tests/fixtures/tiny.json: profile starts at 1e6,
    /// chunk 1 = 7 samples of 5ms (shoot/update alternating), chunk 2 has a
    /// negative delta that must be clamped. Expected totals:
    /// total 46ms, shoot 26ms (15 + 11), update 15ms.
    fn fixture_events() -> Vec<TraceEvent> {
        vec![
            profile_event(1_000_000.0),
            chunk_event(1_055_000.0, &NODES_ABC, &[2, 3, 2, 3, 2, 3, 2], &[5000.0; 7]),
            chunk_event(
                1_095_000.0,
                &[(1, "(root)", None), (4, "shoot", Some(1))],
                &[4, 4, 4, 4],
                &[4000.0, 3000.0, -1000.0, 4000.0],
            ),
            plain_event(1_000_000.0, "RunTask"),
        ]
    }

    fn total(times: &ProfileSelfTimes) -> f64 {
        times.values().sum()
    }

    #[test]
    fn scan_attribution_weights_are_per_sample_deltas() {
        // Regression: the old code summed cumulative deltas (~83x inflation);
        // weights must be the deltas themselves, clamped at zero.
        let events = fixture_events();
        let (nodes, times) = scan_profile_chunks(&events, None, 0);
        assert_eq!(nodes.len(), 4);
        assert_eq!(total(&times), 46_000.0);
        assert_eq!(*times.get(&2).unwrap_or(&0.0), 20_000.0); // 4 × 5ms (idx 0,2,4,6)
        assert_eq!(*times.get(&3).unwrap_or(&0.0), 15_000.0);
        // Node 4: 4000 + 3000 + clamp(-1000 → 0) + 4000.
        assert_eq!(*times.get(&4).unwrap_or(&0.0), 11_000.0);
    }

    #[test]
    fn scan_windowed_attributes_by_sample_time_not_chunk_ts() {
        let events = fixture_events();
        let scope = Scope {
            window: Some((1_000_000.0, 1_030_000.0)),
            tid: None,
            pid: None,
            cat: None,
        };
        let (_, times) = scan_profile_chunks(&events, Some(&scope), 0);
        assert_eq!(total(&times), 30_000.0); // 6 × 5ms samples ending at 1_030_000

        let scope2 = Scope {
            window: Some((1_040_000.0, 1_050_000.0)),
            tid: None,
            pid: None,
            cat: None,
        };
        let (_, times2) = scan_profile_chunks(&events, Some(&scope2), 0);
        assert_eq!(total(&times2), 7_000.0); // 3000 + 0 + 4000 (negative clamped)

        // Windowed totals never exceed the full-trace total.
        assert!(total(&times) <= 46_000.0 && total(&times2) <= 46_000.0);
    }

    #[test]
    fn scan_multi_window_shares_one_walk() {
        let events = fixture_events();
        let windows: [Option<(f64, f64)>; 2] = [
            Some((1_000_000.0, 1_030_000.0)),
            Some((1_040_000.0, 1_050_000.0)),
        ];
        let (_, times) = scan_profile_chunks_windows(&events, None, &windows, 0);
        assert_eq!(times.len(), 2);
        assert_eq!(total(&times[0]), 30_000.0);
        assert_eq!(total(&times[1]), 7_000.0);
        // A None window matches everything (tid-filter-only scopes).
        let (_, times) = scan_profile_chunks_windows(&events, None, &[None], 0);
        assert_eq!(total(&times[0]), 46_000.0);
    }

    #[test]
    fn scan_parallel_matches_serial_exactly() {
        // Deterministic LCG data big enough to engage the parallel split
        // (MIN_CHUNK_WORK = 64k events), with per-chunk reset deltas and
        // occasional negative values.
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut rng = move || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u64
        };
        let mut events: Vec<TraceEvent> = Vec::new();
        for i in 0..22_000u64 {
            let samples: Vec<u64> = (0..8).map(|_| (rng() % 4) + 1).collect();
            let deltas: Vec<f64> = (0..8).map(|_| (rng() % 11) as f64 - 1.0).collect();
            events.push(chunk_event(
                1_000_000.0 + i as f64 * 10_000.0,
                &[(1, "(root)", None), (2, "a", Some(1)), (3, "b", Some(1)), (4, "c", Some(1))],
                &samples,
                &deltas,
            ));
            events.push(plain_event(1_000_000.0 + i as f64 * 10_000.0, "RunTask"));
            events.push(plain_event(1_000_000.0 + i as f64 * 10_000.0 + 1.0, "Paint"));
        }
        assert!(events.len() >= 64_000);

        let (nodes_serial, times_serial) = scan_profile_chunks_core(&events, None, &[None], 1);
        let (nodes_par, times_par) = scan_profile_chunks_core(&events, None, &[None], 8);
        assert_eq!(nodes_serial.len(), nodes_par.len());
        for (id, v) in &nodes_serial {
            assert_eq!(nodes_par.get(id), Some(v));
        }
        assert_eq!(times_serial[0].len(), times_par[0].len());
        for (id, t) in &times_serial[0] {
            assert_eq!(times_par[0].get(id), Some(t), "node {} self-time diverged", id);
        }
    }
}
