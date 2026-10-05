//! Granular trace inspection: filter events by name/category/thread/process/
//! time-window, aggregate CPU samples by function and by call stack, inspect
//! durations, render a busy timeline, and search event args.
//!
//! Each inspector returns `(markdown, json)` built from a single aggregation
//! pass, so `--json` stays in sync with the Markdown output.

use crate::trace::{TraceEvent, effective_runtask_dur, find_profiler_overhead};
use serde_json::{Value, json};
use std::sync::Arc;

// ── Time helpers ──

/// Absolute trace start (min `ts` across events), in microseconds.
/// Metadata events (`thread_name` etc., cat `__metadata`) carry process-start
/// timestamps and must not define the time base.
pub fn trace_start_us(events: &[TraceEvent]) -> f64 {
    events
        .iter()
        .filter(|e| !crate::trace::is_metadata_event(e))
        .map(|e| e.ts)
        .fold(f64::INFINITY, f64::min)
}

/// Absolute trace end (max `ts + dur`), in microseconds.
fn trace_end_us(events: &[TraceEvent]) -> f64 {
    events
        .iter()
        .fold(0.0f64, |acc, e| acc.max(e.ts + e.dur.unwrap_or(0.0)))
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

fn args_compact(raw: Option<&str>, full: bool) -> String {
    match raw {
        Some(s) => {
            if full { truncate(s, 2000) } else { truncate(s, 160) }
        }
        None => String::new(),
    }
}

pub(crate) fn window_label(window: Option<(f64, f64)>) -> &'static str {
    match window {
        Some(_) => "windowed",
        None => "full trace",
    }
}

fn percentile(sorted_asc: &[f64], p: f64) -> f64 {
    if sorted_asc.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted_asc.len() - 1) as f64).round() as usize;
    sorted_asc[idx.min(sorted_asc.len() - 1)]
}

// ── Filters ──

/// Window + thread + process + category scope applied to every inspector.
pub struct Scope {
    pub window: Option<(f64, f64)>,
    pub tid: Option<u64>,
    pub pid: Option<u64>,
    /// Lowercase substring matched against the event `cat` field.
    pub cat: Option<String>,
}

impl Scope {
    pub fn allows_event(&self, e: &TraceEvent) -> bool {
        in_window(e.ts, self.window)
            && self.tid.is_none_or(|t| e.tid == t)
            && self.pid.is_none_or(|p| e.pid == p)
            && self
                .cat
                .as_deref()
                .is_none_or(|c| e.cat.is_some_and(|ec| contains_ignore_case(ec, c)))
    }

    /// Thread/process/category filter without the window check — for CPU
    /// profile chunks, where per-sample times are attributed individually.
    pub fn allows_chunk(&self, e: &TraceEvent) -> bool {
        self.tid.is_none_or(|t| e.tid == t)
            && self.pid.is_none_or(|p| e.pid == p)
            && self
                .cat
                .as_deref()
                .is_none_or(|c| e.cat.is_some_and(|ec| contains_ignore_case(ec, c)))
    }

    pub(crate) fn window_line(&self, min_ts: f64) -> Option<String> {
        self.window.map(|(lo, hi)| {
            format!(
                "- **Window**: {:.2}ms … {:.2}ms from trace start\n",
                (lo - min_ts) / 1000.0,
                (hi - min_ts) / 1000.0,
            )
        })
    }
}

/// Allocation-free ASCII case-insensitive substring check; falls back to the
/// Unicode `to_lowercase` path for non-ASCII needles (identical semantics).
pub(crate) fn contains_ignore_case(hay: &str, needle: &str) -> bool {
    if needle.is_ascii() {
        hay.len() >= needle.len()
            && hay
                .as_bytes()
                .windows(needle.len())
                .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
    } else {
        hay.to_lowercase().contains(needle)
    }
}

/// Function/string matcher: case-insensitive substring or a regex.
pub enum Matcher {
    Substr(String),
    Regex(regex::Regex),
}

impl Matcher {
    pub fn new(pattern: &str, use_regex: bool) -> Result<Self, Box<dyn std::error::Error>> {
        if use_regex {
            Ok(Matcher::Regex(regex::Regex::new(pattern)?))
        } else {
            Ok(Matcher::Substr(pattern.to_lowercase()))
        }
    }

    pub(crate) fn matches(&self, s: &str) -> bool {
        match self {
            Matcher::Substr(p) => contains_ignore_case(s, p),
            Matcher::Regex(re) => re.is_match(s),
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            Matcher::Substr(p) => format!("`{}`", p),
            Matcher::Regex(re) => format!("/{}/", re.as_str()),
        }
    }
}

/// Event-name filter: exact names, or regexes (with `--regex`).
pub enum NameFilter {
    Exact(Vec<String>),
    Regex(Vec<regex::Regex>),
}

impl NameFilter {
    pub fn new(names: &[String], use_regex: bool) -> Result<Self, Box<dyn std::error::Error>> {
        if use_regex {
            let rs = names
                .iter()
                .map(|n| regex::Regex::new(n))
                .collect::<Result<_, _>>()?;
            Ok(NameFilter::Regex(rs))
        } else {
            Ok(NameFilter::Exact(names.to_vec()))
        }
    }

    pub fn matches(&self, name: &str) -> bool {
        match self {
            NameFilter::Exact(v) => v.iter().any(|n| n == name),
            NameFilter::Regex(v) => v.iter().any(|r| r.is_match(name)),
        }
    }

    /// Matches the event name or (if the event is an EventDispatch) its type in args.
    pub fn matches_event(&self, e: &TraceEvent) -> bool {
        if self.matches(e.name) {
            return true;
        }
        if e.name == "EventDispatch"
            && let Some(ty) = e
                .args_value()
                .and_then(|a| a.get("data"))
                .and_then(|d| d.get("type"))
                .and_then(|v| v.as_str())
        {
            return self.matches(ty);
        }
        false
    }
}

/// Output sort order for event/name listings.
#[derive(Clone, Copy)]
pub enum Sort {
    Ts,
    Dur,
    Name,
    Count,
}

// ── Inspectors (each returns (markdown, json)) ──

/// List events matching the filter, scoped, filtered by min duration, sorted.
#[allow(clippy::too_many_arguments)]
pub fn events_section(
    events: &[TraceEvent],
    filter: &NameFilter,
    display: &str,
    scope: &Scope,
    min_dur_us: f64,
    sort: Sort,
    full_args: bool,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    let mut rows: Vec<&TraceEvent> = events
        .iter()
        .filter(|e| filter.matches_event(e))
        .filter(|e| e.dur.unwrap_or(0.0) >= min_dur_us)
        .filter(|e| scope.allows_event(e))
        .collect();
    match sort {
        Sort::Ts => rows.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap()),
        Sort::Dur => rows.sort_by(|a, b| b.dur.unwrap_or(0.0).partial_cmp(&a.dur.unwrap_or(0.0)).unwrap()),
        Sort::Name => rows.sort_by(|a, b| a.name.cmp(b.name).then(a.ts.partial_cmp(&b.ts).unwrap())),
        Sort::Count => {}
    }

    let total = rows.len();
    let mut out = String::new();
    out.push_str(&format!(
        "## Events: {} ({} matches, {})\n\n",
        display,
        total,
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
        out.push('\n');
    }

    let mut json_rows: Vec<Value> = Vec::new();

    if total == 0 {
        out.push_str("No matching events.\n\n");
        return (out, Value::Array(json_rows));
    }

    out.push_str("| # | t(ms) | dur(ms) | name | tid | pid | args |\n");
    out.push_str("|---|-------|---------|------|-----|-----|------|\n");
    for (i, e) in rows.iter().take(top).enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            i + 1,
            fmt_ms(e.ts - min_ts),
            fmt_ms(e.dur.unwrap_or(0.0)),
            e.name,
            e.tid,
            e.pid,
            args_compact(e.args_raw(), full_args),
        ));
        json_rows.push(json!({
            "t_us": (e.ts - min_ts).round(),
            "dur_us": e.dur.unwrap_or(0.0).round(),
            "name": e.name,
            "tid": e.tid,
            "pid": e.pid,
            "args": e.args_value().cloned().unwrap_or(Value::Null),
        }));
    }
    if total > top {
        out.push_str(&format!(
            "\n_Showing {} of {} matches (use --top to see more)._\n",
            top, total
        ));
    }
    out.push('\n');
    (out, Value::Array(json_rows))
}

/// Duration distribution per matched event name: count/total/min/avg/p50/p90/p99/max.
pub fn stats_section(
    events: &[TraceEvent],
    filter: &NameFilter,
    display: &str,
    scope: &Scope,
    min_dur_us: f64,
    min_ts: f64,
) -> (String, Value) {
    let mut groups: rustc_hash::FxHashMap<String, Vec<f64>> = rustc_hash::FxHashMap::default();
    for e in events {
        if !filter.matches_event(e) || !scope.allows_event(e) {
            continue;
        }
        let d = e.dur.unwrap_or(0.0);
        if d < min_dur_us {
            continue;
        }
        let label = if e.name == "EventDispatch" {
            e.args_value()
                .and_then(|a| a.get("data"))
                .and_then(|d| d.get("type"))
                .and_then(|v| v.as_str())
                .map(|t| format!("EventDispatch:{}", t))
                .unwrap_or_else(|| e.name.to_string())
        } else {
            e.name.to_string()
        };
        groups.entry(label).or_default().push(d);
    }

    let mut rows: Vec<(String, Vec<f64>, f64)> = groups
        .into_iter()
        .map(|(name, durs)| {
            let total = durs.iter().sum::<f64>();
            (name, durs, total)
        })
        .collect();
    rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

    let mut out = String::new();
    out.push_str(&format!(
        "## Duration stats: {} ({} names, {})\n\n",
        display,
        rows.len(),
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
        out.push('\n');
    }

    let mut json_rows: Vec<Value> = Vec::new();
    if rows.is_empty() {
        out.push_str("No matching events.\n\n");
        return (out, Value::Array(json_rows));
    }

    out.push_str("| name | count | total(ms) | min | avg | p50 | p90 | p99 | max |\n");
    out.push_str("|------|-------|-----------|-----|-----|-----|-----|-----|-----|\n");
    for (name, mut durs, total) in rows {
        durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let count = durs.len();
        let avg = total / count as f64;
        let ms = |v: f64| format!("{:.2}", v / 1000.0);
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            name,
            count,
            ms(total),
            ms(durs[0]),
            ms(avg),
            ms(percentile(&durs, 50.0)),
            ms(percentile(&durs, 90.0)),
            ms(percentile(&durs, 99.0)),
            ms(*durs.last().unwrap()),
        ));
        json_rows.push(json!({
            "name": name,
            "count": count,
            "total_us": total.round(),
            "min_us": durs[0].round(),
            "avg_us": avg.round(),
            "p50_us": percentile(&durs, 50.0).round(),
            "p90_us": percentile(&durs, 90.0).round(),
            "p99_us": percentile(&durs, 99.0).round(),
            "max_us": durs.last().unwrap().round(),
        }));
    }
    out.push('\n');
    (out, Value::Array(json_rows))
}

