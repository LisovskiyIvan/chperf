//! Two-trace comparison: per-event averages, scroll frames, CPU diff, findings.

use super::cpu::{CpuProfileResult, FunctionTime, SourceType};
use super::layout::LayoutDirtyResult;
use super::scroll::{FrameTask, ScrollFramePercentiles, ScrollFrameResult};
use super::style::StyleRecalcResult;
use super::summary::{EventTypeStat, SummaryResult};
use std::collections::HashMap;

#[derive(Clone)]
#[allow(dead_code)]
pub struct CompareRow {
    pub event_name: String,
    pub count_a: usize,
    pub count_b: usize,
    pub avg_a_us: f64,
    pub avg_b_us: f64,
    pub diff_pct: f64,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct CpuFunctionDiff {
    pub function_name: String,
    pub url: String,
    pub source_type: SourceType,
    pub time_a_us: f64,
    pub time_b_us: f64,
    pub pct_a: f64,
    pub pct_b: f64,
    pub diff_pct: f64,
}

#[derive(Clone)]
pub struct Finding {
    pub severity: FindingSeverity,
    pub category: String,
    pub message: String,
    pub detail: String,
}

#[derive(Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum FindingSeverity {
    Improved,
    Regressed,
    Neutral,
}

#[derive(Clone)]
pub struct CompareResult {
    pub rows: Vec<CompareRow>,
    pub scroll_avg_a: Option<FrameTask>,
    pub scroll_avg_b: Option<FrameTask>,
    pub scroll_pct_a: ScrollFramePercentiles,
    pub scroll_pct_b: ScrollFramePercentiles,
    pub scroll_count_a: usize,
    pub scroll_count_b: usize,
    pub summary_a: SummaryResult,
    pub summary_b: SummaryResult,
    pub cpu_diff: Vec<CpuFunctionDiff>,
    pub layout_a: LayoutDirtyResult,
    pub layout_b: LayoutDirtyResult,
    pub style_recalc_a: StyleRecalcResult,
    pub style_recalc_b: StyleRecalcResult,
    pub findings: Vec<Finding>,
}

fn pct_diff(a: f64, b: f64) -> f64 {
    if a > 0.0 {
        (b - a) / a * 100.0
    } else {
        0.0
    }
}

