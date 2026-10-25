//! Self-contained, offline HTML report: one file with inline CSS and SVG
//! charts (no external assets, no JS). Renders the full analysis plus the
//! memory / input / async sections for one or two (compared) traces.

use crate::app::App;
use chperf_core::analysis::*;
use chperf_core::{inspect, trace};
use serde_json::Value;

// ── Formatting helpers ──

fn esc(s: &str) -> String {
    // Single pass: the chained-replace version scanned + reallocated 4x.
    // Byte-identical: replacements introduce no escapable chars ('&' is
    // emitted only as part of an entity, after the '&' arm already ran —
    // same as replacing '&' first), and '"' is only special in attributes,
    // escaped uniformly here as before.
    if !s.bytes().any(|b| matches!(b, b'&' | b'<' | b'>' | b'"')) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn dur(us: f64) -> String {
    if us.abs() < 0.01 {
        "0".to_string()
    } else if us.abs() >= 1_000_000.0 {
        format!("{:.2}s", us / 1_000_000.0)
    } else if us.abs() >= 1_000.0 {
        format!("{:.2}ms", us / 1_000.0)
    } else {
        format!("{:.0}us", us)
    }
}

/// JSON field accessors (section JSON is authoritative for memory/input/async).
fn jf(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0)
}
fn ji(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}
fn js<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

// ── HTML building blocks ──

fn table(out: &mut String, headers: &[&str], rows: Vec<Vec<String>>) {
    out.push_str("<table><thead><tr>");
    for h in headers {
        out.push_str(&format!("<th>{}</th>", esc(h)));
    }
    out.push_str("</tr></thead><tbody>");
    for row in &rows {
        out.push_str("<tr>");
        for (i, cell) in row.iter().enumerate() {
            let cls = if i > 0 { " class=\"num\"" } else { "" };
            out.push_str(&format!("<td{}>{}</td>", cls, cell));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
}

fn section(out: &mut String, title: &str) {
    out.push_str(&format!(
        "<section class=\"section\"><h2>{}</h2>",
        esc(title)
    ));
}

fn metric(out: &mut String, label: &str, value: &str, sub: &str) {
    out.push_str("<div class=\"metric\">");
    out.push_str(&format!("<div class=\"metric-value\">{}</div>", value));
    out.push_str(&format!("<div class=\"metric-label\">{}</div>", esc(label)));
    if !sub.is_empty() {
        out.push_str(&format!("<div class=\"metric-sub\">{}</div>", esc(sub)));
    }
    out.push_str("</div>");
}

fn change_cell_fmt(a: f64, b: f64, fmt: &dyn Fn(f64) -> String) -> String {
    let s = chperf_core::analysis::fmt_change(a, b, fmt);
    if s.contains('→') {
        return format!("<span class=\"pos\">{}</span>", esc(&s));
    }
    let v: f64 = s
        .trim_end_matches('%')
        .trim_start_matches('+')
        .parse()
        .unwrap_or(0.0);
    if s.starts_with('+') && v > 0.5 {
        format!("<span class=\"neg\">{}</span>", esc(&s))
    } else if s.starts_with('-') && v < -5.0 {
        format!("<span class=\"pos\">{}</span>", esc(&s))
    } else {
        format!("<span class=\"muted\">{}</span>", esc(&s))
    }
}

fn change_cell_us(a: f64, b: f64) -> String {
    change_cell_fmt(a, b, &dur)
}

fn change_cell_n(a: f64, b: f64) -> String {
    change_cell_fmt(a, b, &|v| format!("{:.0}", v))
}

// ── SVG line chart ──

fn decimate(points: &[(f64, f64)], max_n: usize) -> Vec<(f64, f64)> {
    if points.len() <= max_n {
        return points.to_vec();
    }
    let denom = (max_n - 1).max(1);
    (0..max_n)
        .map(|k| points[k * (points.len() - 1) / denom])
        .collect()
}

/// Line chart over (x_ms, y). Renders gridlines, y ticks and a polyline.
fn line_chart(title: &str, points: &[(f64, f64)], y_fmt: &str, color: &str) -> String {
    const W: f64 = 1000.0;
    const H: f64 = 260.0;
    const PL: f64 = 64.0;
    const PR: f64 = 16.0;
    const PT: f64 = 14.0;
    const PB: f64 = 30.0;
    let mut s = String::new();
    s.push_str(&format!("<div class=\"chart-title\">{}</div>", esc(title)));
    if points.len() < 2 {
        return format!("{s}<div class=\"muted\">No data.</div>");
    }
    let pts = decimate(points, 700);
    let x0 = pts[0].0;
    let x1 = pts[pts.len() - 1].0;
    let ymax_raw = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    let ymin_raw = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let span = (ymax_raw - ymin_raw).max(1e-9);
    let ymin = ymin_raw - span * 0.05;
    let ymax = ymax_raw + span * 0.05;

    let sx = |x: f64| PL + (x - x0) / (x1 - x0).max(1e-9) * (W - PL - PR);
    let sy = |y: f64| PT + (ymax - y) / (ymax - ymin) * (H - PT - PB);

    s.push_str(&format!(
        "<svg class=\"chart\" viewBox=\"0 0 {:.0} {:.0}\" preserveAspectRatio=\"xMidYMid meet\">",
        W, H
    ));
    for i in 0..=4 {
        let y = ymax - (ymax - ymin) * i as f64 / 4.0;
        let yy = sy(y);
        s.push_str(&format!(
            "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" class=\"grid\"/>",
            PL,
            yy,
            W - PR,
            yy
        ));
        let label = format!("{:.1}{}", y, y_fmt);
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" class=\"axis\">{}</text>",
            PL - 8.0,
            yy + 4.0,
            label
        ));
    }
    // x-axis time labels (min / max), auto ms→s.
    let x_unit = if x1 - x0 >= 2000.0 { 1000.0 } else { 1.0 };
    let xu = if x_unit > 1.0 { "s" } else { "ms" };
    s.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.0}\" class=\"axis\">{:.1}{}</text>",
        PL,
        H - 8.0,
        x0 / x_unit,
        xu
    ));
    s.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.0}\" class=\"axis\" text-anchor=\"end\">{:.1}{}</text>",
        W - PR,
        H - 8.0,
        x1 / x_unit,
        xu
    ));

    // Area + line.
    let mut d = String::new();
    for (i, (x, y)) in pts.iter().enumerate() {
        if i == 0 {
            d.push_str(&format!("M{:.2} {:.2}", sx(*x), sy(*y)));
        } else {
            d.push_str(&format!(" L{:.2} {:.2}", sx(*x), sy(*y)));
        }
    }
    let area = format!(
        "{} L{:.2} {:.2} L{:.2} {:.2} Z",
        d,
        sx(pts[pts.len() - 1].0),
        sy(ymin),
        sx(pts[0].0),
        sy(ymin)
    );
    s.push_str(&format!(
        "<path d=\"{}\" class=\"area\" style=\"fill:{}\"/>",
        area, color
    ));
    s.push_str(&format!(
        "<path d=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"1.8\"/>",
        d, color
    ));
    s.push_str("</svg>");
    s
}

