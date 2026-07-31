//! Granular trace inspection: filter events by name/time-window, aggregate CPU
//! samples by function (optionally within a window), and search event args.
//!
//! Output is Markdown so it can be piped to an LLM, mirroring `--export`.

use crate::trace::TraceEvent;
use serde_json::Value;
use std::collections::HashMap;

/// Absolute trace start (min `ts` across events), in microseconds.
pub fn trace_start_us(events: &[TraceEvent]) -> f64 {
    events.iter().map(|e| e.ts).fold(f64::INFINITY, f64::min)
}

/// An absolute `[lo, hi]` time window in microseconds, derived from
/// `--around <ms_from_start>` and `--window <half_ms>`.
pub fn window_us(
    around_ms: Option<f64>,
    window_ms: Option<f64>,
    min_ts: f64,
) -> Option<(f64, f64)> {
    let around = around_ms?;
    let half_ms = window_ms.unwrap_or(100.0); // default ±100ms
    let center = min_ts + around * 1000.0;
    let half = half_ms * 1000.0;
    Some((center - half, center + half))
}

fn in_window(ts: f64, window: Option<(f64, f64)>) -> bool {
    window.is_none_or(|(lo, hi)| ts >= lo && ts <= hi)
}

fn fmt_ms(us: f64) -> String {
    format!("{:.2}", us / 1000.0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

fn args_compact(args: &Option<Value>) -> String {
    match args {
        Some(v) => {
            let s = serde_json::to_string(v).unwrap_or_default();
            truncate(&s, 160)
        }
        None => String::new(),
    }
}

fn window_label(window: Option<(f64, f64)>) -> &'static str {
    match window {
        Some(_) => "windowed",
        None => "full trace",
    }
}

/// List events matching one of `names`, filtered by window and min duration.
pub fn events_md(
    events: &[TraceEvent],
    names: &[String],
    window: Option<(f64, f64)>,
    min_dur_us: f64,
    top: usize,
    min_ts: f64,
) -> String {
    let mut rows: Vec<&TraceEvent> = events
        .iter()
        .filter(|e| names.iter().any(|n| n == &e.name))
        .filter(|e| e.dur.unwrap_or(0.0) >= min_dur_us)
        .filter(|e| in_window(e.ts, window))
        .collect();
    rows.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap());

    let total = rows.len();
    let mut out = String::new();
    out.push_str(&format!(
        "## Events: {} ({} matches, {})\n\n",
        names.join(" | "),
        total,
        window_label(window),
    ));

    if let Some((lo, hi)) = window {
        out.push_str(&format!(
            "- **Window**: {:.2}ms … {:.2}ms from trace start\n\n",
            (lo - min_ts) / 1000.0,
            (hi - min_ts) / 1000.0,
        ));
    }

    if total == 0 {
        out.push_str("No matching events.\n\n");
        return out;
    }

    out.push_str("| # | t(ms) | dur(ms) | name | tid | args |\n");
    out.push_str("|---|-------|---------|------|-----|------|\n");
    for (i, e) in rows.iter().take(top).enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            i + 1,
            fmt_ms(e.ts - min_ts),
            fmt_ms(e.dur.unwrap_or(0.0)),
            e.name,
            e.tid,
            args_compact(&e.args),
        ));
    }
    if total > top {
        out.push_str(&format!("\n_Showing {} of {} matches (use --top to see more)._\n", top, total));
    }
    out.push('\n');
    out
}

