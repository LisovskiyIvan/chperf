//! Derived analyses over a parsed trace: summary, scroll frames, CPU profile,
//! style/layout/reflow diagnostics, two-trace comparison and jank clusters.

mod compare;
mod counters;
mod cpu;
mod jank;
mod layout;
mod loading;
mod network;
mod raster;
mod reflow;
mod scroll;
mod style;
mod summary;

pub use compare::*;
pub use counters::*;
pub use cpu::*;
pub use jank::*;
pub use layout::*;
pub use loading::*;
pub use network::*;
pub use raster::*;
pub use reflow::*;
pub use scroll::*;
pub use style::*;
pub use summary::*;
