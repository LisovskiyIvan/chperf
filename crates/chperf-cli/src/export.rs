use crate::app::App;
use chperf_core::analysis::*;

fn fmt_us(us: f64) -> String {
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

fn pct_diff(a: f64, b: f64) -> String {
    // Honest 0→N rendering via chperf_core::analysis::fmt_change (never raw diff_pct).
    chperf_core::analysis::fmt_change(a, b, &|v| format!("{:.1}", v))
}

fn pct_diff_count(a: f64, b: f64) -> String {
    chperf_core::analysis::fmt_change(a, b, &|v| format!("{:.0}", v))
}

fn fmt_change_count(a: f64, b: f64) -> String {
    let s = chperf_core::analysis::fmt_change(a, b, &|v| format!("{:.0}", v + 0.0));
    let s = s.replace("-0 →", "0 →").replace("→ -0", "→ 0");
    if s.contains('→') {
        return s;
    }
    let v: f64 = s
        .trim_end_matches('%')
        .trim_start_matches('+')
        .parse()
        .unwrap_or(0.0);
    if s.starts_with('+') && v > 0.5 {
        format!("{} :red_circle:", s)
    } else if s.starts_with('-') && v < -5.0 {
        format!("{} :white_check_mark:", s)
    } else {
        s
    }
}

pub fn export_summary_only(app: &App) -> String {
    let mut out = String::new();

    if let Some(ref cmp) = app.compare {
        let name_a = &app.trace_name_a;
        let name_b = app.trace_name_b.as_deref().unwrap_or("B");
        export_compare_summary(&mut out, app, cmp, name_a, name_b);
    } else {
        out.push_str("No comparison data. Use --compare to compare two traces.\n");
    }

    out
}

pub fn export_markdown(app: &App) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "# Chrome Trace Analysis: {}\n\n",
        app.trace_name_a
    ));
    // Trust header: every full report starts with trace health.
    export_trace_health(&mut out, app);
    // Metadata block
    if let Some(ref meta) = app.metadata {
        out.push_str("**Trace Info**\n\n");
        if let Some(ref url) = meta.page_url {
            out.push_str(&format!("- **URL**: {}\n", url));
        }
        if let Some(ref start) = meta.start_time {
            out.push_str(&format!("- **Recorded**: {}\n", start));
        }
        if let Some(cpu) = meta.cpu_throttling
            && cpu > 1.0
        {
            out.push_str(&format!(
                "- **CPU Throttle**: {:.0}x (reference note only — times are not rescaled)\n",
                cpu,
            ));
        }
        if let Some(ref net) = meta.network_throttling
            && !net.is_empty()
            && net != "No throttling"
        {
            out.push_str(&format!("- **Network**: {}\n", net));
        }
        if let Some(dpr) = meta.host_dpr {
            out.push_str(&format!("- **DPR**: {}\n", dpr));
        }
        out.push('\n');
    } else if app.throttle_factor > 1.0 {
        out.push_str(&format!(
            "> **Note**: CPU throttle {:.0}x (reference note only — times are not rescaled).\n\n",
            app.throttle_factor,
        ));
    }

    // ── Overview ──
    export_overview(&mut out, &app.summary, &app.trace_name_a);

    // ── Renderers (multi-process / OOPIF) ──
    export_renderers(&mut out, app);

    // ── Frame-Blocking Main-Thread Tasks ──
    export_scroll_frames(&mut out, &app.scroll_frames);

    // ── CPU Profile ──
    export_cpu_profile(&mut out, &app.cpu_profile);

    // ── Layout Dirty ──
    export_layout_dirty(&mut out, &app.layout_dirty);

    // ── Style Recalc ──
    export_style_recalc(&mut out, &app.style_recalc);

    // ── Forced Reflow ──
    export_forced_reflows(&mut out, &app.forced_reflows);

    // ── Jank Clusters ──
    export_jank(&mut out, &app.jank, app.frame_budget_us);

    // ── Loading ──
    export_loading(&mut out, app);

    // ── Network ──
    export_network(&mut out, app);

    // ── Raster/Compositor ──
    export_raster(&mut out, app);

    // ── Official long tasks / EventTiming ──
    export_official(&mut out, app);

    // ── Event Breakdown ──
    export_event_breakdown(&mut out, &app.summary);

    // ── Compare ──
    if let Some(ref cmp) = app.compare {
        let name_b = app.trace_name_b.as_deref().unwrap_or("B");
        export_compare_summary(&mut out, app, cmp, &app.trace_name_a, name_b);
        export_compare(&mut out, cmp, &app.trace_name_a, name_b);
    }

    out
}

fn export_trace_health(out: &mut String, app: &App) {
    out.push_str("## Trace health\n\n");
    out.push_str(&format!(
        "- **Events**: {} parsed / {} skipped / {} malformed\n",
        app.trace_stats.parsed, app.trace_stats.skipped, app.trace_stats.malformed
    ));
    out.push_str(&format!(
        "- **Main thread**: tid={} · **Threads**: {} · **Span**: {}\n",
        app.main_tid,
        app.thread_count,
        fmt_us(app.trace_span_us)
    ));
    out.push_str(&format!(
        "- **Renderers**: {} · **Browser main**: {} · **Compositor**: {}\n",
        renderers_line(app),
        app.thread_model
            .browser_main
            .map(|t| format!("tid {}", t))
            .unwrap_or_else(|| "none".to_string()),
        if app.thread_model.compositor.is_empty() {
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
        },
    ));
    let budget_label = match app.frame_budget_us.round() as i64 {
        8333 => "8.33ms (120Hz detected)",
        11111 => "11.11ms (90Hz detected)",
        16667 => "16.67ms (60Hz detected)",
        _ => "custom",
    };
    let budget_str = if budget_label == "custom" {
        format!("{:.2}ms", app.frame_budget_us / 1000.0)
    } else {
        budget_label.to_string()
    };
    out.push_str(&format!("- **Frame budget**: {}\n", budget_str));
    if let Some(ref meta) = app.metadata {
        if let Some(cpu) = meta.cpu_throttling {
            out.push_str(&format!("- **CPU throttle**: {:.0}x\n", cpu));
        }
        if let Some(ref net) = meta.network_throttling
            && !net.is_empty()
        {
            out.push_str(&format!("- **Network throttle**: {}\n", net));
        }
    }
    out.push('\n');
}