/// Aggregate CPU profile self-time for functions whose name contains `pattern`,
/// optionally restricted to a time window (chunk-granularity).
pub fn functions_md(
    events: &[TraceEvent],
    pattern: &str,
    window: Option<(f64, f64)>,
    top: usize,
    min_ts: f64,
) -> String {
    let pat = pattern.to_lowercase();
    let mut node_map: HashMap<u64, (String, String)> = HashMap::new();
    let mut self_times: HashMap<u64, f64> = HashMap::new();
    let mut total_in_scope = 0.0f64;

    for e in events {
        if e.name != "ProfileChunk" {
            continue;
        }
        let args = match &e.args {
            Some(a) => a,
            None => continue,
        };
        let data = match args.get("data") {
            Some(d) => d,
            None => continue,
        };
        let cpu_profile = match data.get("cpuProfile") {
            Some(cp) => cp,
            None => continue,
        };

        // Register nodes from every chunk (definitions precede their samples).
        if let Some(nodes) = cpu_profile.get("nodes").and_then(|n| n.as_array()) {
            for node in nodes {
                let id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let call_frame = node.get("callFrame");
                let func_name = call_frame
                    .and_then(|cf| cf.get("functionName"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("(anonymous)")
                    .to_string();
                let url = call_frame
                    .and_then(|cf| cf.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                node_map.entry(id).or_insert((func_name, url));
            }
        }

        // Accumulate sample time only from in-window chunks.
        if !in_window(e.ts, window) {
            continue;
        }
        let samples = cpu_profile.get("samples").and_then(|s| s.as_array());
        let time_deltas = data.get("timeDeltas").and_then(|t| t.as_array());
        if let (Some(samples), Some(deltas)) = (samples, time_deltas) {
            for (sample, delta) in samples.iter().zip(deltas.iter()) {
                let node_id = sample.as_u64().unwrap_or(0);
                let dt = delta.as_f64().unwrap_or(0.0);
                *self_times.entry(node_id).or_default() += dt;
                total_in_scope += dt;
            }
        }
    }

    let mut funcs: Vec<(&str, &str, f64)> = self_times
        .iter()
        .filter_map(|(id, t)| node_map.get(id).map(|(n, u)| (n.as_str(), u.as_str(), *t)))
        .filter(|(n, _, _)| n.to_lowercase().contains(&pat))
        .collect();
    funcs.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

    let matched_total: f64 = funcs.iter().map(|(_, _, t)| *t).sum();

    let mut out = String::new();
    out.push_str(&format!(
        "## CPU Functions matching `{}` ({} functions, {})\n\n",
        pattern,
        funcs.len(),
        window_label(window),
    ));
    if let Some((lo, hi)) = window {
        out.push_str(&format!(
            "- **Window**: {:.2}ms … {:.2}ms from trace start\n",
            (lo - min_ts) / 1000.0,
            (hi - min_ts) / 1000.0,
        ));
    }
    out.push_str(&format!(
        "- **Matched self-time**: {}\n\n",
        fmt_ms(matched_total),
    ));

    if funcs.is_empty() {
        out.push_str("No matching functions.\n\n");
        return out;
    }

    out.push_str("| # | function | self | % of window | file |\n");
    out.push_str("|---|----------|------|-------------|------|\n");
    for (i, (name, url, t)) in funcs.iter().take(top).enumerate() {
        let pct = if total_in_scope > 0.0 {
            t / total_in_scope * 100.0
        } else {
            0.0
        };
        let short_url = url.rfind('/').map(|i| &url[i + 1..]).unwrap_or(url);
        let label = if name.is_empty() { "(anonymous)" } else { *name };
        out.push_str(&format!(
            "| {} | {} | {}ms | {:.1}% | {} |\n",
            i + 1,
            label,
            fmt_ms(*t),
            pct,
            short_url,
        ));
    }
    out.push('\n');
    out
}

/// Search event `args` (JSON) for `needle` (case-insensitive) and list matches.
pub fn find_md(
    events: &[TraceEvent],
    needle: &str,
    top: usize,
    min_ts: f64,
) -> String {
    let nl = needle.to_lowercase();
    let mut matches: Vec<(&TraceEvent, String)> = Vec::new();

    for e in events {
        let s = match &e.args {
            Some(v) => serde_json::to_string(v).unwrap_or_default(),
            None => continue,
        };
        if let Some(idx) = s.to_lowercase().find(&nl) {
            // Snippet: ~60 chars around the match.
            let chars: Vec<char> = s.chars().collect();
            let start = idx.saturating_sub(60);
            let end = (idx + nl.len() + 60).min(chars.len());
            let snippet: String = chars[start..end].iter().collect();
            matches.push((e, snippet));
        }
    }

    matches.sort_by(|a, b| a.0.ts.partial_cmp(&b.0.ts).unwrap());
    let total = matches.len();

    let mut out = String::new();
    out.push_str(&format!(
        "## Find `{}` in event args ({} matches)\n\n",
        needle, total,
    ));

    if total == 0 {
        out.push_str("No matches.\n\n");
        return out;
    }

    out.push_str("| # | t(ms) | dur(ms) | name | tid | match |\n");
    out.push_str("|---|-------|---------|------|-----|-------|\n");
    for (i, (e, snippet)) in matches.iter().take(top).enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | …{}… |\n",
            i + 1,
            fmt_ms(e.ts - min_ts),
            fmt_ms(e.dur.unwrap_or(0.0)),
            e.name,
            e.tid,
            snippet.replace('|', "\\|"),
        ));
    }
    if total > top {
        out.push_str(&format!(
            "\n_Showing {} of {} matches (use --top to see more)._\n",
            top, total
        ));
    }
    out.push('\n');
    out
}
