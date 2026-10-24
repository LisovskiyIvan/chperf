//! Chrome `ph:"C"` counter events: `args` is a flat name→number map
//! (either top-level or under `args.data`).
//!
//! A "counter" is identified by its args key (e.g. `jsHeapSizeUsed`); the
//! lookup checks top-level args first, then `args.data`. As a fallback for
//! single-series exporters, `counter_series` also matches the event's own
//! name and reads its preferred numeric field (the key equal to the event
//! name, else the alphabetically first numeric key).

use crate::trace::TraceEvent;

/// Numeric fields of a counter event's args: top-level numbers plus numbers
/// under `args.data`. A key present in both places is returned once
/// (top-level value wins).
fn numeric_fields(args: &serde_json::Value) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = Vec::new();
    let Some(obj) = args.as_object() else {
        return out;
    };
    for (k, v) in obj {
        if k != "data"
            && let Some(f) = v.as_f64()
        {
            out.push((k.clone(), f));
        }
    }
    if let Some(data) = obj.get("data").and_then(|d| d.as_object()) {
        for (k, v) in data {
            if out.iter().all(|(ek, _)| ek != k)
                && let Some(f) = v.as_f64()
            {
                out.push((k.clone(), f));
            }
        }
    }
    out
}

/// Value of counter `name` in one event's args: top-level `name` first,
/// then `args.data[name]`.
fn field_value(fields: &[(String, f64)], name: &str) -> Option<f64> {
    fields.iter().find(|(k, _)| k == name).map(|(_, v)| *v)
}

/// Distinct counter names across `ph:"C"` events with per-counter sample
/// counts (events carrying the key). Sorted by name.
pub fn counter_names(events: &[TraceEvent]) -> Vec<(String, usize)> {
    let mut counts: rustc_hash::FxHashMap<String, usize> = rustc_hash::FxHashMap::default();
    for e in events {
        if e.ph != b'C' {
            continue;
        }
        let Some(args) = e.args_value() else {
            continue;
        };
        // Keys are unique per event by construction (numeric_fields dedups
        // top-level / data overlap), so one event counts once per key.
        for (k, _) in numeric_fields(&args) {
            *counts.entry(k).or_default() += 1;
        }
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// `(ts_us, value)` series for counter `name`, sorted by ts. Matches any
/// `ph:"C"` event whose args carry a numeric `name` (top-level or
/// `args.data`); when no args key matches, events whose own name equals
/// `name` contribute their preferred numeric field instead.
pub fn counter_series(events: &[TraceEvent], name: &str) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    for e in events {
        if e.ph != b'C' {
            continue;
        }
        let Some(args) = e.args_value() else {
            continue;
        };
        let fields = numeric_fields(&args);
        if fields.is_empty() {
            continue;
        }
        if let Some(v) = field_value(&fields, name) {
            out.push((e.ts, v));
        } else if e.name == name {
            // Single-series exporter: prefer the key equal to the event
            // name, else the alphabetically first numeric key.
            let v = field_value(&fields, e.name).or_else(|| {
                let mut sorted = fields.clone();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                sorted.into_iter().next().map(|(_, v)| v)
            });
            if let Some(v) = v {
                out.push((e.ts, v));
            }
        }
    }
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::intern_name;

    fn cev(ts: f64, name: &'static str, args: serde_json::Value) -> TraceEvent {
        TraceEvent {
            name: intern_name(name),
            id: 0,
            has_id: false,
            ph: b'C',
            ts,
            dur: None,
            tid: 1,
            pid: 1,
            cat: None,
            args: crate::trace::test_args(args),
        }
    }

    #[test]
    fn names_count_samples_per_key_sorted() {
        let events = vec![
            cev(1.0, "Mem", serde_json::json!({"heap": 10.0, "docs": 2.0})),
            cev(2.0, "Mem", serde_json::json!({"data": {"heap": 12.0}})),
            cev(3.0, "Fps", serde_json::json!({"fps": 60.0})),
            // Non-counter phases are ignored.
            {
                let mut x = cev(4.0, "Mem", serde_json::json!({"heap": 99.0}));
                x.ph = b'X';
                x
            },
        ];
        assert_eq!(
            counter_names(&events),
            vec![
                ("docs".to_string(), 1),
                ("fps".to_string(), 1),
                ("heap".to_string(), 2),
            ]
        );
    }

    #[test]
    fn series_reads_top_level_and_data_sorted_by_ts() {
        let events = vec![
            cev(3.0, "Mem", serde_json::json!({"data": {"heap": 30.0}})),
            cev(1.0, "Mem", serde_json::json!({"heap": 10.0})),
            cev(
                2.0,
                "Mem",
                serde_json::json!({"heap": 20.0, "data": {"heap": 999.0}}),
            ),
        ];
        // Top-level wins over data on the same event; output sorted by ts.
        assert_eq!(
            counter_series(&events, "heap"),
            vec![(1.0, 10.0), (2.0, 20.0), (3.0, 30.0)]
        );
        assert!(counter_series(&events, "missing").is_empty());
    }

    #[test]
    fn series_falls_back_to_event_name() {
        let events = vec![cev(1.0, "fps", serde_json::json!({"v": 60.0}))];
        assert!(counter_names(&events).iter().any(|(n, _)| n == "v"));
        assert_eq!(counter_series(&events, "fps"), vec![(1.0, 60.0)]);
    }

    #[test]
    fn non_numeric_and_missing_args_are_skipped() {
        let events = vec![
            cev(1.0, "A", serde_json::json!({"s": "text", "b": true})),
            cev(2.0, "B", serde_json::json!(null)),
        ];
        assert!(counter_names(&events).is_empty());
        assert!(counter_series(&events, "s").is_empty());
    }
}
