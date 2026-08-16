mod analysis;
mod app;
mod export;
mod inspect;
mod repl;
mod trace;
mod ui;
mod windowed;

use std::io;
use std::path::{Path, PathBuf};

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;
use serde_json::Value;

#[derive(Parser)]
#[command(name = "chperf", about = "Chrome DevTools Trace JSON analyzer (TUI)")]
pub(crate) struct Cli {
    /// Path to trace JSON file (.json or .json.gz) or a directory of traces
    pub(crate) trace: Option<String>,

    /// Interactive REPL: load and analyze once, then query live
    #[arg(long)]
    pub(crate) repl: bool,

    /// Optional second trace file for comparison
    #[arg(short, long)]
    pub(crate) compare: Option<String>,

    /// Export analysis as Markdown (to stdout or file)
    /// Use --export to print to stdout, --export=FILE to write to file
    #[arg(short, long, num_args = 0..=1, default_missing_value = "-")]
    pub(crate) export: Option<String>,

    /// CPU throttle factor (e.g. --throttle 20 divides all times by 20)
    #[arg(short, long)]
    pub(crate) throttle: Option<f64>,

    /// Export only the comparison summary table (use with --export --compare)
    #[arg(short, long)]
    pub(crate) summary: bool,

    /// Inspect: list events by name (comma-separated), e.g. --events GPUTask,RunTask
    #[arg(long)]
    pub(crate) events: Option<String>,

    /// Inspect: aggregate CPU samples whose function name contains this substring
    #[arg(long)]
    pub(crate) function: Option<String>,

    /// Inspect: search event args (JSON) for this substring
    #[arg(long)]
    pub(crate) find: Option<String>,

    /// Inspect: center of the time window, in ms from trace start (use with --window)
    #[arg(long)]
    pub(crate) around: Option<f64>,

    /// Inspect: half-width of the time window in ms (default 100, use with --around)
    #[arg(long)]
    pub(crate) window: Option<f64>,

    /// Inspect: only events with duration >= this value, in microseconds
    #[arg(long)]
    pub(crate) min_dur: Option<f64>,

    /// Inspect: limit number of rows (default 30)
    #[arg(long, default_value_t = 30)]
    pub(crate) top: usize,

    /// Inspect: restrict events/functions/find to this thread (numeric tid or "main")
    #[arg(long)]
    pub(crate) tid: Option<String>,

    /// Inspect: restrict events/functions/find to this process id (pid)
    #[arg(long)]
    pub(crate) pid: Option<u64>,

    /// Inspect: restrict to events whose category (cat) contains this substring
    #[arg(long)]
    pub(crate) cat: Option<String>,

    /// Inspect: list distinct event names with counts/total duration
    #[arg(long)]
    pub(crate) names: bool,

    /// Inspect: list distinct threads (tid) with counts/RunTask duration
    #[arg(long)]
    pub(crate) threads: bool,

    /// Inspect: heaviest CPU call stacks (root → leaf), heaviest first
    #[arg(long)]
    pub(crate) stacks: bool,

    /// Inspect: folded stacks (`a;b;c <us>`) for flamegraph.pl / speedscope
    #[arg(long)]
    pub(crate) flame: bool,

    /// Inspect: break down the heaviest RunTasks into their child events
    #[arg(long)]
    pub(crate) task: bool,

    /// Inspect: busy timeline (RunTask per time bucket)
    #[arg(long)]
    pub(crate) timeline: bool,

    /// Inspect: timeline bucket size in ms (default auto ~40 buckets, 10-500ms)
    #[arg(long)]
    pub(crate) bucket: Option<f64>,

    /// Inspect: duration distribution table instead of event list (use with --events)
    #[arg(long)]
    pub(crate) stats: bool,

    /// Inspect: auto-anchor --around on the worst (longest) RunTask
    #[arg(long)]
    pub(crate) worst: bool,

    /// Inspect: sort order for --events/--names (ts, dur, name, count)
    #[arg(long)]
    pub(crate) sort: Option<String>,

