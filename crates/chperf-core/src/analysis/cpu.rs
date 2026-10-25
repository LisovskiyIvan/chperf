//! CPU profile: parallel ProfileChunk scan, self-time attribution by sample,
//! and the reusable full-trace cache used by windowed queries.

use crate::inspect::Scope;
use crate::trace::{TraceEvent, is_metadata_event};
use std::collections::HashMap;
use std::sync::Arc;

/// (pid, session-subkey, generation): V8 restarts its node-id counter per
/// session, so sample walks and node ids must be scoped per session, not
/// per (pid, tid). The subkey is the Profile/ProfileChunk head's `id` when
/// present, else the sampled `tid`; the generation bumps every time a fresh
/// `Profile` (ph `P`) event reuses an already-seen key, so two isolates
/// sharing pid+tid never merge into one session.
///
/// FxHash: tuple keys on hot scan paths; SipHash costs ~2-4x on millions of
/// per-sample lookups.
pub type SessionKey = (u64, u64, u32);
/// (pid, session-subkey, generation, node id): node ids are only unique
/// within one session.
pub type NodeKey = (u64, u64, u32, u64);
/// Node table: key -> (functionName, url, per-session parent node id).
pub type ProfileNodes = rustc_hash::FxHashMap<NodeKey, (String, String, Option<u64>)>;
/// CPU-profile node -> sampled self-time (µs).
pub type ProfileSelfTimes = rustc_hash::FxHashMap<NodeKey, f64>;

/// Session assignment for a whole event list: one table index per event
/// plus the interned [`SessionKey`] table. Indices are `u32` (4B/event, not
/// 24B): sessions number in the dozens, so the table is tiny.
///
/// The session key is `(pid, id)` when the event carries the
/// Profile/ProfileChunk head's `id`, else `(pid, tid)` — and a fresh
/// `Profile` (ph `P`) event on an already-seen key starts a NEW generation
/// instead of extending the old session. Without the generation split, two
/// isolates sharing pid+tid whose chunks lack a head `id` merge into one
/// session: node ids collide and sample walks continue each other.
///
/// Id-less chunks attach to the latest `Profile` on their tid (a head with
/// an `id` still anchors its tid's chunks).
pub(crate) fn assign_sessions(events: &[TraceEvent]) -> (Vec<u32>, Vec<SessionKey>) {
    let mut generation: rustc_hash::FxHashMap<(u64, u64), u32> = rustc_hash::FxHashMap::default();
    let mut cur: rustc_hash::FxHashMap<(u64, u64), SessionKey> = rustc_hash::FxHashMap::default();
    let mut table: Vec<SessionKey> = Vec::new();
    let mut index: rustc_hash::FxHashMap<SessionKey, u32> = rustc_hash::FxHashMap::default();
    let mut out: Vec<u32> = Vec::with_capacity(events.len());
    // Consecutive events usually share (pid, id/tid): 1-entry caches skip
    // both hash lookups on the hot path (same-thread runs dominate traces).
    let mut last_base = (u64::MAX, u64::MAX);
    let mut last_sess: SessionKey = (u64::MAX, u64::MAX, u32::MAX);
    let mut last_idx = u32::MAX;
    for e in events {
        let base = (e.pid, if e.has_id { e.id } else { e.tid });
        let sess = if e.name == "Profile" && e.ph == b'P' {
            let g = generation.entry(base).or_insert(0);
            *g += 1;
            let s = (base.0, base.1, *g);
            cur.insert(base, s);
            cur.insert((e.pid, e.tid), s);
            s
        } else if base == last_base {
            last_sess
        } else {
            *cur.get(&base).unwrap_or(&(base.0, base.1, 0))
        };
        let idx = if sess == last_sess {
            last_idx
        } else {
            match index.get(&sess) {
                Some(&i) => i,
                None => {
                    let i = table.len() as u32;
                    table.push(sess);
                    index.insert(sess, i);
                    i
                }
            }
        };
        out.push(idx);
        last_base = base;
        last_sess = sess;
        last_idx = idx;
    }
    (out, table)
}

#[derive(Clone)]
pub struct FunctionTime {
    pub function_name: String,
    pub url: String,
    pub self_time_us: f64,
    pub source_type: SourceType,
}

