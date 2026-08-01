//! Granular trace inspection: filter events by name/category/thread/process/
//! time-window, aggregate CPU samples by function and by call stack, inspect
//! durations, render a busy timeline, and search event args.
//!
//! Each inspector returns `(markdown, json)` built from a single aggregation
//! pass, so `--json` stays in sync with the Markdown output.

use crate::trace::TraceEvent;
use serde_json::{Value, json};
use std::collections::HashMap;

// ── Time helpers ──

/// Absolute trace start (min `ts` across events), in microseconds.
pub fn trace_start_us(events: &[TraceEvent]) -> f64 {
    events.iter().map(|e| e.ts).fold(f64::INFINITY, f64::min)
}

/// Absolute trace end (max `ts + dur`), in microseconds.
fn trace_end_us(events: &[TraceEvent]) -> f64 {
    events
        .iter()
        .fold(0.0f64, |acc, e| acc.max(e.ts + e.dur.unwrap_or(0.0)))
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

fn args_compact(args: &Option<Value>, full: bool) -> String {
    match args {
        Some(v) => {
            let s = serde_json::to_string(v).unwrap_or_default();
            if full { truncate(&s, 2000) } else { truncate(&s, 160) }
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
                .map_or(true, |c| e.cat.as_deref().is_some_and(|ec| ec.to_lowercase().contains(c)))
    }

    fn window_line(&self, min_ts: f64) -> Option<String> {
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
fn contains_ignore_case(hay: &str, needle: &str) -> bool {
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

    fn matches(&self, s: &str) -> bool {
        match self {
            Matcher::Substr(p) => contains_ignore_case(s, p),
            Matcher::Regex(re) => re.is_match(s),
        }
    }

    fn label(&self) -> String {
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

    fn matches(&self, name: &str) -> bool {
        match self {
            NameFilter::Exact(v) => v.iter().any(|n| n == name),
            NameFilter::Regex(v) => v.iter().any(|r| r.is_match(name)),
        }
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
        .filter(|e| filter.matches(&e.name))
        .filter(|e| e.dur.unwrap_or(0.0) >= min_dur_us)
        .filter(|e| scope.allows_event(e))
        .collect();
    match sort {
        Sort::Ts => rows.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap()),
        Sort::Dur => rows.sort_by(|a, b| b.dur.unwrap_or(0.0).partial_cmp(&a.dur.unwrap_or(0.0)).unwrap()),
        Sort::Name => rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.ts.partial_cmp(&b.ts).unwrap())),
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
            args_compact(&e.args, full_args),
        ));
        json_rows.push(json!({
            "t_us": (e.ts - min_ts).round(),
            "dur_us": e.dur.unwrap_or(0.0).round(),
            "name": e.name,
            "tid": e.tid,
            "pid": e.pid,
            "args": e.args.clone().unwrap_or(Value::Null),
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
    let mut groups: HashMap<&str, Vec<f64>> = HashMap::new();
    for e in events {
        if !filter.matches(&e.name) || !scope.allows_event(e) {
            continue;
        }
        let d = e.dur.unwrap_or(0.0);
        if d < min_dur_us {
            continue;
        }
        groups.entry(e.name.as_str()).or_default().push(d);
    }

    let mut rows: Vec<(&str, Vec<f64>)> = groups.into_iter().collect();
    rows.sort_by(|a, b| b.1.iter().sum::<f64>().partial_cmp(&a.1.iter().sum::<f64>()).unwrap());

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
    for (name, mut durs) in rows {
        durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let total: f64 = durs.iter().sum();
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

/// Aggregate CPU profile self-time for functions matching `matcher`, scoped.
pub fn functions_section(
    events: &[TraceEvent],
    matcher: &Matcher,
    scope: &Scope,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    let (node_map, self_times) = crate::analysis::scan_profile_chunks(events, Some(scope), 0);
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

/// node id -> (name, url, parent)
pub struct CpuProfile {
    pub nodes: HashMap<u64, (String, String, Option<u64>)>,
    pub leaf_time: HashMap<u64, f64>,
}

/// Register all CPU profile nodes, and accumulate per-leaf sample time from
/// in-scope chunks. Shared by `stacks_section` and `stacks_folded`.
pub fn collect_cpu_profile(events: &[TraceEvent], scope: &Scope) -> CpuProfile {
    let (nodes, leaf_time) = crate::analysis::scan_profile_chunks(events, Some(scope), 0);
    CpuProfile { nodes, leaf_time }
}

fn node_chain_names(nodes: &HashMap<u64, (String, String, Option<u64>)>, leaf: u64) -> Vec<String> {
    let mut ids: Vec<u64> = Vec::new();
    let mut cur = Some(leaf);
    while let Some(id) = cur {
        ids.push(id);
        cur = nodes.get(&id).and_then(|n| n.2);
    }
    ids.reverse();
    ids.iter()
        .map(|id| {
            nodes
                .get(id)
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
) -> (String, Value) {
    let cpu = collect_cpu_profile(events, scope);
    let node_map = cpu.nodes;
    let leaf_time = cpu.leaf_time;

    let mut leaves: Vec<(u64, f64)> = leaf_time
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
) -> String {
    let cpu = collect_cpu_profile(events, scope);
    let mut leaves: Vec<(u64, f64)> = cpu
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
) -> (String, Value) {
    let mut matches: Vec<(&TraceEvent, String)> = Vec::new();
    let needle_label = matcher.label();
    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        let args = match &e.args {
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
            "args": e.args.clone().unwrap_or(Value::Null),
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

fn snippet_around(s: &str, idx: usize, len: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = idx.saturating_sub(60);
    let end = (idx + len + 60).min(chars.len());
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
    let mut stats: HashMap<&str, (usize, f64)> = HashMap::new();
    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        let entry = stats.entry(e.name.as_str()).or_default();
        entry.0 += 1;
        entry.1 += e.dur.unwrap_or(0.0);
    }

    let mut rows: Vec<(String, usize, f64)> = stats
        .into_iter()
        .map(|(n, (c, d))| (n.to_string(), c, d))
        .collect();
    match sort {
        Sort::Count => rows.sort_by(|a, b| b.1.cmp(&a.1)),
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
    let mut tids: HashMap<u64, (usize, f64, HashMap<&str, usize>)> = HashMap::new();
    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        let entry = tids.entry(e.tid).or_default();
        entry.0 += 1;
        if e.name == "RunTask" {
            entry.1 += e.dur.unwrap_or(0.0);
        }
        *entry.2.entry(e.name.as_str()).or_default() += 1;
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
    let bucket_ms = bucket_ms.unwrap_or_else(|| (span_ms / 40.0).round().max(10.0).min(500.0));
    let bucket_us = (bucket_ms * 1000.0).max(1.0);
    let n_buckets = (((end - start) / bucket_us).ceil() as usize).max(1);
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
                runtask[bi] += e.dur.unwrap_or(0.0);
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
    events
        .iter()
        .filter(|e| e.name == "RunTask" && e.ph == "X" && scope.allows_event(e))
        .filter_map(|e| e.dur.map(|d| (e.ts, d)))
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
    let mut tasks: Vec<&TraceEvent> = events
        .iter()
        .filter(|e| e.name == "RunTask" && e.ph == "X" && e.dur.is_some() && scope.allows_event(e))
        .collect();
    tasks.sort_by(|a, b| b.dur.unwrap().partial_cmp(&a.dur.unwrap()).unwrap());
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

    for (rank, rt) in tasks.iter().take(top).enumerate() {
        let rt_ts = rt.ts;
        let rt_end = rt.ts + rt.dur.unwrap();
        let rt_dur = rt.dur.unwrap();

        let mut groups: HashMap<String, (usize, f64)> = HashMap::new();
        let mut top_fc: Option<(String, f64)> = None;
        for e in events {
            if e.tid != rt.tid || e.name == "RunTask" || e.ts < rt_ts || e.ts > rt_end {
                continue;
            }
            let d = e.dur.unwrap_or(0.0);
            let g = groups.entry(e.name.clone()).or_default();
            g.0 += 1;
            g.1 += d;
            if e.name == "FunctionCall" && top_fc.as_ref().map_or(true, |(_, dd)| d > *dd) {
                let fn_name = e
                    .args
                    .as_ref()
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

        out.push_str(&format!(
            "### #{}  t={:.2}ms  dur={}ms  tid={}\n\n",
            rank + 1,
            (rt_ts - min_ts) / 1000.0,
            fmt_ms(rt_dur),
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
            "dur_us": rt_dur.round(),
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
