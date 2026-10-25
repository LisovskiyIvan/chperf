//! Style recalculation (UpdateLayoutTree) analysis.

use crate::trace::TraceEvent;

#[derive(Clone)]
pub struct StyleRecalcEntry {
    pub dur_us: f64,
    pub element_count: u32,
}

#[derive(Clone)]
pub struct StyleRecalcResult {
    pub entries: Vec<StyleRecalcEntry>,
    pub avg_elements: f64,
    pub max_elements: u32,
    pub total_count: usize,
}

pub fn analyze_style_recalc(events: &[TraceEvent], main_tid: u64) -> StyleRecalcResult {
    let mut entries = Vec::new();

    for e in events {
        if e.tid != main_tid || e.name != "UpdateLayoutTree" || e.ph != b'X' {
            continue;
        }
        let args = e.args_value();
        let element_count = args
            .as_ref()
            .and_then(|a| a.get("elementCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        if element_count > 0 {
            entries.push(StyleRecalcEntry {
                dur_us: e.dur.unwrap_or(0.0),
                element_count,
            });
        }
    }

    // Element-count desc; exact ties break by dur desc (scan order is
    // deterministic but not a meaningful key).
    entries.sort_by(|a, b| {
        b.element_count
            .cmp(&a.element_count)
            .then_with(|| b.dur_us.partial_cmp(&a.dur_us).unwrap())
    });

    let n = entries.len().max(1) as f64;
    let avg_elements = entries.iter().map(|e| e.element_count as f64).sum::<f64>() / n;
    let max_elements = entries.iter().map(|e| e.element_count).max().unwrap_or(0);
    let total_count = entries.len();

    StyleRecalcResult {
        entries,
        avg_elements,
        max_elements,
        total_count,
    }
}
