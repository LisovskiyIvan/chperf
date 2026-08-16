//! Forced-reflow (layout thrashing) detection.

use crate::trace::TraceEvent;

#[derive(Clone)]
pub struct ForcedReflowEntry {
    pub task_dur_us: f64,
    pub reflow_count: usize, // number of JS→Layout/ULT alternations
    pub layout_time_us: f64,
}

#[derive(Clone)]
pub struct ForcedReflowResult {
    pub entries: Vec<ForcedReflowEntry>,
    pub total_reflows: usize,
    pub total_layout_time_us: f64,
}

/// Detect forced reflow: RunTask containing alternating FunctionCall→(Layout|UpdateLayoutTree)→FunctionCall pattern
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

    // Single sweep, same as scroll frames: children are in ts order already.
    let mut lo = 0usize;
    for rt in &run_tasks {
        let rt_ts = rt.ts;
        let rt_end = rt_ts + rt.dur.unwrap();

        while lo < main_x.len() && main_x[lo].ts < rt_ts {
            lo += 1;
        }

        // Look for JS→Layout/ULT alternation pattern
        let mut reflow_count = 0usize;
        let mut layout_time = 0.0f64;
        let mut last_was_js = false;
        let mut j = lo;
        while j < main_x.len() && main_x[j].ts <= rt_end {
            let c = main_x[j];
            if c.name != "RunTask" && c.ts + c.dur.unwrap_or(0.0) <= rt_end {
                match c.name {
                    "FunctionCall" => {
                        last_was_js = true;
                    }
                    "Layout" | "UpdateLayoutTree" => {
                        if last_was_js {
                            reflow_count += 1;
                            layout_time += c.dur.unwrap_or(0.0);
                        }
                        last_was_js = false;
                    }
                    _ => {}
                }
            }
            j += 1;
        }

        if reflow_count >= 2 {
            entries.push(ForcedReflowEntry {
                task_dur_us: rt.dur.unwrap(),
                reflow_count,
                layout_time_us: layout_time,
            });
        }
    }

    entries.sort_by_key(|b| std::cmp::Reverse(b.reflow_count));

    let total_reflows: usize = entries.iter().map(|e| e.reflow_count).sum();
    let total_layout_time_us: f64 = entries.iter().map(|e| e.layout_time_us).sum();

    ForcedReflowResult {
        entries,
        total_reflows,
        total_layout_time_us,
    }
}
