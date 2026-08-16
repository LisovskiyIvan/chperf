//! Derived analyses over a parsed trace: summary, scroll frames, CPU profile,
//! style/layout/reflow diagnostics, two-trace comparison and jank clusters.

mod summary;
mod scroll;
mod cpu;
mod style;
mod layout;
mod reflow;
mod compare;
mod jank;

pub use summary::*;
pub use scroll::*;
pub use cpu::*;
pub use style::*;
pub use layout::*;
pub use reflow::*;
pub use compare::*;
pub use jank::*;
