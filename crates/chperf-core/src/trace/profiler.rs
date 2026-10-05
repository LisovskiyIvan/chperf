//! Profiler overhead detection and filtering.
//!
//! When recording a trace via Chrome DevTools or CDP `Tracing.start`, Chrome's
//! sampling profiler initializes on the main thread via `CpuProfiler::StartProfiling`.
//! This internal V8 initialization allocates sample buffers and sets up timers,
//! blocking the main thread for hundreds of milliseconds (often ~800–1200ms).
//!
//! Because this is an artifact of the profiler instrumentation:
//! 1. The enclosing `RunTask` is not actual application work. Without adjustment,
//!    it creates a false Long Task (>50ms) and inflates Total Blocking Time (TBT)
//!    and main thread busy time by ~1s.
//! 2. During this freeze, the compositor drops frames because the main thread is
//!    unresponsive, creating false `DroppedFrame` spikes and artificial jank clusters.
//! 3. Comparisons between two traces (e.g. A vs B) report false regressions or
//!    improvements based on profiler initialization jitter rather than app code.
//!
//! This module identifies `CpuProfiler::StartProfiling` intervals, deducts their
//! duration from enclosing `RunTask`s, and ignores frames dropped during the
//! profiler startup freeze.

use super::event::TraceEvent;

/// Interval of profiler startup overhead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProfilerOverhead {
    pub tid: u64,
    pub ts: f64,
    pub dur: f64,
    /// Task window [start, end] of an enclosing RunTask dominated by this
    /// initialization, if present.
    pub task_window: Option<(f64, f64)>,
}

impl ProfilerOverhead {
    #[inline]
    pub fn end(&self) -> f64 {
        self.ts + self.dur
    }
}

/// Locate all `CpuProfiler::StartProfiling` overhead intervals in `events`.
pub fn find_profiler_overhead(events: &[TraceEvent]) -> Vec<ProfilerOverhead> {
    let mut starts: Vec<(u64, f64, f64)> = Vec::new();
    for e in events {
        if e.name == "CpuProfiler::StartProfiling" {
            let dur = e.dur.unwrap_or(0.0);
            if dur > 0.0 {
                starts.push((e.tid, e.ts, dur));
            }
        }
    }
    if starts.is_empty() {
        return Vec::new();
    }

    starts
        .into_iter()
        .map(|(tid, sp_ts, sp_dur)| {
            let sp_end = sp_ts + sp_dur;
            let mut enclosing_task: Option<(f64, f64)> = None;
            for e in events {
                if e.tid == tid && e.name == "RunTask" && e.ph == b'X'
                    && let Some(dur) = e.dur {
                        let rt_ts = e.ts;
                        let rt_end = rt_ts + dur;
                        // StartProfiling is nested inside this RunTask (allow 10µs float tolerance)
                        if rt_ts <= sp_ts + 10.0 && rt_end + 10.0 >= sp_end {
                            if sp_dur >= 50_000.0 || sp_dur >= 0.5 * dur {
                                enclosing_task = Some((rt_ts, rt_end));
                            }
                            break;
                        }
                    }
            }
            ProfilerOverhead {
                tid,
                ts: sp_ts,
                dur: sp_dur,
                task_window: enclosing_task,
            }
        })
        .collect()
}

/// Compute the effective duration of a RunTask after deducting profiler startup overhead.
#[inline]
pub fn effective_runtask_dur(rt: &TraceEvent, overheads: &[ProfilerOverhead]) -> f64 {
    let dur = match rt.dur {
        Some(d) => d,
        None => return 0.0,
    };
    if overheads.is_empty() {
        return dur;
    }
    let rt_ts = rt.ts;
    let rt_end = rt_ts + dur;
    let mut overlap = 0.0f64;
    for ov in overheads {
        if ov.tid == rt.tid {
            let sp_ts = ov.ts;
            let sp_end = ov.end();
            let o = (rt_end.min(sp_end) - rt_ts.max(sp_ts)).max(0.0);
            overlap += o;
        }
    }
    (dur - overlap).max(0.0)
}

/// Check if a dropped frame event timestamp coincides with the profiler startup freeze.
#[inline]
pub fn is_profiler_dropped_frame(ts: f64, overheads: &[ProfilerOverhead]) -> bool {
    if overheads.is_empty() {
        return false;
    }
    for ov in overheads {
        if let Some((win_lo, win_hi)) = ov.task_window
            && ts >= win_lo && ts <= win_hi {
                return true;
            }
        if ts >= ov.ts && ts <= ov.end() {
            return true;
        }
    }
    false
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
    fn test_profiler_overhead_detection_and_deduction() {
        let events = vec![
            ev("RunTask", b'X', 1_000_000.0, Some(1_079_000.0), 1),
            ev("CpuProfiler::StartProfiling", b'X', 1_001_000.0, Some(1_076_000.0), 1),
            ev("DroppedFrame", b'I', 1_005_000.0, None, 7),
            ev("DroppedFrame", b'I', 1_050_000.0, None, 7),
            ev("DroppedFrame", b'I', 3_000_000.0, None, 7),
            ev("RunTask", b'X', 3_000_000.0, Some(100_000.0), 1),
        ];

        let overheads = find_profiler_overhead(&events);
        assert_eq!(overheads.len(), 1);
        assert_eq!(overheads[0].tid, 1);
        assert_eq!(overheads[0].ts, 1_001_000.0);
        assert_eq!(overheads[0].dur, 1_076_000.0);
        assert_eq!(overheads[0].task_window, Some((1_000_000.0, 2_079_000.0)));

        // Profiler task effective duration is 3ms instead of 1079ms
        let eff_prof = effective_runtask_dur(&events[0], &overheads);
        assert_eq!(eff_prof, 3_000.0);

        // Real task retains full 100ms duration
        let eff_real = effective_runtask_dur(&events[5], &overheads);
        assert_eq!(eff_real, 100_000.0);

        // Dropped frames during the profiler freeze are identified
        assert!(is_profiler_dropped_frame(1_005_000.0, &overheads));
        assert!(is_profiler_dropped_frame(1_050_000.0, &overheads));

        // Dropped frames outside the freeze are not identified as profiler overhead
        assert!(!is_profiler_dropped_frame(3_000_000.0, &overheads));
    }
}
