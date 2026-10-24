//! Async/causality flow chains: Chrome async events (`ph:"s"` start,
//! `"t"` step, `"f"` finish, `"n"` instant step, legacy `"b"`/`"e"`
//! async-nesting open/close) grouped into per-flow chains.
//!
//! Pairing key is `(pid, id)` — the normalized numeric id already on
//! `TraceEvent` (int, string, and `id2:{local,global}` spellings all fold
//! to the same `u64` at parse time). Unlike `normalize_be_pairs`
//! (uppercase `B`/`E` without ids, paired per thread+name into synthetic
//! `X` events), these phases carry ids and may hop threads, so pairing is
//! strictly by `(pid, id)`, never by thread.
//!
//! Chain name is the `s`/`b` (open) event's name, falling back to the most
//! common step name when no open event exists. Only events with an id
//! (`has_id`) participate; everything else is ignored. Steps are sorted by
//! `ts`; chain duration is `(last step ts + its dur or ts) − first ts`,
//! never negative (clock skew clamps to 0).

use crate::trace::TraceEvent;
use std::collections::HashMap;

/// One step of an async flow chain.
pub struct FlowStep {
    pub name: String,
    pub ts_us: f64,
    pub dur_us: Option<f64>,
    pub tid: u64,
    pub pid: u64,
    pub phase: char,
}

/// One async flow chain: all steps sharing `(pid, id)`.
pub struct FlowChain {
    pub name: String,
    pub id: String,
    pub pid: u64,
    pub start_us: f64,
    pub end_us: Option<f64>,
    pub steps: Vec<FlowStep>,
}

/// Aggregate over [`flow_chains`]: total chain count, chains with no
/// close event (`f`/`e`), and per-name `(name, count, avg duration µs)`.
pub struct FlowStats {
    pub chains: usize,
    pub unpaired: usize,
    pub by_name: Vec<(String, usize, f64)>,
}

fn is_open(ph: u8) -> bool {
    ph == b's' || ph == b'b' || ph == b'B'
}

fn is_close(ph: u8) -> bool {
    ph == b'f' || ph == b'e' || ph == b'E'
}

fn is_flow_ph(ph: u8) -> bool {
    matches!(ph, b's' | b't' | b'f' | b'n' | b'b' | b'e' | b'B' | b'E')
}

/// Group async events by `(pid, id)` into chains (see module docs).
/// Events without an id or with a non-flow phase are ignored.
pub fn flow_chains(events: &[TraceEvent]) -> Vec<FlowChain> {
    let mut groups: HashMap<(u64, u64), Vec<&TraceEvent>> = HashMap::default();
    for e in events {
        if !e.has_id || !is_flow_ph(e.ph) {
            continue;
        }
        groups.entry((e.pid, e.id)).or_default().push(e);
    }
    let mut chains: Vec<FlowChain> = groups
        .into_iter()
        .map(|((pid, id), mut steps)| {
            steps.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap());
            let open_name = steps.iter().find(|e| is_open(e.ph)).map(|e| e.name);
            let name = match open_name {
                Some(n) => n.to_string(),
                None => {
                    let mut counts: HashMap<&str, usize> = HashMap::new();
                    for e in &steps {
                        *counts.entry(e.name).or_default() += 1;
                    }
                    counts
                        .into_iter()
                        .max_by_key(|(_, c)| *c)
                        .map(|(n, _)| n.to_string())
                        .unwrap_or_default()
                }
            };
            let start_us = steps[0].ts;
            let closed = steps.iter().any(|e| is_close(e.ph));
            let last = steps[steps.len() - 1];
            let end_ts = last.ts + last.dur.unwrap_or(0.0);
            let _dur = (end_ts - start_us).max(0.0);
            let end_us = if closed { Some(end_ts) } else { None };
            FlowChain {
                name,
                id: id.to_string(),
                pid,
                start_us,
                end_us,
                steps: steps
                    .iter()
                    .map(|e| FlowStep {
                        name: e.name.to_string(),
                        ts_us: e.ts,
                        dur_us: e.dur,
                        tid: e.tid,
                        pid: e.pid,
                        phase: e.ph as char,
                    })
                    .collect(),
            }
        })
        .collect();
    chains.sort_by(|a, b| a.start_us.partial_cmp(&b.start_us).unwrap());
    chains
}

