mod app;
mod cli;
mod export;
mod html;
mod inspect;
mod junit;
mod report_json;

use crate::cli::Cli;
use chperf_core::{analysis, trace};

use clap::Parser;
use std::path::{Path, PathBuf};

// ── Analyzed trace ──

/// A parsed trace plus its full single-trace report: `load_and_analyze`
/// builds the `App` directly (no parallel mirror struct) and keeps the raw
/// events for HTML/JSON export plus the cached time base / main thread.
pub(crate) struct Loaded {
    pub(crate) app: app::App,
    pub(crate) trace: trace::TraceFile,
    /// Metadata-free trace start (µs), cached so inspect queries don't re-scan
    /// every event to find the time base. (The main thread lives on
    /// `app.main_tid`.)
    pub(crate) min_ts: f64,
}

pub(crate) fn load_and_analyze(
    path: &Path,
    name: String,
) -> Result<Loaded, Box<dyn std::error::Error>> {
    eprintln!("Loading {}...", path.display());
    let trace = trace::parse_trace(path).map_err(|e| {
        format!(
            "{}: {}",
            path.display(),
            e.to_string().split('\n').next().unwrap_or("")
        )
    })?;
    let main_tid = trace::detect_main_thread(&trace.trace_events);
    let min_ts = chperf_core::inspect::trace_start_us(&trace.trace_events);
    let events = &trace.trace_events;
    eprintln!("  {} events, main thread tid={}", events.len(), main_tid);
    // Renderer setup first and serially: the per-renderer passes join the
    // parallel section below, so the budget/model must be ready before it.
    // (Both are full-event scans; ~25ms on big traces, sub-ms on small.)
    let frame_budget_us = chperf_core::windowed::detect_frame_budget_us(events);
    let thread_model = trace::detect_thread_model(events);
    // The analysis passes are independent and read-only, so on large
    // traces they run concurrently (each pass itself parallelizes hot work).
    // Per-renderer summary/jank ride the same scope: they hide under the
    // slowest of the seven instead of serializing after them.
    const PARALLEL_THRESHOLD: usize = 20_000;
    let (
        summary,
        scroll_frames,
        layout_dirty,
        style_recalc,
        forced_reflows,
        jank,
        cpu_profile,
        renderer_summaries,
        renderer_janks,
        thread_count,
    ) = if events.len() >= PARALLEL_THRESHOLD {
        std::thread::scope(|s| {
            let a = s.spawn(|| analysis::analyze_summary(events, main_tid));
            let b = s.spawn(|| analysis::analyze_scroll_frames(events, main_tid));
            let c = s.spawn(|| analysis::analyze_cpu_profile_full(events).0);
            let d = s.spawn(|| analysis::analyze_layout_dirty(events, main_tid));
            let e = s.spawn(|| analysis::analyze_style_recalc(events, main_tid));
            let f = s.spawn(|| analysis::analyze_forced_reflows(events, main_tid));
            let g = s.spawn(|| analysis::analyze_jank(events, main_tid, None));
            let h = s.spawn(|| analysis::analyze_summary_per_renderer(events, &thread_model));
            let i = s.spawn(|| {
                analysis::analyze_jank_per_renderer(events, &thread_model, Some(frame_budget_us))
            });
            // FxHash: tids are u64s counted once per run; SipHash would dominate.
            let j = s.spawn(|| {
                let mut tids = rustc_hash::FxHashSet::default();
                tids.reserve(256);
                for e in events {
                    tids.insert(e.tid);
                }
                tids.len()
            });
            (
                a.join().unwrap(),
                b.join().unwrap(),
                d.join().unwrap(),
                e.join().unwrap(),
                f.join().unwrap(),
                g.join().unwrap(),
                c.join().unwrap(),
                h.join().unwrap(),
                i.join().unwrap(),
                j.join().unwrap(),
            )
        })
    } else {
        let mut tids = rustc_hash::FxHashSet::default();
        for e in events {
            tids.insert(e.tid);
        }
        (
            analysis::analyze_summary(events, main_tid),
            analysis::analyze_scroll_frames(events, main_tid),
            analysis::analyze_layout_dirty(events, main_tid),
            analysis::analyze_style_recalc(events, main_tid),
            analysis::analyze_forced_reflows(events, main_tid),
            analysis::analyze_jank(events, main_tid, None),
            analysis::analyze_cpu_profile_full(events).0,
            analysis::analyze_summary_per_renderer(events, &thread_model),
            analysis::analyze_jank_per_renderer(events, &thread_model, Some(frame_budget_us)),
            tids.len(),
        )
    };
    // Single-trace report sections (also used uncompared in compare mode).
    // The analysis passes take lazy event iterators: no `Vec<&Event>`
    // materialization (8B x N events transient) on this path.
    // Independent and read-only: run concurrently (each is 10-25ms on big
    // traces; the scope join costs microseconds).
    let (loading, network, raster, official_long_tasks) = std::thread::scope(|s| {
        let a = s.spawn(|| analysis::analyze_loading(events.iter()));
        let b = s.spawn(|| analysis::analyze_network(events.iter(), main_tid));
        let c = s.spawn(|| analysis::analyze_raster(events.iter(), main_tid));
        let d = s.spawn(|| analysis::analyze_long_tasks_official(events.iter(), main_tid));
        (
            a.join().unwrap(),
            b.join().unwrap(),
            c.join().unwrap(),
            d.join().unwrap(),
        )
    });

    let metadata = trace.metadata.clone();
    let trace_span_us = summary.total_trace_duration_us;
    let app = app::App::new(
        summary,
        scroll_frames,
        cpu_profile,
        layout_dirty,
        style_recalc,
        forced_reflows,
        jank,
        loading,
        network,
        raster,
        official_long_tasks,
        thread_model,
        renderer_summaries,
        renderer_janks,
        trace.stats.clone(),
        thread_count,
        trace_span_us,
        frame_budget_us,
        main_tid,
        None,
        name,
        None,
        metadata,
    );
    Ok(Loaded { app, trace, min_ts })
}

