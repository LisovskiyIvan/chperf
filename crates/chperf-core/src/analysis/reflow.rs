//! Forced-reflow (layout thrashing) detection.

use crate::trace::TraceEvent;

#[derive(Clone)]
pub struct ForcedReflowEntry {
    pub task_dur_us: f64,
    pub reflow_count: usize, // number of forced reflows in task
    pub layout_time_us: f64,
    pub initiator: Option<String>,
}

#[derive(Clone)]
pub struct ForcedReflowResult {
    pub entries: Vec<ForcedReflowEntry>,
    pub total_reflows: usize,
    pub total_layout_time_us: f64,
}

/// Extract initiator script/function from `args.beginData.stackTrace` if present.
///
/// Three passes, most specific first:
/// 1. strict: first frame with a function name (or bare URL) plus a line
///    number, as `func (file:line)` / `file:line`;
/// 2. relaxed: any frame with a function name, else any frame with a URL
///    (deep frames count — a stack that exists anywhere in the array must
///    not degrade to "scheduled");
/// 3. `args.beginData.frame` / the nearest enclosing JS function name is
///    left to the caller: when only a frame id is known there is no
///    script-level initiator, and returning the raw frame id would label
///    every forced layout with noise. `None` here means "no script
///    attribution found", not "scheduled pipeline work".
fn extract_initiator(e: &TraceEvent) -> Option<String> {
    let args = e.args_value()?;
    let begin_data = args.get("beginData")?;
    let stack = begin_data.get("stackTrace").and_then(|v| v.as_array());
    if let Some(stack) = stack {
        for frame in stack {
            let func = frame
                .get("functionName")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let url = frame.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let line = frame.get("lineNumber").and_then(|v| v.as_u64());
            if !func.is_empty() || !url.is_empty() {
                let short_url = if let Some(idx) = url.rfind('/') {
                    &url[idx + 1..]
                } else {
                    url
                };
                if let Some(l) = line {
                    if !func.is_empty() && !short_url.is_empty() {
                        return Some(format!("{func} ({short_url}:{l})"));
                    } else if !short_url.is_empty() {
                        return Some(format!("{short_url}:{l}"));
                    }
                }
                if !func.is_empty() {
                    return Some(func.to_string());
                }
            }
        }
    }
    // Relaxed pass: a stack exists but no frame satisfied the strict rule
    // (e.g. URL-only frames without line numbers). Deep frames count.
    if let Some(stack) = stack {
        for frame in stack {
            let func = frame
                .get("functionName")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !func.is_empty() {
                return Some(func.to_string());
            }
        }
        for frame in stack {
            let url = frame.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if !url.is_empty() {
                let short_url = if let Some(idx) = url.rfind('/') {
                    &url[idx + 1..]
                } else {
                    url
                };
                if let Some(l) = frame.get("lineNumber").and_then(|v| v.as_u64()) {
                    return Some(format!("{short_url}:{l}"));
                }
                return Some(short_url.to_string());
            }
        }
    }
    // Last resort: a frame id proves script context but names no script;
    // surface it rather than silently reporting "scheduled".
    if let Some(frame_id) = args
        .get("beginData")
        .and_then(|bd| bd.get("frame"))
        .and_then(|v| v.as_str())
        && !frame_id.is_empty()
    {
        return Some(format!("frame {frame_id}"));
    }
    None
}

