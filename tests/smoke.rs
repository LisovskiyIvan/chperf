//! CLI smoke matrix: run the built binary against the tiny fixture with many
//! flag combinations and assert exit codes / non-empty output / valid JSON.
//! Catches panics (unwraps, OOB) on flag interactions that unit tests miss.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_chperf");
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.json");

struct Out {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Out {
    let out = Command::new(BIN).args(args).output().expect("spawn chperf");
    Out {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn ok(args: &[&str]) -> Out {
    let o = run(args);
    assert_eq!(o.status, 0, "args {args:?} exited {}: {}{}", o.status, o.stdout, o.stderr);
    assert!(!o.stdout.is_empty(), "args {args:?} produced no stdout");
    o
}

fn contains(args: &[&str], needle: &str) {
    let o = ok(args);
    assert!(o.stdout.contains(needle), "args {args:?}: missing `{needle}` in:\n{}", o.stdout);
}

fn exits_nonzero(args: &[&str]) {
    let o = run(args);
    assert_ne!(o.status, 0, "args {args:?} should have failed, got:\n{}", o.stdout);
}

#[test]
fn smoke_basic_sections() {
    contains(&[FIXTURE, "--names"], "## Event names");
    contains(&[FIXTURE, "--threads"], "## Threads");
    contains(&[FIXTURE, "--timeline", "--around", "1000"], "## Timeline");
    contains(&[FIXTURE, "--task", "--top", "2"], "## RunTask breakdown");
    contains(&[FIXTURE, "--events", "RunTask", "--stats"], "## Duration stats");
    contains(&[FIXTURE, "--worst", "--events", "RunTask"], "## Events");
}

#[test]
fn smoke_windowed() {
    contains(&[FIXTURE, "--anchor", "shoot"], "Anchored at FunctionCall `shoot`");
    contains(&[FIXTURE, "--anchor", "shoot", "--delta"], "## Delta: PRE → SHOOT → POST");
    contains(&[FIXTURE, "--delta"], "requires an anchor"); // exit 0 with message
    contains(&[FIXTURE, "--anchor", "shoot", "--stacks", "--function", "shoot"], "windowed");
    contains(&[FIXTURE, "--stacks", "--around", "1000", "--window", "100"], "## Heaviest call stacks");
    contains(&[FIXTURE, "--calltree", "--url", "weapon.ts"], "incl 20.00ms");
    contains(&[FIXTURE, "--url", "weapon.ts", "--stacks"], "## Heaviest call stacks");
}

#[test]
fn smoke_frames_gc_find() {
    contains(&[FIXTURE, "--frames"], "2 paired");
    contains(&[FIXTURE, "--frames"], "Dropped frames**: 2");
    contains(&[FIXTURE, "--gc"], "Long tasks ≥50ms**: 1 total, 600.0ms combined");
    contains(&[FIXTURE, "--gc", "--lt", "30"], "Long tasks ≥30ms");
    contains(&[FIXTURE, "--find", "player_shoot"], "1 matches");
    contains(&[FIXTURE, "--find", "player_shoot"], "CPU profile matches (0");
    contains(&[FIXTURE, "--anchor", "shoot", "--gc"], "windowed");
    contains(&[FIXTURE, "--anchor", "shoot", "--frames", "--window", "50"], "paired");
}

#[test]
fn smoke_json_csv() {
    let o = ok(&[FIXTURE, "--function", "shoot", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert!(v["sections"]["functions"].as_array().unwrap().len() >= 2);

    let o = ok(&[FIXTURE, "--anchor", "shoot", "--delta", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert_eq!(v["sections"]["delta"]["metrics"][0]["metric"], "frames");

    contains(&[FIXTURE, "--frames", "--csv"], "# frames");
    let o = ok(&[FIXTURE, "--anchor", "shoot", "--delta", "--csv"]);
    assert!(o.stdout.contains("# delta.metrics"), "delta csv missing metrics block");
    contains(&[FIXTURE, "--names", "--csv"], "# names");
}

#[test]
fn smoke_compare_windowed() {
    let o = ok(&[FIXTURE, "--compare", FIXTURE, "--anchor", "shoot", "--delta"]);
    assert!(o.stdout.contains("## Windowed compare: SHOOT & SHOOT\u{2212}PRE"), "missing compare table:\n{}", o.stdout);
    assert!(o.stdout.contains("## Trace A:"), "missing trace A sections");
    assert!(o.stdout.contains("## Trace B:"), "missing trace B sections");
    assert!(o.stdout.contains("B\u{2212}A SHOOT"), "missing B\u{2212}A columns");

    let o = ok(&[FIXTURE, "--compare", FIXTURE, "--anchor", "shoot", "--delta", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert_eq!(v["trace_a"], "tiny");
    assert!(v["compare"]["rows"].as_array().unwrap().len() >= 5);
    // Self-comparison: every B\u{2212}A delta is zero.
    for row in v["compare"]["rows"].as_array().unwrap() {
        assert_eq!(row["diff_shoot"], 0.0, "self-compare diff for {}", row["metric"]);
        assert_eq!(row["diff_delta"], 0.0, "self-compare delta for {}", row["metric"]);
    }

    let o = ok(&[FIXTURE, "--compare", FIXTURE, "--anchor", "shoot", "--delta", "--csv"]);
    assert!(o.stdout.contains("# compare"), "missing compare csv block");
    assert!(o.stdout.contains("# a.delta.metrics"), "missing trace-A delta csv");
    assert!(o.stdout.contains("# b.delta.metrics"), "missing trace-B delta csv");

    // Other sections run on both traces too.
    contains(&[FIXTURE, "--compare", FIXTURE, "--frames"], "## Trace A:");
    contains(&[FIXTURE, "--compare", FIXTURE, "--anchor", "shoot", "--gc"], "windowed");
}

#[test]
fn smoke_regex_and_errors() {
    contains(&[FIXTURE, "--regex", "--events", "Run.*Task"], "## Events");
    exits_nonzero(&[FIXTURE, "--regex", "--function", "("]); // invalid regex
    exits_nonzero(&[FIXTURE, "/nonexistent/trace.json", "--names"]); // missing file
    exits_nonzero(&[FIXTURE, "--events", "RunTask", "--tid", "notanumber"]); // bad tid
    ok(&[FIXTURE, "--anchor", "zzz_no_match", "--names"]); // no-match anchor is a warning
    ok(&[FIXTURE, "--flame"]);
    ok(&[FIXTURE, "--flame", "--function", "shoot"]);
    ok(&[FIXTURE, "--jank", "--around", "1000", "--window", "100"]);
    ok(&[FIXTURE, "--around=-500", "--names"]);
    ok(&[FIXTURE, "--events", "RunTask", "--stats", "--around", "1000", "--window", "100", "--top", "3"]);
    ok(&[FIXTURE, "--function", "shoot", "--top", "1", "--tid", "main"]);
}
