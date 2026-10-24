//! Interactive REPL: load and analyze a trace once, then run live queries
//! against the in-memory data. Commands mirror the CLI flags — the session
//! keeps the parsed events and the fully analyzed `App`, so every query is a
//! single pass over memory instead of a re-parse of the file.

use crate::{Analyzed, app::App, build_app, cli::Cli, inspect::inspect_output, load_and_analyze};
use chperf_core::trace;
use clap::Parser;
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

/// Bare words that map onto CLI flags (first token only).
const BARE: &[(&str, &str)] = &[
    ("events", "--events"),
    ("names", "--names"),
    ("threads", "--threads"),
    ("timeline", "--timeline"),
    ("worst", "--worst"),
    ("task", "--task"),
    ("function", "--function"),
    ("find", "--find"),
    ("stats", "--stats"),
    ("stacks", "--stacks"),
    ("flame", "--flame"),
    ("compare", "--compare"),
    ("export", "--export"),
    ("html", "--html"),
    ("summary", "--summary"),
    ("json", "--json"),
    ("throttle", "--throttle"),
    ("sort", "--sort"),
    ("top", "--top"),
    ("tid", "--tid"),
    ("pid", "--pid"),
    ("cat", "--cat"),
    ("around", "--around"),
    ("window", "--window"),
    ("bucket", "--bucket"),
    ("min-dur", "--min-dur"),
    ("regex", "--regex"),
    ("full-args", "--full-args"),
    ("jank", "--jank"),
    ("memory", "--memory"),
    ("input", "--input"),
    ("async", "--async"),
    ("gaps", "--gaps"),
    ("frame-tree", "--frame-tree"),
    ("anchor", "--anchor"),
    ("delta", "--delta"),
    ("gc", "--gc"),
    ("frames", "--frames"),
    ("calltree", "--calltree"),
    ("url", "--url"),
    ("lt", "--lt"),
    ("pre", "--pre"),
    ("post", "--post"),
    ("csv", "--csv"),
    ("loading", "--loading"),
    ("network", "--network"),
    ("raster", "--raster"),
    ("coverage", "--coverage"),
    ("frame-budget", "--frame-budget"),
];

struct Session {
    analyzed: Analyzed,
    name_a: String,
    app: App,
    compare_name: Option<String>,
    pending_window: Option<(f64, f64)>,
}

enum Cmd {
    Done,
    Quit,
    Run,
}

pub fn run_repl(path: &Path, cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let analyzed = load_and_analyze(path)?;
    let name_a = trace::trace_stem(path);
    let mut app = build_app(&analyzed, None, name_a.clone());
    let load_s = t0.elapsed().as_secs_f64();

    // Throttle: CLI flag priority, else auto-detect from trace metadata.
    let throttle = cli.throttle.unwrap_or_else(|| {
        app.metadata
            .as_ref()
            .and_then(|m| m.cpu_throttling)
            .unwrap_or(1.0)
    });
    if throttle > 1.0 {
        app.throttle_factor = throttle;
    }

    let n = analyzed.trace.trace_events.len();
    println!(
        "# chperf REPL — `{}` loaded in {:.1}s ({} events, all data in memory)\n",
        name_a, load_s, n
    );
    print_help();

    let mut session = Session {
        analyzed,
        name_a,
        app,
        compare_name: None,
        pending_window: None,
    };

    loop {
        print!("> ");
        io::stdout().flush().ok();
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => {
                println!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                println!("read error: {}", e);
                break;
            }
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match run_command(&mut session, line) {
            Ok(Cmd::Quit) => break,
            Ok(Cmd::Run) => {}
            Ok(Cmd::Done) => {}
            Err(e) => println!("error: {}", e),
        }
    }
    Ok(())
}