/// Inter-event gap distribution and cadence/periodicity: count/min/median/avg/p90/p99/max gaps,
/// cadence in Hz, jitter (std dev), histogram buckets, per-type cadence, and sample sequence.
pub fn gaps_section(
    events: &[TraceEvent],
    filter: &NameFilter,
    display: &str,
    scope: &Scope,
    min_dur_us: f64,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    let mut matched: Vec<&TraceEvent> = events
        .iter()
        .filter(|e| filter.matches_event(e))
        .filter(|e| e.dur.unwrap_or(0.0) >= min_dur_us)
        .filter(|e| scope.allows_event(e))
        .collect();
    matched.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap());

    let mut out = String::new();
    let n = matched.len();
    if n < 2 {
        out.push_str(&format!(
            "## Event gaps & cadence: {} ({} match{}, {})\n\n",
            display,
            n,
            if n == 1 { "" } else { "es" },
            window_label(scope.window),
        ));
        if let Some(line) = scope.window_line(min_ts) {
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str("Need at least 2 matching events to compute inter-event gaps.\n\n");
        return (
            out,
            json!({
                "events": n,
                "gaps_count": 0,
                "median_gap_ms": 0.0,
                "cadence_hz": 0.0,
                "buckets": [],
                "by_type": [],
                "sample_sequence": [],
            }),
        );
    }

    let mut gaps_us: Vec<f64> = Vec::with_capacity(n - 1);
    for i in 1..n {
        let gap = (matched[i].ts - matched[i - 1].ts).max(0.0);
        gaps_us.push(gap);
    }
    let mut sorted_gaps = gaps_us.clone();
    sorted_gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let min_gap_ms = sorted_gaps[0] / 1000.0;
    let max_gap_ms = *sorted_gaps.last().unwrap() / 1000.0;
    let p50_gap_ms = percentile(&sorted_gaps, 50.0) / 1000.0;
    let p90_gap_ms = percentile(&sorted_gaps, 90.0) / 1000.0;
    let p99_gap_ms = percentile(&sorted_gaps, 99.0) / 1000.0;
    let total_gap_us: f64 = gaps_us.iter().sum();
    let avg_gap_ms = (total_gap_us / gaps_us.len() as f64) / 1000.0;
    let cadence_hz = if p50_gap_ms > 0.0 { 1000.0 / p50_gap_ms } else { 0.0 };

    let mean_us = total_gap_us / gaps_us.len() as f64;
    let variance_us = gaps_us.iter().map(|g| (g - mean_us).powi(2)).sum::<f64>() / gaps_us.len() as f64;
    let jitter_ms = variance_us.sqrt() / 1000.0;

    out.push_str(&format!(
        "## Event gaps & cadence: {} ({} events, {} gaps, {})\n\n",
        display,
        n,
        gaps_us.len(),
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
        out.push('\n');
    }

    out.push_str(&format!(
        "- **Median gap**: {:.2} ms ({:.1} Hz) · **Jitter (std dev)**: {:.2} ms\n",
        p50_gap_ms, cadence_hz, jitter_ms
    ));
    out.push_str(&format!(
        "- **Gap range**: min {:.2} ms … p90 {:.2} ms … p99 {:.2} ms … max {:.2} ms (avg {:.2} ms)\n\n",
        min_gap_ms, p90_gap_ms, p99_gap_ms, max_gap_ms, avg_gap_ms
    ));

    // Histogram buckets
    struct GapBucket {
        label: &'static str,
        min_ms: f64,
        max_ms: f64,
        count: usize,
    }
    let mut buckets = vec![
        GapBucket { label: "< 16.7ms (>60Hz)", min_ms: 0.0, max_ms: 16.7, count: 0 },
        GapBucket { label: "16.7–33.3ms (30–60Hz)", min_ms: 16.7, max_ms: 33.3, count: 0 },
        GapBucket { label: "33.3–66.7ms (15–30Hz)", min_ms: 33.3, max_ms: 66.7, count: 0 },
        GapBucket { label: "66.7–100ms (10–15Hz)", min_ms: 66.7, max_ms: 100.0, count: 0 },
        GapBucket { label: "100–300ms (3.3–10Hz)", min_ms: 100.0, max_ms: 300.0, count: 0 },
        GapBucket { label: "300–1000ms (1–3.3Hz)", min_ms: 300.0, max_ms: 1000.0, count: 0 },
        GapBucket { label: "> 1000ms (<1Hz)", min_ms: 1000.0, max_ms: f64::INFINITY, count: 0 },
    ];
    for &g_us in &gaps_us {
        let ms = g_us / 1000.0;
        for b in &mut buckets {
            if ms >= b.min_ms && ms < b.max_ms {
                b.count += 1;
                break;
            }
        }
    }

    out.push_str("### Gap distribution (histogram)\n\n");
    out.push_str("| bucket | count | pct | bar |\n");
    out.push_str("|--------|-------|-----|-----|\n");
    let total_gaps = gaps_us.len() as f64;
    let mut json_buckets = Vec::new();
    for b in &buckets {
        let pct = if total_gaps > 0.0 { (b.count as f64 / total_gaps) * 100.0 } else { 0.0 };
        let bar_len = ((pct / 5.0).round() as usize).min(20);
        let bar = "█".repeat(bar_len);
        out.push_str(&format!(
            "| {} | {} | {:.1}% | {} |\n",
            b.label, b.count, pct, bar
        ));
        json_buckets.push(json!({
            "bucket": b.label,
            "count": b.count,
            "pct": (pct * 10.0).round() / 10.0,
        }));
    }
    out.push('\n');

    // Cadence by event type / label
    let get_label = |e: &TraceEvent| -> String {
        if e.name == "EventDispatch"
            && let Some(ty) = e
                .args_value()
                .and_then(|a| a.get("data"))
                .and_then(|d| d.get("type"))
                .and_then(|v| v.as_str())
        {
            return format!("EventDispatch:{}", ty);
        }
        e.name.to_string()
    };

    let mut by_label_ts: rustc_hash::FxHashMap<String, Vec<f64>> = rustc_hash::FxHashMap::default();
    for e in &matched {
        by_label_ts.entry(get_label(e)).or_default().push(e.ts);
    }

    let mut label_stats = Vec::new();
    for (lbl, ts_list) in by_label_ts {
        let count = ts_list.len();
        if count >= 2 {
            let mut l_gaps: Vec<f64> = (1..count).map(|i| (ts_list[i] - ts_list[i - 1]).max(0.0)).collect();
            l_gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let l_min = l_gaps[0] / 1000.0;
            let l_p50 = percentile(&l_gaps, 50.0) / 1000.0;
            let l_p90 = percentile(&l_gaps, 90.0) / 1000.0;
            let l_max = *l_gaps.last().unwrap() / 1000.0;
            let l_hz = if l_p50 > 0.0 { 1000.0 / l_p50 } else { 0.0 };
            let l_mean: f64 = l_gaps.iter().sum::<f64>() / l_gaps.len() as f64;
            let l_var = l_gaps.iter().map(|g| (g - l_mean).powi(2)).sum::<f64>() / l_gaps.len() as f64;
            let l_jitter = l_var.sqrt() / 1000.0;
            label_stats.push((lbl, count, l_p50, l_hz, l_min, l_p90, l_max, l_jitter));
        } else {
            label_stats.push((lbl, count, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0));
        }
    }
    label_stats.sort_by_key(|a| std::cmp::Reverse(a.1));

    out.push_str("### Cadence by event type\n\n");
    out.push_str("| event / type | count | median gap(ms) | cadence | min(ms) | p90(ms) | max(ms) | jitter(ms) |\n");
    out.push_str("|--------------|-------|----------------|---------|---------|---------|---------|------------|\n");
    let mut json_by_type = Vec::new();
    for (lbl, count, p50, hz, min, p90, max, jitter) in &label_stats {
        let cad_str = if *hz >= 0.5 { format!("{:.1} Hz", hz) } else if *p50 > 0.0 { format!("{:.0} ms", p50) } else { "—".to_string() };
        let p50_str = if *p50 > 0.0 { fmt_ms(*p50 * 1000.0) } else { "—".to_string() };
        let min_str = if *p50 > 0.0 { fmt_ms(*min * 1000.0) } else { "—".to_string() };
        let p90_str = if *p90 > 0.0 { fmt_ms(*p90 * 1000.0) } else { "—".to_string() };
        let max_str = if *p50 > 0.0 { fmt_ms(*max * 1000.0) } else { "—".to_string() };
        let jit_str = if *p50 > 0.0 { fmt_ms(*jitter * 1000.0) } else { "—".to_string() };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            lbl, count, p50_str, cad_str, min_str, p90_str, max_str, jit_str
        ));
        json_by_type.push(json!({
            "label": lbl,
            "count": count,
            "median_gap_ms": (p50 * 100.0).round() / 100.0,
            "cadence_hz": (hz * 10.0).round() / 10.0,
            "min_gap_ms": (min * 100.0).round() / 10.0,
            "p90_gap_ms": (p90 * 100.0).round() / 100.0,
            "max_gap_ms": (max * 100.0).round() / 100.0,
            "jitter_ms": (jitter * 100.0).round() / 10.0,
        }));
    }
    out.push('\n');

    // Sample sequence (first `top`)
    let shown = top.min(n).max(1);
    out.push_str(&format!("### Event sequence (first {} of {})\n\n", shown, n));
    out.push_str("| # | t(ms) | gap(ms) | event / type | dur(ms) |\n");
    out.push_str("|---|-------|---------|--------------|---------|\n");
    let mut json_sequence = Vec::new();
    for i in 0..shown {
        let e = matched[i];
        let gap_str = if i == 0 { "—".to_string() } else { fmt_ms(gaps_us[i - 1]) };
        let lbl = get_label(e);
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            i + 1,
            fmt_ms(e.ts - min_ts),
            gap_str,
            lbl,
            fmt_ms(e.dur.unwrap_or(0.0)),
        ));
        json_sequence.push(json!({
            "index": i + 1,
            "t_us": (e.ts - min_ts).round(),
            "gap_us": if i == 0 { Value::Null } else { json!(gaps_us[i - 1].round()) },
            "label": lbl,
            "dur_us": e.dur.unwrap_or(0.0).round(),
        }));
    }
    if n > shown {
        out.push_str(&format!("\n_Showing {} of {} events (use --top to see more)._\n", shown, n));
    }
    out.push('\n');

    let summary = json!({
        "events": n,
        "gaps_count": gaps_us.len(),
        "min_gap_ms": (min_gap_ms * 100.0).round() / 100.0,
        "median_gap_ms": (p50_gap_ms * 100.0).round() / 100.0,
        "avg_gap_ms": (avg_gap_ms * 100.0).round() / 100.0,
        "p90_gap_ms": (p90_gap_ms * 100.0).round() / 100.0,
        "p99_gap_ms": (p99_gap_ms * 100.0).round() / 100.0,
        "max_gap_ms": (max_gap_ms * 100.0).round() / 100.0,
        "cadence_hz": (cadence_hz * 10.0).round() / 10.0,
        "jitter_ms": (jitter_ms * 100.0).round() / 10.0,
        "buckets": json_buckets,
        "by_type": json_by_type,
        "sample_sequence": json_sequence,
    });

    (out, summary)
}