/// Detect forced reflow: Layout or UpdateLayoutTree occurring synchronously
/// during JavaScript execution (its interval overlapping a `FunctionCall` /
/// `EvaluateScript` / `v8.run` / `v8.compile` interval on the same thread)
/// or carrying a script stackTrace initiator.
///
/// Both the `entries` table and the totals use the same >=1 rule: every task
/// with at least one forced reflow gets an entry (single-reflow tasks are
/// kept — they are the common case — with their initiator when known).
/// `layout_time_us` counts only top-level forced layouts per task (nested
/// `Layout`s inside an outer forced `Layout`/`UpdateLayoutTree` are not
/// summed twice); use `total_layout_time_us` for cross-trace comparison.
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

    // Single sweep: children are in ts order already.
    let mut lo = 0usize;
    for rt in &run_tasks {
        let rt_ts = rt.ts;
        let rt_dur = rt.dur.unwrap();
        let rt_end = rt_ts + rt_dur;

        while lo < main_x.len() && main_x[lo].ts < rt_ts {
            lo += 1;
        }

        // JS intervals and candidate layouts overlapping this task, clipped
        // to the task window (children straddling the RunTask end are
        // clipped, not dropped — the in-task slice still ran here).
        let mut js_spans: Vec<(f64, f64)> = Vec::new();
        let mut candidates: Vec<(f64, f64, Option<String>)> = Vec::new();
        let mut j = lo;
        while j < main_x.len() && main_x[j].ts < rt_end {
            let c = main_x[j];
            if c.name != "RunTask" {
                let c_dur = c.dur.unwrap_or(0.0);
                if c_dur > 0.0 {
                    let s = c.ts.max(rt_ts);
                    let e = (c.ts + c_dur).min(rt_end);
                    if e > s {
                        match c.name.as_str() {
                            "FunctionCall" | "EvaluateScript" | "v8.run" | "v8.compile" => {
                                js_spans.push((s, e));
                            }
                            "Layout" | "UpdateLayoutTree" => {
                                candidates.push((s, e, extract_initiator(c)));
                            }
                            _ => {}
                        }
                    }
                }
            }
            j += 1;
        }
        // Merge JS spans so overlap tests stay linear.
        js_spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut merged: Vec<(f64, f64)> = Vec::with_capacity(js_spans.len());
        for (s, e) in js_spans {
            if let Some(last) = merged.last_mut()
                && s <= last.1
            {
                if e > last.1 {
                    last.1 = e;
                }
                continue;
            }
            merged.push((s, e));
        }
        // Forced = overlaps an active JS interval (partial overlap is
        // conservative and correct: any synchronous slice counts) or
        // carries an explicit script initiator stack trace. Both lists are
        // in ts order (single in-order sweep above), so one pointer through
        // the merged JS spans serves all candidates in linear time.
        let mut forced: Vec<(f64, f64, Option<String>)> = Vec::new();
        let mut p = 0usize;
        for (s, e, init) in candidates {
            while p < merged.len() && merged[p].1 <= s {
                p += 1;
            }
            let overlaps_js = p < merged.len() && merged[p].0 < e;
            if overlaps_js || init.is_some() {
                forced.push((s, e, init));
            }
        }
        // Count only top-level forced layouts: drop any forced interval
        // contained in an earlier (outer) one so nested Layouts never push
        // `layout_time` past the task duration. Sorted start-asc/end-desc,
        // containment is a single max-end check: every earlier interval
        // starts at or before this one, so this one is nested iff it ends
        // at or before the largest end seen so far.
        forced.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap()
                .then_with(|| b.1.partial_cmp(&a.1).unwrap())
        });
        let mut top_level: Vec<(f64, f64, Option<String>)> = Vec::new();
        let mut max_end = f64::NEG_INFINITY;
        for (s, e, init) in forced {
            if e <= max_end {
                continue;
            }
            max_end = e;
            top_level.push((s, e, init));
        }

        let reflow_count = top_level.len();
        if reflow_count == 0 {
            continue;
        }
        let layout_time: f64 = top_level.iter().map(|(s, e, _)| e - s).sum();
        let top_initiator = top_level.iter().find_map(|(_, _, init)| init.clone());
        // Entries and totals share the same >=1 rule: every task with at
        // least one forced reflow is listed (single-reflow tasks are the
        // common case) and counted.
        entries.push(ForcedReflowEntry {
            task_dur_us: rt_dur,
            reflow_count,
            layout_time_us: layout_time,
            initiator: top_initiator,
        });
    }

    entries.sort_by_key(|b| std::cmp::Reverse(b.reflow_count));

    // Totals are the sums over `entries` (same >=1 gate), so the table and
    // the headline numbers can never disagree.
    let total_reflows: usize = entries.iter().map(|e| e.reflow_count).sum();
    let total_layout_time_us: f64 = entries.iter().map(|e| e.layout_time_us).sum();

    ForcedReflowResult {
        entries,
        total_reflows,
        total_layout_time_us,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_x(ts: f64, dur: f64, name: &str, args: Option<serde_json::Value>) -> TraceEvent {
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
            args: args.and_then(crate::trace::test_args),
        }
    }

    #[test]
    fn detects_loop_reflow_inside_single_function_call() {
        // One FunctionCall that queries layout in a loop: 4 UpdateLayoutTree calls
        let events = vec![
            ev_x(100.0, 500.0, "RunTask", None),
            ev_x(110.0, 400.0, "FunctionCall", None),
            ev_x(120.0, 10.0, "UpdateLayoutTree", None),
            ev_x(150.0, 15.0, "UpdateLayoutTree", None),
            ev_x(200.0, 20.0, "Layout", None),
            ev_x(250.0, 12.0, "UpdateLayoutTree", None),
        ];

        let res = analyze_forced_reflows(&events, 1);
        assert_eq!(res.total_reflows, 4);
        assert_eq!(res.entries.len(), 1);
        assert_eq!(res.entries[0].reflow_count, 4);
        assert_eq!(res.entries[0].layout_time_us, 57.0);
    }

    #[test]
    fn scheduled_pipeline_after_js_is_not_forced_reflow() {
        // JS finishes at 200, then scheduled rendering runs at 210 and 230
        let events = vec![
            ev_x(100.0, 300.0, "RunTask", None),
            ev_x(110.0, 90.0, "FunctionCall", None), // ends at 200
            ev_x(210.0, 20.0, "UpdateLayoutTree", None),
            ev_x(230.0, 30.0, "Layout", None),
        ];

        let res = analyze_forced_reflows(&events, 1);
        assert_eq!(res.total_reflows, 0);
        assert!(res.entries.is_empty());
    }

    #[test]
    fn partial_overlap_with_js_is_forced() {
        // Layout [190, 220) overlaps the tail of FunctionCall [110, 200):
        // any synchronous slice counts (conservative overlap rule).
        let events = vec![
            ev_x(100.0, 300.0, "RunTask", None),
            ev_x(110.0, 90.0, "FunctionCall", None),
            ev_x(190.0, 30.0, "Layout", None),
        ];
        let res = analyze_forced_reflows(&events, 1);
        assert_eq!(res.total_reflows, 1);
        assert_eq!(res.entries.len(), 1);
        assert_eq!(res.entries[0].layout_time_us, 30.0);
    }

    #[test]
    fn straddling_child_is_clipped_not_dropped() {
        // Layout [390, 420) straddles the RunTask end at 400: the in-task
        // 10µs slice still ran here, so it counts (clipped) instead of
        // vanishing from both tasks.
        let events = vec![
            ev_x(100.0, 300.0, "RunTask", None),
            ev_x(110.0, 290.0, "FunctionCall", None), // clipped to task end 400
            ev_x(390.0, 30.0, "Layout", None),
        ];
        let res = analyze_forced_reflows(&events, 1);
        assert_eq!(res.total_reflows, 1);
        assert_eq!(res.entries[0].layout_time_us, 10.0);
    }

    #[test]
    fn nested_forced_layouts_count_once() {
        // UpdateLayoutTree [120, 180) containing Layout [130, 170), both
        // forced: only the top-level interval counts, so layout_time (60)
        // can never exceed the task duration by double counting.
        let events = vec![
            ev_x(100.0, 300.0, "RunTask", None),
            ev_x(110.0, 200.0, "FunctionCall", None),
            ev_x(120.0, 60.0, "UpdateLayoutTree", None),
            ev_x(130.0, 40.0, "Layout", None),
        ];
        let res = analyze_forced_reflows(&events, 1);
        assert_eq!(res.total_reflows, 1);
        assert_eq!(res.entries[0].layout_time_us, 60.0);
    }

    #[test]
    fn initiator_falls_back_to_deep_frames_and_frame_id() {
        // Strict pass finds nothing (no line numbers), but the stack exists
        // deeper in the array: the relaxed pass must surface it, never
        // silently "scheduled".
        let deep = serde_json::json!({
            "beginData": {"stackTrace": [
                {"functionName": "", "url": ""},
                {"functionName": "", "url": "https://x/app.js"},
            ]}
        });
        assert_eq!(
            extract_initiator(&ev_x(0.0, 10.0, "Layout", Some(deep))).as_deref(),
            Some("app.js")
        );
        // URL-only frame without a line number.
        let url_only = serde_json::json!({
            "beginData": {"stackTrace": [
                {"functionName": "", "url": "https://x/bundle.js"},
            ]}
        });
        assert_eq!(
            extract_initiator(&ev_x(0.0, 10.0, "Layout", Some(url_only))).as_deref(),
            Some("bundle.js")
        );
        // No stack at all, but a frame id proves script context.
        let framed = serde_json::json!({"beginData": {"frame": "ABC123"}});
        assert_eq!(
            extract_initiator(&ev_x(0.0, 10.0, "Layout", Some(framed))).as_deref(),
            Some("frame ABC123")
        );
        assert!(extract_initiator(&ev_x(0.0, 10.0, "Layout", None)).is_none());
    }

    #[test]
    fn totals_equal_sum_of_entries_across_tasks() {
        // Two tasks with forced reflows: headline totals must equal the
        // sums over entries (same >=1 gate), never drift apart.
        let events = vec![
            ev_x(100.0, 300.0, "RunTask", None),
            ev_x(110.0, 100.0, "FunctionCall", None),
            ev_x(120.0, 10.0, "Layout", None),
            ev_x(150.0, 20.0, "Layout", None),
            ev_x(500.0, 300.0, "RunTask", None),
            ev_x(510.0, 200.0, "FunctionCall", None),
            ev_x(520.0, 30.0, "UpdateLayoutTree", None),
        ];
        let res = analyze_forced_reflows(&events, 1);
        assert_eq!(res.entries.len(), 2);
        let count: usize = res.entries.iter().map(|e| e.reflow_count).sum();
        let time: f64 = res.entries.iter().map(|e| e.layout_time_us).sum();
        assert_eq!(res.total_reflows, count);
        assert_eq!(res.total_layout_time_us, time);
        assert_eq!(res.total_reflows, 3);
        assert_eq!(res.total_layout_time_us, 60.0);
    }

    #[test]
    fn disjoint_js_spans_each_gate_their_layouts() {
        // Two disjoint FunctionCalls: a Layout between them is scheduled
        // (not forced); Layouts overlapping either span are forced. This
        // exercises the linear sweep across multiple merged spans.
        let events = vec![
            ev_x(100.0, 500.0, "RunTask", None),
            ev_x(110.0, 50.0, "FunctionCall", None), // [110,160)
            ev_x(300.0, 50.0, "FunctionCall", None), // [300,350)
            ev_x(120.0, 10.0, "Layout", None),       // forced (span 1)
            ev_x(200.0, 10.0, "Layout", None),       // scheduled (gap)
            ev_x(310.0, 10.0, "Layout", None),       // forced (span 2)
        ];
        let res = analyze_forced_reflows(&events, 1);
        assert_eq!(res.total_reflows, 2);
        assert_eq!(res.entries[0].layout_time_us, 20.0);
    }

    #[test]
    fn partially_overlapping_forced_layouts_are_not_nested() {
        // [120,180) and [150,220) overlap but neither contains the other:
        // both count (no max-end false positive), 60 + 70 = 130.
        let events = vec![
            ev_x(100.0, 300.0, "RunTask", None),
            ev_x(110.0, 200.0, "FunctionCall", None),
            ev_x(120.0, 60.0, "Layout", None),
            ev_x(150.0, 70.0, "Layout", None),
        ];
        let res = analyze_forced_reflows(&events, 1);
        assert_eq!(res.total_reflows, 2);
        assert_eq!(res.entries[0].layout_time_us, 130.0);
    }
}
