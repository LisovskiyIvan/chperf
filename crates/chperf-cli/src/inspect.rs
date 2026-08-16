//! CLI-side inspect dispatch: turn `Cli` flags + parsed events into the
//! requested `chperf_core` sections, then render them as Markdown / JSON /
//! CSV. Shared by the single-trace CLI path, the REPL, and `--compare`.

use crate::cli::Cli;
use chperf_core::{analysis, inspect, trace, windowed};
use serde_json::Value;

/// Windowed comparison of two traces: run every requested section on both,
/// and when `--delta` is set, merge the PRE/SHOOT/POST metric rows into a
/// single A-vs-B table (SHOOT, SHOOT−PRE, and the inter-trace deltas).
pub(crate) fn inspect_compare_output(
    events_a: &[trace::TraceEvent],
    name_a: &str,
    events_b: &[trace::TraceEvent],
    name_b: &str,
    cli: &Cli,
) -> Result<(), Box<dyn std::error::Error>> {
    let min_ts_a = inspect::trace_start_us(events_a);
    let min_ts_b = inspect::trace_start_us(events_b);
    let ws_a = resolve_windows(events_a, min_ts_a, cli)?;
    let ws_b = resolve_windows(events_b, min_ts_b, cli)?;

    // --flame: folded stacks for both traces, concatenated (one flamegraph).
    if cli.flame {
        let matcher = cli
            .function
            .as_deref()
            .map(|p| inspect::Matcher::new(p, cli.regex))
            .transpose()?;
        print!("{}", inspect::stacks_folded(events_a, matcher.as_ref(), &ws_a.scope, None));
        print!("{}", inspect::stacks_folded(events_b, matcher.as_ref(), &ws_b.scope, None));
        return Ok(());
    }

    // Compute delta data once per trace: the per-trace delta section and the
    // merged A-vs-B table share the same raw data (a full event sweep + a
    // multi-window CPU scan), so computing it twice would double that work.
    let compare = if cli.delta {
        match (
            ws_a.pre, ws_a.shoot, ws_a.post, ws_a.anchor_ts,
            ws_b.pre, ws_b.shoot, ws_b.post, ws_b.anchor_ts,
        ) {
            (Some(pa), Some(sa), Some(qa), Some(aa), Some(pb), Some(sb), Some(qb), Some(ab)) => {
                let da = windowed::delta_data(events_a, pa, sa, qa, aa, &cli.frame_event, cli.lt);
                let db = windowed::delta_data(events_b, pb, sb, qb, ab, &cli.frame_event, cli.lt);
                Some((da, db))
            }
            _ => None,
        }
    } else {
        None
    };

    let sections_a = build_sections(events_a, min_ts_a, &ws_a, cli, compare.as_ref().map(|(da, _)| da), None)?;
    let sections_b = build_sections(events_b, min_ts_b, &ws_b, cli, compare.as_ref().map(|(_, db)| db), None)?;

    if cli.json {
        let mut obj = serde_json::Map::new();
        obj.insert("trace_a".into(), serde_json::json!(name_a));
        obj.insert("trace_b".into(), serde_json::json!(name_b));
        if let Some(ref note) = ws_a.anchor_note {
            obj.insert("anchor_a".into(), serde_json::json!(note));
        }
        if let Some(ref note) = ws_b.anchor_note {
            obj.insert("anchor_b".into(), serde_json::json!(note));
        }
        let mut secs_a = serde_json::Map::new();
        for (k, _, j) in &sections_a {
            secs_a.insert((*k).into(), j.clone());
        }
        let mut secs_b = serde_json::Map::new();
        for (k, _, j) in &sections_b {
            secs_b.insert((*k).into(), j.clone());
        }
        obj.insert("sections_a".into(), serde_json::Value::Object(secs_a));
        obj.insert("sections_b".into(), serde_json::Value::Object(secs_b));
        obj.insert("compare".into(), compare_json(&compare));
        println!("{}", serde_json::to_string_pretty(&serde_json::Value::Object(obj))?);
        return Ok(());
    }

    if cli.csv {
        csv_blocks(&sections_a, Some("a"));
        csv_blocks(&sections_b, Some("b"));
        if let Some((da, db)) = &compare {
            println!("# compare");
            print!("{}", windowed::rows_to_csv(&compare_csv_rows(da, db)));
        }
        return Ok(());
    }

    let mut out = String::new();
    out.push_str(&format!("# chperf inspect compare: {} vs {}\n\n", name_a, name_b));
    if let Some(ref note) = ws_a.anchor_note {
        out.push_str(&format!("**A ({}):** {}\n", name_a, note));
    }
    if let Some(ref note) = ws_b.anchor_note {
        out.push_str(&format!("**B ({}):** {}\n", name_b, note));
    }
    out.push('\n');
    out.push_str(&format!("## Trace A: {}\n\n", name_a));
    for (_, m, _) in &sections_a {
        out.push_str(m);
    }
    out.push_str(&format!("## Trace B: {}\n\n", name_b));
    for (_, m, _) in &sections_b {
        out.push_str(m);
    }
    match &compare {
        Some((da, db)) => {
            out.push_str("## Windowed compare: SHOOT & SHOOT−PRE\n\n");
            out.push_str(&format!(
                "- **A SHOOT**: {:.2}ms … {:.2}ms from trace start\n",
                (da.shoot.0 - min_ts_a) / 1000.0,
                (da.shoot.1 - min_ts_a) / 1000.0,
            ));
            out.push_str(&format!(
                "- **B SHOOT**: {:.2}ms … {:.2}ms from trace start\n\n",
                (db.shoot.0 - min_ts_b) / 1000.0,
                (db.shoot.1 - min_ts_b) / 1000.0,
            ));
            out.push_str("| metric | A SHOOT | A Δ | B SHOOT | B Δ | B−A SHOOT | B−A Δ |\n");
            out.push_str("|--------|---------|-----|---------|-----|-----------|-------|\n");
            // Render rows from raw data: n = counts, else ms.
            for row in &da.rows {
                let rb = db.rows.iter().find(|b| b.metric == row.metric);
                let Some(rb) = rb else { continue };
                let counts = row.unit == "n";
                let fmt = |v: f64, counts: bool| {
                    if counts { format!("{:.0}", v) } else { format!("{:.1}", v / 1000.0) }
                };
                let dfmt = |v: f64, counts: bool| {
                    if counts { format!("{:+}", v as i64) } else { format!("{:+.1}", v / 1000.0) }
                };
                let diff_shoot = rb.shoot - row.shoot;
                let diff_delta = rb.delta_pre() - row.delta_pre();
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} | {} |\n",
                    row.metric,
                    fmt(row.shoot, counts),
                    dfmt(row.delta_pre(), counts),
                    fmt(rb.shoot, counts),
                    dfmt(rb.delta_pre(), counts),
                    dfmt(diff_shoot, counts),
                    dfmt(diff_delta, counts),
                ));
            }
            out.push('\n');
        }
        None => {
            if cli.delta {
                out.push_str(
                    "**--delta requires an anchor in both traces: --anchor <substr>, --around <ms> or --worst.**\n\n",
                );
            }
        }
    }
    print!("{}", out);
    Ok(())
}

