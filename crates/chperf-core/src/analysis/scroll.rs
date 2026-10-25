//! Frame-blocking main-thread task analysis: `RunTask`s whose children
//! include heavy rendering work, broken down by overlap-safe cost category.
//!
//! NOTE on naming: despite the historical `ScrollFrame*` identifiers (kept
//! for API compatibility), this reports frame-blocking main-thread tasks,
//! not proven scroll-driven frames. Tasks are only treated as scroll-linked
//! when the trace carries scroll/input evidence (see `is_scroll_input`); when
//! the trace has no input events at all the old workload heuristic is used
//! and `ScrollFrameResult::heuristic_fallback` is set so callers can label
//! the output honestly.

use super::summary::correct_event_dur;
use crate::trace::{find_profiler_overhead, TraceEvent};

/// Case-insensitive ASCII substring search: SIMD first-byte scan (`memchr2`
/// over both cases), then an exact compare at each candidate. All scroll
/// markers are lowercase ASCII, so this agrees with
/// `to_lowercase().contains` on real traces with no per-event allocation.
fn contains_ci(hay: &str, needle: &[u8]) -> bool {
    let h = hay.as_bytes();
    if h.len() < needle.len() {
        return false;
    }
    let lo = needle[0].to_ascii_lowercase();
    let hi = needle[0].to_ascii_uppercase();
    for i in memchr::memchr2_iter(lo, hi, h) {
        if h.len() - i >= needle.len() && h[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            return true;
        }
    }
    false
}

/// One frame budget at 60fps. Sub-tasks longer than this can drop a frame,
/// so this — not the 50ms long-task threshold — is the inclusion gate.
/// Tasks containing a >50ms child are additionally flagged via
/// `FrameTask::over_50ms` (the old long-task subset).
pub const FRAME_BUDGET_US: f64 = 16_667.0;
/// Classic long-task threshold, kept only to mark the >50ms subset.
pub const LONG_TASK_US: f64 = 50_000.0;

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
    /// True when the task contains a >50ms (`LONG_TASK_US`) sub-event: the
    /// old long-task subset, kept so >50ms jank stays distinguishable now
    /// that the inclusion gate is the frame budget.
    pub over_50ms: bool,
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

    /// Returns breakdown as (label, value) sorted by value desc
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
    /// True when the trace contained no scroll/input events anywhere, so
    /// task selection fell back to the workload heuristic (heavy child with
    /// no proof of scroll origin). Callers should label the output
    /// "frame-blocking main-thread tasks" in that case, never "scroll".
    pub heuristic_fallback: bool,
}

/// True when `e` is scroll/input evidence: a scroll-ish input event or an
/// `EventDispatch`/`InputLatency` carrying a scroll-ish `args.data.type`
/// (`"scroll"`, `"GestureScrollUpdate"`, `"GestureScrollBegin/End"`,
/// `"MouseWheel"`, all matched case-insensitively).
fn is_scroll_input(e: &TraceEvent) -> bool {
    // All patterns are lowercase ASCII; `contains_ci` agrees with the old
    // `to_lowercase().contains` on real traces with no per-event allocation.
    // Every gesture pattern contains "scroll", so check that first.
    let name = &e.name;
    // A scroll-named event counts even without parseable args, so no args
    // scan here: the result is `true` regardless.
    if contains_ci(name, b"scroll") {
        return true;
    }
    if contains_ci(name, b"mousewheel") {
        return true;
    }
    if name == "EventDispatch" || name == "InputLatency" {
        if let Some(raw) = e.args_raw() {
            if contains_ci(raw, b"scroll") || contains_ci(raw, b"mousewheel") {
                return true;
            }
        }
    }
    false
}

/// Category bucket for the breakdown, or `None` when the event is not one
/// of the tracked rendering cost categories.
fn bucket_of(name: &str) -> Option<usize> {
    Some(match name {
        "FunctionCall" => 0,        // js
        "UpdateLayoutTree" => 1,    // ult
        "Paint" => 2,               // paint
        "Layerize" | "Commit" => 3, // composite
        "HitTest" => 4,             // hit test
        "Layout" => 5,              // layout
        _ => return None,
    })
}

