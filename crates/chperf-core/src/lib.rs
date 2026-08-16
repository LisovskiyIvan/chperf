//! chperf-core: Chrome DevTools trace parsing and analysis, free of any CLI /
//! UI concerns. The `chperf-cli` crate layers argument parsing, report
//! rendering and the REPL on top.
//!
//! - [`trace`] — parse `.json` / `.json.gz` traces into [`trace::TraceEvent`]s
//! - [`analysis`] — derived analyses (summary, scroll, CPU, layout, jank, …)
//! - [`inspect`] — granular query sections (events, functions, stacks, …)
//! - [`windowed`] — anchor/window/delta/GC/calltree/CSV helpers

pub mod analysis;
pub mod inspect;
pub mod trace;
pub mod windowed;
