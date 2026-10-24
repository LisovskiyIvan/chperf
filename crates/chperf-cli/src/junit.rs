//! JUnit XML reports for CI: a `<testsuite>` document written to a file
//! (`--junit=FILE`) or stdout (bare `--junit`).
//!
//! Two modes, matching the two report paths:
//!
//! - Compare mode (`--compare`, full report): one testcase per compare
//!   finding — `Regressed` findings become `<failure>`, Improved/Neutral
//!   pass. Testcase name is `{category}: {message}`.
//! - Single-trace mode: one testcase per metric gate (see [`single_gates`]
//!   for the thresholds). Failures reflect real thresholds, documented in
//!   the README.
//!
//! Report-only: writing JUnit never changes the exit code (exit 3 still
//! comes only from `--fail-on-regressed`).

use chperf_core::analysis::{Finding, FindingSeverity};

/// `(testcase name, failed?, failure message, failure body)`.
pub(crate) struct Gate {
    pub(crate) name: &'static str,
    pub(crate) failed: bool,
    pub(crate) message: String,
    pub(crate) detail: String,
}

/// Single-trace metric gates. Thresholds (also in the README):
/// - `long-tasks`: fails when any main-thread RunTask exceeds 50ms.
/// - `dropped-frames`: fails when any `DroppedFrame` was recorded.
/// - `total-blocking-time`: fails when TBT (sum of long-task time past
///   50ms) exceeds 500ms.
/// - `frame-blocking-tasks`: fails when any frame-blocking main-thread
///   task was attributed (scroll or heuristic fallback).
pub(crate) fn single_gates(
    long_tasks: usize,
    dropped: usize,
    tbt_us: f64,
    frame_blocking: usize,
) -> Vec<Gate> {
    vec![
        Gate {
            name: "long-tasks",
            failed: long_tasks > 0,
            message: format!("{} long tasks (>50ms) on the main thread", long_tasks),
            detail: format!("long_task_count={} (threshold: 0)", long_tasks),
        },
        Gate {
            name: "dropped-frames",
            failed: dropped > 0,
            message: format!("{} dropped frames", dropped),
            detail: format!("dropped_frames={} (threshold: 0)", dropped),
        },
        Gate {
            name: "total-blocking-time",
            failed: tbt_us > 500_000.0,
            message: format!("total blocking time {:.1}ms exceeds 500ms", tbt_us / 1000.0),
            detail: format!("total_blocking_time_us={:.0} (threshold: 500000)", tbt_us),
        },
        Gate {
            name: "frame-blocking-tasks",
            failed: frame_blocking > 0,
            message: format!("{} frame-blocking main-thread tasks", frame_blocking),
            detail: format!("frame_blocking_tasks={} (threshold: 0)", frame_blocking),
        },
    ]
}

/// Escape `<`, `>`, `&`, `"`, `'` for XML attributes and text.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// One JUnit testcase: (classname, name, optional (failure message, body)).
type Case = (String, String, Option<(String, String)>);

fn doc(suite: &str, cases: &[Case]) -> String {
    let failures = cases.iter().filter(|(_, _, f)| f.is_some()).count();
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<testsuite name=\"{}\" tests=\"{}\" failures=\"{}\">\n",
        esc(suite),
        cases.len(),
        failures,
    ));
    for (classname, name, failure) in cases {
        match failure {
            Some((msg, body)) => {
                out.push_str(&format!(
                    "  <testcase classname=\"{}\" name=\"{}\">\n    <failure message=\"{}\">{}</failure>\n  </testcase>\n",
                    esc(classname),
                    esc(name),
                    esc(msg),
                    esc(body),
                ));
            }
            None => {
                out.push_str(&format!(
                    "  <testcase classname=\"{}\" name=\"{}\"/>\n",
                    esc(classname),
                    esc(name),
                ));
            }
        }
    }
    out.push_str("</testsuite>\n");
    out
}

