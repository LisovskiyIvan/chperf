//! Interactive REPL: load and analyze a trace once, then run live queries
//! against the in-memory data. Commands mirror the CLI flags — the session
//! keeps the parsed events and the fully analyzed `App`, so every query is a
//! single pass over memory instead of a re-parse of the file.

use crate::{Cli, Analyzed, app::App, build_app, inspect_output, load_and_analyze, trace};
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
];

struct Session {
    analyzed: Analyzed,
    name_a: String,
    app: App,
    compare_name: Option<String>,
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
        app.throttle_factor_saved = throttle;
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
    if let Some((name, val)) = t.split_once('=') {
        if let Some((_, flag)) = BARE.iter().find(|(n, _)| *n == name) {
            tokens[0] = format!("{}={}", flag, val);
            return;
        }
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
    map_bare(&mut tokens);

    let mut argv = vec!["chperf".to_string()];
    argv.extend(tokens);
    let cmd = match Cli::try_parse_from(&argv) {
        Ok(c) => c,
        Err(e) => {
            println!("{}", e.to_string().lines().next().unwrap_or("bad command"));
            return Ok(Cmd::Done);
        }
    };

    // Inspect queries run against the session data (no re-parse).
    if cmd.is_inspect() {
        inspect_output(&session.analyzed.trace.trace_events, &session.name_a, &cmd)?;
        return Ok(Cmd::Done);
    }

    // Compare: load the second trace once and rebuild the app.
    if let Some(b_path) = &cmd.compare {
        let t0 = Instant::now();
        let analyzed_b = load_and_analyze(Path::new(b_path))?;
        let name_b = trace::trace_stem(Path::new(b_path));
        session.app = build_app(
            &session.analyzed,
            Some((&analyzed_b, name_b.clone())),
            session.name_a.clone(),
        );
        session.compare_name = Some(name_b.clone());
        println!(
            "# compare `{}` vs `{}` built in {:.1}s\n",
            session.name_a,
            name_b,
            t0.elapsed().as_secs_f64()
        );
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
    if cmd.summary {
        print!("{}", crate::export::export_summary_only(&session.app));
        return Ok(Cmd::Done);
    }

    // Tune the session.
    if let Some(t) = cmd.throttle {
        session.app.throttle_factor = t;
        session.app.throttle_factor_saved = t;
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

fn print_status(s: &Session) {
    let ev = s.analyzed.trace.trace_events.len();
    let busy = s.app.summary.main_thread_busy_us / 1e6;
    println!(
        "# `{}`: {} events, main thread busy {:.1}s, long tasks {}, throttle {}x",
        s.name_a,
        ev,
        busy,
        s.app.summary.long_task_count,
        s.app.throttle_factor
    );
    if let Some(b) = &s.compare_name {
        println!("  compare: `{}`", b);
    }
}

fn print_help() {
    println!(
        "Commands (flags compose like the CLI, `--around/--window` anchor queries):"
    );
    println!("  events <name[,name]> [--sort dur|ts|name|count] [--top N] [--min-dur US]");
    println!("      [--tid TID|main] [--pid N] [--cat S] [--around MS] [--window MS] [--regex] [--full-args] [--json]");
    println!("  names [--top N] | threads [--top N] | timeline [--around MS] [--window MS] [--bucket MS]");
    println!("  stats <names> | function <pat> [--regex] | find <pat> [--regex] [--full-args]");
    println!("  worst [--task] [--stacks] [--top N] | task [--top N] | stacks [--top N] | flame [--function P]");
    println!("  compare <file2> | export [file] | summary | throttle N | status | clear | help | quit");
    println!();
}
