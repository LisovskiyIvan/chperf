//! Compositor/raster/GPU pipeline: per-thread lanes plus category totals.
//!
//! Category mapping (documented so callers can label honestly):
//! raster = `RasterTask`, `Rasterize`, `Paint`; decode = `Decode Image`,
//! `DecodeImage`, `PaintImage`; composite = `CompositeLayers`, `DrawFrame`,
//! `BeginFrame`, `ActivateLayerTree`, `Commit`, `Layerize`; gpu = `GPUTask`.
//! Only `X` phases with a duration contribute time; every phase counts
//! toward `count`.

use crate::trace::TraceEvent;
use rustc_hash::FxHashMap;

#[derive(Clone)]
pub struct RasterLane {
    pub tid: u64,
    pub thread_name: String,
    /// Event name forming this lane.
    pub name: String,
    pub count: usize,
    pub total_us: f64,
    pub max_us: f64,
}

#[derive(Clone)]
pub struct DecodeHotspot {
    pub ts_us: f64,
    pub dur_us: f64,
    /// `args.data.url`, else `nodeId`/`nodeName`/`domNodeId`, else `""`.
    pub url_or_node: String,
}

#[derive(Clone)]
pub struct RasterResult {
    /// Per `(tid, name)` lanes, hottest first.
    pub lanes: Vec<RasterLane>,
    pub total_raster_us: f64,
    pub total_decode_us: f64,
    pub total_composite_us: f64,
    pub gpu_task_us: f64,
    pub max_decode_us: f64,
    /// `Decode Image`/`PaintImage` events, worst first.
    pub decode_hotspots: Vec<DecodeHotspot>,
}

/// Category of a tracked raster-pipeline event, or `None` when untracked.
fn category_of(name: &str) -> Option<u8> {
    Some(match name {
        "RasterTask" | "Rasterize" | "Paint" => 0,
        "Decode Image" | "DecodeImage" | "PaintImage" => 1,
        "CompositeLayers" | "DrawFrame" | "BeginFrame" | "ActivateLayerTree" | "Commit"
        | "Layerize" => 2,
        "GPUTask" => 3,
        _ => return None,
    })
}

fn is_decode_hotspot(name: &str) -> bool {
    matches!(name, "Decode Image" | "DecodeImage" | "PaintImage")
}

