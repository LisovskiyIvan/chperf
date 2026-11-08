//! GPU time analysis: `GPUTask` utilization plus compositor frame-pipeline latency.
//!
//! Two independent event families feed one result:
//! - `GPUTask` complete events (`ph` `X`) on the GPU process carry the
//!   GPU-side cost of submitted work; `args.data.renderer_pid` attributes
//!   each task to the renderer that submitted it.
//! - Compositor async pairs (`SubmitCompositorFrameToPresentationCompositorFrame`
//!   with its nested `StartDrawToSwapStart` / `SwapEndToPresentationCompositorFrame`
//!   phases, paired on `(pid, name, id)` where `id` is the parser-normalized
//!   `id`/`id2.local`) measure submit→present frame latency.
//!
//! Traces without a GPU process yield `tasks: None`; traces without the
//! compositor pairs yield `frame_latency: None`.

use crate::trace::{is_metadata_event, TraceEvent};
use rustc_hash::FxHashMap;
use serde::Serialize;

/// Retained longest-task list cap: the 30k-task sweep aggregates into
/// scalars and per-renderer shares, keeping only the top-N tasks.
const TOP_TASKS: usize = 10;

/// Full submit→present pair; nests the two phase pairs below.
const SUBMIT_NAME: &str = "SubmitCompositorFrameToPresentationCompositorFrame";
/// GPU draw phase pair (nested inside submit).
const DRAW_NAME: &str = "StartDrawToSwapStart";
/// Swap→present (vsync wait) phase pair (nested inside submit).
const PRESENT_NAME: &str = "SwapEndToPresentationCompositorFrame";

/// One renderer's share of `GPUTask` busy time.
#[derive(Clone, Serialize)]
pub struct GpuRendererShare {
    /// Renderer that submitted the work (`args.data.renderer_pid`); `None`
    /// when the event carries no attribution.
    pub renderer_pid: Option<u64>,
    pub count: usize,
    pub busy_us: f64,
    /// Share of total `GPUTask` busy time (`busy_us` based).
    pub share_pct: f64,
}

/// One of the longest `GPUTask` events.
#[derive(Clone, Serialize)]
pub struct GpuLongTask {
    pub dur_us: f64,
    /// Offset from trace start (µs), so tables sort without absolute clocks.
    pub ts_offset_us: f64,
    pub renderer_pid: Option<u64>,
}

/// `GPUTask` aggregate: GPU-side cost of submitted compositor work.
#[derive(Clone, Serialize)]
pub struct GpuTasks {
    pub count: usize,
    /// Raw sum of task durations (GPU work is single-threaded per process,
    /// so this matches the busy union on real traces).
    pub busy_us: f64,
    /// `busy_us` as a share of the trace span.
    pub utilization_pct: f64,
    pub median_us: f64,
    pub p95_us: f64,
    pub max_us: f64,
    /// Per-`renderer_pid` breakdown, hottest first.
    pub per_renderer: Vec<GpuRendererShare>,
    /// Longest tasks, worst first (capped at [`TOP_TASKS`]).
    pub longest: Vec<GpuLongTask>,
}

/// Duration aggregate for one frame-pipeline phase.
#[derive(Clone, Serialize)]
pub struct GpuPhaseStats {
    pub count: usize,
    pub median_us: f64,
    pub p95_us: f64,
    pub max_us: f64,
}

/// Compositor submit→present latency plus its draw / present phase split.
///
/// Each phase aggregates independently: a phase with no pairs reports
/// `count: 0` and zero stats rather than dropping the whole section.
#[derive(Clone, Serialize)]
pub struct GpuFrameLatency {
    /// Paired submit→present frames (the `total` phase count).
    pub frame_count: usize,
    /// Full `SubmitCompositorFrameToPresentationCompositorFrame` latency.
    pub total: GpuPhaseStats,
    /// `StartDrawToSwapStart` GPU draw phase.
    pub draw: GpuPhaseStats,
    /// `SwapEndToPresentationCompositorFrame` swap→present phase.
    pub present: GpuPhaseStats,
}

/// GPU analysis result. Either half is `None` when its events are absent
/// (traces without a GPU process or without compositor frame pairs).
#[derive(Clone, Serialize)]
pub struct GpuResult {
    pub tasks: Option<GpuTasks>,
    pub frame_latency: Option<GpuFrameLatency>,
}

/// `args.data.renderer_pid` attribution (`None` when absent or non-numeric).
fn renderer_pid_of(e: &TraceEvent) -> Option<u64> {
    e.args_value()
        .as_ref()
        .and_then(|a| a.get("data"))
        .and_then(|d| d.get("renderer_pid"))
        .and_then(|v| v.as_u64())
}