    /// Inspect: print full event args (no truncation) for --events/--find
    #[arg(long)]
    pub(crate) full_args: bool,

    /// Inspect: interpret --function/--find/--events as regex instead of substring/exact
    #[arg(long)]
    pub(crate) regex: bool,

    /// Inspect: emit JSON (for jq/pipelines) instead of Markdown
    #[arg(long)]
    json: bool,

    /// Inspect: emit CSV instead of Markdown (--json for JSON)
    #[arg(long)]
    pub(crate) csv: bool,

    /// Inspect: jank clusters (dropped frames / spikes below Long Task threshold)
    #[arg(long)]
    jank: bool,

    /// Inspect: anchor windows on the first FunctionCall functionName /
    /// CPU profile function or URL / event-args match of this substring
    #[arg(long)]
    pub(crate) anchor: Option<String>,

    /// Inspect: PRE window length in ms before the SHOOT window (default 500)
    #[arg(long, default_value_t = 500.0)]
    pub(crate) pre: f64,

    /// Inspect: POST window length in ms after the SHOOT window (default 500)
    #[arg(long, default_value_t = 500.0)]
    pub(crate) post: f64,

    /// Inspect: compare PRE / SHOOT / POST windows (frames, dropped frames,
    /// GC, long tasks, CPU samples, busy time) with deltas
    #[arg(long)]
    pub(crate) delta: bool,

    /// Inspect: inclusive CPU call tree (self + subtree time); prune with
    /// --function / --url
    #[arg(long)]
    pub(crate) calltree: bool,

    /// Inspect: restrict CPU functions/stacks/calltree to source URLs
    /// containing this substring
    #[arg(long)]
    pub(crate) url: Option<String>,

    /// Inspect: GC + long-task report for the window
    #[arg(long)]
    pub(crate) gc: bool,

    /// Inspect: long-task threshold in ms (default 50, use with --gc/--delta)
    #[arg(long, default_value_t = 50.0)]
    pub(crate) lt: f64,

    /// Inspect: per-frame duration stats (b/e-paired events) for the window
    #[arg(long)]
    pub(crate) frames: bool,

    /// Inspect: frame event name for --frames/--delta
    #[arg(long, default_value = "SubmitCompositorFrameToPresentationCompositorFrame")]
    pub(crate) frame_event: String,
}

impl Cli {
    /// Any granular-inspect flag present?
    pub(crate) fn is_inspect(&self) -> bool {
        self.events.is_some()
            || self.function.is_some()
            || self.find.is_some()
            || self.names
            || self.threads
            || self.stacks
            || self.flame
            || self.task
            || self.timeline
            || self.worst
            || self.jank
            || self.anchor.is_some()
            || self.delta
            || self.calltree
            || self.gc
            || self.frames
    }
}

// ── Analyzed trace ──

/// All derived analysis for a single parsed trace.
pub(crate) struct Analyzed {
    pub(crate) trace: trace::TraceFile,
    #[allow(dead_code)]
    pub(crate) main_tid: u64,
    pub(crate) summary: analysis::SummaryResult,
    pub(crate) scroll_frames: analysis::ScrollFrameResult,
    pub(crate) cpu_profile: analysis::CpuProfileResult,
    /// Raw full-trace CPU profile (node table + self-times), kept so REPL
    /// queries with an empty scope can skip re-scanning the trace.
    pub(crate) cpu_cache: analysis::CpuProfileCache,
    pub(crate) layout_dirty: analysis::LayoutDirtyResult,
    pub(crate) style_recalc: analysis::StyleRecalcResult,
    pub(crate) forced_reflows: analysis::ForcedReflowResult,
    pub(crate) jank: analysis::JankResult,
}