/// Aggregate CPU profile self-time for functions matching `matcher`, scoped.
pub fn functions_section(
    events: &[TraceEvent],
    matcher: &Matcher,
    scope: &Scope,
    top: usize,
    min_ts: f64,
    cache: Option<&crate::analysis::CpuProfileCache>,
) -> (String, Value) {
    let (node_map, self_times) = crate::analysis::cpu_profile_for(events, scope, cache);
    let total_in_scope: f64 = self_times.values().sum();

    let mut funcs: Vec<(&str, &str, f64)> = self_times
        .iter()
        .filter_map(|(id, t)| node_map.get(id).map(|(n, u, _)| (n.as_str(), u.as_str(), *t)))
        .filter(|(n, _, _)| matcher.matches(n))
        .collect();
    funcs.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

    let matched_total: f64 = funcs.iter().map(|(_, _, t)| *t).sum();

    let mut out = String::new();
    out.push_str(&format!(
        "## CPU Functions matching {} ({} functions, {})\n\n",
        matcher.label(),
        funcs.len(),
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
    }
    out.push_str(&format!("- **Matched self-time**: {}ms\n\n", fmt_ms(matched_total)));

    let mut json_rows: Vec<Value> = Vec::new();
    if funcs.is_empty() {
        out.push_str("No matching functions.\n\n");
        return (out, Value::Array(json_rows));
    }

    out.push_str("| # | function | self | % in scope | file |\n");
    out.push_str("|---|----------|------|------------|------|\n");
    for (i, (name, url, t)) in funcs.iter().take(top).enumerate() {
        let pct = if total_in_scope > 0.0 { t / total_in_scope * 100.0 } else { 0.0 };
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
        json_rows.push(json!({
            "function": if name.is_empty() { "(anonymous)" } else { *name },
            "url": url,
            "self_us": t.round(),
            "pct": pct,
        }));
    }
    out.push('\n');
    (out, Value::Array(json_rows))
}

// ── CPU profile collection (shared by stacks + flame) ──

/// node id -> (name, url, parent). The node table is `Arc`-shared: cached
/// queries re-use it without re-cloning thousands of `(name, url)` strings.
pub struct CpuProfile {
    pub nodes: Arc<crate::analysis::ProfileNodes>,
    pub leaf_time: crate::analysis::ProfileSelfTimes,
}

/// Register all CPU profile nodes, and accumulate per-leaf sample time from
/// in-scope chunks. Shared by `stacks_section` and `stacks_folded`.
pub fn collect_cpu_profile(
    events: &[TraceEvent],
    scope: &Scope,
    cache: Option<&crate::analysis::CpuProfileCache>,
) -> CpuProfile {
    let (nodes, leaf_time) = crate::analysis::cpu_profile_for(events, scope, cache);
    CpuProfile { nodes, leaf_time }
}

fn node_chain_names(nodes: &crate::analysis::ProfileNodes, leaf: crate::analysis::NodeKey) -> Vec<String> {
    // Parent ids resolve within the leaf's own (pid, session).
    let mut keys: Vec<crate::analysis::NodeKey> = Vec::new();
    let mut cur = Some(leaf);
    while let Some(key) = cur {
        keys.push(key);
        cur = nodes
            .get(&key)
            .and_then(|n| n.2)
            .map(|p| (key.0, key.1, p));
    }
    keys.reverse();
    keys.iter()
        .map(|key| {
            nodes
                .get(key)
                .map(|(n, _, _)| {
                    if n.is_empty() { "(anonymous)".to_string() } else { n.clone() }
                })
                .unwrap_or_else(|| "(unknown)".to_string())
        })
        .collect()
}

/// Aggregate CPU sample time per leaf node and render each leaf's full call
/// stack (root → leaf), heaviest first. Optionally filter leaves by `matcher`.
pub fn stacks_section(
    events: &[TraceEvent],
    matcher: Option<&Matcher>,
    scope: &Scope,
    top: usize,
    min_ts: f64,
    cache: Option<&crate::analysis::CpuProfileCache>,
) -> (String, Value) {
    let cpu = collect_cpu_profile(events, scope, cache);
    let node_map = cpu.nodes;
    let leaf_time = cpu.leaf_time;

    let mut leaves: Vec<(crate::analysis::NodeKey, f64)> = leaf_time
        .into_iter()
        .filter(|(id, _)| match matcher {
            Some(m) => node_map.get(id).map(|(n, _, _)| m.matches(n)).unwrap_or(false),
            None => true,
        })
        .collect();
    leaves.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let mut out = String::new();
    let filter_desc = match matcher {
        Some(m) => format!("leaf ~ {}", m.label()),
        None => "all leaves".to_string(),
    };
    out.push_str(&format!(
        "## Heaviest call stacks ({} leaves, {}, {})\n\n",
        leaves.len(),
        filter_desc,
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
    }

    let mut json_rows: Vec<Value> = Vec::new();
    if leaves.is_empty() {
        out.push_str("No matching stacks.\n\n");
        return (out, Value::Array(json_rows));
    }

    let grand_total: f64 = leaves.iter().map(|(_, t)| *t).sum();
    for (rank, (id, t)) in leaves.iter().take(top).enumerate() {
        let (name, url, _parent) = match node_map.get(id) {
            Some(n) => (n.0.as_str().to_string(), n.1.clone(), n.2),
            None => ("(unknown)".to_string(), String::new(), None),
        };
        let chain = node_chain_names(&node_map, *id);
        let depth = chain.len();
        let pct = if grand_total > 0.0 { t / grand_total * 100.0 } else { 0.0 };
        let display_chain = if depth > 14 {
            let head: Vec<&str> = chain[..2].iter().map(|s| s.as_str()).collect();
            let tail: Vec<&str> = chain[depth - 10..].iter().map(|s| s.as_str()).collect();
            format!("{} → … → {}", head.join(" → "), tail.join(" → "))
        } else {
            chain.join(" → ")
        };

        let short_url = url.rfind('/').map(|i| &url[i + 1..]).unwrap_or(&url).to_string();
        let leaf_label = if name.is_empty() { "(anonymous)" } else { &name };

        out.push_str(&format!(
            "### #{}  {}ms ({:.1}% of matched)  depth {}\n",
            rank + 1,
            fmt_ms(*t),
            pct,
            depth,
        ));
        out.push_str(&format!("- **leaf**: `{}` _{}_ \n", leaf_label, short_url));
        out.push_str(&format!("- **stack**: {}\n\n", display_chain));

        json_rows.push(json!({
            "rank": rank + 1,
            "self_us": t.round(),
            "pct": pct,
            "depth": depth,
            "leaf": leaf_label,
            "leaf_url": short_url,
            "stack": chain,
        }));
    }
    (out, Value::Array(json_rows))
}

/// Folded-stack output (`a;b;c <weight>`) for flamegraph.pl / speedscope.
/// `weight` is total self-time in microseconds. One line per distinct leaf.
pub fn stacks_folded(
    events: &[TraceEvent],
    matcher: Option<&Matcher>,
    scope: &Scope,
    cache: Option<&crate::analysis::CpuProfileCache>,
) -> String {
    let cpu = collect_cpu_profile(events, scope, cache);
    let mut leaves: Vec<(crate::analysis::NodeKey, f64)> = cpu
        .leaf_time
        .iter()
        .filter(|(id, _)| match matcher {
            Some(m) => cpu.nodes.get(id).map(|(n, _, _)| m.matches(n)).unwrap_or(false),
            None => true,
        })
        .map(|(id, t)| (*id, *t))
        .collect();
    leaves.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let mut out = String::new();
    for (id, t) in &leaves {
        let chain = node_chain_names(&cpu.nodes, *id);
        out.push_str(&format!("{} {:.0}\n", chain.join(";"), t));
    }
    out
}

/// Does this JSON value (or any nested string, object key, number, bool)
/// match? Mirrors the old behavior of matching the fully serialized JSON.
fn value_matches(v: &Value, matcher: &Matcher) -> bool {
    match v {
        Value::String(s) => matcher.matches(s),
        Value::Array(a) => a.iter().any(|v| value_matches(v, matcher)),
        Value::Object(m) => {
            m.iter()
                .any(|(k, v)| matcher.matches(k) || value_matches(v, matcher))
        }
        Value::Number(n) => matcher.matches(&n.to_string()),
        Value::Bool(b) => matcher.matches(if *b { "true" } else { "false" }),
        Value::Null => matcher.matches("null"),
    }
}

/// Search event `args` (JSON) for `matcher` and list matches.
pub fn find_section(
    events: &[TraceEvent],
    matcher: &Matcher,
    scope: &Scope,
    full_args: bool,
    top: usize,
    min_ts: f64,
    cache: Option<&crate::analysis::CpuProfileCache>,
) -> (String, Value) {
    let mut matches: Vec<(&TraceEvent, String)> = Vec::new();
    let needle_label = matcher.label();
    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        let Some(raw) = e.args_raw() else { continue };
        // Fast pre-filter on the raw JSON bytes: substring needles can't
        // match an event whose raw args don't contain them, so skip the
        // parse (and the cache fill) entirely.
        if let Matcher::Substr(p) = matcher
            && !contains_ignore_case(raw, p) {
                continue;
            }
        let args = match e.args_value() {
            Some(v) => v,
            None => continue,
        };
        // Walk the JSON tree instead of serializing the whole value to a
        // string per event (numbers/bools are skipped without allocation).
        if value_matches(args, matcher) {
            let s = serde_json::to_string(args).unwrap_or_default();
            let snippet = if full_args {
                truncate(&s, 500)
            } else {
                match matcher {
                    Matcher::Substr(p) => {
                        let idx = s.to_lowercase().find(p).unwrap_or(0);
                        snippet_around(&s, idx, p.len())
                    }
                    Matcher::Regex(re) => {
                        let m = re.find(&s);
                        let idx = m.as_ref().map(|m| m.start()).unwrap_or(0);
                        let len = m.map(|m| m.len()).unwrap_or(0);
                        snippet_around(&s, idx, len)
                    }
                }
            };
            matches.push((e, snippet));
        }
    }

    matches.sort_by(|a, b| a.0.ts.partial_cmp(&b.0.ts).unwrap());
    let total = matches.len();

    let mut out = String::new();
    out.push_str(&format!(
        "## Find {} in event args ({} matches)\n\n",
        needle_label, total,
    ));

    let mut json_rows: Vec<Value> = Vec::new();
    if total == 0 {
        out.push_str("No matches.\n\n");
        return (out, Value::Array(json_rows));
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
        json_rows.push(json!({
            "t_us": (e.ts - min_ts).round(),
            "dur_us": e.dur.unwrap_or(0.0).round(),
            "name": e.name,
            "tid": e.tid,
            "args": e.args_value().cloned().unwrap_or(Value::Null),
        }));
    }
    if total > top {
        out.push_str(&format!(
            "\n_Showing {} of {} matches (use --top to see more)._\n",
            top, total
        ));
    }
    out.push('\n');

    // Combined search: also match CPU profile function names and source URLs.
    let (node_map, self_times) = crate::analysis::cpu_profile_for(events, scope, cache);
    let mut cpu_matches: Vec<(String, String, f64)> = self_times
        .iter()
        .filter_map(|(id, t)| node_map.get(id).map(|(n, u, _)| (n.clone(), u.clone(), *t)))
        .filter(|(n, u, _)| matcher.matches(n) || matcher.matches(u))
        .collect();
    cpu_matches.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

    out.push_str(&format!(
        "### CPU profile matches ({} functions/URLs)\n\n",
        cpu_matches.len(),
    ));
    if cpu_matches.is_empty() {
        out.push_str("No CPU profile matches.\n\n");
        return (out, Value::Array(json_rows));
    }
    out.push_str("| # | function | self(ms) | url |\n");
    out.push_str("|---|----------|----------|-----|\n");
    for (i, (name, url, t)) in cpu_matches.iter().take(top).enumerate() {
        let short_url = url.rfind('/').map(|i| &url[i + 1..]).unwrap_or(url);
        let label = if name.is_empty() { "(anonymous)" } else { name };
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            i + 1,
            label,
            fmt_ms(*t),
            short_url,
        ));
        json_rows.push(json!({
            "source": "cpu-profile",
            "function": if name.is_empty() { "(anonymous)" } else { name },
            "self_us": t.round(),
            "url": url,
        }));
    }
    out.push('\n');
    (out, Value::Array(json_rows))
}

