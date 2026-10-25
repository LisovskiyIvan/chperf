//! Windowed analyses: semantic anchors (--anchor), per-frame statistics
//! (--frames), GC/long-task reports (--gc), the inclusive CPU call tree
//! (--calltree), the PRE/SHOOT/POST delta comparison (--delta) and CSV
//! rendering. Each inspector returns `(markdown, json)`, so `--json` and
//! `--csv` stay in sync with the Markdown output.

use crate::inspect::{Matcher, Scope};
use crate::trace::{
    TraceEvent, effective_runtask_dur, find_profiler_overhead, is_profiler_dropped_frame,
};
use serde_json::{Value, json};
use std::collections::HashMap;

// ── Anchor detection (--anchor) ──

/// A semantic time anchor: the first trace occurrence of a search pattern.
pub struct Anchor {
    /// Absolute timestamp (µs).
    pub ts: f64,
    /// What matched: `FunctionCall` / `cpu-profile` / `args`.
    pub kind: &'static str,
    /// The matched text (function name, URL, or JSON value).
    pub label: String,
}

fn value_match_text(v: &Value, matcher: &Matcher) -> Option<String> {
    match v {
        Value::String(s) => matcher.matches(s).then(|| s.clone()),
        Value::Array(a) => a.iter().find_map(|v| value_match_text(v, matcher)),
        Value::Object(m) => m.iter().find_map(|(k, v)| {
            if matcher.matches(k) {
                Some(k.clone())
            } else {
                value_match_text(v, matcher)
            }
        }),
        Value::Number(n) => {
            let s = n.to_string();
            matcher.matches(&s).then_some(s)
        }
        _ => None,
    }
}