pub(crate) fn load_and_analyze(path: &Path) -> Result<Analyzed, Box<dyn std::error::Error>> {
    eprintln!("Loading {}...", path.display());
    let trace = trace::parse_trace(path)?;
    let main_tid = trace::detect_main_thread(&trace.trace_events);
    let events = &trace.trace_events;
    eprintln!(
        "  {} events, main thread tid={}",
        events.len(),
        main_tid
    );
    // The six analysis passes are independent and read-only, so on large
    // traces they run concurrently (each pass itself parallelizes hot work).
    const PARALLEL_THRESHOLD: usize = 200_000;
    let (summary, scroll_frames, layout_dirty, style_recalc, forced_reflows, jank, (cpu_profile, cpu_cache)) =
        if events.len() >= PARALLEL_THRESHOLD {
            std::thread::scope(|s| {
                let a = s.spawn(|| analysis::analyze_summary(events, main_tid));
                let b = s.spawn(|| analysis::analyze_scroll_frames(events, main_tid));
                let c = s.spawn(|| analysis::analyze_cpu_profile_full(events));
                let d = s.spawn(|| analysis::analyze_layout_dirty(events, main_tid));
                let e = s.spawn(|| analysis::analyze_style_recalc(events, main_tid));
                let f = s.spawn(|| analysis::analyze_forced_reflows(events, main_tid));
                let g = s.spawn(|| analysis::analyze_jank(events, main_tid, None));
                (
                    a.join().unwrap(),
                    b.join().unwrap(),
                    d.join().unwrap(),
                    e.join().unwrap(),
                    f.join().unwrap(),
                    g.join().unwrap(),
                    c.join().unwrap(),
                )
            })
        } else {
            (
                analysis::analyze_summary(events, main_tid),
                analysis::analyze_scroll_frames(events, main_tid),
                analysis::analyze_layout_dirty(events, main_tid),
                analysis::analyze_style_recalc(events, main_tid),
                analysis::analyze_forced_reflows(events, main_tid),
                analysis::analyze_jank(events, main_tid, None),
                analysis::analyze_cpu_profile_full(events),
            )
        };
    Ok(Analyzed {
        summary,
        scroll_frames,
        cpu_profile,
        cpu_cache,
        layout_dirty,
        style_recalc,
        forced_reflows,
        jank,
        trace,
        main_tid,
    })
}

