mod analysis;
mod app;
mod export;
mod inspect;
mod trace;
mod ui;

use std::io;
use std::path::{Path, PathBuf};

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;

#[derive(Parser)]
#[command(name = "chperf", about = "Chrome DevTools Trace JSON analyzer (TUI)")]
struct Cli {
    /// Path to trace JSON file (.json or .json.gz) or a directory of traces
    trace: String,

    /// Optional second trace file for comparison
    #[arg(short, long)]
    compare: Option<String>,

    /// Export analysis as Markdown (to stdout or file)
    /// Use --export to print to stdout, --export=FILE to write to file
    #[arg(short, long, num_args = 0..=1, default_missing_value = "-")]
    export: Option<String>,

    /// CPU throttle factor (e.g. --throttle 20 divides all times by 20)
    #[arg(short, long)]
    throttle: Option<f64>,

    /// Export only the comparison summary table (use with --export --compare)
    #[arg(short, long)]
    summary: bool,

    /// Inspect: list events by name (comma-separated), e.g. --events GPUTask,RunTask
    #[arg(long)]
    events: Option<String>,

    /// Inspect: aggregate CPU samples whose function name contains this substring
    #[arg(long)]
    function: Option<String>,

    /// Inspect: search event args (JSON) for this substring
    #[arg(long)]
    find: Option<String>,

    /// Inspect: center of the time window, in ms from trace start (use with --window)
    #[arg(long)]
    around: Option<f64>,

    /// Inspect: half-width of the time window in ms (default 100, use with --around)
    #[arg(long)]
    window: Option<f64>,

    /// Inspect: only events with duration >= this value, in microseconds
    #[arg(long)]
    min_dur: Option<f64>,

    /// Inspect: limit number of rows (default 30)
    #[arg(long, default_value_t = 30)]
    top: usize,

    /// Inspect: restrict events/functions/find to this thread id (tid)
    #[arg(long)]
    tid: Option<u64>,

    /// Inspect: restrict events/functions/find to this process id (pid)
    #[arg(long)]
    pid: Option<u64>,

    /// Inspect: list distinct event names with counts/total duration
    #[arg(long)]
    names: bool,

    /// Inspect: list distinct threads (tid) with counts/RunTask duration
    #[arg(long)]
    threads: bool,

    /// Inspect: heaviest CPU call stacks (root → leaf), heaviest first
    #[arg(long)]
    stacks: bool,

    /// Inspect: interpret --function/--find/--events as regex instead of substring/exact
    #[arg(long)]
    regex: bool,
}

impl Cli {
    /// Any granular-inspect flag present?
    fn is_inspect(&self) -> bool {
        self.events.is_some()
            || self.function.is_some()
            || self.find.is_some()
            || self.names
            || self.threads
            || self.stacks
    }
}

// ── Analyzed trace ──

/// All derived analysis for a single parsed trace.
struct Analyzed {
    trace: trace::TraceFile,
    #[allow(dead_code)]
    main_tid: u64,
    summary: analysis::SummaryResult,
    scroll_frames: analysis::ScrollFrameResult,
    cpu_profile: analysis::CpuProfileResult,
    layout_dirty: analysis::LayoutDirtyResult,
    style_recalc: analysis::StyleRecalcResult,
    forced_reflows: analysis::ForcedReflowResult,
}

fn load_and_analyze(path: &Path) -> Result<Analyzed, Box<dyn std::error::Error>> {
    eprintln!("Loading {}...", path.display());
    let trace = trace::parse_trace(path)?;
    let main_tid = trace::detect_main_thread(&trace.trace_events);
    eprintln!(
        "  {} events, main thread tid={}",
        trace.trace_events.len(),
        main_tid
    );
    Ok(Analyzed {
        summary: analysis::analyze_summary(&trace.trace_events, main_tid),
        scroll_frames: analysis::analyze_scroll_frames(&trace.trace_events, main_tid),
        cpu_profile: analysis::analyze_cpu_profile(&trace.trace_events),
        layout_dirty: analysis::analyze_layout_dirty(&trace.trace_events, main_tid),
        style_recalc: analysis::analyze_style_recalc(&trace.trace_events, main_tid),
        forced_reflows: analysis::analyze_forced_reflows(&trace.trace_events, main_tid),
        trace,
        main_tid,
    })
}