/// Raw compare rows as JSON values (µs for `ms` metrics, counts for `n`).
pub(crate) fn compare_csv_rows(
    da: &windowed::DeltaData,
    db: &windowed::DeltaData,
) -> Vec<Value> {
    let mut rows = Vec::new();
    for row in &da.rows {
        let Some(rb) = db.rows.iter().find(|b| b.metric == row.metric) else { continue };
        rows.push(serde_json::json!({
            "metric": row.metric,
            "unit": row.unit,
            "a_shoot": row.shoot,
            "a_delta": row.delta_pre(),
            "b_shoot": rb.shoot,
            "b_delta": rb.delta_pre(),
            "diff_shoot": rb.shoot - row.shoot,
            "diff_delta": rb.delta_pre() - row.delta_pre(),
        }));
    }
    rows
}

pub(crate) fn compare_json(compare: &Option<(windowed::DeltaData, windowed::DeltaData)>) -> Value {
    match compare {
        Some((da, db)) => {
            let rows = compare_csv_rows(da, db);
            serde_json::json!({
                "anchor_a_us": da.anchor_us.round(),
                "anchor_b_us": db.anchor_us.round(),
                "windows_a": {
                    "pre": [da.pre.0.round(), da.pre.1.round()],
                    "shoot": [da.shoot.0.round(), da.shoot.1.round()],
                    "post": [da.post.0.round(), da.post.1.round()],
                },
                "windows_b": {
                    "pre": [db.pre.0.round(), db.pre.1.round()],
                    "shoot": [db.shoot.0.round(), db.shoot.1.round()],
                    "post": [db.post.0.round(), db.post.1.round()],
                },
                "rows": rows,
            })
        }
        None => Value::Null,
    }
}

