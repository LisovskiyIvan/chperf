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
        let element_count = e
            .args_value()
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

    entries.sort_by_key(|b| std::cmp::Reverse(b.element_count));

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
