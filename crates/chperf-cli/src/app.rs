//! Report model: bundles a trace's analyses and metadata for export / REPL.
//! This is the presentation aggregate the CLI builds over `chperf_core`.

use chperf_core::analysis::*;
use chperf_core::trace::{ParseStats, ThreadModel, TraceMetadata};

pub struct App {
    /// CPU throttle factor (reference note only: shown in the report header,
    /// times are not rescaled).
    pub throttle_factor: f64,
    pub summary: SummaryResult,
    pub scroll_frames: ScrollFrameResult,
    pub cpu_profile: CpuProfileResult,
    pub layout_dirty: LayoutDirtyResult,
    pub style_recalc: StyleRecalcResult,
    pub forced_reflows: ForcedReflowResult,
    pub jank: JankResult,
    pub loading: LoadingResult,
    pub network: NetworkResult,
    pub raster: RasterResult,
    pub official_long_tasks: Vec<LatencyEntry>,
    pub thread_model: ThreadModel,
    pub renderer_summaries: Vec<(chperf_core::trace::RendererThread, SummaryResult)>,
    pub renderer_janks: Vec<(chperf_core::trace::RendererThread, JankResult)>,
    pub trace_stats: ParseStats,
    pub thread_count: usize,
    pub trace_span_us: f64,
    pub frame_budget_us: f64,
    pub main_tid: u64,
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
        loading: LoadingResult,
        network: NetworkResult,
        raster: RasterResult,
        official_long_tasks: Vec<LatencyEntry>,
        thread_model: ThreadModel,
        renderer_summaries: Vec<(chperf_core::trace::RendererThread, SummaryResult)>,
        renderer_janks: Vec<(chperf_core::trace::RendererThread, JankResult)>,
        trace_stats: ParseStats,
        thread_count: usize,
        trace_span_us: f64,
        frame_budget_us: f64,
        main_tid: u64,
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
            loading,
            network,
            raster,
            official_long_tasks,
            thread_model,
            renderer_summaries,
            renderer_janks,
            trace_stats,
            thread_count,
            trace_span_us,
            frame_budget_us,
            main_tid,
            compare,
            trace_name_a,
            trace_name_b,
            metadata,
        }
    }
}
