//! Whole-trace summary: duration, long tasks, main-thread busy time and a
//! per-event-name breakdown.

use crate::trace::{TraceEvent, effective_runtask_dur, find_profiler_overhead, is_metadata_event};

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
    let overheads = find_profiler_overhead(events);
    // Single pass over the trace: duration bounds, long tasks, busy time, stats.
    let mut long_task_durs: Vec<f64> = Vec::new();
    let mut total_blocking_time_us = 0.0f64;
    let mut long_tasks_total_us = 0.0f64;
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
        if e.name == "RunTask" && e.dur.is_some() {
            let eff = effective_runtask_dur(e, &overheads);
            main_thread_busy_us += eff;
            if eff > 50_000.0 {
                long_task_durs.push(eff);
                total_blocking_time_us += eff - 50_000.0;
                long_tasks_total_us += eff;
            }
        }
        if let Some(key) = target_key(e.name)
            && let Some(d) = e.dur {
                let dur = if e.name == "RunTask" {
                    effective_runtask_dur(e, &overheads)
                } else {
                    d
                };
                let entry = stats_map.entry(key).or_default();
                entry.0 += dur;
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
        total_blocking_time_us,
        long_tasks_total_us,
    }
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
            args_cache: std::sync::OnceLock::new(),
        }
    }

    #[test]
    fn summary_excludes_cpu_profiler_start_from_tbt_and_long_tasks() {
        // RunTask of 1079ms contains CpuProfiler::StartProfiling of 1076ms.
        // Effective duration is 3ms -> NOT a long task, 0 TBT, 3ms busy time.
        let events = vec![
            ev("RunTask", b'X', 1_000_000.0, Some(1_079_000.0), 1),
            ev("CpuProfiler::StartProfiling", b'X', 1_001_000.0, Some(1_076_000.0), 1),
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
}

