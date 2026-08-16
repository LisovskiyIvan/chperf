//! Report model: bundles a trace's analyses and metadata for export / REPL.
//! This is the presentation aggregate the CLI builds over `chperf_core`.

use chperf_core::analysis::*;
use chperf_core::trace::TraceMetadata;

pub struct App {
    /// CPU throttle factor (1.0 = none, Nx = divide times by N for real-world).
    pub throttle_factor: f64,
    pub summary: SummaryResult,
    pub scroll_frames: ScrollFrameResult,
    pub cpu_profile: CpuProfileResult,
    pub layout_dirty: LayoutDirtyResult,
    pub style_recalc: StyleRecalcResult,
    pub forced_reflows: ForcedReflowResult,
    pub jank: JankResult,
    pub compare: Option<CompareResult>,
    pub trace_name_a: String,
    pub trace_name_b: Option<String>,
    pub metadata: Option<TraceMetadata>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        summary: SummaryResult,
        scroll_frames: ScrollFrameResult,
        cpu_profile: CpuProfileResult,
        layout_dirty: LayoutDirtyResult,
        style_recalc: StyleRecalcResult,
        forced_reflows: ForcedReflowResult,
        jank: JankResult,
        compare: Option<CompareResult>,
        trace_name_a: String,
        trace_name_b: Option<String>,
        metadata: Option<TraceMetadata>,
    ) -> Self {
        App {
            throttle_factor: 1.0,
            summary,
            scroll_frames,
            cpu_profile,
            layout_dirty,
            style_recalc,
            forced_reflows,
            jank,
            compare,
            trace_name_a,
            trace_name_b,
            metadata,
        }
    }
}
