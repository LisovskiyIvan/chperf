//! Whole-trace summary: duration, long tasks, main-thread busy time and a
//! per-event-name breakdown with both inclusive and exclusive (self) time.
//!
//! "Frame-blocking main-thread tasks" is the accurate label for what the
//! scroll analysis reports; see `scroll.rs`.

use crate::trace::{ProfilerOverhead, TraceEvent, find_profiler_overhead, is_metadata_event};

/// All profiler overhead intervals as `(tid, start, end)` spans.
///
/// (Lives in `analysis` rather than `trace::profiler` because that module
/// is not re-exported; this is the helper analysis passes use to ask for
/// the spans. A future `trace` re-export can delegate here.)
///
/// Passes that attribute child-event time (summary, scroll) must correct or
/// exclude children falling inside these spans: during the
/// `CpuProfiler::StartProfiling` freeze the main thread is stalled by
/// instrumentation, so any nested `FunctionCall` / `UpdateLayoutTree` /
/// `Layout` / `Paint` time there is an artifact spike, not app work.
/// Prefer `correct_event_dur` (proportional deduction) over dropping.
pub fn overhead_spans(overheads: &[ProfilerOverhead]) -> Vec<(u64, f64, f64)> {
    overheads
        .iter()
        .map(|ov| (ov.tid, ov.ts, ov.end()))
        .collect()
}

/// Event duration after deducting overlap with profiler overhead spans.
///
/// For `RunTask` this matches the profiler's effective-duration deduction;
/// for any other event it deducts the portion of `[ts, ts+dur)` stalled
/// inside a same-`tid` overhead span (clamped at zero). Events fully inside
/// the freeze correct to `0.0` — callers skip those rather than attribute
/// artifact time.
pub fn correct_event_dur(e: &TraceEvent, overheads: &[ProfilerOverhead]) -> f64 {
    let dur = match e.dur {
        Some(d) => d,
        None => return 0.0,
    };
    if overheads.is_empty() || dur <= 0.0 {
        return dur.max(0.0);
    }
    let end = e.ts + dur;
    let mut overlap = 0.0f64;
    for ov in overheads {
        if ov.tid == e.tid {
            overlap += (end.min(ov.end()) - e.ts.max(ov.ts)).max(0.0);
        }
    }
    (dur - overlap).max(0.0)
}

/// Exclusive (self) time of one event: its overhead-corrected duration minus
/// the union of the corrected durations of the events it contains.
///
/// `children` are the overhead-corrected, parent-clipped intervals of the X
/// events on the same thread contained in the parent. Merging first (rather
/// than summing) keeps nested children (a `Layout` inside an
/// `UpdateLayoutTree` inside a `RunTask`) from being subtracted twice.
fn self_of(dur: f64, children: &mut [(f64, f64)]) -> f64 {
    if children.is_empty() || dur <= 0.0 {
        return dur.max(0.0);
    }
    children.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut covered = 0.0f64;
    let mut cur_s = children[0].0;
    let mut cur_e = children[0].1;
    for &(s, e) in children.iter().skip(1) {
        if s <= cur_e {
            if e > cur_e {
                cur_e = e;
            }
        } else {
            covered += cur_e - cur_s;
            cur_s = s;
            cur_e = e;
        }
    }
    covered += cur_e - cur_s;
    (dur - covered).max(0.0)
}

/// Self time for every event in `sorted` (same-`tid` X events sorted by `ts`).
///
/// `dur_of[i]` is the overhead-corrected duration of `sorted[i]`. Returns the
/// self time per index. Children are events whose raw interval is contained
/// in the parent's raw interval (identical intervals break ties by index so
/// they are not subtracted both ways); subtraction uses corrected durations
/// clipped to the parent's corrected end, merged as a union.
fn compute_self_times(sorted: &[&TraceEvent], dur_of: &[f64]) -> Vec<f64> {
    let n = sorted.len();
    let mut out = dur_of.to_vec();
    // Parents sort before their children (ts asc, dur desc), so every
    // potential child of `sorted[i]` comes after `i`; the scan stops at the
    // first event starting at/after the parent's end.
    for i in 0..n {
        let p = sorted[i];
        let p_dur = match p.dur {
            Some(d) => d,
            None => continue,
        };
        if p_dur <= 0.0 {
            out[i] = 0.0;
            continue;
        }
        let p_end = p.ts + p_dur;
        let p_eff_end = p.ts + dur_of[i];
        let mut children: Vec<(f64, f64)> = Vec::new();
        let mut j = i + 1;
        while j < n && sorted[j].ts < p_end {
            let c = sorted[j];
            if c.ph == b'X'
                && let Some(cd) = c.dur
                && cd > 0.0
            {
                let c_end = c.ts + cd;
                // Contained (or equal but ordered after the parent).
                if c.ts >= p.ts && (c_end < p_end || (c_end == p_end && j > i)) && c_end <= p_end {
                    let cs = c.ts.max(p.ts);
                    let ce = (c.ts + dur_of[j]).min(p_eff_end);
                    if ce > cs {
                        children.push((cs, ce));
                    }
                }
            }
            j += 1;
        }
        out[i] = self_of(dur_of[i], &mut children);
    }
    out
}