pub fn analyze_scroll_frames(events: &[TraceEvent], main_tid: u64) -> ScrollFrameResult {
    let overheads = find_profiler_overhead(events);
    // One fused pass: main-thread X indices (+ their corrected durations)
    // and scroll/input timestamps. Three separate filters here would read
    // the whole events vec three times (~450MB on big traces).
    let mut main_x: Vec<usize> = Vec::with_capacity(events.len() / 8);
    let mut input_ts: Vec<f64> = Vec::with_capacity(4096);
    // Overhead-corrected duration per event index (children inside a
    // profiler-freeze span correct toward 0 so the artifact spike lands in
    // no category).
    let mut dur_of: Vec<f64> = vec![0.0f64; events.len()];
    for (i, e) in events.iter().enumerate() {
        if e.tid == main_tid && e.ph == b'X' {
            main_x.push(i);
            dur_of[i] = correct_event_dur(e, &overheads);
        }
        if is_scroll_input(e) {
            input_ts.push(e.ts);
        }
    }
    main_x.sort_by(|&a, &b| events[a].ts.partial_cmp(&events[b].ts).unwrap());

    let run_tasks: Vec<usize> = main_x
        .iter()
        .copied()
        .filter(|&i| events[i].name == "RunTask" && events[i].dur.is_some())
        .collect();

    // Scroll/input evidence anywhere in the trace decides the gate: with
    // evidence, a task must overlap one; without, fall back to the workload
    // heuristic and say so (`heuristic_fallback`).
    input_ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let trace_has_input = !input_ts.is_empty();
    let task_has_input = |lo: f64, hi: f64| -> bool {
        let start = input_ts.partition_point(|&t| t < lo);
        input_ts.get(start).is_some_and(|&t| t <= hi)
    };

    let mut tasks = Vec::new();

    // Single sweep: RunTasks on a thread are serial and non-nested, so each
    // main-thread event falls inside at most one task. `lo` only moves forward.
    let mut lo = 0usize;
    for &rt in &run_tasks {
        let rt_ts = events[rt].ts;
        let rt_dur = dur_of[rt];
        let rt_end_raw = rt_ts + events[rt].dur.unwrap_or(0.0);
        let rt_end = rt_ts + rt_dur;

        while lo < main_x.len() && events[main_x[lo]].ts < rt_ts {
            lo += 1;
        }
        // Children: main-thread X events overlapping the task, clipped to
        // the task interval (straddlers are clipped, not dropped). A child
        // starting exactly at the task end overlaps nothing, so back-to-back
        // RunTasks sharing a boundary still attribute it to the next task.
        let mut kids: Vec<usize> = Vec::new();
        let mut j = lo;
        while j < main_x.len() && events[main_x[j]].ts < rt_end_raw.max(rt_end) {
            let i = main_x[j];
            if i != rt && events[i].ph == b'X' {
                let d = events[i].dur.unwrap_or(0.0);
                if d > 0.0 {
                    let s = events[i].ts.max(rt_ts);
                    let e = (events[i].ts + d).min(rt_end_raw);
                    if e > s {
                        kids.push(i);
                    }
                }
            }
            j += 1;
        }

        // Inclusion gate: a tracked-category child over the frame budget.
        // With input evidence in the trace, the task must additionally
        // overlap scroll/input evidence (itself or any overlapping event).
        // Gates use overhead-corrected durations so profiler-freeze
        // artifacts can't promote a task.
        let mut has_budget_child = false;
        let mut has_heavy_child = false;
        let mut has_input = false;
        for &i in &kids {
            let e = &events[i];
            if is_scroll_input(e) {
                has_input = true;
            }
            if bucket_of(&e.name).is_some() {
                let d = dur_of[i];
                if d > FRAME_BUDGET_US {
                    has_budget_child = true;
                }
                if d > LONG_TASK_US {
                    has_heavy_child = true;
                }
            }
        }
        // Input events on other threads still gate the task when their
        // timestamp falls inside the task interval (precomputed, sorted).
        if !has_input {
            has_input = task_has_input(rt_ts, rt_end_raw);
        }
        if !has_budget_child {
            continue;
        }
        if trace_has_input && !has_input {
            continue;
        }

        // Self-time bucketing: each child's corrected slice minus the
        // tracked intervals it contains (clipped to the task), attributed
        // to its category. Nested work (Layout inside UpdateLayoutTree)
        // counts once, under the innermost category; per-category unions
        // below additionally keep overlapping same-category siblings from
        // double-counting past the task duration.
        let mut spans: [Vec<(f64, f64)>; 6] = Default::default();
        // Order children parents-before-children for containment scans.
        kids.sort_by(|&a, &b| {
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
        for (k, &i) in kids.iter().enumerate() {
            let slot = match bucket_of(&events[i].name) {
                Some(s) => s,
                None => continue,
            };
            let parent_ts = events[i].ts;
            let parent_dur = events[i].dur.unwrap_or(0.0);
            let s = parent_ts.max(rt_ts);
            let e = (events[i].ts + dur_of[i]).min(rt_end);
            if e <= s {
                continue;
            }
            let mut inner: Vec<(f64, f64)> = Vec::new();
            for &m in kids.iter().skip(k + 1) {
                if events[m].ph != b'X' {
                    continue;
                }
                let md = events[m].dur.unwrap_or(0.0);
                if md <= 0.0 || bucket_of(&events[m].name).is_none() {
                    continue;
                }
                let ms = events[m].ts;
                let me_raw = ms + md;
                // Strictly contained (or equal but ordered later).
                if ms >= parent_ts && me_raw <= parent_ts + parent_dur {
                    let cs = ms.max(s);
                    let ce = (events[m].ts + dur_of[m]).min(e);
                    if ce > cs {
                        inner.push((cs, ce));
                    }
                }
            }
            inner.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            // Emit the sub-slices of [s, e] not covered by nested tracked
            // children, so the per-category union below stays exact.
            let mut cur = s;
            for &(a, b) in &inner {
                if a > cur {
                    spans[slot].push((cur, a.min(e)));
                }
                if b > cur {
                    cur = b;
                }
                if cur >= e {
                    break;
                }
            }
            if cur < e {
                spans[slot].push((cur, e));
            }
        }
        let mut cat = [0.0f64; 6];
        for (slot, spans) in spans.iter_mut().enumerate() {
            spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            let mut total = 0.0f64;
            let mut cs = f64::INFINITY;
            let mut ce = f64::NEG_INFINITY;
            for &(a, b) in spans.iter() {
                if a <= ce {
                    if b > ce {
                        ce = b;
                    }
                } else {
                    if ce > cs {
                        total += ce - cs;
                    }
                    cs = a;
                    ce = b;
                }
            }
            if ce > cs {
                total += ce - cs;
            }
            cat[slot] = total;
        }

        tasks.push(FrameTask {
            ts: rt_ts,
            dur_us: rt_dur,
            js_us: cat[0],
            ult_us: cat[1],
            paint_us: cat[2],
            composite_us: cat[3],
            hit_test_us: cat[4],
            layout_us: cat[5],
            over_50ms: has_heavy_child,
        });
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
        over_50ms: tasks.iter().any(|t| t.over_50ms),
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
        // Linear interpolation, matching windowed::percentiles:
        // rank = q/100*(n-1), lerp between the bracketing samples.
        let percentile = |p: f64| -> f64 {
            let n = durs.len();
            if n == 1 {
                return durs[0];
            }
            let rank = p / 100.0 * (n - 1) as f64;
            let lo = rank.floor() as usize;
            let hi = rank.ceil() as usize;
            let frac = rank - lo as f64;
            durs[lo] + (durs[hi] - durs[lo]) * frac
        };
        ScrollFramePercentiles {
            p50_us: percentile(50.0),
            p90_us: percentile(90.0),
            p99_us: percentile(99.0),
        }
    };

    // Worst-first: consumers render "Worst Scroll Tasks" with take(10/15),
    // and every sibling analysis (layout/style/reflow) also returns sorted.
    // avg/percentiles are order-independent.
    tasks.sort_by(|a, b| b.dur_us.partial_cmp(&a.dur_us).unwrap());

    ScrollFrameResult {
        tasks,
        avg,
        percentiles,
        heuristic_fallback: !trace_has_input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str, ts: f64, dur: f64) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph: b'X',
            ts,
            dur: Some(dur),
            tid: 1,
            pid: 1,
            cat: None,
            args: None,
        }
    }

    fn ev_args(name: &str, ts: f64, dur: f64, args: serde_json::Value) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph: b'X',
            ts,
            dur: Some(dur),
            tid: 1,
            pid: 1,
            cat: None,
            args: crate::trace::test_args(args),
        }
    }

    #[test]
    fn boundary_child_of_next_task_does_not_flag_previous() {
        // Back-to-back RunTasks share the boundary (rt2.ts == rt1_end): the
        // next task's 55ms child starting exactly there must not turn the
        // tiny previous task into a scroll frame.
        let events = vec![
            ev("RunTask", 1_000.0, 1_000.0),
            ev("RunTask", 2_000.0, 60_000.0),
            ev("FunctionCall", 2_000.0, 55_000.0),
        ];
        let r = analyze_scroll_frames(&events, 1);
        assert_eq!(r.tasks.len(), 1);
        assert_eq!(r.tasks[0].dur_us, 60_000.0);
        assert_eq!(r.tasks[0].js_us, 55_000.0);
    }

    #[test]
    fn tasks_are_sorted_worst_first() {
        // "Worst Scroll Tasks" tables take(10/15) chronologically-ordered
        // tasks showed the earliest, not the worst.
        let events = vec![
            ev("RunTask", 1_000.0, 60_000.0),
            ev("FunctionCall", 1_000.0, 55_000.0),
            ev("RunTask", 100_000.0, 70_000.0),
            ev("FunctionCall", 100_000.0, 60_000.0),
            ev("RunTask", 200_000.0, 65_000.0),
            ev("UpdateLayoutTree", 200_000.0, 52_000.0),
        ];
        let r = analyze_scroll_frames(&events, 1);
        let durs: Vec<f64> = r.tasks.iter().map(|t| t.dur_us).collect();
        assert_eq!(durs, vec![70_000.0, 65_000.0, 60_000.0]);
    }

    #[test]
    fn frame_budget_gate_catches_sub_50ms_jank_and_marks_heavy() {
        // 20ms FunctionCall: over the 16.67ms frame budget but under 50ms.
        // No input events in the trace -> heuristic fallback, task kept,
        // over_50ms stays false.
        let events = vec![
            ev("RunTask", 1_000.0, 25_000.0),
            ev("FunctionCall", 1_000.0, 20_000.0),
        ];
        let r = analyze_scroll_frames(&events, 1);
        assert_eq!(r.tasks.len(), 1);
        assert_eq!(r.tasks[0].js_us, 20_000.0);
        assert!(!r.tasks[0].over_50ms);
        assert!(r.heuristic_fallback);
        // 10ms child: under the frame budget -> not a task.
        let events = vec![
            ev("RunTask", 1_000.0, 25_000.0),
            ev("FunctionCall", 1_000.0, 10_000.0),
        ];
        let r = analyze_scroll_frames(&events, 1);
        assert!(r.tasks.is_empty());
    }

    #[test]
    fn nested_children_count_once_under_self_time() {
        // UpdateLayoutTree 60ms containing a nested Layout 50ms: inclusive
        // sums would report 110ms for a 60ms child; self-time buckets must
        // report 10ms style + 50ms layout.
        let events = vec![
            ev("RunTask", 1_000.0, 70_000.0),
            ev("UpdateLayoutTree", 1_000.0, 60_000.0),
            ev("Layout", 5_000.0, 50_000.0),
        ];
        let r = analyze_scroll_frames(&events, 1);
        assert_eq!(r.tasks.len(), 1);
        assert_eq!(r.tasks[0].layout_us, 50_000.0);
        assert_eq!(r.tasks[0].ult_us, 10_000.0);
        assert!(r.tasks[0].over_50ms);
    }

    #[test]
    fn scroll_input_gates_tasks_when_evidence_exists() {
        // Trace carries scroll input: only the task overlapping it is kept,
        // and the fallback flag is off.
        let input = serde_json::json!({"data": {"type": "GestureScrollUpdate"}});
        let events = vec![
            ev("RunTask", 1_000.0, 60_000.0),
            ev("FunctionCall", 1_000.0, 55_000.0),
            ev("RunTask", 100_000.0, 60_000.0),
            ev("FunctionCall", 100_000.0, 55_000.0),
            ev_args("EventDispatch", 105_000.0, 5_000.0, input),
        ];
        let r = analyze_scroll_frames(&events, 1);
        assert!(!r.heuristic_fallback);
        assert_eq!(r.tasks.len(), 1);
        assert_eq!(r.tasks[0].ts, 100_000.0);
    }

    fn ev_tid(name: &str, ts: f64, dur: f64, tid: u64) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph: b'X',
            ts,
            dur: Some(dur),
            tid,
            pid: 1,
            cat: None,
            args: None,
        }
    }

    #[test]
    fn scroll_input_matching_is_case_insensitive() {
        // Lowercase args type, lowercase scrollBegin name, and MouseWheel
        // in mixed case all count as scroll evidence.
        for name in [
            "GestureScrollBegin",
            "scrollbegin",
            "SCROLLEND",
            "mousewheel",
        ] {
            let events = vec![
                ev("RunTask", 1_000.0, 60_000.0),
                ev("FunctionCall", 1_000.0, 55_000.0),
                ev_tid(name, 5_000.0, 1_000.0, 2),
            ];
            let r = analyze_scroll_frames(&events, 1);
            assert!(!r.heuristic_fallback, "name {name} must be scroll input");
            assert_eq!(r.tasks.len(), 1, "name {name} must gate the task in");
        }
        // Lowercase type inside EventDispatch args.
        let input = serde_json::json!({"data": {"type": "gesturescrollupdate"}});
        let events = vec![
            ev("RunTask", 1_000.0, 60_000.0),
            ev("FunctionCall", 1_000.0, 55_000.0),
            ev_args("EventDispatch", 5_000.0, 1_000.0, input),
        ];
        let r = analyze_scroll_frames(&events, 1);
        assert!(!r.heuristic_fallback);
        assert_eq!(r.tasks.len(), 1);
    }

    #[test]
    fn unrelated_key_input_does_not_disable_heuristic() {
        // A keyDown dispatch is input, but not scroll input: the workload
        // heuristic stays on and the heavy task is still reported.
        let input = serde_json::json!({"data": {"type": "keyDown"}});
        let events = vec![
            ev("RunTask", 1_000.0, 60_000.0),
            ev("FunctionCall", 1_000.0, 55_000.0),
            ev_args("EventDispatch", 5_000.0, 1_000.0, input),
        ];
        let r = analyze_scroll_frames(&events, 1);
        assert!(r.heuristic_fallback);
        assert_eq!(r.tasks.len(), 1);
    }

    #[test]
    fn cross_thread_scroll_input_gates_task() {
        // Scroll evidence on another thread still gates when its timestamp
        // falls inside the task interval.
        let input = serde_json::json!({"data": {"type": "scroll"}});
        let events = vec![
            ev("RunTask", 1_000.0, 60_000.0),
            ev("FunctionCall", 1_000.0, 55_000.0),
            ev_args("InputLatency", 5_000.0, 1_000.0, input),
        ];
        // ev_args pins tid 1; move the input off-thread.
        let mut events = events;
        events[2].tid = 9;
        let r = analyze_scroll_frames(&events, 1);
        assert!(!r.heuristic_fallback);
        assert_eq!(r.tasks.len(), 1);
    }

    #[test]
    fn overlapping_same_category_siblings_union_not_sum() {
        // Two overlapping FunctionCalls [0,60] + [40,100]: summed they claim
        // 120ms of JS in a 100ms task; unioned they claim exactly 100ms.
        let events = vec![
            ev("RunTask", 1_000.0, 100_000.0),
            ev("FunctionCall", 1_000.0, 60_000.0),
            ev("FunctionCall", 41_000.0, 60_000.0),
        ];
        let r = analyze_scroll_frames(&events, 1);
        assert_eq!(r.tasks.len(), 1);
        assert_eq!(r.tasks[0].js_us, 100_000.0);
        let total = r.tasks[0].js_us
            + r.tasks[0].ult_us
            + r.tasks[0].layout_us
            + r.tasks[0].paint_us
            + r.tasks[0].composite_us
            + r.tasks[0].hit_test_us;
        assert!(
            total <= r.tasks[0].dur_us + 1.0,
            "breakdown {total} over dur"
        );
    }
}
