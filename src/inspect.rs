//! Granular trace inspection: filter events by name/category/thread/process/
//! time-window, aggregate CPU samples by function and by call stack, inspect
//! durations, render a busy timeline, and search event args.
//!
//! Output is Markdown so it can be piped to an LLM, mirroring `--export`.

use crate::trace::TraceEvent;
use serde_json::Value;
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
            if full {
                truncate(&s, 2000)
            } else {
                truncate(&s, 160)
            }
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
            Matcher::Substr(p) => s.to_lowercase().contains(p),
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

// ── Inspectors ──

/// List events matching the filter, scoped, filtered by min duration, sorted.
pub fn events_md(
    events: &[TraceEvent],
    filter: &NameFilter,
    display: &str,
    scope: &Scope,
    min_dur_us: f64,
    sort: Sort,
    full_args: bool,
    top: usize,
    min_ts: f64,
) -> String {
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
        Sort::Count => {} // not meaningful for a per-event listing
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

    if total == 0 {
        out.push_str("No matching events.\n\n");
        return out;
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

/// Duration distribution per matched event name: count/total/min/avg/p50/p90/p99/max.
pub fn stats_md(
    events: &[TraceEvent],
    filter: &NameFilter,
    display: &str,
    scope: &Scope,
    min_dur_us: f64,
    min_ts: f64,
) -> String {
    let mut groups: HashMap<String, Vec<f64>> = HashMap::new();
    for e in events {
        if !filter.matches(&e.name) || !scope.allows_event(e) {
            continue;
        }
        let d = e.dur.unwrap_or(0.0);
        if d < min_dur_us {
            continue;
        }
        groups.entry(e.name.clone()).or_default().push(d);
    }

    let mut rows: Vec<(String, Vec<f64>)> = groups.into_iter().collect();
    rows.sort_by(|a, b| {
        let ta: f64 = a.1.iter().sum();
        let tb: f64 = b.1.iter().sum();
        tb.partial_cmp(&ta).unwrap()
    });

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

    if rows.is_empty() {
        out.push_str("No matching events.\n\n");
        return out;
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
    }
    out.push('\n');
    out
}

/// Aggregate CPU profile self-time for functions matching `matcher`, scoped.
pub fn functions_md(
    events: &[TraceEvent],
    matcher: &Matcher,
    scope: &Scope,
    top: usize,
    min_ts: f64,
) -> String {
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

        // Accumulate sample time only from in-scope chunks.
        if !scope.allows_event(e) {
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
    out.push_str(&format!(
        "- **Matched self-time**: {}ms\n\n",
        fmt_ms(matched_total),
    ));

    if funcs.is_empty() {
        out.push_str("No matching functions.\n\n");
        return out;
    }

    out.push_str("| # | function | self | % in scope | file |\n");
    out.push_str("|---|----------|------|------------|------|\n");
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

/// Aggregate CPU sample time per leaf node and render each leaf's full call
/// stack (root → leaf), heaviest first. Optionally filter leaves by `matcher`.
pub fn stacks_md(
    events: &[TraceEvent],
    matcher: Option<&Matcher>,
    scope: &Scope,
    top: usize,
    min_ts: f64,
) -> String {
    // node id -> (name, url, parent)
    let mut node_map: HashMap<u64, (String, String, Option<u64>)> = HashMap::new();
    let mut leaf_time: HashMap<u64, f64> = HashMap::new();

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
                let parent = node.get("parent").and_then(|v| v.as_u64());
                node_map.entry(id).or_insert((func_name, url, parent));
            }
        }

        if !scope.allows_event(e) {
            continue;
        }
        let samples = cpu_profile.get("samples").and_then(|s| s.as_array());
        let time_deltas = data.get("timeDeltas").and_then(|t| t.as_array());
        if let (Some(samples), Some(deltas)) = (samples, time_deltas) {
            for (sample, delta) in samples.iter().zip(deltas.iter()) {
                let node_id = sample.as_u64().unwrap_or(0);
                let dt = delta.as_f64().unwrap_or(0.0);
                *leaf_time.entry(node_id).or_default() += dt;
            }
        }
    }

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

    if leaves.is_empty() {
        out.push_str("No matching stacks.\n\n");
        return out;
    }

    let grand_total: f64 = leaves.iter().map(|(_, t)| *t).sum();
    for (rank, (id, t)) in leaves.iter().take(top).enumerate() {
        let (name, url, _parent) = match node_map.get(id) {
            Some(n) => (n.0.as_str().to_string(), n.1.clone(), n.2),
            None => ("(unknown)".to_string(), String::new(), None),
        };
        // Reconstruct root → leaf chain by walking parents.
        let mut chain_ids: Vec<u64> = Vec::new();
        let mut cur = Some(*id);
        while let Some(cid) = cur {
            chain_ids.push(cid);
            cur = node_map.get(&cid).and_then(|n| n.2);
        }
        chain_ids.reverse();

        let names: Vec<String> = chain_ids
            .iter()
            .map(|cid| {
                node_map
                    .get(cid)
                    .map(|(n, _, _)| {
                        if n.is_empty() {
                            "(anonymous)".to_string()
                        } else {
                            n.clone()
                        }
                    })
                    .unwrap_or_else(|| "(unknown)".to_string())
            })
            .collect();

        let depth = names.len();
        let pct = if grand_total > 0.0 { t / grand_total * 100.0 } else { 0.0 };
        let display_chain = if depth > 14 {
            let head: Vec<&str> = names[..2].iter().map(|s| s.as_str()).collect();
            let tail: Vec<&str> = names[depth - 10..].iter().map(|s| s.as_str()).collect();
            format!("{} → … → {}", head.join(" → "), tail.join(" → "))
        } else {
            names.join(" → ")
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
    }
    out
}

/// Search event `args` (JSON) for `matcher` and list matches.
pub fn find_md(
    events: &[TraceEvent],
    matcher: &Matcher,
    scope: &Scope,
    full_args: bool,
    top: usize,
    min_ts: f64,
) -> String {
    let mut matches: Vec<(&TraceEvent, String)> = Vec::new();
    let needle_label = matcher.label();
    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        let s = match &e.args {
            Some(v) => serde_json::to_string(v).unwrap_or_default(),
            None => continue,
        };
        if matcher.matches(&s) {
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

fn snippet_around(s: &str, idx: usize, len: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = idx.saturating_sub(60);
    let end = (idx + len + 60).min(chars.len());
    chars[start..end].iter().collect()
}

/// Discovery: distinct event names with count and total duration, scoped, sorted.
pub fn names_md(events: &[TraceEvent], scope: &Scope, sort: Sort, top: usize, min_ts: f64) -> String {
    let mut stats: HashMap<String, (usize, f64)> = HashMap::new();
    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        let entry = stats.entry(e.name.clone()).or_default();
        entry.0 += 1;
        entry.1 += e.dur.unwrap_or(0.0);
    }

    let mut rows: Vec<(String, usize, f64)> = stats.into_iter().map(|(n, (c, d))| (n, c, d)).collect();
    match sort {
        Sort::Count => rows.sort_by(|a, b| b.1.cmp(&a.1)),
        Sort::Name => rows.sort_by(|a, b| a.0.cmp(&b.0)),
        _ => rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap()),
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
    if total == 0 {
        out.push_str("No events.\n\n");
        return out;
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
    }
    if total > top {
        out.push_str(&format!(
            "\n_Showing {} of {} names (use --top to see more)._\n",
            top, total
        ));
    }
    out.push('\n');
    out
}

/// Discovery: distinct threads (tid) with event count, RunTask total duration,
/// and the most frequent event name. Scoped.
pub fn threads_md(events: &[TraceEvent], scope: &Scope, top: usize, min_ts: f64) -> String {
    let mut tids: HashMap<u64, (usize, f64, HashMap<String, usize>)> = HashMap::new();
    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        let entry = tids.entry(e.tid).or_default();
        entry.0 += 1;
        if e.name == "RunTask" {
            entry.1 += e.dur.unwrap_or(0.0);
        }
        *entry.2.entry(e.name.clone()).or_default() += 1;
    }

    let mut rows: Vec<(u64, usize, f64, String)> = tids
        .into_iter()
        .map(|(tid, (count, runtask, names))| {
            let top_name = names
                .into_iter()
                .max_by_key(|(_, c)| *c)
                .map(|(n, _)| n)
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
    if total == 0 {
        out.push_str("No threads.\n\n");
        return out;
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
    }
    if total > top {
        out.push_str(&format!(
            "\n_Showing {} of {} threads (use --top to see more)._\n",
            top, total
        ));
    }
    out.push('\n');
    out
}

/// Text busy-timeline: buckets the (windowed or full) trace and shows RunTask
/// duration and event count per bucket, with a proportional bar.
pub fn timeline_md(
    events: &[TraceEvent],
    scope: &Scope,
    bucket_ms: Option<f64>,
    min_ts: f64,
) -> String {
    let start = scope.window.map(|(lo, _)| lo).unwrap_or(min_ts);
    let end = scope.window.map(|(_, hi)| hi).unwrap_or_else(|| trace_end_us(events));
    if end <= start {
        return "_Empty trace range._\n\n".to_string();
    }

    let span_ms = (end - start) / 1000.0;
    let bucket_ms = bucket_ms.unwrap_or_else(|| {
        // ~40 buckets, clamped to [10ms, 500ms]
        (span_ms / 40.0).round().max(10.0).min(500.0)
    });
    let bucket_us = (bucket_ms * 1000.0).max(1.0);
    let n_buckets = ((end - start) / bucket_us).ceil() as usize;
    let n_buckets = n_buckets.max(1);
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

    let mut out = String::new();
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
    }
    out.push('\n');
    out
}

/// Find the longest RunTask (ts, dur) in scope (window ignored). Used by --worst.
pub fn worst_runtask(events: &[TraceEvent], scope: &Scope) -> Option<(f64, f64)> {
    events
        .iter()
        .filter(|e| e.name == "RunTask" && e.ph == "X" && scope.allows_event(e))
        .filter_map(|e| e.dur.map(|d| (e.ts, d)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
}