/// Per-event self times for main-thread X events, keyed by event index.
///
/// Shared helper so `summary` and `scroll` bucket overlap-safe self time the
/// same way. `dur_of` maps `events` indices to overhead-corrected durations.
/// Events that are not main-thread X events with a duration get `0.0`.
pub fn main_thread_self_times(events: &[TraceEvent], main_tid: u64, dur_of: &[f64]) -> Vec<f64> {
    let mut sorted_idx: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.tid == main_tid && e.ph == b'X' && e.dur.is_some())
        .map(|(i, _)| i)
        .collect();
    // Parents before children: ts asc, longer first on ties.
    sorted_idx.sort_by(|&a, &b| {
        events[a]
            .ts
            .partial_cmp(&events[b].ts)
            .unwrap()
            .then_with(|| {
                events[b]
                    .dur
                    .unwrap_or(0.0)
                    .partial_cmp(&events[a].dur.unwrap_or(0.0))
                    .unwrap()
            })
    });
    let sorted: Vec<&TraceEvent> = sorted_idx.iter().map(|&i| &events[i]).collect();
    let sorted_dur: Vec<f64> = sorted_idx.iter().map(|&i| dur_of[i]).collect();
    let sorted_self = compute_self_times(&sorted, &sorted_dur);
    let mut out = vec![0.0f64; events.len()];
    for (k, &i) in sorted_idx.iter().enumerate() {
        out[i] = sorted_self[k];
    }
    out
}

#[derive(Clone)]
pub struct EventTypeStat {
    pub name: &'static str,
    /// Inclusive (wall-clock) time: sum of overhead-corrected durations.
    /// Sibling categories overlap (a `Layout` nested in a `RunTask` counts
    /// toward both), so per-category shares may sum past 100% — that is
    /// expected for inclusive accounting, not a bug.
    pub total_time_us: f64,
    /// Exclusive (self) time: inclusive time minus the union of contained
    /// child intervals, so nesting never double counts. Shares computed
    /// from this column do sum sanely against the trace window.
    pub self_time_us: f64,
    pub count: usize,
    /// Mean of the same overhead-corrected inclusive durations summed in
    /// `total_time_us` (`total / count`): for `RunTask` this is the mean
    /// *effective* (profiler-corrected) duration, for every other event it
    /// is the mean raw duration corrected the same way. Use `avg_self_us`
    /// for the mean exclusive time.
    pub avg_time_us: f64,
    /// Mean of the same overhead-corrected exclusive durations summed in
    /// `self_time_us` (`self_total / count`).
    pub avg_self_us: f64,
    /// Inclusive share of the trace window (`total_time_us` based); may
    /// exceed 100% when summed across categories — see `total_time_us`.
    /// For an overlap-safe share use `self_pct_of_trace`.
    pub pct_of_trace: f64, // percentage of total trace time
    /// Overlap-safe share of the trace window (`self_time_us` based).
    pub self_pct_of_trace: f64,
}

#[derive(Clone)]
pub struct SummaryResult {
    pub long_task_count: usize,
    pub long_tasks_top: Vec<f64>,
    pub total_trace_duration_us: f64,
    /// Honest main-thread busy time: the UNION of effective RunTask intervals
    /// (sweep-merged, O(n log n)), so overlapping/synthetic tasks never report
    /// more than the trace span. For the raw sum see `runtask_total_us`.
    pub main_thread_busy_us: f64,
    /// Raw sum of effective RunTask durations (may exceed the trace span when
    /// tasks overlap; kept for accounting against the union above).
    pub runtask_total_us: f64,
    pub event_stats: Vec<EventTypeStat>,
    pub total_blocking_time_us: f64, // sum of (dur - 50ms) for main thread RunTasks > 50ms (TBT)
    pub long_tasks_total_us: f64,    // total duration of all long tasks on main thread
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
    analyze_summary_multi(events, &[main_tid])
        .into_iter()
        .next()
        .expect("one tid in, one result out")
}

