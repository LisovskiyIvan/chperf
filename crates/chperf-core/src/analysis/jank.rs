//! Jank-cluster detection: fixed 50ms buckets where main-thread work
//! exceeds the frame budget.
//!
//! A bucket is hot when any of these hold:
//! - it contains dropped frames, or
//! - any RunTask / FireAnimationFrame / GPUTask event in it lasts at least
//!   the frame budget (detected from vsync cadence, default 16.667ms @60Hz), or
//! - its attributed main-thread busy time reaches 50ms.
//!
//! Busy time is attributed work-to-frame: each task's duration is split
//! across the buckets it overlaps proportionally, so `busy_us` never exceeds
//! the bucket span and a long task smears across the frames it actually
//! blocked. The `max_*` spikes stay with the bucket containing the event's
//! start.

use crate::inspect::Scope;
use crate::trace::{
    TraceEvent, effective_runtask_dur, find_profiler_overhead, is_metadata_event,
    is_profiler_dropped_frame,
};

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

/// Default bucket width (ms). Fixed so clusters are comparable across
/// traces; overridable via `analyze_jank_with_budget_and_bucket`.
pub const DEFAULT_BUCKET_MS: f64 = 50.0;

/// Split a task interval across the buckets it overlaps proportionally to
/// the overlap, so `busy[b]` never exceeds the bucket span. Touches one
/// bucket per overlapped bucket (~dur/bucket_us); total O(tasks + buckets).
fn attribute_busy(busy: &mut [f64], min_ts: f64, bucket_us: f64, start: f64, dur: f64) {
    let n = busy.len();
    if n == 0 || dur <= 0.0 {
        return;
    }
    let end = start + dur;
    let first = (((start - min_ts) / bucket_us) as usize).min(n - 1);
    let last = (((end - min_ts) / bucket_us) as usize).min(n - 1);
    if first == last {
        busy[first] += dur;
        return;
    }
    for (i, slot) in busy[first..=last].iter_mut().enumerate() {
        let lo = min_ts + (first + i) as f64 * bucket_us;
        let overlap = (end.min(lo + bucket_us) - start.max(lo)).max(0.0);
        *slot += overlap;
    }
}

/// A single hot bucket (or any bucket containing dropped frames): dropped
/// frames, any event at/over the frame budget, or ≥50ms attributed busy.
fn bucket_hot(
    busy: f64,
    max_run: f64,
    max_faf: f64,
    max_gpu: f64,
    dropped: u32,
    budget_us: f64,
) -> bool {
    dropped > 0
        || max_run >= budget_us
        || max_faf >= budget_us
        || max_gpu >= budget_us
        || busy >= 50_000.0
}

/// Detect jank clusters: fixed-width buckets where dropped frames,
/// ≥frame-budget spikes (RunTask/FireAnimationFrame/GPUTask) or heavy
/// main-thread busy occurred. Adjacent hot buckets merge into clusters,
/// ranked by dropped frames → worst spike → busy. For each top cluster,
/// the dominating FunctionCalls are collected (the "what happened" chain).
/// This catches spikes that the 50ms Long Task summary misses.
///
/// The frame budget is auto-detected from the trace's vsync cadence (see
/// [`crate::windowed::detect_frame_budget_us`]). For an explicit budget or
/// bucket width use `analyze_jank_with_budget(_and_bucket)`.
pub fn analyze_jank(events: &[TraceEvent], main_tid: u64, scope: Option<&Scope>) -> JankResult {
    analyze_jank_with_budget(
        events,
        main_tid,
        scope,
        crate::windowed::detect_frame_budget_us(events),
    )
}

/// `analyze_jank` with an explicit frame budget (µs): a bucket is hot when
/// any event lasts at least `budget_us`, plus the dropped-frame and 50ms
/// busy rules. Prefer [`crate::windowed::detect_frame_budget_us`] or a
/// `--frame-budget` override for the value.
pub fn analyze_jank_with_budget(
    events: &[TraceEvent],
    main_tid: u64,
    scope: Option<&Scope>,
    budget_us: f64,
) -> JankResult {
    analyze_jank_with_budget_and_bucket(events, main_tid, scope, budget_us, None)
}