/// How a sampled function is attributed. Percentages derived from
/// `CpuProfileResult` totals (`app_time_us` etc. over `total_sample_time_us`)
/// are shares **of sampled time** — the denominator is profiler sample
/// weight, not trace wall-clock time.
#[derive(Clone, PartialEq, Eq)]
pub enum SourceType {
    /// Framework/library code: `node_modules` or a known framework package
    /// (`svelte`, `react`, `vue`, `angular`, `polyfill`). Bundler output
    /// (`chunk-*`, `.min.`, `vendor`) is NOT runtime — it is usually the
    /// app's own code after bundling.
    Runtime, // framework/library
    /// User application code: anything with a URL that is not `Runtime`.
    AppCode, // user application code
    /// Browser internals: no URL at all.
    Native, // browser internals / no URL
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

/// Classify a script URL. Only `node_modules` and known framework packages
/// count as `Runtime`; app bundles (`chunk-*`, `.min.js`, `vendor`, …) are
/// the app's own code after bundling and classify as `AppCode`. Empty URL =
/// `Native` (browser internals).
fn classify_url(url: &str) -> SourceType {
    if url.is_empty() {
        return SourceType::Native;
    }
    // Allocation-free ASCII matching (all markers are lowercase ASCII).
    if crate::inspect::contains_ignore_case(url, "node_modules")
        || crate::inspect::contains_ignore_case(url, "svelte")
        || crate::inspect::contains_ignore_case(url, "react")
        || crate::inspect::contains_ignore_case(url, "vue")
        || crate::inspect::contains_ignore_case(url, "angular")
        || crate::inspect::contains_ignore_case(url, "polyfill")
    {
        SourceType::Runtime
    } else {
        SourceType::AppCode
    }
}

#[derive(Clone)]
pub struct CpuProfileResult {
    pub functions: Vec<FunctionTime>,
    /// Total profiler sample weight (µs). This is the denominator of every
    /// "of sampled time" percentage — not trace wall-clock time.
    pub total_sample_time_us: f64,
    /// Sample time attributed to `AppCode` functions (of sampled time).
    pub app_time_us: f64,
    /// Sample time attributed to `Runtime` functions (of sampled time).
    pub runtime_time_us: f64,
    /// Sample time attributed to `Native` functions (of sampled time).
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
    let (sessions, session_table) = assign_sessions(events);
    let (nodes, mut times) =
        scan_profile_chunks_core(events, scope, &windows, &sessions, &session_table, threads);
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
    let (sessions, session_table) = assign_sessions(events);
    scan_profile_chunks_core(
        events,
        scope,
        windows,
        &sessions,
        &session_table,
        scan_threads(reserve_threads),
    )
}

#[derive(serde::Deserialize)]
struct ChunkDeltasOnly {
    data: Option<DeltasData>,
}

#[derive(serde::Deserialize)]
struct DeltasData {
    #[serde(rename = "timeDeltas")]
    time_deltas: Option<Vec<f64>>,
}

#[derive(serde::Deserialize)]
struct ChunkFull<'a> {
    #[serde(borrow)]
    data: Option<ChunkDataFull<'a>>,
}

#[derive(serde::Deserialize)]
struct ChunkDataFull<'a> {
    #[serde(rename = "cpuProfile", borrow)]
    cpu_profile: Option<CpuProfileFull<'a>>,
    #[serde(rename = "timeDeltas")]
    time_deltas: Option<Vec<f64>>,
}

#[derive(serde::Deserialize)]
struct CpuProfileFull<'a> {
    #[serde(borrow)]
    nodes: Option<Vec<ProfileNodeRaw<'a>>>,
    samples: Option<Vec<u64>>,
}

#[derive(serde::Deserialize)]
struct ProfileNodeRaw<'a> {
    #[serde(default)]
    id: u64,
    #[serde(rename = "callFrame", borrow)]
    call_frame: Option<CallFrameRaw<'a>>,
    parent: Option<u64>,
}

#[derive(serde::Deserialize)]
struct CallFrameRaw<'a> {
    #[serde(rename = "functionName", borrow)]
    function_name: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    url: Option<std::borrow::Cow<'a, str>>,
}

fn scan_threads(reserve_threads: usize) -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(reserve_threads).clamp(1, 4))
        .unwrap_or(1)
}

