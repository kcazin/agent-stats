//! The reporter: reads Claude Code, Codex and Grok transcripts incrementally and every 15 s reports daily token
//! totals and live sessions to its hubs. `agent-stats report|join|hubs|status|doctor` land in [`cli_main`]. See
//! docs/ARCHITECTURE.md and README.md.

mod app;
mod cli;
pub mod config;
mod git;
mod hub;
mod ingest;
mod js;
mod lines;
pub mod release;
mod report;
mod scan;
mod state;
mod t3;

pub use cli::cli_main;
pub use report::platform;
