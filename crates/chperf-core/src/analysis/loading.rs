//! Loading milestone extraction: navigation/paint milestones, LCP candidates,
//! user-timing marks/measures and official responsiveness signals.
//!
//! All timestamps are absolute trace microseconds in `ts_us`; `rel_us` fields
//! are relative to `navigationStart` when the trace contains one, `None`
//! otherwise. Missing milestones are `None`/empty, never `0.0`.

use crate::trace::TraceEvent;
use rustc_hash::FxHashMap;

#[derive(Clone)]
pub struct LoadingEntry {
    pub name: String,
    /// Absolute trace timestamp, `None` when the milestone is absent.
    pub ts_us: Option<f64>,
    /// Timestamp relative to `navigationStart`, `None` when absent or when
    /// the trace has no `navigationStart`.
    pub rel_us: Option<f64>,
    pub dur_us: Option<f64>,
}

#[derive(Clone)]
pub struct LcpCandidate {
    pub ts_us: f64,
    pub rel_us: Option<f64>,
    /// `args.data.size` (element area), when present.
    pub size: Option<f64>,
    /// `args.data.nodeName`, else `nodeId`/`domNodeId`/`id` stringified.
    pub node: Option<String>,
    /// `args.data.url`, when present.
    pub url: Option<String>,
}

#[derive(Clone)]
pub struct UserTimingEntry {
    pub name: String,
    pub ts_us: f64,
    pub dur_us: Option<f64>,
}

#[derive(Clone)]
pub struct LoadingResult {
    pub navigation_start_us: Option<f64>,
    /// One entry per known milestone in canonical order; absent milestones
    /// carry `None` timestamps.
    pub entries: Vec<LoadingEntry>,
    /// Relative to `navigationStart` when present, else absolute.
    pub first_contentful_paint_us: Option<f64>,
    /// Last non-invalidated LCP candidate; relative when `navigationStart`
    /// is present, else absolute.
    pub largest_contentful_paint_us: Option<f64>,
    /// Valid (non-invalidated) LCP candidates, oldest first.
    pub lcp: Vec<LcpCandidate>,
    pub user_timing: Vec<UserTimingEntry>,
}

/// Official responsiveness signal: `EventTiming`, `InteractionToNextPaint`,
/// `responsiveness::UserInteraction`, `PerformanceTracker::LongTask`,
/// `longtask`.
#[derive(Clone)]
pub struct LatencyEntry {
    pub name: String,
    pub ts_us: f64,
    /// `e.dur`, else `args.data.duration`/`args.detail.duration`, else `None`.
    pub dur_us: Option<f64>,
    /// `args.data.type` (e.g. `"keydown"`), `""` when absent.
    pub interaction_type: String,
}

/// Milestones reported in `entries`, in canonical order. Any phase matches
/// (`navigationStart` shows up as `I`/`R`/mark-like events depending on the
/// exporter).
const MILESTONES: &[&str] = &[
    "navigationStart",
    "firstPaint",
    "firstContentfulPaint",
    "firstImagePaint",
    "firstMeaningfulPaint",
    "MarkLoad",
    "MarkDOMContent",
    "domContentLoadedEventEnd",
    "loadEvent",
];

fn is_milestone(name: &str) -> bool {
    MILESTONES.contains(&name)
}

fn is_lcp_candidate(name: &str) -> bool {
    name == "LargestContentfulPaint::Candidate" || name == "LargestTextPaint::Candidate"
}

fn is_user_timing(e: &TraceEvent) -> bool {
    e.cat.as_deref() == Some("blink.user_timing")
        || e.name == "PerformanceMark"
        || e.name == "PerformanceMeasure"
}