/// Aggregate [`flow_chains`] output: chain count, unpaired (no `f`/`e`)
/// count, and per-name `(name, count, avg duration µs)` sorted by count.
/// Average duration uses the same clamped chain duration as the chains.
pub fn flow_stats(events: &[TraceEvent]) -> FlowStats {
    let chains = flow_chains(events);
    let unpaired = chains.iter().filter(|c| c.end_us.is_none()).count();
    let mut by_name: HashMap<String, (usize, f64)> = HashMap::new();
    for c in &chains {
        let last = &c.steps[c.steps.len() - 1];
        let dur = (last.ts_us + last.dur_us.unwrap_or(0.0) - c.start_us).max(0.0);
        let e = by_name.entry(c.name.clone()).or_default();
        e.0 += 1;
        e.1 += dur;
    }
    let mut by_name: Vec<(String, usize, f64)> = by_name
        .into_iter()
        .map(|(n, (count, total))| (n, count, total / count as f64))
        .collect();
    by_name.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    FlowStats {
        chains: chains.len(),
        unpaired,
        by_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::TraceEvent;

    fn ev(name: &str, ph: u8, id: u64, has_id: bool, ts: f64, pid: u64, tid: u64) -> TraceEvent {
        TraceEvent {
            name: crate::trace::intern_name(name),
            id,
            has_id,
            ph,
            ts,
            dur: None,
            tid,
            pid,
            cat: None,
            args: None,
        }
    }

    #[test]
    fn paired_start_finish() {
        let events = vec![
            ev("Animation", b's', 7, true, 1_000.0, 1, 1),
            ev("Animation", b'f', 7, true, 5_000.0, 1, 1),
        ];
        let chains = flow_chains(&events);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].name, "Animation");
        assert_eq!(chains[0].id, "7");
        assert_eq!(chains[0].steps.len(), 2);
        assert_eq!(chains[0].end_us, Some(5_000.0));
        let st = flow_stats(&events);
        assert_eq!((st.chains, st.unpaired), (1, 0));
        assert_eq!(st.by_name[0], ("Animation".to_string(), 1, 4_000.0));
    }

    #[test]
    fn steps_in_order_and_duration_uses_last_dur() {
        let mut e = vec![
            ev("Load", b'f', 3, true, 9_000.0, 1, 2),
            ev("Load", b't', 3, true, 5_000.0, 1, 1),
            ev("Load", b's', 3, true, 1_000.0, 1, 1),
        ];
        e[0].dur = Some(1_000.0);
        let chains = flow_chains(&e);
        assert_eq!(chains.len(), 1);
        let phases: Vec<char> = chains[0].steps.iter().map(|s| s.phase).collect();
        assert_eq!(phases, vec!['s', 't', 'f']);
        assert_eq!(chains[0].end_us, Some(10_000.0));
        let st = flow_stats(&e);
        assert_eq!(st.by_name[0].2, 9_000.0);
    }

    #[test]
    fn unpaired_and_idless_ignored() {
        let events = vec![
            ev("A", b's', 1, true, 1_000.0, 1, 1),
            ev("B", b'X', 0, false, 2_000.0, 1, 1),
            ev("A", b'f', 0, false, 3_000.0, 1, 1),
        ];
        let chains = flow_chains(&events);
        assert_eq!(chains.len(), 1);
        assert!(chains[0].end_us.is_none());
        assert_eq!(flow_stats(&events).unpaired, 1);
    }

    #[test]
    fn string_and_id2_ids_pair_via_parse() {
        // int 42, string "42", and id2 {local:"42"} normalize to one id.
        let raw = r#"{"traceEvents": [
            {"name":"Net","ph":"s","ts":1000,"pid":1,"tid":1,"id":42},
            {"name":"Net","ph":"t","ts":2000,"pid":1,"tid":2,"id":"42"},
            {"name":"Net","ph":"f","ts":3000,"pid":1,"tid":1,"id2":{"local":"42"}}
        ]}"#;
        let path = std::env::temp_dir().join(format!("chperf-flow-{}.json", std::process::id()));
        std::fs::write(&path, raw).unwrap();
        let trace = crate::trace::parse_trace(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let chains = flow_chains(&trace.trace_events);
        assert_eq!(chains.len(), 1, "all id spellings must pair");
        assert_eq!(chains[0].steps.len(), 3);
        assert_eq!(chains[0].end_us, Some(3_000.0));
    }

    #[test]
    fn cross_thread_steps_stay_in_one_chain() {
        let events = vec![
            ev("Pipe", b's', 9, true, 1_000.0, 1, 1),
            ev("Pipe", b'n', 9, true, 2_000.0, 1, 5),
            ev("Pipe", b'f', 9, true, 3_000.0, 1, 9),
        ];
        let chains = flow_chains(&events);
        assert_eq!(chains.len(), 1);
        let tids: Vec<u64> = chains[0].steps.iter().map(|s| s.tid).collect();
        assert_eq!(tids, vec![1, 5, 9]);
    }

    #[test]
    fn legacy_nesting_phases_pair() {
        let events = vec![
            ev("Nest", b'b', 4, true, 1_000.0, 2, 1),
            ev("Nest", b'e', 4, true, 4_000.0, 2, 1),
        ];
        let chains = flow_chains(&events);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].name, "Nest");
        assert_eq!(chains[0].end_us, Some(4_000.0));
    }

    #[test]
    fn skewed_timestamps_stay_non_negative() {
        // Finish stamped before its start: sorting still yields a
        // non-negative duration (clamped, never a negative avg).
        let events = vec![
            ev("S", b's', 5, true, 2_000.0, 1, 1),
            ev("S", b'f', 5, true, 1_000.0, 1, 1),
        ];
        let chains = flow_chains(&events);
        assert_eq!(chains[0].end_us, Some(2_000.0));
        let st = flow_stats(&events);
        assert!(st.by_name[0].2 >= 0.0);
    }
}