/// Linear-interpolated percentile over ascending-sorted samples
/// (same rank math as `scroll` / `windowed` percentiles).
fn percentile(sorted_asc: &[f64], p: f64) -> f64 {
    let n = sorted_asc.len();
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return sorted_asc[0];
    }
    let rank = p / 100.0 * (n - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    sorted_asc[lo] + (sorted_asc[hi] - sorted_asc[lo]) * (rank - lo as f64)
}

/// Phase slot for the three paired names, or `None` when untracked.
fn phase_slot(name: &str) -> Option<u8> {
    Some(match name {
        SUBMIT_NAME => 0,
        DRAW_NAME => 1,
        PRESENT_NAME => 2,
        _ => return None,
    })
}

/// Bounded worst-first insert: binary-search into the duration-descending
/// vec, truncated to `TOP_TASKS` — O(`TOP_TASKS`) per task, so the 30k-task
/// sweep never materializes a 30k struct collect.
fn insert_top(top: &mut Vec<GpuLongTask>, task: GpuLongTask) {
    let pos = top.partition_point(|t| t.dur_us > task.dur_us);
    if pos < TOP_TASKS {
        top.insert(pos, task);
        top.truncate(TOP_TASKS);
    }
}

fn phase_stats(durs: &mut Vec<f64>) -> GpuPhaseStats {
    let count = durs.len();
    if durs.is_empty() {
        return GpuPhaseStats {
            count: 0,
            median_us: 0.0,
            p95_us: 0.0,
            max_us: 0.0,
        };
    }
    durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    GpuPhaseStats {
        count,
        median_us: percentile(durs, 50.0),
        p95_us: percentile(durs, 95.0),
        max_us: durs[durs.len() - 1],
    }
}