/// Shared scan core. `windows` may be empty (attribute nothing) or contain
/// one window per output map; a `None` window matches every sample. The
/// sequential `profile_chunk_bases` walk always runs: windowed attribution
/// needs sample-time bases, and the profiler-attach cap needs each pid's
/// first chunk even without windows.
fn scan_profile_chunks_core(
    events: &[TraceEvent],
    scope: Option<&Scope>,
    windows: &[Option<(f64, f64)>],
    sessions: &[u32],
    session_table: &[SessionKey],
    threads: usize,
) -> (ProfileNodes, Vec<ProfileSelfTimes>) {
    const MIN_CHUNK_WORK: usize = 64_000;
    let any_window = windows.iter().any(|w| w.is_some());
    let (bases, first_chunks) = profile_chunk_bases(events, sessions, any_window);
    let bases = &bases;
    let first_chunks = &first_chunks;
    let sessions = &sessions;
    let session_table = &session_table;
    if threads == 1 || events.len() < MIN_CHUNK_WORK {
        return scan_profile_chunk(
            events, 0, scope, bases, first_chunks, sessions, session_table, windows,
        );
    }
    let chunk = events.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = events
            .chunks(chunk)
            .enumerate()
            .map(|(ci, c)| {
                s.spawn(move || {
                    scan_profile_chunk(
                        c,
                        ci * chunk,
                        scope,
                        bases,
                        first_chunks,
                        sessions,
                        session_table,
                        windows,
                    )
                })
            })
            .collect();
        let mut nodes: ProfileNodes = ProfileNodes::default();
        let mut times: Vec<ProfileSelfTimes> = (0..windows.len())
            .map(|_| ProfileSelfTimes::default())
            .collect();
        for h in handles {
            let (n, t) = h.join().unwrap();
            // Latest definition wins, in chunk order: V8 reuses node ids
            // within a session, so the last chunk's names/parents describe
            // the live frames. (Joining in chunk order keeps this
            // deterministic across thread schedules.)
            for (id, v) in n {
                nodes.insert(id, v);
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
/// last sample + this chunk's first delta. The walk is anchored per profiler
/// session (see `assign_sessions`) on the `Profile` (ph=P) event's ts.
/// Returns the base per event index plus the event indices of each session's
/// first chunk — the only chunks whose deltas[0] spans profiler attach
/// instead of a previous sample (needed to cap the attach warm-up out of
/// self-time, even without windows).
///
/// With `CHPERF_CHECK` set, also verifies that reconstructed sample times
/// stay inside the trace bounds — if Chrome ever changes `timeDeltas`
/// semantics, the drift shows up here instead of silently skewing windows.
fn profile_chunk_bases(
    events: &[TraceEvent],
    session_idx: &[u32],
    collect_bases: bool,
) -> (HashMap<usize, f64>, rustc_hash::FxHashSet<usize>) {
    // Profile heads register their anchor under their assigned session; the
    // per-event session assignment already resolved id-less chunks to the
    // latest Profile on their tid, so no (pid, tid) aliasing is needed here.
    let mut starts: rustc_hash::FxHashMap<u32, f64> = rustc_hash::FxHashMap::default();
    for (e, s) in events.iter().zip(session_idx.iter()) {
        if e.name == "Profile" && e.ph == b'P' {
            starts.entry(*s).or_insert(e.ts);
        }
    }
    let mut bases: HashMap<usize, f64> = HashMap::new();
    let mut first_chunks: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    // Last sample time per session; concurrent sessions (workers, multiple
    // processes) must not continue each other's walks.
    let mut prev_last: rustc_hash::FxHashMap<u32, f64> = rustc_hash::FxHashMap::default();
    // Sample-time bounds (first sample of the walk, last sample per pid) for
    // the CHPERF_CHECK sanity pass.
    let mut sample_min = f64::INFINITY;
    let mut sample_max = f64::NEG_INFINITY;
    let do_check = check_enabled();
    for (idx, e) in events.iter().enumerate() {
        if e.name != "ProfileChunk" {
            continue;
        }
        let sess = session_idx[idx];
        if !collect_bases && !do_check && prev_last.contains_key(&sess) {
            continue;
        }
        let Some(raw) = e.args_raw() else {
            continue;
        };
        if !raw.contains("\"timeDeltas\"") {
            continue;
        }
        let Some(td) = serde_json::from_str::<ChunkDeltasOnly>(raw)
            .ok()
            .and_then(|c| c.data)
            .and_then(|d| d.time_deltas)
        else {
            continue;
        };
        let Some(&first) = td.first() else {
            continue;
        };
        let pl = prev_last
            .get(&sess)
            .copied()
            .unwrap_or_else(|| starts.get(&sess).copied().unwrap_or(0.0));
        if !prev_last.contains_key(&sess) {
            // First delta-bearing chunk of this session: its deltas[0] reaches
            // back to the Profile event's ts, not to a previous sample.
            first_chunks.insert(idx);
        }
        let base = pl + first;
        let sum: f64 = td.iter().copied().sum();
        prev_last.insert(sess, pl + sum);
        if do_check {
            if base < sample_min {
                sample_min = base;
            }
            let last = pl + sum;
            if last > sample_max {
                sample_max = last;
            }
        }
        if collect_bases {
            bases.insert(idx, base);
        }
    }
    if do_check {
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
    (bases, first_chunks)
}

/// Cached check of the `CHPERF_CHECK` env flag.
fn check_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("CHPERF_CHECK").is_ok())
}

/// Single-threaded scan of one events slice, shared by the parallel helper.
/// `global_offset` maps slice-local indices back to event indices for the
/// chunk bases and session lookups.
fn scan_profile_chunk(
    events: &[TraceEvent],
    global_offset: usize,
    scope: Option<&Scope>,
    bases: &HashMap<usize, f64>,
    first_chunks: &rustc_hash::FxHashSet<usize>,
    sessions: &[u32],
    session_table: &[SessionKey],
    windows: &[Option<(f64, f64)>],
) -> (ProfileNodes, Vec<ProfileSelfTimes>) {
    let mut node_map: ProfileNodes = ProfileNodes::default();
    let mut self_times: Vec<ProfileSelfTimes> = (0..windows.len())
        .map(|_| ProfileSelfTimes::default())
        .collect();

    for (i, e) in events.iter().enumerate() {
        if e.name != "ProfileChunk" {
            continue;
        }
        let sess = session_table[sessions[global_offset + i] as usize];
        let Some(raw) = e.args_raw() else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<ChunkFull>(raw) else {
            continue;
        };
        let Some(data) = parsed.data else {
            continue;
        };
        let Some(cpu_profile) = data.cpu_profile else {
            continue;
        };

        if let Some(nodes) = cpu_profile.nodes {
            for node in nodes {
                let id = node.id;
                let parent = node.parent;
                let call_frame = node.call_frame;
                // Latest definition wins: V8 reuses node ids within a
                // session (later chunks redefine id 1..N for new frames),
                // so an `or_insert` would keep a stale name and attribute
                // later samples to the old function.
                node_map.insert(
                    (sess.0, sess.1, sess.2, id),
                    (
                        call_frame
                            .as_ref()
                            .and_then(|cf| cf.function_name.as_deref())
                            .unwrap_or("(anonymous)")
                            .to_string(),
                        call_frame
                            .as_ref()
                            .and_then(|cf| cf.url.as_deref())
                            .unwrap_or("")
                            .to_string(),
                        parent,
                    ),
                );
            }
        }

        if let Some(scope) = scope
            && !scope.allows_chunk(e)
        {
            continue;
        }
        let samples = cpu_profile.samples;
        let time_deltas = data.time_deltas;

        if let (Some(samples), Some(deltas)) = (samples, time_deltas) {
            let n = samples.len().min(deltas.len());
            if n == 0 {
                continue;
            }
            let first_cap: f64 = if first_chunks.contains(&(global_offset + i)) {
                let mut rest: Vec<f64> = deltas[..n]
                    .iter()
                    .skip(1)
                    .copied()
                    .filter(|d| *d > 0.0)
                    .collect();
                if rest.is_empty() {
                    0.0
                } else {
                    rest.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    rest[rest.len() / 2]
                }
            } else {
                f64::INFINITY
            };
            let weight_at = |k: usize| {
                let w = deltas[k].max(0.0);
                if k == 0 { w.min(first_cap) } else { w }
            };
            let windowed = windows.iter().any(|w| w.is_some());
            if !windowed {
                let t0 = &mut self_times[0];
                for (k, &node_id) in samples.iter().take(n).enumerate() {
                    let weight = weight_at(k);
                    if weight > 0.0 {
                        *t0.entry((sess.0, sess.1, sess.2, node_id)).or_default() += weight;
                    }
                }
                continue;
            }
            let base = bases.get(&(global_offset + i)).copied().unwrap_or(0.0);
            // Sample i lives at `base + sum(deltas[1..=i])` (base already
            // includes deltas[0]); advancing by deltas[k+1] keeps the walk in
            // step with Chrome's own advance-first sample times even when
            // deltas are non-uniform (idle gap before the chunk's first
            // sample used to shift every sample of the chunk by deltas[0]).
            let mut sample_ts = base;
            for (k, &node_id) in samples.iter().take(n).enumerate() {
                // Weights clamp negative deltas and cap the attach warm-up
                // (first sample of a session); the time walk uses the raw
                // deltas so timestamps stay true.
                let weight = weight_at(k);
                if weight > 0.0 {
                    for (wi, w) in windows.iter().enumerate() {
                        // Left-closed, right-open: a sample exactly on a
                        // window edge belongs to the window starting there,
                        // so adjacent PRE/SHOOT/POST windows never share a
                        // sample.
                        if w.is_none_or(|(lo, hi)| sample_ts >= lo && sample_ts < hi) {
                            *self_times[wi]
                                .entry((sess.0, sess.1, sess.2, node_id))
                                .or_default() += weight;
                        }
                    }
                }
                if let Some(&d) = deltas.get(k + 1) {
                    sample_ts += d;
                }
            }
        }
    }
    (node_map, self_times)
}

/// Test-only convenience wrapper; production code uses `analyze_cpu_profile_full`
/// so the raw node table + self-times can be kept in the cache for later queries.
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
    (
        result,
        CpuProfileCache {
            nodes: Arc::new(node_map),
            self_times,
        },
    )
}

fn build_cpu_profile_result(
    node_map: &ProfileNodes,
    self_times: &ProfileSelfTimes,
) -> CpuProfileResult {
    // Aggregate per node first, then merge into one row per (name, url): a
    // function sampled at several call sites (recursion) has one node per
    // frame, and per-node rows would split — or, after sorting, mislead
    // consumers that pick a single entry — its total self-time.
    let mut by_func: rustc_hash::FxHashMap<(String, String), (f64, SourceType)> =
        rustc_hash::FxHashMap::default();
    for (key, time) in self_times {
        let (name, url) = node_map
            .get(key)
            .map(|(n, u, _)| (n.clone(), u.clone()))
            .unwrap_or_default();
        let source_type = classify_url(&url);
        let entry = by_func.entry((name, url)).or_insert((0.0, source_type));
        entry.0 += time;
    }

    let mut functions: Vec<FunctionTime> = by_func
        .into_iter()
        .map(|((name, url), (time, source_type))| FunctionTime {
            function_name: name,
            url,
            self_time_us: time,
            source_type,
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
            name: "Profile".into(),
            id: 0,
            has_id: false,
            ph: b'P',
            ts,
            dur: None,
            tid: 65,
            pid: 2,
            cat: None,
            args: None,
        }
    }

    fn chunk_event(
        ts: f64,
        nodes: &[(u64, &str, Option<u64>)],
        samples: &[u64],
        deltas: &[f64],
    ) -> TraceEvent {
        let nodes_json: Vec<serde_json::Value> = nodes
            .iter()
            .map(|(id, name, parent)| {
                serde_json::json!({"id": id, "callFrame": {"functionName": name, "url": ""}, "parent": parent})
            })
            .collect();
        TraceEvent {
            name: "ProfileChunk".into(),
            id: 0,
            has_id: false,
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
        }
    }

    fn plain_event(ts: f64, name: &str) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph: b'X',
            ts,
            dur: Some(100.0),
            tid: 1,
            pid: 2,
            cat: None,
            args: None,
        }
    }

    const NODES_ABC: [(u64, &str, Option<u64>); 3] = [
        (1, "(root)", None),
        (2, "shoot", Some(1)),
        (3, "update", Some(1)),
    ];

    /// Fixture mirroring tests/fixtures/tiny.json: profile starts at 1e6,
    /// chunk 1 = 7 samples of 5ms (shoot/update alternating), chunk 2 has a
    /// negative delta that must be clamped. Expected totals:
    /// total 46ms, shoot 26ms (15 + 11), update 15ms.
    fn fixture_events() -> Vec<TraceEvent> {
        vec![
            profile_event(1_000_000.0),
            chunk_event(
                1_055_000.0,
                &NODES_ABC,
                &[2, 3, 2, 3, 2, 3, 2],
                &[5000.0; 7],
            ),
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

    /// Node key in the fixture's single session: pid 2, subkey tid 65,
    /// first (and only) generation.
    fn nk(n: u64) -> crate::analysis::NodeKey {
        (2, 65, 1, n)
    }

    #[test]
    fn scan_attribution_weights_are_per_sample_deltas() {
        // Regression: the old code summed cumulative deltas (~83x inflation);
        // weights must be the deltas themselves, clamped at zero.
        let events = fixture_events();
        let (nodes, times) = scan_profile_chunks(&events, None, 0);
        assert_eq!(nodes.len(), 4);
        assert_eq!(total(&times), 46_000.0);
        assert_eq!(*times.get(&nk(2)).unwrap_or(&0.0), 20_000.0); // 4 × 5ms (idx 0,2,4,6)
        assert_eq!(*times.get(&nk(3)).unwrap_or(&0.0), 15_000.0);
        // Node 4: 4000 + 3000 + clamp(-1000 → 0) + 4000.
        assert_eq!(*times.get(&nk(4)).unwrap_or(&0.0), 11_000.0);
    }

    /// Node-id reuse takes the latest definition; a fresh Profile on the
    /// same pid+tid starts a new session; window bounds are [lo, hi).
    #[test]
    fn node_id_reuse_takes_latest_definition() {
        // V8 reuses node ids within a session: a later chunk redefines id 2
        // under a new name. Latest wins — the old code's `or_insert` kept
        // the stale name and attributed the new samples to the old function.
        let events = vec![
            profile_event(0.0),
            chunk_event(
                0.0,
                &[(1, "(root)", None), (2, "oldFunc", Some(1))],
                &[2],
                &[1_000.0],
            ),
            chunk_event(
                10_000.0,
                &[(1, "(root)", None), (2, "newFunc", Some(1))],
                &[2],
                &[1_000.0],
            ),
        ];
        let (nodes, times) = scan_profile_chunks(&events, None, 0);
        assert_eq!(nodes.get(&nk(2)).map(|n| n.0.as_str()), Some("newFunc"));
        // Chunk 1's lone sample is attach warm-up (capped to 0 with no
        // sibling deltas); only chunk 2's sample weighs in.
        assert_eq!(*times.get(&nk(2)).unwrap_or(&0.0), 1_000.0);
        let result = build_cpu_profile_result(&nodes, &times);
        assert!(
            result
                .functions
                .iter()
                .all(|f| f.function_name != "oldFunc")
        );
        assert_eq!(
            result
                .functions
                .iter()
                .find(|f| f.function_name == "newFunc")
                .map(|f| f.self_time_us),
            Some(1_000.0)
        );
    }

    #[test]
    fn fresh_profile_starts_new_session_on_same_pid_tid() {
        // Two isolates sharing pid+tid, chunks without a head `id`: the
        // second `Profile` (ph P) starts a new session instead of extending
        // the old one — same node id, separate names and self-times.
        let events = vec![
            profile_event(0.0),
            chunk_event(
                0.0,
                &[(1, "(root)", None), (2, "isoA", Some(1))],
                &[2, 2],
                &[500.0, 500.0],
            ),
            profile_event(1_000_000.0),
            chunk_event(
                1_000_000.0,
                &[(1, "(root)", None), (2, "isoB", Some(1))],
                &[2, 2],
                &[700.0, 700.0],
            ),
        ];
        let sessions = assign_sessions(&events);
        // Profile, chunk, Profile, chunk: generations 1, 1, 2, 2.
        let keys: Vec<SessionKey> = sessions
            .0
            .iter()
            .map(|&i| sessions.1[i as usize])
            .collect();
        assert_eq!(
            keys,
            vec![(2, 65, 1), (2, 65, 1), (2, 65, 2), (2, 65, 2)]
        );
        let (nodes, times) = scan_profile_chunks(&events, None, 0);
        assert_eq!(
            nodes.get(&(2, 65, 1, 2)).map(|n| n.0.as_str()),
            Some("isoA")
        );
        assert_eq!(
            nodes.get(&(2, 65, 2, 2)).map(|n| n.0.as_str()),
            Some("isoB")
        );
        // Each session's first chunk has its attach warm-up capped at the
        // median of its sibling deltas: 500 + 500 and 700 + 700.
        assert_eq!(*times.get(&(2, 65, 1, 2)).unwrap_or(&0.0), 1_000.0);
        assert_eq!(*times.get(&(2, 65, 2, 2)).unwrap_or(&0.0), 1_400.0);
        // The second session's samples must not continue the first walk:
        // its windowed base restarts at its own Profile ts.
        let windows = [Some((1_000_000.0, 2_000_000.0))];
        let (_, wtimes) = scan_profile_chunks_windows(&events, None, &windows, 0);
        assert_eq!(*wtimes[0].get(&(2, 65, 2, 2)).unwrap_or(&0.0), 1_400.0);
        assert!(!wtimes[0].contains_key(&(2, 65, 1, 2)));
    }

    #[test]
    fn windowed_sample_window_is_left_closed_right_open() {
        // Samples at 1_005_000 and 1_010_000. A window ending exactly on a
        // sample excludes it; a window starting on it includes it — adjacent
        // windows never share a sample.
        let events = vec![
            profile_event(1_000_000.0),
            chunk_event(
                1_000_000.0,
                &[(1, "(root)", None), (2, "f", Some(1))],
                &[2, 2],
                &[5_000.0, 5_000.0],
            ),
        ];
        let windows = [
            Some((1_005_000.0, 1_010_000.0)),
            Some((1_010_000.0, 1_015_000.0)),
        ];
        let (_, times) = scan_profile_chunks_windows(&events, None, &windows, 0);
        assert_eq!(times.len(), 2);
        assert_eq!(*times[0].get(&nk(2)).unwrap_or(&0.0), 5_000.0);
        assert_eq!(*times[1].get(&nk(2)).unwrap_or(&0.0), 5_000.0);
    }

    #[test]
    fn profiler_attach_warmup_is_capped_on_first_chunk() {
        // First chunk of the pid: deltas[0] spans profiler attach (360ms of
        // warm-up landing on whatever was mid-call at attach), the rest are
        // normal 1ms sampling intervals. Second chunk's deltas[0] is a real
        // inter-sample gap and must stay uncapped.
        let nodes = &[(1, "(root)", None), (2, "spatialWebAudioUpdater", Some(1))];
        let events = vec![
            profile_event(0.0),
            chunk_event(
                0.0,
                nodes,
                &[2, 2, 2, 2],
                &[360_000.0, 1_000.0, 1_000.0, 1_000.0],
            ),
            chunk_event(1_000_000.0, nodes, &[2], &[50_000.0]),
        ];
        // Fast path: capped at the median of chunk 1's other deltas (1ms) →
        // 1000 + 3 × 1000 + uncapped 50_000.
        let (_, times) = scan_profile_chunks(&events, None, 0);
        assert_eq!(*times.get(&nk(2)).unwrap_or(&0.0), 54_000.0);
        // Windowed path caps the same way.
        let windows = [Some((0.0, 2_000_000.0))];
        let (_, times) = scan_profile_chunks_windows(&events, None, &windows, 0);
        assert_eq!(*times[0].get(&nk(2)).unwrap_or(&0.0), 54_000.0);
    }

    #[test]
    fn attach_cap_with_no_reference_interval_counts_free() {
        // A single-sample first chunk has no other deltas to derive a
        // sampling interval from → the attach sample weighs nothing.
        let nodes = &[(1, "(root)", None), (2, "i", Some(1))];
        let events = vec![
            profile_event(0.0),
            chunk_event(0.0, nodes, &[2], &[360_000.0]),
            chunk_event(1_000_000.0, nodes, &[2], &[1_000.0]),
        ];
        let (_, times) = scan_profile_chunks(&events, None, 0);
        assert_eq!(*times.get(&nk(2)).unwrap_or(&0.0), 1_000.0);
    }

    fn profile_event_sess(ts: f64, pid: u64, tid: u64) -> TraceEvent {
        let mut e = profile_event(ts);
        e.pid = pid;
        e.tid = tid;
        e
    }

    fn chunk_event_sess(
        ts: f64,
        pid: u64,
        tid: u64,
        nodes: &[(u64, &str, Option<u64>)],
        samples: &[u64],
        deltas: &[f64],
    ) -> TraceEvent {
        let mut e = chunk_event(ts, nodes, samples, deltas);
        e.pid = pid;
        e.tid = tid;
        e
    }

    #[test]
    fn concurrent_sessions_do_not_share_walks_or_node_ids() {
        // Two isolates that both restart node ids at 1 (web worker, second
        // renderer): pid-keyed walks continued each other's sample times and
        // or_insert merged same-id nodes under one name. Sessions must stay
        // independent — here keyed by (pid, tid) since the chunks carry no id.
        let events = vec![
            profile_event_sess(0.0, 2, 65),
            chunk_event_sess(
                0.0,
                2,
                65,
                &[(1, "(root)", None), (2, "funcA", Some(1))],
                &[2, 2, 2],
                &[360_000.0, 1_000.0, 1_000.0],
            ),
            profile_event_sess(1_000_000.0, 3, 65),
            chunk_event_sess(
                1_000_000.0,
                3,
                65,
                &[(1, "(root)", None), (2, "funcB", Some(1))],
                &[2, 2],
                &[500.0, 500.0],
            ),
        ];
        let (nodes, times) = scan_profile_chunks(&events, None, 0);
        // Same node id 2, different sessions: both names survive; each keeps
        // its own self-time (A capped at 1ms interval; B at its own 500µs).
        assert_eq!(nodes.get(&nk(2)).map(|n| n.0.as_str()), Some("funcA"));
        assert_eq!(
            nodes.get(&(3, 65, 1, 2)).map(|n| n.0.as_str()),
            Some("funcB")
        );
        assert_eq!(*times.get(&nk(2)).unwrap_or(&0.0), 3_000.0);
        assert_eq!(*times.get(&(3, 65, 1, 2)).unwrap_or(&0.0), 1_000.0);
        // Aggregated per (name, url): funcB is not folded into funcA.
        let result = build_cpu_profile_result(&nodes, &times);
        let f = |name: &str| {
            result
                .functions
                .iter()
                .find(|f| f.function_name == name)
                .map(|f| f.self_time_us)
                .unwrap_or_default()
        };
        assert_eq!(f("funcA"), 3_000.0);
        assert_eq!(f("funcB"), 1_000.0);
    }

    #[test]
    fn windowed_walk_does_not_double_count_deltas0() {
        // Chunk deltas [4000, 300, 300]: samples sit at base, +300, +600.
        // The old walk advanced by deltas[k] AFTER recording, drifting every
        // sample after the first by deltas[0] and emptying mid-chunk windows.
        let nodes = &[(1, "(root)", None), (2, "funcA", Some(1))];
        let events = vec![
            profile_event(1_000_000.0),
            chunk_event(1_000_000.0, nodes, &[2, 2, 2], &[4_000.0, 300.0, 300.0]),
        ];
        // Window covers only sample 1 (at 1_004_300).
        let windows = [Some((1_004_200.0, 1_004_500.0))];
        let (_, times) = scan_profile_chunks_windows(&events, None, &windows, 0);
        assert_eq!(*times[0].get(&nk(2)).unwrap_or(&0.0), 300.0);
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
        // 5 × 5ms: the sample exactly at 1_030_000 belongs to the next
        // [lo, hi) window, not this one.
        assert_eq!(total(&times), 25_000.0);

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
        assert_eq!(total(&times[0]), 25_000.0);
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
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state >> 33
        };
        let mut events: Vec<TraceEvent> = Vec::new();
        for i in 0..22_000u64 {
            let samples: Vec<u64> = (0..8).map(|_| (rng() % 4) + 1).collect();
            let deltas: Vec<f64> = (0..8).map(|_| (rng() % 11) as f64 - 1.0).collect();
            events.push(chunk_event(
                1_000_000.0 + i as f64 * 10_000.0,
                &[
                    (1, "(root)", None),
                    (2, "a", Some(1)),
                    (3, "b", Some(1)),
                    (4, "c", Some(1)),
                ],
                &samples,
                &deltas,
            ));
            events.push(plain_event(1_000_000.0 + i as f64 * 10_000.0, "RunTask"));
            events.push(plain_event(
                1_000_000.0 + i as f64 * 10_000.0 + 1.0,
                "Paint",
            ));
        }
        assert!(events.len() >= 64_000);

        let sessions = assign_sessions(&events);
        let (nodes_serial, times_serial) =
            scan_profile_chunks_core(&events, None, &[None], &sessions.0, &sessions.1, 1);
        let (nodes_par, times_par) =
            scan_profile_chunks_core(&events, None, &[None], &sessions.0, &sessions.1, 8);
        assert_eq!(nodes_serial.len(), nodes_par.len());
        for (id, v) in &nodes_serial {
            assert_eq!(nodes_par.get(id), Some(v));
        }
        assert_eq!(times_serial[0].len(), times_par[0].len());
        for (id, t) in &times_serial[0] {
            assert_eq!(
                times_par[0].get(id),
                Some(t),
                "node {:?} self-time diverged",
                id
            );
        }
    }

    #[test]
    fn cpu_profile_cache_matches_fresh_scan_on_full_scope() {
        // The REPL cache is only consulted for an empty scope; it must return
        // exactly what a fresh full-trace scan would.
        let events = fixture_events();
        let empty = Scope {
            window: None,
            tid: None,
            pid: None,
            cat: None,
        };
        let (result, cache) = analyze_cpu_profile_full(&events);
        assert!(cache.serves(&empty));
        assert!(!cache.serves(&Scope {
            window: Some((0.0, 1.0)),
            tid: None,
            pid: None,
            cat: None
        }));

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