pub fn analyze_raster(events: &[&TraceEvent], main_tid: u64) -> RasterResult {
    let _ = main_tid;
    // Thread names from `thread_name` metadata (single pre-pass; metadata
    // events are cheap to match by interned name).
    let mut names: FxHashMap<u64, String> = FxHashMap::default();
    for e in events {
        if e.name == "thread_name"
            && let Some(n) = e
                .args_value()
                .as_ref()
                .and_then(|a| a.get("name"))
                .and_then(|v| v.as_str())
        {
            names.entry(e.tid).or_insert_with(|| n.to_string());
        }
    }

    let mut lane_idx: FxHashMap<(u64, &str), usize> = FxHashMap::default();
    let mut lanes: Vec<RasterLane> = Vec::new();
    let mut totals = [0.0f64; 4];
    let mut max_decode_us = 0.0f64;
    let mut decode_hotspots: Vec<DecodeHotspot> = Vec::new();

    // Single pass; `args_value` is parsed only for decode-hotspot names.
    for e in events {
        let cat = match category_of(&e.name) {
            Some(c) => c,
            None => continue,
        };
        let dur = if e.ph == b'X' {
            e.dur.unwrap_or(0.0).max(0.0)
        } else {
            0.0
        };
        totals[cat as usize] += dur;

        let idx = match lane_idx.get(&(e.tid, e.name.as_str())) {
            Some(&i) => i,
            None => {
                let i = lanes.len();
                lanes.push(RasterLane {
                    tid: e.tid,
                    thread_name: names.get(&e.tid).cloned().unwrap_or_default(),
                    name: e.name.to_string(),
                    count: 0,
                    total_us: 0.0,
                    max_us: 0.0,
                });
                lane_idx.insert((e.tid, e.name.as_str()), i);
                i
            }
        };
        let lane = &mut lanes[idx];
        lane.count += 1;
        lane.total_us += dur;
        if dur > lane.max_us {
            lane.max_us = dur;
        }

        if is_decode_hotspot(&e.name) {
            if dur > max_decode_us {
                max_decode_us = dur;
            }
            let url_or_node = e
                .args_value()
                .as_ref()
                .and_then(|a| a.get("data"))
                .and_then(|d| {
                    d.get("url")
                        .or_else(|| d.get("nodeId"))
                        .or_else(|| d.get("domNodeId"))
                        .or_else(|| d.get("nodeName"))
                })
                .and_then(|v| match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            decode_hotspots.push(DecodeHotspot {
                ts_us: e.ts,
                dur_us: dur,
                url_or_node,
            });
        }
    }

    // Hottest lane first (ties by tid, then name); hotspots worst first
    // (ties by ts). Lane order otherwise follows map iteration (hash order).
    lanes.sort_by(|a, b| {
        b.total_us
            .partial_cmp(&a.total_us)
            .unwrap()
            .then_with(|| a.tid.cmp(&b.tid))
            .then_with(|| a.name.cmp(&b.name))
    });
    decode_hotspots.sort_by(|a, b| {
        b.dur_us
            .partial_cmp(&a.dur_us)
            .unwrap()
            .then_with(|| a.ts_us.partial_cmp(&b.ts_us).unwrap())
    });

    RasterResult {
        lanes,
        total_raster_us: totals[0],
        total_decode_us: totals[1],
        total_composite_us: totals[2],
        gpu_task_us: totals[3],
        max_decode_us,
        decode_hotspots,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::TraceEvent;

    fn ev(name: &str, ph: u8, ts: f64, dur: Option<f64>, tid: u64) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph,
            ts,
            dur,
            tid,
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
        tid: u64,
        args: serde_json::Value,
    ) -> TraceEvent {
        TraceEvent {
            name: name.into(),
            id: 0,
            has_id: false,
            ph,
            ts,
            dur,
            tid,
            pid: 1,
            cat: None,
            args: crate::trace::test_args(args),
        }
    }

    fn thread_name(tid: u64, name: &str) -> TraceEvent {
        ev_args(
            "thread_name",
            b'M',
            0.0,
            None,
            tid,
            serde_json::json!({"name": name}),
        )
    }

    #[test]
    fn multi_thread_lanes_with_names_and_totals() {
        let events = [
            thread_name(2, "CompositorTileWorker"),
            thread_name(3, "CrGpuMain"),
            ev("RasterTask", b'X', 100.0, Some(500.0), 2),
            ev("Rasterize", b'X', 200.0, Some(300.0), 2),
            ev("Decode Image", b'X', 300.0, Some(800.0), 2),
            ev("CompositeLayers", b'X', 400.0, Some(100.0), 2),
            ev("GPUTask", b'X', 500.0, Some(250.0), 3),
            ev("RunTask", b'X', 600.0, Some(999.0), 1), // untracked
        ];
        // Same events without args carry no hotspot source; add one with args.
        let mut events = Vec::from(events);
        events.push(ev_args(
            "PaintImage",
            b'X',
            700.0,
            Some(600.0),
            2,
            serde_json::json!({"data": {"url": "https://x/img.png"}}),
        ));
        let r = analyze_raster(&events.iter().collect::<Vec<_>>(), 1);
        assert_eq!(r.total_raster_us, 800.0);
        assert_eq!(r.total_decode_us, 1_400.0);
        assert_eq!(r.total_composite_us, 100.0);
        assert_eq!(r.gpu_task_us, 250.0);
        assert_eq!(r.max_decode_us, 800.0);
        // Lanes grouped by (tid, name): 6 tracked lanes, hottest first.
        assert_eq!(r.lanes.len(), 6);
        assert_eq!(r.lanes[0].name, "Decode Image");
        assert_eq!(r.lanes[0].thread_name, "CompositorTileWorker");
        assert!(r.lanes.windows(2).all(|w| w[0].total_us >= w[1].total_us));
        let gpu = r.lanes.iter().find(|l| l.name == "GPUTask").unwrap();
        assert_eq!(gpu.tid, 3);
        assert_eq!(gpu.thread_name, "CrGpuMain");
        // Hotspots worst first; the argless Decode Image sorts after PaintImage.
        assert_eq!(r.decode_hotspots.len(), 2);
        assert_eq!(r.decode_hotspots[0].url_or_node, "");
        assert_eq!(r.decode_hotspots[1].url_or_node, "https://x/img.png");
        assert_eq!(r.decode_hotspots[0].dur_us, 800.0);
    }

    #[test]
    fn non_x_phases_count_without_time_and_empty_trace() {
        let r = analyze_raster(&[], 1);
        assert!(r.lanes.is_empty());
        assert!(r.decode_hotspots.is_empty());
        assert_eq!(r.total_raster_us, 0.0);
        assert_eq!(r.max_decode_us, 0.0);
        // BeginFrame as B/E pairs: counted, zero duration.
        let events = [
            ev("BeginFrame", b'B', 100.0, None, 2),
            ev("BeginFrame", b'E', 150.0, None, 2),
        ];
        let r = analyze_raster(&events.iter().collect::<Vec<_>>(), 1);
        assert_eq!(r.lanes.len(), 1);
        assert_eq!(r.lanes[0].count, 2);
        assert_eq!(r.lanes[0].total_us, 0.0);
        assert_eq!(r.total_composite_us, 0.0);
    }

    #[test]
    fn decode_hotspot_node_fallbacks() {
        let events = [
            ev_args(
                "DecodeImage",
                b'X',
                10.0,
                Some(50.0),
                4,
                serde_json::json!({"data": {"nodeId": 77}}),
            ),
            ev_args(
                "Decode Image",
                b'X',
                20.0,
                Some(70.0),
                4,
                serde_json::json!({"data": {"nodeName": "IMG"}}),
            ),
        ];
        let r = analyze_raster(&events.iter().collect::<Vec<_>>(), 1);
        assert_eq!(r.decode_hotspots.len(), 2);
        assert_eq!(r.decode_hotspots[0].url_or_node, "IMG");
        assert_eq!(r.decode_hotspots[1].url_or_node, "77");
        assert_eq!(r.max_decode_us, 70.0);
        // Unknown thread: empty name, still grouped.
        assert_eq!(r.lanes[0].thread_name, "");
    }
}
