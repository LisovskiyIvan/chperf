//! Self-contained, offline HTML report: one file with inline CSS and SVG
//! charts (no external assets, no JS). Renders the full analysis plus the
//! memory / input / async sections for one or two (compared) traces.

use chperf_core::analysis::*;
use chperf_core::{inspect, trace};
use crate::app::App;
use serde_json::Value;

// ── Formatting helpers ──

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
fn js(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
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
    out.push_str(&format!("<section class=\"section\"><h2>{}</h2>", esc(title)));
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

fn change_cell(a: f64, b: f64) -> String {
    if a <= 0.0 && b <= 0.0 {
        return "<span class=\"muted\">—</span>".to_string();
    }
    if a <= 0.0 {
        return "<span class=\"pos\">new</span>".to_string();
    }
    let d = (b - a) / a * 100.0;
    if d > 5.0 {
        format!("<span class=\"neg\">+{:.0}%</span>", d)
    } else if d < -5.0 {
        format!("<span class=\"pos\">{:.0}%</span>", d)
    } else {
        format!("<span class=\"muted\">{:+.0}%</span>", d)
    }
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
    s.push_str(&format!(
        "<div class=\"chart-title\">{}</div>",
        esc(title)
    ));
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
            PL, yy, W - PR, yy
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
        PL, H - 8.0, x0 / x_unit, xu
    ));
    s.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.0}\" class=\"axis\" text-anchor=\"end\">{:.1}{}</text>",
        W - PR, H - 8.0, x1 / x_unit, xu
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
    s.push_str(&format!("<path d=\"{}\" class=\"area\" style=\"fill:{}\"/>", area, color));
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

fn memory_points(samples: &Value) -> Vec<(f64, f64)> {
    samples
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| (jf(s, "t_us") / 1000.0, jf(s, "heap_mb")))
                .collect()
        })
        .unwrap_or_default()
}

fn nodes_points(samples: &Value) -> Vec<(f64, f64)> {
    samples
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| (jf(s, "t_us") / 1000.0, jf(s, "nodes")))
                .collect()
        })
        .unwrap_or_default()
}

// ── Main entry ──