/// Fill in the compare side of an already-built `App` from a second loaded
/// trace (plus its display name).
fn apply_compare(app: &mut app::App, other: &Loaded, name_b: String) {
    app.compare = Some(analysis::analyze_compare(
        &app.summary,
        &other.app.summary,
        &app.scroll_frames,
        &other.app.scroll_frames,
        &app.cpu_profile,
        &other.app.cpu_profile,
        &app.layout_dirty,
        &other.app.layout_dirty,
        &app.style_recalc,
        &other.app.style_recalc,
        &app.forced_reflows,
        &other.app.forced_reflows,
        &app.jank,
        &other.app.jank,
    ));
    app.trace_name_b = Some(name_b);
}

/// True when any inspect modifier is present without an inspect query.
/// (--throttle/--export/--html/--compare/--summary/--fail-on-regressed are
/// full-report flags, not modifiers; --json/--csv handled separately.)
fn has_inspect_modifier(cli: &Cli) -> bool {
    cli.around.is_some()
        || cli.window.is_some()
        || cli.tid.is_some()
        || cli.pid.is_some()
        || cli.cat.is_some()
        || cli.sort.is_some()
        || cli.min_dur.is_some()
        || cli.regex
        || cli.full_args
        || cli.bucket.is_some()
        || cli.frame_budget.is_some()
        || (cli.lt - 50.0).abs() > f64::EPSILON
        || (cli.pre - 500.0).abs() > f64::EPSILON
        || (cli.post - 500.0).abs() > f64::EPSILON
        || cli.url.is_some()
        || cli.stats
        || cli.gaps
        || cli.top != 30
}