/// Build the App (with optional compare) from analyzed traces. Borrows the
/// analyses so the REPL can rebuild the app (e.g. after `compare <file>`).
fn build_app(
    a: &Analyzed,
    b: Option<(&Analyzed, String)>,
    name_a: String,
) -> app::App {
    let (compare_result, trace_name_b) = if let Some((bb, name_b)) = b {
        let cmp = analysis::analyze_compare(
            &a.summary,
            &bb.summary,
            &a.scroll_frames,
            &bb.scroll_frames,
            &a.cpu_profile,
            &bb.cpu_profile,
            &a.layout_dirty,
            &bb.layout_dirty,
            &a.style_recalc,
            &bb.style_recalc,
        );
        (Some(cmp), Some(name_b))
    } else {
        (None, None)
    };

    let metadata = a.trace.metadata.clone();
    app::App::new(
        a.summary.clone(),
        a.scroll_frames.clone(),
        a.cpu_profile.clone(),
        a.layout_dirty.clone(),
        a.style_recalc.clone(),
        a.forced_reflows.clone(),
        a.jank.clone(),
        compare_result,
        name_a,
        trace_name_b,
        metadata,
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let trace_path = match cli.trace.as_deref().map(Path::new) {
        Some(p) => p,
        None => {
            eprintln!("error: a trace file path is required (or use `--repl <trace>`)");
            std::process::exit(2);
        }
    };

    // Interactive REPL: load + analyze once, then run live queries.
    if cli.repl {
        return repl::run_repl(trace_path, &cli);
    }

    // Directory mode: scan for traces, then batch-export or list them.
    if trace_path.is_dir() {
        let traces = trace::list_traces(trace_path)?;
        if traces.is_empty() {
            eprintln!(
                "No trace files (*.json / *.json.gz) found in {}",
                cli.trace.as_deref().unwrap_or("")
            );
            return Ok(());
        }
        if cli.export.is_some() {
            return batch_export(&cli, &traces);
        }
        // No TUI picker: list traces and guide the user to pick one explicitly.
        eprintln!("Found {} trace(s) in {}:", traces.len(), cli.trace.as_deref().unwrap_or(""));
        for (i, p) in traces.iter().enumerate() {
            eprintln!("  {:>3}. {}", i + 1, p.display());
        }
        eprintln!(
            "\nAnalyze one: chperf <file> [--export] [--compare <file2>] | inspect: --events/--function/--find"
        );
        eprintln!("Export all:  chperf {} --export", cli.trace.as_deref().unwrap_or(""));
        return Ok(());
    }

    // Granular inspect mode: print targeted tables to stdout, skip analysis/TUI.
    if cli.is_inspect() {
        return run_inspect(trace_path, &cli);
    }

    run_single(trace_path, cli.compare.as_deref().map(Path::new), &cli)
}

/// Granular inspection: events/functions/stacks/timeline/args, each scoped to a
/// window/thread/process/category. Prints Markdown to stdout.
fn run_inspect(path: &Path, cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Loading {}...", path.display());
    let trace = trace::parse_trace(path)?;
    eprintln!("  {} events", trace.trace_events.len());
    let name_a = trace::trace_stem(path);
    let Some(path_b) = cli.compare.as_deref().map(Path::new) else {
        return inspect_output(&trace.trace_events, &name_a, cli, None);
    };
    eprintln!("Loading {}...", path_b.display());
    let trace_b = trace::parse_trace(path_b)?;
    eprintln!("  {} events", trace_b.trace_events.len());
    inspect_compare_output(
        &trace.trace_events,
        &name_a,
        &trace_b.trace_events,
        &trace::trace_stem(path_b),
        cli,
    )
}

/// Windowed comparison of two traces: run every requested section on both,
/// and when `--delta` is set, merge the PRE/SHOOT/POST metric rows into a
/// single A-vs-B table (SHOOT, SHOOT−PRE, and the inter-trace deltas).
fn inspect_compare_output(
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
fn compare_csv_rows(
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

fn compare_json(compare: &Option<(windowed::DeltaData, windowed::DeltaData)>) -> Value {
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
    trace_name: &str,
    cli: &Cli,
    cpu_cache: Option<&analysis::CpuProfileCache>,
) -> Result<(), Box<dyn std::error::Error>> {
    let min_ts = inspect::trace_start_us(events);
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

/// Load, analyze, then export or launch TUI for a single (optionally compared) trace.
fn run_single(
    path_a: &Path,
    path_b: Option<&Path>,
    cli: &Cli,
) -> Result<(), Box<dyn std::error::Error>> {
    let analyzed_a = load_and_analyze(path_a)?;

    let b_opt = if let Some(compare_path) = path_b {
        let analyzed_b = load_and_analyze(compare_path)?;
        Some((analyzed_b, trace::trace_stem(compare_path)))
    } else {
        None
    };

    let mut app = build_app(&analyzed_a, b_opt.as_ref().map(|(b, n)| (b, n.clone())), trace::trace_stem(path_a));

    // Apply throttle: CLI flag takes priority, otherwise auto-detect from trace metadata
    let throttle = cli.throttle.unwrap_or_else(|| {
        analyzed_a
            .trace
            .metadata
            .as_ref()
            .and_then(|m| m.cpu_throttling)
            .unwrap_or(1.0)
    });
    if throttle > 1.0 {
        app.throttle_factor = throttle;
        app.throttle_factor_saved = throttle;
        eprintln!(
            "  CPU throttle: {:.0}x ({})",
            throttle,
            if cli.throttle.is_some() {
                "from --throttle"
            } else {
                "auto-detected from trace"
            }
        );
    }

    // Export mode: skip TUI, output Markdown
    if let Some(ref export_target) = cli.export {
        let md = if cli.summary {
            export::export_summary_only(&app)
        } else {
            export::export_markdown(&app)
        };
        if export_target == "-" {
            print!("{}", md);
        } else {
            std::fs::write(export_target, &md)?;
            eprintln!("Exported to {}", export_target);
        }
        return Ok(());
    }

    run_tui(app)
}

/// Batch-export every trace in a directory as Markdown files.
/// Traces are analyzed in parallel in small groups: trace parsing is
/// memory-heavy, so we cap concurrency to keep peak RAM bounded.
fn batch_export(cli: &Cli, traces: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    const GROUP: usize = 3;
    let summary_only = cli.summary;
    let throttle = cli.throttle;
    for group in traces.chunks(GROUP) {
        std::thread::scope(|s| {
            for path in group {
                s.spawn(|| {
                    if let Err(e) = export_one(path, throttle, summary_only) {
                        eprintln!("  ERROR {}: {}", path.display(), e);
                    }
                });
            }
        });
    }
    Ok(())
}

/// Load, analyze, apply throttle, export a single trace to `chperf-export-<stem>.md`.
fn export_one(
    path: &Path,
    throttle: Option<f64>,
    summary_only: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let stem = trace::trace_stem(path);
    let analyzed = load_and_analyze(path)?;
    let mut app = build_app(&analyzed, None, stem.clone());

    // Throttle: CLI flag priority, else auto-detect from trace metadata
    let throttle = throttle.unwrap_or_else(|| {
        analyzed
            .trace
            .metadata
            .as_ref()
            .and_then(|m| m.cpu_throttling)
            .unwrap_or(1.0)
    });
    if throttle > 1.0 {
        app.throttle_factor = throttle;
        app.throttle_factor_saved = throttle;
    }

    let md = if summary_only {
        export::export_summary_only(&app)
    } else {
        export::export_markdown(&app)
    };

    let out_path = path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("chperf-export-{}.md", stem));
    std::fs::write(&out_path, &md)?;
    eprintln!("  -> {}", out_path.display());
    Ok(())
}

// ── TUI ──

fn run_tui(mut app: app::App) -> Result<(), Box<dyn std::error::Error>> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Event loop
    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        if let Event::Key(key) = event::read()? {
            // Dismiss status message on any keypress
            if app.status_message.is_some() {
                app.status_message = None;
                continue;
            }

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    app.should_quit = true;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.should_quit = true;
                }
                KeyCode::Char('e') => {
                    // Export to file from TUI
                    let filename = format!("chperf-export-{}.md", app.trace_name_a);
                    let md = export::export_markdown(&app);
                    if let Err(e) = std::fs::write(&filename, &md) {
                        app.set_message(format!("Export failed: {}", e));
                    } else {
                        app.set_message(format!("Exported to {}", filename));
                    }
                }
                KeyCode::Tab => app.next_tab(),
                KeyCode::BackTab => app.prev_tab(),
                KeyCode::Char('t') => app.toggle_throttle(),
                KeyCode::Char('j') | KeyCode::Down => app.scroll_down(1),
                KeyCode::Char('k') | KeyCode::Up => app.scroll_up(1),
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.scroll_down(20)
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.scroll_up(20)
                }
                KeyCode::Char('g') => app.scroll_offset = 0,
                KeyCode::Char('G') => {
                    let max = app.row_count().saturating_sub(1);
                    app.scroll_offset = max;
                }
                KeyCode::Char('1')
                    if !app.tabs.is_empty() => {
                        app.tab = app.tabs[0];
                        app.scroll_offset = 0;
                    }
                KeyCode::Char('2')
                    if app.tabs.len() > 1 => {
                        app.tab = app.tabs[1];
                        app.scroll_offset = 0;
                    }
                KeyCode::Char('3')
                    if app.tabs.len() > 2 => {
                        app.tab = app.tabs[2];
                        app.scroll_offset = 0;
                    }
                KeyCode::Char('4')
                    if app.tabs.len() > 3 => {
                        app.tab = app.tabs[3];
                        app.scroll_offset = 0;
                    }
                KeyCode::Char('5')
                    if app.tabs.len() > 4 => {
                        app.tab = app.tabs[4];
                        app.scroll_offset = 0;
                    }
                _ => {}
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{compare_csv_rows, compare_json};
    use crate::analysis;
    use crate::inspect::{self, Scope};
    use crate::trace::TraceEvent;
    use crate::windowed;
    use std::path::PathBuf;

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny.json")
    }

    fn fixture_events() -> Vec<TraceEvent> {
        crate::trace::parse_trace(&fixture_path()).unwrap().trace_events
    }

    /// End-to-end golden through the real JSON parser: CPU attribution,
    /// time base, frames, GC, anchor (mirrors the manual Python cross-check
    /// we ran on the real trace, on a deterministic fixture).
    #[test]
    fn fixture_golden_full_pipeline() {
        let events = fixture_events();

        // Time base: metadata at ts=0 must be ignored.
        assert_eq!(inspect::trace_start_us(&events), 1_000_000.0);

        // CPU: per-sample delta weights (5ms × 7 + 4+3+0+4 ms), nodes first-wins.
        let cpu = analysis::analyze_cpu_profile(&events);
        assert_eq!(cpu.total_sample_time_us, 46_000.0);
        // Two distinct node ids share the name "shoot" (chunk1 node 2 and
        // chunk2 node 4): 4×5ms + 11ms.
        let shoot_total: f64 = cpu
            .functions
            .iter()
            .filter(|f| f.function_name == "shoot")
            .map(|f| f.self_time_us)
            .sum();
        assert_eq!(shoot_total, 31_000.0);

        // Windowed attribution by sample time.
        let scope = Scope {
            window: Some((1_000_000.0, 1_030_000.0)),
            tid: None,
            pid: None,
            cat: None,
        };
        let (_, times) = analysis::scan_profile_chunks(&events, Some(&scope), 0);
        let total: f64 = times.values().sum();
        assert_eq!(total, 30_000.0);

        // Frames: 2 b/e pairs (16ms, 20ms), 2 dropped.
        let (md, _) = windowed::frames_section(
            &events,
            &Scope { window: None, tid: None, pid: None, cat: None },
            "SubmitCompositorFrameToPresentationCompositorFrame",
            1_000_000.0,
        );
        assert!(md.contains("2 paired"));
        assert!(md.contains("Dropped frames**: 2"));

        // GC: 1 major 5ms, 1 minor 1ms, 1 long task ≥50ms.
        let (md, _) = windowed::gc_section(
            &events,
            &Scope { window: None, tid: None, pid: None, cat: None },
            50.0,
            1_000_000.0,
        );
        assert!(md.contains("Long tasks ≥50ms**: 1 total, 600.0ms combined"));

        // Anchor: FunctionCall functionName wins.
        let m = inspect::Matcher::new("shoot", false).unwrap();
        let a = windowed::find_anchor(&events, &m).unwrap();
        assert_eq!(a.kind, "FunctionCall");
        assert_eq!(a.ts, 1_005_000.0);

        // Combined find: event args + cpu-profile names/urls.
        let (md, _) = inspect::find_section(
            &events,
            &inspect::Matcher::new("player_shoot", false).unwrap(),
            &Scope { window: None, tid: None, pid: None, cat: None },
            false,
            30,
            1_000_000.0,
            None,
        );
        assert!(md.contains("CPU profile matches (0"));
        let (md, _) = inspect::find_section(
            &events,
            &inspect::Matcher::new("weapon.ts", false).unwrap(),
            &Scope { window: None, tid: None, pid: None, cat: None },
            false,
            30,
            1_000_000.0,
            None,
        );
        assert!(md.contains("CPU profile matches (2"));
    }

    /// Windowed compare: two identical analyses yield zero deltas, and the
    /// merged rows carry raw units.
    #[test]
    fn compare_rows_self_diff_is_zero() {
        let events = fixture_events();
        let da = windowed::delta_data(
            &events,
            (300_000.0, 900_000.0),
            (1_000_000.0, 1_030_000.0),
            (2_000_000.0, 2_500_000.0),
            1_010_000.0,
            "SubmitCompositorFrameToPresentationCompositorFrame",
            50.0,
        );
        let rows = compare_csv_rows(&da, &da);
        assert_eq!(rows.len(), 13);
        for r in &rows {
            assert_eq!(r["diff_shoot"], 0.0, "{}", r["metric"]);
            assert_eq!(r["diff_delta"], 0.0, "{}", r["metric"]);
        }
        let j = compare_json(&Some((da.clone(), da)));
        assert_eq!(j["rows"].as_array().unwrap().len(), 13);
        assert!(j["anchor_a_us"].is_number());
        // CPU samples are raw µs: 6 × 5ms samples with ts ≤ 1_030_000.
        let cpu = j["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["metric"] == "CPU samples")
            .unwrap();
        assert_eq!(cpu["a_shoot"], 30_000.0);
    }

    /// Deterministic pseudo-fuzzing: structurally-valid but adversarial
    /// traces (weird timestamps, missing/mismatched arrays, cycles, wrong
    /// types) must never panic and must keep the sample-time invariants.
    #[test]
    fn adversarial_traces_never_panic_and_keep_invariants() {
        let mut state = 0xdead_beef_cafe_f00du64;
        let mut rng = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u64
        };
        let names = ["RunTask", "FunctionCall", "ProfileChunk", "thread_name", "MajorGC", "MinorGC", "DroppedFrame", "SubmitCompositorFrameToPresentationCompositorFrame", "Paint", "Profile", "Layout", "UpdateLayoutTree"];
        let phases = ["X", "b", "e", "P", "M", "I", "B", "E", "N", ""];

        for iter in 0..60 {
            let n_ev = (rng() % 200) as usize;
            let mut arr: Vec<serde_json::Value> = Vec::with_capacity(n_ev + 2);
            arr.push(serde_json::json!({"name": "thread_name", "cat": "__metadata", "ph": "M", "ts": 0}));
            arr.push(serde_json::json!({"name": "Profile", "ph": "P", "ts": 1_000_000.0, "pid": 2}));
            for _ in 0..n_ev {
                let name = names[(rng() % names.len() as u64) as usize];
                let ph = phases[(rng() % phases.len() as u64) as usize];
                let ts = match rng() % 5 {
                    0 => 0.0,
                    1 => -1000.0,
                    2 => 3_100_000_000.0,
                    _ => (rng() % 2_000_000) as f64,
                };
                let dur = match rng() % 4 {
                    0 => None,
                    1 => Some(-5.0),
                    _ => Some((rng() % 200_000) as f64),
                };
                let mut ev = serde_json::json!({"name": name, "ph": ph, "ts": ts});
                if let Some(d) = dur {
                    ev["dur"] = serde_json::json!(d);
                }
                ev["tid"] = serde_json::json!(rng() % 8);
                ev["pid"] = serde_json::json!(rng() % 3);
                if name == "ProfileChunk" {
                    match rng() % 4 {
                        0 => {} // no cpuProfile at all
                        1 => {
                            ev["args"] = serde_json::json!({"data": {"cpuProfile": {
                                "nodes": [{"id": 0, "callFrame": {"functionName": "", "url": ""}, "parent": 7}],
                                "samples": [0, 1, 2]
                            }}}); // missing timeDeltas
                        }
                        2 => {
                            // parent cycle + duplicate ids + samples longer than deltas
                            ev["args"] = serde_json::json!({"data": {"cpuProfile": {
                                "nodes": [
                                    {"id": 1, "callFrame": {"functionName": "a", "url": ""}, "parent": 2},
                                    {"id": 2, "callFrame": {"functionName": "b", "url": ""}, "parent": 1},
                                    {"id": 2, "callFrame": {"functionName": "dup", "url": ""}, "parent": 1}
                                ],
                                "samples": [1, 2, 3, 9, 1],
                                "timeDeltas": [100.0, "oops", -50.0]
                            }}});
                        }
                        _ => {
                            let n_s = (rng() % 6) as usize;
                            let samples: Vec<u64> = (0..n_s).map(|_| rng() % 5).collect();
                            let deltas: Vec<f64> = (0..n_s).map(|_| (rng() % 50) as f64 - 20.0).collect();
                            ev["args"] = serde_json::json!({"data": {"cpuProfile": {
                                "nodes": [{"id": 1, "callFrame": {"functionName": "x", "url": "http://x.ts"}, "parent": null}],
                                "samples": samples,
                                "timeDeltas": deltas
                            }}});
                        }
                    }
                } else if name == "FunctionCall" && rng() % 3 == 0 {
                    ev["args"] = serde_json::json!({"data": {"functionName": "shoot"}});
                }
                arr.push(ev);
            }
            let trace = serde_json::json!({"traceEvents": arr});

            let path = std::env::temp_dir().join(format!("chperf-fuzz-{}-{}.json", std::process::id(), iter));
            std::fs::write(&path, serde_json::to_string(&trace).unwrap()).unwrap();

            let events = crate::trace::parse_trace(&path).unwrap().trace_events;
            let _ = std::fs::remove_file(&path);

            // Full scan: totals finite and non-negative.
            let (_nodes, full) = analysis::scan_profile_chunks(&events, None, 0);
            let full_total: f64 = full.values().sum();
            assert!(full_total.is_finite() && full_total >= 0.0, "iter {}: bad full total {}", iter, full_total);

            // Whole-trace window == full scan; partial window ⊆ full.
            let (min_ts, max_ts) = {
                let mut mn = f64::INFINITY;
                let mut mx = 0.0f64;
                for e in &events {
                    if crate::trace::is_metadata_event(e) {
                        continue;
                    }
                    mn = mn.min(e.ts);
                    mx = mx.max(e.ts + e.dur.unwrap_or(0.0));
                }
                if !mn.is_finite() {
                    (0.0, 1.0)
                } else {
                    (mn, mx.max(mn + 1.0))
                }
            };
            let scope_all = Scope { window: Some((min_ts, max_ts)), tid: None, pid: None, cat: None };
            let (_, win_all) = analysis::scan_profile_chunks(&events, Some(&scope_all), 0);
            let win_total: f64 = win_all.values().sum();
            assert!(
                (win_total - full_total).abs() <= 1e-6 * full_total.max(1.0),
                "iter {}: windowed {} != full {}",
                iter,
                win_total,
                full_total
            );
            let mid = (min_ts + max_ts) / 2.0;
            let scope_half = Scope { window: Some((min_ts, mid)), tid: None, pid: None, cat: None };
            let (_, win_half) = analysis::scan_profile_chunks(&events, Some(&scope_half), 0);
            for (id, t) in &win_half {
                assert!(*t <= full.get(id).copied().unwrap_or(0.0) + 1e-9, "iter {}: windowed > full for node {}", iter, id);
            }

            // Analysis passes must not panic on garbage.
            let _ = analysis::analyze_cpu_profile(&events);
            let _ = analysis::analyze_summary(&events, 1);
            let _ = analysis::analyze_jank(&events, 1, None);
            let _ = analysis::analyze_jank(&events, 1, Some(&scope_half));
            let _ = crate::trace::detect_main_thread(&events);
            let main_tid = crate::trace::detect_main_thread(&events);
            let _ = windowed::gc_section(
                &events,
                &Scope { window: Some((min_ts, max_ts)), tid: Some(main_tid), pid: None, cat: None },
                50.0,
                min_ts,
            );
            let _ = windowed::frames_section(
                &events,
                &Scope { window: Some((min_ts, max_ts)), tid: None, pid: None, cat: None },
                "SubmitCompositorFrameToPresentationCompositorFrame",
                min_ts,
            );
            let _ = windowed::find_anchor(&events, &inspect::Matcher::new("shoot", false).unwrap());
            let _ = windowed::calltree_section(
                &events,
                &Scope { window: None, tid: None, pid: None, cat: None },
                None,
                None,
                30,
                min_ts,
                None,
            );
        }
    }
}
