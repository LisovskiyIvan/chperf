mod app;
mod cli;
mod export;
mod html;
mod inspect;
mod repl;

use chperf_core::{analysis, trace};
use crate::cli::Cli;

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
    let trace = trace::parse_trace(path)?;
    let main_tid = trace::detect_main_thread(&trace.trace_events);
    let min_ts = chperf_core::inspect::trace_start_us(&trace.trace_events);
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
        min_ts,
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

    // Granular inspect mode: print targeted tables to stdout, skip analysis.
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
    let min_ts = chperf_core::inspect::trace_start_us(&trace.trace_events);
    let Some(path_b) = cli.compare.as_deref().map(Path::new) else {
        return inspect::inspect_output(&trace.trace_events, min_ts, &name_a, cli, None);
    };
    eprintln!("Loading {}...", path_b.display());
    let trace_b = trace::parse_trace(path_b)?;
    eprintln!("  {} events", trace_b.trace_events.len());
    inspect::inspect_compare_output(
        &trace.trace_events,
        &name_a,
        &trace_b.trace_events,
        &trace::trace_stem(path_b),
        cli,
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

    // CLI-only: render the report to stdout (or a file via --export=FILE).
    // --html wins over --export: it needs the raw events for the memory /
    // input / async sections, which only live on `Analyzed` here.
    if let Some(target) = cli.html.as_deref() {
        let events_b = b_opt.as_ref().map(|(b, _)| (b.trace.trace_events.as_slice(), b.min_ts));
        let doc = html::export_html(
            &app,
            &analyzed_a.trace.trace_events,
            analyzed_a.min_ts,
            events_b,
        );
        match target {
            "-" => print!("{}", doc),
            t => {
                std::fs::write(t, &doc)?;
                eprintln!("Exported HTML to {}", t);
            }
        }
        return Ok(());
    }

    let md = if cli.summary {
        export::export_summary_only(&app)
    } else {
        export::export_markdown(&app)
    };
    match cli.export.as_deref() {
        Some("-") | None => print!("{}", md),
        Some(target) => {
            std::fs::write(target, &md)?;
            eprintln!("Exported to {}", target);
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use chperf_core::{analysis, inspect::{self, Scope}, trace::{self, TraceEvent}, windowed};
    use crate::inspect::{compare_csv_rows, compare_json};
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
            state >> 33
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

            let events = trace::parse_trace(&path).unwrap().trace_events;
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
            let _ = analysis::analyze_cpu_profile_full(&events).0;
            let _ = analysis::analyze_summary(&events, 1);
            let _ = analysis::analyze_jank(&events, 1, None);
            let _ = analysis::analyze_jank(&events, 1, Some(&scope_half));
            let _ = trace::detect_main_thread(&events);
            let main_tid = trace::detect_main_thread(&events);
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