fn main() {
    if let Err(e) = real_main() {
        let msg = e.to_string();
        if msg.starts_with("usage:") {
            eprintln!("chperf: {}", msg);
            std::process::exit(2);
        }
        // Parse/IO errors already carry "path: message" context.
        eprintln!("chperf: {}", msg);
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let raw = match cli.trace.as_deref() {
        Some(t) => t,
        None => {
            eprintln!("error: a trace file path is required");
            std::process::exit(2);
        }
    };

    match resolve_inputs(raw)? {
        Input::Single(path) => {
            if cli.out_dir.is_some() {
                return Err(
                    "usage: --out-dir needs batch input (a directory or glob pattern)".into(),
                );
            }
            run_single_input(&path, &cli)
        }
        Input::Batch(traces) => {
            if traces.is_empty() {
                eprintln!(
                    "No trace files (*.json / *.json.gz) found in {}",
                    cli.trace.as_deref().unwrap_or("")
                );
                return Ok(());
            }
            if cli.export.is_none() && cli.html.is_none() && !cli.json {
                // No TUI picker: list traces and guide the user to pick one explicitly.
                eprintln!(
                    "Found {} trace(s) in {}:",
                    traces.len(),
                    cli.trace.as_deref().unwrap_or("")
                );
                for (i, p) in traces.iter().enumerate() {
                    eprintln!("  {:>3}. {}", i + 1, p.display());
                }
                eprintln!(
                    "\nAnalyze one: chperf <file> [--export] [--compare <file2>] | inspect: --events/--function/--find"
                );
                eprintln!(
                    "Export all:  chperf {} --export [--html] [--json] [--out-dir DIR]",
                    cli.trace.as_deref().unwrap_or("")
                );
                return Ok(());
            }
            batch_export(&cli, &traces)
        }
    }
}

/// Positional trace input after expansion: one file, or many (directory /
/// glob) for batch export.
enum Input {
    Single(PathBuf),
    Batch(Vec<PathBuf>),
}

/// File → itself; directory → its `.json`/`.json.gz` listing (empty
/// directories list nothing and report, they are not an error); anything
/// else (glob pattern, missing path) → `chperf_core::trace::expand_trace_input`
/// (file → itself, glob → matches, Err when nothing found, exit 1).
fn resolve_inputs(raw: &str) -> Result<Input, Box<dyn std::error::Error>> {
    let p = Path::new(raw);
    if p.is_file() {
        return Ok(Input::Single(p.to_path_buf()));
    }
    if p.is_dir() {
        let traces = trace::list_traces(p).map_err(|e| {
            format!(
                "{}: {}",
                p.display(),
                e.to_string().split('\n').next().unwrap_or("")
            )
        })?;
        return Ok(Input::Batch(traces));
    }
    let mut traces = trace::expand_trace_input(raw).map_err(|e| format!("{}: {}", raw, e))?;
    traces.sort();
    Ok(Input::Batch(traces))
}

/// File → itself; directory → its `.json`/`.json.gz` listing (empty
fn run_single_input(trace_path: &Path, cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Modifier gating + full-report JSON/CSV handling (no inspect query).
    if !cli.is_inspect() {
        if cli.stats {
            return Err(
                "usage: --stats requires --events <names> (duration table of an event list)".into(),
            );
        }
        if cli.gaps {
            return Err(
                "usage: --gaps requires --events <names> (no silent EventDispatch default)".into(),
            );
        }
        if cli.summary && cli.compare.is_none() {
            return Err(
                "usage: --summary requires --compare <file> (summary-only is a compare view)"
                    .into(),
            );
        }
        if cli.csv {
            return Err(
                "usage: CSV only for inspect queries (add --events/--names/--frames/...)".into(),
            );
        }
        if cli.fail_on_regressed && cli.compare.is_none() {
            return Err("usage: --fail-on-regressed requires --compare <file>".into());
        }
        if cli.json {
            if has_inspect_modifier(cli) {
                // --json with other modifiers but no query: modifiers are meaningless.
                return Err("usage: no inspect query given — add --events/--timeline/--frames/--loading/--network/--raster/--coverage/... or drop these flags for the full report".into());
            }
            return run_single(trace_path, cli.compare.as_deref().map(Path::new), cli);
        }
        if has_inspect_modifier(cli) {
            return Err("usage: no inspect query given — add --events/--timeline/--frames/--loading/--network/--raster/--coverage/... or drop these flags for the full report".into());
        }
    } else {
        // Inspect path validates --stats/--gaps inside inspect_output.
        if cli.summary {
            // --summary with an inspect query: summary flag is for compare-only.
            return Err("usage: --summary requires --compare <file> (not an inspect query)".into());
        }
        if cli.fail_on_regressed && cli.compare.is_none() {
            return Err("usage: --fail-on-regressed requires --compare <file>".into());
        }
    }

    // Granular inspect mode: print targeted tables to stdout, skip analysis.
    if cli.is_inspect() {
        return run_inspect(trace_path, cli);
    }

    run_single(trace_path, cli.compare.as_deref().map(Path::new), cli)
}

/// Granular inspection: events/functions/stacks/timeline/args, each scoped to a
/// window/thread/process/category. Prints Markdown to stdout.
fn run_inspect(path: &Path, cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Loading {}...", path.display());
    let trace = trace::parse_trace(path).map_err(|e| {
        format!(
            "{}: {}",
            path.display(),
            e.to_string().split('\n').next().unwrap_or("")
        )
    })?;
    eprintln!("  {} events", trace.trace_events.len());
    let name_a = trace::trace_stem(path);
    let min_ts = chperf_core::inspect::trace_start_us(&trace.trace_events);
    // --junit with --compare needs the full compare report path (findings
    // only exist there); bare --junit writes XML to stdout, so it cannot
    // share stdout with an inspect query.
    if cli.junit.is_some() && cli.compare.is_some() {
        return Err(
            "usage: --junit with --compare needs the full report path (drop inspect flags)".into(),
        );
    }
    if cli.junit.as_deref() == Some("-") {
        return Err(
            "usage: bare --junit writes XML to stdout (alongside nothing else) — drop inspect flags or use --junit=FILE"
                .into(),
        );
    }
    if let Some(target) = cli.junit.as_deref() {
        let main_tid = trace::detect_main_thread(&trace.trace_events);
        let summary = analysis::analyze_summary(&trace.trace_events, main_tid);
        let jank = analysis::analyze_jank(&trace.trace_events, main_tid, None);
        let scroll = analysis::analyze_scroll_frames(&trace.trace_events, main_tid);
        let gates = junit::single_gates(
            summary.long_task_count,
            jank.total_dropped,
            summary.total_blocking_time_us,
            scroll.tasks.len(),
        );
        junit::write_junit(target, &junit::junit_single_xml(&gates))?;
    }
    let Some(path_b) = cli.compare.as_deref().map(Path::new) else {
        return inspect::inspect_output(&trace.trace_events, min_ts, &name_a, cli, &trace.stats);
    };
    eprintln!("Loading {}...", path_b.display());
    let trace_b = trace::parse_trace(path_b).map_err(|e| {
        format!(
            "{}: {}",
            path_b.display(),
            e.to_string().split('\n').next().unwrap_or("")
        )
    })?;
    eprintln!("  {} events", trace_b.trace_events.len());
    inspect::inspect_compare_output(
        &trace.trace_events,
        &name_a,
        &trace_b.trace_events,
        &trace::trace_stem(path_b),
        cli,
        &trace.stats,
        &trace_b.stats,
    )
}

/// Load, analyze, then export the report for a single (optionally compared) trace.
fn run_single(
    path_a: &Path,
    path_b: Option<&Path>,
    cli: &Cli,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut analyzed_a = load_and_analyze(path_a, trace::trace_stem(path_a))?;

    let b_opt = if let Some(compare_path) = path_b {
        let analyzed_b = load_and_analyze(compare_path, trace::trace_stem(compare_path))?;
        Some(analyzed_b)
    } else {
        None
    };
    if let Some(ref bb) = b_opt {
        let name_b = bb.app.trace_name_a.clone();
        apply_compare(&mut analyzed_a.app, bb, name_b);
    }
    // Split the report from its backing events (both borrowed below).
    let Loaded {
        mut app,
        trace,
        min_ts,
    } = analyzed_a;
    // Apply throttle: CLI flag takes priority, otherwise auto-detect from trace metadata
    let throttle = cli.throttle.unwrap_or_else(|| {
        trace
            .metadata
            .as_ref()
            .and_then(|m| m.cpu_throttling)
            .unwrap_or(1.0)
    });
    if throttle > 1.0 {
        app.throttle_factor = throttle;
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

    // --junit: report-only XML (compare findings or single-trace gates).
    // Bare `--junit` (stdout) suppresses the normal stdout report so stdout
    // carries only XML; file exports still happen. Never changes the exit
    // code (that stays with --fail-on-regressed).
    let junit_stdout = cli.junit.as_deref() == Some("-");
    if let Some(target) = cli.junit.as_deref() {
        let xml = match app.compare {
            Some(ref cmp) => junit::junit_compare_xml(&cmp.findings),
            None => {
                let gates = junit::single_gates(
                    app.summary.long_task_count,
                    app.jank.total_dropped,
                    app.summary.total_blocking_time_us,
                    app.scroll_frames.tasks.len(),
                );
                junit::junit_single_xml(&gates)
            }
        };
        junit::write_junit(target, &xml)?;
    }

    // CLI-only: render the report to stdout (or a file via --export=FILE).
    // --html wins over --export: it needs the raw events for the memory /
    // input / async sections, which only live on the loaded trace here.
    if let Some(target) = cli.html.as_deref() {
        // Paired A-vs-B report: the B app is already built, so the HTML can
        // reuse the per-section renderers for both traces.
        let b_param = b_opt
            .as_ref()
            .map(|bb| (&bb.app, bb.trace.trace_events.as_slice(), bb.min_ts));
        let doc = html::export_html(&app, &trace.trace_events, min_ts, b_param);
        match target {
            "-" => {
                if !junit_stdout {
                    print!("{}", doc)
                }
            }
            t => {
                std::fs::write(t, &doc).map_err(|e| format!("{}: {}", t, e))?;
                eprintln!("Exported HTML to {}", t);
            }
        }
        return check_fail_on_regressed(&app, cli);
    }

    // Machine-readable full report: --json with no inspect query.
    if cli.json {
        let v = report_json::full_report_json(&app, &trace.trace_events, min_ts);
        let s = serde_json::to_string_pretty(&v)?;
        match cli.export.as_deref() {
            Some(target) if target != "-" => {
                std::fs::write(target, &s).map_err(|e| format!("{}: {}", target, e))?;
                eprintln!("Exported JSON to {}", target);
            }
            _ => {
                if !junit_stdout {
                    print!("{}", s)
                }
            }
        }
        return check_fail_on_regressed(&app, cli);
    }

    let md = if cli.summary {
        export::export_summary_only(&app)
    } else {
        export::export_markdown(&app)
    };
    match cli.export.as_deref() {
        Some("-") | None => {
            if !junit_stdout {
                print!("{}", md)
            }
        }
        Some(target) => {
            std::fs::write(target, &md).map_err(|e| format!("{}: {}", target, e))?;
            eprintln!("Exported to {}", target);
        }
    }
    check_fail_on_regressed(&app, cli)
}

/// --fail-on-regressed: exit 3 when any compare finding is Regressed.
fn check_fail_on_regressed(app: &app::App, cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.fail_on_regressed
        && let Some(ref cmp) = app.compare
        && cmp
            .findings
            .iter()
            .any(|f| f.severity == analysis::FindingSeverity::Regressed)
    {
        std::process::exit(3);
    }
    Ok(())
}

/// Batch-export every trace (directory / glob) as Markdown, HTML and/or JSON
/// files. Traces are analyzed in parallel in small groups: trace parsing is
/// memory-heavy, so we cap concurrency to keep peak RAM bounded.
/// Per-file failures print `chperf: {path}: {msg}` + final "N failed" + exit 1.
fn batch_export(cli: &Cli, traces: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    const GROUP: usize = 3;
    if cli.summary {
        return Err("usage: --summary requires --compare <file> (not batch export)".into());
    }
    let formats = BatchFormats {
        md: cli.export.is_some(),
        html: cli.html.is_some(),
        json: cli.json,
    };
    let out_dir: Option<PathBuf> = match cli.out_dir.as_deref() {
        Some(d) => {
            std::fs::create_dir_all(d)
                .map_err(|e| format!("{}: cannot create --out-dir: {}", d, e))?;
            Some(PathBuf::from(d))
        }
        None => None,
    };
    let throttle = cli.throttle;
    let failed = AtomicUsize::new(0);
    for group in traces.chunks(GROUP) {
        std::thread::scope(|s| {
            for path in group {
                s.spawn(|| {
                    if let Err(e) = export_one(path, throttle, out_dir.as_deref(), &formats) {
                        eprintln!("chperf: {}: {}", path.display(), e);
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
    }
    let n = failed.load(Ordering::Relaxed);
    if n > 0 {
        eprintln!("{} failed", n);
        std::process::exit(1);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct BatchFormats {
    md: bool,
    html: bool,
    json: bool,
}

/// Load, analyze, apply throttle, export a single trace to
/// `chperf-export-<stem>.md` / `.html` / `.json` (per requested format) in
/// `out_dir`, or next to the trace when `out_dir` is None.
fn export_one(
    path: &Path,
    throttle: Option<f64>,
    out_dir: Option<&Path>,
    formats: &BatchFormats,
) -> Result<(), Box<dyn std::error::Error>> {
    let stem = trace::trace_stem(path);
    let Loaded {
        mut app,
        trace,
        min_ts,
    } = load_and_analyze(path, stem.clone())?;

    // Throttle: CLI flag priority, else auto-detect from trace metadata
    let throttle = throttle.unwrap_or_else(|| {
        trace
            .metadata
            .as_ref()
            .and_then(|m| m.cpu_throttling)
            .unwrap_or(1.0)
    });
    if throttle > 1.0 {
        app.throttle_factor = throttle;
    }

    let dir: &Path = out_dir.unwrap_or_else(|| path.parent().unwrap_or(Path::new(".")));
    let out_path = |ext: &str| dir.join(format!("chperf-export-{}.{}", stem, ext));
    let events = &trace.trace_events;

    if formats.md {
        let md = export::export_markdown(&app);
        let p = out_path("md");
        std::fs::write(&p, &md)?;
        eprintln!("  -> {}", p.display());
    }
    if formats.html {
        let doc = html::export_html(&app, events, min_ts, None);
        let p = out_path("html");
        std::fs::write(&p, &doc)?;
        eprintln!("  -> {}", p.display());
    }
    if formats.json {
        let v = report_json::full_report_json(&app, events, min_ts);
        let p = out_path("json");
        std::fs::write(&p, serde_json::to_string_pretty(&v)?)?;
        eprintln!("  -> {}", p.display());
    }
    Ok(())
}
