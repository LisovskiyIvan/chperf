//! Network request lifecycle: pairs `Resource*` events by
//! `args.data.requestId` (string or number; `id`/`id2` also accepted) into
//! per-request timings, plus `XHR*` entries keyed by URL.
//!
//! A request missing its start or end event still yields an entry (with
//! `None` for the missing side) and is counted in `incomplete` — unpaired
//! events never crash the analysis.

use crate::trace::TraceEvent;
use rustc_hash::FxHashMap;

#[derive(Clone)]
pub struct NetRequest {
    pub request_id: String,
    pub url: String,
    pub start_us: Option<f64>,
    pub first_byte_us: Option<f64>,
    pub end_us: Option<f64>,
    pub send_to_first_byte_us: Option<f64>,
    pub total_us: Option<f64>,
    /// Sum of `args.data.encodedDataLength` over `ResourceReceivedData`
    /// chunks (falls back to any lifecycle event carrying it when no chunks
    /// were seen), `None` when never reported.
    pub encoded_data_length: Option<u64>,
    pub data_chunks: usize,
    /// `args.data.fromCache`/`cached`, or a `mimeType` containing "cache".
    pub from_cache: bool,
    /// `args.data.renderBlocking` (`true`, or a blocking string variant).
    pub render_blocking: bool,
    /// `args.data.priority`/`initialPriority`, when reported.
    pub priority: Option<String>,
    /// Raw `args.data.timing.queueingTime` when reported (units as traced).
    pub queued_us: Option<f64>,
    /// `XHRReadyStateChange`/`XHRLoad` entry (keyed by URL), not a
    /// `Resource*` request.
    pub is_xhr: bool,
    /// The start event fired on `main_tid`.
    pub on_main_thread: bool,
}

#[derive(Clone)]
pub struct NetworkResult {
    /// Slowest first (`total_us` descending, `None` last); see `slowest`.
    pub requests: Vec<NetRequest>,
    pub request_count: usize,
    pub cache_hits: usize,
    pub render_blocking_count: usize,
    /// `max(end_us) - min(start_us)` over paired requests, `0.0` when empty.
    pub total_wall_us: f64,
    /// Entries missing `start_us` or `end_us`.
    pub incomplete: usize,
}

impl NetworkResult {
    /// First `n` entries of the slowest-first `requests` list.
    pub fn slowest(&self, n: usize) -> &[NetRequest] {
        &self.requests[..n.min(self.requests.len())]
    }
}

#[derive(Default)]
struct Build {
    request_id: String,
    url: String,
    start_us: Option<f64>,
    first_byte_us: Option<f64>,
    end_us: Option<f64>,
    encoded_data_length: Option<u64>,
    data_chunks: usize,
    from_cache: bool,
    render_blocking: bool,
    priority: Option<String>,
    queued_us: Option<f64>,
    is_xhr: bool,
    on_main_thread: bool,
}

fn val_to_id(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Some(u.to_string())
            } else if let Some(i) = n.as_i64() {
                Some(i.to_string())
            } else {
                n.as_f64().map(|f| f.to_string())
            }
        }
        _ => None,
    }
}

fn request_id_of(e: &TraceEvent, args: &Option<serde_json::Value>) -> Option<String> {
    if let Some(a) = args {
        if let Some(d) = a.get("data") {
            for k in ["requestId", "id", "id2"] {
                if let Some(id) = d.get(k).and_then(val_to_id) {
                    return Some(id);
                }
            }
        }
        for k in ["id", "id2"] {
            if let Some(id) = a.get(k).and_then(val_to_id) {
                return Some(id);
            }
        }
    }
    if e.has_id {
        return Some(e.id.to_string());
    }
    None
}

