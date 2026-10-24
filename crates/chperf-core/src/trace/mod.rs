//! Trace loading and the parsed event model.
//!
//! Split across three submodules: [`event`] (the `TraceEvent` struct + name
//! interner), [`meta`] (top-level `TraceFile`/`TraceMetadata` and helpers)
//! and [`parse`] (the hand-rolled, block-parallel `traceEvents` tokenizer).

mod event;
mod meta;
mod parse;
mod profiler;

pub use event::TraceEvent;
pub use event::normalize_be_pairs;
#[cfg(test)]
pub(crate) use event::test_args;

pub use meta::{
    FrameInfo, RendererThread, ThreadModel, TraceFile, TraceMetadata, detect_main_thread,
    detect_thread_model, expand_trace_input, is_metadata_event, list_traces,
    list_traces_with_skipped, trace_stem,
};
pub use parse::{ParseStats, parse_trace};
pub use profiler::{
    ProfilerOverhead, effective_runtask_dur, find_profiler_overhead, is_profiler_dropped_frame,
};