/// Per-tid accumulators for one summary pass (see `analyze_summary_multi`).
#[derive(Default)]
struct SummaryTidAccum {
    long_task_durs: Vec<f64>,
    total_blocking_time_us: f64,
    long_tasks_total_us: f64,
    stats_map: rustc_hash::FxHashMap<&'static str, (f64, f64, usize)>,
    runtask_total_us: f64,
    busy_intervals: Vec<(f64, f64)>,
}

/// One `analyze_summary` per tid, sharing the profiler-overhead scan and
/// the trace-bounds pass across tids. Per-tid durations, self times and
/// accumulators are computed tid-by-tid (not fused into one loop) so peak
/// transient stays one `dur_of`/`self` pair, and every per-tid accumulation
/// visits events in the same order as `analyze_summary` — results are
/// identical, including float summation order and map iteration order.
pub fn analyze_summary_multi(events: &[TraceEvent], main_tids: &[u64]) -> Vec<SummaryResult> {
    let overheads = find_profiler_overhead(events);
    // Trace bounds are tid-independent: one shared pass (metadata excluded,
    // exactly like the single-tid loop below).
    let mut min_ts = f64::INFINITY;
    let mut max_ts = 0.0f64;
    for e in events {
        if is_metadata_event(e) {
            continue;
        }
        let end = e.ts + e.dur.unwrap_or(0.0);
        if e.ts < min_ts {
            min_ts = e.ts;
        }
        if end > max_ts {
            max_ts = end;
        }
    }
    let total_trace_duration_us = (max_ts - min_ts).max(0.0);

    main_tids
        .iter()
        .map(|&main_tid| {
            let dur_of: Vec<f64> = events
                .iter()
                .map(|e| {
                    if e.tid == main_tid && e.ph == b'X' {
                        correct_event_dur(e, &overheads)
                    } else {
                        0.0
                    }
                })
                .collect();
            let self_of_all = main_thread_self_times(events, main_tid, &dur_of);
            let mut acc = SummaryTidAccum::default();
            for (idx, e) in events.iter().enumerate() {
                if e.tid != main_tid || e.ph != b'X' {
                    continue;
                }
                if e.name == "RunTask" && e.dur.is_some() {
                    let eff = dur_of[idx];
                    acc.runtask_total_us += eff;
                    if eff > 0.0 {
                        acc.busy_intervals.push((e.ts, e.ts + eff));
                    }
                    if eff > 50_000.0 {
                        acc.long_task_durs.push(eff);
                        acc.total_blocking_time_us += eff - 50_000.0;
                        acc.long_tasks_total_us += eff;
                    }
                }
                if let Some(key) = target_key(&e.name)
                    && e.dur.is_some()
                {
                    let entry = acc.stats_map.entry(key).or_default();
                    entry.0 += dur_of[idx];
                    entry.1 += self_of_all[idx];
                    entry.2 += 1;
                }
            }
            finish_summary(acc, total_trace_duration_us)
        })
        .collect()
}
/// Finish one tid's summary: longest-task ranking, busy union, per-name
/// stats. Split from the accumulation loop so single- and multi-tid paths
/// share it exactly.
fn finish_summary(
    acc: SummaryTidAccum,
    total_trace_duration_us: f64,
) -> SummaryResult {
    let SummaryTidAccum {
        mut long_task_durs,
        total_blocking_time_us,
        long_tasks_total_us,
        stats_map,
        runtask_total_us,
        mut busy_intervals,
    } = acc;
    long_task_durs.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let long_task_count = long_task_durs.len();
    let long_tasks_top: Vec<f64> = long_task_durs.into_iter().take(10).collect();

    // Honest busy: union of effective RunTask intervals (sweep-merge,
    // O(n log n) via the sort; the linear merge keeps nesting/overlap from
    // reporting more than the trace span.
    busy_intervals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut union_busy_us = 0.0f64;
    let mut cur_s = f64::INFINITY;
    let mut cur_e = f64::NEG_INFINITY;
    for (s, e) in busy_intervals {
        if s <= cur_e {
            if e > cur_e {
                cur_e = e;
            }
        } else {
            if cur_e > cur_s {
                union_busy_us += cur_e - cur_s;
            }
            cur_s = s;
            cur_e = e;
        }
    }
    if cur_e > cur_s {
        union_busy_us += cur_e - cur_s;
    }

    let mut event_stats: Vec<EventTypeStat> = stats_map
        .into_iter()
        .map(|(name, (total, self_total, count))| EventTypeStat {
            name,
            total_time_us: total,
            self_time_us: self_total,
            count,
            avg_time_us: if count > 0 { total / count as f64 } else { 0.0 },
            avg_self_us: if count > 0 {
                self_total / count as f64
            } else {
                0.0
            },
            pct_of_trace: if total_trace_duration_us > 0.0 {
                total / total_trace_duration_us * 100.0
            } else {
                0.0
            },
            self_pct_of_trace: if total_trace_duration_us > 0.0 {
                self_total / total_trace_duration_us * 100.0
            } else {
                0.0
            },
        })
        .collect();
    // Total desc; exact ties break by name asc (map order is hash-dependent).
    event_stats.sort_by(|a, b| {
        b.total_time_us
            .partial_cmp(&a.total_time_us)
            .unwrap()
            .then_with(|| a.name.cmp(&b.name))
    });

    SummaryResult {
        long_task_count,
        long_tasks_top,
        total_trace_duration_us,
        main_thread_busy_us: union_busy_us,
        runtask_total_us,
        event_stats,
        total_blocking_time_us,
        long_tasks_total_us,
    }
}