/// `idx`/`len` are BYTE offsets into `s` (from `find()`/regex `start()` on
/// possibly-lowercased text); convert to a char range before slicing the char
/// vec, or multibyte text before the match panics / renders the wrong span.
/// Non-boundary offsets (the Substr path matches on a lowercased copy whose
/// byte layout can differ) are floored/ceiled to the nearest boundary.
fn snippet_around(s: &str, idx: usize, len: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut start_byte = idx.min(s.len());
    while start_byte > 0 && !s.is_char_boundary(start_byte) {
        start_byte -= 1;
    }
    let mut end_byte = (idx.saturating_add(len)).min(s.len());
    while end_byte < s.len() && !s.is_char_boundary(end_byte) {
        end_byte += 1;
    }
    let match_start = s[..start_byte].chars().count();
    let match_end = s[..end_byte].chars().count();
    let start = match_start.saturating_sub(60);
    let end = (match_end + 60).min(chars.len());
    chars[start..end].iter().collect()
}

/// Discovery: distinct event names with count and total duration, scoped, sorted.
pub fn names_section(
    events: &[TraceEvent],
    scope: &Scope,
    sort: Sort,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    let mut stats: rustc_hash::FxHashMap<&str, (usize, f64)> = rustc_hash::FxHashMap::default();
    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        let entry = stats.entry(e.name).or_default();
        entry.0 += 1;
        entry.1 += e.dur.unwrap_or(0.0);
    }

    let mut rows: Vec<(String, usize, f64)> = stats
        .into_iter()
        .map(|(n, (c, d))| (n.to_string(), c, d))
        .collect();
    match sort {
        Sort::Count => rows.sort_by_key(|b| std::cmp::Reverse(b.1)),
        Sort::Name => rows.sort_by(|a, b| a.0.cmp(&b.0)),
        _ => rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap().then(a.0.cmp(&b.0))),
    }
    let total = rows.len();

    let mut out = String::new();
    out.push_str(&format!(
        "## Event names ({} distinct, {})\n\n",
        total,
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
        out.push('\n');
    }
    let mut json_rows: Vec<Value> = Vec::new();
    if total == 0 {
        out.push_str("No events.\n\n");
        return (out, Value::Array(json_rows));
    }

    out.push_str("| # | name | count | total(ms) | avg(ms) |\n");
    out.push_str("|---|------|-------|-----------|---------|\n");
    for (i, (name, count, dur)) in rows.iter().take(top).enumerate() {
        let avg = if *count > 0 { dur / *count as f64 } else { 0.0 };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            i + 1,
            name,
            count,
            fmt_ms(*dur),
            fmt_ms(avg),
        ));
        json_rows.push(json!({
            "name": name,
            "count": count,
            "total_us": dur.round(),
            "avg_us": avg.round(),
        }));
    }
    if total > top {
        out.push_str(&format!(
            "\n_Showing {} of {} names (use --top to see more)._\n",
            top, total
        ));
    }
    out.push('\n');
    (out, Value::Array(json_rows))
}

/// Discovery: distinct threads (tid) with event count, RunTask total duration,
/// and the most frequent event name. Scoped.
pub fn threads_section(
    events: &[TraceEvent],
    scope: &Scope,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    let overheads = find_profiler_overhead(events);
    let mut tids: rustc_hash::FxHashMap<u64, (usize, f64, rustc_hash::FxHashMap<&str, usize>)> = rustc_hash::FxHashMap::default();
    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        let entry = tids.entry(e.tid).or_default();
        entry.0 += 1;
        if e.name == "RunTask" {
            entry.1 += effective_runtask_dur(e, &overheads);
        }
        *entry.2.entry(e.name).or_default() += 1;
    }

    let mut rows: Vec<(u64, usize, f64, String)> = tids
        .into_iter()
        .map(|(tid, (count, runtask, names))| {
            let top_name = names
                .into_iter()
                .max_by_key(|(_, c)| *c)
                .map(|(n, _)| n.to_string())
                .unwrap_or_default();
            (tid, count, runtask, top_name)
        })
        .collect();
    rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
    let total = rows.len();

    let mut out = String::new();
    out.push_str(&format!(
        "## Threads ({} distinct, {})\n\n",
        total,
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
        out.push('\n');
    }
    let mut json_rows: Vec<Value> = Vec::new();
    if total == 0 {
        out.push_str("No threads.\n\n");
        return (out, Value::Array(json_rows));
    }

    out.push_str("| # | tid | events | RunTask(ms) | top event |\n");
    out.push_str("|---|-----|--------|------------|-----------|\n");
    for (i, (tid, count, runtask, top_name)) in rows.iter().take(top).enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            i + 1,
            tid,
            count,
            fmt_ms(*runtask),
            top_name,
        ));
        json_rows.push(json!({
            "tid": tid,
            "events": count,
            "runtask_us": runtask.round(),
            "top_event": top_name,
        }));
    }
    if total > top {
        out.push_str(&format!(
            "\n_Showing {} of {} threads (use --top to see more)._\n",
            top, total
        ));
    }
    out.push('\n');
    (out, Value::Array(json_rows))
}

/// Text busy-timeline: buckets the (windowed or full) trace and shows RunTask
/// duration and event count per bucket, with a proportional bar.
pub fn timeline_section(
    events: &[TraceEvent],
    scope: &Scope,
    bucket_ms: Option<f64>,
    min_ts: f64,
) -> (String, Value) {
    let start = scope.window.map(|(lo, _)| lo).unwrap_or(min_ts);
    let end = scope
        .window
        .map(|(_, hi)| hi)
        .unwrap_or_else(|| trace_end_us(events));
    let mut out = String::new();
    let mut json_rows: Vec<Value> = Vec::new();

    if end <= start {
        out.push_str("_Empty trace range._\n\n");
        return (out, Value::Array(json_rows));
    }

    let span_ms = (end - start) / 1000.0;
    let bucket_ms = bucket_ms.unwrap_or_else(|| (span_ms / 40.0).round().clamp(10.0, 500.0));
    let bucket_us = (bucket_ms * 1000.0).max(1.0);
    let n_buckets = (((end - start) / bucket_us).ceil() as usize).max(1);
    let overheads = find_profiler_overhead(events);
    let mut runtask = vec![0.0f64; n_buckets];
    let mut counts = vec![0usize; n_buckets];

    for e in events {
        if !scope.allows_event(e) || e.ts < start || e.ts > end {
            continue;
        }
        let bi = ((e.ts - start) / bucket_us) as usize;
        if bi < n_buckets {
            counts[bi] += 1;
            if e.name == "RunTask" {
                runtask[bi] += effective_runtask_dur(e, &overheads);
            }
        }
    }


    out.push_str(&format!(
        "## Timeline ({} buckets × {:.0}ms, {})\n\n",
        n_buckets,
        bucket_ms,
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
        out.push('\n');
    }

    out.push_str("| # | t(ms) | runtask(ms) | busy | events |\n");
    out.push_str("|---|-------|-------------|------|--------|\n");
    let width = 24;
    for i in 0..n_buckets {
        let busy = (runtask[i] / bucket_us).min(1.0);
        let filled = (busy * width as f64).round() as usize;
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(width - filled));
        let t_ms = (start - min_ts) / 1000.0 + i as f64 * bucket_ms;
        out.push_str(&format!(
            "| {} | {:.0} | {} | {} {:.0}% | {} |\n",
            i + 1,
            t_ms,
            fmt_ms(runtask[i]),
            bar,
            busy * 100.0,
            counts[i],
        ));
        json_rows.push(json!({
            "bucket": i,
            "t_us": ((start - min_ts) + i as f64 * bucket_us).round(),
            "runtask_us": runtask[i].round(),
            "busy": busy,
            "events": counts[i],
        }));
    }
    out.push('\n');
    (out, Value::Array(json_rows))
}

