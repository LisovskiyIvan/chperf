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
fn extract_initiator(e: &TraceEvent) -> Option<String> {
    let args = e.args_value()?;
    let begin_data = args.get("beginData")?;
    let stack = begin_data.get("stackTrace")?.as_array()?;
    for frame in stack {
        let func = frame.get("functionName").and_then(|v| v.as_str()).unwrap_or("");
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
    None
}

/// Detect forced reflow: Layout or UpdateLayoutTree occurring synchronously
/// during JavaScript execution (inside FunctionCall, EvaluateScript, etc.)
/// or carrying a script stackTrace initiator.
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
    let mut all_task_reflows: Vec<usize> = Vec::new();
    let mut all_task_layout: Vec<f64> = Vec::new();

    // Single sweep: children are in ts order already.
    let mut lo = 0usize;
    for rt in &run_tasks {
        let rt_ts = rt.ts;
        let rt_dur = rt.dur.unwrap();
        let rt_end = rt_ts + rt_dur;

        while lo < main_x.len() && main_x[lo].ts < rt_ts {
            lo += 1;
        }

        let mut reflow_count = 0usize;
        let mut layout_time = 0.0f64;
        let mut active_js_end = 0.0f64;
        let mut top_initiator: Option<String> = None;

        let mut j = lo;
        while j < main_x.len() && main_x[j].ts <= rt_end {
            let c = main_x[j];
            if c.name != "RunTask" && c.ts + c.dur.unwrap_or(0.0) <= rt_end {
                match c.name {
                    "FunctionCall" | "EvaluateScript" | "v8.run" | "v8.compile" => {
                        let end = c.ts + c.dur.unwrap_or(0.0);
                        if end > active_js_end {
                            active_js_end = end;
                        }
                    }
                    "Layout" | "UpdateLayoutTree" => {
                        let init = extract_initiator(c);
                        // A reflow is forced if it occurs while JS is running (c.ts < active_js_end)
                        // or if it carries an explicit script initiator stack trace.
                        if c.ts < active_js_end || init.is_some() {
                            reflow_count += 1;
                            layout_time += c.dur.unwrap_or(0.0);
                            if top_initiator.is_none() && init.is_some() {
                                top_initiator = init;
                            }
                        }
                    }
                    _ => {}
                }
            }
            j += 1;
        }

        if reflow_count > 0 {
            all_task_reflows.push(reflow_count);
            all_task_layout.push(layout_time);
        }
        if reflow_count >= 2 {
            entries.push(ForcedReflowEntry {
                task_dur_us: rt_dur,
                reflow_count,
                layout_time_us: layout_time,
                initiator: top_initiator,
            });
        }
    }

    entries.sort_by_key(|b| std::cmp::Reverse(b.reflow_count));

    // Totals cover every task with at least one forced reflow; the `entries`
    // table keeps the >=2 (thrashing) gate. Summing entries only reported 0
    // for pages whose handlers force exactly one reflow per task.
    let total_reflows: usize = all_task_reflows.iter().sum();
    let total_layout_time_us: f64 = all_task_layout.iter().sum();

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
            name: crate::trace::intern_name(name),
            id: 0,
            has_id: false,
            ph: b'X',
            ts,
            dur: Some(dur),
            tid: 1,
            pid: 1,
            cat: None,
            args: args.and_then(crate::trace::test_args),
            args_cache: std::sync::OnceLock::new(),
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
}