/// Per-renderer summaries: one summary per `ThreadModel` renderer, sharing
/// the overhead scan and bounds pass across tids (see
/// `analyze_summary_multi`).
/// The full event list is passed each time with that renderer's tid as the
/// main thread (events are NOT pre-filtered, so the trace window stays
/// global while busy/long-task attribution is per-renderer). Tids are
/// assumed unique across pids, as in `detect_main_thread`.
pub fn analyze_summary_per_renderer(
    events: &[TraceEvent],
    model: &crate::trace::ThreadModel,
) -> Vec<(crate::trace::RendererThread, SummaryResult)> {
    let tids: Vec<u64> = model.renderers.iter().map(|r| r.tid).collect();
    let results = analyze_summary_multi(events, &tids);
    model
        .renderers
        .iter()
        .cloned()
        .zip(results)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str, ph: u8, ts: f64, dur: Option<f64>, tid: u64) -> TraceEvent {
        TraceEvent {
            name: name.into(),
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
    fn summary_excludes_cpu_profiler_start_from_tbt_and_long_tasks() {
        // RunTask of 1079ms contains CpuProfiler::StartProfiling of 1076ms.
        // Effective duration is 3ms -> NOT a long task, 0 TBT, 3ms busy time.
        let events = vec![
            ev("RunTask", b'X', 1_000_000.0, Some(1_079_000.0), 1),
            ev(
                "CpuProfiler::StartProfiling",
                b'X',
                1_001_000.0,
                Some(1_076_000.0),
                1,
            ),
            ev("RunTask", b'X', 3_000_000.0, Some(80_000.0), 1), // Real 80ms long task
        ];

        let res = analyze_summary(&events, 1);
        assert_eq!(res.long_task_count, 1);
        assert_eq!(res.long_tasks_top, vec![80_000.0]);
        assert_eq!(res.long_tasks_total_us, 80_000.0);
        // TBT: (80ms - 50ms) = 30ms. Profiler task contributed 0ms TBT.
        assert_eq!(res.total_blocking_time_us, 30_000.0);
        // Main thread busy: 3ms (profiler task effective) + 80ms = 83ms.
        assert_eq!(res.main_thread_busy_us, 83_000.0);
    }

    #[test]
    fn event_stats_carry_overlap_safe_self_time() {
        // RunTask 100ms > ULT 60ms > Layout 50ms. Inclusive double counts
        // the nesting (100 + 60 + 50 = 210 for 100ms of work); self time
        // must attribute each slice once (40 + 10 + 50 = 100).
        let events = vec![
            ev("RunTask", b'X', 0.0, Some(100_000.0), 1),
            ev("UpdateLayoutTree", b'X', 10_000.0, Some(60_000.0), 1),
            ev("Layout", b'X', 15_000.0, Some(50_000.0), 1),
        ];
        let res = analyze_summary(&events, 1);
        let by_name = |n: &str| res.event_stats.iter().find(|s| s.name == n).unwrap();
        assert_eq!(by_name("RunTask").total_time_us, 100_000.0);
        assert_eq!(by_name("RunTask").self_time_us, 40_000.0);
        assert_eq!(by_name("UpdateLayoutTree").total_time_us, 60_000.0);
        assert_eq!(by_name("UpdateLayoutTree").self_time_us, 10_000.0);
        assert_eq!(by_name("Layout").total_time_us, 50_000.0);
        assert_eq!(by_name("Layout").self_time_us, 50_000.0);
        // avg_time_us is the mean of the same corrected inclusive durations
        // as total_time_us (not raw), and avg_self_us the mean of self.
        for n in ["RunTask", "UpdateLayoutTree", "Layout"] {
            let s = by_name(n);
            assert_eq!(s.avg_time_us, s.total_time_us / s.count as f64);
            assert_eq!(s.avg_self_us, s.self_time_us / s.count as f64);
        }
        assert_eq!(by_name("RunTask").avg_time_us, 100_000.0);
        assert_eq!(by_name("RunTask").avg_self_us, 40_000.0);
        // Self shares sum sanely against the trace window; inclusive shares
        // document that they may exceed 100%.
        assert!(by_name("RunTask").pct_of_trace > 0.0);
        assert_eq!(by_name("Layout").self_pct_of_trace, 50.0);
    }

    #[test]
    fn self_time_child_equal_to_parent_counts_once() {
        // Identical intervals: the first in sort order parents the second.
        // Exactly one of them keeps the time; the pair sums to 100ms.
        let events = vec![
            ev("RunTask", b'X', 0.0, Some(100_000.0), 1),
            ev("FunctionCall", b'X', 0.0, Some(100_000.0), 1),
        ];
        let res = analyze_summary(&events, 1);
        let by_name = |n: &str| res.event_stats.iter().find(|s| s.name == n).unwrap();
        assert_eq!(by_name("RunTask").self_time_us, 0.0);
        assert_eq!(by_name("FunctionCall").self_time_us, 100_000.0);
    }

    #[test]
    fn self_time_overlapping_children_merge_not_sum() {
        // Siblings [10,60] and [40,90]: union is [10,90] = 80ms, so the
        // parent keeps 20ms. Summing would wrongly leave 0ms.
        let events = vec![
            ev("RunTask", b'X', 0.0, Some(100_000.0), 1),
            ev("FunctionCall", b'X', 10_000.0, Some(50_000.0), 1),
            ev("EvaluateScript", b'X', 40_000.0, Some(50_000.0), 1),
        ];
        let res = analyze_summary(&events, 1);
        let by_name = |n: &str| res.event_stats.iter().find(|s| s.name == n).unwrap();
        assert_eq!(by_name("RunTask").self_time_us, 20_000.0);
    }

    #[test]
    fn self_time_overlapping_sibling_and_grandchild() {
        // ULT [10,70] with nested Layout [20,60], overlapped by Paint
        // [50,90]: parent union is [10,90] = 80ms (grandchild inside ULT
        // adds no new coverage), ULT keeps 60 - 40 = 20ms.
        let events = vec![
            ev("RunTask", b'X', 0.0, Some(100_000.0), 1),
            ev("UpdateLayoutTree", b'X', 10_000.0, Some(60_000.0), 1),
            ev("Layout", b'X', 20_000.0, Some(40_000.0), 1),
            ev("Paint", b'X', 50_000.0, Some(40_000.0), 1),
        ];
        let res = analyze_summary(&events, 1);
        let by_name = |n: &str| res.event_stats.iter().find(|s| s.name == n).unwrap();
        assert_eq!(by_name("RunTask").self_time_us, 20_000.0);
        assert_eq!(by_name("UpdateLayoutTree").self_time_us, 20_000.0);
        assert_eq!(by_name("Layout").self_time_us, 40_000.0);
        assert_eq!(by_name("Paint").self_time_us, 40_000.0);
    }

    #[test]
    fn self_time_boundary_child_belongs_to_next_parent() {
        // A child starting exactly at the parent's end is not contained;
        // back-to-back parents sharing a boundary don't steal each other.
        let events = vec![
            ev("RunTask", b'X', 0.0, Some(50_000.0), 1),
            ev("RunTask", b'X', 50_000.0, Some(50_000.0), 1),
            ev("FunctionCall", b'X', 50_000.0, Some(40_000.0), 1),
        ];
        let res = analyze_summary(&events, 1);
        let by_name = |n: &str| res.event_stats.iter().find(|s| s.name == n).unwrap();
        assert_eq!(by_name("RunTask").self_time_us, 60_000.0);
        assert_eq!(by_name("FunctionCall").self_time_us, 40_000.0);
    }

    #[test]
    fn self_time_events_without_dur_are_zero() {
        // Duration-less X events (instant markers with ph X but no dur)
        // contribute nothing and must not break the sweep.
        let events = vec![
            ev("RunTask", b'X', 0.0, Some(100_000.0), 1),
            ev("FunctionCall", b'X', 10_000.0, None, 1),
            ev("Layout", b'X', 20_000.0, Some(30_000.0), 1),
        ];
        let res = analyze_summary(&events, 1);
        let by_name = |n: &str| res.event_stats.iter().find(|s| s.name == n).unwrap();
        assert_eq!(by_name("RunTask").self_time_us, 70_000.0);
        assert_eq!(by_name("Layout").self_time_us, 30_000.0);
        // The dur-less FunctionCall is not tracked at all.
        assert!(res.event_stats.iter().all(|s| s.name != "FunctionCall"));
        let dur_of = vec![100_000.0, 0.0, 30_000.0];
        let selfs = main_thread_self_times(&events, 1, &dur_of);
        assert_eq!(selfs, vec![70_000.0, 0.0, 30_000.0]);
    }

    #[test]
    fn busy_time_is_union_not_sum() {
        // Overlapping 100ms tasks [0,100] + [50,150]: raw sum is 200ms but
        // honest busy is the 150ms union — never past the trace span.
        let events = vec![
            ev("RunTask", b'X', 0.0, Some(100_000.0), 1),
            ev("RunTask", b'X', 50_000.0, Some(100_000.0), 1),
        ];
        let res = analyze_summary(&events, 1);
        assert_eq!(res.runtask_total_us, 200_000.0);
        assert_eq!(res.main_thread_busy_us, 150_000.0);
        // Nested tasks collapse to the outer interval.
        let nested = vec![
            ev("RunTask", b'X', 0.0, Some(100_000.0), 1),
            ev("RunTask", b'X', 10_000.0, Some(10_000.0), 1),
        ];
        let res = analyze_summary(&nested, 1);
        assert_eq!(res.runtask_total_us, 110_000.0);
        assert_eq!(res.main_thread_busy_us, 100_000.0);
        // Disjoint tasks: union equals the sum (old behavior preserved).
        let apart = vec![
            ev("RunTask", b'X', 0.0, Some(100_000.0), 1),
            ev("RunTask", b'X', 200_000.0, Some(50_000.0), 1),
        ];
        let res = analyze_summary(&apart, 1);
        assert_eq!(res.runtask_total_us, 150_000.0);
        assert_eq!(res.main_thread_busy_us, 150_000.0);
    }

    #[test]
    fn per_renderer_attributes_own_thread() {
        use crate::trace::{RendererThread, ThreadModel};
        let model = ThreadModel {
            primary: RendererThread {
                pid: 1,
                tid: 1,
                name: "CrRendererMain".to_string(),
            },
            renderers: vec![
                RendererThread {
                    pid: 1,
                    tid: 1,
                    name: "CrRendererMain".to_string(),
                },
                RendererThread {
                    pid: 2,
                    tid: 2,
                    name: "Renderer".to_string(),
                },
            ],
            browser_main: None,
            compositor: vec![],
        };
        let events = vec![
            ev("RunTask", b'X', 0.0, Some(60_000.0), 1),
            ev("RunTask", b'X', 0.0, Some(10_000.0), 2),
        ];
        let out = analyze_summary_per_renderer(&events, &model);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0.tid, 1);
        assert_eq!(out[0].1.main_thread_busy_us, 60_000.0);
        assert_eq!(out[0].1.long_task_count, 1);
        assert_eq!(out[1].0.tid, 2);
        assert_eq!(out[1].1.main_thread_busy_us, 10_000.0);
        assert_eq!(out[1].1.long_task_count, 0);
        // Trace window stays global in both.
        assert_eq!(
            out[0].1.total_trace_duration_us,
            out[1].1.total_trace_duration_us
        );
    }
}