fn num(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn str_of(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn user_timing_name(e: &TraceEvent, args: &Option<serde_json::Value>) -> String {
    args.as_ref()
        .and_then(|a| a.get("data"))
        .and_then(|d| d.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| e.name.to_string())
}

fn user_timing_dur(e: &TraceEvent, args: &Option<serde_json::Value>) -> Option<f64> {
    if let Some(d) = e.dur {
        return Some(d);
    }
    let a = args.as_ref()?;
    a.get("data")
        .and_then(|d| d.get("duration"))
        .and_then(num)
        .or_else(|| {
            a.get("detail")
                .and_then(|d| d.get("duration"))
                .and_then(num)
        })
        .or_else(|| {
            a.get("data")
                .and_then(|d| d.get("detail"))
                .and_then(|d| d.get("duration"))
                .and_then(num)
        })
}

pub fn analyze_loading(events: &[&TraceEvent]) -> LoadingResult {
    // Earliest (ts, dur) per milestone name; single pass, O(n).
    let mut first: FxHashMap<&str, (f64, Option<f64>)> = FxHashMap::default();
    let mut raw_candidates: Vec<LcpCandidate> = Vec::new();
    let mut invalidated_before = f64::NEG_INFINITY;
    let mut has_invalidate = false;
    let mut user_timing: Vec<UserTimingEntry> = Vec::new();

    for e in events {
        if is_milestone(&e.name) {
            match first.get(e.name.as_str()) {
                Some(&(ts, _)) if ts <= e.ts => {}
                _ => {
                    first.insert(e.name.as_str(), (e.ts, e.dur));
                }
            }
            continue;
        }
        if is_lcp_candidate(&e.name) {
            let args = e.args_value();
            let data = args.as_ref().and_then(|a| a.get("data"));
            let size = data.and_then(|d| d.get("size")).and_then(num);
            let node = data
                .and_then(|d| {
                    d.get("nodeName")
                        .or_else(|| d.get("nodeId"))
                        .or_else(|| d.get("domNodeId"))
                        .or_else(|| d.get("id"))
                })
                .and_then(str_of);
            let url = data
                .and_then(|d| d.get("url"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            raw_candidates.push(LcpCandidate {
                ts_us: e.ts,
                rel_us: None, // filled after navigationStart is known
                size,
                node,
                url,
            });
            continue;
        }
        if e.name == "largestContentfulPaint::Invalidate" {
            has_invalidate = true;
            if e.ts > invalidated_before {
                invalidated_before = e.ts;
            }
            continue;
        }
        if is_user_timing(e) {
            let args = e.args_value();
            user_timing.push(UserTimingEntry {
                name: user_timing_name(e, &args),
                ts_us: e.ts,
                dur_us: user_timing_dur(e, &args),
            });
        }
    }

    let navigation_start_us = first.get("navigationStart").map(|&(ts, _)| ts);
    let rel = |ts: f64| navigation_start_us.map(|n| ts - n);

    let entries: Vec<LoadingEntry> = MILESTONES
        .iter()
        .map(|m| match first.get(m) {
            Some(&(ts, dur)) => LoadingEntry {
                name: m.to_string(),
                ts_us: Some(ts),
                rel_us: rel(ts),
                dur_us: dur,
            },
            None => LoadingEntry {
                name: m.to_string(),
                ts_us: None,
                rel_us: None,
                dur_us: None,
            },
        })
        .collect();

    let first_contentful_paint_us = first
        .get("firstContentfulPaint")
        .map(|&(ts, _)| rel(ts).unwrap_or(ts));

    // A candidate is invalidated when an invalidate lands at or after it.
    let mut lcp: Vec<LcpCandidate> = raw_candidates
        .into_iter()
        .filter(|c| !has_invalidate || c.ts_us > invalidated_before)
        .collect();
    lcp.sort_by(|a, b| a.ts_us.partial_cmp(&b.ts_us).unwrap());
    for c in &mut lcp {
        c.rel_us = rel(c.ts_us);
    }
    let largest_contentful_paint_us = lcp.last().map(|c| c.rel_us.unwrap_or(c.ts_us));

    user_timing.sort_by(|a, b| a.ts_us.partial_cmp(&b.ts_us).unwrap());

    LoadingResult {
        navigation_start_us,
        entries,
        first_contentful_paint_us,
        largest_contentful_paint_us,
        lcp,
        user_timing,
    }
}

/// Official responsiveness/long-task signals (`EventTiming`,
/// `InteractionToNextPaint`, `responsiveness::UserInteraction`,
/// `PerformanceTracker::LongTask`, `longtask`), main thread only, sorted by
/// timestamp. Empty when the trace lacks them — that is the expected case
/// for most traces; nothing is synthesized.
pub fn analyze_long_tasks_official(events: &[&TraceEvent], main_tid: u64) -> Vec<LatencyEntry> {
    let mut out: Vec<LatencyEntry> = Vec::new();
    for e in events {
        let known = matches!(
            e.name.as_str(),
            "EventTiming"
                | "InteractionToNextPaint"
                | "responsiveness::UserInteraction"
                | "PerformanceTracker::LongTask"
                | "longtask"
        );
        if !known || e.tid != main_tid {
            continue;
        }
        let args = e.args_value();
        let interaction_type = args
            .as_ref()
            .and_then(|a| a.get("data"))
            .and_then(|d| d.get("type").or_else(|| d.get("interactionType")))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let dur_us = e.dur.or_else(|| {
            args.as_ref()
                .and_then(|a| a.get("data"))
                .and_then(|d| d.get("duration"))
                .and_then(num)
        });
        out.push(LatencyEntry {
            name: e.name.to_string(),
            ts_us: e.ts,
            dur_us,
            interaction_type,
        });
    }
    out.sort_by(|a, b| a.ts_us.partial_cmp(&b.ts_us).unwrap());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::TraceEvent;

    fn ev(name: &str, ph: u8, ts: f64, dur: Option<f64>) -> TraceEvent {
        TraceEvent {
            name: name.to_owned(),
            id: 0,
            has_id: false,
            ph,
            ts,
            dur,
            tid: 1,
            pid: 1,
            cat: None,
            args: None,
        }
    }

    fn ev_args(
        name: &str,
        ph: u8,
        ts: f64,
        dur: Option<f64>,
        args: serde_json::Value,
    ) -> TraceEvent {
        TraceEvent {
            name: name.to_owned(),
            id: 0,
            has_id: false,
            ph,
            ts,
            dur,
            tid: 1,
            pid: 1,
            cat: None,
            args: crate::trace::test_args(args),
        }
    }

    fn ev_cat(
        name: &str,
        ph: u8,
        ts: f64,
        dur: Option<f64>,
        cat: &str,
        args: serde_json::Value,
    ) -> TraceEvent {
        TraceEvent {
            name: name.to_owned(),
            id: 0,
            has_id: false,
            ph,
            ts,
            dur,
            tid: 1,
            pid: 1,
            cat: Some(cat.to_owned()),
            args: crate::trace::test_args(args),
        }
    }

    #[test]
    fn happy_path_milestones_lcp_and_user_timing() {
        let events = [
            ev("navigationStart", b'I', 1_000_000.0, None),
            ev("firstPaint", b'I', 1_200_000.0, None),
            ev("firstContentfulPaint", b'I', 1_300_000.0, None),
            ev("firstMeaningfulPaint", b'I', 1_500_000.0, None),
            ev("loadEvent", b'X', 2_000_000.0, Some(50_000.0)),
            ev_args(
                "LargestContentfulPaint::Candidate",
                b'I',
                1_400_000.0,
                None,
                serde_json::json!({"data": {"size": 100.0, "nodeName": "IMG", "url": "https://x/a.png"}}),
            ),
            ev_args(
                "LargestTextPaint::Candidate",
                b'I',
                1_450_000.0,
                None,
                serde_json::json!({"data": {"size": 200.0, "nodeId": 42}}),
            ),
            ev_cat(
                "Measure",
                b'X',
                1_100_000.0,
                Some(25_000.0),
                "blink.user_timing",
                serde_json::json!({"data": {"name": "app-boot"}}),
            ),
            ev_args(
                "PerformanceMark",
                b'I',
                1_150_000.0,
                None,
                serde_json::json!({"data": {"name": "mark-1"}}),
            ),
        ];
        let r = analyze_loading(&events.iter().collect::<Vec<_>>());
        assert_eq!(r.navigation_start_us, Some(1_000_000.0));
        assert_eq!(r.first_contentful_paint_us, Some(300_000.0));
        assert_eq!(r.largest_contentful_paint_us, Some(450_000.0));
        assert_eq!(r.lcp.len(), 2);
        assert_eq!(r.lcp[0].node.as_deref(), Some("IMG"));
        assert_eq!(r.lcp[1].node.as_deref(), Some("42"));
        assert_eq!(r.user_timing.len(), 2);
        assert_eq!(r.user_timing[0].name, "app-boot");
        assert_eq!(r.user_timing[0].dur_us, Some(25_000.0));
        assert_eq!(r.user_timing[1].name, "mark-1");
        assert_eq!(r.user_timing[1].dur_us, None);
        // Absent milestones are None, never 0.
        let img = r
            .entries
            .iter()
            .find(|e| e.name == "firstImagePaint")
            .unwrap();
        assert_eq!(img.ts_us, None);
        let load = r.entries.iter().find(|e| e.name == "loadEvent").unwrap();
        assert_eq!(load.rel_us, Some(1_000_000.0));
        assert_eq!(load.dur_us, Some(50_000.0));
    }

    #[test]
    fn missing_events_yield_none_and_empty() {
        let r = analyze_loading(&[]);
        assert_eq!(r.navigation_start_us, None);
        assert_eq!(r.first_contentful_paint_us, None);
        assert_eq!(r.largest_contentful_paint_us, None);
        assert!(r.lcp.is_empty());
        assert!(r.user_timing.is_empty());
        assert_eq!(r.entries.len(), MILESTONES.len());
        assert!(
            r.entries
                .iter()
                .all(|e| e.ts_us.is_none() && e.rel_us.is_none())
        );
        // No navigationStart: absolute times, no rel.
        let r = analyze_loading(&[&ev("firstContentfulPaint", b'R', 500_000.0, None)]);
        assert_eq!(r.navigation_start_us, None);
        assert_eq!(r.first_contentful_paint_us, Some(500_000.0));
        let fcp = r
            .entries
            .iter()
            .find(|e| e.name == "firstContentfulPaint")
            .unwrap();
        assert_eq!(fcp.ts_us, Some(500_000.0));
        assert_eq!(fcp.rel_us, None);
    }

    #[test]
    fn invalidate_drops_earlier_lcp_candidates() {
        let cand = |ts: f64| {
            ev_args(
                "LargestContentfulPaint::Candidate",
                b'I',
                ts,
                None,
                serde_json::json!({"data": {"size": 10.0}}),
            )
        };
        let events = [
            ev("navigationStart", b'R', 0.0, None),
            cand(100_000.0),
            cand(200_000.0),
            ev("largestContentfulPaint::Invalidate", b'I', 250_000.0, None),
            cand(300_000.0),
        ];
        let r = analyze_loading(&events.iter().collect::<Vec<_>>());
        assert_eq!(r.lcp.len(), 1);
        assert_eq!(r.lcp[0].ts_us, 300_000.0);
        assert_eq!(r.largest_contentful_paint_us, Some(300_000.0));
        // Everything invalidated -> empty, never 0.
        let events = [
            ev("navigationStart", b'R', 0.0, None),
            cand(100_000.0),
            ev("largestContentfulPaint::Invalidate", b'I', 500_000.0, None),
        ];
        let r = analyze_loading(&events.iter().collect::<Vec<_>>());
        assert!(r.lcp.is_empty());
        assert_eq!(r.largest_contentful_paint_us, None);
    }

    #[test]
    fn official_latency_signals_and_empty_trace() {
        assert!(analyze_long_tasks_official(&[], 1).is_empty());
        // Non-matching names and other threads are ignored.
        assert!(
            analyze_long_tasks_official(&[&ev("RunTask", b'X', 10.0, Some(60_000.0))], 1)
                .is_empty()
        );
        let mut other_thread = ev_args(
            "EventTiming",
            b'X',
            20.0,
            Some(100.0),
            serde_json::json!({"data": {"type": "keydown", "duration": 100.0}}),
        );
        other_thread.tid = 9;
        let events = [
            other_thread,
            ev_args(
                "EventTiming",
                b'X',
                30.0,
                Some(120.0),
                serde_json::json!({"data": {"type": "pointerdown"}}),
            ),
            ev("InteractionToNextPaint", b'X', 40.0, Some(200.0)),
            ev("PerformanceTracker::LongTask", b'X', 50.0, Some(75_000.0)),
            ev("longtask", b'X', 60.0, Some(60_000.0)),
        ];
        let r = analyze_long_tasks_official(&events.iter().collect::<Vec<_>>(), 1);
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].name, "EventTiming");
        assert_eq!(r[0].interaction_type, "pointerdown");
        assert_eq!(r[0].dur_us, Some(120.0));
        assert_eq!(r[1].name, "InteractionToNextPaint");
        assert!(r.windows(2).all(|w| w[0].ts_us <= w[1].ts_us));
    }
}
