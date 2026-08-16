//! Jank-cluster detection: spikes below the 50ms Long Task threshold.

use crate::inspect::Scope;
use crate::trace::{TraceEvent, is_metadata_event};

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
    // One pass for both bounds: min_ts skips metadata events (their ts is the
    // process start), max_ts includes everything.
    let mut min_ts = f64::INFINITY;
    let mut max_ts = 0.0f64;
    for e in events {
        if !is_metadata_event(e) && e.ts < min_ts {
            min_ts = e.ts;
        }
        let end = e.ts + e.dur.unwrap_or(0.0);
        if end > max_ts {
            max_ts = end;
        }
    }
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
