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
    assert_eq!(
        o.status, 0,
        "args {args:?} exited {}: {}{}",
        o.status, o.stdout, o.stderr
    );
    assert!(!o.stdout.is_empty(), "args {args:?} produced no stdout");
    o
}

fn contains(args: &[&str], needle: &str) {
    let o = ok(args);
    assert!(
        o.stdout.contains(needle),
        "args {args:?}: missing `{needle}` in:\n{}",
        o.stdout
    );
}

fn exits_nonzero(args: &[&str]) {
    let o = run(args);
    assert_ne!(
        o.status, 0,
        "args {args:?} should have failed, got:\n{}",
        o.stdout
    );
}

#[test]
fn smoke_basic_sections() {
    contains(&[FIXTURE, "--names"], "## Event names");
    contains(&[FIXTURE, "--threads"], "## Threads");
    contains(&[FIXTURE, "--timeline", "--around", "1000"], "## Timeline");
    contains(&[FIXTURE, "--task", "--top", "2"], "## RunTask breakdown");
    contains(
        &[FIXTURE, "--events", "RunTask", "--stats"],
        "## Duration stats",
    );
    contains(
        &[FIXTURE, "--events", "RunTask", "--gaps"],
        "## Event gaps & cadence",
    );
    contains(&[FIXTURE, "--frame-tree"], "## Frame / Document Tree");
    contains(&[FIXTURE, "--worst", "--events", "RunTask"], "## Events");
}

#[test]
fn smoke_windowed() {
    contains(
        &[FIXTURE, "--anchor", "shoot"],
        "Anchored at FunctionCall `shoot`",
    );
    contains(
        &[FIXTURE, "--anchor", "shoot", "--delta"],
        "## Delta: PRE → SHOOT → POST",
    );
    contains(&[FIXTURE, "--delta"], "requires an anchor"); // exit 0 with message
    contains(
        &[
            FIXTURE,
            "--anchor",
            "shoot",
            "--stacks",
            "--function",
            "shoot",
        ],
        "windowed",
    );
    contains(
        &[FIXTURE, "--stacks", "--around", "1000", "--window", "100"],
        "## Heaviest call stacks",
    );
    contains(
        &[FIXTURE, "--calltree", "--url", "weapon.ts"],
        "incl 20.00ms",
    );
    contains(
        &[FIXTURE, "--url", "weapon.ts", "--stacks"],
        "## Heaviest call stacks",
    );
}

#[test]
fn smoke_frames_gc_find() {
    contains(&[FIXTURE, "--frames"], "2 paired");
    contains(&[FIXTURE, "--frames"], "Dropped frames**: 2");
    contains(
        &[FIXTURE, "--gc"],
        "Long tasks >50ms**: 1 total, 600.0ms combined",
    );
    contains(&[FIXTURE, "--gc", "--lt", "30"], "Long tasks >30ms");
    contains(&[FIXTURE, "--find", "player_shoot"], "1 matches");
    contains(
        &[FIXTURE, "--find", "player_shoot"],
        "CPU profile matches (0",
    );
    contains(&[FIXTURE, "--anchor", "shoot", "--gc"], "windowed");
    contains(
        &[FIXTURE, "--anchor", "shoot", "--frames", "--window", "50"],
        "paired",
    );
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
    assert!(
        o.stdout.contains("# delta.metrics"),
        "delta csv missing metrics block"
    );
    contains(&[FIXTURE, "--names", "--csv"], "# names");
}

