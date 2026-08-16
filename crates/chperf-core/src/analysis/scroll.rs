//! Scroll-frame analysis: RunTasks whose children include heavy
//! UpdateLayoutTree / FunctionCall work, broken down by cost category.

use crate::trace::TraceEvent;

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
}

pub fn analyze_scroll_frames(events: &[TraceEvent], main_tid: u64) -> ScrollFrameResult {
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

    let mut tasks = Vec::new();

    // Single sweep: RunTasks on a thread are serial and non-nested, so each
    // main-thread event falls inside at most one task. `lo` only moves forward.
    let mut lo = 0usize;
    for rt in &run_tasks {
        let rt_ts = rt.ts;
        let rt_dur = rt.dur.unwrap();
        let rt_end = rt_ts + rt_dur;

        while lo < main_x.len() && main_x[lo].ts < rt_ts {
            lo += 1;
        }
        let mut j = lo;
        let mut has_heavy_ult = false;
        let mut has_heavy_fc = false;
        while j < main_x.len() && main_x[j].ts <= rt_end {
            let e = main_x[j];
            if e.name != "RunTask" {
                let d = e.dur.unwrap_or(0.0);
                if d > 50_000.0 {
                    if e.name == "UpdateLayoutTree" {
                        has_heavy_ult = true;
                    } else if e.name == "FunctionCall" {
                        has_heavy_fc = true;
                    }
                }
            }
            j += 1;
        }

        if !has_heavy_ult && !has_heavy_fc {
            continue;
        }

        let mut ft = FrameTask {
            ts: rt_ts,
            dur_us: rt_dur,
            js_us: 0.0,
            ult_us: 0.0,
            paint_us: 0.0,
            composite_us: 0.0,
            hit_test_us: 0.0,
            layout_us: 0.0,
        };

        j = lo;
        while j < main_x.len() && main_x[j].ts <= rt_end {
            let e = main_x[j];
            if e.name != "RunTask" {
                let d = e.dur.unwrap_or(0.0);
                if d > 0.0 && e.ts + d <= rt_end {
                    match e.name {
                        "FunctionCall" => ft.js_us += d,
                        "UpdateLayoutTree" => ft.ult_us += d,
                        "Paint" => ft.paint_us += d,
                        "Layerize" | "Commit" => ft.composite_us += d,
                        "HitTest" => ft.hit_test_us += d,
                        "Layout" => ft.layout_us += d,
                        _ => {}
                    }
                }
            }
            j += 1;
        }
        tasks.push(ft);
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
        let percentile = |p: f64| -> f64 {
            let idx = ((p / 100.0) * (durs.len() as f64 - 1.0)).round() as usize;
            durs[idx.min(durs.len() - 1)]
        };
        ScrollFramePercentiles {
            p50_us: percentile(50.0),
            p90_us: percentile(90.0),
            p99_us: percentile(99.0),
        }
    };

    ScrollFrameResult { tasks, avg, percentiles }
}
