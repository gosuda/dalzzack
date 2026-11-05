//! CLI module — batch step execution engine and output shaping.

/// `--field` / `--jq` / `--raw` output filtering.
pub mod brief;
pub mod output;
mod steps;

pub use steps::{object_steps, read_steps, run_steps, Step};
