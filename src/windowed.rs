//! Windowed analyses: semantic anchors (--anchor), per-frame statistics
//! (--frames), GC/long-task reports (--gc), the inclusive CPU call tree
//! (--calltree), the PRE/SHOOT/POST delta comparison (--delta) and CSV
//! rendering. Each inspector returns `(markdown, json)`, so `--json` and
//! `--csv` stay in sync with the Markdown output.

use crate::inspect::{Matcher, Scope};
use crate::trace::TraceEvent;
use serde_json::{Value, json};
use std::collections::HashMap;

// ── Anchor detection (--anchor) ──

/// A semantic time anchor: the first trace occurrence of a search pattern.
pub struct Anchor {
    /// Absolute timestamp (µs).
    pub ts: f64,
    /// What matched: `FunctionCall` / `cpu-profile` / `args`.
    pub kind: &'static str,
    /// The matched text (function name, URL, or JSON value).
    pub label: String,
}

fn value_match_text<'a>(v: &'a Value, matcher: &Matcher) -> Option<&'a str> {
    match v {
        Value::String(s) => matcher.matches(s).then_some(s.as_str()),
        Value::Array(a) => a.iter().find_map(|v| value_match_text(v, matcher)),
        Value::Object(m) => m
            .iter()
            .find_map(|(k, v)| {
                if matcher.matches(k) {
                    Some(k.as_str())
                } else {
                    value_match_text(v, matcher)
                }
            }),
        Value::Number(n) => {
            let s = n.to_string();
            matcher.matches(&s).then_some(Box::leak(s.into_boxed_str()))
        }
        _ => None,
    }
}