/// Split a command line into tokens, honoring single/double quotes and
/// backslash escapes.
fn tokenize(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_s = false;
    let mut in_d = false;
    let mut esc = false;
    for c in line.chars() {
        if esc {
            cur.push(c);
            esc = false;
        } else if c == '\\' && !in_s {
            esc = true;
        } else if c == '\'' && !in_d {
            in_s = !in_s;
        } else if c == '"' && !in_s {
            in_d = !in_d;
        } else if (c == ' ' || c == '\t') && !in_s && !in_d {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Map the first bare token onto its CLI flag: `events RunTask` →
/// `--events RunTask`, `export=out.md` → `--export=out.md`.
/// `stats <names>` is special: stats applies to `--events`, so it expands to
/// `--events <names> --stats`.
fn map_bare(tokens: &mut Vec<String>) {
    if tokens.is_empty() {
        return;
    }
    let t = tokens[0].clone();
    if t == "stats" {
        tokens[0] = "--events".to_string();
        tokens.push("--stats".to_string());
        return;
    }
    if t == "gaps" && tokens.len() > 1 && !tokens[1].starts_with('-') {
        tokens[0] = "--events".to_string();
        tokens.push("--gaps".to_string());
        return;
    }
    if let Some((name, val)) = t.split_once('=')
        && let Some((_, flag)) = BARE.iter().find(|(n, _)| *n == name)
    {
        tokens[0] = format!("{}={}", flag, val);
        return;
    }
    if let Some((_, flag)) = BARE.iter().find(|(n, _)| *n == t) {
        tokens[0] = flag.to_string();
    }
}

fn run_command(session: &mut Session, line: &str) -> Result<Cmd, Box<dyn std::error::Error>> {
    let mut tokens = tokenize(line);
    if tokens.is_empty() {
        return Ok(Cmd::Run);
    }
    match tokens[0].as_str() {
        "quit" | "exit" | "q" => return Ok(Cmd::Quit),
        "help" | "?" => {
            print_help();
            return Ok(Cmd::Done);
        }
        "clear" => {
            print!("\x1b[2J\x1b[H");
            return Ok(Cmd::Done);
        }
        "status" | "info" => {
            print_status(session);
            return Ok(Cmd::Done);
        }
        _ => {}
    }
    // `task 3`: expand the 3rd heaviest RunTask (same output as --task row expansion).
    if tokens[0] == "task"
        && tokens.len() == 2
        && let Ok(n) = tokens[1].parse::<usize>()
    {
        return run_task_n(session, n);
    }
    // `jank 3`: set the window around jank cluster #3's span.
    if tokens[0] == "jank"
        && tokens.len() == 2
        && let Ok(n) = tokens[1].parse::<usize>()
    {
        return run_jank_n(session, n);
    }
    map_bare(&mut tokens);

    let mut argv = vec!["chperf".to_string()];
    argv.extend(tokens);
    let mut cmd = match Cli::try_parse_from(&argv) {
        Ok(c) => c,
        Err(e) => {
            println!("{}", e.to_string().lines().next().unwrap_or("bad command"));
            return Ok(Cmd::Done);
        }
    };
    // Apply the REPL's pending window (set by `jank N`) when the command
    // carries no explicit anchor.
    if let Some((lo, hi)) = session.pending_window
        && cmd.around.is_none()
        && cmd.anchor.is_none()
        && !cmd.worst
        && cmd.window.is_none()
    {
        let center = (lo + hi) / 2.0;
        let half_ms = (hi - lo) / 2.0 / 1000.0;
        cmd.around = Some((center - session.analyzed.min_ts) / 1000.0);
        cmd.window = Some(half_ms);
    }

    // Inspect queries run against the session data (no re-parse), unless a
    // compare was requested in the same command: `compare b.json --gc` must
    // build the comparison (loading B) before the flags apply — dropping the
    // compare silently left the user on single-trace output.
    if let Some(b_path) = &cmd.compare {
        let t0 = Instant::now();
        let analyzed_b = load_and_analyze(Path::new(b_path))?;
        let name_b = trace::trace_stem(Path::new(b_path));
        // build_app constructs a fresh App; carry the session throttle over
        // or a previously set `throttle N` silently resets to 1x.
        let throttle = session.app.throttle_factor;
        session.app = build_app(
            &session.analyzed,
            Some((&analyzed_b, name_b.clone())),
            session.name_a.clone(),
        );
        session.app.throttle_factor = throttle;
        session.compare_name = Some(name_b.clone());
        println!(
            "# compare `{}` vs `{}` built in {:.1}s\n",
            session.name_a,
            name_b,
            t0.elapsed().as_secs_f64()
        );
        if !cmd.is_inspect() {
            return Ok(Cmd::Done);
        }
    }

    // Inspect queries run against the session data (no re-parse).
    if cmd.is_inspect() {
        inspect_output(
            &session.analyzed.trace.trace_events,
            session.analyzed.min_ts,
            &session.name_a,
            &cmd,
            Some(&session.analyzed.cpu_cache),
            &session.analyzed.trace.stats,
        )?;
        return Ok(Cmd::Done);
    }

    // Export (optionally summary-only) from the built app.
    if let Some(target) = &cmd.export {
        let md = if cmd.summary {
            crate::export::export_summary_only(&session.app)
        } else {
            crate::export::export_markdown(&session.app)
        };
        if target == "-" {
            print!("{}", md);
        } else {
            std::fs::write(target, &md)?;
            println!("exported to {}", target);
        }
        return Ok(Cmd::Done);
    }
    // Self-contained HTML report (trace A only; the compared trace's raw
    // events are dropped after `compare` rebuilds the app).
    if let Some(target) = &cmd.html {
        if session.compare_name.is_some() {
            eprintln!("warning: HTML export renders trace A only (compare B dropped)");
        }
        let doc = crate::html::export_html(
            &session.app,
            &session.analyzed.trace.trace_events,
            session.analyzed.min_ts,
            None,
        );
        if target == "-" {
            print!("{}", doc);
        } else {
            std::fs::write(target, &doc)?;
            println!("exported HTML to {}", target);
        }
        return Ok(Cmd::Done);
    }
    if cmd.summary {
        print!("{}", crate::export::export_summary_only(&session.app));
        return Ok(Cmd::Done);
    }

    // Tune the session.
    if let Some(t) = cmd.throttle {
        session.app.throttle_factor = t;
        println!("throttle = {}x", t);
        return Ok(Cmd::Done);
    }

    // Nothing matched: the positional held the unknown word.
    if let Some(t) = cmd.trace.as_deref() {
        println!("? unknown command `{}` (try `help`)", t);
    } else {
        println!("? nothing to do (try `help`)");
    }
    Ok(Cmd::Done)
}

fn run_task_n(session: &mut Session, n: usize) -> Result<Cmd, Box<dyn std::error::Error>> {
    if n == 0 {
        println!("usage: task <rank>=1..");
        return Ok(Cmd::Done);
    }
    let scope = chperf_core::inspect::Scope {
        window: None,
        tid: None,
        pid: None,
        cat: None,
    };
    let (md, _) = chperf_core::inspect::task_section(
        &session.analyzed.trace.trace_events,
        &scope,
        n,
        session.analyzed.min_ts,
    );
    // Print only the Nth `### #N` block (same output as --task row expansion).
    let blocks: Vec<&str> = md.split("### #").collect();
    if n < blocks.len() {
        print!("### #{}", blocks[n]);
    } else {
        println!(
            "no task #{} (only {} tasks)",
            n,
            blocks.len().saturating_sub(1)
        );
    }
    Ok(Cmd::Done)
}

fn run_jank_n(session: &mut Session, n: usize) -> Result<Cmd, Box<dyn std::error::Error>> {
    if n == 0 {
        println!("usage: jank <cluster#>=1..");
        return Ok(Cmd::Done);
    }
    let events = &session.analyzed.trace.trace_events;
    let main_tid = chperf_core::trace::detect_main_thread(events);
    let res = chperf_core::analysis::analyze_jank(events, main_tid, None);
    if n > res.clusters.len() {
        println!(
            "no jank cluster #{} (only {} clusters)",
            n,
            res.clusters.len()
        );
        return Ok(Cmd::Done);
    }
    let c = &res.clusters[n - 1];
    session.pending_window = Some((c.start_us, c.end_us));
    println!(
        "# window set around jank cluster #{}: {:.2}ms … {:.2}ms from trace start (span {:.2}ms, {} dropped)",
        n,
        (c.start_us - session.analyzed.min_ts) / 1000.0,
        (c.end_us - session.analyzed.min_ts) / 1000.0,
        (c.end_us - c.start_us) / 1000.0,
        c.dropped_frames,
    );
    Ok(Cmd::Done)
}

fn print_status(s: &Session) {
    let ev = s.analyzed.trace.trace_events.len();
    let busy = s.app.summary.main_thread_busy_us / 1e6;
    println!(
        "# `{}`: {} events, main thread busy {:.1}s, long tasks {}, throttle {}x",
        s.name_a, ev, busy, s.app.summary.long_task_count, s.app.throttle_factor
    );
    if let Some(b) = &s.compare_name {
        println!("  compare: `{}`", b);
    }
}

fn print_help() {
    println!("Commands (flags compose like the CLI, `--around/--window` anchor queries):");
    println!("  events <name[,name]> [--sort dur|ts|name|count] [--top N] [--min-dur US]");
    println!(
        "      [--tid TID|main] [--pid N] [--cat S] [--around MS] [--window MS] [--regex] [--full-args] [--json]"
    );
    println!(
        "  names [--top N] | threads [--top N] | timeline [--around MS] [--window MS] [--bucket MS]"
    );
    println!(
        "  stats <names> | gaps <names> | function <pat> [--regex] | find <pat> [--regex] [--full-args] | jank [--top N] | jank N (window around cluster N)"
    );
    println!(
        "  worst [--task] [--stacks] [--top N] | task [--top N] | task N (expand Nth RunTask) | stacks [--function P] [--top N] | flame [--function P]"
    );
    println!(
        "  anchor <pat> [--delta] [--pre MS] [--post MS] [--window MS] | delta [--frame-budget MS] | calltree [--url P] [--function P]"
    );
    println!(
        "  gc [--lt MS] | frames [--frame-event NAME] [--frame-budget MS] | frame-tree | csv (add --csv to any query)"
    );
    println!("  memory [--around MS] [--window MS] | input [--top N] | async [--top N]");
    println!(
        "  loading | network | raster | coverage | frame-budget MS | bucket MS | url P | lt MS | pre MS | post MS"
    );
    println!(
        "  compare <file2> | export [file] | html [file] | summary | throttle N | status | clear | help | quit"
    );
    println!("  every query also accepts [--json] for jq/pipelines");
    println!();
}
