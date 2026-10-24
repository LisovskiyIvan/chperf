mod app;
mod cli;
mod export;
mod html;
mod inspect;
mod junit;
mod repl;
mod report_json;

use crate::cli::Cli;
use chperf_core::{analysis, trace};

use clap::Parser;
use std::path::{Path, PathBuf};

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
    /// Metadata-free trace start (µs), cached so REPL queries don't re-scan
    /// every event to find the time base.
    pub(crate) min_ts: f64,
}

pub(crate) fn load_and_analyze(path: &Path) -> Result<Analyzed, Box<dyn std::error::Error>> {
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
    // The six analysis passes are independent and read-only, so on large
    // traces they run concurrently (each pass itself parallelizes hot work).
    const PARALLEL_THRESHOLD: usize = 20_000;
    let (
        summary,
        scroll_frames,
        layout_dirty,
        style_recalc,
        forced_reflows,
        jank,
        (cpu_profile, cpu_cache),
    ) = if events.len() >= PARALLEL_THRESHOLD {
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
        min_ts,
    })
}

/// Build the App (with optional compare) from analyzed traces. Borrows the
/// analyses so the REPL can rebuild the app (e.g. after `compare <file>`).
fn build_app(a: &Analyzed, b: Option<(&Analyzed, String)>, name_a: String) -> app::App {
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
            &a.forced_reflows,
            &bb.forced_reflows,
            &a.jank,
            &bb.jank,
        );
        (Some(cmp), Some(name_b))
    } else {
        (None, None)
    };

    let events = &a.trace.trace_events;
    let loading = analysis::analyze_loading(events);
    let network = analysis::analyze_network(events, a.main_tid);
    let raster = analysis::analyze_raster(events, a.main_tid);
    let official_long_tasks = analysis::analyze_long_tasks_official(events, a.main_tid);
    let mut tids = std::collections::HashSet::new();
    for e in events {
        tids.insert(e.tid);
    }
    let frame_budget_us = chperf_core::windowed::detect_frame_budget_us(events);
    let thread_model = trace::detect_thread_model(events);
    let renderer_summaries = analysis::analyze_summary_per_renderer(events, &thread_model);
    let renderer_janks =
        analysis::analyze_jank_per_renderer(events, &thread_model, Some(frame_budget_us));

    let metadata = a.trace.metadata.clone();
    app::App::new(
        a.summary.clone(),
        a.scroll_frames.clone(),
        a.cpu_profile.clone(),
        a.layout_dirty.clone(),
        a.style_recalc.clone(),
        a.forced_reflows.clone(),
        a.jank.clone(),
        loading,
        network,
        raster,
        official_long_tasks,
        thread_model,
        renderer_summaries,
        renderer_janks,
        a.trace.stats.clone(),
        tids.len(),
        a.summary.total_trace_duration_us,
        frame_budget_us,
        a.main_tid,
        compare_result,
        name_a,
        trace_name_b,
        metadata,
    )
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
            eprintln!("error: a trace file path is required (or use `--repl <trace>`)");
            std::process::exit(2);
        }
    };

    // Interactive REPL: load + analyze once, then run live queries.
    // Needs a single trace file (no directory / glob expansion here).
    if cli.repl {
        let p = Path::new(raw);
        if p.is_dir() || has_glob_magic(raw) {
            return Err(
                "usage: --repl takes a single trace file (no directory or glob pattern)".into(),
            );
        }
        return repl::run_repl(p, &cli);
    }

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