/// Shared inspect dispatch: computes sections from already-parsed events.
/// Used by the CLI (`chperf trace --events ...`), the REPL, and (with
/// `--compare`) the windowed two-trace comparison.
pub(crate) fn inspect_output(
    events: &[trace::TraceEvent],
    min_ts: f64,
    trace_name: &str,
    cli: &Cli,
    cpu_cache: Option<&analysis::CpuProfileCache>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ws = resolve_windows(events, min_ts, cli)?;

    // --flame: raw folded stacks for flamegraph.pl / speedscope (no markdown/json).
    if cli.flame {
        let matcher = cli
            .function
            .as_deref()
            .map(|p| inspect::Matcher::new(p, cli.regex))
            .transpose()?;
        print!("{}", inspect::stacks_folded(events, matcher.as_ref(), &ws.scope, cpu_cache));
        return Ok(());
    }

    let sections = build_sections(events, min_ts, &ws, cli, None, cpu_cache)?;
    dispatch_output(&sections, trace_name, min_ts, &ws, cli)
}

/// Resolved anchor/window state, shared by single-trace and compare output.
struct WindowState {
    scope: inspect::Scope,
    pre: Option<(f64, f64)>,
    shoot: Option<(f64, f64)>,
    post: Option<(f64, f64)>,
    anchor_ts: Option<f64>,
    anchor_note: Option<String>,
}

/// Determine the time anchor (--worst > --anchor > --around, all relative to
/// the metadata-free trace start) and derive the SHOOT/PRE/POST windows.
fn resolve_windows(
    events: &[trace::TraceEvent],
    min_ts: f64,
    cli: &Cli,
) -> Result<WindowState, Box<dyn std::error::Error>> {
    // Resolve --tid (numeric or "main" → auto-detected main thread).
    let tid = match &cli.tid {
        None => None,
        Some(s) if s == "main" => Some(trace::detect_main_thread(events)),
        Some(s) => Some(s.parse::<u64>().map_err(|e| {
            format!("invalid --tid `{}` (use a number or \"main\"): {}", s, e)
        })?),
    };

    // Pre-window scope (for --worst search): tid/pid/cat, no window.
    let scope_nowin = inspect::Scope {
        window: None,
        tid,
        pid: cli.pid,
        cat: cli.cat.as_deref().map(|c| c.to_lowercase()),
    };

    let mut anchor_note: Option<String> = None;
    let mut anchor_ts: Option<f64> = None;
    let mut window_ms = cli.window;
    if cli.worst {
        if let Some((ts, dur)) = inspect::worst_runtask(events, &scope_nowin) {
            let w = cli.window.unwrap_or_else(|| (dur / 1000.0 / 2.0).max(50.0));
            window_ms = Some(w);
            anchor_note = Some(format!(
                "Anchored at worst RunTask: t={:.2}ms, dur={:.2}ms (window ±{:.0}ms)",
                (ts - min_ts) / 1000.0,
                dur / 1000.0,
                w
            ));
            anchor_ts = Some(ts);
        }
    } else if let Some(pattern) = &cli.anchor {
        let m = inspect::Matcher::new(pattern, cli.regex)?;
        match windowed::find_anchor(events, &m) {
            Some(a) => {
                anchor_ts = Some(a.ts);
                anchor_note = Some(format!(
                    "Anchored at {} `{}` — t={:.2}ms",
                    a.kind,
                    a.label,
                    (a.ts - min_ts) / 1000.0
                ));
            }
            None => {
                eprintln!("warning: --anchor {} matched nothing", pattern);
            }
        }
    } else {
        anchor_ts = cli.around.map(|ms| min_ts + ms * 1000.0);
    }

    // SHOOT = anchor ± window (half-width). PRE and POST sit adjacent and
    // exist for --delta comparisons.
    let half_ms = window_ms.unwrap_or(100.0);
    let shoot = anchor_ts.map(|ts| (ts - half_ms * 1000.0, ts + half_ms * 1000.0));
    let pre = anchor_ts.map(|ts| {
        (
            ts - (cli.pre + half_ms) * 1000.0,
            ts - half_ms * 1000.0,
        )
    });
    let post = anchor_ts.map(|ts| {
        (
            ts + half_ms * 1000.0,
            ts + (half_ms + cli.post) * 1000.0,
        )
    });
    let scope = inspect::Scope {
        window: shoot,
        tid,
        pid: cli.pid,
        cat: scope_nowin.cat.clone(),
    };

    Ok(WindowState {
        scope,
        pre,
        shoot,
        post,
        anchor_ts,
        anchor_note,
    })
}