fn renderers_line(app: &App) -> String {
    let mut rs = app.thread_model.renderers.clone();
    rs.sort_by_key(|r| (r.pid, r.tid));
    if rs.is_empty() {
        return "0 pids".to_string();
    }
    let parts: Vec<String> = rs
        .iter()
        .map(|r| format!("{}:tid {}", r.pid, r.tid))
        .collect();
    format!(
        "{} pid{} ({})",
        rs.len(),
        if rs.len() == 1 { "" } else { "s" },
        parts.join(", ")
    )
}

/// Per-renderer table (pid/tid, busy union, RunTask total, long tasks,
/// dropped frames + jank clusters). Single-renderer traces get a one-liner.
fn export_renderers(out: &mut String, app: &App) {
    out.push_str("## Renderers\n\n");
    let mut rs = app.thread_model.renderers.clone();
    rs.sort_by_key(|r| (r.pid, r.tid));
    if rs.len() <= 1 {
        let who = rs
            .first()
            .map(|r| format!(" (pid {} tid {})", r.pid, r.tid))
            .unwrap_or_default();
        out.push_str(&format!("Renderers: 1 (single-process trace){}.\n\n", who));
        return;
    }
    out.push_str(&format!(
        "_Primary renderer (tid {}) feeds the main report; other renderers' numbers are secondary (OOPIF/iframe/workers)._ \n\n",
        app.thread_model.primary.tid,
    ));
    out.push_str("| pid | tid | thread | main busy(ms) | RunTask total(ms) | long tasks | dropped | clusters | primary |\n");
    out.push_str("|-----|-----|--------|-----------|---------------|------------|---------|----------|---------|\n");
    for r in &rs {
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
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.pid,
            r.tid,
            name,
            sum.map(|s| fmt_us(s.main_thread_busy_us))
                .unwrap_or_else(|| "—".to_string()),
            sum.map(|s| fmt_us(s.runtask_total_us))
                .unwrap_or_else(|| "—".to_string()),
            sum.map(|s| s.long_task_count.to_string())
                .unwrap_or_else(|| "—".to_string()),
            jk.map(|j| j.total_dropped.to_string())
                .unwrap_or_else(|| "—".to_string()),
            jk.map(|j| j.clusters.len().to_string())
                .unwrap_or_else(|| "—".to_string()),
            if r.tid == app.thread_model.primary.tid {
                "yes"
            } else {
                "no"
            },
        ));
    }
    out.push('\n');
}

fn export_overview(out: &mut String, summary: &SummaryResult, name: &str) {
    let busy_pct = if summary.total_trace_duration_us > 0.0 {
        summary.main_thread_busy_us / summary.total_trace_duration_us * 100.0
    } else {
        0.0
    };

    out.push_str("## Overview\n\n");
    out.push_str(&format!("- **Trace**: {}\n", name));
    out.push_str(&format!(
        "- **Duration**: {}\n",
        fmt_us(summary.total_trace_duration_us)
    ));
    out.push_str(&format!(
        "- **Main Thread Busy**: {:.1}% ({})\n",
        busy_pct,
        fmt_us(summary.main_thread_busy_us)
    ));
    out.push_str(&format!(
        "- **Total Blocking Time (TBT)**: {}\n",
        fmt_us(summary.total_blocking_time_us)
    ));
    out.push_str(&format!(
        "- **Long Tasks (>50ms)**: {} ({})\n",
        summary.long_task_count,
        fmt_us(summary.long_tasks_total_us)
    ));

    if !summary.long_tasks_top.is_empty() {
        out.push_str("- **Top Long Tasks**: ");
        let items: Vec<String> = summary
            .long_tasks_top
            .iter()
            .take(5)
            .map(|d| fmt_us(*d))
            .collect();
        out.push_str(&items.join(", "));
        out.push('\n');
    }
    out.push('\n');
}

fn export_scroll_frames(out: &mut String, sf: &ScrollFrameResult) {
    out.push_str("## Frame-Blocking Main-Thread Tasks\n\n");

    if sf.heuristic_fallback {
        out.push_str(
            "_Heuristic fallback active: no scroll/input evidence in trace, tasks selected by workload heuristic._\n\n",
        );
    } else {
        out.push_str("_Tasks overlap scroll/input evidence in this trace._\n\n");
    }

    if sf.tasks.is_empty() {
        out.push_str(
            "No frame-blocking tasks detected (0 tasks: no RunTask with a child over the frame budget).\n\n",
        );
        return;
    }

    let avg = &sf.avg;
    let pct = &sf.percentiles;
    out.push_str(&format!("- **Frame-blocking tasks**: {}\n", sf.tasks.len()));
    out.push_str(&format!("- **Avg Duration**: {}\n", fmt_us(avg.dur_us)));
    out.push_str(&format!(
        "- **P50**: {} / **P90**: {} / **P99**: {}\n",
        fmt_us(pct.p50_us),
        fmt_us(pct.p90_us),
        fmt_us(pct.p99_us)
    ));
    out.push_str(&format!("- **Bottleneck**: {}\n", avg.bottleneck()));
    out.push('\n');

    out.push_str("### Average Breakdown (per task)\n\n");
    out.push_str("| Category | Time | % of Task |\n");
    out.push_str("|----------|------|-----------|\n");

    let categories = [
        ("JS (FunctionCall)", avg.js_us),
        ("Style (UpdateLayoutTree)", avg.ult_us),
        ("Layout", avg.layout_us),
        ("Paint", avg.paint_us),
        ("Composite (Layerize+Commit)", avg.composite_us),
        ("HitTest", avg.hit_test_us),
    ];

    for (name, us) in &categories {
        let pct = if avg.dur_us > 0.0 {
            us / avg.dur_us * 100.0
        } else {
            0.0
        };
        out.push_str(&format!("| {} | {} | {:.1}% |\n", name, fmt_us(*us), pct));
    }
    out.push('\n');

    // Top 10 worst tasks
    out.push_str("### Worst Frame-Blocking Tasks\n\n");
    out.push_str("| # | Duration | Bottleneck | JS | Style | Layout | Paint |\n");
    out.push_str("|---|----------|------------|-------|-------|--------|-------|\n");
    for (i, task) in sf.tasks.iter().take(10).enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            i + 1,
            fmt_us(task.dur_us),
            task.bottleneck(),
            fmt_us(task.js_us),
            fmt_us(task.ult_us),
            fmt_us(task.layout_us),
            fmt_us(task.paint_us),
        ));
    }
    out.push('\n');
}