/// Build the App (with optional compare) from analyzed traces.
fn build_app(
    a: Analyzed,
    b: Option<(Analyzed, String)>,
    name_a: String,
) -> (app::App, trace::TraceFile) {
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
    let app = app::App::new(
        a.summary,
        a.scroll_frames,
        a.cpu_profile,
        a.layout_dirty,
        a.style_recalc,
        a.forced_reflows,
        compare_result,
        name_a,
        trace_name_b,
        metadata,
    );
    (app, a.trace)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let trace_path = Path::new(&cli.trace);

    // Directory mode: scan for traces, then batch-export or list them.
    if trace_path.is_dir() {
        let traces = trace::list_traces(trace_path)?;
        if traces.is_empty() {
            eprintln!(
                "No trace files (*.json / *.json.gz) found in {}",
                cli.trace
            );
            return Ok(());
        }
        if cli.export.is_some() {
            return batch_export(&cli, &traces);
        }
        // No TUI picker: list traces and guide the user to pick one explicitly.
        eprintln!("Found {} trace(s) in {}:", traces.len(), cli.trace);
        for (i, p) in traces.iter().enumerate() {
            eprintln!("  {:>3}. {}", i + 1, p.display());
        }
        eprintln!(
            "\nAnalyze one: chperf <file> [--export] [--compare <file2>] | inspect: --events/--function/--find"
        );
        eprintln!("Export all:  chperf {} --export", cli.trace);
        return Ok(());
    }

    // Granular inspect mode: print targeted tables to stdout, skip analysis/TUI.
    if cli.is_inspect() {
        return run_inspect(trace_path, &cli);
    }

    run_single(trace_path, cli.compare.as_deref().map(Path::new), &cli)
}

/// Granular inspection: events by name, CPU functions/stacks, or arg search,
/// each scoped to a window/thread/process. Prints Markdown to stdout.
fn run_inspect(path: &Path, cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Loading {}...", path.display());
    let trace = trace::parse_trace(path)?;
    let events = &trace.trace_events;
    let min_ts = inspect::trace_start_us(events);
    let window = inspect::window_us(cli.around, cli.window, min_ts);
    let min_dur_us = cli.min_dur.unwrap_or(0.0);
    let scope = inspect::Scope {
        window,
        tid: cli.tid,
        pid: cli.pid,
    };

    let mut out = String::new();
    out.push_str(&format!("# chperf inspect: {}\n\n", trace::trace_stem(path)));
    if let Some((lo, hi)) = window {
        out.push_str(&format!(
            "**Window**: {:.2}ms … {:.2}ms from trace start\n\n",
            (lo - min_ts) / 1000.0,
            (hi - min_ts) / 1000.0,
        ));
    }

    if cli.names {
        out.push_str(&inspect::names_md(events, &scope, cli.top, min_ts));
    }

    if cli.threads {
        out.push_str(&inspect::threads_md(events, &scope, cli.top, min_ts));
    }

    if let Some(names_raw) = &cli.events {
        let names: Vec<String> = names_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let filter = inspect::NameFilter::new(&names, cli.regex)?;
        out.push_str(&inspect::events_md(
            events,
            &filter,
            names_raw.trim(),
            &scope,
            min_dur_us,
            cli.top,
            min_ts,
        ));
    }

    // Functions and stacks can share a single matcher (filter by name).
    let func_matcher = if let Some(pattern) = &cli.function {
        Some(inspect::Matcher::new(pattern, cli.regex)?)
    } else {
        None
    };

    if let Some(m) = &func_matcher {
        out.push_str(&inspect::functions_md(events, m, &scope, cli.top, min_ts));
    }

    if cli.stacks {
        out.push_str(&inspect::stacks_md(
            events,
            func_matcher.as_ref(),
            &scope,
            cli.top,
            min_ts,
        ));
    }

    if let Some(needle) = &cli.find {
        let m = inspect::Matcher::new(needle, cli.regex)?;
        out.push_str(&inspect::find_md(events, &m, &scope, cli.top, min_ts));
    }

    print!("{}", out);
    Ok(())
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

    let (mut app, trace_a) = build_app(analyzed_a, b_opt, trace::trace_stem(path_a));

    // Apply throttle: CLI flag takes priority, otherwise auto-detect from trace metadata
    let throttle = cli.throttle.unwrap_or_else(|| {
        trace_a
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
fn batch_export(cli: &Cli, traces: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    let summary_only = cli.summary;
    for path in traces {
        let stem = trace::trace_stem(path);
        let analyzed = load_and_analyze(path)?;
        let (mut app, _trace) = build_app(analyzed, None, stem.clone());

        // Throttle: CLI flag priority, else auto-detect from trace metadata
        let throttle = cli.throttle.unwrap_or_else(|| {
            app.metadata
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
    }
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
                KeyCode::Char('1') => {
                    if !app.tabs.is_empty() {
                        app.tab = app.tabs[0];
                        app.scroll_offset = 0;
                    }
                }
                KeyCode::Char('2') => {
                    if app.tabs.len() > 1 {
                        app.tab = app.tabs[1];
                        app.scroll_offset = 0;
                    }
                }
                KeyCode::Char('3') => {
                    if app.tabs.len() > 2 {
                        app.tab = app.tabs[2];
                        app.scroll_offset = 0;
                    }
                }
                KeyCode::Char('4') => {
                    if app.tabs.len() > 3 {
                        app.tab = app.tabs[3];
                        app.scroll_offset = 0;
                    }
                }
                KeyCode::Char('5') => {
                    if app.tabs.len() > 4 {
                        app.tab = app.tabs[4];
                        app.scroll_offset = 0;
                    }
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
