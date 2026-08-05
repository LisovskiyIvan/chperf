//! Independent Rust reference cross-check of the chperf binary's `--json`
//! output.
//!
//! chperf is a bin crate (no lib.rs), so integration tests cannot import
//! its internals. This test instead re-implements the trace-analysis
//! semantics from scratch (std + serde_json + flate2 only) and compares its
//! results against what the binary actually prints. A mismatch means the
//! binary's observable behavior drifted from the semantics documented here.
//!
//! Semantics cross-checked (all verified against the tool at commit 2ce4455):
//! - events: traceEvents array; name/ph (strings), ts (f64, default 0),
//!   dur (Option<f64>), tid/pid (u64), args (optional JSON object)
//! - main thread: first RunTask ph X with dur > 500_000 us, else the tid
//!   with the most RunTask ph X events
//! - CPU samples: ProfileChunk events in array order; per-pid walk anchored
//!   on the first Profile (ph P) event, `prev += timeDeltas[i]` with RAW
//!   values (may be negative), sample weight = max(0, timeDeltas[i]);
//!   `prev` carries across chunks of the same pid
//! - anchor (cpu-profile pass): earliest sample time per node id, function
//!   name match preferred over URL match, case-insensitive substring
//! - frames: same-tid b/e pairs via a stack (e.ts - b.ts when positive);
//!   the window filter applies to BOTH b and e events (verified against the
//!   binary: an e outside the window leaves its b unpaired)
//! - dropped frames: events named DroppedFrame with ts in window
//! - GC groups (ph X only, dur.unwrap_or(0.0)): major = MajorGC,
//!   minor = MinorGC | V8.GCScavenger, other = V8.GC*/CppGC* prefixes
//! - delta metrics: MajorGC+MinorGC only, counted on any tid; RunTask /
//!   FunctionCall restricted to the main tid
//! - long tasks: RunTask on the main tid with dur >= threshold
//! - percentiles: sorted ascending, idx = round(p/100 * (n-1)) clamped

use flate2::read::GzDecoder;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_chperf");
const MEDIUM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/medium.json");
const FRAME_EVENT: &str = "SubmitCompositorFrameToPresentationCompositorFrame";

// ── Trace loading ──

#[derive(Debug)]
struct Event {
    name: String,
    ph: String,
    ts: f64,
    dur: Option<f64>,
    tid: u64,
    pid: u64,
    args: Option<Value>,
}

fn load_trace(path: &Path) -> Vec<Event> {
    let raw = std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let text = if path.to_string_lossy().ends_with(".gz") {
        let mut dec = GzDecoder::new(&raw[..]);
        let mut s = String::new();
        dec.read_to_string(&mut s)
            .unwrap_or_else(|e| panic!("cannot gunzip {}: {e}", path.display()));
        s
    } else {
        String::from_utf8(raw).unwrap_or_else(|e| panic!("{} is not UTF-8: {e}", path.display()))
    };
    let mut root: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()));
    let arr = match &mut root {
        Value::Object(m) => m.remove("traceEvents").unwrap_or(Value::Array(Vec::new())),
        _ => Value::Array(Vec::new()),
    };
    let Value::Array(items) = arr else {
        panic!("traceEvents of {} is not an array", path.display());
    };
    let mut events = Vec::with_capacity(items.len());
    for it in items {
        events.push(Event {
            name: it.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            ph: it.get("ph").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            ts: it.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0),
            dur: it.get("dur").and_then(|v| v.as_f64()),
            tid: it.get("tid").and_then(|v| v.as_u64()).unwrap_or(0),
            pid: it.get("pid").and_then(|v| v.as_u64()).unwrap_or(0),
            args: it.get("args").cloned(),
        });
    }
    events
}

// ── Main thread ──

