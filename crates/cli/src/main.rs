//! `agent-stats`: one binary, every role. Each crate owns its subcommands; this file only dispatches.

use std::process::ExitCode;

const USAGE: &str = "usage: agent-stats <command> [args]

Reporter (every machine):
  report [--once] [--dry | --print]   run the reporter (what the service runs)
  join <hub-url> [--code CODE]        add a hub this machine reports to (pairing code from `hub pair`)
  hubs [list | remove <name> | view-token [<name>]]
                                      hubs this machine reports to; view-token: a read-only token for a widget
  status                              what's running here and when it last reached each hub
  doctor                              check config, transcripts, hubs and the service

Fleet (every Mac a replica, docs/FLEET.md):
  fleet status | create | replica | request <url> | requests | approve <id> | deny <id>   [--json]
                                      start a fleet, make this machine a replica, ask to join, answer requests
  fleet discover | add --ssh <user@host> | invite | list | remove <machine>   [--json]
                                      find machines, install and join one over SSH, or print a line to paste

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
    // An app's launchd job can't name a file under the home directory for its output, so when an app runs the
    // services (and nobody is watching the output), a role appends to the log `service install` would have given it.
    #[cfg(unix)]
    {
        let role = if serving { Some(cmd) } else { (cmd == "update" && rest == ["--auto"]).then_some("updater") };
        if let Some(role) =
            role.filter(|_| unsafe { libc::isatty(1) } == 0 && agent_stats_common::managed_by().is_some())
        {
            log_to(&agent_stats_common::state_dir().join("logs").join(format!("{role}.log")));
        }
    }
    match cmd {
        "report" | "join" | "hubs" | "status" | "doctor" => agent_stats_reporter::cli_main(cmd, rest),
        "hub" => agent_stats_hub::main(rest),
        "relay" => agent_stats_relay::main(rest),
        "service" | "update" | "rollback" => agent_stats_ops::main(cmd, rest),
        "fleet" => agent_stats_fleet::main(rest),
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

/// Sends stdout and stderr to the end of `path` (rotated like a service log first). Best effort: without it the
/// output goes where it went before.
#[cfg(unix)]
fn log_to(path: &std::path::Path) {
    use std::os::fd::AsRawFd;
    agent_stats_common::rotate_log(path, 10 << 20, 3);
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(path));
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        unsafe {
            libc::dup2(f.as_raw_fd(), 1);
            libc::dup2(f.as_raw_fd(), 2);
        }
    }
}