/// Find the longest RunTask (ts, dur) in scope (window ignored). Used by --worst.
pub fn worst_runtask(events: &[TraceEvent], scope: &Scope) -> Option<(f64, f64)> {
    let overheads = find_profiler_overhead(events);
    events
        .iter()
        .filter(|e| e.name == "RunTask" && e.ph == b'X' && scope.allows_event(e))
        .filter_map(|e| {
            e.dur.map(|_| {
                let eff = effective_runtask_dur(e, &overheads);
                (e.ts, eff)
            })
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
}

/// Drill into the heaviest RunTasks: for each, list its child events grouped by
/// name with duration, and the top FunctionCall's target function.
pub fn task_section(
    events: &[TraceEvent],
    scope: &Scope,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    let overheads = find_profiler_overhead(events);
    let mut tasks: Vec<(&TraceEvent, f64)> = events
        .iter()
        .filter(|e| e.name == "RunTask" && e.ph == b'X' && e.dur.is_some() && scope.allows_event(e))
        .map(|e| {
            let eff = effective_runtask_dur(e, &overheads);
            (e, eff)
        })
        .collect();
    tasks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then_with(|| b.0.dur.unwrap().partial_cmp(&a.0.dur.unwrap()).unwrap()));
    let total = tasks.len();

    let mut out = String::new();
    out.push_str(&format!(
        "## RunTask breakdown ({} tasks, {})\n\n",
        total,
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
        out.push('\n');
    }

    let mut json_rows: Vec<Value> = Vec::new();

    for (rank, &(rt, eff_dur)) in tasks.iter().take(top).enumerate() {
        let rt_ts = rt.ts;
        let rt_end = rt.ts + rt.dur.unwrap();
        let rt_dur = rt.dur.unwrap();
        let overhead = (rt_dur - eff_dur).max(0.0);

        let mut groups: rustc_hash::FxHashMap<String, (usize, f64)> = rustc_hash::FxHashMap::default();
        let mut top_fc: Option<(String, f64)> = None;
        for e in events {
            if e.tid != rt.tid || e.name == "RunTask" || e.ts < rt_ts || e.ts > rt_end {
                continue;
            }
            let d = e.dur.unwrap_or(0.0);
            let g = groups.entry(e.name.to_string()).or_default();
            g.0 += 1;
            g.1 += d;
            if e.name == "FunctionCall" && top_fc.as_ref().is_none_or(|(_, dd)| d > *dd) {
                let fn_name = e
                    .args_value()
                    .and_then(|a| a.get("data"))
                    .and_then(|d| d.get("functionName"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                top_fc = Some((fn_name, d));
            }
        }

        let mut children: Vec<(String, usize, f64)> = groups.into_iter().map(|(n, (c, d))| (n, c, d)).collect();
        children.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap().then(a.0.cmp(&b.0)));

        let dur_str = if overhead > 0.0 {
            format!("{}ms (raw {}ms, profiler overhead {}ms)", fmt_ms(eff_dur), fmt_ms(rt_dur), fmt_ms(overhead))
        } else {
            format!("{}ms", fmt_ms(rt_dur))
        };
        out.push_str(&format!(
            "### #{}  t={:.2}ms  dur={}  tid={}\n\n",
            rank + 1,
            (rt_ts - min_ts) / 1000.0,
            dur_str,
            rt.tid,
        ));
        if children.is_empty() {
            out.push_str("_No child events._\n\n");
        } else {
            out.push_str("| child | count | total(ms) | % of task |\n");
            out.push_str("|-------|-------|-----------|-----------|\n");
            for (name, count, dur) in &children {
                let pct = if rt_dur > 0.0 { dur / rt_dur * 100.0 } else { 0.0 };
                out.push_str(&format!(
                    "| {} | {} | {} | {:.1}% |\n",
                    name,
                    count,
                    fmt_ms(*dur),
                    pct,
                ));
            }
            if let Some((fn_name, d)) = &top_fc {
                let label = if fn_name.is_empty() { "(anonymous)" } else { fn_name };
                out.push_str(&format!("\n- **top FunctionCall**: `{}` ({})\n", label, fmt_ms(*d)));
            }
            out.push('\n');
        }

        let json_children: Vec<Value> = children
            .iter()
            .map(|(name, count, dur)| json!({"name": name, "count": count, "total_us": dur.round()}))
            .collect();
        json_rows.push(json!({
            "rank": rank + 1,
            "t_us": (rt_ts - min_ts).round(),
            "dur_us": eff_dur.round(),
            "raw_dur_us": rt_dur.round(),
            "profiler_overhead_us": overhead.round(),
            "tid": rt.tid,
            "children": json_children,
            "top_function_call": top_fc.as_ref().map(|(n, d)| json!({
                "name": if n.is_empty() { "(anonymous)" } else { n },
                "dur_us": d.round(),
            })),
        }));
    }
    (out, Value::Array(json_rows))
}

/// Jank clusters: windows where dropped frames / ≥16.7ms spikes occurred,
/// even below the 50ms Long Task threshold. See analysis::analyze_jank.
/// Honors the scope's window: events outside are ignored, and hot buckets
/// are not merged across the window boundary.
pub fn jank_section(
    events: &[TraceEvent],
    scope: &Scope,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    let main_tid = crate::trace::detect_main_thread(events);
    let res = crate::analysis::analyze_jank(events, main_tid, Some(scope));

    let mut out = String::new();
    out.push_str(&format!(
        "## Jank Clusters ({} total dropped frames, {}ms buckets)\n\n",
        res.total_dropped,
        res.bucket_ms.round() as i64,
    ));
    out.push_str(
        "Windows where dropped frames, ≥16.7ms spikes (RunTask/FireAnimationFrame/GPUTask)\n\
         or heavy main-thread busy occurred — even below the 50ms Long Task threshold.\n\n",
    );

    let mut json_rows: Vec<Value> = Vec::new();
    if res.clusters.is_empty() {
        out.push_str("No jank clusters found.\n\n");
        return (out, Value::Array(json_rows));
    }

    out.push_str("| # | t(ms) | span(ms) | busy(ms) | max RunTask | max FAF | max GPUTask | dropped | what happened |\n");
    out.push_str("|---|-------|----------|----------|-------------|---------|-------------|---------|---------------|\n");
    for (i, c) in res.clusters.iter().take(top).enumerate() {
        let calls: String = c
            .top_calls
            .iter()
            .map(|(n, d)| format!("{} ({:.1}ms)", n, d / 1000.0))
            .collect::<Vec<_>>()
            .join(", ");
        let calls = if calls.is_empty() {
            "—".to_string()
        } else {
            calls
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            i + 1,
            fmt_ms(c.start_us - min_ts),
            fmt_ms(c.end_us - c.start_us),
            fmt_ms(c.busy_us),
            fmt_ms(c.max_run_us),
            fmt_ms(c.max_faf_us),
            fmt_ms(c.max_gpu_us),
            c.dropped_frames,
            calls,
        ));
        json_rows.push(json!({
            "rank": i + 1,
            "t_us": (c.start_us - min_ts).round(),
            "span_us": (c.end_us - c.start_us).round(),
            "busy_us": c.busy_us.round(),
            "max_run_us": c.max_run_us.round(),
            "max_faf_us": c.max_faf_us.round(),
            "max_gpu_us": c.max_gpu_us.round(),
            "dropped_frames": c.dropped_frames,
            "top_calls": c.top_calls.iter().map(|(n, d)| json!({"name": n, "dur_us": d.round()})).collect::<Vec<_>>(),
        }));
    }
    out.push('\n');
    (out, Value::Array(json_rows))
}

/// Evenly-spaced index selection for downsampling a long timeline to `top`
/// rows. Always includes the first and last sample.
fn downsample_indices(n: usize, top: usize) -> Vec<usize> {
    if n == 0 || top == 0 {
        return Vec::new();
    }
    if n <= top {
        return (0..n).collect();
    }
    let denom = (top - 1).max(1);
    (0..top).map(|k| k * (n - 1) / denom).collect()
}

/// Memory timeline: `UpdateCounters` (phase `I`) samples carry the JS heap
/// size and DOM node / document / event-listener counts. Renders a peak &
/// growth summary plus a (downsampled) timeline; JSON returns every sample.
pub fn memory_section(
    events: &[TraceEvent],
    scope: &Scope,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    #[derive(Clone, Copy)]
    struct Sample {
        ts: f64,
        heap: f64,
        nodes: f64,
        documents: f64,
        listeners: f64,
    }
    let mut samples: Vec<Sample> = Vec::new();
    for e in events {
        if e.name != "UpdateCounters" || e.ph != b'I' || !scope.allows_event(e) {
            continue;
        }
        let Some(args) = e.args_value() else { continue };
        let data = args.get("data");
        samples.push(Sample {
            ts: e.ts,
            heap: data.and_then(|d| d.get("jsHeapSizeUsed")).and_then(|v| v.as_f64()).unwrap_or(0.0),
            nodes: data.and_then(|d| d.get("nodes")).and_then(|v| v.as_f64()).unwrap_or(0.0),
            documents: data.and_then(|d| d.get("documents")).and_then(|v| v.as_f64()).unwrap_or(0.0),
            listeners: data.and_then(|d| d.get("jsEventListeners")).and_then(|v| v.as_f64()).unwrap_or(0.0),
        });
    }
    samples.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap());
    let n = samples.len();

    let mut out = String::new();
    out.push_str(&format!(
        "## Memory timeline (UpdateCounters) — {} samples, {}\n\n",
        n,
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
        out.push('\n');
    }
    if n == 0 {
        out.push_str("No UpdateCounters samples.\n\n");
        let summary = json!({"samples": 0});
        return (out, json!({"summary": summary, "samples": []}));
    }

    let mb = |b: f64| b / 1_048_576.0;
    let first = samples[0];
    let last = samples[n - 1];
    let peak = samples
        .iter()
        .copied()
        .max_by(|a, b| a.heap.partial_cmp(&b.heap).unwrap())
        .unwrap();
    let peak_nodes = samples.iter().map(|s| s.nodes).fold(f64::NEG_INFINITY, f64::max);
    let peak_documents = samples.iter().map(|s| s.documents).fold(f64::NEG_INFINITY, f64::max);
    let peak_listeners = samples.iter().map(|s| s.listeners).fold(f64::NEG_INFINITY, f64::max);

    let span_s = (last.ts - first.ts) / 1_000_000.0;
    let heap_growth_mb = mb(last.heap - first.heap);
    let nodes_growth = last.nodes - first.nodes;
    let listeners_growth = last.listeners - first.listeners;
    let (heap_rate, nodes_rate, listeners_rate) = if span_s > 0.05 {
        (
            heap_growth_mb / span_s,
            nodes_growth / span_s,
            listeners_growth / span_s,
        )
    } else {
        (0.0, 0.0, 0.0)
    };

    out.push_str(&format!(
        "- **JS heap**: {:.1} MB → {:.1} MB ({:+.1} MB), peak {:.1} MB at t={:.2}ms\n",
        mb(first.heap),
        mb(last.heap),
        mb(last.heap - first.heap),
        mb(peak.heap),
        (peak.ts - min_ts) / 1000.0,
    ));
    out.push_str(&format!(
        "- **DOM nodes** peak {:.0} · **documents** peak {:.0} · **event listeners** peak {:.0}\n",
        peak_nodes, peak_documents, peak_listeners,
    ));
    if span_s > 0.05 {
        out.push_str(&format!(
            "- **Growth velocity**: {:+.2} MB/s heap · {:+.1} nodes/s · {:+.1} listeners/s (span {:.1}s)\n\n",
            heap_rate, nodes_rate, listeners_rate, span_s,
        ));
    } else {
        out.push('\n');
    }

    // Significant change points (jumps)
    struct ChangePoint {
        ts: f64,
        docs: f64,
        delta_docs: f64,
        nodes: f64,
        delta_nodes: f64,
        listeners: f64,
        delta_listeners: f64,
        heap: f64,
        delta_heap: f64,
    }
    let mut change_points: Vec<ChangePoint> = Vec::new();
    for i in 1..n {
        let prev = samples[i - 1];
        let curr = samples[i];
        let dd = curr.documents - prev.documents;
        let dn = curr.nodes - prev.nodes;
        let dl = curr.listeners - prev.listeners;
        let dh = curr.heap - prev.heap;
        if dd != 0.0 || dl.abs() >= 30.0 || dn.abs() >= 50.0 {
            change_points.push(ChangePoint {
                ts: curr.ts,
                docs: curr.documents,
                delta_docs: dd,
                nodes: curr.nodes,
                delta_nodes: dn,
                listeners: curr.listeners,
                delta_listeners: dl,
                heap: curr.heap,
                delta_heap: dh,
            });
        }
    }

    if !change_points.is_empty() {
        let cp_shown = change_points.len().min(top);
        out.push_str(&format!(
            "### Significant change points (jumps) — {} detected\n\n",
            change_points.len()
        ));
        out.push_str("| t(ms) | docs | nodes (Δ) | listeners (Δ) | heap(MB) (Δ) |\n");
        out.push_str("|-------|------|-----------|---------------|--------------|\n");
        for cp in change_points.iter().take(cp_shown) {
            let dfmt = |v: f64| {
                if v == 0.0 { String::new() } else { format!(" ({:+})", v as i64) }
            };
            let dhfmt = |v: f64| {
                let m = mb(v);
                if m.abs() < 0.05 { String::new() } else { format!(" ({:+.1})", m) }
            };
            out.push_str(&format!(
                "| {:.2} | {:.0}{} | {:.0}{} | {:.0}{} | {:.1}{} |\n",
                (cp.ts - min_ts) / 1000.0,
                cp.docs,
                dfmt(cp.delta_docs),
                cp.nodes,
                dfmt(cp.delta_nodes),
                cp.listeners,
                dfmt(cp.delta_listeners),
                mb(cp.heap),
                dhfmt(cp.delta_heap),
            ));
        }
        if change_points.len() > cp_shown {
            out.push_str(&format!(
                "\n_Showing {} of {} change points (use --top to see more)._\n",
                cp_shown, change_points.len()
            ));
        }
        out.push('\n');
    }

    // Growth progression (5s buckets)
    let bucket_size_us = 5_000_000.0;
    let mut json_buckets = Vec::new();
    if span_s >= 5.0 && n >= 10 {
        let mut buckets_map: rustc_hash::FxHashMap<usize, Vec<Sample>> = rustc_hash::FxHashMap::default();
        for s in &samples {
            let b = ((s.ts - first.ts) / bucket_size_us) as usize;
            buckets_map.entry(b).or_default().push(*s);
        }
        let mut bucket_keys: Vec<usize> = buckets_map.keys().copied().collect();
        bucket_keys.sort();

        out.push_str("### Growth progression (5s buckets)\n\n");
        out.push_str("| window | docs | nodes (Δ) | listeners (Δ) | heap(MB) (Δ) |\n");
        out.push_str("|--------|------|-----------|---------------|--------------|\n");
        for &k in &bucket_keys {
            let list = &buckets_map[&k];
            let b_first = list[0];
            let b_last = *list.last().unwrap();
            let dn = b_last.nodes - b_first.nodes;
            let dl = b_last.listeners - b_first.listeners;
            let dh = mb(b_last.heap - b_first.heap);
            let w_start = k as f64 * 5.0;
            let w_end = w_start + 5.0;
            out.push_str(&format!(
                "| {:.0}–{:.0}s | {:.0} → {:.0} | {:.0} → {:.0} ({:+}) | {:.0} → {:.0} ({:+}) | {:.1} → {:.1} ({:+.1}) |\n",
                w_start,
                w_end,
                b_first.documents,
                b_last.documents,
                b_first.nodes,
                b_last.nodes,
                dn as i64,
                b_first.listeners,
                b_last.listeners,
                dl as i64,
                mb(b_first.heap),
                mb(b_last.heap),
                dh,
            ));
            json_buckets.push(json!({
                "window": format!("{:.0}-{:.0}s", w_start, w_end),
                "docs_start": b_first.documents,
                "docs_end": b_last.documents,
                "nodes_delta": dn,
                "listeners_delta": dl,
                "heap_delta_mb": (dh * 100.0).round() / 100.0,
            }));
        }
        out.push('\n');
    }

    out.push_str("| t(ms) | heap(MB) | nodes | docs | listeners |\n");
    out.push_str("|-------|----------|-------|------|-----------|\n");
    let shown = top.max(1);
    for idx in downsample_indices(n, shown) {
        let s = samples[idx];
        out.push_str(&format!(
            "| {:.2} | {:.1} | {:.0} | {:.0} | {:.0} |\n",
            (s.ts - min_ts) / 1000.0,
            mb(s.heap),
            s.nodes,
            s.documents,
            s.listeners,
        ));
    }
    if n > shown {
        out.push_str(&format!(
            "\n_Showing {} of {} samples (use --top to see more)._{}",
            shown, n, '\n'
        ));
    }
    out.push('\n');

    let json_rows: Vec<Value> = samples
        .iter()
        .map(|s| {
            json!({
                "t_us": (s.ts - min_ts).round(),
                "heap_mb": s.heap / 1_048_576.0,
                "nodes": s.nodes,
                "documents": s.documents,
                "listeners": s.listeners,
            })
        })
        .collect();

    let json_change_points: Vec<Value> = change_points
        .iter()
        .map(|cp| {
            json!({
                "t_us": (cp.ts - min_ts).round(),
                "docs": cp.docs,
                "delta_docs": cp.delta_docs,
                "nodes": cp.nodes,
                "delta_nodes": cp.delta_nodes,
                "listeners": cp.listeners,
                "delta_listeners": cp.delta_listeners,
                "heap_mb": (mb(cp.heap) * 100.0).round() / 100.0,
                "delta_heap_mb": (mb(cp.delta_heap) * 100.0).round() / 100.0,
            })
        })
        .collect();

    let summary = json!({
        "samples": n,
        "first_heap_mb": mb(first.heap),
        "last_heap_mb": mb(last.heap),
        "growth_mb": mb(last.heap - first.heap),
        "peak_heap_mb": mb(peak.heap),
        "peak_heap_t_us": (peak.ts - min_ts).round(),
        "peak_nodes": peak_nodes,
        "peak_documents": peak_documents,
        "peak_listeners": peak_listeners,
        "growth_velocity": {
            "span_s": (span_s * 10.0).round() / 10.0,
            "heap_mb_per_s": (heap_rate * 100.0).round() / 100.0,
            "nodes_per_s": (nodes_rate * 10.0).round() / 10.0,
            "listeners_per_s": (listeners_rate * 10.0).round() / 10.0,
        },
    });
    (out, json!({
        "summary": summary,
        "samples": json_rows,
        "change_points": json_change_points,
        "buckets": json_buckets,
    }))
}

fn event_type_category(ty: &str) -> &'static str {
    if ty.starts_with("pointer")
        || ty.starts_with("mouse")
        || ty.starts_with("key")
        || ty.starts_with("touch")
        || matches!(
            ty,
            "click"
                | "dblclick"
                | "auxclick"
                | "contextmenu"
                | "wheel"
                | "select"
                | "submit"
                | "input"
                | "beforeinput"
                | "compositionstart"
                | "compositionupdate"
                | "compositionend"
        )
    {
        "Input"
    } else if matches!(
        ty,
        "seeking"
            | "seeked"
            | "waiting"
            | "timeupdate"
            | "canplay"
            | "canplaythrough"
            | "playing"
            | "play"
            | "pause"
            | "ended"
            | "loadeddata"
            | "loadedmetadata"
            | "durationchange"
            | "volumechange"
            | "ratechange"
            | "progress"
            | "emptied"
            | "stalled"
            | "suspend"
            | "cuechange"
    ) {
        "Media"
    } else if matches!(
        ty,
        "resize"
            | "scroll"
            | "scrollend"
            | "focus"
            | "blur"
            | "focusin"
            | "focusout"
            | "load"
            | "unload"
            | "beforeunload"
            | "DOMContentLoaded"
            | "DOMActivate"
            | "DOMFocusIn"
            | "DOMFocusOut"
            | "visibilitychange"
            | "readystatechange"
    ) {
        "DOM"
    } else {
        "Other"
    }
}

/// Input latency: `EventDispatch` (phase `X`) events by input type
/// (pointer/mouse/key/…), with per-type duration percentiles, categories, cadence and worst events.
pub fn input_section(
    events: &[TraceEvent],
    scope: &Scope,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    let mut by_type_durs: rustc_hash::FxHashMap<String, Vec<f64>> = rustc_hash::FxHashMap::default();
    let mut by_type_ts: rustc_hash::FxHashMap<String, Vec<f64>> = rustc_hash::FxHashMap::default();
    let mut worst: Vec<(String, f64, f64)> = Vec::new(); // (type, ts, dur)

    let mut t_min = f64::INFINITY;
    let mut t_max = 0.0f64;
    for e in events {
        if !crate::trace::is_metadata_event(e) {
            t_min = t_min.min(e.ts);
            t_max = t_max.max(e.ts);
        }
    }
    let five_s_us = 5_000_000.0;

    for e in events {
        if e.name != "EventDispatch" || e.ph != b'X' || !scope.allows_event(e) {
            continue;
        }
        let d = e.dur.unwrap_or(0.0);
        let ty = e
            .args_value()
            .and_then(|a| a.get("data"))
            .and_then(|d| d.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)")
            .to_string();
        by_type_durs.entry(ty.clone()).or_default().push(d);
        by_type_ts.entry(ty.clone()).or_default().push(e.ts);
        worst.push((ty, e.ts, d));
    }
    worst.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
    let total_events = worst.len();

    struct TypeSummary {
        ty: String,
        cat: &'static str,
        count: usize,
        total: f64,
        avg: f64,
        p50: f64,
        p99: f64,
        max: f64,
        p50_gap: f64,
        hz: f64,
        first_5s: usize,
        last_5s: usize,
    }

    let mut rows: Vec<TypeSummary> = by_type_durs
        .into_iter()
        .map(|(ty, mut durs)| {
            durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let count = durs.len();
            let total: f64 = durs.iter().sum();
            let avg = total / count as f64;
            let p50 = percentile(&durs, 50.0);
            let p99 = percentile(&durs, 99.0);
            let max = *durs.last().unwrap();
            let cat = event_type_category(&ty);
            let ts_list = by_type_ts.get(&ty).unwrap();
            let first_5s = ts_list.iter().filter(|&&t| t < t_min + five_s_us).count();
            let last_5s = ts_list.iter().filter(|&&t| t > t_max - five_s_us).count();
            let (p50_gap, hz) = if ts_list.len() >= 2 {
                let mut sorted_ts = ts_list.clone();
                sorted_ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let mut gaps = (1..sorted_ts.len()).map(|i| (sorted_ts[i] - sorted_ts[i-1]).max(0.0)).collect::<Vec<_>>();
                gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let med = percentile(&gaps, 50.0) / 1000.0;
                let h = if med > 0.0 { 1000.0 / med } else { 0.0 };
                (med, h)
            } else {
                (0.0, 0.0)
            };
            TypeSummary {
                ty,
                cat,
                count,
                total,
                avg,
                p50,
                p99,
                max,
                p50_gap,
                hz,
                first_5s,
                last_5s,
            }
        })
        .collect();
    rows.sort_by(|a, b| b.total.partial_cmp(&a.total).unwrap());

    let mut out = String::new();
    out.push_str(&format!(
        "## Input latency (EventDispatch) — {} events, {} types, {}\n\n",
        total_events,
        rows.len(),
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
        out.push('\n');
    }
    if rows.is_empty() {
        out.push_str("No EventDispatch events.\n\n");
        return (out, json!({"types": [], "worst": []}));
    }

    out.push_str("| type | category | count | cadence | total(ms) | avg(ms) | p50(ms) | p99(ms) | max(ms) | first 5s | last 5s |\n");
    out.push_str("|------|----------|-------|---------|-----------|---------|---------|---------|---------|----------|---------|\n");
    let type_rows: Vec<Value> = rows
        .iter()
        .map(|r| {
            let cad_str = if r.hz >= 0.5 {
                format!("{:.1} Hz", r.hz)
            } else if r.p50_gap > 0.0 {
                format!("{:.0} ms", r.p50_gap)
            } else {
                "—".to_string()
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                r.ty,
                r.cat,
                r.count,
                cad_str,
                fmt_ms(r.total),
                fmt_ms(r.avg),
                fmt_ms(r.p50),
                fmt_ms(r.p99),
                fmt_ms(r.max),
                r.first_5s,
                r.last_5s,
            ));
            json!({
                "type": r.ty,
                "category": r.cat,
                "count": r.count,
                "cadence_hz": (r.hz * 10.0).round() / 10.0,
                "median_gap_ms": (r.p50_gap * 100.0).round() / 100.0,
                "total_us": r.total.round(),
                "avg_us": r.avg.round(),
                "p50_us": r.p50.round(),
                "p99_us": r.p99.round(),
                "max_us": r.max.round(),
                "first_5s": r.first_5s,
                "last_5s": r.last_5s,
            })
        })
        .collect();
    out.push('\n');

    out.push_str("### Worst inputs\n\n");
    out.push_str("| # | t(ms) | dur(ms) | type |\n");
    out.push_str("|---|-------|---------|------|\n");
    let worst_rows: Vec<Value> = worst
        .iter()
        .take(top)
        .enumerate()
        .map(|(i, (ty, ts, d))| {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                i + 1,
                fmt_ms(*ts - min_ts),
                fmt_ms(*d),
                ty,
            ));
            json!({
                "t_us": (ts - min_ts).round(),
                "dur_us": d.round(),
                "type": ty,
            })
        })
        .collect();
    out.push('\n');
    (out, json!({"types": type_rows, "worst": worst_rows}))
}