fn detect_main_tid(events: &[Event]) -> u64 {
    for e in events {
        if e.name == "RunTask" && e.ph == "X"
            && let Some(dur) = e.dur
            && dur > 500_000.0 {
                return e.tid;
            }
    }
    let mut counts: HashMap<u64, usize> = HashMap::new();
    for e in events {
        if e.name == "RunTask" && e.ph == "X" {
            *counts.entry(e.tid).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(tid, _)| tid)
        .unwrap_or(0)
}

// ── CPU sample walk ──

#[derive(Debug)]
struct Sample {
    ts: f64,
    weight: f64,
}

#[derive(Debug)]
struct CpuScan {
    /// All samples in chunk-walk order with absolute times and weights.
    samples: Vec<Sample>,
    /// Node table: id -> (functionName, url). First occurrence wins.
    nodes: HashMap<u64, (String, String)>,
}

/// Walk ProfileChunk events in array order (scan-style, mirroring the
/// binary's windowed scan): per pid, `prev` starts at the first Profile
/// (ph P) event's ts (or 0) and carries across chunks. The `prev` advance
/// depends ONLY on `args.data.timeDeltas`: a chunk with a non-empty
/// timeDeltas array advances `prev` by the FULL delta sum even when the
/// cpuProfile/samples arrays are absent (Chrome emits such chunks).
/// Chunks without timeDeltas contribute nothing and do not advance `prev`.
///
/// Sample times use the scan's record-first formula: a chunk's first
/// sample sits at `base = prev_at_chunk_start + timeDeltas[0]`, and each
/// sample is recorded BEFORE its delta is added — so sample i sits at
/// `base + timeDeltas[0..i]` (unlike the anchor walk's advance-first
/// formula, which puts sample i at `prev + timeDeltas[0..=i]`; the two
/// agree when a chunk's deltas are uniform). Weight is `max(0.0, delta)`.
fn cpu_scan(events: &[Event]) -> CpuScan {
    let mut starts: HashMap<u64, f64> = HashMap::new();
    for e in events {
        if e.name == "Profile" && e.ph == "P" {
            starts.entry(e.pid).or_insert(e.ts);
        }
    }
    let mut samples = Vec::new();
    let mut nodes: HashMap<u64, (String, String)> = HashMap::new();
    let mut prev_last: HashMap<u64, f64> = HashMap::new();
    for e in events {
        if e.name != "ProfileChunk" {
            continue;
        }
        let Some(args) = &e.args else { continue };
        let Some(data) = args.get("data") else { continue };
        if let Some(cp) = data.get("cpuProfile")
            && let Some(list) = cp.get("nodes").and_then(|n| n.as_array()) {
                for node in list {
                    let id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                    let cf = node.get("callFrame");
                    let name = cf
                        .and_then(|c| c.get("functionName"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("(anonymous)")
                        .to_string();
                    let url = cf
                        .and_then(|c| c.get("url"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    nodes.entry(id).or_insert((name, url));
                }
            }
        let Some(deltas) = data.get("timeDeltas").and_then(|t| t.as_array()) else {
            continue;
        };
        let Some(first) = deltas.first().and_then(|v| v.as_f64()) else {
            continue;
        };
        let pl = prev_last
            .get(&e.pid)
            .copied()
            .unwrap_or_else(|| starts.get(&e.pid).copied().unwrap_or(0.0));
        if let Some(cp) = data.get("cpuProfile")
            && let Some(samples_arr) = cp.get("samples").and_then(|s| s.as_array()) {
                let n = samples_arr.len().min(deltas.len());
                let mut cur = pl + first;
                for d in deltas.iter().take(n) {
                    let d = d.as_f64().unwrap_or(0.0);
                    samples.push(Sample {
                        ts: cur,
                        weight: d.max(0.0),
                    });
                    cur += d;
                }
            }
        let sum: f64 = deltas.iter().filter_map(|v| v.as_f64()).sum();
        prev_last.insert(e.pid, pl + sum);
    }
    CpuScan { samples, nodes }
}

// ── Anchor detection (case-insensitive substring) ──

#[derive(Debug, PartialEq, Clone)]
enum AnchorKind {
    FunctionCall,
    CpuProfile,
    Args,
}

#[derive(Debug, Clone)]
struct AnchorHit {
    ts: f64,
    kind: AnchorKind,
    label: String,
}

fn matches_ignore_case(hay: &str, needle: &str) -> bool {
    !needle.is_empty() && hay.to_lowercase().contains(&needle.to_lowercase())
}

/// First sample time per node id, using the binary's anchor-pass walk: the
/// per-pid `prev` advances ONLY when a chunk carries both a samples and a
/// timeDeltas array (by the iterated min-length prefix) — unlike the scan
/// walk, which advances on timeDeltas alone. Both walks exist in the tool
/// and give the same times on well-formed chunks.
fn anchor_node_first(events: &[Event], starts: &HashMap<u64, f64>) -> HashMap<u64, f64> {
    let mut first: HashMap<u64, f64> = HashMap::new();
    let mut prev_last: HashMap<u64, f64> = HashMap::new();
    for e in events {
        if e.name != "ProfileChunk" {
            continue;
        }
        let Some(args) = &e.args else { continue };
        let Some(data) = args.get("data") else { continue };
        let Some(cp) = data.get("cpuProfile") else { continue };
        let Some(samples_arr) = cp.get("samples").and_then(|s| s.as_array()) else { continue };
        let Some(deltas) = data.get("timeDeltas").and_then(|t| t.as_array()) else { continue };
        let n = samples_arr.len().min(deltas.len());
        let mut cur = prev_last
            .get(&e.pid)
            .copied()
            .unwrap_or_else(|| starts.get(&e.pid).copied().unwrap_or(0.0));
        for i in 0..n {
            let d = deltas[i].as_f64().unwrap_or(0.0);
            cur += d;
            let node = samples_arr[i].as_u64().unwrap_or(0);
            if node != 0 {
                let t = first.entry(node).or_insert(f64::INFINITY);
                if cur < *t {
                    *t = cur;
                }
            }
        }
        prev_last.insert(e.pid, cur);
    }
    first
}

/// Priority: FunctionCall data.functionName, then CPU-profile node names
/// (function name before URL, earliest sample time), then any event args.
fn find_anchor(events: &[Event], scan: &CpuScan, pattern: &str) -> Option<AnchorHit> {
    let mut best: Option<(f64, String)> = None;
    for e in events {
        if e.name != "FunctionCall" || e.ph != "X" {
            continue;
        }
        let Some(fn_name) = e
            .args
            .as_ref()
            .and_then(|a| a.get("data"))
            .and_then(|d| d.get("functionName"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        if matches_ignore_case(fn_name, pattern) && best.as_ref().is_none_or(|b| e.ts < b.0) {
            best = Some((e.ts, fn_name.to_string()));
        }
    }
    if let Some((ts, label)) = best {
        return Some(AnchorHit { ts, kind: AnchorKind::FunctionCall, label });
    }

    let mut starts: HashMap<u64, f64> = HashMap::new();
    for e in events {
        if e.name == "Profile" && e.ph == "P" {
            starts.entry(e.pid).or_insert(e.ts);
        }
    }
    let first = anchor_node_first(events, &starts);
    let pick = |name: &str| -> Option<(f64, String)> {
        let mut b: Option<(f64, String)> = None;
        for (id, t) in &first {
            if *t == f64::INFINITY {
                continue;
            }
            let Some((n, u)) = scan.nodes.get(id) else { continue };
            let hay = if name == "functionName" { n } else { u };
            if matches_ignore_case(hay, pattern) && b.as_ref().is_none_or(|old| *t < old.0) {
                b = Some((*t, hay.clone()));
            }
        }
        b
    };
    if let Some((ts, label)) = pick("functionName").or_else(|| pick("url")) {
        return Some(AnchorHit { ts, kind: AnchorKind::CpuProfile, label });
    }

    let mut best: Option<(f64, String)> = None;
    for e in events {
        let Some(args) = &e.args else { continue };
        if let Some(text) = value_match_text(args, pattern)
            && best.as_ref().is_none_or(|b| e.ts < b.0) {
                best = Some((e.ts, text));
            }
    }
    best.map(|(ts, label)| AnchorHit { ts, kind: AnchorKind::Args, label })
}

fn value_match_text(v: &Value, pattern: &str) -> Option<String> {
    match v {
        Value::String(s) => matches_ignore_case(s, pattern).then(|| s.clone()),
        Value::Array(a) => a.iter().find_map(|v| value_match_text(v, pattern)),
        Value::Object(m) => m.iter().find_map(|(k, v)| {
            if matches_ignore_case(k, pattern) {
                Some(k.clone())
            } else {
                value_match_text(v, pattern)
            }
        }),
        Value::Number(n) => {
            let s = n.to_string();
            matches_ignore_case(&s, pattern).then_some(s)
        }
        _ => None,
    }
}

// ── Frames ──

/// Pair same-tid `b`/`e` events into durations (µs) via a per-tid stack.
/// The window filter is applied to BOTH `b` and `e` events, so an `e`
/// outside the window leaves its `b` unpaired (verified against the binary).
fn paired_durations(events: &[Event], name: &str, window: Option<(f64, f64)>) -> Vec<f64> {
    let mut stacks: HashMap<u64, Vec<f64>> = HashMap::new();
    let mut durs = Vec::new();
    for e in events {
        if e.name != name {
            continue;
        }
        if let Some((lo, hi)) = window
            && (e.ts < lo || e.ts > hi) {
                continue;
            }
        match e.ph.as_str() {
            "b" => stacks.entry(e.tid).or_default().push(e.ts),
            "e" => {
                if let Some(st) = stacks.get_mut(&e.tid)
                    && let Some(t) = st.pop()
                    && e.ts > t {
                        durs.push(e.ts - t);
                    }
            }
            _ => {}
        }
    }
    durs
}

/// Nearest-rank percentiles on a sorted-ascending slice: (p50, p90, p99, max).
fn percentiles(sorted_asc: &[f64]) -> (f64, f64, f64, f64) {
    if sorted_asc.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let p = |q: f64| -> f64 {
        let idx = ((q / 100.0) * (sorted_asc.len() - 1) as f64).round() as usize;
        sorted_asc[idx.min(sorted_asc.len() - 1)]
    };
    (p(50.0), p(90.0), p(99.0), *sorted_asc.last().unwrap())
}

// ── Delta window metrics (PRE / SHOOT / POST rows) ──

#[derive(Debug)]
struct WinStats {
    cpu_us: f64,
    frame_durs: Vec<f64>,
    dropped: usize,
    runtask_us: f64,
    js_us: f64,
    gc_us: f64,
    gc_count: usize,
    lt_count: usize,
    lt_us: f64,
}

fn window_stats(
    events: &[Event],
    scan: &CpuScan,
    win: (f64, f64),
    main_tid: u64,
    lt_us: f64,
) -> WinStats {
    let (lo, hi) = win;
    let cpu_us: f64 = scan
        .samples
        .iter()
        .filter(|s| s.ts >= lo && s.ts <= hi)
        .map(|s| s.weight)
        .sum();
    let mut st = WinStats {
        cpu_us,
        frame_durs: paired_durations(events, FRAME_EVENT, Some(win)),
        dropped: 0,
        runtask_us: 0.0,
        js_us: 0.0,
        gc_us: 0.0,
        gc_count: 0,
        lt_count: 0,
        lt_us: 0.0,
    };
    for e in events {
        if e.ts < lo || e.ts > hi {
            continue;
        }
        if e.ph == "X" && e.tid == main_tid {
            match e.name.as_str() {
                "RunTask" => {
                    if let Some(d) = e.dur {
                        st.runtask_us += d;
                        if d >= lt_us {
                            st.lt_count += 1;
                            st.lt_us += d;
                        }
                    }
                }
                "FunctionCall" => st.js_us += e.dur.unwrap_or(0.0),
                _ => {}
            }
        }
        match e.name.as_str() {
            "MajorGC" | "MinorGC" if e.ph == "X" => {
                st.gc_us += e.dur.unwrap_or(0.0);
                st.gc_count += 1;
            }
            "DroppedFrame" => st.dropped += 1,
            _ => {}
        }
    }
    st
}

// ── GC groups + long tasks (--gc section) ──

#[derive(Debug)]
struct GcStats {
    groups: [usize; 3],
    totals: [f64; 3],
    lt_count: usize,
    lt_total: f64,
}

fn gc_stats(events: &[Event], win: (f64, f64), main_tid: u64, lt_us: f64) -> GcStats {
    let (lo, hi) = win;
    let mut st = GcStats {
        groups: [0; 3],
        totals: [0.0; 3],
        lt_count: 0,
        lt_total: 0.0,
    };
    for e in events {
        if e.ts < lo || e.ts > hi || e.ph != "X" {
            continue;
        }
        let d = e.dur.unwrap_or(0.0);
        let gi = match e.name.as_str() {
            "MajorGC" => Some(0),
            "MinorGC" | "V8.GCScavenger" => Some(1),
            n if n.starts_with("V8.GC") || n.starts_with("CppGC") => Some(2),
            _ => None,
        };
        match gi {
            Some(g) => {
                st.groups[g] += 1;
                st.totals[g] += d;
            }
            None if e.name == "RunTask" && e.tid == main_tid && d >= lt_us => {
                st.lt_count += 1;
                st.lt_total += d;
            }
            None => {}
        }
    }
    st
}

// ── Binary harness + PASS/FAIL helpers ──

fn run_bin(args: &[&str]) -> Value {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run chperf {args:?}: {e}"));
    assert!(
        out.status.success(),
        "chperf {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("chperf {args:?} emitted invalid JSON: {e}"))
}

fn check_eq<T: PartialEq + std::fmt::Display>(what: &str, got: T, want: T) {
    if got == want {
        println!("PASS  {what}: {got}");
    } else {
        panic!("FAIL  {what}: got {got}, want {want}");
    }
}

fn check_eq_debug<T: PartialEq + std::fmt::Debug>(what: &str, got: T, want: T) {
    if got == want {
        println!("PASS  {what}: {got:?}");
    } else {
        panic!("FAIL  {what}: got {got:?}, want {want:?}");
    }
}

fn check_close(what: &str, got: f64, want: f64, tol: f64) {
    if (got - want).abs() <= tol {
        println!("PASS  {what}: got {got}, want {want} (tol {tol})");
    } else {
        panic!("FAIL  {what}: got {got}, want {want} (tol {tol})");
    }
}

fn win_pair(v: &Value) -> (f64, f64) {
    let a = v.as_array().unwrap_or_else(|| panic!("window is not an array"));
    (a[0].as_f64().unwrap(), a[1].as_f64().unwrap())
}

fn metric_val(metrics: &[Value], metric: &str, col: &str) -> f64 {
    metrics
        .iter()
        .find(|r| r["metric"].as_str() == Some(metric))
        .unwrap_or_else(|| panic!("delta output missing metric row {metric:?}"))
        .get(col)
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| panic!("delta metric row {metric:?} missing column {col:?}"))
}

// ── Cross-check one trace against the binary ──

#[derive(Debug)]
struct Verify {
    cpu_total: f64,
    anchor: Option<AnchorHit>,
    pre: (f64, f64),
    shoot: (f64, f64),
    post: (f64, f64),
    pre_stats: WinStats,
    shoot_stats: WinStats,
    post_stats: WinStats,
    full_frames: Vec<f64>,
    full_dropped: usize,
    gc: GcStats,
    main_tid: u64,
}

fn verify_trace(path: &Path, pattern: &str) -> Verify {
    println!("\n=== cross-check {} ===", path.display());
    let events = load_trace(path);
    println!("loaded {} events", events.len());
    let main_tid = detect_main_tid(&events);
    println!("main thread tid = {main_tid}");
    let scan = cpu_scan(&events);
    let cpu_total: f64 = scan.samples.iter().map(|s| s.weight).sum();
    println!(
        "reference CPU total = {:.0} us from {} samples",
        cpu_total,
        scan.samples.len()
    );
    let anchor = find_anchor(&events, &scan, pattern);
    println!(
        "reference anchor = {:?}",
        anchor.as_ref().map(|a| (a.kind.clone(), a.ts, a.label.as_str()))
    );
    let path_s = path.to_str().unwrap_or_else(|| panic!("non-UTF8 path {}", path.display()));

    // Check 1: full-trace CPU total via --calltree --top 1 --json.
    let root1 = run_bin(&[path_s, "--calltree", "--top", "1", "--json"]);
    let ct = root1["sections"]["calltree"]
        .as_array()
        .unwrap_or_else(|| panic!("calltree is not an array"));
    assert!(!ct.is_empty(), "--calltree --top 1 produced no rows on {}", path.display());
    let tool_inc = ct[0]["inclusive_us"]
        .as_f64()
        .expect("calltree row missing inclusive_us");
    check_eq("check1 calltree[0].inclusive_us", tool_inc, cpu_total.round());

    // Check 2: --anchor --delta --frames --gc --json.
    let root2 = run_bin(&[path_s, "--anchor", pattern, "--delta", "--frames", "--gc", "--json"]);
    match root2.get("anchor").and_then(|a| a.as_str()) {
        Some(note) => {
            println!("anchor note: {note}");
            if note.contains("cpu-profile") {
                let a = anchor.as_ref().unwrap_or_else(|| {
                    panic!("tool anchored cpu-profile but the reference found no anchor for {pattern:?}")
                });
                assert!(
                    matches!(a.kind, AnchorKind::CpuProfile),
                    "reference anchor kind {:?} != cpu-profile",
                    a.kind
                );
                let tool_anchor = root2["sections"]["delta"]["anchor_us"]
                    .as_f64()
                    .expect("delta missing anchor_us");
                check_close("check2 delta.anchor_us", tool_anchor, a.ts, 300.0);
            } else {
                println!("SKIP check2 anchor_us: note has no cpu-profile kind");
            }
        }
        None => println!("SKIP check2 anchor_us: no anchor note"),
    }

    let delta = &root2["sections"]["delta"];
    let windows = &delta["windows"];
    let pre = win_pair(&windows["pre"]);
    let shoot = win_pair(&windows["shoot"]);
    let post = win_pair(&windows["post"]);
    println!("windows: pre={pre:?} shoot={shoot:?} post={post:?}");
    let metrics = delta["metrics"].as_array().expect("delta metrics missing");
    let threshold_us = root2["sections"]["gc"][0]["long_tasks"]["threshold_us"]
        .as_f64()
        .expect("gc long_tasks.threshold_us missing");
    println!("long-task threshold = {threshold_us:.0} us");

    let pre_stats = window_stats(&events, &scan, pre, main_tid, threshold_us);
    let shoot_stats = window_stats(&events, &scan, shoot, main_tid, threshold_us);
    let post_stats = window_stats(&events, &scan, post, main_tid, threshold_us);

    let lt_metric = format!("long tasks ≥{:.0}ms", threshold_us / 1000.0);
    for (label, stats) in [
        ("PRE", &pre_stats),
        ("SHOOT", &shoot_stats),
        ("POST", &post_stats),
    ] {
        let col = label.to_lowercase();
        let mut durs = stats.frame_durs.clone();
        durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let (p50, p90, p99, max) = percentiles(&durs);
        check_eq(
            &format!("{label} frames count"),
            metric_val(metrics, "frames", &col),
            durs.len() as f64,
        );
        for (pname, pval) in [("p50", p50), ("p90", p90), ("p99", p99), ("max", max)] {
            let m = format!("frame {pname}");
            check_close(
                &format!("{label} {m}"),
                metric_val(metrics, &m, &col),
                pval / 1000.0,
                0.2,
            );
        }
        check_eq(
            &format!("{label} dropped frames"),
            metric_val(metrics, "dropped frames", &col),
            stats.dropped as f64,
        );
        check_close(
            &format!("{label} main busy (RunTask)"),
            metric_val(metrics, "main busy (RunTask)", &col),
            stats.runtask_us / 1000.0,
            0.2,
        );
        check_close(
            &format!("{label} JS (FunctionCall)"),
            metric_val(metrics, "JS (FunctionCall)", &col),
            stats.js_us / 1000.0,
            0.2,
        );
        check_close(
            &format!("{label} GC (Major+Minor)"),
            metric_val(metrics, "GC (Major+Minor)", &col),
            stats.gc_us / 1000.0,
            0.2,
        );
        check_eq(
            &format!("{label} GC count"),
            metric_val(metrics, "GC count", &col),
            stats.gc_count as f64,
        );
        check_eq(
            &format!("{label} {lt_metric}"),
            metric_val(metrics, &lt_metric, &col),
            stats.lt_count as f64,
        );
        check_close(
            &format!("{label} long task time"),
            metric_val(metrics, "long task time", &col),
            stats.lt_us / 1000.0,
            0.2,
        );
        check_close(
            &format!("{label} CPU samples"),
            metric_val(metrics, "CPU samples", &col),
            stats.cpu_us / 1000.0,
            0.2,
        );
    }

    // Check 3: full-trace frames (no anchor, so the frames section is full
    // scope — with --anchor the frames section would be scoped to SHOOT).
    let root3 = run_bin(&[path_s, "--frames", "--json"]);
    let f0 = &root3["sections"]["frames"][0];
    let mut full_durs = paired_durations(&events, FRAME_EVENT, None);
    full_durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (p50, p90, p99, max) = percentiles(&full_durs);
    let full_dropped = events.iter().filter(|e| e.name == "DroppedFrame").count();
    check_eq("check3 frames count", f0["count"].as_f64().unwrap(), full_durs.len() as f64);
    check_eq("check3 frames p50_us", f0["p50_us"].as_f64().unwrap(), p50.round());
    check_eq("check3 frames p90_us", f0["p90_us"].as_f64().unwrap(), p90.round());
    check_eq("check3 frames p99_us", f0["p99_us"].as_f64().unwrap(), p99.round());
    check_eq("check3 frames max_us", f0["max_us"].as_f64().unwrap(), max.round());
    check_eq("check3 dropped_frames", f0["dropped_frames"].as_f64().unwrap(), full_dropped as f64);

    // GC section from check 2 (scoped to the SHOOT window).
    let gc = gc_stats(&events, shoot, main_tid, threshold_us);
    let g0 = root2["sections"]["gc"][0]["gc"]
        .as_array()
        .expect("gc section missing groups");
    for (i, name) in ["major", "minor", "other"].iter().enumerate() {
        let row = g0[i].as_object().unwrap_or_else(|| panic!("gc group {i} missing"));
        check_eq(
            &format!("check2 gc {name} count"),
            row["count"].as_f64().unwrap(),
            gc.groups[i] as f64,
        );
        check_eq(
            &format!("check2 gc {name} total_us"),
            row["total_us"].as_f64().unwrap(),
            gc.totals[i].round(),
        );
    }
    let lt = &root2["sections"]["gc"][0]["long_tasks"];
    check_eq("check2 long_tasks count", lt["count"].as_f64().unwrap(), gc.lt_count as f64);
    check_eq("check2 long_tasks total_us", lt["total_us"].as_f64().unwrap(), gc.lt_total.round());

    Verify {
        cpu_total,
        anchor,
        pre,
        shoot,
        post,
        pre_stats,
        shoot_stats,
        post_stats,
        full_frames: full_durs,
        full_dropped,
        gc,
        main_tid,
    }
}

// ── Tests ──

/// Always runs against tests/fixtures/medium.json. The fixture is designed
/// so every semantic has a non-trivial, hand-checkable answer (negative
/// timeDelta, b/e frame pairs, GC groups, long tasks, dropped frames, CPU
/// profile anchor). Expected values verified against the binary at commit
/// 2ce4455.
#[test]
fn reference_medium_fixture() {
    let v = verify_trace(Path::new(MEDIUM), "shoot");

    println!("\n=== medium fixture ground truths ===");
    check_eq("fixture CPU total (us)", v.cpu_total, 46_000.0);
    let a = v
        .anchor
        .as_ref()
        .unwrap_or_else(|| panic!("fixture must anchor via cpu-profile"));
    assert!(
        matches!(a.kind, AnchorKind::CpuProfile),
        "fixture anchor kind should be cpu-profile, got {:?}",
        a.kind
    );
    check_eq("fixture anchor ts (us)", a.ts, 1_005_000.0);
    check_eq_debug("fixture PRE window", v.pre, (405_000.0, 905_000.0));
    check_eq_debug("fixture SHOOT window", v.shoot, (905_000.0, 1_105_000.0));
    check_eq_debug("fixture POST window", v.post, (1_105_000.0, 1_605_000.0));

    let s = &v.shoot_stats;
    check_eq("fixture SHOOT CPU samples (ms)", s.cpu_us / 1000.0, 46.0);
    check_eq("fixture SHOOT frames count", s.frame_durs.len() as f64, 3.0);
    check_eq("fixture SHOOT dropped frames", s.dropped as f64, 2.0);
    check_eq("fixture SHOOT long tasks count", s.lt_count as f64, 2.0);
    check_eq("fixture SHOOT main busy (ms)", s.runtask_us / 1000.0, 680.0);
    check_eq("fixture SHOOT JS (ms)", s.js_us / 1000.0, 40.0);
    check_eq("fixture SHOOT GC (ms)", s.gc_us / 1000.0, 6.0);
    check_eq("fixture SHOOT GC count", s.gc_count as f64, 2.0);

    let p = &v.pre_stats;
    check_eq("fixture PRE CPU samples (ms)", p.cpu_us / 1000.0, 0.0);
    let q = &v.post_stats;
    check_eq("fixture POST CPU samples (ms)", q.cpu_us / 1000.0, 0.0);
    check_eq("fixture POST frames count", q.frame_durs.len() as f64, 1.0);
    check_eq("fixture POST GC (ms)", q.gc_us / 1000.0, 2.0);
    check_eq("fixture POST GC count", q.gc_count as f64, 1.0);

    check_eq("fixture full frames count", v.full_frames.len() as f64, 5.0);
    check_eq("fixture full frames max (ms)", v.full_frames.last().unwrap() / 1000.0, 50.0);
    check_eq("fixture full dropped frames", v.full_dropped as f64, 2.0);

    let g = &v.gc;
    check_eq_debug("fixture gc major", (g.groups[0] as f64, g.totals[0]), (1.0, 5_000.0));
    check_eq_debug("fixture gc minor", (g.groups[1] as f64, g.totals[1]), (2.0, 1_500.0));
    check_eq_debug("fixture gc other", (g.groups[2] as f64, g.totals[2]), (0.0, 0.0));
    check_eq_debug("fixture long_tasks", (g.lt_count as f64, g.lt_total), (2.0, 660_000.0));
}

/// Reads CHPERF_TEST_TRACE; skipped (with a message) when unset. Supports
/// plain .json and .json.gz (via flate2).
#[test]
fn reference_real_trace() {
    let Ok(trace) = std::env::var("CHPERF_TEST_TRACE") else {
        eprintln!("SKIP reference_real_trace: CHPERF_TEST_TRACE not set");
        return;
    };
    println!("using CHPERF_TEST_TRACE={trace}");
    let path = Path::new(&trace);
    assert!(path.is_file(), "CHPERF_TEST_TRACE {trace} is not a file");
    let v = verify_trace(path, "shoot");
    println!(
        "\nreal-trace summary: cpu_total_us={:.0}, main_tid={}, anchor={:?}",
        v.cpu_total,
        v.main_tid,
        v.anchor.as_ref().map(|a| (a.ts, a.label.as_str())),
    );
}