fn export_cpu_profile(out: &mut String, cp: &CpuProfileResult) {
    out.push_str("## CPU Profile (Top Functions by Self-Time)\n\n");

    if cp.functions.is_empty() {
        out.push_str("No CPU profile data found (ProfileChunk events).\n\n");
        return;
    }

    let total = cp.total_sample_time_us;

    // `+ 0.0` normalizes -0.0 (from zero-width shares) so it renders as "0.0%".
    let share = |us: f64| {
        if total > 0.0 {
            us / total * 100.0 + 0.0
        } else {
            0.0
        }
    };
    let app_pct = share(cp.app_time_us);
    let rt_pct = share(cp.runtime_time_us);
    let native_pct = share(cp.native_time_us);

    out.push_str(&format!("- **Total Sample Time**: {}\n", fmt_us(total)));
    out.push_str(&format!(
        "- **App Code**: {} ({:.1}%)\n",
        fmt_us(cp.app_time_us),
        app_pct
    ));
    out.push_str(&format!(
        "- **Runtime/Library**: {} ({:.1}%)\n",
        fmt_us(cp.runtime_time_us),
        rt_pct
    ));
    out.push_str(&format!(
        "- **Native/Browser**: {} ({:.1}%)\n\n",
        fmt_us(cp.native_time_us),
        native_pct
    ));

    out.push_str("| # | Source | Function | Self Time | % of sampled time | Cumulative % of sampled time | File |\n");
    out.push_str("|---|--------|----------|-----------|---------------------|--------------------------------|------|\n");

    let mut cum_pct = 0.0;
    for (i, f) in cp.functions.iter().take(30).enumerate() {
        let pct = if total > 0.0 {
            f.self_time_us / total * 100.0
        } else {
            0.0
        };
        cum_pct += pct;

        let name = if f.function_name.is_empty() {
            "(anonymous)"
        } else {
            &f.function_name
        };

        // Shorten URL to last path segment
        let short_url = if let Some(idx) = f.url.rfind('/') {
            &f.url[idx + 1..]
        } else {
            &f.url
        };

        out.push_str(&format!(
            "| {} | {} | {} | {} | {:.1}% | {:.1}% | {} |\n",
            i + 1,
            f.source_type.label(),
            name,
            fmt_us(f.self_time_us),
            pct,
            cum_pct,
            short_url,
        ));
    }
    out.push('\n');
}

fn export_layout_dirty(out: &mut String, ld: &LayoutDirtyResult) {
    out.push_str("## Layout Dirty Objects\n\n");

    if ld.entries.is_empty() {
        out.push_str("No Layout events with dirty object data found.\n\n");
        return;
    }

    out.push_str(&format!("- **Layout Events**: {}\n", ld.entries.len()));
    out.push_str(&format!("- **Avg Dirty Objects**: {:.0}\n", ld.avg_dirty));
    out.push_str(&format!("- **Max Dirty Objects**: {}\n", ld.max_dirty));
    out.push_str(&format!("- **Avg Dirty Ratio**: {:.1}%\n\n", ld.avg_ratio));

    out.push_str("### Top 10 Layout Events (by dirty count)\n\n");
    out.push_str("| # | Duration | Dirty | Total | Ratio |\n");
    out.push_str("|---|----------|-------|-------|-------|\n");
    for (i, e) in ld.entries.iter().take(10).enumerate() {
        let ratio = if e.total_count > 0 {
            e.dirty_count as f64 / e.total_count as f64 * 100.0
        } else {
            0.0
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {:.1}% |\n",
            i + 1,
            fmt_us(e.dur_us),
            e.dirty_count,
            e.total_count,
            ratio,
        ));
    }
    out.push('\n');
}

fn export_style_recalc(out: &mut String, sr: &StyleRecalcResult) {
    out.push_str("## Style Recalc (UpdateLayoutTree)\n\n");
    if sr.entries.is_empty() {
        out.push_str("none found (0 events).\n\n");
        return;
    }
    out.push_str(&format!("- **Events**: {}\n", sr.total_count));
    out.push_str(&format!("- **Avg Elements**: {:.0}\n", sr.avg_elements));
    out.push_str(&format!("- **Max Elements**: {}\n\n", sr.max_elements));

    out.push_str("| # | Duration | Elements |\n");
    out.push_str("|---|----------|----------|\n");
    for (i, e) in sr.entries.iter().take(10).enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            i + 1,
            fmt_us(e.dur_us),
            e.element_count,
        ));
    }
    out.push('\n');
}

fn export_forced_reflows(out: &mut String, fr: &ForcedReflowResult) {
    out.push_str("## Forced Reflow (Layout Thrashing)\n\n");
    if fr.entries.is_empty() {
        out.push_str("none found (0 tasks with thrashing).\n\n");
        return;
    }
    out.push_str(&format!(
        "- **Tasks with thrashing**: {}\n",
        fr.entries.len()
    ));
    out.push_str(&format!("- **Total reflows**: {}\n", fr.total_reflows));
    out.push_str(&format!(
        "- **Total layout time**: {}\n\n",
        fmt_us(fr.total_layout_time_us)
    ));

    let has_initiator = fr.entries.iter().any(|e| e.initiator.is_some());
    if has_initiator {
        out.push_str("| # | Task Duration | Reflows | Layout Time | Initiator |\n");
        out.push_str("|---|---------------|---------|-------------|-----------|\n");
        for (i, e) in fr.entries.iter().take(10).enumerate() {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                i + 1,
                fmt_us(e.task_dur_us),
                e.reflow_count,
                fmt_us(e.layout_time_us),
                e.initiator.as_deref().unwrap_or("-"),
            ));
        }
    } else {
        out.push_str("| # | Task Duration | Reflows | Layout Time |\n");
        out.push_str("|---|---------------|---------|-------------|\n");
        for (i, e) in fr.entries.iter().take(10).enumerate() {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                i + 1,
                fmt_us(e.task_dur_us),
                e.reflow_count,
                fmt_us(e.layout_time_us),
            ));
        }
    }
    out.push('\n');
}

