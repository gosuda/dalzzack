//! Tool implementations for browser automation.
//!
//! Each tool is a standalone async function invoked via `execute_tool()` dispatcher.
//! Called from the CLI daemon (socket server) and batch step engine.

pub mod definitions;
/// Navigation timeline, failed requests and the live `record` switch.
pub mod network;