pub fn export_html(
    app: &App,
    events_a: &[trace::TraceEvent],
    min_ts_a: f64,
    events_b: Option<(&[trace::TraceEvent], f64)>,
) -> String {
    let mut o = String::new();
    o.push_str("<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">");
    o.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    o.push_str(&format!("<title>chperf: {}</title>", esc(&app.trace_name_a)));
    o.push_str(CSS);
    o.push_str("</head><body><div class=\"container\">");

    // ── Header ──
    o.push_str(&format!("<h1>Chrome Trace Analysis: <span class=\"muted\">{}</span></h1>", esc(&app.trace_name_a)));
    if let Some(ref meta) = app.metadata {
        if let Some(ref url) = meta.page_url {
            o.push_str(&format!("<div class=\"meta\">URL: <code>{}</code></div>", esc(url)));
        }
        if let Some(ref start) = meta.start_time {
            o.push_str(&format!("<div class=\"meta\">Recorded: {}</div>", esc(start)));
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

    // ── Overview cards ──
    o.push_str("<div class=\"metrics\">");
    metric(&mut o, "Duration", &dur(app.summary.total_trace_duration_us), "");
    let busy = if app.summary.total_trace_duration_us > 0.0 {
        app.summary.main_thread_busy_us / app.summary.total_trace_duration_us * 100.0
    } else {
        0.0
    };
    metric(&mut o, "Main thread busy", &format!("{:.1}%", busy), &dur(app.summary.main_thread_busy_us));
    metric(&mut o, "Long tasks (>50ms)", &format!("{}", app.summary.long_task_count), "");
    metric(&mut o, "Dropped frames", &format!("{}", app.jank.total_dropped), "");
    o.push_str("</div>");

    // ── Memory ──
    let scope = full_scope();
    let (_, mem) = inspect::memory_section(events_a, &scope, 700, min_ts_a);
    let summary = &mem["summary"];
    if jf(summary, "samples") > 0.0 {
        section(&mut o, "Memory timeline");
        o.push_str("<div class=\"metrics\">");
        metric(&mut o, "JS heap (start → end)", &format!("{:.1} MB", jf(summary, "first_heap_mb")), "");
        metric(
            &mut o,
            "Heap growth",
            &format!("{:+.1} MB", jf(summary, "growth_mb")),
            &format!("peak {:.1} MB", jf(summary, "peak_heap_mb")),
        );
        metric(&mut o, "DOM nodes (peak)", &format!("{:.0}", jf(summary, "peak_nodes")), "");
        metric(
            &mut o,
            "Listeners / documents",
            &format!("{:.0} / {:.0}", jf(summary, "peak_listeners"), jf(summary, "peak_documents")),
            "",
        );
        o.push_str("</div>");
        o.push_str(&line_chart("JS heap (MB)", &memory_points(&mem["samples"]), "", "#3b82f6"));
        o.push_str(&line_chart("DOM nodes", &nodes_points(&mem["samples"]), "", "#10b981"));
        o.push_str("</section>");
    }

    // ── CPU top functions ──
    let cp = &app.cpu_profile;
    if !cp.functions.is_empty() {
        section(&mut o, "CPU profile — top functions by self time");
        let total = cp.total_sample_time_us;
        let mut rows = Vec::new();
        let mut cum = 0.0;
        for f in cp.functions.iter().take(30) {
            let pct = if total > 0.0 { f.self_time_us / total * 100.0 } else { 0.0 };
            cum += pct;
            let name = if f.function_name.is_empty() { "(anonymous)" } else { &f.function_name };
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
        table(&mut o, &["self %", "source", "function", "self time", "cum %", "file"], rows);
        o.push_str("</section>");
    }

    // ── Event breakdown ──
    if !app.summary.event_stats.is_empty() {
        section(&mut o, "Event breakdown");
        let rows = app
            .summary
            .event_stats
            .iter()
            .map(|s| {
                vec![
                    esc(s.name),
                    format!("{}", s.count),
                    dur(s.total_time_us),
                    dur(s.avg_time_us),
                    format!("{:.1}%", s.pct_of_trace),
                ]
            })
            .collect();
        table(&mut o, &["event", "count", "total", "avg", "% of trace"], rows);
        o.push_str("</section>");
    }

    // ── Input latency ──
    let (_, inp) = inspect::input_section(events_a, &scope, 30, min_ts_a);
    if let Some(types) = inp["types"].as_array()
        && !types.is_empty() {
            section(&mut o, "Input latency by type");
            let rows = types
                .iter()
                .map(|t| {
                    vec![
                        esc(&js(t, "type")),
                        format!("{}", ji(t, "count")),
                        dur(jf(t, "total_us")),
                        dur(jf(t, "avg_us")),
                        dur(jf(t, "p50_us")),
                        dur(jf(t, "p99_us")),
                        dur(jf(t, "max_us")),
                    ]
                })
                .collect();
            table(&mut o, &["type", "count", "total", "avg", "p50", "p99", "max"], rows);
            if let Some(worst) = inp["worst"].as_array()
                && !worst.is_empty() {
                    o.push_str("<h3>Worst inputs</h3>");
                    let rows = worst
                        .iter()
                        .take(20)
                        .map(|w| {
                            vec![
                                format!("{:.2}ms", jf(w, "t_us") / 1000.0),
                                dur(jf(w, "dur_us")),
                                esc(&js(w, "type")),
                            ]
                        })
                        .collect();
                    table(&mut o, &["t", "duration", "type"], rows);
                }
            o.push_str("</section>");
        }

    // ── Async tasks ──
    let (_, asy) = inspect::async_section(events_a, &scope, 30, min_ts_a);
    if let Some(tasks) = asy["tasks"].as_array()
        && !tasks.is_empty() {
            section(&mut o, "Async tasks (s/f paired)");
            let rows = tasks
                .iter()
                .map(|t| {
                    vec![
                        esc(&js(t, "name")),
                        format!("{}", ji(t, "count")),
                        dur(jf(t, "total_us")),
                        dur(jf(t, "avg_us")),
                        dur(jf(t, "p50_us")),
                        dur(jf(t, "p99_us")),
                        dur(jf(t, "max_us")),
                    ]
                })
                .collect();
            table(&mut o, &["name", "count", "total", "avg", "p50", "p99", "max"], rows);
            if let Some(longest) = asy["longest"].as_array()
                && !longest.is_empty() {
                    o.push_str("<h3>Longest tasks</h3>");
                    let rows = longest
                        .iter()
                        .take(20)
                        .map(|w| {
                            vec![
                                format!("{:.2}ms", jf(w, "t_us") / 1000.0),
                                dur(jf(w, "dur_us")),
                                esc(&js(w, "name")),
                            ]
                        })
                        .collect();
                    table(&mut o, &["t", "duration", "name"], rows);
                }
            o.push_str("</section>");
        }

    // ── Jank clusters ──
    if !app.jank.clusters.is_empty() || app.jank.total_dropped > 0 {
        section(&mut o, "Jank clusters");
        o.push_str(&format!(
            "<p class=\"muted\">Bucket {}ms — dropped frames and ≥16.7ms spikes below the Long Task threshold.</p>",
            app.jank.bucket_ms.round() as i64
        ));
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
        table(&mut o, &["start", "span", "busy", "max RunTask", "dropped", "what happened"], rows);
        o.push_str("</section>");
    }

    // ── Scroll frames ──
    if !app.scroll_frames.tasks.is_empty() {
        section(&mut o, "Scroll frame analysis");
        let sf = &app.scroll_frames;
        o.push_str("<div class=\"metrics\">");
        metric(&mut o, "Scroll tasks", &format!("{}", sf.tasks.len()), "");
        metric(&mut o, "Avg duration", &dur(sf.avg.dur_us), &format!("bottleneck: {}", sf.avg.bottleneck()));
        metric(
            &mut o,
            "P50 / P90 / P99",
            &format!("{} / {} / {}", dur(sf.percentiles.p50_us), dur(sf.percentiles.p90_us), dur(sf.percentiles.p99_us)),
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
        table(&mut o, &["duration", "bottleneck", "JS", "style", "layout", "paint"], rows);
        o.push_str("</section>");
    }

    // ── Compare (B) ──
    if let (Some(cmp), Some((events_b, min_ts_b))) = (&app.compare, events_b) {
        let name_b = app.trace_name_b.as_deref().unwrap_or("B");
        section(&mut o, &format!("Comparison: A vs {name_b}"));

        // Findings
        if !cmp.findings.is_empty() {
            o.push_str("<ul class=\"findings\">");
            for f in &cmp.findings {
                let (cls, label) = match f.severity {
                    FindingSeverity::Improved => ("pos", "improved"),
                    FindingSeverity::Regressed => ("neg", "regressed"),
                    FindingSeverity::Neutral => ("muted", "neutral"),
                };
                let detail = if f.detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", f.detail)
                };
                o.push_str(&format!(
                    "<li><span class=\"{cls}\">[{label}]</span> {}: {}{}</li>",
                    esc(&f.category),
                    esc(&f.message),
                    esc(&detail),
                ));
            }
            o.push_str("</ul>");
        }

        // Quick stats
        let sa = &cmp.summary_a;
        let sb = &cmp.summary_b;
        let mut rows: Vec<Vec<String>> = vec![
            vec!["Long tasks".into(), format!("{}", sa.long_task_count), format!("{}", sb.long_task_count), change_cell(sa.long_task_count as f64, sb.long_task_count as f64)],
            vec!["Worst task".into(), dur(sa.long_tasks_top.first().copied().unwrap_or(0.0)), dur(sb.long_tasks_top.first().copied().unwrap_or(0.0)), change_cell(sa.long_tasks_top.first().copied().unwrap_or(0.0), sb.long_tasks_top.first().copied().unwrap_or(0.0))],
            vec!["Main thread busy".into(), dur(sa.main_thread_busy_us), dur(sb.main_thread_busy_us), change_cell(sa.main_thread_busy_us, sb.main_thread_busy_us)],
            vec!["Layout dirty (avg)".into(), format!("{:.0}", cmp.layout_a.avg_dirty), format!("{:.0}", cmp.layout_b.avg_dirty), change_cell(cmp.layout_a.avg_dirty, cmp.layout_b.avg_dirty)],
        ];
        if cmp.style_recalc_a.total_count > 0 || cmp.style_recalc_b.total_count > 0 {
            rows.push(vec![
                "Style elements (avg)".into(),
                format!("{:.0}", cmp.style_recalc_a.avg_elements),
                format!("{:.0}", cmp.style_recalc_b.avg_elements),
                change_cell(cmp.style_recalc_a.avg_elements, cmp.style_recalc_b.avg_elements),
            ]);
        }
        table(&mut o, &["metric", "A", "B", "change"], rows);

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
                .map(|(n, a, b)| vec![(*n).to_string(), dur(*a), dur(*b), change_cell(*a, *b)])
                .collect();
            table(&mut o, &["category", "A", "B", "change"], rows);
        }

        // CPU diff
        if !cmp.cpu_diff.is_empty() {
            o.push_str("<h3>CPU profile diff</h3>");
            let rows = cmp
                .cpu_diff
                .iter()
                .take(20)
                .map(|d| {
                    let name = if d.function_name.is_empty() { "(anonymous)" } else { &d.function_name };
                    let pp = d.pct_b - d.pct_a;
                    vec![
                        format!("{:.1}%", d.pct_a),
                        format!("{:.1}%", d.pct_b),
                        format!("{:+.1}pp", pp),
                        d.source_type.label().to_string(),
                        esc(name),
                    ]
                })
                .collect();
            table(&mut o, &["A %", "B %", "Δ pp", "source", "function"], rows);
        }
        o.push_str("</section>");

        // Memory comparison: B timeline next to A.
        let (_, mem_b) = inspect::memory_section(events_b, &scope, 700, min_ts_b);
        let sum_b = &mem_b["summary"];
        if jf(summary, "samples") > 0.0 || jf(sum_b, "samples") > 0.0 {
            section(&mut o, "Memory comparison");
            o.push_str("<div class=\"metrics\">");
            metric(&mut o, "Heap growth A", &format!("{:+.1} MB", jf(summary, "growth_mb")), "");
            metric(&mut o, "Heap growth B", &format!("{:+.1} MB", jf(sum_b, "growth_mb")), "");
            metric(&mut o, "Peak heap A", &format!("{:.1} MB", jf(summary, "peak_heap_mb")), "");
            metric(&mut o, "Peak heap B", &format!("{:.1} MB", jf(sum_b, "peak_heap_mb")), "");
            o.push_str("</div>");
            o.push_str(&line_chart("JS heap — A (MB)", &memory_points(&mem["samples"]), "", "#3b82f6"));
            o.push_str(&line_chart("JS heap — B (MB)", &memory_points(&mem_b["samples"]), "", "#f59e0b"));
            o.push_str("</section>");
        }
    }

    o.push_str("<footer>Generated by <code>chperf</code></footer>");
    o.push_str("</div></body></html>");
    o
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
