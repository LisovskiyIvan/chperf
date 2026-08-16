//! Trace loading and the parsed event model.
//!
//! Split across three submodules: [`event`] (the `TraceEvent` struct + name
//! interner), [`meta`] (top-level `TraceFile`/`TraceMetadata` and helpers)
//! and [`parse`] (the hand-rolled, block-parallel `traceEvents` tokenizer).

mod event;
mod meta;
mod parse;

pub use event::TraceEvent;
pub(crate) use event::intern_name;
#[cfg(test)]
pub(crate) use event::test_args;

pub use meta::{TraceFile, TraceMetadata, detect_main_thread, is_metadata_event, list_traces, trace_stem};
pub use parse::parse_trace;
