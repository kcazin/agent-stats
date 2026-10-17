//! `agent-stats`: one binary, every role. Each crate owns its subcommands; this file only dispatches.

use std::process::ExitCode;

const USAGE: &str = "usage: agent-stats <command> [args]

Reporter (every machine):
  report [--once] [--dry | --print]   run the reporter (what the service runs)
  join <hub-url> [--code CODE]        add a hub this machine reports to (pairing code from `hub pair`)
  hubs [list | remove <name>]         hubs this machine reports to
  status                              what's running here and when it last reached each hub
  doctor                              check config, transcripts, hubs and the service

Hub (one machine, or the cloud):
  hub serve | pair [--label L] [--replace M] | machines | revoke <machine> | purge <machine> | repos | projects

Relay (optional public snapshot):
  relay serve

Install and updates:
  service install <report|hub|relay|updater> [--every 15m] | uninstall <role> | status
  update [--version V] | update --auto (what the updater timer runs) | rollback
  version";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    // launchd appends a long-running role's output to state_dir()/logs/<role>.log; keep it bounded across restarts.
    // (systemd sends it to the journal, so this is a no-op there.)
    let serving = match cmd {
        "report" => !rest.iter().any(|a| a == "--once" || a == "--dry" || a == "--print"),
        "hub" | "relay" => rest.first().map(String::as_str) == Some("serve"),
        _ => false,
    };
    // One-shot commands get the default SIGPIPE so `agent-stats hub repos | head` ends quietly instead of panicking.
    // Servers keep Rust's ignored SIGPIPE: a client hanging up must surface as an error, not kill the process.
    #[cfg(unix)]
    if !serving {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    if serving {
        let log = agent_stats_common::state_dir().join("logs").join(format!("{cmd}.log"));
        agent_stats_common::rotate_log(&log, 10 << 20, 3);
    }
    match cmd {
        "report" | "join" | "hubs" | "status" | "doctor" => agent_stats_reporter::cli_main(cmd, rest),
        "hub" => agent_stats_hub::main(rest),
        "relay" => agent_stats_relay::main(rest),
        "service" | "update" | "rollback" => agent_stats_ops::main(cmd, rest),
        "version" | "--version" | "-V" => {
            println!("agent-stats {} (protocol {})", agent_stats_common::VERSION, agent_stats_common::PROTOCOL);
            ExitCode::SUCCESS
        }
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("unknown command {cmd}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
