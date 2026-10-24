//! End-to-end pipeline tests through the real JSON parser: CPU attribution,
//! time base, frames, GC, anchor, windowed-compare rows and adversarial
//! fuzzing. Moved out of the `chperf` binary target (`src/main.rs`) so the
//! binary stays test-free; everything here uses only the public
//! `chperf_core` API.

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

    // CPU: per-sample delta weights (5ms × 7 + 4+3+0+4 ms).
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

    // Windowed attribution by sample time: [lo, hi), so the sample
    // exactly at 1_030_000 belongs to the next window (5 × 5ms here).
    let scope = Scope {
        window: Some((1_000_000.0, 1_030_000.0)),
        tid: None,
        pid: None,
        cat: None,
    };
    let (_, times) = analysis::scan_profile_chunks(&events, Some(&scope), 0);
    let total: f64 = times.values().sum();
    assert_eq!(total, 25_000.0);

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
    let db = da.clone();
    assert_eq!(da.rows.len(), 13);
    for (ra, rb) in da.rows.iter().zip(db.rows.iter()) {
        assert_eq!(ra.metric, rb.metric);
        assert_eq!(rb.shoot - ra.shoot, 0.0, "{}", ra.metric);
        assert_eq!(rb.delta_pre() - ra.delta_pre(), 0.0, "{}", ra.metric);
    }
    // CPU samples are raw µs: 5 × 5ms samples with ts in [1_000_000, 1_030_000).
    let cpu = da.rows.iter().find(|r| r.metric == "CPU samples").unwrap();
    assert_eq!(cpu.shoot, 25_000.0);
    assert_eq!(cpu.pre, 0.0);
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

        let path =
            std::env::temp_dir().join(format!("chperf-fuzz-{}-{}.json", std::process::id(), iter));
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