/// Find the first trace occurrence of `matcher`, by priority:
/// 1. FunctionCall `data.functionName` (actual JS executions)
/// 2. CPU profile node names / source URLs (sampled call frames)
/// 3. any event args (JSON values)
///
/// Priority matters: e.g. searching `shoot` must anchor on the first *shoot
/// call*, not on the `shooterX` script URL that shows up when modules load.
pub fn find_anchor(events: &[TraceEvent], matcher: &Matcher) -> Option<Anchor> {
    // Pass 1: FunctionCall data.functionName.
    let mut best: Option<Anchor> = None;
    for e in events {
        if e.name != "FunctionCall" || e.ph != "X" {
            continue;
        }
        let Some(fn_name) = e
            .args
            .as_ref()
            .and_then(|a| a.get("data"))
            .and_then(|d| d.get("functionName"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        if matcher.matches(fn_name) && best.as_ref().is_none_or(|b| e.ts < b.ts) {
            best = Some(Anchor {
                ts: e.ts,
                kind: "FunctionCall",
                label: fn_name.to_string(),
            });
        }
    }
    if best.is_some() {
        return best;
    }

    // Pass 2: CPU profile node names / URLs, by earliest sample time.
    // Sample times walk the chunk sequence anchored on the per-process
    // `Profile` (ph=P) event; timeDeltas are inter-sample gaps.
    let mut starts: HashMap<u64, f64> = HashMap::new();
    for e in events {
        if e.name == "Profile" && e.ph == "P" {
            starts.entry(e.pid).or_insert(e.ts);
        }
    }
    let mut node_first: HashMap<u64, (f64, String, String)> = HashMap::new();
    let mut prev_last: HashMap<u64, f64> = HashMap::new();
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
        for node in cpu_profile
            .get("nodes")
            .and_then(|n| n.as_array())
            .into_iter()
            .flatten()
        {
            let id = node.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            if id == 0 || node_first.contains_key(&id) {
                continue;
            }
            let call_frame = node.get("callFrame");
            let name = call_frame
                .and_then(|cf| cf.get("functionName"))
                .and_then(|v| v.as_str())
                .unwrap_or("(anonymous)")
                .to_string();
            let url = call_frame
                .and_then(|cf| cf.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            node_first.insert(id, (f64::INFINITY, name, url));
        }
        let samples = cpu_profile.get("samples").and_then(|s| s.as_array());
        let deltas = data.get("timeDeltas").and_then(|t| t.as_array());
        if let (Some(samples), Some(deltas)) = (samples, deltas) {
            let n = samples.len().min(deltas.len());
            let mut sample_ts = prev_last
                .get(&e.pid)
                .copied()
                .unwrap_or_else(|| starts.get(&e.pid).copied().unwrap_or(0.0));
            for (i, s) in samples.iter().enumerate().take(n) {
                let weight = deltas[i].as_f64().unwrap_or(0.0).max(0.0);
                let id = s.as_u64().unwrap_or(0);
                if id != 0 {
                    if let Some(entry) = node_first.get_mut(&id) {
                        if sample_ts < entry.0 {
                            entry.0 = sample_ts;
                        }
                    }
                }
                sample_ts += weight;
            }
            prev_last.insert(e.pid, sample_ts);
        }
    }
    // Prefer function-name matches over URL-only matches: a substring like
    // `shoot` also hits module URLs (`src/games/shooterX/...`) that appear
    // long before the first actual shoot call.
    let mut cpu_best: Option<Anchor> = None;
    for (_id, (t, name, _url)) in &node_first {
        if *t == f64::INFINITY || !matcher.matches(name) {
            continue;
        }
        if cpu_best.as_ref().is_none_or(|b| *t < b.ts) {
            cpu_best = Some(Anchor {
                ts: *t,
                kind: "cpu-profile",
                label: name.clone(),
            });
        }
    }
    if cpu_best.is_none() {
        for (_id, (t, _name, url)) in &node_first {
            if *t == f64::INFINITY || !matcher.matches(url) {
                continue;
            }
            if cpu_best.as_ref().is_none_or(|b| *t < b.ts) {
                cpu_best = Some(Anchor {
                    ts: *t,
                    kind: "cpu-profile",
                    label: url.clone(),
                });
            }
        }
    }
    if let Some(a) = cpu_best {
        return Some(a);
    }

    // Pass 3: any event args.
    let mut args_best: Option<Anchor> = None;
    for e in events {
        let Some(args) = &e.args else { continue };
        if let Some(text) = value_match_text(args, matcher) {
            if args_best.as_ref().is_none_or(|b| e.ts < b.ts) {
                args_best = Some(Anchor {
                    ts: e.ts,
                    kind: "args",
                    label: text.to_string(),
                });
            }
        }
    }
    args_best
}

// ── Frame statistics (--frames, --delta) ──

/// Pair `b`/`e` events with the given name into durations (µs). Chrome frame
/// pipeline events are traced as `b`/`e` pairs without `dur`; same-thread
/// pairing via a stack. Events outside `window` are skipped by their `b` ts.
fn paired_durations(events: &[TraceEvent], name: &str, window: Option<(f64, f64)>) -> Vec<f64> {
    let mut stacks: HashMap<u64, Vec<f64>> = HashMap::new();
    let mut durs = Vec::new();
    for e in events {
        if e.name != name {
            continue;
        }
        if let Some((lo, hi)) = window {
            if e.ts < lo || e.ts > hi {
                continue;
            }
        }
        match e.ph.as_str() {
            "b" => stacks.entry(e.tid).or_default().push(e.ts),
            "e" => {
                if let Some(st) = stacks.get_mut(&e.tid) {
                    if let Some(t) = st.pop() {
                        if e.ts > t {
                            durs.push(e.ts - t);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    durs
}

fn percentiles(sorted_asc: &[f64]) -> (f64, f64, f64, f64) {
    // (p50, p90, p99, max)
    if sorted_asc.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let p = |q: f64| -> f64 {
        let idx = ((q / 100.0) * (sorted_asc.len() - 1) as f64).round() as usize;
        sorted_asc[idx.min(sorted_asc.len() - 1)]
    };
    (p(50.0), p(90.0), p(99.0), *sorted_asc.last().unwrap())
}

/// Per-frame duration stats + dropped frames within a window.
pub fn frames_section(
    events: &[TraceEvent],
    scope: &Scope,
    frame_event: &str,
    min_ts: f64,
) -> (String, Value) {
    let mut durs = paired_durations(events, frame_event, scope.window);
    durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let dropped = events
        .iter()
        .filter(|e| e.name == "DroppedFrame" && scope.allows_event(e))
        .count();
    let n = durs.len();
    let (p50, p90, p99, max) = percentiles(&durs);
    let avg = if n > 0 { durs.iter().sum::<f64>() / n as f64 } else { 0.0 };
    let ms = |v: f64| format!("{:.2}", v / 1000.0);

    let mut out = String::new();
    out.push_str(&format!(
        "## Frames: {} ({} paired, {})\n\n",
        frame_event,
        n,
        crate::inspect::window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
    }
    out.push_str(&format!(
        "- **Dropped frames**: {}\n\n",
        dropped
    ));
    out.push_str("| count | avg(ms) | p50 | p90 | p99 | max |\n");
    out.push_str("|-------|---------|-----|-----|-----|-----|\n");
    out.push_str(&format!(
        "| {} | {} | {} | {} | {} | {} |\n",
        n,
        ms(avg),
        ms(p50),
        ms(p90),
        ms(p99),
        ms(max),
    ));
    out.push('\n');

    let row = json!({
        "frame_event": frame_event,
        "count": n,
        "avg_us": avg.round(),
        "p50_us": p50.round(),
        "p90_us": p90.round(),
        "p99_us": p99.round(),
        "max_us": max.round(),
        "dropped_frames": dropped,
    });
    (out, Value::Array(vec![row]))
}

// ── GC & Long Tasks (--gc) ──

/// GC + long-task report scoped to the window. Long tasks are main-thread
/// RunTasks ≥ `lt_ms` (default 50). GC split into major / minor / other
/// (all other V8.GC*/CppGC* X-events, mostly background work).
pub fn gc_section(
    events: &[TraceEvent],
    scope: &Scope,
    lt_ms: f64,
    min_ts: f64,
) -> (String, Value) {
    let main_tid = crate::trace::detect_main_thread(events);
    let lt_us = lt_ms * 1000.0;

    let mut groups: [(&str, usize, f64, f64); 3] = [
        ("MajorGC", 0, 0.0, 0.0),
        ("MinorGC / GCScavenger", 0, 0.0, 0.0),
        ("other V8.GC*/CppGC*", 0, 0.0, 0.0),
    ];
    let mut long_tasks: Vec<f64> = Vec::new();

    for e in events {
        if !scope.allows_event(e) {
            continue;
        }
        if e.ph != "X" {
            continue;
        }
        let d = e.dur.unwrap_or(0.0);
        let gi = match e.name.as_str() {
            "MajorGC" => Some(0),
            "MinorGC" | "V8.GCScavenger" => Some(1),
            n if n.starts_with("V8.GC") || n.starts_with("CppGC") => Some(2),
            _ => None,
        };
        if let Some(gi) = gi {
            let g = &mut groups[gi];
            g.1 += 1;
            g.2 += d;
            if d > g.3 {
                g.3 = d;
            }
        } else if e.name == "RunTask" && e.tid == main_tid && d >= lt_us {
            long_tasks.push(d);
        }
    }
    long_tasks.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let lt_total: f64 = long_tasks.iter().sum();
    let lt_max = long_tasks.first().copied().unwrap_or(0.0);

    let mut out = String::new();
    out.push_str(&format!(
        "## GC & Long Tasks ({})\n\n",
        crate::inspect::window_label(scope.window),
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
    }
    out.push_str("| group | count | total(ms) | max(ms) |\n");
    out.push_str("|-------|-------|-----------|---------|\n");
    for (name, count, total, max) in &groups {
        out.push_str(&format!(
            "| {} | {} | {:.2} | {:.2} |\n",
            name,
            count,
            total / 1000.0,
            max / 1000.0,
        ));
    }
    out.push('\n');
    out.push_str(&format!(
        "- **Long tasks ≥{}ms**: {} total, {:.1}ms combined, max {:.1}ms\n",
        lt_ms as i64,
        long_tasks.len(),
        lt_total.max(0.0) / 1000.0,
        lt_max / 1000.0,
    ));
    let top3: Vec<String> = long_tasks
        .iter()
        .take(3)
        .map(|d| format!("{:.1}ms", d / 1000.0))
        .collect();
    if !top3.is_empty() {
        out.push_str(&format!("- top: {}\n", top3.join(", ")));
    }
    out.push('\n');

    let json_groups: Vec<Value> = groups
        .iter()
        .map(|(name, count, total, max)| {
            json!({"group": name, "count": count, "total_us": total.round(), "max_us": max.round()})
        })
        .collect();
    let row = json!({
        "gc": json_groups,
        "long_tasks": {
            "threshold_us": lt_us.round(),
            "count": long_tasks.len(),
            "total_us": lt_total.round(),
            "max_us": lt_max.round(),
            "top_us": long_tasks.iter().take(3).map(|d| d.round()).collect::<Vec<f64>>(),
        },
    });
    (out, Value::Array(vec![row]))
}

// ── Inclusive CPU call tree (--calltree) ──

fn short_url(url: &str) -> &str {
    url.rfind('/').map(|i| &url[i + 1..]).unwrap_or(url)
}

/// Top-down CPU call tree with inclusive (self + subtree) time. `--function`
/// and `--url` prune to subtrees rooted at matching nodes (ancestors of a
/// match are kept so the path stays visible).
pub fn calltree_section(
    events: &[TraceEvent],
    scope: &Scope,
    name_matcher: Option<&Matcher>,
    url_matcher: Option<&Matcher>,
    top: usize,
    min_ts: f64,
) -> (String, Value) {
    let cpu = crate::inspect::collect_cpu_profile(events, scope);
    let nodes = cpu.nodes;
    let self_time = cpu.leaf_time;

    // Children map + roots.
    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut roots: Vec<u64> = Vec::new();
    for (id, (_, _, parent)) in &nodes {
        match parent {
            Some(p) if nodes.contains_key(p) => children.entry(*p).or_default().push(*id),
            _ => roots.push(*id),
        }
    }

    // Pre-order DFS (deterministic: children sorted by self-time desc).
    let mut order: Vec<u64> = Vec::new();
    let mut stack: Vec<u64> = roots.clone();
    while let Some(id) = stack.pop() {
        order.push(id);
        if let Some(ch) = children.get(&id) {
            let mut ch = ch.clone();
            ch.sort_by(|a, b| {
                self_time
                    .get(b)
                    .unwrap_or(&0.0)
                    .partial_cmp(self_time.get(a).unwrap_or(&0.0))
                    .unwrap()
            });
            for c in ch.into_iter().rev() {
                stack.push(c);
            }
        }
    }

    // Inclusive time: reverse DFS order accumulates children into parents.
    let mut inclusive: HashMap<u64, f64> = self_time.clone();
    for &id in order.iter().rev() {
        let Some(Some(p)) = nodes.get(&id).map(|n| n.2) else { continue };
        if nodes.contains_key(&p) {
            let inc = *inclusive.get(&id).unwrap_or(&0.0);
            *inclusive.entry(p).or_default() += inc;
        }
    }

    // Match semantics: AND when both filters are given.
    let matched = |n: &(String, String, Option<u64>)| -> bool {
        let name_ok = name_matcher.is_none_or(|m| m.matches(&n.0));
        let url_ok = url_matcher.is_none_or(|m| m.matches(&n.1));
        name_ok && url_ok
    };
    let has_filter = name_matcher.is_some() || url_matcher.is_some();

    // has_match: node itself or any descendant matches.
    let mut has_match: HashMap<u64, bool> = HashMap::new();
    for &id in order.iter().rev() {
        let m = matched(nodes.get(&id).unwrap());
        let kids = children
            .get(&id)
            .map(|ch| ch.iter().any(|c| has_match.get(c).copied().unwrap_or(false)))
            .unwrap_or(false);
        has_match.insert(id, m || (has_filter && kids));
    }

    let total_us: f64 = self_time.values().sum();

    // Emit pruned tree, depth-first, children by inclusive desc, top-limit.
    struct Row {
        depth: usize,
        id: u64,
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut roots_sorted = roots.clone();
    roots_sorted.sort_by(|a, b| {
        inclusive
            .get(b)
            .unwrap_or(&0.0)
            .partial_cmp(inclusive.get(a).unwrap_or(&0.0))
            .unwrap()
    });
    let mut stack: Vec<(u64, usize)> = roots_sorted.into_iter().map(|r| (r, 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        if !has_match.get(&id).copied().unwrap_or(false) {
            continue;
        }
        rows.push(Row { depth, id });
        if rows.len() >= top {
            break;
        }
        if let Some(ch) = children.get(&id) {
            let mut ch: Vec<u64> = ch
                .iter()
                .copied()
                .filter(|c| has_match.get(c).copied().unwrap_or(false))
                .collect();
            ch.sort_by(|a, b| {
                inclusive
                    .get(b)
                    .unwrap_or(&0.0)
                    .partial_cmp(inclusive.get(a).unwrap_or(&0.0))
                    .unwrap()
            });
            for c in ch.into_iter().rev() {
                stack.push((c, depth + 1));
            }
        }
    }

    let filter_desc = match (name_matcher, url_matcher) {
        (Some(n), Some(u)) => format!("{} + url~{}", n.label(), u.label()),
        (Some(n), None) => format!("~ {}", n.label()),
        (None, Some(u)) => format!("url~{}", u.label()),
        (None, None) => "all nodes".to_string(),
    };
    let mut out = String::new();
    out.push_str(&format!(
        "## CPU call tree (inclusive, {} nodes shown, {})\n\n",
        rows.len(),
        filter_desc,
    ));
    if let Some(line) = scope.window_line(min_ts) {
        out.push_str(&line);
    }
    out.push_str(&format!("- **Total sampled**: {}ms\n\n", fmt_ms(total_us)));

    let mut json_rows: Vec<Value> = Vec::new();
    if rows.is_empty() {
        out.push_str("No matching nodes.\n\n");
        return (out, Value::Array(json_rows));
    }

    out.push_str("```text\n");
    for row in &rows {
        let (name, url, _) = nodes.get(&row.id).unwrap();
        let inc = *inclusive.get(&row.id).unwrap_or(&0.0);
        let own = *self_time.get(&row.id).unwrap_or(&0.0);
        let pct = if total_us > 0.0 { inc / total_us * 100.0 } else { 0.0 };
        let label = if name.is_empty() { "(anonymous)" } else { name };
        let indent = "  ".repeat(row.depth);
        let file = if url.is_empty() {
            String::new()
        } else {
            format!("  [{}]", short_url(url))
        };
        out.push_str(&format!(
            "{}{}{}  self {:.2}ms · incl {:.2}ms ({:.1}%)\n",
            indent,
            label,
            file,
            own / 1000.0,
            inc / 1000.0,
            pct,
        ));
        json_rows.push(json!({
            "depth": row.depth,
            "function": if name.is_empty() { "(anonymous)" } else { name },
            "url": url,
            "self_us": own.round(),
            "inclusive_us": inc.round(),
            "pct": pct,
        }));
    }
    out.push_str("```\n");
    if rows.len() >= top && !has_filter {
        out.push_str(&format!("\n_Showing {} of {} nodes (use --top to see more)._", rows.len(), order.len()));
    }
    out.push('\n');
    (out, Value::Array(json_rows))
}

fn fmt_ms(us: f64) -> String {
    format!("{:.2}", us / 1000.0)
}

// ── Delta: PRE / SHOOT / POST (--delta) ──

struct WindowStats {
    frames_n: usize,
    frames: [f64; 4], // p50, p90, p99, max
    dropped: usize,
    runtask_us: f64,
    js_us: f64,
    gc_us: f64,
    gc_count: usize,
    lt_count: usize,
    lt_us: f64,
    cpu_us: f64,
    top_cpu: Vec<(String, f64)>,
}

fn window_stats(
    events: &[TraceEvent],
    window: (f64, f64),
    frame_event: &str,
    lt_ms: f64,
    main_tid: u64,
) -> WindowStats {
    let lt_us = lt_ms * 1000.0;
    let (lo, hi) = window;

    let mut runtask_us = 0.0f64;
    let mut js_us = 0.0f64;
    let mut gc_us = 0.0f64;
    let mut gc_count = 0usize;
    let mut lt_count = 0usize;
    let mut lt_us_total = 0.0f64;
    let mut dropped = 0usize;

    for e in events {
        if e.ts < lo || e.ts > hi {
            continue;
        }
        if e.ph == "X" && e.tid == main_tid {
            match e.name.as_str() {
                "RunTask" => {
                    if let Some(d) = e.dur {
                        runtask_us += d;
                        if d >= lt_us {
                            lt_count += 1;
                            lt_us_total += d;
                        }
                    }
                }
                "FunctionCall" => js_us += e.dur.unwrap_or(0.0),
                _ => {}
            }
        }
        match e.name.as_str() {
            "MajorGC" | "MinorGC" if e.ph == "X" => {
                gc_us += e.dur.unwrap_or(0.0);
                gc_count += 1;
            }
            "DroppedFrame" => dropped += 1,
            _ => {}
        }
    }

    let mut frames = paired_durations(events, frame_event, Some(window));
    frames.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let frames_n = frames.len();
    let (p50, p90, p99, max) = percentiles(&frames);

    let scope = Scope {
        window: Some(window),
        tid: None,
        pid: None,
        cat: None,
    };
    let (node_map, self_times) = crate::analysis::scan_profile_chunks(events, Some(&scope), 0);
    let cpu_us: f64 = self_times.values().sum();
    let mut top_cpu: Vec<(String, f64)> = self_times
        .iter()
        .filter_map(|(id, t)| node_map.get(id).map(|(n, _, _)| (n.clone(), *t)))
        .filter(|(n, _)| !n.is_empty())
        .collect();
    top_cpu.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    top_cpu.truncate(3);

    WindowStats {
        frames_n,
        frames: [p50, p90, p99, max],
        dropped,
        runtask_us,
        js_us,
        gc_us,
        gc_count,
        lt_count,
        lt_us: lt_us_total,
        cpu_us,
        top_cpu,
    }
}

/// Compare PRE / SHOOT / POST windows around an anchor: frame stats, dropped
/// frames, GC, long tasks, main-thread busy/JS and CPU sample totals, plus
/// SHOOT−PRE and POST−SHOOT deltas.
#[allow(clippy::too_many_arguments)]
pub fn delta_section(
    events: &[TraceEvent],
    pre: (f64, f64),
    shoot: (f64, f64),
    post: (f64, f64),
    anchor_ts: f64,
    frame_event: &str,
    lt_ms: f64,
    min_ts: f64,
) -> (String, Value) {
    let main_tid = crate::trace::detect_main_thread(events);
    let s_pre = window_stats(events, pre, frame_event, lt_ms, main_tid);
    let s_shoot = window_stats(events, shoot, frame_event, lt_ms, main_tid);
    let s_post = window_stats(events, post, frame_event, lt_ms, main_tid);

    let ms = |v: f64| format!("{:.1}", v / 1000.0);
    let dms = |a: f64, b: f64| format!("{:+.1}", (b - a) / 1000.0);
    let dcount = |a: usize, b: usize| format!("{:+}", b as i64 - a as i64);

    let mut rows: Vec<(String, String, String, String, String, String, String)> = Vec::new();
    let mut push = |metric: &str, unit: &str, pre_v: String, shoot_v: String, post_v: String, dpre: String, dpost: String| {
        rows.push((
            metric.to_string(),
            unit.to_string(),
            pre_v,
            shoot_v,
            post_v,
            dpre,
            dpost,
        ));
    };

    push(
        "frames",
        "n",
        s_pre.frames_n.to_string(),
        s_shoot.frames_n.to_string(),
        s_post.frames_n.to_string(),
        dcount(s_pre.frames_n, s_shoot.frames_n),
        dcount(s_shoot.frames_n, s_post.frames_n),
    );
    for (i, name) in ["frame p50", "frame p90", "frame p99", "frame max"].iter().enumerate() {
        push(
            name,
            "ms",
            ms(s_pre.frames[i]),
            ms(s_shoot.frames[i]),
            ms(s_post.frames[i]),
            dms(s_pre.frames[i], s_shoot.frames[i]),
            dms(s_shoot.frames[i], s_post.frames[i]),
        );
    }
    push(
        "dropped frames",
        "n",
        s_pre.dropped.to_string(),
        s_shoot.dropped.to_string(),
        s_post.dropped.to_string(),
        dcount(s_pre.dropped, s_shoot.dropped),
        dcount(s_shoot.dropped, s_post.dropped),
    );
    let lt_label = format!("long tasks ≥{:.0}ms", lt_ms);
    push(
        &lt_label,
        "n",
        s_pre.lt_count.to_string(),
        s_shoot.lt_count.to_string(),
        s_post.lt_count.to_string(),
        dcount(s_pre.lt_count, s_shoot.lt_count),
        dcount(s_shoot.lt_count, s_post.lt_count),
    );
    push(
        "long task time",
        "ms",
        ms(s_pre.lt_us),
        ms(s_shoot.lt_us),
        ms(s_post.lt_us),
        dms(s_pre.lt_us, s_shoot.lt_us),
        dms(s_shoot.lt_us, s_post.lt_us),
    );
    push(
        "main busy (RunTask)",
        "ms",
        ms(s_pre.runtask_us),
        ms(s_shoot.runtask_us),
        ms(s_post.runtask_us),
        dms(s_pre.runtask_us, s_shoot.runtask_us),
        dms(s_shoot.runtask_us, s_post.runtask_us),
    );
    push(
        "JS (FunctionCall)",
        "ms",
        ms(s_pre.js_us),
        ms(s_shoot.js_us),
        ms(s_post.js_us),
        dms(s_pre.js_us, s_shoot.js_us),
        dms(s_shoot.js_us, s_post.js_us),
    );
    push(
        "GC (Major+Minor)",
        "ms",
        ms(s_pre.gc_us),
        ms(s_shoot.gc_us),
        ms(s_post.gc_us),
        dms(s_pre.gc_us, s_shoot.gc_us),
        dms(s_shoot.gc_us, s_post.gc_us),
    );
    push(
        "GC count",
        "n",
        s_pre.gc_count.to_string(),
        s_shoot.gc_count.to_string(),
        s_post.gc_count.to_string(),
        dcount(s_pre.gc_count, s_shoot.gc_count),
        dcount(s_shoot.gc_count, s_post.gc_count),
    );
    push(
        "CPU samples",
        "ms",
        ms(s_pre.cpu_us),
        ms(s_shoot.cpu_us),
        ms(s_post.cpu_us),
        dms(s_pre.cpu_us, s_shoot.cpu_us),
        dms(s_shoot.cpu_us, s_post.cpu_us),
    );

    let mut out = String::new();
    out.push_str("## Delta: PRE → SHOOT → POST\n\n");
    out.push_str(&format!(
        "- **SHOOT**: {:.2}ms … {:.2}ms from trace start\n",
        (shoot.0 - min_ts) / 1000.0,
        (shoot.1 - min_ts) / 1000.0,
    ));
    out.push_str(&format!(
        "- **PRE**:  {:.2}ms … {:.2}ms\n",
        (pre.0 - min_ts) / 1000.0,
        (pre.1 - min_ts) / 1000.0,
    ));
    out.push_str(&format!(
        "- **POST**: {:.2}ms … {:.2}ms\n\n",
        (post.0 - min_ts) / 1000.0,
        (post.1 - min_ts) / 1000.0,
    ));

    out.push_str("| metric | PRE | SHOOT | POST | SHOOT−PRE | POST−SHOOT |\n");
    out.push_str("|--------|-----|-------|------|-----------|------------|\n");
    let mut json_rows: Vec<Value> = Vec::new();
    for (metric, unit, pre_v, shoot_v, post_v, dpre, dpost) in &rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            metric, pre_v, shoot_v, post_v, dpre, dpost,
        ));
        let num = |s: &str| s.parse::<f64>().ok();
        json_rows.push(json!({
            "metric": metric,
            "unit": unit,
            "pre": num(pre_v),
            "shoot": num(shoot_v),
            "post": num(post_v),
            "delta_pre": num(dpre),
            "delta_post": num(dpost),
        }));
    }
    out.push('\n');

    // Top CPU self-time per window.
    out.push_str("| window | top CPU self-time |\n");
    out.push_str("|--------|-------------------|\n");
    for (label, stats) in [("PRE", &s_pre), ("SHOOT", &s_shoot), ("POST", &s_post)] {
        let top: String = if stats.top_cpu.is_empty() {
            "—".to_string()
        } else {
            stats
                .top_cpu
                .iter()
                .map(|(n, t)| format!("{} ({:.1}ms)", n, t / 1000.0))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!("| {} | {} |\n", label, top));
    }
    out.push('\n');

    let top_json = |stats: &WindowStats| -> Value {
        Value::Array(
            stats
                .top_cpu
                .iter()
                .map(|(n, t)| json!({"function": n, "self_us": t.round()}))
                .collect(),
        )
    };
    let mut obj = serde_json::Map::new();
    obj.insert("anchor_us".into(), json!(anchor_ts.round()));
    obj.insert("windows".into(), json!({
        "pre": [pre.0.round(), pre.1.round()],
        "shoot": [shoot.0.round(), shoot.1.round()],
        "post": [post.0.round(), post.1.round()],
    }));
    obj.insert("metrics".into(), Value::Array(json_rows));
    obj.insert("top_cpu".into(), json!({
        "pre": top_json(&s_pre),
        "shoot": top_json(&s_shoot),
        "post": top_json(&s_post),
    }));
    (out, Value::Object(obj))
}

// ── CSV ──

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn json_to_field(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(_) | Value::Bool(_) => v.to_string(),
        other => other.to_string(),
    }
}

/// Render a JSON array of objects as CSV (header from the first row's keys).
/// Nested objects/arrays are serialized as compact JSON inside the field.
pub fn rows_to_csv(rows: &[Value]) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        return out;
    }
    let Some(obj) = rows[0].as_object() else {
        return out;
    };
    let keys: Vec<String> = obj.keys().cloned().collect();
    out.push_str(
        &keys
            .iter()
            .map(|k| csv_field(k))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for r in rows {
        let Some(o) = r.as_object() else { continue };
        let vals: Vec<String> = keys
            .iter()
            .map(|k| csv_field(&json_to_field(o.get(k).unwrap_or(&Value::Null))))
            .collect();
        out.push_str(&vals.join(","));
        out.push('\n');
    }
    out
}