/// Find the first trace occurrence of `matcher`, by priority:
/// 1. FunctionCall `data.functionName` (actual JS executions)
/// 2. CPU profile node names / source URLs (sampled call frames)
/// 3. any event args (JSON values)
///
/// Priority matters: e.g. searching `shoot` must anchor on the first *shoot
/// call*, not on the `shooterX` script URL that shows up when modules load.
pub fn find_anchor(events: &[TraceEvent], matcher: &Matcher) -> Option<Anchor> {
    // Pass 1: FunctionCall data.functionName.
    let mut best: Option<Anchor> = None;
    for e in events {
        if e.name != "FunctionCall" || e.ph != b'X' {
            continue;
        }
        // Fast pre-filter on raw bytes: substring needles can't match an
        // event whose raw args don't contain them, so skip the args parse
        // (and the OnceLock fill) entirely.
        if let Matcher::Substr(p) = matcher {
            let Some(raw) = e.args_raw() else { continue };
            if !crate::inspect::contains_ignore_case(raw, p) {
                continue;
            }
        }
        let args = e.args_value();
        let Some(fn_name) = args
            .as_ref()
            .and_then(|a| a.get("data"))
            .and_then(|d| d.get("functionName"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        if matcher.matches(fn_name) && best.as_ref().is_none_or(|b| e.ts < b.ts) {
            best = Some(Anchor {
                ts: e.ts,
                kind: "FunctionCall",
                label: fn_name.to_string(),
            });
        }
    }
    if best.is_some() {
        return best;
    }

    // Pass 2: CPU profile node names / URLs, by earliest sample time.
    // Sample times walk each session's chunk sequence anchored on the
    // session's `Profile` (ph=P) event; timeDeltas are inter-sample gaps.
    // Sessions (workers, multiple processes, reused pid+tid) have
    // independent node id spaces and walks, keyed by `assign_sessions` exactly
    // like the CPU scan, so anchor times agree with `scan_profile_chunks`.
    let (session_idx, session_table) = crate::analysis::assign_sessions(events);
    // Profile-head anchor per session (same resolution as
    // `profile_chunk_bases`): id-less chunks attach to the latest Profile
    // on their tid via the session assignment.
    let mut starts: rustc_hash::FxHashMap<u32, f64> =
        rustc_hash::FxHashMap::default();
    for (e, s) in events.iter().zip(session_idx.iter()) {
        if e.name == "Profile" && e.ph == b'P' {
            starts.entry(*s).or_insert(e.ts);
        }
    }
    let mut node_first: rustc_hash::FxHashMap<
        (crate::analysis::SessionKey, u64),
        (f64, String, String),
    > = rustc_hash::FxHashMap::default();
    let mut prev_last: rustc_hash::FxHashMap<u32, f64> =
        rustc_hash::FxHashMap::default();
    for (idx, e) in events.iter().enumerate() {
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
        let sess_idx = session_idx[idx];
        // Node ids live in SessionKey space (kept for identical iteration
        // order); the u32 index only keys the query-only time maps.
        let sess = session_table[sess_idx as usize];
        for node in cpu_profile
            .get("nodes")
            .and_then(|n| n.as_array())
            .into_iter()
            .flatten()
        {
            let id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let key = (sess, id);
            if id == 0 || node_first.contains_key(&key) {
                continue;
            }
            let call_frame = node.get("callFrame");
            let name = call_frame
                .and_then(|cf| cf.get("functionName"))
                .and_then(|v| v.as_str())
                .unwrap_or("(anonymous)")
                .to_string();
            let url = call_frame
                .and_then(|cf| cf.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            node_first.insert(key, (f64::INFINITY, name, url));
        }
        let samples = cpu_profile.get("samples").and_then(|s| s.as_array());
        let deltas = data.get("timeDeltas").and_then(|t| t.as_array());
        if let (Some(samples), Some(deltas)) = (samples, deltas) {
            let n = samples.len().min(deltas.len());
            // Sample i lives at `prev_last + sum(deltas[0..=i])`: advance
            // before recording so the first sample lands on `prev_last +
            // delta[0]` (same walk as `profile_chunk_bases`).
            let mut sample_ts = prev_last
                .get(&sess_idx)
                .copied()
                .unwrap_or_else(|| starts.get(&sess_idx).copied().unwrap_or(0.0));
            for (i, s) in samples.iter().enumerate().take(n) {
                sample_ts += deltas[i].as_f64().unwrap_or(0.0);
                let id = s.as_u64().unwrap_or(0);
                if id != 0
                    && let Some(entry) = node_first.get_mut(&(sess, id))
                    && sample_ts < entry.0
                {
                    entry.0 = sample_ts;
                }
            }
            prev_last.insert(sess_idx, sample_ts);
        }
    }
    // Prefer function-name matches over URL-only matches: a substring like
    // `shoot` also hits module URLs (`src/games/shooterX/...`) that appear
    // long before the first actual shoot call.
    let mut cpu_best: Option<Anchor> = None;
    for (t, name, _url) in node_first.values() {
        if *t == f64::INFINITY || !matcher.matches(name) {
            continue;
        }
        // Earliest sample wins; exact-ts ties pick the alphabetically
        // first name (map order is hash-dependent).
        if cpu_best
            .as_ref()
            .is_none_or(|b| *t < b.ts || (*t == b.ts && *name < b.label))
        {
            cpu_best = Some(Anchor {
                ts: *t,
                kind: "cpu-profile",
                label: name.clone(),
            });
        }
    }
    if cpu_best.is_none() {
        for (t, _name, url) in node_first.values() {
            if *t == f64::INFINITY || !matcher.matches(url) {
                continue;
            }
            // Earliest sample wins; exact-ts ties pick the alphabetically
            // first url (map order is hash-dependent).
            if cpu_best
                .as_ref()
                .is_none_or(|b| *t < b.ts || (*t == b.ts && *url < b.label))
            {
                cpu_best = Some(Anchor {
                    ts: *t,
                    kind: "cpu-profile",
                    label: url.clone(),
                });
            }
        }
    }
    if let Some(a) = cpu_best {
        return Some(a);
    }

    // Pass 3: any event args.
    let mut args_best: Option<Anchor> = None;
    for e in events {
        let Some(raw) = e.args_raw() else { continue };
        if let Matcher::Substr(p) = matcher
            && !crate::inspect::contains_ignore_case(raw, p)
        {
            continue;
        }
        let Some(args) = e.args_value() else { continue };
        if let Some(text) = value_match_text(&args, matcher)
            && args_best.as_ref().is_none_or(|b| e.ts < b.ts)
        {
            args_best = Some(Anchor {
                ts: e.ts,
                kind: "args",
                label: text,
            });
        }
    }
    args_best
}

// ── Frame statistics (--frames, --delta) ──

/// Frame budget (µs) per detected refresh rate: 120Hz / 90Hz / 60Hz.
pub const FRAME_BUDGET_120HZ_US: f64 = 8_333.0;
pub const FRAME_BUDGET_90HZ_US: f64 = 11_111.0;
pub const FRAME_BUDGET_60HZ_US: f64 = 16_667.0;

/// Frame-tick event names used for cadence detection, in preference order.
const FRAME_TICK_EVENTS: [&str; 3] = [
    "SubmitCompositorFrameToPresentationCompositorFrame",
    "DrawFrame",
    "BeginFrame",
];

/// Detect the frame budget (µs) from the trace's vsync cadence: the median
/// interval between consecutive frame-tick `b` events (compositor
/// presentation, else DrawFrame/BeginFrame) maps to the nearest standard
/// budget (≤10ms → 120Hz, ≤13ms → 90Hz, else 60Hz). Falls back to 60Hz when
/// fewer than 3 ticks are present. Pure function, deterministic.
pub fn detect_frame_budget_us(events: &[TraceEvent]) -> f64 {
    for name in FRAME_TICK_EVENTS {
        let mut ts: Vec<f64> = events
            .iter()
            .filter(|e| e.name == name && e.ph == b'b')
            .map(|e| e.ts)
            .collect();
        if ts.len() < 3 {
            continue;
        }
        ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut gaps: Vec<f64> = ts
            .windows(2)
            .map(|w| w[1] - w[0])
            .filter(|&d| d > 0.0)
            .collect();
        if gaps.len() < 2 {
            continue;
        }
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mid = gaps.len() / 2;
        let median = if gaps.len() % 2 == 1 {
            gaps[mid]
        } else {
            (gaps[mid - 1] + gaps[mid]) / 2.0
        };
        if median <= 10_000.0 {
            return FRAME_BUDGET_120HZ_US;
        }
        if median <= 13_000.0 {
            return FRAME_BUDGET_90HZ_US;
        }
        return FRAME_BUDGET_60HZ_US;
    }
    FRAME_BUDGET_60HZ_US
}

/// Count DroppedFrame events in scope, excluding ones inside the profiler
/// startup freeze. Single source of truth shared by `frames_section` and
/// jank analysis, so the same trace never reports two dropped counts.
pub fn dropped_frames(events: &[TraceEvent], scope: Option<&Scope>) -> usize {
    let overheads = find_profiler_overhead(events);
    events
        .iter()
        .filter(|e| {
            e.name == "DroppedFrame"
                && scope.is_none_or(|s| s.allows_event(e))
                && !is_profiler_dropped_frame(e.ts, &overheads)
        })
        .count()
}

/// Pair `b`/`e` events with the given name into durations (µs). Chrome frame
/// pipeline events are traced as `b`/`e` pairs without `dur`; same-thread
/// pairing via a stack.
///
/// Window rule: pairing runs globally per tid stack, then pairs are filtered
/// by their **begin** (`b`) timestamp — an `e` outside the window still
/// closes a pair whose `b` is inside (and vice versa: a `b` outside is
/// ignored even when its `e` lands inside). Concretely the window keeps
/// pairs with `lo <= b.ts < hi`, so frames are attributed to their submit
/// time. Windows are left-closed, right-open, matching the CPU sample
/// attribution and the PRE/SHOOT/POST event sweep: adjacent windows never
/// share a pair.
fn paired_durations(events: &[TraceEvent], name: &str, window: Option<(f64, f64)>) -> Vec<f64> {
    let mut stacks: HashMap<u64, Vec<f64>> = HashMap::new();
    let mut pairs: Vec<(f64, f64)> = Vec::new(); // (b_ts, dur)
    for e in events {
        if e.name != name {
            continue;
        }
        match e.ph {
            b'b' => stacks.entry(e.tid).or_default().push(e.ts),
            b'e' => {
                if let Some(st) = stacks.get_mut(&e.tid)
                    && let Some(t) = st.pop()
                    && e.ts > t
                {
                    pairs.push((t, e.ts - t));
                }
            }
            _ => {}
        }
    }
    match window {
        None => pairs.into_iter().map(|(_, d)| d).collect(),
        Some((lo, hi)) => pairs
            .into_iter()
            .filter(|(b, _)| *b >= lo && *b < hi)
            .map(|(_, d)| d)
            .collect(),
    }
}

/// Linear-interpolated percentiles over ascending-sorted samples:
/// rank = q/100*(n-1), value interpolated between the bracketing samples.
/// (p50, p90, p99, max). n < 2 degrades gracefully (n=1 returns the sample).
fn percentiles(sorted_asc: &[f64]) -> (f64, f64, f64, f64) {
    // (p50, p90, p99, max)
    if sorted_asc.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let n = sorted_asc.len();
    let p = |q: f64| -> f64 {
        let rank = q / 100.0 * (n - 1) as f64;
        let lo = rank.floor() as usize;
        let hi = rank.ceil() as usize;
        let frac = rank - lo as f64;
        sorted_asc[lo] + (sorted_asc[hi] - sorted_asc[lo]) * frac
    };
    (p(50.0), p(90.0), p(99.0), *sorted_asc.last().unwrap())
}

/// Per-frame duration stats + dropped frames within a window.
///
/// Frames are attributed to their submit time: `paired_durations` pairs by
/// the `b` (begin) event, so a frame whose `e` falls outside the window
/// still counts when its `b` is inside (and vice versa).
pub fn frames_section(
    events: &[TraceEvent],
    scope: &Scope,
    frame_event: &str,
    min_ts: f64,
) -> (String, Value) {
    frames_section_impl(events, scope, frame_event, min_ts, None)
}

/// `frames_section` with an explicit frame budget (µs): the budget is
/// reported in the output (`frame_budget_us`) but does not change pairing.
/// Prefer `detect_frame_budget_us` or a `--frame-budget` override.
pub fn frames_section_with_budget(
    events: &[TraceEvent],
    scope: &Scope,
    frame_event: &str,
    min_ts: f64,
    budget_us: f64,
) -> (String, Value) {
    frames_section_impl(events, scope, frame_event, min_ts, Some(budget_us))
}

fn frames_section_impl(
    events: &[TraceEvent],
    scope: &Scope,
    frame_event: &str,
    min_ts: f64,
    budget_us: Option<f64>,
) -> (String, Value) {
    let mut durs = paired_durations(events, frame_event, scope.window);
    durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let dropped = dropped_frames(events, Some(scope));
    let n = durs.len();
    let (p50, p90, p99, max) = percentiles(&durs);
    let avg = if n > 0 {
        durs.iter().sum::<f64>() / n as f64
    } else {
        0.0
    };
    let ms = |v: f64| format!("{:.2}", v / 1000.0);

    let mut out = String::new();
    out.push_str(&format!(
        "## Frames: {} ({} paired, {})\n\n",
        frame_event,
        n,
        crate::inspect::window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
    }
    out.push_str(&format!("- **Dropped frames**: {}\n", dropped));
    if let Some(b) = budget_us {
        out.push_str(&format!("- **Frame budget**: {:.2}ms\n", b / 1000.0));
    }
    out.push('\n');
    out.push_str("| count | avg(ms) | p50 | p90 | p99 | max |\n");
    out.push_str("|-------|---------|-----|-----|-----|-----|\n");
    out.push_str(&format!(
        "| {} | {} | {} | {} | {} | {} |\n",
        n,
        ms(avg),
        ms(p50),
        ms(p90),
        ms(p99),
        ms(max),
    ));
    out.push('\n');

    let mut obj = serde_json::Map::new();
    obj.insert("frame_event".into(), json!(frame_event));
    obj.insert("count".into(), json!(n));
    obj.insert("avg_us".into(), json!(avg.round()));
    obj.insert("p50_us".into(), json!(p50.round()));
    obj.insert("p90_us".into(), json!(p90.round()));
    obj.insert("p99_us".into(), json!(p99.round()));
    obj.insert("max_us".into(), json!(max.round()));
    obj.insert("dropped_frames".into(), json!(dropped));
    if let Some(b) = budget_us {
        obj.insert("frame_budget_us".into(), json!(b.round()));
    }
    (out, Value::Array(vec![Value::Object(obj)]))
}

// ── GC & Long Tasks (--gc) ──

/// GC + long-task report scoped to the window. Long tasks are main-thread
/// RunTasks > `lt_ms` (default 50; the Long Task API counts tasks strictly
/// over 50ms). GC split into major / minor / other
/// (all other V8.GC*/CppGC* X-events, mostly background work).
pub fn gc_section(
    events: &[TraceEvent],
    scope: &Scope,
    lt_ms: f64,
    min_ts: f64,
) -> (String, Value) {
    let main_tid = crate::trace::detect_main_thread(events);
    let overheads = find_profiler_overhead(events);
    let lt_us = lt_ms * 1000.0;

    let mut groups: [(&str, usize, f64, f64); 3] = [
        ("MajorGC", 0, 0.0, 0.0),
        ("MinorGC / GCScavenger", 0, 0.0, 0.0),
        ("other V8.GC*/CppGC*", 0, 0.0, 0.0),
    ];
    let mut long_tasks: Vec<f64> = Vec::new();

    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        if e.ph != b'X' {
            continue;
        }
        let d = e.dur.unwrap_or(0.0);
        let gi = match e.name.as_str() {
            "MajorGC" => Some(0),
            "MinorGC" | "V8.GCScavenger" => Some(1),
            n if n.starts_with("V8.GC") || n.starts_with("CppGC") => Some(2),
            _ => None,
        };
        if let Some(gi) = gi {
            let g = &mut groups[gi];
            g.1 += 1;
            g.2 += d;
            if d > g.3 {
                g.3 = d;
            }
        } else if e.name == "RunTask" && e.tid == main_tid {
            let eff = effective_runtask_dur(e, &overheads);
            if eff > lt_us {
                long_tasks.push(eff);
            }
        }
    }

    long_tasks.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let lt_total: f64 = long_tasks.iter().sum();
    let lt_max = long_tasks.first().copied().unwrap_or(0.0);

    let mut out = String::new();
    out.push_str(&format!(
        "## GC & Long Tasks ({})\n\n",
        crate::inspect::window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
    }
    out.push_str("| group | count | total(ms) | max(ms) |\n");
    out.push_str("|-------|-------|-----------|---------|\n");
    for (name, count, total, max) in &groups {
        out.push_str(&format!(
            "| {} | {} | {:.2} | {:.2} |\n",
            name,
            count,
            total / 1000.0,
            max / 1000.0,
        ));
    }
    out.push('\n');
    out.push_str(&format!(
        "- **Long tasks >{}ms**: {} total, {:.1}ms combined, max {:.1}ms\n",
        lt_ms as i64,
        long_tasks.len(),
        lt_total.max(0.0) / 1000.0,
        lt_max / 1000.0,
    ));
    let top3: Vec<String> = long_tasks
        .iter()
        .take(3)
        .map(|d| format!("{:.1}ms", d / 1000.0))
        .collect();
    if !top3.is_empty() {
        out.push_str(&format!("- top: {}\n", top3.join(", ")));
    }
    out.push('\n');

    let json_groups: Vec<Value> = groups
        .iter()
        .map(|(name, count, total, max)| {
            json!({"group": name, "count": count, "total_us": total.round(), "max_us": max.round()})
        })
        .collect();
    let row = json!({
        "gc": json_groups,
        "long_tasks": {
            "threshold_us": lt_us.round(),
            "count": long_tasks.len(),
            "total_us": lt_total.round(),
            "max_us": lt_max.round(),
            "top_us": long_tasks.iter().take(3).map(|d| d.round()).collect::<Vec<f64>>(),
        },
    });
    (out, Value::Array(vec![row]))
}

// ── Inclusive CPU call tree (--calltree) ──

fn short_url(url: &str) -> &str {
    url.rfind('/').map(|i| &url[i + 1..]).unwrap_or(url)
}

/// Top-down CPU call tree with inclusive (self + subtree) time. `--function`
/// and `--url` prune to subtrees rooted at matching nodes (ancestors of a
/// match are kept so the path stays visible).
pub fn calltree_section(
    events: &[TraceEvent],
    scope: &Scope,
    name_matcher: Option<&Matcher>,
    url_matcher: Option<&Matcher>,
    top: usize,
    min_ts: f64,
    cache: Option<&crate::analysis::CpuProfileCache>,
) -> (String, Value) {
    let cpu = crate::inspect::collect_cpu_profile(events, scope, cache);
    let nodes = cpu.nodes;
    let self_time = cpu.leaf_time;

    // Children map + roots. Keys are (pid, session, node id); a parent id
    // resolves within its own session only.
    let mut children: rustc_hash::FxHashMap<
        crate::analysis::NodeKey,
        Vec<crate::analysis::NodeKey>,
    > = rustc_hash::FxHashMap::default();
    let mut roots: Vec<crate::analysis::NodeKey> = Vec::new();
    for (key, (_, _, parent)) in nodes.iter() {
        match parent {
            Some(p) => {
                let pk = (key.0, key.1, key.2, *p);
                if nodes.contains_key(&pk) {
                    children.entry(pk).or_default().push(*key);
                } else {
                    roots.push(*key);
                }
            }
            None => roots.push(*key),
        }
    }

    // Pre-order DFS (deterministic: children sorted by self-time desc).
    let mut order: Vec<crate::analysis::NodeKey> = Vec::new();
    let mut stack: Vec<crate::analysis::NodeKey> = roots.clone();
    while let Some(id) = stack.pop() {
        order.push(id);
        if let Some(ch) = children.get(&id) {
            let mut ch = ch.clone();
            // Self-time desc; exact ties break by node id (map order is
            // hash-dependent).
            ch.sort_by(|a, b| {
                self_time
                    .get(b)
                    .unwrap_or(&0.0)
                    .partial_cmp(self_time.get(a).unwrap_or(&0.0))
                    .unwrap()
                    .then_with(|| a.cmp(b))
            });
            for c in ch.into_iter().rev() {
                stack.push(c);
            }
        }
    }

    // Inclusive time: reverse DFS order accumulates children into parents.
    let mut inclusive: crate::analysis::ProfileSelfTimes = self_time.clone();
    for &id in order.iter().rev() {
        let Some(Some(p)) = nodes.get(&id).map(|n| n.2) else {
            continue;
        };
        let pk = (id.0, id.1, id.2, p);
        if nodes.contains_key(&pk) {
            let inc = *inclusive.get(&id).unwrap_or(&0.0);
            *inclusive.entry(pk).or_default() += inc;
        }
    }

    // Match semantics: AND when both filters are given.
    let matched = |n: &(String, String, Option<u64>)| -> bool {
        let name_ok = name_matcher.is_none_or(|m| m.matches(&n.0));
        let url_ok = url_matcher.is_none_or(|m| m.matches(&n.1));
        name_ok && url_ok
    };
    let has_filter = name_matcher.is_some() || url_matcher.is_some();

    // has_match: node itself or any descendant matches.
    let mut has_match: rustc_hash::FxHashMap<crate::analysis::NodeKey, bool> =
        rustc_hash::FxHashMap::default();
    for &id in order.iter().rev() {
        let m = matched(nodes.get(&id).unwrap());
        let kids = children
            .get(&id)
            .map(|ch| {
                ch.iter()
                    .any(|c| has_match.get(c).copied().unwrap_or(false))
            })
            .unwrap_or(false);
        has_match.insert(id, m || (has_filter && kids));
    }

    let total_us: f64 = self_time.values().sum();

    // Emit pruned tree, depth-first, children by inclusive desc, top-limit.
    struct Row {
        depth: usize,
        id: crate::analysis::NodeKey,
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut roots_sorted = roots.clone();
    // Inclusive desc; exact ties break by node id (map order is
    // hash-dependent).
    roots_sorted.sort_by(|a, b| {
        inclusive
            .get(b)
            .unwrap_or(&0.0)
            .partial_cmp(inclusive.get(a).unwrap_or(&0.0))
            .unwrap()
            .then_with(|| a.cmp(b))
    });
    let mut stack: Vec<(crate::analysis::NodeKey, usize)> =
        roots_sorted.into_iter().map(|r| (r, 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        if !has_match.get(&id).copied().unwrap_or(false) {
            continue;
        }
        rows.push(Row { depth, id });
        if rows.len() >= top {
            break;
        }
        if let Some(ch) = children.get(&id) {
            let mut ch: Vec<crate::analysis::NodeKey> = ch
                .iter()
                .copied()
                .filter(|c| has_match.get(c).copied().unwrap_or(false))
                .collect();
            ch.sort_by(|a, b| {
                inclusive
                    .get(b)
                    .unwrap_or(&0.0)
                    .partial_cmp(inclusive.get(a).unwrap_or(&0.0))
                    .unwrap()
                    .then_with(|| a.cmp(b))
            });
            for c in ch.into_iter().rev() {
                stack.push((c, depth + 1));
            }
        }
    }

    let filter_desc = match (name_matcher, url_matcher) {
        (Some(n), Some(u)) => format!("{} + url~{}", n.label(), u.label()),
        (Some(n), None) => format!("~ {}", n.label()),
        (None, Some(u)) => format!("url~{}", u.label()),
        (None, None) => "all nodes".to_string(),
    };
    let mut out = String::new();
    out.push_str(&format!(
        "## CPU call tree (inclusive, {} nodes shown, {})\n\n",
        rows.len(),
        filter_desc,
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
    }
    out.push_str(&format!("- **Total sampled**: {}ms\n\n", fmt_ms(total_us)));

    let mut json_rows: Vec<Value> = Vec::new();
    if rows.is_empty() {
        out.push_str("No matching nodes.\n\n");
        return (out, Value::Array(json_rows));
    }

    out.push_str("```text\n");
    for row in &rows {
        let (name, url, _) = nodes.get(&row.id).unwrap();
        let inc = *inclusive.get(&row.id).unwrap_or(&0.0);
        let own = *self_time.get(&row.id).unwrap_or(&0.0);
        let pct = if total_us > 0.0 {
            inc / total_us * 100.0
        } else {
            0.0
        };
        let label = if name.is_empty() { "(anonymous)" } else { name };
        let indent = "  ".repeat(row.depth);
        let file = if url.is_empty() {
            String::new()
        } else {
            format!("  [{}]", short_url(url))
        };
        out.push_str(&format!(
            "{}{}{}  self {:.2}ms · incl {:.2}ms ({:.1}%)\n",
            indent,
            label,
            file,
            own / 1000.0,
            inc / 1000.0,
            pct,
        ));
        json_rows.push(json!({
            "depth": row.depth,
            "function": if name.is_empty() { "(anonymous)" } else { name },
            "url": url,
            "self_us": own.round(),
            "inclusive_us": inc.round(),
            "pct": pct,
        }));
    }
    out.push_str("```\n");
    if rows.len() >= top && !has_filter {
        out.push_str(&format!(
            "\n_Showing {} of {} nodes (use --top to see more)._",
            rows.len(),
            order.len()
        ));
    }
    out.push('\n');
    (out, Value::Array(json_rows))
}

fn fmt_ms(us: f64) -> String {
    // `+ 0.0` collapses -0.0 to +0.0 (see inspect.rs `fmt_ms`).
    format!("{:.2}", us / 1000.0 + 0.0)
}

// ── Delta: PRE / SHOOT / POST (--delta) ──

#[derive(Clone)]
struct WindowStats {
    frames_n: usize,
    frames: [f64; 4], // p50, p90, p99, max
    dropped: usize,
    runtask_us: f64,
    js_us: f64,
    gc_us: f64,
    gc_count: usize,
    lt_count: usize,
    lt_us: f64,
    cpu_us: f64,
    top_cpu: Vec<(String, f64)>,
}

fn window_stats_from_acc(
    acc: &WindowAcc,
    frames: &[f64],
    cpu_us: f64,
    top_cpu: Vec<(String, f64)>,
) -> WindowStats {
    let mut sorted = frames.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (p50, p90, p99, max) = percentiles(&sorted);
    WindowStats {
        frames_n: sorted.len(),
        frames: [p50, p90, p99, max],
        dropped: acc.dropped,
        runtask_us: acc.runtask,
        js_us: acc.js,
        gc_us: acc.gc,
        gc_count: acc.gc_count,
        lt_count: acc.lt_count,
        lt_us: acc.lt_us,
        cpu_us,
        top_cpu,
    }
}

struct WindowAcc {
    dropped: usize,
    runtask: f64,
    js: f64,
    gc: f64,
    gc_count: usize,
    lt_count: usize,
    lt_us: f64,
}

/// Compute PRE/SHOOT/POST stats in one sweep: a single event pass for the
/// counter metrics and a single multi-window CPU profile scan (the three
/// windows share one chunk-bases walk and one parallel scan).
#[allow(clippy::too_many_arguments)]
fn delta_windows_stats(
    events: &[TraceEvent],
    pre: (f64, f64),
    shoot: (f64, f64),
    post: (f64, f64),
    frame_event: &str,
    lt_ms: f64,
    main_tid: u64,
) -> (WindowStats, WindowStats, WindowStats) {
    let lt_us = lt_ms * 1000.0;
    let overheads = find_profiler_overhead(events);
    let wins = [pre, shoot, post];
    let mut accs = [
        WindowAcc {
            dropped: 0,
            runtask: 0.0,
            js: 0.0,
            gc: 0.0,
            gc_count: 0,
            lt_count: 0,
            lt_us: 0.0,
        },
        WindowAcc {
            dropped: 0,
            runtask: 0.0,
            js: 0.0,
            gc: 0.0,
            gc_count: 0,
            lt_count: 0,
            lt_us: 0.0,
        },
        WindowAcc {
            dropped: 0,
            runtask: 0.0,
            js: 0.0,
            gc: 0.0,
            gc_count: 0,
            lt_count: 0,
            lt_us: 0.0,
        },
    ];

    for e in events {
        let ts = e.ts;
        for (wi, (lo, hi)) in wins.iter().enumerate() {
            // Left-closed, right-open (same as `paired_durations` and the
            // CPU sample attribution): adjacent PRE/SHOOT/POST windows never
            // share an event; frames are attributed to their submit time.
            if ts < *lo || ts >= *hi {
                continue;
            }
            if e.ph == b'X' && e.tid == main_tid {
                match e.name.as_str() {
                    "RunTask" => {
                        if e.dur.is_some() {
                            let eff = effective_runtask_dur(e, &overheads);
                            accs[wi].runtask += eff;
                            if eff > lt_us {
                                accs[wi].lt_count += 1;
                                accs[wi].lt_us += eff;
                            }
                        }
                    }
                    "FunctionCall" => accs[wi].js += e.dur.unwrap_or(0.0),
                    _ => {}
                }
            }
            // Same minor-GC family `gc_section` counts (MinorGC / V8.GCScavenger);
            // omitting the scavenger name made --delta report 0 GC where --gc
            // showed scavenges.
            match e.name.as_str() {
                "MajorGC" | "MinorGC" | "V8.GCScavenger" if e.ph == b'X' => {
                    accs[wi].gc += e.dur.unwrap_or(0.0);
                    accs[wi].gc_count += 1;
                }
                "DroppedFrame" if !is_profiler_dropped_frame(e.ts, &overheads) => {
                    accs[wi].dropped += 1;
                }
                _ => {}
            }
        }
    }

    let frame_wins = [
        paired_durations(events, frame_event, Some(pre)),
        paired_durations(events, frame_event, Some(shoot)),
        paired_durations(events, frame_event, Some(post)),
    ];

    let windows: [Option<(f64, f64)>; 3] = [Some(pre), Some(shoot), Some(post)];
    let (node_map, times) = crate::analysis::scan_profile_chunks_windows(events, None, &windows, 0);

    let mut out: [WindowStats; 3] = std::array::from_fn(|_| WindowStats {
        frames_n: 0,
        frames: [0.0; 4],
        dropped: 0,
        runtask_us: 0.0,
        js_us: 0.0,
        gc_us: 0.0,
        gc_count: 0,
        lt_count: 0,
        lt_us: 0.0,
        cpu_us: 0.0,
        top_cpu: Vec::new(),
    });
    for wi in 0..3 {
        let times_map = &times[wi];
        let cpu_us: f64 = times_map.values().sum();
        let mut top_cpu: Vec<(String, f64)> = times_map
            .iter()
            .filter_map(|(id, t)| node_map.get(id).map(|(n, _, _)| (n.clone(), *t)))
            .filter(|(n, _)| !n.is_empty())
            .collect();
        // Self-time desc; exact ties break by name asc (map order is
        // hash-dependent).
        top_cpu.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap()
                .then_with(|| a.0.cmp(&b.0))
        });
        top_cpu.truncate(3);
        out[wi] = window_stats_from_acc(&accs[wi], &frame_wins[wi], cpu_us, top_cpu);
    }
    (out[0].clone(), out[1].clone(), out[2].clone())
}

// ── Delta data (raw, shared by the per-trace section and windowed compare) ──

/// One metric row of the PRE/SHOOT/POST comparison in raw units: µs for
/// `ms` metrics, counts for `n` metrics.
#[derive(Clone)]
pub struct DeltaRow {
    pub metric: String,
    pub unit: &'static str,
    pub pre: f64,
    pub shoot: f64,
    pub post: f64,
}

impl DeltaRow {
    pub fn delta_pre(&self) -> f64 {
        self.shoot - self.pre
    }
    pub fn delta_post(&self) -> f64 {
        self.post - self.shoot
    }
}

/// Raw delta analysis for one trace.
#[derive(Clone)]
pub struct DeltaData {
    pub rows: Vec<DeltaRow>,
    /// Top CPU self-time (function, µs) per window: [PRE, SHOOT, POST].
    pub top_cpu: [Vec<(String, f64)>; 3],
    pub anchor_us: f64,
    pub pre: (f64, f64),
    pub shoot: (f64, f64),
    pub post: (f64, f64),
    /// Frame budget (µs) in effect for this trace (detected or override).
    pub frame_budget_us: f64,
}

/// Compute the PRE/SHOOT/POST metric rows for one trace (frames, dropped
/// frames, GC, long tasks, busy/JS time, CPU samples) in raw units.
#[allow(clippy::too_many_arguments)]
pub fn delta_data(
    events: &[TraceEvent],
    pre: (f64, f64),
    shoot: (f64, f64),
    post: (f64, f64),
    anchor_ts: f64,
    frame_event: &str,
    lt_ms: f64,
) -> DeltaData {
    delta_data_with_budget(
        events,
        pre,
        shoot,
        post,
        anchor_ts,
        frame_event,
        lt_ms,
        detect_frame_budget_us(events),
    )
}

/// `delta_data` with an explicit frame budget (µs), recorded on the result.
/// Prefer `detect_frame_budget_us` or a `--frame-budget` override.
#[allow(clippy::too_many_arguments)]
pub fn delta_data_with_budget(
    events: &[TraceEvent],
    pre: (f64, f64),
    shoot: (f64, f64),
    post: (f64, f64),
    anchor_ts: f64,
    frame_event: &str,
    lt_ms: f64,
    budget_us: f64,
) -> DeltaData {
    let main_tid = crate::trace::detect_main_thread(events);
    let (s_pre, s_shoot, s_post) =
        delta_windows_stats(events, pre, shoot, post, frame_event, lt_ms, main_tid);

    let mut rows: Vec<DeltaRow> = Vec::new();
    let mut push = |metric: &str, unit: &'static str, pre: f64, shoot: f64, post: f64| {
        rows.push(DeltaRow {
            metric: metric.to_string(),
            unit,
            pre,
            shoot,
            post,
        });
    };
    push(
        "frames",
        "n",
        s_pre.frames_n as f64,
        s_shoot.frames_n as f64,
        s_post.frames_n as f64,
    );
    for (i, name) in ["frame p50", "frame p90", "frame p99", "frame max"]
        .iter()
        .enumerate()
    {
        push(
            name,
            "ms",
            s_pre.frames[i],
            s_shoot.frames[i],
            s_post.frames[i],
        );
    }
    push(
        "dropped frames",
        "n",
        s_pre.dropped as f64,
        s_shoot.dropped as f64,
        s_post.dropped as f64,
    );
    push(
        &format!("long tasks >{:.0}ms", lt_ms),
        "n",
        s_pre.lt_count as f64,
        s_shoot.lt_count as f64,
        s_post.lt_count as f64,
    );
    push(
        "long task time",
        "ms",
        s_pre.lt_us,
        s_shoot.lt_us,
        s_post.lt_us,
    );
    push(
        "main busy (RunTask)",
        "ms",
        s_pre.runtask_us,
        s_shoot.runtask_us,
        s_post.runtask_us,
    );
    push(
        "JS (FunctionCall)",
        "ms",
        s_pre.js_us,
        s_shoot.js_us,
        s_post.js_us,
    );
    push(
        "GC (Major+Minor)",
        "ms",
        s_pre.gc_us,
        s_shoot.gc_us,
        s_post.gc_us,
    );
    push(
        "GC count",
        "n",
        s_pre.gc_count as f64,
        s_shoot.gc_count as f64,
        s_post.gc_count as f64,
    );
    push(
        "CPU samples",
        "ms",
        s_pre.cpu_us,
        s_shoot.cpu_us,
        s_post.cpu_us,
    );

    DeltaData {
        rows,
        top_cpu: [s_pre.top_cpu, s_shoot.top_cpu, s_post.top_cpu],
        anchor_us: anchor_ts,
        pre,
        shoot,
        post,
        frame_budget_us: budget_us,
    }
}

/// Compare PRE / SHOOT / POST windows around an anchor: frame stats, dropped
/// frames, GC, long tasks, main-thread busy/JS and CPU sample totals, plus
/// SHOOT−PRE and POST−SHOOT deltas.
///
/// Frames are attributed to their submit time (window holds the `b` event);
/// see `paired_durations`.
#[allow(clippy::too_many_arguments)]
pub fn delta_section(
    events: &[TraceEvent],
    pre: (f64, f64),
    shoot: (f64, f64),
    post: (f64, f64),
    anchor_ts: f64,
    frame_event: &str,
    lt_ms: f64,
    min_ts: f64,
) -> (String, Value) {
    let data = delta_data(events, pre, shoot, post, anchor_ts, frame_event, lt_ms);
    delta_section_from_data(&data, min_ts)
}

/// Render the PRE/SHOOT/POST section from already-computed `DeltaData`, so
/// callers that also need the raw rows (e.g. the two-trace compare) compute
/// `delta_data` exactly once per trace.
pub fn delta_section_from_data(data: &DeltaData, min_ts: f64) -> (String, Value) {
    let ms = |v: f64| format!("{:.1}", v / 1000.0 + 0.0);
    let dms = |a: f64, b: f64| format!("{:+.1}", (b - a) / 1000.0 + 0.0);
    let dcount = |a: f64, b: f64| format!("{:+}", b as i64 - a as i64);
    let num = |v: f64| (v / 1000.0 * 10.0).round() / 10.0; // ms, 1 decimal

    let mut out = String::new();
    out.push_str("## Delta: PRE → SHOOT → POST\n\n");
    out.push_str(&format!(
        "- **SHOOT**: {:.2}ms … {:.2}ms from trace start\n",
        (data.shoot.0 - min_ts) / 1000.0,
        (data.shoot.1 - min_ts) / 1000.0,
    ));
    out.push_str(&format!(
        "- **PRE**:  {:.2}ms … {:.2}ms\n",
        (data.pre.0 - min_ts) / 1000.0,
        (data.pre.1 - min_ts) / 1000.0,
    ));
    out.push_str(&format!(
        "- **POST**: {:.2}ms … {:.2}ms\n\n",
        (data.post.0 - min_ts) / 1000.0,
        (data.post.1 - min_ts) / 1000.0,
    ));

    out.push_str("| metric | PRE | SHOOT | POST | SHOOT−PRE | POST−SHOOT |\n");
    out.push_str("|--------|-----|-------|------|-----------|------------|\n");
    let mut json_rows: Vec<Value> = Vec::new();
    for r in &data.rows {
        let dpre = r.delta_pre();
        let dpost = r.delta_post();
        let (pv, sv, pvv, dprev, dpostv) = if r.unit == "n" {
            (
                format!("{:.0}", r.pre),
                format!("{:.0}", r.shoot),
                format!("{:.0}", r.post),
                dcount(r.pre, r.shoot),
                dcount(r.shoot, r.post),
            )
        } else {
            (
                ms(r.pre),
                ms(r.shoot),
                ms(r.post),
                dms(r.pre, r.shoot),
                dms(r.shoot, r.post),
            )
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            r.metric, pv, sv, pvv, dprev, dpostv,
        ));
        json_rows.push(json!({
            "metric": r.metric,
            "unit": r.unit,
            "pre": if r.unit == "n" { r.pre } else { num(r.pre) },
            "shoot": if r.unit == "n" { r.shoot } else { num(r.shoot) },
            "post": if r.unit == "n" { r.post } else { num(r.post) },
            "delta_pre": if r.unit == "n" { dpre } else { num(dpre) },
            "delta_post": if r.unit == "n" { dpost } else { num(dpost) },
        }));
    }
    out.push('\n');

    // Top CPU self-time per window.
    out.push_str("| window | top CPU self-time |\n");
    out.push_str("|--------|-------------------|\n");
    for (label, top_cpu) in [
        ("PRE", &data.top_cpu[0]),
        ("SHOOT", &data.top_cpu[1]),
        ("POST", &data.top_cpu[2]),
    ] {
        let top: String = if top_cpu.is_empty() {
            "—".to_string()
        } else {
            top_cpu
                .iter()
                .map(|(n, t)| format!("{} ({:.1}ms)", n, t / 1000.0))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!("| {} | {} |\n", label, top));
    }
    out.push('\n');

    let top_json = |top_cpu: &[(String, f64)]| -> Value {
        Value::Array(
            top_cpu
                .iter()
                .map(|(n, t)| json!({"function": n, "self_us": t.round()}))
                .collect(),
        )
    };
    let mut obj = serde_json::Map::new();
    obj.insert("anchor_us".into(), json!(data.anchor_us.round()));
    obj.insert(
        "frame_budget_us".into(),
        json!(data.frame_budget_us.round()),
    );
    obj.insert(
        "windows".into(),
        json!({
            "pre": [data.pre.0.round(), data.pre.1.round()],
            "shoot": [data.shoot.0.round(), data.shoot.1.round()],
            "post": [data.post.0.round(), data.post.1.round()],
        }),
    );
    obj.insert("metrics".into(), Value::Array(json_rows));
    obj.insert(
        "top_cpu".into(),
        json!({
            "pre": top_json(&data.top_cpu[0]),
            "shoot": top_json(&data.top_cpu[1]),
            "post": top_json(&data.top_cpu[2]),
        }),
    );
    (out, Value::Object(obj))
}

// ── CSV ──

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn json_to_field(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(_) | Value::Bool(_) => v.to_string(),
        other => other.to_string(),
    }
}

/// Render a JSON array of objects as CSV (header from the first row's keys).
/// Nested objects/arrays are serialized as compact JSON inside the field.
pub fn rows_to_csv(rows: &[Value]) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        return out;
    }
    let Some(obj) = rows[0].as_object() else {
        return out;
    };
    let keys: Vec<String> = obj.keys().cloned().collect();
    out.push_str(
        &keys
            .iter()
            .map(|k| csv_field(k))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for r in rows {
        let Some(o) = r.as_object() else { continue };
        let vals: Vec<String> = keys
            .iter()
            .map(|k| csv_field(&json_to_field(o.get(k).unwrap_or(&Value::Null))))
            .collect();
        out.push_str(&vals.join(","));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::TraceEvent;

    fn ev(
        name: &str,
        ph: &str,
        ts: f64,
        dur: Option<f64>,
        args: Option<serde_json::Value>,
    ) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph: ph.as_bytes().first().copied().unwrap_or(0),
            ts,
            dur,
            tid: 1,
            pid: 2,
            cat: None,
            args: args.and_then(crate::trace::test_args),
        }
    }

    fn chunk(
        ts: f64,
        nodes: &[(u64, &str, &str, Option<u64>)],
        samples: &[u64],
        deltas: &[f64],
    ) -> TraceEvent {
        let nodes_json: Vec<serde_json::Value> = nodes
            .iter()
            .map(|(id, name, url, parent)| {
                serde_json::json!({"id": id, "callFrame": {"functionName": name, "url": url}, "parent": parent})
            })
            .collect();
        ev(
            "ProfileChunk",
            "P",
            ts,
            None,
            Some(serde_json::json!({
                "data": {"cpuProfile": {"nodes": nodes_json, "samples": samples}, "timeDeltas": deltas}
            })),
        )
    }

    fn fc(ts: f64, fn_name: &str) -> TraceEvent {
        ev(
            "FunctionCall",
            "X",
            ts,
            Some(1000.0),
            Some(serde_json::json!({"data": {"functionName": fn_name}})),
        )
    }

    /// Anchor priority: FunctionCall functionName beats cpu-profile beats args.
    #[test]
    fn anchor_prefers_functioncall_then_cpu_then_args() {
        let events = vec![
            ev("Profile", "P", 1_000_000.0, None, None),
            // args match only, earliest of all
            ev(
                "WebSocketSend",
                "X",
                1_000_100.0,
                Some(10.0),
                Some(serde_json::json!({"data": {"msg": "player_shoot"}})),
            ),
            // cpu-profile node "shoot" first sampled at 1_001_000
            chunk(
                1_002_000.0,
                &[
                    (1, "(root)", "", None),
                    (9, "shoot", "http://x/weapon.ts", Some(1)),
                ],
                &[9, 9],
                &[1000.0, 1000.0],
            ),
            // FunctionCall shoot — latest, but pass 1 wins by priority
            fc(2_000_000.0, "shoot"),
        ];
        let m = Matcher::new("shoot", false).unwrap();
        let a = find_anchor(&events, &m).expect("anchor");
        assert_eq!(a.kind, "FunctionCall");
        assert_eq!(a.ts, 2_000_000.0);

        // No FunctionCall match → cpu-profile by earliest sample time.
        let m2 = Matcher::new("weapon.ts", false).unwrap();
        let a2 = find_anchor(&events, &m2).expect("anchor");
        assert_eq!(a2.kind, "cpu-profile");
        assert_eq!(a2.ts, 1_001_000.0);

        // Function names preferred over URL-only matches (shooterX trap).
        let events2 = vec![
            ev("Profile", "P", 1_000_000.0, None, None),
            chunk(
                1_002_000.0,
                &[
                    (1, "(root)", "", None),
                    (7, "setup", "http://x/games/shooterX/game.ts", Some(1)),
                    (8, "handleShoot", "http://x/weapon.ts", Some(1)),
                ],
                &[7, 7, 8],
                &[1000.0, 1000.0, 1000.0],
            ),
        ];
        let a3 = find_anchor(&events2, &m).expect("anchor");
        assert_eq!(a3.label, "handleShoot");
        assert_eq!(a3.ts, 1_003_000.0);

        // Only args match → args anchor.
        let m4 = Matcher::new("player_shoot", false).unwrap();
        let a4 = find_anchor(&events, &m4).expect("anchor");
        assert_eq!(a4.kind, "args");
        assert_eq!(a4.ts, 1_000_100.0);
    }

    /// Window rule for frame pairs: a pair belongs to the window holding
    /// its `b`. An `e` outside the window still closes an inside `b`
    /// (counted); a `b` outside is ignored even when its `e` lands inside.
    #[test]
    fn paired_durations_filter_by_begin_only() {
        let events = vec![
            ev("DrawFrame", "b", 1_000_000.0, None, None),
            ev("DrawFrame", "e", 1_020_000.0, None, None), // e outside
            ev("DrawFrame", "b", 2_000_000.0, None, None), // b outside
            ev("DrawFrame", "e", 2_005_000.0, None, None),
        ];
        let durs = paired_durations(&events, "DrawFrame", Some((0.0, 1_010_000.0)));
        assert_eq!(durs, vec![20_000.0]);
        let durs = paired_durations(&events, "DrawFrame", Some((1_990_000.0, 3_000_000.0)));
        assert_eq!(durs, vec![5_000.0]);
        // Unwindowed pairing is unaffected.
        let durs = paired_durations(&events, "DrawFrame", None);
        assert_eq!(durs, vec![20_000.0, 5_000.0]);
    }

    /// Frame b/e pairing golden: durations 16ms and 20ms, 2 dropped frames.
    #[test]
    fn frames_section_golden() {
        let events = vec![
            ev(
                "SubmitCompositorFrameToPresentationCompositorFrame",
                "b",
                1_000_000.0,
                None,
                None,
            ),
            ev(
                "SubmitCompositorFrameToPresentationCompositorFrame",
                "e",
                1_016_000.0,
                None,
                None,
            ),
            ev(
                "SubmitCompositorFrameToPresentationCompositorFrame",
                "b",
                1_017_000.0,
                None,
                None,
            ),
            ev(
                "SubmitCompositorFrameToPresentationCompositorFrame",
                "e",
                1_037_000.0,
                None,
                None,
            ),
            ev("DroppedFrame", "I", 1_003_000.0, None, None),
            ev("DroppedFrame", "I", 1_004_000.0, None, None),
        ];
        let scope = Scope {
            window: None,
            tid: None,
            pid: None,
            cat: None,
        };
        let (md, json) = frames_section(
            &events,
            &scope,
            "SubmitCompositorFrameToPresentationCompositorFrame",
            1_000_000.0,
        );
        assert!(md.contains("2 paired"), "md: {}", md);
        assert!(md.contains("Dropped frames**: 2"), "md: {}", md);
        let row = json.as_array().unwrap()[0].clone();
        assert_eq!(row["count"], 2);
        // Linear interpolation over [16000, 20000].
        assert_eq!(row["p50_us"], 18_000.0);
        assert_eq!(row["p90_us"], 19_600.0);
        assert_eq!(row["p99_us"], 19_960.0);
        assert_eq!(row["max_us"], 20_000.0);
        assert_eq!(row["dropped_frames"], 2);
    }

    fn tick(ts: f64) -> TraceEvent {
        ev(
            "SubmitCompositorFrameToPresentationCompositorFrame",
            "b",
            ts,
            None,
            None,
        )
    }

    /// 120Hz cadence (~8.33ms ticks) → 8333µs budget.
    #[test]
    fn budget_detects_120hz() {
        let events: Vec<TraceEvent> = (0..6)
            .map(|i| tick(1_000_000.0 + i as f64 * 8_333.0))
            .collect();
        assert_eq!(detect_frame_budget_us(&events), 8_333.0);
    }

    /// 90Hz cadence (~11.1ms ticks) → 11111µs budget.
    #[test]
    fn budget_detects_90hz() {
        let events: Vec<TraceEvent> = (0..6)
            .map(|i| tick(1_000_000.0 + i as f64 * 11_111.0))
            .collect();
        assert_eq!(detect_frame_budget_us(&events), 11_111.0);
    }

    /// 60Hz cadence (~16.67ms ticks) → 16667µs budget.
    #[test]
    fn budget_detects_60hz() {
        let events: Vec<TraceEvent> = (0..6)
            .map(|i| tick(1_000_000.0 + i as f64 * 16_667.0))
            .collect();
        assert_eq!(detect_frame_budget_us(&events), 16_667.0);
    }

    /// Too few ticks, or none at all → 60Hz fallback.
    #[test]
    fn budget_falls_back_without_ticks() {
        assert_eq!(detect_frame_budget_us(&[]), 16_667.0);
        let events = vec![tick(1_000_000.0), tick(1_008_333.0)];
        assert_eq!(detect_frame_budget_us(&events), 16_667.0);
    }

    /// DrawFrame ticks are used when no compositor presentation events exist.
    #[test]
    fn budget_falls_back_to_drawframe() {
        let events: Vec<TraceEvent> = (0..5)
            .map(|i| {
                ev(
                    "DrawFrame",
                    "b",
                    1_000_000.0 + i as f64 * 8_333.0,
                    None,
                    None,
                )
            })
            .collect();
        assert_eq!(detect_frame_budget_us(&events), 8_333.0);
    }

    /// Shared dropped-frame counter honors scope and the profiler freeze.
    #[test]
    fn dropped_frames_helper_matches_section() {
        let events = vec![
            tick(1_000_000.0),
            ev("DroppedFrame", "I", 1_003_000.0, None, None),
            ev("DroppedFrame", "I", 2_000_000.0, None, None),
        ];
        let scope = Scope {
            window: Some((0.0, 1_500_000.0)),
            tid: None,
            pid: None,
            cat: None,
        };
        assert_eq!(dropped_frames(&events, Some(&scope)), 1);
        assert_eq!(dropped_frames(&events, None), 2);
        let (_, json) = frames_section(
            &events,
            &scope,
            "SubmitCompositorFrameToPresentationCompositorFrame",
            1_000_000.0,
        );
        assert_eq!(json[0]["dropped_frames"], 1);
    }

    /// `_with_budget` reports the budget without changing the stats.
    #[test]
    fn frames_section_with_budget_reports_budget() {
        let events = vec![
            tick(1_000_000.0),
            ev(
                "SubmitCompositorFrameToPresentationCompositorFrame",
                "e",
                1_016_000.0,
                None,
                None,
            ),
        ];
        let scope = Scope {
            window: None,
            tid: None,
            pid: None,
            cat: None,
        };
        let (md, json) = frames_section_with_budget(
            &events,
            &scope,
            "SubmitCompositorFrameToPresentationCompositorFrame",
            1_000_000.0,
            8_333.0,
        );
        assert!(md.contains("Frame budget**: 8.33ms"), "md: {}", md);
        assert_eq!(json[0]["frame_budget_us"], 8_333.0);
        assert_eq!(json[0]["count"], 1);
        let (md_plain, _) = frames_section(
            &events,
            &scope,
            "SubmitCompositorFrameToPresentationCompositorFrame",
            1_000_000.0,
        );
        assert!(!md_plain.contains("Frame budget"));
    }

    /// GC golden: 1 Major (5ms), 1 Minor (1ms), 1 long task ≥50ms (600ms).
    #[test]
    fn gc_section_golden() {
        let events = vec![
            ev("MajorGC", "X", 1_002_000.0, Some(5000.0), None),
            ev("MinorGC", "X", 1_010_000.0, Some(1000.0), None),
            ev("RunTask", "X", 1_000_000.0, Some(600_000.0), None),
            ev("RunTask", "X", 1_050_000.0, Some(20_000.0), None),
        ];
        let scope = Scope {
            window: None,
            tid: None,
            pid: None,
            cat: None,
        };
        let (md, json) = gc_section(&events, &scope, 50.0, 1_000_000.0);
        assert!(md.contains("| MajorGC | 1 | 5.00 | 5.00 |"));
        assert!(md.contains("Long tasks >50ms**: 1 total, 600.0ms combined"));
        let row = json.as_array().unwrap()[0].clone();
        assert_eq!(row["gc"][0]["count"], 1);
        assert_eq!(row["gc"][0]["total_us"], 5000.0);
        assert_eq!(row["long_tasks"]["count"], 1);
        assert_eq!(row["long_tasks"]["total_us"], 600_000.0);
    }

    /// Delta compares three windows in one sweep (golden totals from the
    /// fixture semantics: chunk1 samples 1_005_000..1_035_000).
    #[test]
    fn delta_section_golden() {
        let events = vec![
            ev("Profile", "P", 1_000_000.0, None, None),
            chunk(
                1_055_000.0,
                &[
                    (1, "(root)", "", None),
                    (2, "shoot", "", Some(1)),
                    (3, "update", "", Some(1)),
                ],
                &[2, 3, 2, 3, 2, 3, 2],
                &[5000.0; 7],
            ),
        ];
        // SHOOT = [1_000_000, 1_030_000]; PRE/POST empty.
        let (md, json) = delta_section(
            &events,
            (300_000.0, 900_000.0),
            (1_000_000.0, 1_030_000.0),
            (2_000_000.0, 2_500_000.0),
            1_010_000.0,
            "SubmitCompositorFrameToPresentationCompositorFrame",
            50.0,
            1_000_000.0,
        );
        assert!(md.contains("Delta: PRE → SHOOT → POST"));
        let obj = json.as_object().unwrap();
        let metrics = obj["metrics"].as_array().unwrap();
        let cpu = metrics
            .iter()
            .find(|m| m["metric"] == "CPU samples")
            .unwrap();
        assert_eq!(cpu["pre"], 0.0);
        // 5 × 5ms samples: the one exactly at 1_030_000 is the next
        // window's ([lo, hi)), not SHOOT's.
        assert_eq!(cpu["shoot"], 25.0); // ms in the delta table
        assert_eq!(cpu["delta_pre"], 25.0); // 25ms
    }

    /// CSV escaping: commas, quotes and newlines inside fields.
    #[test]
    fn csv_escapes_fields() {
        let rows = serde_json::from_str::<Vec<Value>>(
            r#"[{"a": "x,y", "b": 1}, {"a": "say \"hi\"", "b": 2}]"#,
        )
        .unwrap();
        let csv = rows_to_csv(&rows);
        assert_eq!(csv, "a,b\n\"x,y\",1\n\"say \"\"hi\"\"\",2\n");
    }
}