// ── Data extraction (reuses the inspect section JSON) ──

fn full_scope() -> inspect::Scope {
    inspect::Scope {
        window: None,
        tid: None,
        pid: None,
        cat: None,
    }
}

// ── Main entry ──

pub fn export_html(
    app: &App,
    events_a: &[trace::TraceEvent],
    min_ts_a: f64,
    b: Option<(&App, &[trace::TraceEvent], f64)>,
) -> String {
    // Full report is ~90KB on large traces; reserve up front so the
    // section renderers below append without repeated regrowth.
    let mut o = String::with_capacity(96 * 1024);
    o.push_str("<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">");
    o.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    let title = match &b {
        Some((app_b, _, _)) => format!("{} vs {}", app.trace_name_a, app_b.trace_name_a),
        None => app.trace_name_a.clone(),
    };
    o.push_str(&format!("<title>chperf: {}</title>", esc(&title)));
    o.push_str(CSS);
    o.push_str("</head><body><div class=\"container\">");

    // ── Header ──
    match &b {
        Some((app_b, _, _)) => o.push_str(&format!(
            "<h1>Chrome Trace Analysis: <span class=\"muted\">{}</span> vs <span class=\"muted\">{}</span></h1>",
            esc(&app.trace_name_a),
            esc(&app_b.trace_name_a),
        )),
        None => o.push_str(&format!(
            "<h1>Chrome Trace Analysis: <span class=\"muted\">{}</span></h1>",
            esc(&app.trace_name_a)
        )),
    }
    if let Some(ref meta) = app.metadata {
        if let Some(ref url) = meta.page_url {
            o.push_str(&format!(
                "<div class=\"meta\">URL: <code>{}</code></div>",
                esc(url)
            ));
        }
        if let Some(ref start) = meta.start_time {
            o.push_str(&format!(
                "<div class=\"meta\">Recorded: {}</div>",
                esc(start)
            ));
        }
        if let Some(dpr) = meta.host_dpr {
            o.push_str(&format!("<div class=\"meta\">DPR: {:.2}</div>", dpr));
        }
    }
    if app.throttle_factor > 1.0 {
        o.push_str(&format!(
            "<div class=\"meta\">CPU throttle {:.0}x applied — divide times by {:.0} for real-world.</div>",
            app.throttle_factor, app.throttle_factor
        ));
    }

    // Compare mode: findings at top, shared A/B header, then the paired
    // per-section bodies (each reuses the single-trace renderer), then the
    // detailed compare tables.
    if let Some((app_b, events_b, min_ts_b)) = b {
        if let Some(cmp) = &app.compare {
            render_findings_top(&mut o, cmp);
        }
        render_compare_header(&mut o, app, app_b);
        o.push_str(&format!("<h2>Trace A: {}</h2>", esc(&app.trace_name_a)));
        render_trace_body(&mut o, app, events_a, min_ts_a);
        o.push_str(&format!("<h2>Trace B: {}</h2>", esc(&app_b.trace_name_a)));
        render_trace_body(&mut o, app_b, events_b, min_ts_b);
        render_compare_detail(&mut o, app, events_a, min_ts_a, events_b, min_ts_b);
        o.push_str("<footer>Generated by <code>chperf</code></footer>");
        o.push_str("</div></body></html>");
        return o;
    }

    render_trace_body(&mut o, app, events_a, min_ts_a);

    o.push_str("<footer>Generated by <code>chperf</code></footer>");
    o.push_str("</div></body></html>");
    o
}

/// Single-trace report body: health, overview cards, and every analysis
/// section. Shared by the single-trace report and each half of a compare
/// report, so both halves reuse the same per-section renderers.
fn render_trace_body(o: &mut String, app: &App, events_a: &[trace::TraceEvent], min_ts_a: f64) {
    // ── Trace health (always first) ──
    section(o, "Trace health");
    let renderers_line = {
        let mut rs = app.thread_model.renderers.clone();
        rs.sort_by_key(|r| (r.pid, r.tid));
        if rs.is_empty() {
            "0 pids".to_string()
        } else {
            format!(
                "{} pid{} ({})",
                rs.len(),
                if rs.len() == 1 { "" } else { "s" },
                rs.iter()
                    .map(|r| format!("{}:tid {}", r.pid, r.tid))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    };
    let browser_main = app
        .thread_model
        .browser_main
        .map(|t| format!("tid {}", t))
        .unwrap_or_else(|| "none".to_string());
    let compositor = if app.thread_model.compositor.is_empty() {
        "none".to_string()
    } else {
        format!(
            "tids {}",
            app.thread_model
                .compositor
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    o.push_str(&format!(
        "<p class=\"muted\">Events: {} parsed / {} skipped / {} malformed · main tid {} · {} threads · span {} · frame budget {} · CPU throttle {} · network {} · renderers {} · browser main {} · compositor {}</p>",
        app.trace_stats.parsed,
        app.trace_stats.skipped,
        app.trace_stats.malformed,
        app.main_tid,
        app.thread_count,
        dur(app.trace_span_us),
        match app.frame_budget_us.round() as i64 {
            8333 => "8.33ms (120Hz detected)",
            11111 => "11.11ms (90Hz detected)",
            16667 => "16.67ms (60Hz detected)",
            _ => "custom",
        },
        app.metadata.as_ref().and_then(|m| m.cpu_throttling).map(|v| format!("{:.0}x", v)).unwrap_or_else(|| "1x".to_string()),
        app.metadata.as_ref().and_then(|m| m.network_throttling.clone()).unwrap_or_else(|| "—".to_string()),
        esc(&renderers_line),
        esc(&browser_main),
        esc(&compositor),
    ));
    o.push_str("</section>");

    // ── Renderers (multi-process / OOPIF) ──
    {
        let mut rs = app.thread_model.renderers.clone();
        rs.sort_by_key(|r| (r.pid, r.tid));
        section(o, "Renderers");
        if rs.len() <= 1 {
            let who = rs
                .first()
                .map(|r| format!(" (pid {} tid {})", r.pid, r.tid))
                .unwrap_or_default();
            o.push_str(&format!(
                "<p class=\"muted\">Renderers: 1 (single-process trace){}.</p>",
                esc(&who)
            ));
            o.push_str("</section>");
        } else {
            o.push_str(&format!(
                "<p class=\"muted\">Primary renderer (tid {}) feeds the main report; other renderers' numbers are secondary (OOPIF/iframe/workers).</p>",
                app.thread_model.primary.tid
            ));
            let rows = rs
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
                    let name = if r.name.is_empty() { "—" } else { &r.name };
                    vec![
                        format!("{}", r.pid),
                        format!("{}", r.tid),
                        esc(name),
                        sum.map(|s| dur(s.main_thread_busy_us))
                            .unwrap_or_else(|| "—".to_string()),
                        sum.map(|s| dur(s.runtask_total_us))
                            .unwrap_or_else(|| "—".to_string()),
                        sum.map(|s| format!("{}", s.long_task_count))
                            .unwrap_or_else(|| "—".to_string()),
                        jk.map(|j| format!("{}", j.total_dropped))
                            .unwrap_or_else(|| "—".to_string()),
                        jk.map(|j| format!("{}", j.clusters.len()))
                            .unwrap_or_else(|| "—".to_string()),
                        if r.tid == app.thread_model.primary.tid {
                            "yes".to_string()
                        } else {
                            "no".to_string()
                        },
                    ]
                })
                .collect();
            table(
                o,
                &[
                    "pid",
                    "tid",
                    "thread",
                    "main busy",
                    "RunTask total",
                    "long tasks",
                    "dropped",
                    "clusters",
                    "primary",
                ],
                rows,
            );
            o.push_str("</section>");
        }
    }

    // ── Overview cards ──
    o.push_str("<div class=\"metrics\">");
    metric(o, "Duration", &dur(app.summary.total_trace_duration_us), "");
    let busy = if app.summary.total_trace_duration_us > 0.0 {
        app.summary.main_thread_busy_us / app.summary.total_trace_duration_us * 100.0
    } else {
        0.0
    };
    metric(
        o,
        "Main thread busy",
        &format!("{:.1}%", busy),
        &dur(app.summary.main_thread_busy_us),
    );
    metric(
        o,
        "Long tasks (>50ms)",
        &format!("{}", app.summary.long_task_count),
        "",
    );
    metric(
        o,
        "Dropped frames",
        &format!("{}", app.jank.total_dropped),
        "",
    );
    o.push_str("</div>");

    // ── Memory ──
    let scope = full_scope();
    // Lean chart data (summary scalars + decimated-ready points): the full
    // section would also materialize the markdown table and 79K per-sample
    // JSON rows that HTML never reads.
    let mem = inspect::memory_chart_data(events_a, &scope, min_ts_a);
    let summary = &mem.summary;
    if jf(summary, "samples") > 0.0 {
        section(o, "Memory timeline");
        o.push_str("<div class=\"metrics\">");
        metric(
            o,
            "JS heap (start → end)",
            &format!("{:.1} MB", jf(summary, "first_heap_mb")),
            "",
        );
        metric(
            o,
            "Heap growth",
            &format!("{:+.1} MB", jf(summary, "growth_mb")),
            &format!("peak {:.1} MB", jf(summary, "peak_heap_mb")),
        );
        metric(
            o,
            "DOM nodes (peak)",
            &format!("{:.0}", jf(summary, "peak_nodes")),
            "",
        );
        metric(
            o,
            "Listeners / documents",
            &format!(
                "{:.0} / {:.0}",
                jf(summary, "peak_listeners"),
                jf(summary, "peak_documents")
            ),
            "",
        );
        o.push_str("</div>");
        o.push_str(&line_chart(
            "JS heap (MB)",
            &mem.heap,
            "",
            "#3b82f6",
        ));
        o.push_str(&line_chart(
            "DOM nodes",
            &mem.nodes,
            "",
            "#10b981",
        ));
        o.push_str("</section>");
    }

    // ── CPU top functions ──
    let cp = &app.cpu_profile;
    section(o, "CPU profile — top functions by self time");
    if cp.functions.is_empty() {
        o.push_str("<p class=\"muted\">none found (0 functions).</p>");
        o.push_str("</section>");
    } else {
        let total = cp.total_sample_time_us;
        let mut rows = Vec::new();
        let mut cum = 0.0;
        for f in cp.functions.iter().take(30) {
            let pct = if total > 0.0 {
                f.self_time_us / total * 100.0
            } else {
                0.0
            };
            cum += pct;
            let name = if f.function_name.is_empty() {
                "(anonymous)"
            } else {
                &f.function_name
            };
            let short = f.url.rfind('/').map(|i| &f.url[i + 1..]).unwrap_or(&f.url);
            rows.push(vec![
                format!("{:.1}%", pct),
                f.source_type.label().to_string(),
                esc(name),
                dur(f.self_time_us),
                format!("{:.1}%", cum),
                esc(short),
            ]);
        }
        table(
            o,
            &[
                "self % of sampled time",
                "source",
                "function",
                "self time",
                "cum % of sampled time",
                "file",
            ],
            rows,
        );
        o.push_str("</section>");
    }

    // ── Event breakdown (top 30) ──
    section(o, "Event breakdown");
    if app.summary.event_stats.is_empty() {
        o.push_str("<p class=\"muted\">none found (0 event types).</p>");
    } else {
        let rows = app
            .summary
            .event_stats
            .iter()
            .take(30)
            .map(|s| {
                vec![
                    esc(s.name),
                    format!("{}", s.count),
                    dur(s.total_time_us),
                    dur(s.self_time_us),
                    dur(s.avg_time_us),
                    format!("{:.1}%", s.pct_of_trace),
                    format!("{:.1}%", s.self_pct_of_trace),
                ]
            })
            .collect();
        table(
            o,
            &[
                "event",
                "count",
                "total",
                "self",
                "avg",
                "% of trace",
                "self % of trace",
            ],
            rows,
        );
        if app.summary.event_stats.len() > 30 {
            o.push_str(&format!(
                "<p class=\"muted\">{} more, use --events/--stats for the full list.</p>",
                app.summary.event_stats.len() - 30
            ));
        }
    }
    o.push_str("</section>");

    // ── Input latency ──
    let (_, inp) = inspect::input_section(events_a, &scope, 30, min_ts_a);
    if let Some(types) = inp["types"].as_array()
        && !types.is_empty()
    {
        section(o, "Input latency by type");
        let rows = types
            .iter()
            .map(|t| {
                vec![
                    esc(js(t, "type")),
                    ji(t, "count").to_string(),
                    dur(jf(t, "total_us")),
                    dur(jf(t, "avg_us")),
                    dur(jf(t, "p50_us")),
                    dur(jf(t, "p99_us")),
                    dur(jf(t, "max_us")),
                ]
            })
            .collect();
        table(
            o,
            &["type", "count", "total", "avg", "p50", "p99", "max"],
            rows,
        );
        if let Some(worst) = inp["worst"].as_array()
            && !worst.is_empty()
        {
            o.push_str("<h3>Worst inputs</h3>");
            let rows = worst
                .iter()
                .take(20)
                .map(|w| {
                    vec![
                        format!("{:.2}ms", jf(w, "t_us") / 1000.0),
                        dur(jf(w, "dur_us")),
                        esc(js(w, "type")),
                    ]
                })
                .collect();
            table(o, &["t", "duration", "type"], rows);
        }
        o.push_str("</section>");
    }

    // ── Async tasks ──
    let (_, asy) = inspect::async_section(events_a, &scope, 30, min_ts_a);
    if let Some(tasks) = asy["tasks"].as_array()
        && !tasks.is_empty()
    {
        section(o, "Async tasks (s/f paired)");
        let rows = tasks
            .iter()
            .map(|t| {
                vec![
                    esc(js(t, "name")),
                    ji(t, "count").to_string(),
                    dur(jf(t, "total_us")),
                    dur(jf(t, "avg_us")),
                    dur(jf(t, "p50_us")),
                    dur(jf(t, "p99_us")),
                    dur(jf(t, "max_us")),
                ]
            })
            .collect();
        table(
            o,
            &["name", "count", "total", "avg", "p50", "p99", "max"],
            rows,
        );
        if let Some(longest) = asy["longest"].as_array()
            && !longest.is_empty()
        {
            o.push_str("<h3>Longest tasks</h3>");
            let rows = longest
                .iter()
                .take(20)
                .map(|w| {
                    vec![
                        format!("{:.2}ms", jf(w, "t_us") / 1000.0),
                        dur(jf(w, "dur_us")),
                        esc(js(w, "name")),
                    ]
                })
                .collect();
            table(o, &["t", "duration", "name"], rows);
        }
        o.push_str("</section>");
    }

    // ── Jank clusters (explicit empty) ──
    section(o, "Jank clusters");
    {
        let budget_label = match app.frame_budget_us.round() as i64 {
            8333 => "8.33ms (120Hz detected)",
            11111 => "11.11ms (90Hz detected)",
            16667 => "16.67ms (60Hz detected)",
            _ => "custom",
        };
        o.push_str(&format!(
            "<p class=\"muted\">Bucket {}ms · frame budget {} — dropped frames and over-budget spikes below the Long Task threshold.</p>",
            app.jank.bucket_ms.round() as i64, budget_label
        ));
        if app.jank.clusters.is_empty() && app.jank.total_dropped == 0 {
            o.push_str("<p class=\"muted\">none found (0 clusters, 0 dropped frames).</p>");
            o.push_str("</section>");
        } else {
            let rows = app
                .jank
                .clusters
                .iter()
                .map(|c| {
                    let calls = c
                        .top_calls
                        .iter()
                        .map(|(n, d)| format!("{} ({})", n, dur(*d)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    vec![
                        dur(c.start_us),
                        dur(c.end_us - c.start_us),
                        dur(c.busy_us),
                        dur(c.max_run_us),
                        format!("{}", c.dropped_frames),
                        esc(&calls),
                    ]
                })
                .collect();
            table(
                o,
                &[
                    "start",
                    "span",
                    "busy",
                    "max RunTask",
                    "dropped",
                    "what happened",
                ],
                rows,
            );
            o.push_str("</section>");
        }
    }

    // ── Frame-blocking tasks (explicit empty) ──
    section(o, "Frame-blocking main-thread tasks");
    {
        let sf = &app.scroll_frames;
        if sf.heuristic_fallback {
            o.push_str("<p class=\"muted\">Heuristic fallback active: no scroll/input evidence in trace.</p>");
        }
        if sf.tasks.is_empty() {
            o.push_str("<p class=\"muted\">none found (0 frame-blocking tasks).</p>");
            o.push_str("</section>");
        } else {
            o.push_str("<div class=\"metrics\">");
            metric(
                o,
                "Frame-blocking tasks",
                &format!("{}", sf.tasks.len()),
                "",
            );
            metric(
                o,
                "Avg duration",
                &dur(sf.avg.dur_us),
                &format!("bottleneck: {}", sf.avg.bottleneck()),
            );
            metric(
                o,
                "P50 / P90 / P99",
                &format!(
                    "{} / {} / {}",
                    dur(sf.percentiles.p50_us),
                    dur(sf.percentiles.p90_us),
                    dur(sf.percentiles.p99_us)
                ),
                "",
            );
            o.push_str("</div>");
            let rows = sf
                .tasks
                .iter()
                .take(15)
                .map(|t| {
                    vec![
                        dur(t.dur_us),
                        t.bottleneck().to_string(),
                        dur(t.js_us),
                        dur(t.ult_us),
                        dur(t.layout_us),
                        dur(t.paint_us),
                    ]
                })
                .collect();
            table(
                o,
                &["duration", "bottleneck", "JS", "style", "layout", "paint"],
                rows,
            );
            o.push_str("</section>");
        }
    }

    // ── Loading ──
    section(o, "Loading");
    {
        let ld = &app.loading;
        o.push_str("<div class=\"metrics\">");
        metric(
            o,
            "FCP",
            &ld.first_contentful_paint_us
                .map(dur)
                .unwrap_or_else(|| "none".to_string()),
            "",
        );
        metric(
            o,
            "LCP",
            &ld.largest_contentful_paint_us
                .map(dur)
                .unwrap_or_else(|| "none".to_string()),
            "",
        );
        metric(o, "User timing", &format!("{}", ld.user_timing.len()), "");
        metric(o, "LCP candidates", &format!("{}", ld.lcp.len()), "");
        o.push_str("</div>");
        if ld.lcp.is_empty() {
            o.push_str("<p class=\"muted\">No LCP candidates found.</p>");
        } else {
            let rows = ld
                .lcp
                .iter()
                .take(10)
                .map(|c| {
                    vec![
                        dur(c.ts_us),
                        c.size
                            .map(|v| format!("{:.0}", v))
                            .unwrap_or_else(|| "—".to_string()),
                        esc(c.node.as_deref().unwrap_or("—")),
                        esc(c.url.as_deref().unwrap_or("—")),
                    ]
                })
                .collect();
            table(o, &["t", "size", "node", "url"], rows);
        }
        if !ld.user_timing.is_empty() {
            let rows = ld
                .user_timing
                .iter()
                .take(10)
                .map(|u| {
                    vec![
                        dur(u.ts_us),
                        u.dur_us.map(dur).unwrap_or_else(|| "—".to_string()),
                        esc(&u.name),
                    ]
                })
                .collect();
            table(o, &["t", "dur", "name"], rows);
        } else {
            o.push_str("<p class=\"muted\">No user timing entries found.</p>");
        }
        o.push_str("</section>");
    }

    // ── Network ──
    section(o, "Network");
    {
        let nw = &app.network;
        o.push_str("<div class=\"metrics\">");
        metric(o, "Requests", &format!("{}", nw.request_count), "");
        metric(o, "Cache hits", &format!("{}", nw.cache_hits), "");
        metric(
            o,
            "Render-blocking",
            &format!("{}", nw.render_blocking_count),
            "",
        );
        metric(o, "Total wall", &dur(nw.total_wall_us), "");
        o.push_str("</div>");
        if nw.requests.is_empty() {
            o.push_str("<p class=\"muted\">none found (0 requests).</p>");
        } else {
            let rows = nw
                .slowest(10)
                .iter()
                .map(|q| {
                    vec![
                        q.total_us.map(dur).unwrap_or_else(|| "—".to_string()),
                        esc(&q.url),
                        esc(q.priority.as_deref().unwrap_or("—")),
                        if q.from_cache {
                            "yes".to_string()
                        } else {
                            "no".to_string()
                        },
                    ]
                })
                .collect();
            table(o, &["total", "url", "priority", "cached"], rows);
        }
        o.push_str("</section>");
    }

    // ── Raster / Compositor ──
    section(o, "Raster / Compositor");
    {
        let rs = &app.raster;
        o.push_str("<div class=\"metrics\">");
        metric(o, "Raster", &dur(rs.total_raster_us), "");
        metric(
            o,
            "Decode",
            &dur(rs.total_decode_us),
            &format!("max {}", dur(rs.max_decode_us)),
        );
        metric(o, "Composite", &dur(rs.total_composite_us), "");
        metric(o, "GPU", &dur(rs.gpu_task_us), "");
        o.push_str("</div>");
        if rs.lanes.is_empty() {
            o.push_str("<p class=\"muted\">none found (0 lanes).</p>");
        } else {
            let rows = rs
                .lanes
                .iter()
                .take(15)
                .map(|l| {
                    vec![
                        format!("{}", l.tid),
                        esc(if l.thread_name.is_empty() {
                            "—"
                        } else {
                            &l.thread_name
                        }),
                        esc(&l.name),
                        format!("{}", l.count),
                        dur(l.total_us),
                        dur(l.max_us),
                    ]
                })
                .collect();
            table(
                o,
                &["tid", "thread", "event", "count", "total", "max"],
                rows,
            );
        }
        if rs.decode_hotspots.is_empty() {
            o.push_str("<p class=\"muted\">No decode hotspots found.</p>");
        } else {
            let rows = rs
                .decode_hotspots
                .iter()
                .take(10)
                .map(|h| {
                    vec![
                        dur(h.dur_us),
                        esc(if h.url_or_node.is_empty() {
                            "—"
                        } else {
                            &h.url_or_node
                        }),
                    ]
                })
                .collect();
            table(o, &["dur", "source"], rows);
        }
        o.push_str("</section>");
    }

    // ── Style / Reflow / Layout (explicit empty) ──
    section(o, "Style Recalc (UpdateLayoutTree)");
    {
        let sr = &app.style_recalc;
        if sr.entries.is_empty() {
            o.push_str("<p class=\"muted\">none found (0 events).</p>");
        } else {
            o.push_str(&format!(
                "<p class=\"muted\">{} events · avg {:.0} · max {}</p>",
                sr.total_count, sr.avg_elements, sr.max_elements
            ));
            let rows = sr
                .entries
                .iter()
                .take(10)
                .map(|e| vec![dur(e.dur_us), format!("{}", e.element_count)])
                .collect();
            table(o, &["duration", "elements"], rows);
        }
        o.push_str("</section>");
    }
    section(o, "Forced Reflow (Layout Thrashing)");
    {
        let fr = &app.forced_reflows;
        if fr.entries.is_empty() {
            o.push_str("<p class=\"muted\">none found (0 tasks with thrashing).</p>");
        } else {
            o.push_str(&format!(
                "<p class=\"muted\">{} tasks · {} reflows · {}</p>",
                fr.entries.len(),
                fr.total_reflows,
                dur(fr.total_layout_time_us)
            ));
            let rows = fr
                .entries
                .iter()
                .take(10)
                .map(|e| {
                    vec![
                        dur(e.task_dur_us),
                        format!("{}", e.reflow_count),
                        dur(e.layout_time_us),
                    ]
                })
                .collect();
            table(o, &["task", "reflows", "layout"], rows);
        }
        o.push_str("</section>");
    }
    section(o, "Layout Dirty Objects");
    {
        let ld = &app.layout_dirty;
        if ld.entries.is_empty() {
            o.push_str("<p class=\"muted\">none found (0 layout events).</p>");
        } else {
            o.push_str(&format!(
                "<p class=\"muted\">{} events · avg {:.0} · max {}</p>",
                ld.entries.len(),
                ld.avg_dirty,
                ld.max_dirty
            ));
            let rows = ld
                .entries
                .iter()
                .take(10)
                .map(|e| {
                    vec![
                        dur(e.dur_us),
                        format!("{}", e.dirty_count),
                        format!("{}", e.total_count),
                    ]
                })
                .collect();
            table(o, &["duration", "dirty", "total"], rows);
        }
        o.push_str("</section>");
    }

    // ── Official responsiveness (only if non-empty) ──
    if !app.official_long_tasks.is_empty() {
        section(o, "Official responsiveness signals (EventTiming)");
        let rows = app
            .official_long_tasks
            .iter()
            .take(20)
            .map(|l| {
                vec![
                    dur(l.ts_us),
                    l.dur_us.map(dur).unwrap_or_else(|| "—".to_string()),
                    esc(&l.name),
                    esc(if l.interaction_type.is_empty() {
                        "—"
                    } else {
                        &l.interaction_type
                    }),
                ]
            })
            .collect();
        table(o, &["t", "dur", "name", "interaction"], rows);
        o.push_str("</section>");
    }
}

/// Compare findings block (rendered at the top of a compare report). Same
/// category/message/detail text as the Markdown export, with plain-text
/// `[IMPROVED]` / `[REGRESSED]` / `[NEUTRAL]` markers instead of emoji.
fn render_findings_top(o: &mut String, cmp: &CompareResult) {
    if cmp.findings.is_empty() {
        return;
    }
    section(o, "Comparison findings");
    o.push_str("<ul class=\"findings\">");
    for f in cmp.findings.iter() {
        let (cls, label) = match f.severity {
            FindingSeverity::Improved => ("pos", "[IMPROVED]"),
            FindingSeverity::Regressed => ("neg", "[REGRESSED]"),
            FindingSeverity::Neutral => ("muted", "[NEUTRAL]"),
        };
        let detail = if f.detail.is_empty() {
            String::new()
        } else {
            format!(" ({})", f.detail)
        };
        o.push_str(&format!(
            "<li><span class=\"{cls}\">{label}</span> {}: {}{}</li>",
            esc(&f.category),
            esc(&f.message),
            esc(&detail),
        ));
    }
    o.push_str("</ul>");
    o.push_str("</section>");
}

fn budget_label_str(budget_us: f64) -> String {
    match budget_us.round() as i64 {
        8333 => "8.33ms (120Hz detected)".to_string(),
        11111 => "11.11ms (90Hz detected)".to_string(),
        16667 => "16.67ms (60Hz detected)".to_string(),
        _ => format!("{:.2}ms", budget_us / 1000.0),
    }
}

/// Shared A/B overview + health header for a compare report.
fn render_compare_header(o: &mut String, a: &App, b: &App) {
    section(o, "Overview: A vs B");
    let health = |app: &App| {
        format!(
            "{} parsed / {} skipped / {} malformed",
            app.trace_stats.parsed, app.trace_stats.skipped, app.trace_stats.malformed
        )
    };
    let thr = |app: &App| {
        app.metadata
            .as_ref()
            .and_then(|m| m.cpu_throttling)
            .map(|v| format!("{:.0}x", v))
            .unwrap_or_else(|| "1x".to_string())
    };
    let busy = |app: &App| {
        if app.summary.total_trace_duration_us > 0.0 {
            format!(
                "{:.1}% ({})",
                app.summary.main_thread_busy_us / app.summary.total_trace_duration_us * 100.0,
                dur(app.summary.main_thread_busy_us)
            )
        } else {
            "0.0%".to_string()
        }
    };
    let rows: Vec<Vec<String>> = vec![
        vec!["Trace".into(), esc(&a.trace_name_a), esc(&b.trace_name_a)],
        vec!["Events".into(), health(a), health(b)],
        vec![
            "Main thread / threads".into(),
            format!("tid={} · {} threads", a.main_tid, a.thread_count),
            format!("tid={} · {} threads", b.main_tid, b.thread_count),
        ],
        vec!["Span".into(), dur(a.trace_span_us), dur(b.trace_span_us)],
        vec![
            "Frame budget".into(),
            budget_label_str(a.frame_budget_us),
            budget_label_str(b.frame_budget_us),
        ],
        vec!["CPU throttle".into(), thr(a), thr(b)],
        vec![
            "Duration".into(),
            dur(a.summary.total_trace_duration_us),
            dur(b.summary.total_trace_duration_us),
        ],
        vec!["Main thread busy".into(), busy(a), busy(b)],
        vec![
            "Long tasks (>50ms)".into(),
            format!("{}", a.summary.long_task_count),
            format!("{}", b.summary.long_task_count),
        ],
        vec![
            "Dropped frames".into(),
            format!("{}", a.jank.total_dropped),
            format!("{}", b.jank.total_dropped),
        ],
    ];
    table(o, &["metric", "A", "B"], rows);
    o.push_str("</section>");
}

/// Detailed A-vs-B compare tables (quick stats, scroll, CPU diff, memory).
/// Findings render at the top (`render_findings_top`); the paired
/// per-section bodies render separately via `render_trace_body`.
fn render_compare_detail(
    o: &mut String,
    app: &App,
    events_a: &[trace::TraceEvent],
    min_ts_a: f64,
    events_b: &[trace::TraceEvent],
    min_ts_b: f64,
) {
    let Some(cmp) = &app.compare else {
        return;
    };
    {
        let name_b = app.trace_name_b.as_deref().unwrap_or("B");
        section(o, &format!("Comparison details: A vs {name_b}"));

        // Quick stats
        let sa = &cmp.summary_a;
        let sb = &cmp.summary_b;
        let mut rows: Vec<Vec<String>> = vec![
            vec![
                "Total Blocking Time (TBT)".into(),
                dur(sa.total_blocking_time_us),
                dur(sb.total_blocking_time_us),
                change_cell_us(sa.total_blocking_time_us, sb.total_blocking_time_us),
            ],
            vec![
                "Long tasks".into(),
                format!("{}", sa.long_task_count),
                format!("{}", sb.long_task_count),
                change_cell_n(sa.long_task_count as f64, sb.long_task_count as f64),
            ],
            vec![
                "Worst task".into(),
                dur(sa.long_tasks_top.first().copied().unwrap_or(0.0)),
                dur(sb.long_tasks_top.first().copied().unwrap_or(0.0)),
                change_cell_us(
                    sa.long_tasks_top.first().copied().unwrap_or(0.0),
                    sb.long_tasks_top.first().copied().unwrap_or(0.0),
                ),
            ],
            vec![
                "Dropped frames".into(),
                format!("{}", cmp.jank_a.total_dropped),
                format!("{}", cmp.jank_b.total_dropped),
                change_cell_n(
                    cmp.jank_a.total_dropped as f64,
                    cmp.jank_b.total_dropped as f64,
                ),
            ],
            vec![
                "Forced reflows".into(),
                format!("{}", cmp.forced_reflow_a.total_reflows),
                format!("{}", cmp.forced_reflow_b.total_reflows),
                change_cell_n(
                    cmp.forced_reflow_a.total_reflows as f64,
                    cmp.forced_reflow_b.total_reflows as f64,
                ),
            ],
            vec![
                "Main thread busy".into(),
                dur(sa.main_thread_busy_us),
                dur(sb.main_thread_busy_us),
                change_cell_us(sa.main_thread_busy_us, sb.main_thread_busy_us),
            ],
            vec![
                "Layout dirty (avg)".into(),
                format!("{:.0}", cmp.layout_a.avg_dirty),
                format!("{:.0}", cmp.layout_b.avg_dirty),
                change_cell_n(cmp.layout_a.avg_dirty, cmp.layout_b.avg_dirty),
            ],
        ];
        if cmp.style_recalc_a.total_count > 0 || cmp.style_recalc_b.total_count > 0 {
            rows.push(vec![
                "Style elements (avg)".into(),
                format!("{:.0}", cmp.style_recalc_a.avg_elements),
                format!("{:.0}", cmp.style_recalc_b.avg_elements),
                change_cell_n(
                    cmp.style_recalc_a.avg_elements,
                    cmp.style_recalc_b.avg_elements,
                ),
            ]);
        }
        table(o, &["metric", "A", "B", "change"], rows);

        // Scroll comparison
        if let (Some(avg_a), Some(avg_b)) = (&cmp.scroll_avg_a, &cmp.scroll_avg_b) {
            o.push_str("<h3>Scroll frame comparison (avg per task)</h3>");
            let cats = [
                ("Duration", avg_a.dur_us, avg_b.dur_us),
                ("JS", avg_a.js_us, avg_b.js_us),
                ("Style", avg_a.ult_us, avg_b.ult_us),
                ("Layout", avg_a.layout_us, avg_b.layout_us),
                ("Paint", avg_a.paint_us, avg_b.paint_us),
                ("Composite", avg_a.composite_us, avg_b.composite_us),
                ("HitTest", avg_a.hit_test_us, avg_b.hit_test_us),
            ];
            let rows = cats
                .iter()
                .map(|(n, a, b)| vec![(*n).to_string(), dur(*a), dur(*b), change_cell_us(*a, *b)])
                .collect();
            table(o, &["category", "A", "B", "change"], rows);
        }

        // CPU diff
        if !cmp.cpu_diff.is_empty() {
            o.push_str("<h3>CPU profile diff</h3>");
            let rows = cmp
                .cpu_diff
                .iter()
                .take(20)
                .map(|d| {
                    let name = if d.function_name.is_empty() {
                        "(anonymous)"
                    } else {
                        &d.function_name
                    };
                    let pp = d.pct_b - d.pct_a;
                    let diff_time_str = if d.diff_time_us > 0.0 {
                        format!("+{}", dur(d.diff_time_us))
                    } else {
                        dur(d.diff_time_us)
                    };
                    vec![
                        dur(d.time_a_us),
                        dur(d.time_b_us),
                        diff_time_str,
                        format!("{:.1}%", d.pct_a),
                        format!("{:.1}%", d.pct_b),
                        format!("{:+.1}pp", pp),
                        d.source_type.label().to_string(),
                        esc(name),
                    ]
                })
                .collect();
            table(
                o,
                &[
                    "A time", "B time", "Δ time", "A %", "B %", "Δ pp", "source", "function",
                ],
                rows,
            );
        }
        o.push_str("</section>");

        // Memory comparison: B timeline next to A (A summary recomputed
        // here; the per-section bodies above own their local copies).
        let scope = full_scope();
        let mem_a = inspect::memory_chart_data(events_a, &scope, min_ts_a);
        let summary = &mem_a.summary;
        let mem_b = inspect::memory_chart_data(events_b, &scope, min_ts_b);
        let sum_b = &mem_b.summary;
        if jf(summary, "samples") > 0.0 || jf(sum_b, "samples") > 0.0 {
            section(o, "Memory comparison");
            o.push_str("<div class=\"metrics\">");
            metric(
                o,
                "Heap growth A",
                &format!("{:+.1} MB", jf(summary, "growth_mb")),
                "",
            );
            metric(
                o,
                "Heap growth B",
                &format!("{:+.1} MB", jf(sum_b, "growth_mb")),
                "",
            );
            metric(
                o,
                "Peak heap A",
                &format!("{:.1} MB", jf(summary, "peak_heap_mb")),
                "",
            );
            metric(
                o,
                "Peak heap B",
                &format!("{:.1} MB", jf(sum_b, "peak_heap_mb")),
                "",
            );
            o.push_str("</div>");
            o.push_str(&line_chart(
                "JS heap — A (MB)",
                &mem_a.heap,
                "",
                "#3b82f6",
            ));
            o.push_str(&line_chart(
                "JS heap — B (MB)",
                &mem_b.heap,
                "",
                "#f59e0b",
            ));
            o.push_str("</section>");
        }
    }
}

const CSS: &str = r#"
<style>
:root { --fg:#1a2233; --muted:#6b7280; --border:#e6e8ef; --bg:#f5f6f8; --accent:#3b82f6; }
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--fg); font:15px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif; }
.container { max-width:1080px; margin:0 auto; padding:28px 20px 60px; }
h1 { font-size:22px; margin:0 0 4px; }
h1 .muted { color:var(--muted); font-weight:400; }
h2 { font-size:17px; margin:0 0 12px; }
h3 { font-size:14px; margin:18px 0 8px; }
.meta { color:var(--muted); font-size:13px; }
.meta code { background:#eef0f5; padding:1px 5px; border-radius:4px; }
.metrics { display:grid; grid-template-columns:repeat(auto-fit,minmax(170px,1fr)); gap:12px; margin:16px 0; }
.metric { background:#fff; border:1px solid var(--border); border-radius:10px; padding:14px 16px; }
.metric-value { font-size:24px; font-weight:700; font-variant-numeric:tabular-nums; }
.metric-label { color:var(--muted); font-size:13px; margin-top:2px; }
.metric-sub { color:var(--muted); font-size:12px; }
.section { background:#fff; border:1px solid var(--border); border-radius:12px; padding:20px; margin:16px 0; }
table { width:100%; border-collapse:collapse; margin:4px 0 8px; }
th, td { padding:7px 10px; text-align:left; border-bottom:1px solid var(--border); font-size:13.5px; }
th { color:var(--muted); font-weight:600; white-space:nowrap; }
td.num { font-variant-numeric:tabular-nums; text-align:right; }
th:not(:first-child), td.num { white-space:nowrap; }
tbody tr:hover { background:#fafbfd; }
code { font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace; font-size:13px; }
.pos { color:#059669; font-weight:600; }
.neg { color:#dc2626; font-weight:600; }
.muted { color:var(--muted); }
.chart { width:100%; height:auto; display:block; margin:6px 0 14px; }
.chart-title { font-size:13px; color:var(--muted); margin-top:6px; }
.axis { font-size:11px; fill:var(--muted); }
.grid { stroke:#eef0f5; }
.findings { margin:8px 0 16px; padding-left:18px; }
.findings li { margin:4px 0; }
footer { color:var(--muted); font-size:12px; text-align:center; margin-top:28px; }
@media (prefers-color-scheme: dark) {
  :root { --fg:#e5e9f0; --muted:#98a1b3; --border:#2a3040; --bg:#141821; }
  .metric, .section { background:#1b2029; }
  tbody tr:hover { background:#1f2530; }
  .meta code { background:#232a38; }
  .grid { stroke:#2a3040; }
}
</style>
"#;
