//! `agent-stats machines`: the machines of this fleet, and adding more. `discover` finds candidates (Tailscale peers,
//! Bonjour `_agent-stats._tcp`), `add --ssh` installs and joins a machine over SSH, `invite` prints a line to paste
//! on one, `list` and `remove` manage the enrolled ones. Tokiscope draws these from their `--json` output; the
//! contracts are in docs/FLEET.md ("CLI contracts").

pub mod discover;
pub mod local;
mod out;
pub mod remote;

use std::process::ExitCode;
use std::time::Duration;

pub type Res<T> = Result<T, String>;

const USAGE: &str = "usage: agent-stats machines <command> [--json]
  discover [--timeout 3s]      machines on your tailnet and local network, and whether they run agent-stats
  add --ssh <user@host> [--label L] [--replace]
                               install agent-stats there over SSH and join it to this fleet (no code to type)
  invite [--label L]           a one-time line to paste on a machine you can't reach over SSH
  list                         machines of this fleet
  remove <machine> [--ssh <user@host>]
                               revoke a machine; with --ssh also stop and uninstall its reporter";

/// Options shared by the subcommands.
#[derive(Debug, Default)]
pub struct Args {
    pub json: bool,
    pub timeout: Option<Duration>,
    pub ssh: Option<String>,
    pub label: Option<String>,
    pub replace: bool,
    pub rest: Vec<String>,
}

/// `3s`, `1500ms`, `2` (seconds); between 0.5 s and 60 s.
fn parse_timeout(s: &str) -> Option<Duration> {
    let d = if let Some(ms) = s.strip_suffix("ms") {
        Duration::from_millis(ms.parse().ok()?)
    } else {
        Duration::from_secs_f64(s.strip_suffix('s').unwrap_or(s).parse::<f64>().ok().filter(|v| v.is_finite())?)
    };
    (Duration::from_millis(500)..=Duration::from_secs(60)).contains(&d).then_some(d)
}

pub fn parse_args(args: &[String]) -> Res<Args> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(x) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match x.as_str() {
            "--json" => a.json = true,
            "--replace" => a.replace = true,
            "--timeout" => a.timeout = Some(parse_timeout(&val("--timeout")?).ok_or("--timeout: like 3s or 1500ms")?),
            "--ssh" => a.ssh = Some(val("--ssh")?),
            "--label" => a.label = Some(val("--label")?),
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => a.rest.push(s.to_string()),
        }
    }
    Ok(a)
}

/// `agent-stats machines <subcommand> [args]`. (`cmd` is the top-level command, `machines`.)
pub fn main(cmd: &str, args: &[String]) -> ExitCode {
    if cmd != "machines" {
        eprintln!("unknown command {cmd}");
        return ExitCode::from(2);
    }
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if matches!(sub, "help" | "--help" | "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let a = match parse_args(&args[1..]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("agent-stats machines {sub}: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let res = match sub {
        "discover" => discover::run(&a),
        "add" => remote::add(&a),
        "invite" => local::invite(&a),
        "list" => local::list(&a),
        "remove" => remote::remove(&a),
        _ => Err(format!("unknown command {sub}\n\n{USAGE}")),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        // `add` has already said why, as an event.
        Err(e) if e.is_empty() => ExitCode::FAILURE,
        Err(e) => {
            if a.json {
                println!("{}", serde_json::json!({ "error": e }));
            }
            eprintln!("agent-stats machines {sub}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A machine name as hosts spell it, made comparable: `Killians-Mac-mini.local`, `killians-mac-mini` (a Tailscale
/// DNS label) and `Killian’s Mac mini` all become `killians-mac-mini`.
pub fn norm_name(s: &str) -> String {
    let s = s.trim().trim_end_matches('.');
    let s = s.strip_suffix(".local").unwrap_or(s);
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if c == '\'' || c == '’' {
            continue;
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_compare_across_spellings() {
        assert_eq!(norm_name("Killians-Mac-mini.local"), "killians-mac-mini");
        assert_eq!(norm_name("Killian’s Mac mini"), "killians-mac-mini");
        assert_eq!(norm_name("killians-mac-mini."), "killians-mac-mini");
        assert_eq!(norm_name("cachy"), "cachy");
    }

    #[test]
    fn args_and_timeouts() {
        let a = parse_args(&["--ssh".into(), "me@box".into(), "--json".into(), "--replace".into()]).unwrap();
        assert_eq!(a.ssh.as_deref(), Some("me@box"));
        assert!(a.json && a.replace);
        assert_eq!(parse_timeout("3s"), Some(Duration::from_secs(3)));
        assert_eq!(parse_timeout("1500ms"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_timeout("0.1"), None);
        assert!(parse_args(&["--bogus".into()]).is_err());
    }
}