/// Browser frame and document hierarchy extracted from `TracingStartedInBrowser`.
pub fn frame_tree_section(
    events: &[TraceEvent],
    frames_meta: Option<&[crate::trace::FrameInfo]>,
    _min_ts: f64,
) -> (String, Value) {
    let extracted_frames;
    let frames: &[crate::trace::FrameInfo] = match frames_meta {
        Some(f) if !f.is_empty() => f,
        _ => {
            extracted_frames = extract_frames_from_events(events);
            &extracted_frames
        }
    };

    let mut out = String::new();
    let total = frames.len();
    out.push_str(&format!(
        "## Frame / Document Tree (TracingStartedInBrowser) — {} frame{}\n\n",
        total,
        if total == 1 { "" } else { "s" },
    ));

    if frames.is_empty() {
        out.push_str("No TracingStartedInBrowser frame tree found in trace.\n\n");
        return (out, json!({"total_frames": 0, "frames": [], "processes": []}));
    }

    let mut id_map: rustc_hash::FxHashMap<&str, &crate::trace::FrameInfo> = rustc_hash::FxHashMap::default();
    let mut children_map: rustc_hash::FxHashMap<&str, Vec<&crate::trace::FrameInfo>> = rustc_hash::FxHashMap::default();
    let mut processes: rustc_hash::FxHashMap<u64, (usize, String)> = rustc_hash::FxHashMap::default();

    for f in frames {
        id_map.insert(&f.id, f);
        let entry = processes.entry(f.process_id).or_insert((0, f.url.clone()));
        entry.0 += 1;
        if entry.1.is_empty() && !f.url.is_empty() {
            entry.1 = f.url.clone();
        }
    }

    for f in frames {
        if let Some(ref p_id) = f.parent_id
            && id_map.contains_key(p_id.as_str())
        {
            children_map.entry(p_id.as_str()).or_default().push(f);
        }
    }

    fn render_frame(
        f: &crate::trace::FrameInfo,
        depth: usize,
        children_map: &rustc_hash::FxHashMap<&str, Vec<&crate::trace::FrameInfo>>,
        out: &mut String,
        rendered: &mut rustc_hash::FxHashSet<String>,
    ) {
        rendered.insert(f.id.clone());
        let indent = "  ".repeat(depth);
        let flags = if f.is_main_frame { " [main]" } else { "" };
        let name_str = if !f.name.is_empty() { format!(" ({})", f.name) } else { String::new() };
        let url_display = if f.url.is_empty() { "(empty url)" } else { &f.url };
        out.push_str(&format!(
            "{}- **`{}`**{}{} · pid {} (frame `{}`)\n",
            indent,
            url_display,
            flags,
            name_str,
            f.process_id,
            f.id,
        ));
        if let Some(children) = children_map.get(f.id.as_str()) {
            for child in children {
                render_frame(child, depth + 1, children_map, out, rendered);
            }
        }
    }

    let mut rendered = rustc_hash::FxHashSet::default();
    // Roots: no parent, or parent not in id_map
    for f in frames {
        let is_root = match &f.parent_id {
            None => true,
            Some(pid) => !id_map.contains_key(pid.as_str()),
        };
        if is_root {
            render_frame(f, 0, &children_map, &mut out, &mut rendered);
        }
    }
    // Any remaining (e.g. cycle or orphan)
    for f in frames {
        if !rendered.contains(&f.id) {
            render_frame(f, 0, &children_map, &mut out, &mut rendered);
        }
    }
    out.push('\n');

    // Process summary
    let mut procs_vec: Vec<(u64, usize, String)> = processes.into_iter().map(|(pid, (cnt, url))| (pid, cnt, url)).collect();
    procs_vec.sort_by_key(|a| std::cmp::Reverse(a.1));

    out.push_str(&format!("### Frame Processes ({} distinct pid{})\n\n", procs_vec.len(), if procs_vec.len() == 1 { "" } else { "s" }));
    out.push_str("| pid | frames | primary url |\n");
    out.push_str("|-----|--------|-------------|\n");
    let mut json_procs = Vec::new();
    for (pid, cnt, url) in &procs_vec {
        out.push_str(&format!("| {} | {} | {} |\n", pid, cnt, url));
        json_procs.push(json!({
            "process_id": pid,
            "frames": cnt,
            "primary_url": url,
        }));
    }
    out.push('\n');

    let json_frames: Vec<Value> = frames
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "parent_id": f.parent_id,
                "process_id": f.process_id,
                "url": f.url,
                "name": f.name,
                "is_main_frame": f.is_main_frame,
            })
        })
        .collect();

    (out, json!({
        "total_frames": total,
        "frames": json_frames,
        "processes": json_procs,
    }))
}

