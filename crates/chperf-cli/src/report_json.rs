//! Machine-readable full report JSON (no inspect query + `--json`).
//! Durations carry `*_us` (raw microseconds) plus `*_ms` mirrors, matching
//! inspect JSON conventions.

use crate::app::App;
use chperf_core::analysis::FindingSeverity;
use chperf_core::{inspect, trace};
use serde_json::{json, Value};

fn ms(us: f64) -> f64 {
    (us / 1000.0 * 100.0).round() / 100.0
}

/// Full-report JSON value. Top-level shape (stable):
/// trace, trace_health, summary, frame_blocking_tasks, cpu_profile,
/// jank, loading, network, raster, renderers, memory?, compare?.
pub fn full_report_json(app: &App, events: &[trace::TraceEvent], min_ts: f64) -> Value {
    let scope = inspect::Scope {
        window: None,
        tid: None,
        pid: None,
        cat: None,
    };
    // Memory section: skip the full sweep (it materializes every sample)
    // when the trace holds no UpdateCounters data; the empty shape below
    // is exactly what memory_section returns for zero samples.
    let mem_json = if inspect::has_memory_samples(events) {
        inspect::memory_section(events, &scope, usize::MAX, min_ts).1
    } else {
        json!({"summary": {"samples": 0}, "samples": []})
    };
    let has_memory = mem_json
        .get("summary")
        .and_then(|s| s.get("samples"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        > 0;

    let mut obj = serde_json::Map::with_capacity(11);
    obj.insert("trace".into(), json!(app.trace_name_a));
    obj.insert(
        "trace_health".into(),
        json!({
            "parsed": app.trace_stats.parsed,
            "skipped": app.trace_stats.skipped,
            "malformed": app.trace_stats.malformed,
            "main_tid": app.main_tid,
            "thread_count": app.thread_count,
            "span_us": app.trace_span_us.round(),
            "span_ms": ms(app.trace_span_us),
            "frame_budget_us": app.frame_budget_us.round(),
            "frame_budget_ms": ms(app.frame_budget_us),
            "frame_budget": inspect::frame_budget_label(app.frame_budget_us),
            "renderers": app.thread_model.renderers.iter().map(|r| json!({
                "pid": r.pid, "tid": r.tid, "name": r.name,
            })).collect::<Vec<_>>(),
            "browser_main_tid": app.thread_model.browser_main,
            "compositor_tids": app.thread_model.compositor,
            "cpu_throttle": app.metadata.as_ref().and_then(|m| m.cpu_throttling),
            "network_throttle": app.metadata.as_ref().and_then(|m| m.network_throttling.clone()),
        }),
    );
    obj.insert(
        "summary".into(),
        json!({
            "duration_us": app.summary.total_trace_duration_us.round(),
            "duration_ms": ms(app.summary.total_trace_duration_us),
            "main_busy_us": app.summary.main_thread_busy_us.round(),
            "main_busy_ms": ms(app.summary.main_thread_busy_us),
            "tbt_us": app.summary.total_blocking_time_us.round(),
            "tbt_ms": ms(app.summary.total_blocking_time_us),
            "long_task_count": app.summary.long_task_count,
            "long_tasks_total_us": app.summary.long_tasks_total_us.round(),
            "long_tasks_total_ms": ms(app.summary.long_tasks_total_us),
            "long_tasks_top_us": app.summary.long_tasks_top.iter().map(|v| v.round()).collect::<Vec<_>>(),
            "event_stats": app.summary.event_stats.iter().map(|s| json!({
                "name": s.name,
                "total_us": s.total_time_us.round(), "total_ms": ms(s.total_time_us),
                "self_us": s.self_time_us.round(), "self_ms": ms(s.self_time_us),
                "count": s.count, "avg_us": s.avg_time_us.round(),
                "avg_self_us": s.avg_self_us.round(),
                "pct_of_trace": s.pct_of_trace, "self_pct_of_trace": s.self_pct_of_trace,
            })).collect::<Vec<_>>(),
        }),
    );
    obj.insert(
        "frame_blocking_tasks".into(),
        json!({
            "task_count": app.scroll_frames.tasks.len(),
            "heuristic_fallback": app.scroll_frames.heuristic_fallback,
            "avg_us": app.scroll_frames.avg.dur_us.round(),
            "p50_us": app.scroll_frames.percentiles.p50_us.round(),
            "p90_us": app.scroll_frames.percentiles.p90_us.round(),
            "p99_us": app.scroll_frames.percentiles.p99_us.round(),
            "tasks": app.scroll_frames.tasks.iter().take(30).map(|t| json!({
                "dur_us": t.dur_us.round(), "dur_ms": ms(t.dur_us),
                "js_us": t.js_us.round(), "style_us": t.ult_us.round(),
                "layout_us": t.layout_us.round(), "paint_us": t.paint_us.round(),
                "composite_us": t.composite_us.round(), "hit_test_us": t.hit_test_us.round(),
                "over_50ms": t.over_50ms, "bottleneck": t.bottleneck(),
            })).collect::<Vec<_>>(),
        }),
    );
    obj.insert(
        "cpu_profile".into(),
        json!({
            "total_us": app.cpu_profile.total_sample_time_us.round(),
            "app_us": app.cpu_profile.app_time_us.round(),
            "runtime_us": app.cpu_profile.runtime_time_us.round(),
            "native_us": app.cpu_profile.native_time_us.round(),
            "functions": app.cpu_profile.functions.iter().take(30).map(|f| json!({
                "function": f.function_name, "url": f.url,
                "source": f.source_type.label(),
                "self_us": f.self_time_us.round(), "self_ms": ms(f.self_time_us),
            })).collect::<Vec<_>>(),
        }),
    );
    obj.insert(
        "jank".into(),
        json!({
            "total_dropped": app.jank.total_dropped,
            "bucket_ms": app.jank.bucket_ms,
            "frame_budget_us": app.frame_budget_us.round(),
            "clusters": app.jank.clusters.iter().map(|c| json!({
                "start_us": c.start_us.round(), "span_us": (c.end_us - c.start_us).round(),
                "busy_us": c.busy_us.round(), "max_run_us": c.max_run_us.round(),
                "max_faf_us": c.max_faf_us.round(), "max_gpu_us": c.max_gpu_us.round(),
                "dropped_frames": c.dropped_frames,
                "top_calls": c.top_calls.iter().map(|(n, d)| json!({"name": n, "dur_us": d.round()})).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }),
    );
    obj.insert(
        "loading".into(),
        json!({
            "navigation_start_us": app.loading.navigation_start_us.map(|v| v.round()),
            "fcp_us": app.loading.first_contentful_paint_us.map(|v| v.round()),
            "lcp_us": app.loading.largest_contentful_paint_us.map(|v| v.round()),
            "entries": app.loading.entries.iter().map(|e| json!({
                "name": e.name, "ts_us": e.ts_us.map(|v| v.round()),
                "rel_us": e.rel_us.map(|v| v.round()), "dur_us": e.dur_us.map(|v| v.round()),
            })).collect::<Vec<_>>(),
            "lcp": app.loading.lcp.iter().map(|c| json!({
                "t_us": c.ts_us.round(), "rel_us": c.rel_us.map(|v| v.round()),
                "size": c.size, "node": c.node, "url": c.url,
            })).collect::<Vec<_>>(),
            "user_timing": app.loading.user_timing.iter().map(|u| json!({
                "name": u.name, "t_us": u.ts_us.round(), "dur_us": u.dur_us.map(|v| v.round()),
            })).collect::<Vec<_>>(),
            "official": app.official_long_tasks.iter().map(|l| json!({
                "name": l.name, "t_us": l.ts_us.round(),
                "dur_us": l.dur_us.map(|v| v.round()), "interaction_type": l.interaction_type,
            })).collect::<Vec<_>>(),
        }),
    );
    obj.insert(
        "network".into(),
        json!({
            "request_count": app.network.request_count,
            "cache_hits": app.network.cache_hits,
            "render_blocking_count": app.network.render_blocking_count,
            "total_wall_us": app.network.total_wall_us.round(),
            "total_wall_ms": ms(app.network.total_wall_us),
            "incomplete": app.network.incomplete,
            "requests": app.network.requests.iter().take(20).map(|q| json!({
                "request_id": q.request_id, "url": q.url,
                "start_us": q.start_us.map(|v| v.round()),
                "end_us": q.end_us.map(|v| v.round()),
                "total_us": q.total_us.map(|v| v.round()),
                "total_ms": q.total_us.map(ms),
                "from_cache": q.from_cache, "render_blocking": q.render_blocking,
                "priority": q.priority, "is_xhr": q.is_xhr,
            })).collect::<Vec<_>>(),
        }),
    );
    obj.insert(
        "raster".into(),
        json!({
            "total_raster_us": app.raster.total_raster_us.round(),
            "total_decode_us": app.raster.total_decode_us.round(),
            "total_composite_us": app.raster.total_composite_us.round(),
            "gpu_task_us": app.raster.gpu_task_us.round(),
            "max_decode_us": app.raster.max_decode_us.round(),
            "lanes": app.raster.lanes.iter().take(20).map(|l| json!({
                "tid": l.tid, "thread": l.thread_name, "name": l.name,
                "count": l.count, "total_us": l.total_us.round(), "max_us": l.max_us.round(),
            })).collect::<Vec<_>>(),
            "decode_hotspots": app.raster.decode_hotspots.iter().take(10).map(|h| json!({
                "t_us": h.ts_us.round(), "dur_us": h.dur_us.round(), "source": h.url_or_node,
            })).collect::<Vec<_>>(),
        }),
    );
    obj.insert(
        "renderers".into(),
        json!(app
            .thread_model
            .renderers
            .iter()
            .map(|r| {
                let sum = app
                    .renderer_summaries
                    .iter()
                    .find(|(x, _)| x.tid == r.tid)
                    .map(|(_, s)| s);
                let jk = app
                    .renderer_janks
                    .iter()
                    .find(|(x, _)| x.tid == r.tid)
                    .map(|(_, j)| j);
                json!({
                    "pid": r.pid, "tid": r.tid, "name": r.name,
                    "is_primary": r.tid == app.thread_model.primary.tid,
                    "main_busy_us": sum.map(|s| s.main_thread_busy_us.round()),
                    "runtask_total_us": sum.map(|s| s.runtask_total_us.round()),
                    "long_task_count": sum.map(|s| s.long_task_count),
                    "long_tasks_total_us": sum.map(|s| s.long_tasks_total_us.round()),
                    "dropped_frames": jk.map(|j| j.total_dropped),
                    "jank_clusters": jk.map(|j| j.clusters.len()),
                })
            })
            .collect::<Vec<_>>()),
    );
    if has_memory {
        obj.insert("memory".into(), mem_json);
    }
    if let Some(ref cmp) = app.compare {
        obj.insert(
            "compare".into(),
            json!({
                "findings": cmp.findings.iter().map(|f| json!({
                    "severity": match &f.severity {
                        FindingSeverity::Improved => "Improved",
                        FindingSeverity::Regressed => "Regressed",
                        FindingSeverity::Neutral => "Neutral",
                    },
                    "category": f.category, "message": f.message, "detail": f.detail,
                })).collect::<Vec<_>>(),
            }),
        );
    }
    Value::Object(obj)
}