#[allow(clippy::too_many_arguments)]
pub fn analyze_compare(
    summary_a: &SummaryResult,
    summary_b: &SummaryResult,
    scroll_a: &ScrollFrameResult,
    scroll_b: &ScrollFrameResult,
    cpu_a: &CpuProfileResult,
    cpu_b: &CpuProfileResult,
    layout_a: &LayoutDirtyResult,
    layout_b: &LayoutDirtyResult,
    style_recalc_a: &StyleRecalcResult,
    style_recalc_b: &StyleRecalcResult,
) -> CompareResult {
    // ── Event rows ──
    let map_a: HashMap<&str, &EventTypeStat> = summary_a
        .event_stats
        .iter()
        .map(|s| (s.name, s))
        .collect();
    let map_b: HashMap<&str, &EventTypeStat> = summary_b
        .event_stats
        .iter()
        .map(|s| (s.name, s))
        .collect();

    let mut all_names: Vec<&str> = map_a.keys().chain(map_b.keys()).copied().collect();
    all_names.sort();
    all_names.dedup();

    let mut rows: Vec<CompareRow> = all_names
        .into_iter()
        .map(|name| {
            let a = map_a.get(name);
            let b = map_b.get(name);
            let avg_a = a.map(|s| s.avg_time_us).unwrap_or(0.0);
            let avg_b = b.map(|s| s.avg_time_us).unwrap_or(0.0);
            CompareRow {
                event_name: name.to_string(),
                count_a: a.map(|s| s.count).unwrap_or(0),
                count_b: b.map(|s| s.count).unwrap_or(0),
                avg_a_us: avg_a,
                avg_b_us: avg_b,
                diff_pct: pct_diff(avg_a, avg_b),
            }
        })
        .collect();

    rows.sort_by(|a, b| b.diff_pct.abs().partial_cmp(&a.diff_pct.abs()).unwrap());

    // ── Scroll frame ──
    let scroll_avg_a = if scroll_a.tasks.is_empty() {
        None
    } else {
        Some(scroll_a.avg.clone())
    };
    let scroll_avg_b = if scroll_b.tasks.is_empty() {
        None
    } else {
        Some(scroll_b.avg.clone())
    };

    // ── CPU profile diff ──
    let total_a = cpu_a.total_sample_time_us;
    let total_b = cpu_b.total_sample_time_us;

    let mut func_map_a: rustc_hash::FxHashMap<(&str, &str), &FunctionTime> = rustc_hash::FxHashMap::default();
    for f in &cpu_a.functions {
        func_map_a
            .entry((&f.function_name, &f.url))
            .or_insert(f);
    }
    let mut func_map_b: rustc_hash::FxHashMap<(&str, &str), &FunctionTime> = rustc_hash::FxHashMap::default();
    for f in &cpu_b.functions {
        func_map_b
            .entry((&f.function_name, &f.url))
            .or_insert(f);
    }

    let mut all_funcs: Vec<(&str, &str)> = func_map_a
        .keys()
        .chain(func_map_b.keys())
        .copied()
        .collect();
    all_funcs.sort();
    all_funcs.dedup();

    let mut cpu_diff: Vec<CpuFunctionDiff> = all_funcs
        .into_iter()
        .map(|(name, url)| {
            let fa = func_map_a.get(&(name, url));
            let fb = func_map_b.get(&(name, url));
            let time_a = fa.map(|f| f.self_time_us).unwrap_or(0.0);
            let time_b = fb.map(|f| f.self_time_us).unwrap_or(0.0);
            let pct_a = if total_a > 0.0 { time_a / total_a * 100.0 } else { 0.0 };
            let pct_b = if total_b > 0.0 { time_b / total_b * 100.0 } else { 0.0 };
            let source = fa
                .map(|f| f.source_type.clone())
                .or_else(|| fb.map(|f| f.source_type.clone()))
                .unwrap_or(SourceType::Native);
            CpuFunctionDiff {
                function_name: name.to_string(),
                url: url.to_string(),
                source_type: source,
                time_a_us: time_a,
                time_b_us: time_b,
                pct_a,
                pct_b,
                diff_pct: pct_diff(time_a, time_b),
            }
        })
        .filter(|d| d.time_a_us > 0.0 || d.time_b_us > 0.0)
        .collect();

    // Sort by absolute diff in percentage points (most impactful first)
    cpu_diff.sort_by(|a, b| {
        (b.pct_b - b.pct_a)
            .abs()
            .partial_cmp(&(a.pct_b - a.pct_a).abs())
            .unwrap()
    });
    cpu_diff.truncate(30); // top 30

    // ── Findings ──
    let mut findings = Vec::new();

    // Long task comparison
    let lt_diff = pct_diff(
        summary_a.long_task_count as f64,
        summary_b.long_task_count as f64,
    );
    if lt_diff.abs() > 10.0 {
        findings.push(Finding {
            severity: if lt_diff < 0.0 {
                FindingSeverity::Improved
            } else {
                FindingSeverity::Regressed
            },
            category: "Long Tasks".to_string(),
            message: format!(
                "{} -> {} ({:+.0}%)",
                summary_a.long_task_count, summary_b.long_task_count, lt_diff
            ),
            detail: if lt_diff < 0.0 {
                "Fewer long tasks blocking the main thread".to_string()
            } else {
                "More long tasks blocking the main thread".to_string()
            },
        });
    }

    // Scroll frame duration
    if let (Some(sa), Some(sb)) = (&scroll_avg_a, &scroll_avg_b) {
        let dur_diff = pct_diff(sa.dur_us, sb.dur_us);
        if dur_diff.abs() > 5.0 {
            findings.push(Finding {
                severity: if dur_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Scroll Duration".to_string(),
                message: format!("{:+.1}% avg scroll task time", dur_diff),
                detail: format!(
                    "Bottleneck: {} -> {}",
                    sa.bottleneck(),
                    sb.bottleneck()
                ),
            });
        }

        // JS time
        let js_diff = pct_diff(sa.js_us, sb.js_us);
        if js_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if js_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "JS in Scroll".to_string(),
                message: format!("{:+.1}% JS execution time", js_diff),
                detail: String::new(),
            });
        }

        // Style/ULT time
        let ult_diff = pct_diff(sa.ult_us, sb.ult_us);
        if ult_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if ult_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Style Recalc".to_string(),
                message: format!("{:+.1}% UpdateLayoutTree time", ult_diff),
                detail: String::new(),
            });
        }

        // Layout time
        let layout_diff = pct_diff(sa.layout_us, sb.layout_us);
        if layout_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if layout_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Layout".to_string(),
                message: format!("{:+.1}% layout time", layout_diff),
                detail: String::new(),
            });
        }

        // Paint time
        let paint_diff = pct_diff(sa.paint_us, sb.paint_us);
        if paint_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if paint_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Paint".to_string(),
                message: format!("{:+.1}% paint time", paint_diff),
                detail: String::new(),
            });
        }

        // HitTest time
        let hit_diff = pct_diff(sa.hit_test_us, sb.hit_test_us);
        if hit_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if hit_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "HitTest".to_string(),
                message: format!("{:+.1}% hit test time", hit_diff),
                detail: String::new(),
            });
        }

        // Composite time
        let comp_diff = pct_diff(sa.composite_us, sb.composite_us);
        if comp_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if comp_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Composite".to_string(),
                message: format!("{:+.1}% composite time", comp_diff),
                detail: String::new(),
            });
        }
    }

    // Main thread busy
    let busy_diff = pct_diff(summary_a.main_thread_busy_us, summary_b.main_thread_busy_us);
    if busy_diff.abs() > 10.0 {
        findings.push(Finding {
            severity: if busy_diff < 0.0 {
                FindingSeverity::Improved
            } else {
                FindingSeverity::Regressed
            },
            category: "Main Thread".to_string(),
            message: format!("{:+.1}% total busy time", busy_diff),
            detail: String::new(),
        });
    }

    // Layout dirty
    let dirty_diff = pct_diff(layout_a.avg_dirty, layout_b.avg_dirty);
    if dirty_diff.abs() > 15.0 {
        findings.push(Finding {
            severity: if dirty_diff < 0.0 {
                FindingSeverity::Improved
            } else {
                FindingSeverity::Regressed
            },
            category: "Layout Dirty".to_string(),
            message: format!(
                "Avg dirty {:.0} -> {:.0} ({:+.1}%)",
                layout_a.avg_dirty, layout_b.avg_dirty, dirty_diff
            ),
            detail: String::new(),
        });
    }

    // Style recalc elements
    if style_recalc_a.total_count > 0 && style_recalc_b.total_count > 0 {
        let elem_diff = pct_diff(style_recalc_a.avg_elements, style_recalc_b.avg_elements);
        if elem_diff.abs() > 10.0 {
            findings.push(Finding {
                severity: if elem_diff < 0.0 {
                    FindingSeverity::Improved
                } else {
                    FindingSeverity::Regressed
                },
                category: "Style Elements".to_string(),
                message: format!(
                    "Avg {:.0} -> {:.0} ({:+.1}%)",
                    style_recalc_a.avg_elements, style_recalc_b.avg_elements, elem_diff
                ),
                detail: format!(
                    "Max {} -> {}",
                    style_recalc_a.max_elements, style_recalc_b.max_elements
                ),
            });
        }
    }

    // Sort findings: regressions first, then improvements
    findings.sort_by(|a, b| {
        let ord_a = match a.severity {
            FindingSeverity::Regressed => 0,
            FindingSeverity::Improved => 1,
            FindingSeverity::Neutral => 2,
        };
        let ord_b = match b.severity {
            FindingSeverity::Regressed => 0,
            FindingSeverity::Improved => 1,
            FindingSeverity::Neutral => 2,
        };
        ord_a.cmp(&ord_b)
    });

    CompareResult {
        rows,
        scroll_avg_a,
        scroll_avg_b,
        scroll_pct_a: scroll_a.percentiles.clone(),
        scroll_pct_b: scroll_b.percentiles.clone(),
        scroll_count_a: scroll_a.tasks.len(),
        scroll_count_b: scroll_b.tasks.len(),
        summary_a: summary_a.clone(),
        summary_b: summary_b.clone(),
        cpu_diff,
        layout_a: layout_a.clone(),
        layout_b: layout_b.clone(),
        style_recalc_a: style_recalc_a.clone(),
        style_recalc_b: style_recalc_b.clone(),
        findings,
    }
}