/// Helper to parse frame objects directly from `TracingStartedInBrowser`.
pub fn extract_frames_from_events(events: &[TraceEvent]) -> Vec<crate::trace::FrameInfo> {
    for e in events {
        if e.name == "TracingStartedInBrowser" {
            if let Some(args) = e.args_value()
                && let Some(frames) = args
                    .get("data")
                    .and_then(|d| d.get("frames"))
                    .and_then(|f| f.as_array())
            {
                let mut res = Vec::new();
                for frame in frames {
                    let id = frame.get("frame").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let parent_id = frame.get("parent").and_then(|v| v.as_str()).map(|s| s.to_string());
                    let process_id = frame.get("processId").and_then(|v| v.as_u64()).unwrap_or(0);
                    let url = frame.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = frame.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let is_main_frame = frame.get("isInPrimaryMainFrame").and_then(|v| v.as_bool()).unwrap_or(false)
                        || frame.get("isOutermostMainFrame").and_then(|v| v.as_bool()).unwrap_or(false);
                    res.push(crate::trace::FrameInfo {
                        id,
                        parent_id,
                        process_id,
                        url,
                        name,
                        is_main_frame,
                    });
                }
                return res;
            }
            break;
        }
    }
    Vec::new()
}

/// Async task timings: pair `s` (start) and `f` (finish) events by `(pid, id)`
/// and report per-name duration percentiles (RAF, GC jobs, …) plus the longest
/// individual tasks. A task is counted when its start is inside the scope.
pub fn async_section(
    events: &[TraceEvent],
    scope: &Scope,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    // Single sweep in event order: `s` overwrites any pending start for its
    // (pid, id), `f` consumes it. Chrome recycles flow ids, so entries must
    // not survive their finish — a two-pass or_insert kept the FIRST start
    // and reused ids measured against a stale begin.
    let mut starts: rustc_hash::FxHashMap<(u64, u64), (&'static str, f64)> = rustc_hash::FxHashMap::default();
    let mut by_name: rustc_hash::FxHashMap<&'static str, Vec<f64>> = rustc_hash::FxHashMap::default();
    let mut longest: Vec<(&'static str, f64, f64)> = Vec::new(); // (name, start ts, dur)
    for e in events {
        if !e.has_id {
            continue;
        }
        if e.ph == b's' {
            if scope.allows_event(e) {
                starts.insert((e.pid, e.id), (e.name, e.ts));
            }
        } else if e.ph == b'f'
            && let Some((name, start_ts)) = starts.remove(&(e.pid, e.id))
        {
            let dur = (e.ts - start_ts).max(0.0);
            by_name.entry(name).or_default().push(dur);
            longest.push((name, start_ts, dur));
        }
    }
    longest.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
    let total_tasks = longest.len();

    let mut rows: Vec<(&'static str, usize, f64, f64, f64, f64, f64)> = by_name
        .into_iter()
        .map(|(name, mut durs)| {
            durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let count = durs.len();
            let total: f64 = durs.iter().sum();
            let avg = total / count as f64;
            let p50 = percentile(&durs, 50.0);
            let p99 = percentile(&durs, 99.0);
            let max = *durs.last().unwrap();
            (name, count, total, avg, p50, p99, max)
        })
        .collect();
    rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

    let mut out = String::new();
    out.push_str(&format!(
        "## Async tasks (s/f paired) — {} tasks, {} names, {}\n\n",
        total_tasks,
        rows.len(),
        window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
        out.push('\n');
    }
    if rows.is_empty() {
        out.push_str("No async s/f pairs found.\n\n");
        return (out, json!({"tasks": [], "longest": []}));
    }

    out.push_str("| name | count | total(ms) | avg(ms) | p50(ms) | p99(ms) | max(ms) |\n");
    out.push_str("|------|-------|-----------|---------|---------|---------|---------|\n");
    let task_rows: Vec<Value> = rows
        .iter()
        .map(|(name, count, total, avg, p50, p99, max)| {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                name,
                count,
                fmt_ms(*total),
                fmt_ms(*avg),
                fmt_ms(*p50),
                fmt_ms(*p99),
                fmt_ms(*max),
            ));
            json!({
                "name": name,
                "count": count,
                "total_us": total.round(),
                "avg_us": avg.round(),
                "p50_us": p50.round(),
                "p99_us": p99.round(),
                "max_us": max.round(),
            })
        })
        .collect();
    out.push('\n');

    out.push_str("### Longest tasks\n\n");
    out.push_str("| # | t(ms) | dur(ms) | name |\n");
    out.push_str("|---|-------|---------|------|\n");
    let longest_rows: Vec<Value> = longest
        .iter()
        .take(top)
        .enumerate()
        .map(|(i, (name, start_ts, d))| {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                i + 1,
                fmt_ms(*start_ts - min_ts),
                fmt_ms(*d),
                name,
            ));
            json!({
                "t_us": (start_ts - min_ts).round(),
                "dur_us": d.round(),
                "name": name,
            })
        })
        .collect();
    out.push('\n');
    (out, json!({"tasks": task_rows, "longest": longest_rows}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::TraceEvent;

    fn ev(ts: f64, name: &str, cat: Option<&str>) -> TraceEvent {
        TraceEvent {
            name: crate::trace::intern_name(name),
            id: 0,
            has_id: false,
            ph: b'X',
            ts,
            dur: None,
            tid: 1,
            pid: 1,
            cat: cat.map(crate::trace::intern_name),
            args: None,
            args_cache: std::sync::OnceLock::new(),
        }
    }

    fn flow(ph: u8, id: u64, ts: f64) -> TraceEvent {
        TraceEvent {
            name: crate::trace::intern_name("AnimationFrame"),
            id,
            has_id: true,
            ph,
            ts,
            dur: None,
            tid: 1,
            pid: 1,
            cat: None,
            args: None,
            args_cache: std::sync::OnceLock::new(),
        }
    }

    #[test]
    fn async_pairs_consume_reused_ids() {
        // Chrome recycles flow ids: the second s/f pair with the same id must
        // measure against ITS start, not the first one (30ms total was the
        // stale pairing; the truth is 10ms + 1ms).
        let events = vec![
            flow(b's', 7, 1_000.0),
            flow(b'f', 7, 11_000.0),
            flow(b's', 7, 20_000.0),
            flow(b'f', 7, 21_000.0),
        ];
        let scope = Scope { window: None, tid: None, pid: None, cat: None };
        let (_, json) = async_section(&events, &scope, 10, 0.0);
        let row = &json["tasks"][0];
        assert_eq!(row["count"], 2);
        assert_eq!(row["max_us"], 10_000.0);
        assert_eq!(row["total_us"], 11_000.0);
    }

    #[test]
    fn snippet_handles_multibyte_before_match() {
        // idx is a byte offset; slicing a char vec with it panicked (or
        // rendered the wrong span) once multibyte text preceded the match.
        let s = format!("{{\"t\":\"{}needle here\"}}", "日".repeat(50));
        let idx = s.find("needle").unwrap();
        let snip = snippet_around(&s, idx, 6);
        assert!(snip.contains("needle"), "snippet: {snip}");
    }

    #[test]
    fn trace_start_skips_metadata_events() {
        // Regression: thread_name etc. carry process-start ts (here 0) and
        // used to anchor every window into dead time.
        let events = vec![
            ev(0.0, "thread_name", Some("__metadata")),
            ev(0.0, "process_name", Some("__metadata")),
            ev(3_100_000_000.0, "RunTask", Some("devtools.timeline")),
            ev(3_100_100_000.0, "Paint", None),
        ];
        assert_eq!(trace_start_us(&events), 3_100_000_000.0);
    }

    /// Full-field event builder for the memory / input / async sections.
    #[allow(clippy::too_many_arguments)]
    fn evx(
        ts: f64,
        name: &str,
        ph: u8,
        dur: Option<f64>,
        id: u64,
        has_id: bool,
        args: Option<serde_json::Value>,
    ) -> TraceEvent {
        TraceEvent {
            name: crate::trace::intern_name(name),
            id,
            has_id,
            ph,
            ts,
            dur,
            tid: 1,
            pid: 1,
            cat: None,
            args: args.and_then(crate::trace::test_args),
            args_cache: std::sync::OnceLock::new(),
        }
    }

    #[test]
    fn memory_section_reads_update_counters() {
        let events = vec![
            evx(
                1_000.0,
                "UpdateCounters",
                b'I',
                None,
                0,
                false,
                Some(json!({"data": {"jsHeapSizeUsed": 1_048_576.0, "nodes": 10.0, "documents": 2.0, "jsEventListeners": 5.0}})),
            ),
            evx(
                2_000.0,
                "UpdateCounters",
                b'I',
                None,
                0,
                false,
                Some(json!({"data": {"jsHeapSizeUsed": 2_097_152.0, "nodes": 20.0, "documents": 2.0, "jsEventListeners": 8.0}})),
            ),
            evx(1_500.0, "RunTask", b'X', Some(10.0), 0, false, None),
        ];
        let scope = Scope { window: None, tid: None, pid: None, cat: None };
        let (md, j) = memory_section(&events, &scope, 10, 0.0);
        assert!(md.contains("2 samples"), "{}", md);
        assert!(md.contains("1.0 MB → 2.0 MB (+1.0 MB)"), "{}", md);
        assert!(md.contains("peak 2.0 MB"), "{}", md);
        assert!(md.contains("**DOM nodes** peak 20"), "{}", md);
        assert_eq!(j["summary"]["samples"], 2);
        assert_eq!(j["summary"]["growth_mb"], 1.0);
        assert_eq!(j["samples"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn input_section_groups_by_type_with_percentiles() {
        let dispatch = |ty: &str, dur: f64| {
            evx(1_000.0, "EventDispatch", b'X', Some(dur), 0, false, Some(json!({"data": {"type": ty}})))
        };
        let events = vec![
            dispatch("pointerdown", 1_000.0),
            dispatch("pointerdown", 2_000.0),
            dispatch("pointerdown", 4_000.0),
            dispatch("mousemove", 100.0),
            evx(500.0, "RunTask", b'X', Some(9_999.0), 0, false, None),
        ];
        let scope = Scope { window: None, tid: None, pid: None, cat: None };
        let (md, j) = input_section(&events, &scope, 5, 0.0);
        assert!(md.contains("4 events, 2 types"), "{}", md);
        let types = j["types"].as_array().unwrap();
        assert_eq!(types.len(), 2);
        // pointerdown total 7ms beats mousemove 0.1ms → sorted first.
        assert_eq!(types[0]["type"], "pointerdown");
        assert_eq!(types[0]["count"], 3);
        assert_eq!(types[0]["max_us"], 4_000.0);
        assert_eq!(types[0]["p50_us"], 2_000.0);
        // worst event is the 4ms pointerdown.
        assert_eq!(j["worst"].as_array().unwrap()[0]["dur_us"], 4_000.0);
    }

    #[test]
    fn async_section_pairs_s_f_by_id() {
        let events = vec![
            evx(1_000.0, "AnimationFrame", b's', None, 1, true, None),
            evx(5_000.0, "AnimationFrame", b'f', None, 1, true, None),
            evx(2_000.0, "AnimationFrame", b's', None, 2, true, None),
            evx(3_000.0, "AnimationFrame", b'f', None, 2, true, None),
            // start without a finish (id 3): must not be counted.
            evx(6_000.0, "AnimationFrame", b's', None, 3, true, None),
            // start without an id: skipped by pairing.
            evx(7_000.0, "AnimationFrame", b's', None, 0, false, None),
            evx(8_000.0, "AnimationFrame", b'f', None, 0, false, None),
        ];
        let scope = Scope { window: None, tid: None, pid: None, cat: None };
        let (md, j) = async_section(&events, &scope, 5, 0.0);
        assert!(md.contains("2 tasks, 1 names"), "{}", md);
        let tasks = j["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["name"], "AnimationFrame");
        assert_eq!(tasks[0]["count"], 2);
        assert_eq!(tasks[0]["total_us"], 5_000.0); // 4ms + 1ms
        assert_eq!(tasks[0]["max_us"], 4_000.0);
        // longest first: the 4ms task.
        assert_eq!(j["longest"].as_array().unwrap()[0]["dur_us"], 4_000.0);
    }

    #[test]
    fn worst_runtask_and_task_section_disregard_profiler_overhead() {
        let events = vec![
            evx(1_000_000.0, "RunTask", b'X', Some(1_079_000.0), 0, false, None),
            evx(1_001_000.0, "CpuProfiler::StartProfiling", b'X', Some(1_076_000.0), 0, false, None),
            evx(3_000_000.0, "RunTask", b'X', Some(80_000.0), 0, false, None),
        ];
        let scope = Scope { window: None, tid: None, pid: None, cat: None };

        // worst_runtask should pick the 80ms task, not the profiler startup task
        let worst = worst_runtask(&events, &scope);
        assert_eq!(worst, Some((3_000_000.0, 80_000.0)));

        // task_section should rank the 80ms task #1, and report profiler overhead on the 1079ms task
        let (md, j) = task_section(&events, &scope, 5, 0.0);
        let rows = j.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["dur_us"], 80_000.0);
        assert_eq!(rows[1]["dur_us"], 3_000.0);
        assert_eq!(rows[1]["raw_dur_us"], 1_079_000.0);
        assert_eq!(rows[1]["profiler_overhead_us"], 1_076_000.0);
        assert!(md.contains("profiler overhead 1076.00ms"));
    }

    #[test]
    fn gaps_section_detects_cadence_and_periodicity() {
        let dispatch = |ty: &str, ts: f64| {
            evx(ts, "EventDispatch", b'X', Some(10.0), 0, false, Some(json!({"data": {"type": ty}})))
        };
        // 5 seeking events spaced 64ms apart (64_000 µs)
        let events = vec![
            dispatch("seeking", 100_000.0),
            dispatch("seeking", 164_000.0),
            dispatch("seeking", 228_000.0),
            dispatch("seeking", 292_000.0),
            dispatch("seeking", 356_000.0),
        ];
        let scope = Scope { window: None, tid: None, pid: None, cat: None };
        let filter = NameFilter::new(&["seeking".to_string()], false).unwrap();
        let (md, j) = gaps_section(&events, &filter, "seeking", &scope, 0.0, 10, 100_000.0);

        assert!(md.contains("5 events, 4 gaps"), "{}", md);
        assert!(md.contains("64.00 ms (15.6 Hz)"), "{}", md);
        assert_eq!(j["events"], 5);
        assert_eq!(j["gaps_count"], 4);
        assert_eq!(j["median_gap_ms"], 64.0);
        assert_eq!(j["cadence_hz"], 15.6);
        assert_eq!(j["min_gap_ms"], 64.0);
        assert_eq!(j["max_gap_ms"], 64.0);
        assert_eq!(j["sample_sequence"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn frame_tree_section_renders_hierarchy() {
        let events = vec![
            evx(
                0.0,
                "TracingStartedInBrowser",
                b'I',
                None,
                0,
                false,
                Some(json!({
                    "data": {
                        "frames": [
                            {
                                "frame": "ROOT_FRAME",
                                "url": "https://example.com/app",
                                "processId": 1234,
                                "isInPrimaryMainFrame": true
                            },
                            {
                                "frame": "CHILD_SVG",
                                "parent": "ROOT_FRAME",
                                "url": "https://example.com/icon.svg",
                                "processId": 1234
                            }
                        ]
                    }
                })),
            ),
        ];
        let (md, j) = frame_tree_section(&events, None, 0.0);
        assert!(md.contains("2 frames"), "{}", md);
        assert!(md.contains("https://example.com/app"), "{}", md);
        assert!(md.contains("https://example.com/icon.svg"), "{}", md);
        assert_eq!(j["total_frames"], 2);
        assert_eq!(j["processes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn memory_section_detects_significant_jumps_and_velocity() {
        let events = vec![
            evx(
                1_000_000.0,
                "UpdateCounters",
                b'I',
                None,
                0,
                false,
                Some(json!({"data": {"jsHeapSizeUsed": 10_000_000.0, "nodes": 100.0, "documents": 1.0, "jsEventListeners": 10.0}})),
            ),
            evx(
                2_000_000.0,
                "UpdateCounters",
                b'I',
                None,
                0,
                false,
                // +50 listeners, +80 nodes, +1 document -> significant change point!
                Some(json!({"data": {"jsHeapSizeUsed": 12_000_000.0, "nodes": 180.0, "documents": 2.0, "jsEventListeners": 60.0}})),
            ),
        ];
        let scope = Scope { window: None, tid: None, pid: None, cat: None };
        let (md, j) = memory_section(&events, &scope, 10, 1_000_000.0);

        assert!(md.contains("Significant change points (jumps)"), "{}", md);
        assert!(md.contains("Growth velocity"), "{}", md);
        assert_eq!(j["change_points"].as_array().unwrap().len(), 1);
        assert_eq!(j["change_points"][0]["delta_docs"], 1.0);
        assert_eq!(j["change_points"][0]["delta_nodes"], 80.0);
        assert_eq!(j["change_points"][0]["delta_listeners"], 50.0);
    }

    #[test]
    fn input_section_categorizes_and_measures_cadence() {
        let dispatch = |ty: &str, ts: f64| {
            evx(ts, "EventDispatch", b'X', Some(1.0), 0, false, Some(json!({"data": {"type": ty}})))
        };
        let events = vec![
            dispatch("pointerdown", 1_000_000.0),
            dispatch("seeking", 2_000_000.0),
            dispatch("seeking", 2_050_000.0),
            dispatch("resize", 3_000_000.0),
        ];
        let scope = Scope { window: None, tid: None, pid: None, cat: None };
        let (md, j) = input_section(&events, &scope, 10, 1_000_000.0);

        assert!(md.contains("Input"), "{}", md);
        assert!(md.contains("Media"), "{}", md);
        assert!(md.contains("DOM"), "{}", md);
        let types = j["types"].as_array().unwrap();
        let seeking_row = types.iter().find(|r| r["type"] == "seeking").unwrap();
        assert_eq!(seeking_row["category"], "Media");
        assert_eq!(seeking_row["count"], 2);
        assert_eq!(seeking_row["median_gap_ms"], 50.0);
        assert_eq!(seeking_row["cadence_hz"], 20.0);
    }
}