fn num(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn fold_meta(b: &mut Build, data: &serde_json::Value) {
    if b.url.is_empty()
        && let Some(u) = data.get("url").and_then(|v| v.as_str())
        && !u.is_empty()
    {
        b.url = u.to_string();
    }
    if b.priority.is_none() {
        b.priority = data
            .get("priority")
            .or_else(|| data.get("initialPriority"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    if !b.from_cache {
        let cached = data
            .get("fromCache")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || data
                .get("cached")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            || data
                .get("mimeType")
                .or_else(|| data.get("mime_type"))
                .and_then(|v| v.as_str())
                .is_some_and(|m| m.to_ascii_lowercase().contains("cache"));
        b.from_cache = cached;
    }
    if !b.render_blocking
        && let Some(rb) = data.get("renderBlocking")
    {
        b.render_blocking = match rb {
            serde_json::Value::Bool(v) => *v,
            serde_json::Value::String(s) => {
                let l = s.to_ascii_lowercase();
                l.contains("blocking") && !l.contains("non")
            }
            _ => false,
        };
    }
    if b.queued_us.is_none() {
        b.queued_us = data
            .get("timing")
            .and_then(|t| t.get("queueingTime"))
            .and_then(num);
    }
    if let Some(len) = data.get("encodedDataLength").and_then(|v| v.as_u64()) {
        // `ResourceFinish`/response carry the final total; per-chunk values
        // are summed at the call site, so keep the max here.
        b.encoded_data_length = Some(b.encoded_data_length.unwrap_or(0).max(len));
    }
}

pub fn analyze_network(events: &[&TraceEvent], main_tid: u64) -> NetworkResult {
    let mut builds: FxHashMap<String, Build> = FxHashMap::default();
    let mut order: Vec<String> = Vec::new();

    // Single pass; `args_value` is parsed only for lifecycle/XHR names.
    for e in events {
        let is_lifecycle = matches!(
            e.name.as_str(),
            "ResourceWillSendRequest"
                | "ResourceSendRequest"
                | "ResourceReceiveResponse"
                | "ResourceReceivedData"
                | "ResourceFinish"
        );
        let is_xhr = matches!(e.name.as_str(), "XHRReadyStateChange" | "XHRLoad");
        if !is_lifecycle && !is_xhr {
            continue;
        }
        let args = e.args_value();
        let data = args.as_ref().and_then(|a| a.get("data"));

        // XHR events have no requestId: key them by URL.
        let key: String = if is_xhr {
            match data.and_then(|d| d.get("url")).and_then(|v| v.as_str()) {
                Some(u) if !u.is_empty() => format!("xhr:{u}"),
                _ => continue,
            }
        } else {
            match request_id_of(e, &args) {
                Some(id) => id,
                None => continue,
            }
        };

        let idx = match builds.get(&key) {
            Some(_) => key.clone(),
            None => {
                order.push(key.clone());
                builds.insert(
                    key.clone(),
                    Build {
                        request_id: key.clone(),
                        ..Build::default()
                    },
                );
                key.clone()
            }
        };
        let b = builds.get_mut(&idx).unwrap();
        if e.tid == main_tid {
            b.on_main_thread = true;
        }
        if let Some(d) = data {
            fold_meta(b, d);
        }
        match e.name.as_str() {
            "ResourceWillSendRequest" | "ResourceSendRequest" => {
                b.start_us = Some(b.start_us.map_or(e.ts, |s| s.min(e.ts)));
            }
            "ResourceReceiveResponse" => {
                b.first_byte_us = Some(b.first_byte_us.map_or(e.ts, |s| s.min(e.ts)));
            }
            "ResourceReceivedData" => {
                b.data_chunks += 1;
                if let Some(len) = data
                    .and_then(|d| d.get("encodedDataLength"))
                    .and_then(|v| v.as_u64())
                {
                    // Chunks carry per-chunk bytes: sum them.
                    b.encoded_data_length = Some(b.encoded_data_length.unwrap_or(0) + len);
                }
            }
            "ResourceFinish" => {
                let end = e.ts + e.dur.unwrap_or(0.0);
                b.end_us = Some(b.end_us.map_or(end, |s| s.max(end)));
            }
            "XHRReadyStateChange" | "XHRLoad" => {
                b.is_xhr = true;
                b.start_us = Some(b.start_us.map_or(e.ts, |s| s.min(e.ts)));
                b.end_us = Some(b.end_us.map_or(e.ts, |s| s.max(e.ts)));
            }
            _ => {}
        }
    }

    let mut requests: Vec<NetRequest> = order
        .into_iter()
        .filter_map(|k| builds.remove(&k))
        .map(|b| {
            let send_to_first_byte_us = match (b.start_us, b.first_byte_us) {
                (Some(s), Some(f)) => Some((f - s).max(0.0)),
                _ => None,
            };
            let total_us = match (b.start_us, b.end_us) {
                (Some(s), Some(x)) => Some((x - s).max(0.0)),
                _ => None,
            };
            NetRequest {
                request_id: b.request_id,
                url: b.url,
                start_us: b.start_us,
                first_byte_us: b.first_byte_us,
                end_us: b.end_us,
                send_to_first_byte_us,
                total_us,
                encoded_data_length: b.encoded_data_length,
                data_chunks: b.data_chunks,
                from_cache: b.from_cache,
                render_blocking: b.render_blocking,
                priority: b.priority,
                queued_us: b.queued_us,
                is_xhr: b.is_xhr,
                on_main_thread: b.on_main_thread,
            }
        })
        .collect();

    // Slowest first so `slowest(n)` is a prefix slice; entries without a
    // total sort last, ties by start.
    requests.sort_by(|a, b| match (a.total_us, b.total_us) {
        (Some(x), Some(y)) => y
            .partial_cmp(&x)
            .unwrap()
            .then_with(|| a.start_us.partial_cmp(&b.start_us).unwrap()),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    let request_count = requests.len();
    let cache_hits = requests.iter().filter(|r| r.from_cache).count();
    let render_blocking_count = requests.iter().filter(|r| r.render_blocking).count();
    let incomplete = requests
        .iter()
        .filter(|r| r.start_us.is_none() || r.end_us.is_none())
        .count();
    let min_start = requests
        .iter()
        .filter_map(|r| r.start_us)
        .fold(f64::INFINITY, f64::min);
    let max_end = requests
        .iter()
        .filter_map(|r| r.end_us)
        .fold(0.0f64, f64::max);
    let total_wall_us = if min_start.is_finite() && max_end > min_start {
        max_end - min_start
    } else {
        0.0
    };

    NetworkResult {
        requests,
        request_count,
        cache_hits,
        render_blocking_count,
        total_wall_us,
        incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::TraceEvent;

    fn ev(name: &str, ts: f64, dur: Option<f64>, tid: u64) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph: b'I',
            ts,
            dur,
            tid,
            pid: 1,
            cat: None,
            args: None,
        }
    }

    fn ev_args(name: &str, ts: f64, tid: u64, args: serde_json::Value) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph: b'I',
            ts,
            dur: None,
            tid,
            pid: 1,
            cat: None,
            args: crate::trace::test_args(args),
        }
    }

    fn req(id: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut data = serde_json::json!({"requestId": id});
        for (k, v) in extra.as_object().unwrap() {
            data.as_object_mut().unwrap().insert(k.clone(), v.clone());
        }
        serde_json::json!({"data": data})
    }

    #[test]
    fn happy_path_pairs_lifecycle_events() {
        let events = [
            ev_args(
                "ResourceWillSendRequest",
                1_000.0,
                1,
                req(
                    "1",
                    serde_json::json!({"url": "https://x/a.js", "priority": "High", "renderBlocking": "blocking"}),
                ),
            ),
            ev_args(
                "ResourceReceiveResponse",
                1_200.0,
                2,
                req(
                    "1",
                    serde_json::json!({"url": "https://x/a.js", "encodedDataLength": 500}),
                ),
            ),
            ev_args(
                "ResourceReceivedData",
                1_250.0,
                2,
                req("1", serde_json::json!({"encodedDataLength": 300})),
            ),
            ev_args(
                "ResourceReceivedData",
                1_300.0,
                2,
                req("1", serde_json::json!({"encodedDataLength": 200})),
            ),
            ev_args(
                "ResourceFinish",
                1_500.0,
                2,
                req("1", serde_json::json!({})),
            ),
        ];
        let r = analyze_network(&events.iter().collect::<Vec<_>>(), 1);
        assert_eq!(r.request_count, 1);
        assert_eq!(r.incomplete, 0);
        assert_eq!(r.render_blocking_count, 1);
        assert_eq!(r.cache_hits, 0);
        assert_eq!(r.total_wall_us, 500.0);
        let q = &r.requests[0];
        assert_eq!(q.url, "https://x/a.js");
        assert_eq!(q.priority.as_deref(), Some("High"));
        assert!(q.render_blocking);
        assert!(q.on_main_thread);
        assert_eq!(q.start_us, Some(1_000.0));
        assert_eq!(q.first_byte_us, Some(1_200.0));
        assert_eq!(q.send_to_first_byte_us, Some(200.0));
        assert_eq!(q.total_us, Some(500.0));
        assert_eq!(q.data_chunks, 2);
        assert_eq!(q.encoded_data_length, Some(1000));
        assert_eq!(r.slowest(5).len(), 1);
    }

    #[test]
    fn unpaired_events_count_as_incomplete() {
        // Finish without start, numeric requestId, cache hit via mimeType.
        let events = [
            ev_args(
                "ResourceFinish",
                2_000.0,
                2,
                req("7", serde_json::json!({})),
            ),
            ev_args(
                "ResourceWillSendRequest",
                3_000.0,
                1,
                serde_json::json!({"data": {"requestId": 42, "url": "https://x/b.css"}}),
            ),
            ev_args(
                "ResourceReceiveResponse",
                3_100.0,
                2,
                serde_json::json!({"data": {"requestId": 42, "mimeType": "disk cache"}}),
            ),
            // No requestId at all: ignored, must not crash.
            ev("ResourceFinish", 9_000.0, None, 2),
        ];
        let r = analyze_network(&events.iter().collect::<Vec<_>>(), 1);
        assert_eq!(r.request_count, 2);
        assert_eq!(r.incomplete, 2);
        assert_eq!(r.cache_hits, 1);
        assert!(r.slowest(1)[0].total_us.is_none());
    }

    #[test]
    fn xhr_entries_keyed_by_url_and_slowest_order() {
        let xhr = |name: &str, ts: f64| {
            ev_args(
                name,
                ts,
                1,
                serde_json::json!({"data": {"url": "https://x/api"}}),
            )
        };
        let events = [
            xhr("XHRReadyStateChange", 100.0),
            xhr("XHRLoad", 400.0),
            ev_args(
                "ResourceWillSendRequest",
                0.0,
                1,
                req("s", serde_json::json!({"url": "https://x/slow"})),
            ),
            ev_args(
                "ResourceFinish",
                1_000.0,
                1,
                req("s", serde_json::json!({})),
            ),
            ev_args(
                "ResourceWillSendRequest",
                0.0,
                1,
                req("f", serde_json::json!({"url": "https://x/fast"})),
            ),
            ev_args("ResourceFinish", 10.0, 1, req("f", serde_json::json!({}))),
        ];
        let r = analyze_network(&events.iter().collect::<Vec<_>>(), 1);
        assert_eq!(r.request_count, 3);
        let x = r.requests.iter().find(|q| q.is_xhr).unwrap();
        assert_eq!(x.url, "https://x/api");
        assert_eq!(x.total_us, Some(300.0));
        // Slowest-first: 1000, 300 (xhr), 10.
        let totals: Vec<Option<f64>> = r.slowest(3).iter().map(|q| q.total_us).collect();
        assert_eq!(totals, vec![Some(1_000.0), Some(300.0), Some(10.0)]);
        assert_eq!(r.slowest(10).len(), 3);
    }
}