/// Jank clusters: dropped frames / over-budget spikes that stay below the 50ms
/// Long Task threshold and disappear in a long trace's average.
fn export_jank(out: &mut String, jank: &JankResult, budget_us: f64) {
    out.push_str("## Jank Clusters\n\n");
    if jank.clusters.is_empty() && jank.total_dropped == 0 {
        out.push_str("none found (0 clusters, 0 dropped frames).\n\n");
        return;
    }
    out.push_str(&format!(
        "- **Total dropped frames**: {}\n",
        jank.total_dropped
    ));
    out.push_str(&format!("- **Bucket size**: {:.0}ms\n", jank.bucket_ms));
    out.push_str(&format!(
        "- **Frame budget**: {}\n\n",
        match budget_us.round() as i64 {
            8333 => "8.33ms (120Hz detected)".to_string(),
            11111 => "11.11ms (90Hz detected)".to_string(),
            16667 => "16.67ms (60Hz detected)".to_string(),
            _ => format!("{:.2}ms", budget_us / 1000.0),
        }
    ));
    out.push_str(
        "Windows where dropped frames or over-budget spikes (RunTask/FireAnimationFrame/GPUTask\n\
         lasting at least the frame budget) or heavy main-thread busy (≥50ms attributed)\n\
         occurred — below the 50ms Long Task threshold, invisible in the summary.\n\n",
    );

    out.push_str(
        "| # | t | span | busy | max RunTask | max FAF | max GPUTask | dropped | what happened |\n",
    );
    out.push_str("|---|-----|------|------|-------------|---------|-------------|---------|---------------|\n");
    for (i, c) in jank.clusters.iter().enumerate() {
        let calls: String = c
            .top_calls
            .iter()
            .map(|(n, d)| format!("{} ({})", n, fmt_us(*d)))
            .collect::<Vec<_>>()
            .join(", ");
        let calls = if calls.is_empty() {
            "—".to_string()
        } else {
            calls
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            i + 1,
            fmt_us(c.start_us),
            fmt_us(c.end_us - c.start_us),
            fmt_us(c.busy_us),
            fmt_us(c.max_run_us),
            fmt_us(c.max_faf_us),
            fmt_us(c.max_gpu_us),
            c.dropped_frames,
            calls,
        ));
    }
    out.push('\n');
}

fn export_loading(out: &mut String, app: &App) {
    out.push_str("## Loading\n\n");
    match app.loading.navigation_start_us {
        Some(v) => out.push_str(&format!("- **navigationStart**: {}\n", fmt_us(v))),
        None => out.push_str("- **navigationStart**: none found\n"),
    }
    match app.loading.first_contentful_paint_us {
        Some(v) => out.push_str(&format!("- **FCP**: {}\n", fmt_us(v))),
        None => out.push_str("- **FCP**: none found\n"),
    }
    match app.loading.largest_contentful_paint_us {
        Some(v) => out.push_str(&format!("- **LCP**: {}\n", fmt_us(v))),
        None => out.push_str("- **LCP**: none found\n"),
    }
    out.push('\n');
    out.push_str("| milestone | t(ms rel nav) | dur |\n");
    out.push_str("|-----------|----------------|-----|\n");
    for e in &app.loading.entries {
        let rel = e
            .rel_us
            .map(|v| format!("{:.2}ms", v / 1000.0))
            .unwrap_or_else(|| "—".to_string());
        let dur = e.dur_us.map(fmt_us).unwrap_or_else(|| "—".to_string());
        out.push_str(&format!("| {} | {} | {} |\n", e.name, rel, dur));
    }
    out.push('\n');
    if app.loading.lcp.is_empty() {
        out.push_str("No LCP candidates found.\n\n");
    } else {
        out.push_str(&format!(
            "### LCP candidates ({})\n\n",
            app.loading.lcp.len()
        ));
        out.push_str("| # | size | node | url |\n");
        out.push_str("|---|------|------|-----|\n");
        for (i, c) in app.loading.lcp.iter().take(10).enumerate() {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                i + 1,
                c.size
                    .map(|v| format!("{:.0}", v))
                    .unwrap_or_else(|| "—".to_string()),
                c.node.as_deref().unwrap_or("—"),
                c.url.as_deref().unwrap_or("—"),
            ));
        }
        out.push('\n');
    }
    if app.loading.user_timing.is_empty() {
        out.push_str("No user timing entries found.\n\n");
    } else {
        out.push_str(&format!(
            "### User timing ({} entries)\n\n",
            app.loading.user_timing.len()
        ));
        out.push_str("| # | dur | name |\n");
        out.push_str("|---|-----|------|\n");
        for (i, u) in app.loading.user_timing.iter().take(10).enumerate() {
            out.push_str(&format!(
                "| {} | {} | {} |\n",
                i + 1,
                u.dur_us.map(fmt_us).unwrap_or_else(|| "—".to_string()),
                u.name,
            ));
        }
        out.push('\n');
    }
}

fn export_network(out: &mut String, app: &App) {
    out.push_str("## Network\n\n");
    out.push_str(&format!("- **Requests**: {}\n", app.network.request_count));
    out.push_str(&format!("- **Cache hits**: {}\n", app.network.cache_hits));
    out.push_str(&format!(
        "- **Render-blocking**: {}\n",
        app.network.render_blocking_count
    ));
    out.push_str(&format!(
        "- **Total wall**: {}\n",
        fmt_us(app.network.total_wall_us)
    ));
    out.push_str(&format!("- **Incomplete**: {}\n\n", app.network.incomplete));
    if app.network.requests.is_empty() {
        out.push_str("none found (0 requests).\n\n");
        return;
    }
    out.push_str("### Slowest 10 requests\n\n");
    out.push_str("| # | total | url | priority | cached | render-blocking |\n");
    out.push_str("|---|-------|-----|----------|--------|-----------------|\n");
    for (i, q) in app.network.slowest(10).iter().enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            i + 1,
            q.total_us.map(fmt_us).unwrap_or_else(|| "—".to_string()),
            q.url,
            q.priority.as_deref().unwrap_or("—"),
            if q.from_cache { "yes" } else { "no" },
            if q.render_blocking { "yes" } else { "no" },
        ));
    }
    out.push('\n');
}