/// One inspect section: stable key, Markdown, JSON rows.
type Section = (&'static str, String, Value);

/// Build all requested inspect sections for one trace, scoped to `ws`.
fn build_sections(
    events: &[trace::TraceEvent],
    min_ts: f64,
    ws: &WindowState,
    cli: &Cli,
    delta: Option<&windowed::DeltaData>,
    cpu_cache: Option<&analysis::CpuProfileCache>,
) -> Result<Vec<Section>, Box<dyn std::error::Error>> {
    let scope = &ws.scope;
    let sort = match cli.sort.as_deref() {
        Some("dur") => inspect::Sort::Dur,
        Some("name") => inspect::Sort::Name,
        Some("count") => inspect::Sort::Count,
        _ => inspect::Sort::Ts,
    };
    let min_dur_us = cli.min_dur.unwrap_or(0.0);

    let mut sections: Vec<Section> = Vec::new();

    // --jank: whole-trace cluster detection (dropped frames / sub-threshold spikes).
    // With a window scope, detection is restricted to the window.
    if cli.jank {
        let (m, j) = inspect::jank_section(events, scope, cli.top, min_ts);
        sections.push(("jank", m, j));
    }

    if cli.memory {
        let (m, j) = inspect::memory_section(events, scope, cli.top, min_ts);
        sections.push(("memory", m, j));
    }
    if cli.input {
        let (m, j) = inspect::input_section(events, scope, cli.top, min_ts);
        sections.push(("input", m, j));
    }
    if cli.async_ {
        let (m, j) = inspect::async_section(events, scope, cli.top, min_ts);
        sections.push(("async", m, j));
    }

    if cli.timeline {
        let (m, j) = inspect::timeline_section(events, scope, cli.bucket, min_ts);
        sections.push(("timeline", m, j));
    }
    if cli.task {
        let (m, j) = inspect::task_section(events, scope, cli.top, min_ts);
        sections.push(("task", m, j));
    }
    if cli.names {
        let (m, j) = inspect::names_section(events, scope, sort, cli.top, min_ts);
        sections.push(("names", m, j));
    }
    if cli.threads {
        let (m, j) = inspect::threads_section(events, scope, cli.top, min_ts);
        sections.push(("threads", m, j));
    }

    if let Some(names_raw) = &cli.events {
        let names: Vec<String> = names_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let filter = inspect::NameFilter::new(&names, cli.regex)?;
        let (m, j) = if cli.stats {
            inspect::stats_section(events, &filter, names_raw.trim(), scope, min_dur_us, min_ts)
        } else {
            inspect::events_section(
                events,
                &filter,
                names_raw.trim(),
                scope,
                min_dur_us,
                sort,
                cli.full_args,
                cli.top,
                min_ts,
            )
        };
        sections.push((if cli.stats { "stats" } else { "events" }, m, j));
    }

    // Functions and stacks can share a single matcher (filter by name).
    let func_matcher = if let Some(pattern) = &cli.function {
        Some(inspect::Matcher::new(pattern, cli.regex)?)
    } else {
        None
    };
    if let Some(m) = &func_matcher {
        let (md, j) = inspect::functions_section(events, m, scope, cli.top, min_ts, cpu_cache);
        sections.push(("functions", md, j));
    }
    if cli.stacks {
        let (md, j) = inspect::stacks_section(events, func_matcher.as_ref(), scope, cli.top, min_ts, cpu_cache);
        sections.push(("stacks", md, j));
    }
    if cli.calltree {
        let url_matcher = if let Some(p) = &cli.url {
            Some(inspect::Matcher::new(p, cli.regex)?)
        } else {
            None
        };
        let (md, j) = windowed::calltree_section(
            events,
            scope,
            func_matcher.as_ref(),
            url_matcher.as_ref(),
            cli.top,
            min_ts,
            cpu_cache,
        );
        sections.push(("calltree", md, j));
    }
    if cli.gc {
        let (md, j) = windowed::gc_section(events, scope, cli.lt, min_ts);
        sections.push(("gc", md, j));
    }
    if cli.frames {
        let (md, j) = windowed::frames_section(events, scope, &cli.frame_event, min_ts);
        sections.push(("frames", md, j));
    }
    if let Some(needle) = &cli.find {
        let m = inspect::Matcher::new(needle, cli.regex)?;
        let (md, j) = inspect::find_section(events, &m, scope, cli.full_args, cli.top, min_ts, cpu_cache);
        sections.push(("find", md, j));
    }

    // --delta: PRE/SHOOT/POST comparison around the anchor.
    if cli.delta {
        match (ws.pre, ws.shoot, ws.post, ws.anchor_ts) {
            (Some(p), Some(s), Some(q), Some(a)) => {
                let (md, j) = match delta {
                    Some(d) => windowed::delta_section_from_data(d, min_ts),
                    None => windowed::delta_section(
                        events,
                        p,
                        s,
                        q,
                        a,
                        &cli.frame_event,
                        cli.lt,
                        min_ts,
                    ),
                };
                sections.push(("delta", md, j));
            }
            _ => sections.push((
                "delta",
                "**--delta requires an anchor: --anchor <substr>, --around <ms> or --worst.**\n\n"
                    .to_string(),
                Value::Null,
            )),
        }
    }

    // --worst with no other section: default to listing RunTask around the anchor.
    if cli.worst && sections.is_empty() {
        let names = vec!["RunTask".to_string()];
        let filter = inspect::NameFilter::new(&names, false)?;
        let (md, j) = inspect::events_section(
            events,
            &filter,
            "RunTask",
            scope,
            0.0,
            inspect::Sort::Dur,
            cli.full_args,
            cli.top,
            min_ts,
        );
        sections.push(("events", md, j));
    }

    Ok(sections)
}

/// Render collected sections as JSON / CSV / Markdown.
fn dispatch_output(
    sections: &[Section],
    trace_name: &str,
    min_ts: f64,
    ws: &WindowState,
    cli: &Cli,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = String::new();
    if cli.json {
        let mut obj = serde_json::Map::new();
        obj.insert("trace".into(), serde_json::json!(trace_name));
        if let Some(ref note) = ws.anchor_note {
            obj.insert("anchor".into(), serde_json::json!(note));
        }
        obj.insert(
            "window".into(),
            match ws.shoot {
                Some((lo, hi)) => serde_json::json!([
                    ((lo - min_ts) / 1000.0).round(),
                    ((hi - min_ts) / 1000.0).round(),
                ]),
                None => serde_json::Value::Null,
            },
        );
        let mut secs = serde_json::Map::new();
        for (k, _, j) in sections {
            secs.insert((*k).into(), j.clone());
        }
        obj.insert("sections".into(), serde_json::Value::Object(secs));
        println!("{}", serde_json::to_string_pretty(&serde_json::Value::Object(obj))?);
        return Ok(());
    }
    if cli.csv {
        csv_blocks(sections, None);
        return Ok(());
    }
    out.push_str(&format!("# chperf inspect: {}\n\n", trace_name));
    if let Some(ref note) = ws.anchor_note {
        out.push_str(&format!("**{}**\n\n", note));
    }
    if let Some((lo, hi)) = ws.shoot {
        out.push_str(&format!(
            "**Window**: {:.2}ms … {:.2}ms from trace start\n\n",
            (lo - min_ts) / 1000.0,
            (hi - min_ts) / 1000.0,
        ));
    }
    for (_, m, _) in sections {
        out.push_str(m);
    }
    print!("{}", out);
    Ok(())
}

/// Emit one CSV block per section, prefixed by a `# <prefix><key>` comment
/// line. Sections that emit an object (e.g. --delta) render each array
/// field as its own block.
fn csv_blocks(sections: &[Section], prefix: Option<&str>) {
    for (k, _, j) in sections {
        let label = match prefix {
            Some(p) => format!("{}.{}", p, k),
            None => (*k).to_string(),
        };
        match j {
            Value::Array(rows) => {
                println!("# {}", label);
                print!("{}", windowed::rows_to_csv(rows));
            }
            Value::Object(o) => {
                for (field, v) in o {
                    if let Value::Array(rows) = v {
                        println!("# {}.{}", label, field);
                        print!("{}", windowed::rows_to_csv(rows));
                    }
                }
            }
            _ => {}
        }
    }
}
