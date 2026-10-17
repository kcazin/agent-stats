//! New-repository events. A report can name thousands of repositories the hub has never seen, so nothing here costs a
//! thread or a request per repository: a report's new repositories become one event, queued on a bounded channel
//! (full = dropped and logged) and POSTed to `new_repo_webhook` by a single long-lived worker. With no webhook
//! configured there is no worker and nothing is queued.

use crate::config::Config;
use crate::json::{Obj, J};
use crate::validate::Report;
use std::collections::HashMap;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::time::Duration;

/// New repositories one machine may add in one report; rows of further unseen repositories are dropped.
pub const MAX_NEW_PER_REPORT: usize = 200;
/// Repositories named in one webhook event (and logged one by one); the rest are a count.
pub const MAX_LISTED: usize = 50;
/// Events waiting for the worker.
const QUEUE: usize = 256;

struct NewRepo {
    repo: String,
    remote: Option<String>,
    tokens_today: i64,
    project: Option<String>,
}

/// What one report added.
pub struct Event {
    machine: String,
    first_seen: f64,
    count: usize,
    listed: Vec<NewRepo>,
}

impl Event {
    /// `{machine, firstSeen, count, more, repos: [{repo, remote, tokensToday, project}]}`; for a single repository
    /// also its fields at the top level, the shape this webhook always had.
    fn body(&self) -> J {
        let repos: Vec<J> = self
            .listed
            .iter()
            .map(|n| {
                Obj::new()
                    .with("repo", n.repo.as_str())
                    .with("remote", n.remote.clone())
                    .with("tokensToday", n.tokens_today)
                    .with("project", n.project.clone())
                    .into()
            })
            .collect();
        let mut o = Obj::new();
        if let (1, [n]) = (self.count, self.listed.as_slice()) {
            o.set("repo", J::Str(n.repo.clone()));
            o.set("remote", n.remote.clone().into());
            o.set("machine", J::Str(self.machine.clone()));
            o.set("firstSeen", self.first_seen.into());
            o.set("tokensToday", n.tokens_today.into());
            o.set("project", n.project.clone().into());
        } else {
            o.set("machine", J::Str(self.machine.clone()));
            o.set("firstSeen", self.first_seen.into());
        }
        o.set("count", J::from(self.count));
        o.set("more", J::from(self.count - self.listed.len()));
        o.set("repos", J::Arr(repos));
        J::Obj(o)
    }
}

pub struct Notifier {
    tx: SyncSender<Event>,
}

impl Notifier {
    /// Starts the worker thread; an error means the OS refused a thread (no webhook then, the hub carries on).
    pub fn start(url: String, http: ureq::Agent) -> std::io::Result<Notifier> {
        let (tx, rx) = sync_channel::<Event>(QUEUE);
        std::thread::Builder::new().name("hub-webhook".into()).spawn(move || {
            for ev in rx {
                let body = ev.body().stringify();
                let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    http.post(&url)
                        .config()
                        .timeout_global(Some(Duration::from_secs(5)))
                        .build()
                        .header("content-type", "application/json")
                        .send(body.as_bytes())
                }));
                match sent {
                    Ok(Ok(r)) if r.status().is_success() => {}
                    Ok(Ok(r)) => eprintln!("hub: new_repo_webhook answered {}", r.status()),
                    Ok(Err(e)) => eprintln!("hub: new_repo_webhook failed: {e}"),
                    Err(_) => eprintln!("hub: new_repo_webhook panicked, continuing"),
                }
            }
        })?;
        Ok(Notifier { tx })
    }

    fn send(&self, ev: Event) {
        match self.tx.try_send(ev) {
            Ok(()) => {}
            Err(TrySendError::Full(ev)) => {
                eprintln!("hub: new_repo_webhook queue full, dropped the event for {} new repositories", ev.count)
            }
            Err(TrySendError::Disconnected(_)) => eprintln!("hub: new_repo_webhook worker is gone"),
        }
    }
}

/// Logs a report's new repositories (the first `MAX_LISTED` by name, then a count) and, if there's a webhook, queues
/// one event for them. `new` is in report order.
pub fn announce(
    cfg: &Config,
    notifier: Option<&Notifier>,
    r: &Report,
    now: f64,
    today: &str,
    new: Vec<(String, Option<String>)>,
) {
    // Tokens today per repository, once for the whole report.
    let mut tokens: HashMap<&str, i64> = HashMap::new();
    if notifier.is_some() {
        for d in r.repos.iter().filter(|d| d.day == today) {
            let t = tokens.entry(d.repo.as_str()).or_insert(0);
            *t = t.saturating_add(d.tokens);
        }
    }
    let count = new.len();
    let mut listed = Vec::with_capacity(count.min(MAX_LISTED));
    for (repo, remote) in new.into_iter().take(MAX_LISTED) {
        let project = cfg.project_of(Some(&repo), remote.as_deref()).map(|p| p.key.clone());
        eprintln!(
            "hub: new repository {repo}{} from {} ({})",
            remote.as_deref().map(|r| format!(" ({r})")).unwrap_or_default(),
            r.machine,
            project.as_deref().unwrap_or("unassigned")
        );
        let tokens_today = tokens.get(repo.as_str()).copied().unwrap_or(0);
        listed.push(NewRepo { repo, remote, tokens_today, project });
    }
    if count > listed.len() {
        eprintln!("hub: and {} more new repositories from {}", count - listed.len(), r.machine);
    }
    if let Some(n) = notifier {
        n.send(Event { machine: r.machine.clone(), first_seen: now, count, listed });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(names: &[&str], count: usize) -> Event {
        Event {
            machine: "m".into(),
            first_seen: 5.0,
            count,
            listed: names
                .iter()
                .map(|n| NewRepo { repo: n.to_string(), remote: None, tokens_today: 7, project: Some("neon".into()) })
                .collect(),
        }
    }

    #[test]
    fn one_repository_keeps_the_old_shape() {
        let b = ev(&["a"], 1).body().stringify();
        assert!(
            b.starts_with(
                r#"{"repo":"a","remote":null,"machine":"m","firstSeen":5,"tokensToday":7,"project":"neon","count":1"#
            ),
            "{b}"
        );
    }

    #[test]
    fn many_repositories_are_one_event_with_a_count() {
        let b = ev(&["a", "b"], 130).body().stringify();
        assert!(b.starts_with(r#"{"machine":"m","firstSeen":5,"count":130,"more":128,"repos":[{"repo":"a""#), "{b}");
        assert!(!b.contains(r#""project":"neon","count":1,"#));
    }

    #[test]
    fn a_full_queue_drops_events() {
        // A notifier whose worker never runs: the 256th event fits, the next is dropped without blocking.
        let (tx, _rx) = sync_channel::<Event>(QUEUE);
        let n = Notifier { tx };
        for _ in 0..QUEUE + 10 {
            n.send(ev(&["a"], 1));
        }
    }
}