/// `analyze_jank_with_budget` with an explicit bucket width override
/// (`Some(ms)`; non-finite or non-positive falls back to
/// [`DEFAULT_BUCKET_MS`]). Buckets are fixed-width from the trace start so
/// results are comparable across traces.
pub fn analyze_jank_with_budget_and_bucket(
    events: &[TraceEvent],
    main_tid: u64,
    scope: Option<&Scope>,
    budget_us: f64,
    bucket_ms: Option<f64>,
) -> JankResult {
    let overheads = find_profiler_overhead(events);
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

    let bucket_ms = bucket_ms
        .filter(|b| b.is_finite() && *b > 0.0)
        .unwrap_or(DEFAULT_BUCKET_MS);
    let bucket_us = (bucket_ms * 1000.0).max(1.0);
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
        // Same rule as `crate::windowed::dropped_frames` / `frames_section`:
        // the full scope (window + tid/pid/cat), not just the window.
        if scope.is_some_and(|s| !s.allows_event(e)) {
            continue;
        }
        let b = (((e.ts - min_ts) / bucket_us) as usize).min(n - 1);
        match e.name {
            "RunTask" if e.ph == b'X' && e.tid == main_tid => {
                if e.dur.is_some() {
                    let eff = effective_runtask_dur(e, &overheads);
                    // Work-to-frame: smear busy across overlapped buckets;
                    // the max spike stays with the start bucket.
                    attribute_busy(&mut busy, min_ts, bucket_us, e.ts, eff);
                    if eff > max_run[b] {
                        max_run[b] = eff;
                    }
                }
            }
            "FireAnimationFrame" if e.ph == b'X' && e.tid == main_tid => {
                if let Some(d) = e.dur
                    && d > max_faf[b]
                {
                    max_faf[b] = d;
                }
            }
            "GPUTask" if e.ph == b'X' => {
                if let Some(d) = e.dur
                    && d > max_gpu[b]
                {
                    max_gpu[b] = d;
                }
            }
            "DroppedFrame" if !is_profiler_dropped_frame(e.ts, &overheads) => {
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
        let hot = bucket_hot(
            busy[b], max_run[b], max_faf[b], max_gpu[b], dropped[b], budget_us,
        );
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

    // Single source of truth for the total: the shared counter. The
    // per-bucket loop above uses the identical predicate, so this equals
    // the bucket sum.
    let total_dropped: usize = crate::windowed::dropped_frames(events, scope);

    // Rank: dropped frames first, then worst spike, then busy.
    accs.sort_by(|a, b| {
        b.dropped
            .cmp(&a.dropped)
            .then_with(|| {
                b.max_run
                    .partial_cmp(&a.max_run)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                b.busy
                    .partial_cmp(&a.busy)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    let top = accs.into_iter().take(8).collect::<Vec<_>>();

    // Mark buckets belonging to top clusters, then collect the dominating
    // FunctionCalls per cluster in one extra pass.
    let mut in_cluster: Vec<Option<usize>> = vec![None; n];
    for (ci, a) in top.iter().enumerate() {
        in_cluster[a.start..=a.end].fill(Some(ci));
    }
    let mut calls: Vec<rustc_hash::FxHashMap<String, f64>> = (0..top.len())
        .map(|_| rustc_hash::FxHashMap::default())
        .collect();
    for e in events {
        if e.name != "FunctionCall" || e.ph != b'X' {
            continue;
        }
        if let (Some(lo), Some(hi)) = (win_lo, win_hi)
            && (e.ts < lo || e.ts > hi)
        {
            continue;
        }
        let b = (((e.ts - min_ts) / bucket_us) as usize).min(n - 1);
        let args = e.args_value();
        if let Some(ci) = in_cluster[b]
            && let Some(d) = e.dur
            && let Some(name) = args
                .as_ref()
                .and_then(|a| a.get("data"))
                .and_then(|d| d.get("functionName"))
                .and_then(|v| v.as_str())
            && !name.is_empty()
        {
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

/// Per-renderer jank: one `analyze_jank` per `ThreadModel` renderer.
/// The full event list is passed each time with that renderer's tid as the
/// main thread (events are NOT pre-filtered, so buckets and the trace window
/// stay global while busy/spike attribution is per-renderer). The budget is
/// detected once (`detect_frame_budget_us`) unless `budget_us` overrides it.
pub fn analyze_jank_per_renderer(
    events: &[TraceEvent],
    model: &crate::trace::ThreadModel,
    budget_us: Option<f64>,
) -> Vec<(crate::trace::RendererThread, JankResult)> {
    let budget = budget_us.unwrap_or_else(|| crate::windowed::detect_frame_budget_us(events));
    model
        .renderers
        .iter()
        .cloned()
        .map(|r| {
            let res = analyze_jank_with_budget(events, r.tid, None, budget);
            (r, res)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &'static str, ph: u8, ts: f64, dur: Option<f64>, tid: u64) -> TraceEvent {
        TraceEvent {
            name,
            id: 0,
            has_id: false,
            ph,
            ts,
            dur,
            tid,
            pid: 1,
            cat: None,
            args: None,
        }
    }

    #[test]
    fn jank_ignores_profiler_startup_task_and_dropped_frames() {
        let events = vec![
            // Profiler startup task with dropped frames during it
            ev("RunTask", b'X', 1_000_000.0, Some(1_079_000.0), 1),
            ev(
                "CpuProfiler::StartProfiling",
                b'X',
                1_001_000.0,
                Some(1_076_000.0),
                1,
            ),
            ev("DroppedFrame", b'I', 1_010_000.0, None, 7),
            ev("DroppedFrame", b'I', 1_030_000.0, None, 7),
            // Trace end marker to give span
            ev("RunTask", b'X', 3_000_000.0, Some(5_000.0), 1),
        ];

        let res = analyze_jank(&events, 1, None);
        // All dropped frames were inside profiler freeze, so total_dropped is 0
        assert_eq!(res.total_dropped, 0);
        // Profiler task effective duration is 3ms (<16.67ms frame budget and <50ms busy), so no hot clusters
        assert!(
            res.clusters.is_empty(),
            "expected no jank clusters, found {}",
            res.clusters.len()
        );
    }

    /// A 10ms task is hot at a 120Hz budget (≥8333µs) but cold at 60Hz.
    #[test]
    fn jank_budget_override_changes_hotness() {
        let events = vec![
            ev("RunTask", b'X', 1_000_000.0, Some(10_000.0), 1),
            ev("RunTask", b'X', 3_000_000.0, Some(1_000.0), 1),
        ];
        let hot = analyze_jank_with_budget(&events, 1, None, 8_333.0);
        assert_eq!(hot.clusters.len(), 1);
        assert!((hot.clusters[0].max_run_us - 10_000.0).abs() < 1e-9);
        let cold = analyze_jank_with_budget(&events, 1, None, 16_667.0);
        assert!(cold.clusters.is_empty());
    }

    /// `analyze_jank` equals `analyze_jank_with_budget` with the detected
    /// budget (no frame ticks here → 60Hz fallback).
    #[test]
    fn jank_default_uses_detected_budget() {
        let events = vec![
            ev("RunTask", b'X', 1_000_000.0, Some(10_000.0), 1),
            ev("RunTask", b'X', 3_000_000.0, Some(1_000.0), 1),
        ];
        let a = analyze_jank(&events, 1, None);
        let b = analyze_jank_with_budget(&events, 1, None, 16_667.0);
        assert_eq!(a.clusters.len(), b.clusters.len());
        assert_eq!(a.total_dropped, b.total_dropped);
        assert_eq!(a.bucket_ms, b.bucket_ms);
    }

    /// A 60ms task straddling a bucket boundary attributes 40ms + 20ms, so
    /// with a huge budget neither bucket reaches the 50ms busy rule —
    /// start-bucket attribution would have marked it hot via 60ms busy.
    #[test]
    fn jank_busy_smeared_across_buckets() {
        let events = vec![
            ev("RunTask", b'X', 1_000_000.0, Some(1_000.0), 1),
            ev("RunTask", b'X', 1_010_000.0, Some(60_000.0), 1),
            ev("RunTask", b'X', 3_000_000.0, Some(1_000.0), 1),
        ];
        let res = analyze_jank_with_budget_and_bucket(&events, 1, None, 1e12, Some(50.0));
        assert!(
            res.clusters.is_empty(),
            "expected smeared busy to stay cold, found {:?}",
            res.clusters.iter().map(|c| c.busy_us).collect::<Vec<_>>()
        );
    }

    /// A 100ms aligned task fills two buckets (50ms + 50ms); both trip the
    /// busy rule and merge into one cluster preserving total busy.
    #[test]
    fn jank_merged_cluster_preserves_total_busy() {
        let events = vec![
            ev("RunTask", b'X', 1_000_000.0, Some(100_000.0), 1),
            ev("RunTask", b'X', 3_000_000.0, Some(1_000.0), 1),
        ];
        let res = analyze_jank_with_budget_and_bucket(&events, 1, None, 1e12, Some(50.0));
        assert_eq!(res.clusters.len(), 1);
        assert!((res.clusters[0].busy_us - 100_000.0).abs() < 1e-6);
    }

    /// Buckets are fixed at 50ms regardless of trace length.
    #[test]
    fn jank_buckets_deterministic_across_spans() {
        let short = vec![
            ev("RunTask", b'X', 1_000_000.0, Some(20_000.0), 1),
            ev("RunTask", b'X', 1_200_000.0, Some(1_000.0), 1),
        ];
        let mut long = short.clone();
        long.push(ev("RunTask", b'X', 30_000_000.0, Some(1_000.0), 1));
        let a = analyze_jank(&short, 1, None);
        let b = analyze_jank(&long, 1, None);
        assert_eq!(a.bucket_ms, 50.0);
        assert_eq!(b.bucket_ms, 50.0);
    }

    /// Jank total dropped uses the shared counter: identical to
    /// `windowed::dropped_frames` on the same trace.
    #[test]
    fn jank_dropped_matches_shared_helper() {
        let events = vec![
            ev("RunTask", b'X', 1_000_000.0, Some(20_000.0), 1),
            ev("DroppedFrame", b'I', 1_005_000.0, None, 7),
            ev("DroppedFrame", b'I', 1_006_000.0, None, 7),
            ev("RunTask", b'X', 3_000_000.0, Some(1_000.0), 1),
        ];
        let res = analyze_jank(&events, 1, None);
        assert_eq!(
            res.total_dropped,
            crate::windowed::dropped_frames(&events, None)
        );
        assert_eq!(res.total_dropped, 2);
    }

    #[test]
    fn per_renderer_attributes_own_thread() {
        use crate::trace::{RendererThread, ThreadModel};
        let model = ThreadModel {
            primary: RendererThread {
                pid: 1,
                tid: 1,
                name: String::new(),
            },
            renderers: vec![
                RendererThread {
                    pid: 1,
                    tid: 1,
                    name: String::new(),
                },
                RendererThread {
                    pid: 2,
                    tid: 2,
                    name: String::new(),
                },
            ],
            browser_main: None,
            compositor: vec![],
        };
        let events = vec![
            ev("RunTask", b'X', 1_000_000.0, Some(100_000.0), 1),
            ev("RunTask", b'X', 1_000_000.0, Some(1_000.0), 2),
            ev("RunTask", b'X', 3_000_000.0, Some(1_000.0), 1),
        ];
        // Huge budget isolates the 50ms busy rule: 100ms of renderer-1 busy
        // fills two buckets (50ms + 50ms, merged) and trips it; 1ms of
        // renderer-2 busy does not.
        let out = analyze_jank_per_renderer(&events, &model, Some(1e12));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0.tid, 1);
        assert_eq!(out[0].1.clusters.len(), 1);
        assert!((out[0].1.clusters[0].busy_us - 100_000.0).abs() < 1e-6);
        assert_eq!(out[1].0.tid, 2);
        assert!(out[1].1.clusters.is_empty());
    }
}
