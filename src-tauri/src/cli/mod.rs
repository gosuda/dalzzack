//! CLI module — batch step execution engine and output shaping.

/// `--field` / `--jq` / `--raw` output filtering.
pub mod output;
mod steps;

pub use steps::run_steps;
