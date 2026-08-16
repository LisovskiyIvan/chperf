//! CPU profile: parallel ProfileChunk scan, self-time attribution by sample,
//! and the reusable full-trace cache used by REPL queries.

use crate::inspect::Scope;
use crate::trace::{TraceEvent, is_metadata_event};
use std::collections::HashMap;
use std::sync::Arc;

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
            if is_metadata_event(e) {
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

/// Test-only convenience wrapper; production code uses `analyze_cpu_profile_full`
/// so the raw node table + self-times can be kept for the REPL cache.
#[cfg(test)]
pub fn analyze_cpu_profile(events: &[TraceEvent]) -> CpuProfileResult {
    analyze_cpu_profile_full(events).0
}

/// Full-trace CPU profile plus its reusable cache. The cache keeps the raw
/// node table and self-times (which `analyze_cpu_profile` would otherwise
/// drop) so REPL queries with an empty scope can skip re-scanning the trace.
pub fn analyze_cpu_profile_full(events: &[TraceEvent]) -> (CpuProfileResult, CpuProfileCache) {
    let (node_map, self_times) = scan_profile_chunks(events, None, 5);
    let result = build_cpu_profile_result(&node_map, &self_times);
    (result, CpuProfileCache { nodes: Arc::new(node_map), self_times })
}

fn build_cpu_profile_result(node_map: &ProfileNodes, self_times: &ProfileSelfTimes) -> CpuProfileResult {
    let mut functions: Vec<FunctionTime> = self_times
        .iter()
        .map(|(id, time)| {
            // Clone only the (name, url) of nodes that were actually sampled —
            // rebuilding the whole node table first would clone every node's
            // strings, including the (often large) unsampled majority.
            let (name, url) = node_map
                .get(id)
                .map(|(n, u, _)| (n.clone(), u.clone()))
                .unwrap_or_default();
            let source_type = classify_url(&url);
            FunctionTime {
                function_name: name,
                url,
                self_time_us: *time,
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

/// Reusable full-trace CPU profile (node table + per-node self-time).
///
/// `nodes` is `Arc`-shared: it is only ever read (by id), so cached REPL
/// queries bump a reference count instead of re-cloning the whole node table
/// (thousands of nodes × two `String`s) on every request.
pub struct CpuProfileCache {
    pub nodes: Arc<ProfileNodes>,
    pub self_times: ProfileSelfTimes,
}

impl CpuProfileCache {
    /// True when `scope` is the full trace (no time/thread/process/category
    /// filter), so the cached full-trace profile applies verbatim.
    pub fn serves(&self, scope: &Scope) -> bool {
        scope.window.is_none() && scope.tid.is_none() && scope.pid.is_none() && scope.cat.is_none()
    }
}

/// Node table + self-times for `scope`, reused from `cache` when the scope is
/// the full trace (a clone of the already-built maps), otherwise a fresh
/// parallel scan.
pub fn cpu_profile_for(
    events: &[TraceEvent],
    scope: &Scope,
    cache: Option<&CpuProfileCache>,
) -> (Arc<ProfileNodes>, ProfileSelfTimes) {
    match cache.filter(|c| c.serves(scope)) {
        Some(c) => (Arc::clone(&c.nodes), c.self_times.clone()),
        None => {
            let (nodes, times) = scan_profile_chunks(events, Some(scope), 0);
            (Arc::new(nodes), times)
        }
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
            state >> 33
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

    #[test]
    fn cpu_profile_cache_matches_fresh_scan_on_full_scope() {
        // The REPL cache is only consulted for an empty scope; it must return
        // exactly what a fresh full-trace scan would.
        let events = fixture_events();
        let empty = Scope { window: None, tid: None, pid: None, cat: None };
        let (result, cache) = analyze_cpu_profile_full(&events);
        assert!(cache.serves(&empty));
        assert!(!cache.serves(&Scope { window: Some((0.0, 1.0)), tid: None, pid: None, cat: None }));

        let (cached_nodes, cached_times) = cpu_profile_for(&events, &empty, Some(&cache));
        let (fresh_nodes, fresh_times) = cpu_profile_for(&events, &empty, None);
        assert_eq!(cached_nodes.len(), fresh_nodes.len());
        for (id, v) in cached_nodes.iter() {
            assert_eq!(fresh_nodes.get(id), Some(v));
        }
        assert_eq!(cached_times.len(), fresh_times.len());
        for (id, t) in &cached_times {
            assert_eq!(fresh_times.get(id), Some(t));
        }
        // The built result agrees with the raw self-times.
        assert!(result.total_sample_time_us > 0.0);
    }
}