/// GPU time over the event stream. `_frame_budget_us` is accepted for
/// signature parity with the other `analyze_*` passes (currently unused:
/// GPU busy needs no budget gate).
pub fn analyze_gpu<'a>(
    events: impl IntoIterator<Item = &'a TraceEvent>,
    _frame_budget_us: f64,
) -> GpuResult {
    // GPUTask aggregates (durations double as the percentile scratch vec).
    let mut task_durs: Vec<f64> = Vec::new();
    let mut task_busy_us = 0.0f64;
    let mut task_count = 0usize;
    let mut per_renderer: FxHashMap<Option<u64>, (usize, f64)> = FxHashMap::default();
    let mut longest: Vec<GpuLongTask> = Vec::new();
    // Trace span (metadata-free, like `summary`).
    let mut min_ts = f64::INFINITY;
    let mut max_end = 0.0f64;
    // Pending `b` timestamps keyed by (pid, phase slot, id).
    let mut pending: FxHashMap<(u64, u8, u64), f64> = FxHashMap::default();
    let mut latencies: [Vec<f64>; 3] = [Vec::new(), Vec::new(), Vec::new()];

    for e in events {
        if !is_metadata_event(e) {
            if e.ts < min_ts {
                min_ts = e.ts;
            }
            let end = e.ts + e.dur.unwrap_or(0.0);
            if end > max_end {
                max_end = end;
            }
        }
        if e.name == "GPUTask" && e.ph == b'X' {
            let dur = e.dur.unwrap_or(0.0).max(0.0);
            let pid = renderer_pid_of(e);
            task_count += 1;
            task_busy_us += dur;
            task_durs.push(dur);
            let entry = per_renderer.entry(pid).or_default();
            entry.0 += 1;
            entry.1 += dur;
            insert_top(
                &mut longest,
                GpuLongTask {
                    dur_us: dur,
                    // Offset fixed up below once the trace start is known.
                    ts_offset_us: e.ts,
                    renderer_pid: pid,
                },
            );
            continue;
        }
        // Frame-pipeline pairs: id-bearing `b`/`e` only. A `b` without its
        // `e` (truncated tail) stays pending and is dropped; an `e` without
        // a `b` finds no entry and is ignored.
        if !e.has_id {
            continue;
        }
        let Some(slot) = phase_slot(&e.name) else {
            continue;
        };
        let key = (e.pid, slot, e.id);
        if e.ph == b'b' {
            // Stack-like: a repeated `b` re-arms to the latest submit.
            pending.insert(key, e.ts);
        } else if e.ph == b'e' {
            if let Some(b_ts) = pending.remove(&key) {
                let lat = e.ts - b_ts;
                if lat > 0.0 {
                    latencies[slot as usize].push(lat);
                }
            }
        }
    }

    let trace_span_us = (max_end - min_ts).max(0.0);
    let trace_start = if min_ts.is_finite() { min_ts } else { 0.0 };

    let tasks = if task_count == 0 {
        None
    } else {
        task_durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut shares: Vec<GpuRendererShare> = per_renderer
            .into_iter()
            .map(|(renderer_pid, (count, busy_us))| GpuRendererShare {
                renderer_pid,
                count,
                busy_us,
                share_pct: if task_busy_us > 0.0 {
                    busy_us / task_busy_us * 100.0
                } else {
                    0.0
                },
            })
            .collect();
        // Hottest share first; exact ties break by pid (`None` sorts last).
        shares.sort_by(|a, b| {
            b.busy_us
                .partial_cmp(&a.busy_us)
                .unwrap()
                .then_with(|| a.renderer_pid.cmp(&b.renderer_pid))
        });
        for t in &mut longest {
            t.ts_offset_us -= trace_start;
        }
        Some(GpuTasks {
            count: task_count,
            busy_us: task_busy_us,
            utilization_pct: if trace_span_us > 0.0 {
                task_busy_us / trace_span_us * 100.0
            } else {
                0.0
            },
            median_us: percentile(&task_durs, 50.0),
            p95_us: percentile(&task_durs, 95.0),
            max_us: task_durs[task_durs.len() - 1],
            per_renderer: shares,
            longest,
        })
    };

    let frame_latency = if latencies[0].is_empty() {
        None
    } else {
        let total = phase_stats(&mut latencies[0]);
        let draw = phase_stats(&mut latencies[1]);
        let present = phase_stats(&mut latencies[2]);
        Some(GpuFrameLatency {
            frame_count: total.count,
            total,
            draw,
            present,
        })
    };

    GpuResult {
        tasks,
        frame_latency,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::TraceEvent;

    fn ev_x(
        name: &str,
        ts: f64,
        dur: Option<f64>,
        tid: u64,
        pid: u64,
        args: Option<serde_json::Value>,
    ) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph: b'X',
            ts,
            dur,
            tid,
            pid,
            cat: None,
            args: args.and_then(crate::trace::test_args),
        }
    }

    fn gpu_task(ts: f64, dur: f64, renderer_pid: Option<u64>) -> TraceEvent {
        let args = renderer_pid.map(|p| serde_json::json!({"data": {"renderer_pid": p}}));
        ev_x("GPUTask", ts, Some(dur), 931144, 44368, args)
    }

    fn ev_be(name: &str, ph: u8, ts: f64, pid: u64, tid: u64, id: Option<u64>) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: id.unwrap_or(0),
            has_id: id.is_some(),
            ph,
            ts,
            dur: None,
            tid,
            pid,
            cat: None,
            args: None,
        }
    }

    #[test]
    fn gpu_tasks_two_renderers_with_shares_and_percentiles() {
        // Span: [0, 100_000] via the RunTasks below; GPU busy 3_600us → 3.6%.
        let events = vec![
            ev_x("RunTask", 0.0, Some(1_000.0), 1, 7, None),
            ev_x("RunTask", 90_000.0, Some(10_000.0), 1, 7, None),
            gpu_task(1_000.0, 100.0, Some(73654)),
            gpu_task(2_000.0, 200.0, Some(73654)),
            gpu_task(3_000.0, 300.0, Some(73654)),
            gpu_task(4_000.0, 1_000.0, Some(90001)),
            gpu_task(5_000.0, 2_000.0, Some(90001)),
        ];
        let r = analyze_gpu(events.iter(), 16_667.0);
        let t = r.tasks.expect("GPUTask events present");
        assert_eq!(t.count, 5);
        assert_eq!(t.busy_us, 3_600.0);
        assert!((t.utilization_pct - 3.6).abs() < 1e-9);
        // Sorted durs [100,200,300,1000,2000]: median 300, p95 lerps
        // 1000→2000 at rank 3.8 → 1800, max 2000.
        assert_eq!(t.median_us, 300.0);
        assert!((t.p95_us - 1_800.0).abs() < 1e-9);
        assert_eq!(t.max_us, 2_000.0);
        // Per-renderer: 73654 busy 600 (16.67%), 90001 busy 3000 (83.33%).
        assert_eq!(t.per_renderer.len(), 2);
        assert_eq!(t.per_renderer[0].renderer_pid, Some(90001));
        assert_eq!(t.per_renderer[0].count, 2);
        assert_eq!(t.per_renderer[0].busy_us, 3_000.0);
        assert!((t.per_renderer[0].share_pct - 83.333).abs() < 0.01);
        assert_eq!(t.per_renderer[1].renderer_pid, Some(73654));
        assert!((t.per_renderer[1].share_pct - 16.667).abs() < 0.01);
        // Longest worst-first with trace-start offsets and attribution.
        assert_eq!(t.longest.len(), 5);
        assert_eq!(t.longest[0].dur_us, 2_000.0);
        assert_eq!(t.longest[0].ts_offset_us, 5_000.0);
        assert_eq!(t.longest[0].renderer_pid, Some(90001));
        // No compositor pairs → no frame latency.
        assert!(r.frame_latency.is_none());
    }

    #[test]
    fn gpu_longest_list_is_capped() {
        let events: Vec<TraceEvent> = (0..50)
            .map(|i| gpu_task(i as f64 * 10.0, i as f64 + 1.0, Some(1)))
            .collect();
        let r = analyze_gpu(events.iter(), 16_667.0);
        let t = r.tasks.unwrap();
        assert_eq!(t.count, 50);
        assert_eq!(t.longest.len(), TOP_TASKS);
        assert_eq!(t.longest[0].dur_us, 50.0);
        assert!(t.longest.windows(2).all(|w| w[0].dur_us >= w[1].dur_us));
    }

    #[test]
    fn frame_pairs_nested_dangling_and_orphan() {
        // Two submit frames (20ms, 30ms); frame 1 nests draw (5ms) + present
        // (8ms); frame 2 nests draw only. Plus a dangling `b` (truncated
        // tail) and an orphan `e` — both ignored.
        let events = vec![
            ev_be(SUBMIT_NAME, b'b', 1_000.0, 52760, 989929, Some(0x2)),
            ev_be(DRAW_NAME, b'b', 1_001.0, 52760, 989929, Some(0x2)),
            ev_be(DRAW_NAME, b'e', 6_001.0, 52760, 989929, Some(0x2)),
            ev_be(PRESENT_NAME, b'b', 13_000.0, 52760, 989929, Some(0x2)),
            ev_be(PRESENT_NAME, b'e', 21_000.0, 52760, 989929, Some(0x2)),
            ev_be(SUBMIT_NAME, b'e', 21_000.0, 52760, 989929, Some(0x2)),
            ev_be(SUBMIT_NAME, b'b', 30_000.0, 52760, 989929, Some(0x3)),
            ev_be(DRAW_NAME, b'b', 30_500.0, 52760, 989929, Some(0x3)),
            ev_be(DRAW_NAME, b'e', 40_500.0, 52760, 989929, Some(0x3)),
            ev_be(SUBMIT_NAME, b'e', 60_000.0, 52760, 989929, Some(0x3)),
            // Dangling `b` (no `e`) and orphan `e` (no `b`).
            ev_be(SUBMIT_NAME, b'b', 70_000.0, 52760, 989929, Some(0x4)),
            ev_be(DRAW_NAME, b'e', 75_000.0, 52760, 989929, Some(0x9)),
            // Id-less pair: unpaired by design (pairing needs the id).
            ev_be(SUBMIT_NAME, b'b', 80_000.0, 52760, 989929, None),
            ev_be(SUBMIT_NAME, b'e', 90_000.0, 52760, 989929, None),
        ];
        let r = analyze_gpu(events.iter(), 16_667.0);
        // No GPUTask events → tasks None.
        assert!(r.tasks.is_none());
        let f = r.frame_latency.expect("submit pairs exist");
        assert_eq!(f.frame_count, 2);
        // Two samples: median is the midpoint, p95 lerps toward the max.
        assert_eq!(f.total.median_us, 25_000.0);
        assert_eq!(f.total.max_us, 30_000.0);
        assert_eq!(f.draw.count, 2);
        assert_eq!(f.draw.median_us, 7_500.0);
        assert_eq!(f.present.count, 1);
        assert_eq!(f.present.median_us, 8_000.0);
        assert_eq!(f.present.max_us, 8_000.0);
    }

    #[test]
    fn empty_trace_yields_no_gpu_sections() {
        let r = analyze_gpu(std::iter::empty(), 16_667.0);
        assert!(r.tasks.is_none());
        assert!(r.frame_latency.is_none());
        // Metadata-only trace: no span, no tasks — still None, no panic.
        let meta = vec![TraceEvent {
            name: "process_name".into(),
            id: 0,
            has_id: false,
            ph: b'M',
            ts: 0.0,
            dur: None,
            tid: 0,
            pid: 1,
            cat: Some("metadata".into()),
            args: None,
        }];
        let r = analyze_gpu(meta.iter(), 16_667.0);
        assert!(r.tasks.is_none());
        assert!(r.frame_latency.is_none());
    }
}
