//! Multi-renderer (OOPIF) coverage: synthetic two-renderer trace asserting
//! the Renderers report section, `--renderers` JSON shape, enriched
//! `trace_health`, per-renderer jank, and the single-renderer one-liner.

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
    o
}

/// Two renderer pids (11 on pid 1 is busiest → primary) + browser main
/// (tid 31) + compositor lane (tid 12). Pid 2 owns a 60ms long task.
struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn multi_trace(tag: &str) -> (std::path::PathBuf, TempDir) {
    let dir = std::env::temp_dir().join(format!("chperf-renderers-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("multi.json");
    let mut evs = vec![
        serde_json::json!({"name": "thread_name", "cat": "__metadata", "ph": "M", "ts": 0, "pid": 1, "tid": 11, "args": {"name": "CrRendererMain"}}),
        serde_json::json!({"name": "thread_name", "cat": "__metadata", "ph": "M", "ts": 0, "pid": 2, "tid": 21, "args": {"name": "CrRendererMain"}}),
        serde_json::json!({"name": "thread_name", "cat": "__metadata", "ph": "M", "ts": 0, "pid": 3, "tid": 31, "args": {"name": "CrBrowserMain"}}),
        serde_json::json!({"name": "thread_name", "cat": "__metadata", "ph": "M", "ts": 0, "pid": 1, "tid": 12, "args": {"name": "VizCompositorThread"}}),
        serde_json::json!({"name": "RunTask", "ph": "X", "ts": 1000000, "dur": 10000, "pid": 3, "tid": 31}),
    ];
    for i in 0..4 {
        evs.push(serde_json::json!({"name": "RunTask", "ph": "X", "ts": 1000000 + i * 30000, "dur": 20000, "pid": 1, "tid": 11}));
    }
    for i in 0..2 {
        evs.push(serde_json::json!({"name": "RunTask", "ph": "X", "ts": 1000000 + i * 30000, "dur": 20000, "pid": 2, "tid": 21}));
    }
    evs.push(serde_json::json!({"name": "RunTask", "ph": "X", "ts": 1200000, "dur": 60000, "pid": 2, "tid": 21}));
    std::fs::write(
        &p,
        serde_json::to_string(&serde_json::json!({"traceEvents": evs})).unwrap(),
    )
    .unwrap();
    (p, TempDir(dir))
}

#[test]
fn renderers_inspect_section_rows_and_json() {
    let (p, _guard) = multi_trace("inspect");
    let ps = p.to_str().unwrap();
    let o = ok(&[ps, "--renderers"]);
    assert!(
        o.stdout.contains("## Renderers (2 renderers"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("Primary renderer"), "{}", o.stdout);

    let o = ok(&[ps, "--renderers", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    let rows = v["sections"]["renderers"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["pid"], 1);
    assert_eq!(rows[0]["tid"], 11);
    assert_eq!(rows[0]["name"], "CrRendererMain");
    assert_eq!(rows[0]["is_primary"], true);
    assert_eq!(rows[1]["pid"], 2);
    assert_eq!(rows[1]["is_primary"], false);
    assert_eq!(rows[1]["long_task_count"], 1);
    assert_eq!(rows[1]["runtask_count"], 3);

    let o = ok(&[ps, "--renderers", "--csv"]);
    assert!(o.stdout.contains("# renderers"), "{}", o.stdout);
}

#[test]
fn full_report_renderers_and_trace_health() {
    let (p, _guard) = multi_trace("full");
    let ps = p.to_str().unwrap();
    let o = ok(&[ps]);
    assert!(o.stdout.contains("## Renderers"), "{}", o.stdout);
    assert!(o.stdout.contains("Primary renderer"), "{}", o.stdout);
    assert!(o.stdout.contains("Browser main"), "{}", o.stdout);

    let o = ok(&[ps, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    let health = &v["trace_health"];
    assert_eq!(health["renderers"].as_array().unwrap().len(), 2);
    assert_eq!(health["renderers"][0]["pid"], 1);
    assert_eq!(health["browser_main_tid"], 31);
    assert_eq!(health["compositor_tids"], serde_json::json!([12]));
    let renderers = v["renderers"].as_array().unwrap();
    assert_eq!(renderers.len(), 2);
    assert_eq!(renderers[0]["is_primary"], true);
    assert!(renderers[1]["dropped_frames"].is_number());
    assert!(renderers[1]["jank_clusters"].is_number());

    let o = ok(&[ps, "--html"]);
    assert!(o.stdout.contains("Renderers"), "missing html renderers");
    assert!(!o.stdout.contains("single-process trace"));

    let o = ok(&[ps, "--coverage", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert_eq!(
        v["sections"]["coverage"]["renderers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn multi_renderer_jank_has_secondary_block() {
    let (p, _guard) = multi_trace("jank");
    let o = ok(&[p.to_str().unwrap(), "--jank"]);
    assert!(o.stdout.contains("Secondary renderers"), "{}", o.stdout);
}

#[test]
fn single_renderer_paths() {
    // Tiny fixture: one renderer → one-liner, no table, no secondary jank.
    let o = ok(&[FIXTURE]);
    assert!(o.stdout.contains("single-process trace"), "{}", o.stdout);
    assert!(!o.stdout.contains("Primary renderer"), "{}", o.stdout);

    let o = ok(&[FIXTURE, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert_eq!(v["renderers"].as_array().unwrap().len(), 1);

    let o = ok(&[FIXTURE, "--renderers", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&o.stdout).expect("valid JSON");
    assert_eq!(v["sections"]["renderers"].as_array().unwrap().len(), 1);

    let o = ok(&[FIXTURE, "--jank"]);
    assert!(!o.stdout.contains("Secondary"), "{}", o.stdout);

    let o = ok(&[FIXTURE, "--html"]);
    assert!(
        o.stdout.contains("single-process trace"),
        "missing html one-liner"
    );
}