fn export_raster(out: &mut String, app: &App) {
    out.push_str("## Raster / Compositor\n\n");
    out.push_str(&format!(
        "- **Raster total**: {}\n",
        fmt_us(app.raster.total_raster_us)
    ));
    out.push_str(&format!(
        "- **Decode total**: {} (max {})\n",
        fmt_us(app.raster.total_decode_us),
        fmt_us(app.raster.max_decode_us)
    ));
    out.push_str(&format!(
        "- **Composite total**: {}\n",
        fmt_us(app.raster.total_composite_us)
    ));
    out.push_str(&format!(
        "- **GPU task**: {}\n\n",
        fmt_us(app.raster.gpu_task_us)
    ));
    if app.raster.lanes.is_empty() {
        out.push_str("none found (0 lanes).\n\n");
    } else {
        out.push_str("| # | tid | thread | event | count | total | max |\n");
        out.push_str("|---|-----|--------|-------|-------|-------|-----|\n");
        for (i, l) in app.raster.lanes.iter().take(15).enumerate() {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                i + 1,
                l.tid,
                if l.thread_name.is_empty() {
                    "—"
                } else {
                    &l.thread_name
                },
                l.name,
                l.count,
                fmt_us(l.total_us),
                fmt_us(l.max_us),
            ));
        }
        out.push('\n');
    }
    if app.raster.decode_hotspots.is_empty() {
        out.push_str("No decode hotspots found.\n\n");
    } else {
        out.push_str(&format!(
            "### Decode hotspots ({})\n\n",
            app.raster.decode_hotspots.len()
        ));
        out.push_str("| # | dur | source |\n");
        out.push_str("|---|-----|--------|\n");
        for (i, h) in app.raster.decode_hotspots.iter().take(10).enumerate() {
            out.push_str(&format!(
                "| {} | {} | {} |\n",
                i + 1,
                fmt_us(h.dur_us),
                if h.url_or_node.is_empty() {
                    "—"
                } else {
                    &h.url_or_node
                },
            ));
        }
        out.push('\n');
    }
}

fn export_official(out: &mut String, app: &App) {
    if app.official_long_tasks.is_empty() {
        return;
    }
    out.push_str("## Official responsiveness signals (EventTiming)\n\n");
    out.push_str("| # | dur | name | interaction |\n");
    out.push_str("|---|-----|------|-------------|\n");
    for (i, l) in app.official_long_tasks.iter().take(20).enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            i + 1,
            l.dur_us.map(fmt_us).unwrap_or_else(|| "—".to_string()),
            l.name,
            if l.interaction_type.is_empty() {
                "—"
            } else {
                &l.interaction_type
            },
        ));
    }
    out.push('\n');
}

fn export_event_breakdown(out: &mut String, summary: &SummaryResult) {
    out.push_str("## Event Breakdown\n\n");
    out.push_str("| Event | Total | Self | Count | Avg | % of Trace | Self % of Trace |\n");
    out.push_str("|-------|-------|------|-------|-----|------------|-------------------|\n");

    for stat in summary.event_stats.iter().take(30) {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {:.1}% | {:.1}% |\n",
            stat.name,
            fmt_us(stat.total_time_us),
            fmt_us(stat.self_time_us),
            stat.count,
            fmt_us(stat.avg_time_us),
            stat.pct_of_trace,
            stat.self_pct_of_trace,
        ));
    }
    if summary.event_stats.len() > 30 {
        out.push_str(&format!(
            "\n_{} more, use --events/--stats for the full list._\n",
            summary.event_stats.len() - 30
        ));
    }
    out.push('\n');
}

fn fmt_change(a: f64, b: f64) -> String {
    let s = chperf_core::analysis::fmt_change(a, b, &fmt_us);
    if s.contains('→') {
        return s;
    }
    // s is like "+12%" / "-8%" / "+0%".
    let v: f64 = s
        .trim_end_matches('%')
        .trim_start_matches('+')
        .parse()
        .unwrap_or(0.0);
    if s.starts_with('+') && v > 0.5 {
        format!("{} :red_circle:", s)
    } else if s.starts_with('-') && v < -5.0 {
        format!("{} :white_check_mark:", s)
    } else {
        s
    }
}