/// Compare-mode document: one testcase per finding.
pub(crate) fn junit_compare_xml(findings: &[Finding]) -> String {
    let cases: Vec<Case> = findings
        .iter()
        .map(|f| {
            let name = format!("{}: {}", f.category, f.message);
            let failure = if f.severity == FindingSeverity::Regressed {
                Some((f.message.clone(), f.detail.clone()))
            } else {
                None
            };
            ("chperf.compare".to_string(), name, failure)
        })
        .collect();
    doc("chperf-compare", &cases)
}

/// Single-trace document: one testcase per metric gate.
pub(crate) fn junit_single_xml(gates: &[Gate]) -> String {
    let cases: Vec<Case> = gates
        .iter()
        .map(|g| {
            let failure = if g.failed {
                Some((g.message.clone(), g.detail.clone()))
            } else {
                None
            };
            ("chperf.gates".to_string(), g.name.to_string(), failure)
        })
        .collect();
    doc("chperf", &cases)
}

/// Write the XML document: `-` (bare `--junit`) prints to stdout, anything
/// else writes to that file path.
pub(crate) fn write_junit(target: &str, xml: &str) -> Result<(), Box<dyn std::error::Error>> {
    if target == "-" {
        print!("{}", xml);
    } else {
        std::fs::write(target, xml).map_err(|e| format!("{}: {}", target, e))?;
        eprintln!("Exported JUnit to {}", target);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chperf_core::analysis::{Finding, FindingSeverity};

    fn finding(severity: FindingSeverity, category: &str, message: &str) -> Finding {
        Finding {
            severity,
            category: category.to_string(),
            message: message.to_string(),
            detail: "detail".to_string(),
        }
    }

    #[test]
    fn regressed_fails_others_pass() {
        let xml = junit_compare_xml(&[
            finding(FindingSeverity::Regressed, "Long Tasks", "1 -> 2"),
            finding(FindingSeverity::Improved, "Paint", "5ms -> 3ms"),
            finding(FindingSeverity::Neutral, "GC", "same"),
        ]);
        assert!(xml.contains("<testsuite"), "{}", xml);
        assert!(xml.contains("tests=\"3\""), "{}", xml);
        assert!(xml.contains("failures=\"1\""), "{}", xml);
        assert!(xml.contains("Long Tasks: 1 -&gt; 2"), "{}", xml);
        assert_eq!(xml.matches("<failure").count(), 1);
    }

    #[test]
    fn messages_escape_xml() {
        let xml = junit_compare_xml(&[finding(
            FindingSeverity::Regressed,
            "A&B",
            "<tag> \"q\" 's'",
        )]);
        assert!(xml.contains("A&amp;B"), "{}", xml);
        assert!(
            xml.contains("&lt;tag&gt; &quot;q&quot; &apos;s&apos;"),
            "{}",
            xml
        );
        assert!(!xml.contains("<tag>"), "{}", xml);
    }

    #[test]
    fn gates_fail_on_thresholds() {
        let xml = junit_single_xml(&single_gates(0, 0, 100_000.0, 0));
        assert!(xml.contains("tests=\"4\""), "{}", xml);
        assert!(xml.contains("failures=\"0\""), "{}", xml);
        assert!(!xml.contains("<failure"), "{}", xml);
        let xml = junit_single_xml(&single_gates(2, 1, 600_000.0, 3));
        assert!(xml.contains("failures=\"4\""), "{}", xml);
        for gate in [
            "long-tasks",
            "dropped-frames",
            "total-blocking-time",
            "frame-blocking-tasks",
        ] {
            assert!(xml.contains(gate), "{}", xml);
        }
    }

    #[test]
    fn tbt_boundary_is_exclusive() {
        let ok = junit_single_xml(&single_gates(1, 1, 500_000.0, 1));
        assert!(ok.contains("failures=\"3\""), "{}", ok); // TBT exactly 500ms passes
        let bad = junit_single_xml(&single_gates(1, 1, 500_001.0, 1));
        assert!(bad.contains("failures=\"4\""), "{}", bad);
    }
}
