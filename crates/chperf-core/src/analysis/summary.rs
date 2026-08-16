//! Whole-trace summary: duration, long tasks, main-thread busy time and a
//! per-event-name breakdown.

use crate::trace::{TraceEvent, is_metadata_event};

#[derive(Clone)]
pub struct EventTypeStat {
    pub name: &'static str,
    pub total_time_us: f64,
    pub count: usize,
    pub avg_time_us: f64,
    pub pct_of_trace: f64, // percentage of total trace time
}

#[derive(Clone)]
pub struct SummaryResult {
    pub long_task_count: usize,
    pub long_tasks_top: Vec<f64>,
    pub total_trace_duration_us: f64,
    pub main_thread_busy_us: f64, // total RunTask time on main thread
    pub event_stats: Vec<EventTypeStat>,
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
    // Single pass over the trace: duration bounds, long tasks, busy time, stats.
    let mut long_task_durs: Vec<f64> = Vec::new();
    let mut stats_map: rustc_hash::FxHashMap<&'static str, (f64, usize)> = rustc_hash::FxHashMap::default();
    let mut main_thread_busy_us = 0.0f64;
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
        if e.tid != main_tid || e.ph != b'X' {
            continue;
        }
        if e.name == "RunTask"
            && let Some(d) = e.dur {
                main_thread_busy_us += d;
                if d > 50_000.0 {
                    long_task_durs.push(d);
                }
            }
        if let Some(key) = target_key(e.name)
            && let Some(d) = e.dur {
                let entry = stats_map.entry(key).or_default();
                entry.0 += d;
                entry.1 += 1;
            }
    }

    long_task_durs.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let long_task_count = long_task_durs.len();
    let long_tasks_top: Vec<f64> = long_task_durs.into_iter().take(10).collect();
    let total_trace_duration_us = (max_ts - min_ts).max(0.0);

    let mut event_stats: Vec<EventTypeStat> = stats_map
        .into_iter()
        .map(|(name, (total, count))| EventTypeStat {
            name,
            total_time_us: total,
            count,
            avg_time_us: if count > 0 {
                total / count as f64
            } else {
                0.0
            },
            pct_of_trace: if total_trace_duration_us > 0.0 {
                total / total_trace_duration_us * 100.0
            } else {
                0.0
            },
        })
        .collect();
    event_stats.sort_by(|a, b| b.total_time_us.partial_cmp(&a.total_time_us).unwrap());

    SummaryResult {
        long_task_count,
        long_tasks_top,
        total_trace_duration_us,
        main_thread_busy_us,
        event_stats,
    }
}