fn export_compare_summary(
    out: &mut String,
    _app: &App,
    cmp: &CompareResult,
    name_a: &str,
    name_b: &str,
) {
    out.push_str(&format!("## Summary: {} vs {}\n\n", name_a, name_b));

    // ── Overall verdict based on P50 ──
    let has_scroll = cmp.scroll_avg_a.is_some() && cmp.scroll_avg_b.is_some();
    if has_scroll {
        let p50_a = cmp.scroll_pct_a.p50_us;
        let p50_b = cmp.scroll_pct_b.p50_us;
        let p50_change = if p50_a > 0.0 {
            (p50_b - p50_a) / p50_a * 100.0
        } else {
            0.0
        };
        let verdict = if p50_change < -5.0 {
            "Improved"
        } else if p50_change > 5.0 {
            "Regressed"
        } else {
            "No significant change"
        };
        out.push_str(&format!(
            "**Overall**: {} (Scroll P50 {:.0}%)\n\n",
            verdict, p50_change
        ));
    }

    // ── Scroll Performance ──
    if let (Some(avg_a), Some(avg_b)) = (&cmp.scroll_avg_a, &cmp.scroll_avg_b) {
        out.push_str("### Scroll Performance\n\n");
        out.push_str("| Metric | Before | After | Change |\n");
        out.push_str("|--------|--------|-------|--------|\n");

        out.push_str(&format!(
            "| P50 | {} | {} | {} |\n",
            fmt_us(cmp.scroll_pct_a.p50_us),
            fmt_us(cmp.scroll_pct_b.p50_us),
            fmt_change(cmp.scroll_pct_a.p50_us, cmp.scroll_pct_b.p50_us),
        ));
        out.push_str(&format!(
            "| P90 | {} | {} | {} |\n",
            fmt_us(cmp.scroll_pct_a.p90_us),
            fmt_us(cmp.scroll_pct_b.p90_us),
            fmt_change(cmp.scroll_pct_a.p90_us, cmp.scroll_pct_b.p90_us),
        ));
        out.push_str(&format!(
            "| Avg Duration | {} | {} | {} |\n",
            fmt_us(avg_a.dur_us),
            fmt_us(avg_b.dur_us),
            fmt_change(avg_a.dur_us, avg_b.dur_us),
        ));
        out.push_str(&format!(
            "| JS | {} | {} | {} |\n",
            fmt_us(avg_a.js_us),
            fmt_us(avg_b.js_us),
            fmt_change(avg_a.js_us, avg_b.js_us),
        ));
        out.push_str(&format!(
            "| Style (ULT) | {} | {} | {} |\n",
            fmt_us(avg_a.ult_us),
            fmt_us(avg_b.ult_us),
            fmt_change(avg_a.ult_us, avg_b.ult_us),
        ));
        out.push_str(&format!(
            "| Layout | {} | {} | {} |\n",
            fmt_us(avg_a.layout_us),
            fmt_us(avg_b.layout_us),
            fmt_change(avg_a.layout_us, avg_b.layout_us),
        ));
        out.push_str(&format!(
            "| Paint | {} | {} | {} |\n",
            fmt_us(avg_a.paint_us),
            fmt_us(avg_b.paint_us),
            fmt_change(avg_a.paint_us, avg_b.paint_us),
        ));
        out.push_str(&format!(
            "| HitTest | {} | {} | {} |\n",
            fmt_us(avg_a.hit_test_us),
            fmt_us(avg_b.hit_test_us),
            fmt_change(avg_a.hit_test_us, avg_b.hit_test_us),
        ));
        out.push_str(&format!(
            "| Composite | {} | {} | {} |\n",
            fmt_us(avg_a.composite_us),
            fmt_us(avg_b.composite_us),
            fmt_change(avg_a.composite_us, avg_b.composite_us),
        ));
        out.push('\n');
    }

    // ── Root Cause ──
    // Collect root cause metrics: Style Elements, IO, Layout Dirty
    let mut root_cause_rows: Vec<String> = Vec::new();

    if cmp.style_recalc_a.total_count > 0 || cmp.style_recalc_b.total_count > 0 {
        root_cause_rows.push(format!(
            "| Style Elements (avg) | {:.0} | {:.0} | {} |",
            cmp.style_recalc_a.avg_elements,
            cmp.style_recalc_b.avg_elements,
            fmt_change_count(
                cmp.style_recalc_a.avg_elements,
                cmp.style_recalc_b.avg_elements
            ),
        ));
        root_cause_rows.push(format!(
            "| Style Elements (max) | {} | {} | {} |",
            cmp.style_recalc_a.max_elements,
            cmp.style_recalc_b.max_elements,
            fmt_change_count(
                cmp.style_recalc_a.max_elements as f64,
                cmp.style_recalc_b.max_elements as f64
            ),
        ));
    }

    root_cause_rows.push(format!(
        "| Layout Dirty (avg) | {:.0} | {:.0} | {} |",
        cmp.layout_a.avg_dirty,
        cmp.layout_b.avg_dirty,
        fmt_change_count(cmp.layout_a.avg_dirty, cmp.layout_b.avg_dirty),
    ));

    // IntersectionObserver from event rows (scroll-related)
    let io_name = "IntersectionObserverController::computeIntersections";
    if let Some(r) = cmp.rows.iter().find(|r| r.event_name == io_name)
        && (r.avg_a_us > 0.0 || r.avg_b_us > 0.0)
    {
        root_cause_rows.push(format!(
            "| IO (avg) | {} | {} | {} |",
            fmt_us(r.avg_a_us),
            fmt_us(r.avg_b_us),
            fmt_change(r.avg_a_us, r.avg_b_us),
        ));
    }

    if !root_cause_rows.is_empty() {
        out.push_str("### Root Cause\n\n");
        out.push_str("| Metric | Before | After | Change |\n");
        out.push_str("|--------|--------|-------|--------|\n");
        for row in &root_cause_rows {
            out.push_str(row);
            out.push('\n');
        }
        out.push('\n');
    }

    // ── Regressions (scroll-related only) ──
    if let (Some(avg_a), Some(avg_b)) = (&cmp.scroll_avg_a, &cmp.scroll_avg_b) {
        let scroll_cats: Vec<(&str, f64, f64)> = vec![
            ("JS", avg_a.js_us, avg_b.js_us),
            ("Style (ULT)", avg_a.ult_us, avg_b.ult_us),
            ("Layout", avg_a.layout_us, avg_b.layout_us),
            ("Paint", avg_a.paint_us, avg_b.paint_us),
            ("HitTest", avg_a.hit_test_us, avg_b.hit_test_us),
            ("Composite", avg_a.composite_us, avg_b.composite_us),
        ];

        let regressions: Vec<_> = scroll_cats
            .iter()
            .filter(|(_, a, b)| {
                if *a > 0.0 {
                    let d = (b - a) / a * 100.0;
                    d > 5.0
                } else {
                    *b > 0.0
                }
            })
            .collect();

        if !regressions.is_empty() {
            out.push_str("### Regressions\n\n");
            out.push_str("| Metric | Before | After | Change |\n");
            out.push_str("|--------|--------|-------|--------|\n");
            for (name, a, b) in &regressions {
                out.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    name,
                    fmt_us(*a),
                    fmt_us(*b),
                    fmt_change(*a, *b),
                ));
            }
            out.push('\n');
        }
    }

    // ── Notes (non-scroll context) ──
    let sa = &cmp.summary_a;
    let sb = &cmp.summary_b;

    let mut notes: Vec<String> = Vec::new();

    // Total Blocking Time (TBT)
    let tbt_change = pct_diff(sa.total_blocking_time_us, sb.total_blocking_time_us);
    notes.push(format!(
        "Total Blocking Time (TBT): {} → {} ({}).",
        fmt_us(sa.total_blocking_time_us),
        fmt_us(sb.total_blocking_time_us),
        tbt_change,
    ));

    // Long Tasks
    let lt_change = if sa.long_task_count > 0 {
        pct_diff_count(sa.long_task_count as f64, sb.long_task_count as f64).to_string()
    } else {
        "-".to_string()
    };
    notes.push(format!(
        "Long Tasks: {} → {} ({}). Includes non-scroll tasks.",
        sa.long_task_count, sb.long_task_count, lt_change,
    ));

    // Dropped Frames
    if cmp.jank_a.total_dropped > 0 || cmp.jank_b.total_dropped > 0 {
        let diff = cmp.jank_b.total_dropped as isize - cmp.jank_a.total_dropped as isize;
        notes.push(format!(
            "Dropped Frames: {} → {} ({:+}).",
            cmp.jank_a.total_dropped, cmp.jank_b.total_dropped, diff,
        ));
    }

    // Forced Reflows
    if cmp.forced_reflow_a.total_reflows > 0 || cmp.forced_reflow_b.total_reflows > 0 {
        let reflow_diff = pct_diff_count(
            cmp.forced_reflow_a.total_reflows as f64,
            cmp.forced_reflow_b.total_reflows as f64,
        );
        notes.push(format!(
            "Forced Reflows: {} → {} ({}). Thrashing time: {} → {}.",
            cmp.forced_reflow_a.total_reflows,
            cmp.forced_reflow_b.total_reflows,
            reflow_diff,
            fmt_us(cmp.forced_reflow_a.total_layout_time_us),
            fmt_us(cmp.forced_reflow_b.total_layout_time_us),
        ));
    }

    // Main Thread Busy
    let busy_change = pct_diff(sa.main_thread_busy_us, sb.main_thread_busy_us);
    notes.push(format!(
        "Main Thread Busy: {} → {} ({}). Includes non-scroll tasks.",
        fmt_us(sa.main_thread_busy_us),
        fmt_us(sb.main_thread_busy_us),
        busy_change,
    ));

    // GC (MajorGC + MinorGC)
    let gc_names = ["MajorGC", "MinorGC"];
    for gc_name in &gc_names {
        if let Some(r) = cmp.rows.iter().find(|r| r.event_name == *gc_name)
            && (r.avg_a_us > 0.0 || r.avg_b_us > 0.0)
        {
            notes.push(format!(
                "{}: avg {} → {} ({})",
                gc_name,
                fmt_us(r.avg_a_us),
                fmt_us(r.avg_b_us),
                pct_diff(r.avg_a_us, r.avg_b_us),
            ));
        }
    }

    if !notes.is_empty() {
        out.push_str("### Notes\n\n");
        for note in &notes {
            out.push_str(&format!("- {}\n", note));
        }
        out.push('\n');
    }
}