#[test]
fn smoke_compare_windowed() {
    let o = ok(&[
        FIXTURE,
        "--compare",
        FIXTURE,
        "--anchor",
        "shoot",
        "--delta",
    ]);
    assert!(
        o.stdout
            .contains("## Windowed compare: SHOOT & SHOOT\u{2212}PRE"),
        "missing compare table:\n{}",
        o.stdout
    );
    assert!(o.stdout.contains("## Trace A:"), "missing trace A sections");
    assert!(o.stdout.contains("## Trace B:"), "missing trace B sections");
    assert!(
        o.stdout.contains("B\u{2212}A SHOOT"),
        "missing B\u{2212}A columns"
    );

    let o = ok(&[
        FIXTURE,
        "--compare",
        FIXTURE,
        "--anchor",
        "shoot",
        "--delta",
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert_eq!(v["trace_a"], "tiny");
    assert!(v["compare"]["rows"].as_array().unwrap().len() >= 5);
    // Self-comparison: every B\u{2212}A delta is zero.
    for row in v["compare"]["rows"].as_array().unwrap() {
        assert_eq!(
            row["diff_shoot"], 0.0,
            "self-compare diff for {}",
            row["metric"]
        );
        assert_eq!(
            row["diff_delta"], 0.0,
            "self-compare delta for {}",
            row["metric"]
        );
    }

    let o = ok(&[
        FIXTURE,
        "--compare",
        FIXTURE,
        "--anchor",
        "shoot",
        "--delta",
        "--csv",
    ]);
    assert!(o.stdout.contains("# compare"), "missing compare csv block");
    assert!(
        o.stdout.contains("# a.delta.metrics"),
        "missing trace-A delta csv"
    );
    assert!(
        o.stdout.contains("# b.delta.metrics"),
        "missing trace-B delta csv"
    );

    // Other sections run on both traces too.
    contains(&[FIXTURE, "--compare", FIXTURE, "--frames"], "## Trace A:");
    contains(
        &[FIXTURE, "--compare", FIXTURE, "--anchor", "shoot", "--gc"],
        "windowed",
    );
}

#[test]
fn smoke_memory_input_async_html() {
    contains(&[FIXTURE, "--memory"], "## Memory timeline");
    contains(&[FIXTURE, "--input"], "## Input latency");
    contains(&[FIXTURE, "--async"], "## Async tasks");
    // Self-contained HTML: no markdown, valid DOCTYPE, includes the sections.
    let o = ok(&[FIXTURE, "--html"]);
    assert!(o.stdout.starts_with("<!DOCTYPE html>"), "bad html head");
    assert!(o.stdout.contains("CPU profile"), "html missing CPU section");
    assert!(!o.stdout.contains("## "), "html leaked markdown headers");
}

#[test]
fn smoke_regex_and_errors() {
    contains(&[FIXTURE, "--regex", "--events", "Run.*Task"], "## Events");
    exits_nonzero(&[FIXTURE, "--regex", "--function", "("]); // invalid regex
    exits_nonzero(&[FIXTURE, "/nonexistent/trace.json", "--names"]); // missing file
    exits_nonzero(&[FIXTURE, "--events", "RunTask", "--tid", "notanumber"]); // bad tid
    // anchor miss hard-fails (exit 2) unless --full-on-miss
    exits_nonzero(&[FIXTURE, "--anchor", "zzz_no_match", "--names"]);
    ok(&[
        FIXTURE,
        "--anchor",
        "zzz_no_match",
        "--full-on-miss",
        "--names",
    ]);
    // --window without anchor errors
    exits_nonzero(&[FIXTURE, "--names", "--window", "100"]);
    // --stats/--gaps without --events error
    exits_nonzero(&[FIXTURE, "--stats"]);
    exits_nonzero(&[FIXTURE, "--gaps"]);
    ok(&[FIXTURE, "--flame"]);
    ok(&[FIXTURE, "--flame", "--function", "shoot"]);
    ok(&[FIXTURE, "--jank", "--around", "1000", "--window", "100"]);
    ok(&[FIXTURE, "--around=-500", "--names"]);
    ok(&[
        FIXTURE, "--events", "RunTask", "--stats", "--around", "1000", "--window", "100", "--top",
        "3",
    ]);
    ok(&[
        FIXTURE,
        "--function",
        "shoot",
        "--top",
        "1",
        "--tid",
        "main",
    ]);
}

fn exits_code(args: &[&str], want: i32) -> Out {
    let o = run(args);
    assert_eq!(
        o.status, want,
        "args {args:?} exited {}, want {want}: {}{}",
        o.status, o.stdout, o.stderr
    );
    o
}

#[test]
fn smoke_frame_budget() {
    // --frame-budget override flows into --frames/--jank/--delta JSON + markdown.
    let o = ok(&[FIXTURE, "--frames", "--frame-budget", "8.33", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert_eq!(v["sections"]["frames"][0]["frame_budget_us"], 8330.0);
    contains(
        &[FIXTURE, "--frames", "--frame-budget", "8.33"],
        "Frame budget",
    );
    contains(&[FIXTURE, "--frames", "--frame-budget", "8.33"], "8.33ms");
    let o = ok(&[FIXTURE, "--jank", "--frame-budget", "16.67", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert!(v["sections"]["jank"].is_array());
    contains(&[FIXTURE, "--jank", "--bucket", "25"], "25ms buckets");
}

#[test]
fn smoke_new_detectors() {
    contains(&[FIXTURE, "--loading"], "## Loading milestones");
    contains(&[FIXTURE, "--loading", "--json"], "navigation_start_us");
    contains(&[FIXTURE, "--network"], "## Network");
    contains(&[FIXTURE, "--raster"], "## Raster");
    // Empty detectors render explicit none-found (tiny has no network/raster/loading data).
    contains(&[FIXTURE, "--network"], "none found (0 requests)");
    contains(&[FIXTURE, "--raster"], "No raster/compositor events found");
    // Window scoping works for new detectors.
    ok(&[FIXTURE, "--loading", "--around", "1000", "--window", "100"]);
    ok(&[FIXTURE, "--network", "--around", "1000", "--window", "100"]);
    ok(&[FIXTURE, "--raster", "--around", "1000", "--window", "100"]);
}

#[test]
fn smoke_coverage() {
    contains(&[FIXTURE, "--coverage"], "## Trace coverage");
    contains(&[FIXTURE, "--coverage"], "Recognized");
    let o = ok(&[FIXTURE, "--coverage", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert!(v["sections"]["coverage"]["recognized"].as_u64().unwrap() > 0);
    assert!(v["sections"]["coverage"]["parsed"].as_u64().unwrap() >= 17);
}

#[test]
fn smoke_full_report_json_shape() {
    let o = ok(&[FIXTURE, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    for k in [
        "trace",
        "trace_health",
        "summary",
        "frame_blocking_tasks",
        "cpu_profile",
        "jank",
        "loading",
        "network",
        "raster",
    ] {
        assert!(v.get(k).is_some(), "full JSON missing key {k}");
    }
    // *_us + ms mirrors.
    assert!(v["summary"]["duration_us"].is_number());
    assert!(v["summary"]["duration_ms"].is_number());
    assert!(v["trace_health"]["frame_budget_us"].is_number());
    assert!(v["trace_health"]["parsed"].is_number());
    // Event stats carry inclusive + self.
    let es = v["summary"]["event_stats"].as_array().unwrap();
    assert!(!es.is_empty());
    assert!(es[0].get("total_us").is_some());
    assert!(es[0].get("self_us").is_some());
    // CSV without query errors.
    exits_code(&[FIXTURE, "--csv"], 2);
}

#[test]
fn smoke_exit_codes() {
    // scope hard-fail exits 2
    exits_code(&[FIXTURE, "--anchor", "zzz_no_match", "--names"], 2);
    exits_code(
        &[
            FIXTURE,
            "--anchor",
            "zzz_no_match",
            "--full-on-miss",
            "--names",
        ],
        0,
    );
    // modifier gating exits 2
    exits_code(&[FIXTURE, "--top", "5"], 2);
    exits_code(&[FIXTURE, "--around", "10"], 2);
    // --summary without --compare exits 2
    exits_code(&[FIXTURE, "--summary"], 2);
    // --fail-on-regressed without --compare exits 2
    exits_code(&[FIXTURE, "--fail-on-regressed"], 2);
    // parse/io errors exit 1 with clean prefix
    let o = exits_code(&["/nonexistent/trace.json", "--names"], 1);
    assert!(o.stderr.contains("chperf:"), "stderr: {}", o.stderr);
    assert!(!o.stderr.contains("Error:"), "stderr: {}", o.stderr);
    // self-compare with --fail-on-regressed exits 0 (no regressed findings)
    exits_code(&[FIXTURE, "--compare", FIXTURE, "--fail-on-regressed"], 0);
    // regressed B exits 3
    {
        let dir = std::env::temp_dir().join(format!("chperf-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.json");
        let b = dir.join("b.json");
        let mk = |dur: u64| {
            serde_json::json!({"traceEvents": [
                {"name": "thread_name", "cat": "__metadata", "ph": "M", "ts": 0, "pid": 1, "tid": 1, "args": {"name": "CrRendererMain"}},
                {"name": "RunTask", "ph": "X", "ts": 1000000, "dur": dur, "pid": 1, "tid": 1},
            ]})
        };
        std::fs::write(&a, serde_json::to_string(&mk(60000)).unwrap()).unwrap();
        std::fs::write(&b, serde_json::to_string(&mk(600000)).unwrap()).unwrap();
        let o = run(&[
            a.to_str().unwrap(),
            "--compare",
            b.to_str().unwrap(),
            "--fail-on-regressed",
        ]);
        assert_eq!(
            o.status, 3,
            "regressed compare should exit 3: {}{}",
            o.stdout, o.stderr
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn smoke_full_report_sections() {
    // Trust header + honest labels + empty sections explicit.
    contains(&[FIXTURE], "## Trace health");
    contains(&[FIXTURE], "Frame-Blocking Main-Thread Tasks");
    contains(&[FIXTURE], "Heuristic fallback");
    contains(&[FIXTURE], "## Loading");
    contains(&[FIXTURE], "## Network");
    contains(&[FIXTURE], "## Raster");
    // Empty style/reflow render explicit none-found.
    contains(&[FIXTURE], "none found (0 events)");
    // CPU % headers say sampled time.
    contains(&[FIXTURE], "% of sampled time");
    // Event breakdown has self column.
    contains(&[FIXTURE], "Self % of Trace");
    // HTML renders trace A only warning path + sections.
    let o = ok(&[FIXTURE, "--html"]);
    assert!(o.stdout.contains("Trace health"));
    assert!(o.stdout.contains("Frame-blocking main-thread tasks"));
}

/// Temp dir with two trace copies under distinct stems.
fn two_traces(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("chperf-wave4-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("alpha.json");
    let b = dir.join("beta.json");
    let src = std::fs::read(FIXTURE).unwrap();
    std::fs::write(&a, &src).unwrap();
    std::fs::write(&b, &src).unwrap();
    (dir, a, b)
}

#[test]
fn smoke_glob_input() {
    let (dir, _, _) = two_traces("glob");
    let pat = format!("{}/*.json", dir.to_str().unwrap());
    // Glob without export flags lists the matches like a directory.
    let o = run(&[&pat, "--names"]);
    assert_eq!(o.status, 0, "glob list: {}{}", o.stdout, o.stderr);
    assert!(o.stderr.contains("2 trace(s)"), "stderr: {}", o.stderr);
    // Empty glob match errors (exit 1).
    let pat_none = format!("{}/zzz-no-match-*.json", dir.to_str().unwrap());
    exits_code(&[&pat_none, "--names"], 1);
    // Single-file glob match batch-exports.
    let out = dir.join("out");
    let o = run(&[&pat, "--json", "--out-dir", out.to_str().unwrap()]);
    assert_eq!(o.status, 0, "batch glob: {}{}", o.stdout, o.stderr);
    assert!(out.join("chperf-export-alpha.json").is_file());
    assert!(out.join("chperf-export-beta.json").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn smoke_batch_export_formats() {
    let (dir, _, _) = two_traces("batch");
    let out = dir.join("out");
    let o = run(&[
        dir.to_str().unwrap(),
        "--export",
        "--html",
        "--json",
        "--out-dir",
        out.to_str().unwrap(),
    ]);
    assert_eq!(o.status, 0, "batch: {}{}", o.stdout, o.stderr);
    for stem in ["alpha", "beta"] {
        let md = out.join(format!("chperf-export-{}.md", stem));
        let html = out.join(format!("chperf-export-{}.html", stem));
        let json = out.join(format!("chperf-export-{}.json", stem));
        assert!(md.is_file(), "missing {}", md.display());
        assert!(html.is_file(), "missing {}", html.display());
        assert!(json.is_file(), "missing {}", json.display());
        let h = std::fs::read_to_string(&html).unwrap();
        assert!(h.starts_with("<!DOCTYPE html>"), "bad html head");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert!(v.get("trace").is_some(), "full JSON missing trace");
    }
    // Default out location: next to each trace.
    let o = run(&[dir.to_str().unwrap(), "--export"]);
    assert_eq!(o.status, 0, "batch md: {}{}", o.stdout, o.stderr);
    assert!(dir.join("chperf-export-alpha.md").is_file());
    // Per-file failure keeps going: poison one trace, others still export.
    std::fs::write(dir.join("broken.json"), "{not json").unwrap();
    let out2 = dir.join("out2");
    let o = run(&[
        dir.to_str().unwrap(),
        "--export",
        "--out-dir",
        out2.to_str().unwrap(),
    ]);
    assert_eq!(
        o.status, 1,
        "batch with broken file: {}{}",
        o.stdout, o.stderr
    );
    assert!(o.stderr.contains("broken.json"), "stderr: {}", o.stderr);
    assert!(o.stderr.contains("1 failed"), "stderr: {}", o.stderr);
    assert!(out2.join("chperf-export-alpha.md").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn smoke_counters() {
    // Fixture has no Counter events: explicit none-found.
    contains(&[FIXTURE, "--counters"], "No counters found");
    let o = ok(&[FIXTURE, "--counters", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert_eq!(v["sections"]["counters"].as_array().unwrap().len(), 0);
    // Trace with Counter events: summary rows, name filter, JSON + CSV.
    let dir = std::env::temp_dir().join(format!("chperf-wave4-counters-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("c.json");
    let trace = serde_json::json!({"traceEvents": [
        {"name": "thread_name", "cat": "__metadata", "ph": "M", "ts": 0, "pid": 1, "tid": 1, "args": {"name": "CrRendererMain"}},
        {"name": "fpsCounter", "ph": "C", "ts": 1000000, "pid": 1, "tid": 1, "args": {"fps": 60, "dropped": 0}},
        {"name": "fpsCounter", "ph": "C", "ts": 2000000, "pid": 1, "tid": 1, "args": {"fps": 30, "dropped": 4}},
        {"name": "otherCounter", "ph": "C", "ts": 1500000, "pid": 1, "tid": 1, "args": {"x": 1}},
    ]});
    std::fs::write(&p, serde_json::to_string(&trace).unwrap()).unwrap();
    let ps = p.to_str().unwrap();
    contains(&[ps, "--counters"], "## Counters (3 counters");
    contains(&[ps, "--counters"], "| fps | 2 |");
    contains(&[ps, "--counters", "fps"], "## Counter: fps (2 samples");
    let o = ok(&[ps, "--counters", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert_eq!(v["sections"]["counters"].as_array().unwrap().len(), 3);
    let o = ok(&[ps, "--counters", "fps", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert_eq!(v["sections"]["counters"]["samples"], 2);
    assert_eq!(
        v["sections"]["counters"]["series"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(v["sections"]["counters"]["min"], 30.0);
    assert_eq!(v["sections"]["counters"]["max"], 60.0);
    contains(&[ps, "--counters", "--csv"], "# counters");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn smoke_html_compare_names() {
    let (_, a, b) = two_traces("htmlcmp");
    let (astr, bstr) = (a.to_str().unwrap(), b.to_str().unwrap());
    let o = ok(&[astr, "--compare", bstr, "--html"]);
    assert!(o.stdout.starts_with("<!DOCTYPE html>"), "bad html head");
    // Both trace names appear: title, A/B header, and both bodies.
    assert!(o.stdout.contains("alpha"), "missing trace A name");
    assert!(o.stdout.contains("beta"), "missing trace B name");
    assert!(o.stdout.contains("Overview: A vs B"), "missing A/B header");
    assert!(o.stdout.contains("Trace A:"), "missing Trace A body");
    assert!(o.stdout.contains("Trace B:"), "missing Trace B body");
    let _ = std::fs::remove_dir_all(a.parent().unwrap());
}

#[test]
fn smoke_memory_json_cap() {
    // 40 UpdateCounters samples: --memory --json caps at --top, notes the rest.
    let dir = std::env::temp_dir().join(format!("chperf-wave4-memcap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("m.json");
    let mut evs = vec![
        serde_json::json!({"name": "thread_name", "cat": "__metadata", "ph": "M", "ts": 0, "pid": 1, "tid": 1, "args": {"name": "CrRendererMain"}}),
        serde_json::json!({"name": "RunTask", "ph": "X", "ts": 1000000, "dur": 60000, "pid": 1, "tid": 1}),
    ];
    for i in 0..40 {
        evs.push(serde_json::json!({"name": "UpdateCounters", "ph": "I", "ts": 1000000 + i * 100000, "pid": 1, "tid": 1,
            "args": {"data": {"jsHeapSizeUsed": 10000000 + i * 100000, "nodes": 100 + i, "documents": 5, "jsEventListeners": 10}}}));
    }
    std::fs::write(
        &p,
        serde_json::to_string(&serde_json::json!({"traceEvents": evs})).unwrap(),
    )
    .unwrap();
    let ps = p.to_str().unwrap();
    let o = ok(&[ps, "--memory", "--top", "10", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    let mem = &v["sections"]["memory"];
    assert_eq!(mem["samples"].as_array().unwrap().len(), 10);
    assert_eq!(mem["truncated"], 30);
    assert_eq!(mem["summary"]["samples"], 40);
    // Markdown is unaffected by the JSON cap.
    contains(&[ps, "--memory"], "## Memory timeline");
    let _ = std::fs::remove_dir_all(&dir);
}
