//! Layout dirty-object analysis.

use crate::trace::TraceEvent;

#[derive(Clone)]
pub struct LayoutDirtyEntry {
    #[allow(dead_code)]
    pub ts: f64,
    pub dur_us: f64,
    pub dirty_count: u32,
    pub total_count: u32,
}

#[derive(Clone)]
pub struct LayoutDirtyResult {
    pub entries: Vec<LayoutDirtyEntry>,
    pub avg_dirty: f64,
    pub max_dirty: u32,
    pub avg_ratio: f64,
}

pub fn analyze_layout_dirty(events: &[TraceEvent], main_tid: u64) -> LayoutDirtyResult {
    let mut entries = Vec::new();

    for e in events {
        if e.tid != main_tid || e.name != "Layout" || e.ph != b'X' {
            continue;
        }
        let args = match e.args_value() {
            Some(a) => a,
            None => continue,
        };
        let begin_data = match args.get("beginData") {
            Some(bd) => bd,
            None => continue,
        };

        let dirty = begin_data
            .get("dirtyObjects")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let total = begin_data
            .get("totalObjects")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        entries.push(LayoutDirtyEntry {
            ts: e.ts,
            dur_us: e.dur.unwrap_or(0.0),
            dirty_count: dirty,
            total_count: total,
        });
    }

    entries.sort_by_key(|b| std::cmp::Reverse(b.dirty_count));

    let n = entries.len().max(1) as f64;
    let avg_dirty = entries.iter().map(|e| e.dirty_count as f64).sum::<f64>() / n;
    let max_dirty = entries.iter().map(|e| e.dirty_count).max().unwrap_or(0);
    let avg_ratio = entries
        .iter()
        .filter(|e| e.total_count > 0)
        .map(|e| e.dirty_count as f64 / e.total_count as f64 * 100.0)
        .sum::<f64>()
        / n;

    LayoutDirtyResult {
        entries,
        avg_dirty,
        max_dirty,
        avg_ratio,
    }
}