fn export_compare(out: &mut String, cmp: &CompareResult, name_a: &str, name_b: &str) {
    out.push_str(&format!(
        "## Comparison: {} (A) vs {} (B)\n\n",
        name_a, name_b
    ));

    // Key Findings
    if !cmp.findings.is_empty() {
        out.push_str("### Key Findings\n\n");
        for f in &cmp.findings {
            let icon = match f.severity {
                FindingSeverity::Improved => "+",
                FindingSeverity::Regressed => "!",
                FindingSeverity::Neutral => "=",
            };
            let label = match f.severity {
                FindingSeverity::Improved => "IMPROVED",
                FindingSeverity::Regressed => "REGRESSED",
                FindingSeverity::Neutral => "NEUTRAL",
            };
            if f.detail.is_empty() {
                out.push_str(&format!(
                    "- **[{}]** {} {}: {}\n",
                    icon, label, f.category, f.message
                ));
            } else {
                out.push_str(&format!(
                    "- **[{}]** {} {}: {} ({})\n",
                    icon, label, f.category, f.message, f.detail
                ));
            }
        }
        out.push('\n');
    }

    // Summary stats comparison
    out.push_str("### Quick Stats\n\n");
    out.push_str("| Metric | A | B | Change |\n");
    out.push_str("|--------|---|---|--------|\n");

    let sa = &cmp.summary_a;
    let sb = &cmp.summary_b;

    out.push_str(&format!(
        "| Total Blocking Time (TBT) | {} | {} | {} |\n",
        fmt_us(sa.total_blocking_time_us),
        fmt_us(sb.total_blocking_time_us),
        pct_diff(sa.total_blocking_time_us, sb.total_blocking_time_us),
    ));
    out.push_str(&format!(
        "| Long Tasks | {} | {} | {} |\n",
        sa.long_task_count,
        sb.long_task_count,
        pct_diff_count(sa.long_task_count as f64, sb.long_task_count as f64),
    ));
    out.push_str(&format!(
        "| Worst Task | {} | {} | {} |\n",
        fmt_us(sa.long_tasks_top.first().copied().unwrap_or(0.0)),
        fmt_us(sb.long_tasks_top.first().copied().unwrap_or(0.0)),
        pct_diff(
            sa.long_tasks_top.first().copied().unwrap_or(0.0),
            sb.long_tasks_top.first().copied().unwrap_or(0.0)
        ),
    ));
    out.push_str(&format!(
        "| Dropped Frames | {} | {} | {} |\n",
        cmp.jank_a.total_dropped,
        cmp.jank_b.total_dropped,
        if cmp.jank_a.total_dropped == cmp.jank_b.total_dropped {
            "0%".to_string()
        } else {
            format!(
                "{:+}",
                cmp.jank_b.total_dropped as isize - cmp.jank_a.total_dropped as isize
            )
        },
    ));
    out.push_str(&format!(
        "| Forced Reflows | {} | {} | {} |\n",
        cmp.forced_reflow_a.total_reflows,
        cmp.forced_reflow_b.total_reflows,
        pct_diff_count(
            cmp.forced_reflow_a.total_reflows as f64,
            cmp.forced_reflow_b.total_reflows as f64
        ),
    ));
    out.push_str(&format!(
        "| Main Thread Busy | {} | {} | {} |\n",
        fmt_us(sa.main_thread_busy_us),
        fmt_us(sb.main_thread_busy_us),
        pct_diff(sa.main_thread_busy_us, sb.main_thread_busy_us),
    ));
    out.push_str(&format!(
        "| Layout Dirty (avg) | {:.0} | {:.0} | {} |\n",
        cmp.layout_a.avg_dirty,
        cmp.layout_b.avg_dirty,
        pct_diff_count(cmp.layout_a.avg_dirty, cmp.layout_b.avg_dirty),
    ));
    if cmp.style_recalc_a.total_count > 0 || cmp.style_recalc_b.total_count > 0 {
        out.push_str(&format!(
            "| Style Elements (avg) | {:.0} | {:.0} | {} |\n",
            cmp.style_recalc_a.avg_elements,
            cmp.style_recalc_b.avg_elements,
            pct_diff_count(
                cmp.style_recalc_a.avg_elements,
                cmp.style_recalc_b.avg_elements
            ),
        ));
        out.push_str(&format!(
            "| Style Elements (max) | {} | {} | {} |\n",
            cmp.style_recalc_a.max_elements,
            cmp.style_recalc_b.max_elements,
            pct_diff_count(
                cmp.style_recalc_a.max_elements as f64,
                cmp.style_recalc_b.max_elements as f64
            ),
        ));
    }
    out.push('\n');

    // Scroll frame comparison
    if let (Some(avg_a), Some(avg_b)) = (&cmp.scroll_avg_a, &cmp.scroll_avg_b) {
        out.push_str("### Scroll Frame Comparison\n\n");
        out.push_str("| Metric | A | B | Change |\n");
        out.push_str("|--------|---|---|--------|\n");
        out.push_str(&format!(
            "| Tasks | {} | {} | {} |\n",
            cmp.scroll_count_a,
            cmp.scroll_count_b,
            pct_diff_count(cmp.scroll_count_a as f64, cmp.scroll_count_b as f64)
        ));
        out.push_str(&format!(
            "| Avg Duration | {} | {} | {} |\n",
            fmt_us(avg_a.dur_us),
            fmt_us(avg_b.dur_us),
            pct_diff(avg_a.dur_us, avg_b.dur_us)
        ));
        out.push_str(&format!(
            "| P50 | {} | {} | {} |\n",
            fmt_us(cmp.scroll_pct_a.p50_us),
            fmt_us(cmp.scroll_pct_b.p50_us),
            pct_diff(cmp.scroll_pct_a.p50_us, cmp.scroll_pct_b.p50_us)
        ));
        out.push_str(&format!(
            "| P90 | {} | {} | {} |\n",
            fmt_us(cmp.scroll_pct_a.p90_us),
            fmt_us(cmp.scroll_pct_b.p90_us),
            pct_diff(cmp.scroll_pct_a.p90_us, cmp.scroll_pct_b.p90_us)
        ));
        out.push_str(&format!(
            "| P99 | {} | {} | {} |\n\n",
            fmt_us(cmp.scroll_pct_a.p99_us),
            fmt_us(cmp.scroll_pct_b.p99_us),
            pct_diff(cmp.scroll_pct_a.p99_us, cmp.scroll_pct_b.p99_us)
        ));

        out.push_str("#### Cost Breakdown (Average per Task)\n\n");
        out.push_str("| Category | A | B | Change |\n");
        out.push_str("|----------|---|---|--------|\n");

        let cats = [
            ("JS", avg_a.js_us, avg_b.js_us),
            ("Style (ULT)", avg_a.ult_us, avg_b.ult_us),
            ("Layout", avg_a.layout_us, avg_b.layout_us),
            ("Paint", avg_a.paint_us, avg_b.paint_us),
            ("Composite", avg_a.composite_us, avg_b.composite_us),
            ("HitTest", avg_a.hit_test_us, avg_b.hit_test_us),
        ];

        for (name, a, b) in &cats {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                name,
                fmt_us(*a),
                fmt_us(*b),
                pct_diff(*a, *b),
            ));
        }
        out.push_str(&format!(
            "\n- **A Bottleneck**: {} ({} tasks)\n",
            avg_a.bottleneck(),
            cmp.scroll_count_a
        ));
        out.push_str(&format!(
            "- **B Bottleneck**: {} ({} tasks)\n\n",
            avg_b.bottleneck(),
            cmp.scroll_count_b
        ));
    }

    // Event comparison (Total, Avg, Count)
    out.push_str("### Per-Event Comparison\n\n");
    out.push_str("| Event | Total (A) | Total (B) | Δ Total | Avg (A) | Avg (B) | Δ Avg | Count (A) | Count (B) |\n");
    out.push_str("|-------|-----------|-----------|---------|---------|---------|-------|-----------|-----------|\n");
    for r in &cmp.rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.event_name,
            fmt_us(r.total_a_us),
            fmt_us(r.total_b_us),
            pct_diff(r.total_a_us, r.total_b_us),
            fmt_us(r.avg_a_us),
            fmt_us(r.avg_b_us),
            pct_diff(r.avg_a_us, r.avg_b_us),
            r.count_a,
            r.count_b,
        ));
    }
    out.push('\n');

    // CPU profile diff
    if !cmp.cpu_diff.is_empty() {
        out.push_str("### CPU Profile Diff (Top Functions by Impact)\n\n");
        out.push_str(
            "| Source | Function | Time (A) | Time (B) | Δ Time | A % | B % | Change (pp) |\n",
        );
        out.push_str(
            "|--------|----------|----------|----------|--------|-----|-----|-------------|\n",
        );
        for d in cmp.cpu_diff.iter().take(20) {
            let name = if d.function_name.is_empty() {
                "(anonymous)"
            } else {
                &d.function_name
            };
            let pp = d.pct_b - d.pct_a;
            let pp_str = if pp > 0.0 {
                format!("+{:.1}pp", pp)
            } else {
                format!("{:.1}pp", pp)
            };
            let diff_time_str = if d.diff_time_us > 0.0 {
                format!("+{}", fmt_us(d.diff_time_us))
            } else {
                fmt_us(d.diff_time_us)
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {:.1}% | {:.1}% | {} |\n",
                d.source_type.label(),
                name,
                fmt_us(d.time_a_us),
                fmt_us(d.time_b_us),
                diff_time_str,
                d.pct_a,
                d.pct_b,
                pp_str,
            ));
        }
        out.push('\n');
    }
}