/// True when the input should be glob-expanded (contains `*`, `?` or `[`).
fn has_glob_magic(input: &str) -> bool {
    input.chars().any(|c| c == '*' || c == '?' || c == '[')
}

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
        return inspect::inspect_output(
            &trace.trace_events,
            min_ts,
            &name_a,
            cli,
            None,
            &trace.stats,
        );
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
    let analyzed_a = load_and_analyze(path_a)?;

    let b_opt = if let Some(compare_path) = path_b {
        let analyzed_b = load_and_analyze(compare_path)?;
        Some((analyzed_b, trace::trace_stem(compare_path)))
    } else {
        None
    };

    let mut app = build_app(
        &analyzed_a,
        b_opt.as_ref().map(|(b, n)| (b, n.clone())),
        trace::trace_stem(path_a),
    );
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
    // input / async sections, which only live on `Analyzed` here.
    if let Some(target) = cli.html.as_deref() {
        // Paired A-vs-B report: build the standalone B app so the HTML can
        // reuse the per-section renderers for both traces.
        let app_b_opt: Option<app::App> =
            b_opt.as_ref().map(|(bb, n)| build_app(bb, None, n.clone()));
        let b_param = match (&app_b_opt, &b_opt) {
            (Some(ab), Some((bb, _))) => Some((ab, bb.trace.trace_events.as_slice(), bb.min_ts)),
            _ => None,
        };
        let doc = html::export_html(
            &app,
            &analyzed_a.trace.trace_events,
            analyzed_a.min_ts,
            b_param,
        );
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
        let v =
            report_json::full_report_json(&app, &analyzed_a.trace.trace_events, analyzed_a.min_ts);
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
    }

    let dir: &Path = out_dir.unwrap_or_else(|| path.parent().unwrap_or(Path::new(".")));
    let out_path = |ext: &str| dir.join(format!("chperf-export-{}.{}", stem, ext));
    let events = &analyzed.trace.trace_events;

    if formats.md {
        let md = export::export_markdown(&app);
        let p = out_path("md");
        std::fs::write(&p, &md)?;
        eprintln!("  -> {}", p.display());
    }
    if formats.html {
        let doc = html::export_html(&app, events, analyzed.min_ts, None);
        let p = out_path("html");
        std::fs::write(&p, &doc)?;
        eprintln!("  -> {}", p.display());
    }
    if formats.json {
        let v = report_json::full_report_json(&app, events, analyzed.min_ts);
        let p = out_path("json");
        std::fs::write(&p, serde_json::to_string_pretty(&v)?)?;
        eprintln!("  -> {}", p.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::inspect::{compare_csv_rows, compare_json};
    use chperf_core::{
        analysis,
        inspect::{self, Scope},
        trace::{self, TraceEvent},
        windowed,
    };
    use std::path::PathBuf;

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny.json")
    }

    fn fixture_events() -> Vec<TraceEvent> {
        trace::parse_trace(&fixture_path()).unwrap().trace_events
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
        let cpu = analysis::analyze_cpu_profile_full(&events).0;
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
            &Scope {
                window: None,
                tid: None,
                pid: None,
                cat: None,
            },
            "SubmitCompositorFrameToPresentationCompositorFrame",
            1_000_000.0,
        );
        assert!(md.contains("2 paired"));
        assert!(md.contains("Dropped frames**: 2"));

        // GC: 1 major 5ms, 1 minor 1ms, 1 long task ≥50ms.
        let (md, _) = windowed::gc_section(
            &events,
            &Scope {
                window: None,
                tid: None,
                pid: None,
                cat: None,
            },
            50.0,
            1_000_000.0,
        );
        assert!(md.contains("Long tasks >50ms**: 1 total, 600.0ms combined"));

        // Anchor: FunctionCall functionName wins.
        let m = inspect::Matcher::new("shoot", false).unwrap();
        let a = windowed::find_anchor(&events, &m).unwrap();
        assert_eq!(a.kind, "FunctionCall");
        assert_eq!(a.ts, 1_005_000.0);

        // Combined find: event args + cpu-profile names/urls.
        let (md, _) = inspect::find_section(
            &events,
            &inspect::Matcher::new("player_shoot", false).unwrap(),
            &Scope {
                window: None,
                tid: None,
                pid: None,
                cat: None,
            },
            false,
            30,
            1_000_000.0,
            None,
        );
        assert!(md.contains("CPU profile matches (0"));
        let (md, _) = inspect::find_section(
            &events,
            &inspect::Matcher::new("weapon.ts", false).unwrap(),
            &Scope {
                window: None,
                tid: None,
                pid: None,
                cat: None,
            },
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
            state >> 33
        };
        let names = [
            "RunTask",
            "FunctionCall",
            "ProfileChunk",
            "thread_name",
            "MajorGC",
            "MinorGC",
            "DroppedFrame",
            "SubmitCompositorFrameToPresentationCompositorFrame",
            "Paint",
            "Profile",
            "Layout",
            "UpdateLayoutTree",
        ];
        let phases = ["X", "b", "e", "P", "M", "I", "B", "E", "N", ""];

        for iter in 0..60 {
            let n_ev = (rng() % 200) as usize;
            let mut arr: Vec<serde_json::Value> = Vec::with_capacity(n_ev + 2);
            arr.push(
                serde_json::json!({"name": "thread_name", "cat": "__metadata", "ph": "M", "ts": 0}),
            );
            arr.push(
                serde_json::json!({"name": "Profile", "ph": "P", "ts": 1_000_000.0, "pid": 2}),
            );
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
                            let deltas: Vec<f64> =
                                (0..n_s).map(|_| (rng() % 50) as f64 - 20.0).collect();
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

            let path = std::env::temp_dir().join(format!(
                "chperf-fuzz-{}-{}.json",
                std::process::id(),
                iter
            ));
            std::fs::write(&path, serde_json::to_string(&trace).unwrap()).unwrap();

            let events = trace::parse_trace(&path).unwrap().trace_events;
            let _ = std::fs::remove_file(&path);

            // Full scan: totals finite and non-negative.
            let (_nodes, full) = analysis::scan_profile_chunks(&events, None, 0);
            let full_total: f64 = full.values().sum();
            assert!(
                full_total.is_finite() && full_total >= 0.0,
                "iter {}: bad full total {}",
                iter,
                full_total
            );

            // Whole-trace window == full scan; partial window ⊆ full.
            let (min_ts, max_ts) = {
                let mut mn = f64::INFINITY;
                let mut mx = 0.0f64;
                for e in &events {
                    if trace::is_metadata_event(e) {
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
            let scope_all = Scope {
                window: Some((min_ts, max_ts)),
                tid: None,
                pid: None,
                cat: None,
            };
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
            let scope_half = Scope {
                window: Some((min_ts, mid)),
                tid: None,
                pid: None,
                cat: None,
            };
            let (_, win_half) = analysis::scan_profile_chunks(&events, Some(&scope_half), 0);
            for (id, t) in &win_half {
                assert!(
                    *t <= full.get(id).copied().unwrap_or(0.0) + 1e-9,
                    "iter {}: windowed > full for node {:?}",
                    iter,
                    id
                );
            }

            // Analysis passes must not panic on garbage.
            let _ = analysis::analyze_cpu_profile_full(&events).0;
            let _ = analysis::analyze_summary(&events, 1);
            let _ = analysis::analyze_jank(&events, 1, None);
            let _ = analysis::analyze_jank(&events, 1, Some(&scope_half));
            let _ = trace::detect_main_thread(&events);
            let main_tid = trace::detect_main_thread(&events);
            let _ = windowed::gc_section(
                &events,
                &Scope {
                    window: Some((min_ts, max_ts)),
                    tid: Some(main_tid),
                    pid: None,
                    cat: None,
                },
                50.0,
                min_ts,
            );
            let _ = windowed::frames_section(
                &events,
                &Scope {
                    window: Some((min_ts, max_ts)),
                    tid: None,
                    pid: None,
                    cat: None,
                },
                "SubmitCompositorFrameToPresentationCompositorFrame",
                min_ts,
            );
            let _ = windowed::find_anchor(&events, &inspect::Matcher::new("shoot", false).unwrap());
            let _ = windowed::calltree_section(
                &events,
                &Scope {
                    window: None,
                    tid: None,
                    pid: None,
                    cat: None,
                },
                None,
                None,
                30,
                min_ts,
                None,
            );
        }
    }
}
