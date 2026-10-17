//! `agent-stats-reporter [--once] [--dry | --print]`: the reporter on its own, the same as `agent-stats report`.
//! Kept for the golden tests and installs that run this binary.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    agent_stats_reporter::cli_main("report", &args)
}
