//! Talking to hubs: the well-known descriptor, route selection with backoff, `POST /report`, and `POST /pair`.
//!
//! Each configured hub is a [`Link`] with its own route, backoff and acknowledgement state, so a slow or dead hub
//! never holds back another: every request has a bounded timeout, a hub in backoff is skipped, and the reporter
//! sends to its hubs in parallel.

use crate::config::Hub;
use agent_stats_common::WELL_KNOWN;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Backoff after a failure doubles from this…
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
/// …up to this.
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// While on a later route, look for an earlier (preferred) one this often.
pub const REPROBE: Duration = Duration::from_secs(300);
/// Routes a hub entry may grow to by learning a fleet's replicas.
pub const MAX_ROUTES: usize = 16;
/// `POST /report` and `POST /pair`.
pub const POST_TIMEOUT: Duration = Duration::from_secs(10);
/// `GET /.well-known/agent-stats`: small, so a dead route costs little.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// HTTP clients with the reporter's timeouts.
#[derive(Clone)]
pub struct Http {
    pub post: ureq::Agent,
    pub probe: ureq::Agent,
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(timeout.min(Duration::from_secs(4))))
        .http_status_as_error(false)
        // Never follow a redirect: a hub answers itself. Following one sent reports (and the bearer token) to
        // whatever the redirect named, and an HTML page answering 200 there passed for an acknowledgement.
        .max_redirects(0)
        // A report goes out every 15 s, longer than Node's 5 s keep-alive: a pooled connection would be stale.
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .user_agent(format!("agent-stats/{}", agent_stats_common::VERSION))
        .build()
        .into()
}

impl Http {
    pub fn new() -> Http {
        Http { post: agent(POST_TIMEOUT), probe: agent(PROBE_TIMEOUT) }
    }
}

impl Default for Http {
    fn default() -> Self {
        Http::new()
    }
}

/// `GET /.well-known/agent-stats`.
#[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub hub_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub protocol: Option<u32>,
    /// The zone the hub counts days in (hubs before 0.5 don't say).
    #[serde(default)]
    pub timezone: Option<String>,
    /// A fleet replica's name, and the routes of every replica of its fleet (docs/FLEET.md).
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub fleet_routes: Vec<String>,
}

/// A response body, shortened for messages.
fn snippet(body: &str) -> String {
    let b = body.trim();
    // A JSON error's message reads better than the whole object.
    let msg = serde_json::from_str::<serde_json::Value>(b)
        .ok()
        .and_then(|v| v.get("error").or_else(|| v.get("message")).and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| b.to_owned());
    let mut s: String = msg.chars().take(160).collect();
    if s.len() < msg.len() {
        s.push('…');
    }
    s
}

/// Why an answer that is not a plain 200 from the route itself is refused: a redirect is never followed.
fn not_followed(status: u16, location: Option<&str>) -> Option<String> {
    (300..400).contains(&status).then(|| {
        format!(
            "answered {status}, a redirect{} (not followed: not an agent-stats hub, or the route is wrong)",
            location.map(|l| format!(" to {l}")).unwrap_or_default()
        )
    })
}

/// Whether a body looks like an HTML page (a login portal, a proxy's error page, a web server's index).
fn is_html(body: &str) -> bool {
    let b = body.trim_start().get(..15).unwrap_or(body.trim_start()).to_ascii_lowercase();
    b.starts_with("<!doctype") || b.starts_with("<html") || b.starts_with("<?xml") || b.starts_with("<head")
}

fn location(res: &ureq::http::Response<ureq::Body>) -> Option<String> {
    res.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_owned)
}

pub fn well_known(http: &Http, route: &str) -> Result<Descriptor, String> {
    let res = http.probe.get(format!("{route}{WELL_KNOWN}")).call().map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    if let Some(e) = not_followed(status, location(&res).as_deref()) {
        return Err(format!("{WELL_KNOWN} {e}"));
    }
    let body = res.into_body().read_to_string().map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(format!("{WELL_KNOWN} answered {status}"));
    }
    serde_json::from_str(&body).map_err(|_| format!("{WELL_KNOWN} is not an agent-stats descriptor"))
}

/// Does this descriptor belong to the hub with `id`?
pub fn check(d: &Descriptor, id: &str) -> Result<(), String> {
    if let Some(role) = d.role.as_deref().filter(|r| *r != "hub") {
        return Err(format!("is a {role}, not a hub"));
    }
    match d.hub_id.as_deref() {
        Some(h) if h == id => Ok(()),
        Some(h) => Err(format!("is hub {h}, expected {id}")),
        None => Err("descriptor has no hubId".into()),
    }
}

/// `POST <route>/report`. Accepted only when the hub answers 200 with `{"ok": true}`: anything else (a redirect, an
/// HTML page, another JSON) did not come from an agent-stats hub that stored the report, so its keys stay unacked.
pub fn send(http: &Http, route: &str, token: &str, body: &[u8]) -> Result<(), String> {
    let res = http
        .post
        .post(format!("{route}/report"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .send(body)
        .map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    if let Some(e) = not_followed(status, location(&res).as_deref()) {
        return Err(format!("hub {e}"));
    }
    let text = res.into_body().read_to_string().unwrap_or_default();
    if status == 200 {
        let ok = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("ok")?.as_bool());
        return match ok {
            Some(true) => Ok(()),
            _ if is_html(&text) => Err("not an agent-stats hub (got an HTML page with 200, not {\"ok\":true})".into()),
            _ => Err(format!("not an agent-stats hub (200 without {{\"ok\":true}}: {})", snippet(&text))),
        };
    }
    let why = match status {
        401 => " (token rejected: join again with a new pairing code)",
        403 => " (this token belongs to another machine id)",
        413 => " (report too large)",
        426 => " (the hub doesn't speak this reporter's protocol: update the hub)",
        _ => "",
    };
    let detail = snippet(&text);
    Err(if detail.is_empty() {
        format!("hub answered {status}{why}")
    } else {
        format!("hub answered {status}{why}: {detail}")
    })
}

#[derive(Serialize, Debug)]
pub struct PairRequest<'a> {
    pub code: &'a str,
    pub machine: &'a str,
    pub label: &'a str,
    pub platform: &'a str,
    pub version: &'a str,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    pub hub_id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub token: String,
    #[serde(default)]
    pub routes: Vec<String>,
    /// The zone the hub counts days in (hubs before 0.5 don't say).
    #[serde(default)]
    pub timezone: Option<String>,
    /// A read-only token for a widget on this machine (hubs before 0.5 don't give one).
    #[serde(default)]
    pub view_token: Option<String>,
}

/// `POST <url>/view-token` with this machine's token: a new read-only token, replacing the one it had.
pub fn view_token(http: &Http, url: &str, token: &str) -> Result<String, String> {
    let res = http
        .post
        .post(format!("{url}/view-token"))
        .header("authorization", format!("Bearer {token}"))
        .send(&[][..])
        .map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    if let Some(e) = not_followed(status, location(&res).as_deref()) {
        return Err(format!("{url}/view-token {e}"));
    }
    let text = res.into_body().read_to_string().unwrap_or_default();
    match status {
        200 => {}
        404 => return Err("the hub has no /view-token: update it to 0.5 or later".into()),
        401 | 403 => return Err("the hub doesn't know this machine's token: join again".into()),
        _ => return Err(format!("hub answered {status}: {}", snippet(&text))),
    }
    #[derive(Deserialize)]
    struct V {
        #[serde(rename = "viewToken")]
        view_token: String,
    }
    serde_json::from_str::<V>(&text)
        .ok()
        .map(|v| v.view_token)
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| format!("unexpected /view-token answer: {}", snippet(&text)))
}

/// `POST <url>/pair`: swaps a pairing code for this machine's token.
pub fn pair(http: &Http, url: &str, req: &PairRequest) -> Result<PairResponse, String> {
    let res = http
        .post
        .post(format!("{url}/pair"))
        .header("content-type", "application/json")
        .send(serde_json::to_vec(req).unwrap_or_default().as_slice())
        .map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    if let Some(e) = not_followed(status, location(&res).as_deref()) {
        return Err(format!("{url}/pair {e}"));
    }
    let text = res.into_body().read_to_string().unwrap_or_default();
    if status != 200 {
        let why = match status {
            400 | 401 | 403 | 404 if text.to_ascii_lowercase().contains("code") => "",
            401 | 403 => " (the code is wrong, used or expired: make a new one with `agent-stats hub pair` on the hub)",
            404 => " (this server has no /pair: is it an agent-stats hub of version 0.2 or later?)",
            _ => "",
        };
        return Err(format!("hub answered {status}{why}: {}", snippet(&text)));
    }
    let r: PairResponse = serde_json::from_str(&text).map_err(|e| {
        if is_html(&text) {
            "not an agent-stats hub (/pair answered an HTML page)".to_owned()
        } else {
            format!("unexpected /pair answer: {e}")
        }
    })?;
    if r.token.trim().is_empty() {
        return Err("the hub returned an empty token".into());
    }
    if !valid_id(&r.hub_id) {
        return Err(format!("the hub returned an unusable hubId {:?}", r.hub_id));
    }
    Ok(r)
}

/// A hubId ends up in a file name.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// One configured hub and how reaching it is going.
#[derive(Debug, Clone)]
pub struct Link {
    pub hub: Hub,
    /// Index of the verified route in use.
    pub route: Option<usize>,
    /// For hubs without an id (nothing to verify): the route to try next.
    start: usize,
    pub backoff: Duration,
    pub next_try: Option<Instant>,
    last_probe: Option<Instant>,
    /// Seconds since the epoch.
    pub last_ok_at: Option<u64>,
    pub last_error: Option<String>,
    /// When this hub last received a full sync (ms since the epoch).
    pub last_full: f64,
    /// When a report was last attempted, and the live-session signature it carried (to send on change).
    pub last_sent: Option<Instant>,
    pub sent_sig: Option<u64>,
    /// The last error that was logged, to log each kind once.
    pub logged: Option<String>,
    /// Routes of other replicas of this hub's fleet, learned from its descriptor and not yet saved to config.toml.
    pub learned: Vec<String>,
}

impl Link {
    pub fn new(hub: Hub) -> Link {
        Link {
            hub,
            route: None,
            start: 0,
            backoff: Duration::ZERO,
            next_try: None,
            last_probe: None,
            last_ok_at: None,
            last_error: None,
            last_full: 0.0,
            last_sent: None,
            sent_sig: None,
            logged: None,
            learned: Vec::new(),
        }
    }

    /// Whether to send to this hub now: it has a token and a route, and is not backing off.
    pub fn due(&self, now: Instant) -> bool {
        self.hub.token.is_some() && !self.hub.routes.is_empty() && self.next_try.is_none_or(|t| now >= t)
    }

    pub fn route_url(&self) -> Option<&str> {
        self.route.and_then(|i| self.hub.routes.get(i)).map(String::as_str)
    }

    /// The first of `routes[range]` that is this hub.
    fn probe(&mut self, http: &Http, upto: usize, now: Instant) -> Result<usize, String> {
        self.last_probe = Some(now);
        let Some(id) = self.hub.id.clone() else {
            // Nothing to verify (a legacy hub): take routes in turn, starting after the last one that failed.
            return Ok(if upto < self.hub.routes.len() { 0 } else { self.start % self.hub.routes.len() });
        };
        let mut errors = Vec::new();
        let routes: Vec<String> = self.hub.routes.iter().take(upto).cloned().collect();
        for (i, r) in routes.iter().enumerate() {
            match well_known(http, r).and_then(|d| check(&d, &id).map(|()| d)) {
                Ok(d) => {
                    self.learn(&d.fleet_routes);
                    return Ok(i);
                }
                Err(e) => errors.push(format!("{r}: {e}")),
            }
        }
        Err(errors.join("; "))
    }

    /// A fleet's other replicas: their routes go after the ones this hub entry has (each is verified by id before
    /// it gets a report, like any route), so a reporter whose replica is away reports to another.
    fn learn(&mut self, routes: &[String]) {
        for r in routes {
            let r = crate::config::normalize_url(r);
            if self.hub.routes.len() >= MAX_ROUTES {
                break;
            }
            if !r.is_empty() && !self.hub.routes.iter().any(|o| crate::config::same_route(o, &r)) {
                self.hub.routes.push(r.clone());
                self.learned.push(r);
            }
        }
    }

    /// Picks the route to use. A hub with an id re-verifies the route in use (route 0 included) at least every
    /// `REPROBE`, by probing the routes in order again: an earlier one that answers takes over, and a route whose
    /// descriptor no longer has this hub's id is skipped (a bare hostname may resolve to another machine on another
    /// network, which must not get the token or the report). A route is also verified whenever it is selected,
    /// which includes the next attempt after any failure (a failure drops the route). No per-request probe.
    fn pick(&mut self, http: &Http, now: Instant) -> Result<usize, String> {
        let stale = self.last_probe.is_none_or(|t| now.duration_since(t) >= REPROBE);
        match self.route {
            Some(i) if self.hub.id.is_some() => {
                if !stale {
                    return Ok(i);
                }
                self.route = None;
                let i = self.probe(http, self.hub.routes.len(), now)?;
                self.route = Some(i);
                Ok(i)
            }
            // No id, nothing to verify: stay, or go back to the first route now and then.
            Some(0) => Ok(0),
            Some(i) => {
                if stale {
                    if let Ok(better) = self.probe(http, i, now) {
                        self.route = Some(better);
                        return Ok(better);
                    }
                }
                Ok(i)
            }
            None => {
                let i = self.probe(http, self.hub.routes.len(), now)?;
                self.route = Some(i);
                Ok(i)
            }
        }
    }

    /// Sends `body` to this hub. On failure the route is dropped (re-probed next time) and the hub backs off.
    pub fn attempt(&mut self, http: &Http, body: &[u8], now: Instant) -> Result<(), String> {
        let res = self.pick(http, now).and_then(|i| {
            let token = self.hub.token.as_deref().unwrap_or_default();
            send(http, &self.hub.routes[i], token, body).map_err(|e| format!("{}: {e}", self.hub.routes[i]))
        });
        match &res {
            Ok(()) => {
                self.backoff = Duration::ZERO;
                self.next_try = None;
                self.last_ok_at = Some(agent_stats_common::now_secs());
                self.last_error = None;
            }
            Err(e) => {
                if let Some(i) = self.route.take() {
                    self.start = i + 1;
                }
                self.backoff = if self.backoff.is_zero() { BACKOFF_MIN } else { (self.backoff * 2).min(BACKOFF_MAX) };
                self.next_try = Some(now + self.backoff);
                self.last_error = Some(e.clone());
            }
        }
        res
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A request a fake server received.
    #[derive(Debug, Clone)]
    pub struct Req {
        pub method: String,
        pub path: String,
        pub auth: Option<String>,
        pub body: String,
    }

    /// A tiny HTTP/1.1 server on 127.0.0.1 answering with `handler(request) -> (status, body)`.
    pub struct Fake {
        pub url: String,
        pub reqs: Arc<Mutex<Vec<Req>>>,
    }

    pub fn serve(handler: impl Fn(&Req) -> (u16, String) + Send + Sync + 'static) -> Fake {
        serve_with(move |r| {
            let (status, body) = handler(r);
            (status, vec![("content-type".into(), "application/json".into())], body)
        })
    }

    /// Like [`serve`], with the response headers chosen by the handler.
    pub fn serve_with(handler: impl Fn(&Req) -> (u16, Vec<(String, String)>, String) + Send + Sync + 'static) -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let r2 = reqs.clone();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in l.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut rd = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if rd.read_line(&mut line).is_err() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned());
                let (mut len, mut auth) = (0usize, None);
                loop {
                    let mut h = String::new();
                    if rd.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap_or((&h, ""));
                    match k.trim().to_ascii_lowercase().as_str() {
                        "content-length" => len = v.trim().parse().unwrap_or(0),
                        "authorization" => auth = Some(v.trim().to_owned()),
                        _ => {}
                    }
                }
                let mut body = vec![0; len];
                let _ = rd.read_exact(&mut body);
                let req = Req { method, path, auth, body: String::from_utf8_lossy(&body).into_owned() };
                r2.lock().unwrap().push(req.clone());
                let (status, headers, out) = handler(&req);
                let headers: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{out}",
                    out.len()
                );
            }
        });
        Fake { url, reqs }
    }

    pub fn hub_server(id: &'static str, report_status: u16) -> Fake {
        serve(move |r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", WELL_KNOWN) => (200, format!(r#"{{"role":"hub","hubId":"{id}","name":"n","protocol":2}}"#)),
            ("POST", "/report") => (report_status, r#"{"ok":true}"#.into()),
            _ => (404, String::new()),
        })
    }

    pub fn hub(name: &str, id: Option<&str>, routes: &[&str]) -> Hub {
        Hub {
            name: name.into(),
            id: id.map(str::to_owned),
            routes: routes.iter().map(|r| r.to_string()).collect(),
            token: Some("tok".into()),
            token_source: "test".into(),
            legacy: id.is_none(),
        }
    }

    /// A port nothing listens on.
    pub fn dead_url() -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    }

    #[test]
    fn learns_the_other_replicas_of_a_fleet_and_fails_over_to_them() {
        let http = Http::new();
        let air = hub_server("abc", 200);
        let air_url = air.url.clone();
        let mini = serve(move |r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", WELL_KNOWN) => (
                200,
                format!(
                    r#"{{"role":"hub","hubId":"abc","node":"mini","fleetRoutes":["http://mini.example:7777","{air_url}/"]}}"#
                ),
            ),
            ("POST", "/report") => (200, r#"{"ok":true}"#.into()),
            _ => (404, String::new()),
        });
        let mut l = Link::new(hub("home", Some("abc"), &[&mini.url]));
        l.attempt(&http, b"{}", Instant::now()).unwrap();
        assert_eq!(l.hub.routes, [mini.url.clone(), "http://mini.example:7777".into(), air.url.clone()]);
        assert_eq!(l.learned.len(), 2);
        // The replica it joined goes away: the next report goes to one it learned.
        l.hub.routes[0] = dead_url();
        l.route = None;
        l.next_try = None;
        l.attempt(&http, b"{}", Instant::now()).unwrap();
        assert_eq!(l.route_url(), Some(air.url.as_str()));
    }

    #[test]
    fn picks_the_first_route_that_is_this_hub_and_reprobes_earlier_ones() {
        let http = Http::new();
        let other = hub_server("other", 200);
        let right = hub_server("abc", 200);
        let dead = dead_url();
        let mut l = Link::new(hub("h", Some("abc"), &[&dead, &other.url, &right.url]));
        let t0 = Instant::now();
        l.attempt(&http, b"{}", t0).unwrap();
        assert_eq!(l.route, Some(2));
        assert_eq!(right.reqs.lock().unwrap().iter().filter(|r| r.path == "/report").count(), 1);
        assert!(other.reqs.lock().unwrap().iter().all(|r| r.path == WELL_KNOWN), "never reports to the wrong hub");
        assert_eq!(right.reqs.lock().unwrap().last().unwrap().auth.as_deref(), Some("Bearer tok"));
        // Within the re-probe interval the earlier routes are not tried again.
        let probes = other.reqs.lock().unwrap().len();
        l.attempt(&http, b"{}", t0 + Duration::from_secs(15)).unwrap();
        assert_eq!(other.reqs.lock().unwrap().len(), probes);
        // An earlier route that becomes this hub is found at the next re-probe.
        l.hub.routes[1] = right.url.clone();
        l.attempt(&http, b"{}", t0 + REPROBE + Duration::from_secs(1)).unwrap();
        assert_eq!(l.route, Some(1));
    }

    #[test]
    fn failures_back_off_and_legacy_hubs_skip_the_check() {
        let http = Http::new();
        let wrong = hub_server("other", 200);
        let mut l = Link::new(hub("h", Some("abc"), &[&wrong.url]));
        let t0 = Instant::now();
        let e = l.attempt(&http, b"{}", t0).unwrap_err();
        assert!(e.contains("is hub other, expected abc"), "{e}");
        assert_eq!(l.backoff, BACKOFF_MIN);
        assert!(!l.due(t0));
        assert!(l.due(t0 + BACKOFF_MIN));
        for _ in 0..20 {
            let _ = l.attempt(&http, b"{}", t0);
        }
        assert_eq!(l.backoff, BACKOFF_MAX);
        // No id: no descriptor needed (the JS hub has none); a failing route moves on to the next.
        let js_hub = serve(|r| if r.path == "/report" { (200, r#"{"ok":true}"#.into()) } else { (404, String::new()) });
        let dead = dead_url();
        let mut l = Link::new(hub("default", None, &[&dead, &js_hub.url]));
        assert!(l.attempt(&http, b"{}", t0).is_err());
        l.attempt(&http, b"{}", t0).unwrap();
        assert_eq!(l.route, Some(1));
        assert!(js_hub.reqs.lock().unwrap().iter().all(|r| r.path == "/report"));
        // 401 explains itself.
        let denied = serve(|_| (401, r#"{"error":"bad token"}"#.into()));
        let mut l = Link::new(hub("x", None, &[&denied.url]));
        let e = l.attempt(&http, b"{}", t0).unwrap_err();
        assert!(e.contains("401") && e.contains("join again") && e.contains("bad token"), "{e}");
    }

    /// A web server that redirects everything to an HTML page answering 200 (a captive portal, a proxy, a stale
    /// DNS name pointing at a web host).
    fn redirector() -> (Fake, Fake) {
        let page = serve_with(|_| {
            (200, vec![("content-type".into(), "text/html".into())], "<!DOCTYPE html><html>hi</html>".into())
        });
        let to = page.url.clone();
        let front = serve_with(move |_| (302, vec![("location".into(), format!("{to}/login"))], String::new()));
        (front, page)
    }

    #[test]
    fn redirects_are_not_followed_and_only_ok_true_is_an_acknowledgement() {
        let http = Http::new();
        let (front, page) = redirector();
        // POST /report: a 302 is an error, and the target never sees the token or the body.
        let e = send(&http, &front.url, "tok", b"{}").unwrap_err();
        assert!(e.contains("302") && e.contains("not followed"), "{e}");
        assert!(page.reqs.lock().unwrap().is_empty(), "the redirect target was contacted");
        // An HTML 200 straight from the route, or 200 with other JSON, is not an acknowledgement either.
        let e = send(&http, &page.url, "tok", b"{}").unwrap_err();
        assert!(e.contains("not an agent-stats hub") && e.contains("HTML"), "{e}");
        let other = serve(|_| (200, r#"{"ok":false}"#.into()));
        assert!(send(&http, &other.url, "tok", b"{}").unwrap_err().contains("not an agent-stats hub"));
        let other = serve(|_| (204, String::new()));
        assert!(send(&http, &other.url, "tok", b"{}").is_err());
        let good = serve(|_| (200, r#"{"ok":true}"#.into()));
        send(&http, &good.url, "tok", b"{}").unwrap();
        // The well-known probe and /pair don't follow redirects either.
        let e = well_known(&http, &front.url).unwrap_err();
        assert!(e.contains("302") && e.contains("not followed"), "{e}");
        let req = PairRequest { code: "C", machine: "m", label: "l", platform: "linux", version: "0" };
        let e = pair(&http, &front.url, &req).unwrap_err();
        assert!(e.contains("302") && e.contains("not followed"), "{e}");
        let e = pair(&http, &page.url, &req).unwrap_err();
        assert!(e.contains("not an agent-stats hub"), "{e}");
        assert!(page.reqs.lock().unwrap().iter().all(|r| r.path != "/login"), "a redirect was followed");
        // A Link with a redirecting route keeps its keys: the attempt fails.
        let mut l = Link::new(hub("legacy", None, &[&front.url]));
        assert!(l.attempt(&http, b"{}", Instant::now()).is_err());
        assert!(l.last_ok_at.is_none());
    }

    #[test]
    fn the_route_in_use_is_reverified_even_route_0() {
        let http = Http::new();
        let id = Arc::new(Mutex::new("abc".to_owned()));
        let id2 = id.clone();
        let first = serve(move |r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", WELL_KNOWN) => (200, format!(r#"{{"role":"hub","hubId":"{}"}}"#, id2.lock().unwrap())),
            ("POST", "/report") => (200, r#"{"ok":true}"#.into()),
            _ => (404, String::new()),
        });
        let second = hub_server("abc", 200);
        let mut l = Link::new(hub("h", Some("abc"), &[&first.url, &second.url]));
        let t0 = Instant::now();
        l.attempt(&http, b"{}", t0).unwrap();
        assert_eq!(l.route, Some(0));
        let count = |f: &Fake, p: &str| f.reqs.lock().unwrap().iter().filter(|r| r.path == p).count();
        // No per-request probe within the interval.
        l.attempt(&http, b"{}", t0 + Duration::from_secs(15)).unwrap();
        assert_eq!(count(&first, WELL_KNOWN), 1);
        // The name now resolves to another machine (another hub): after the interval route 0 is checked again,
        // skipped, and the report goes to the next route that is this hub.
        *id.lock().unwrap() = "evil".into();
        let reports_before = count(&first, "/report");
        l.attempt(&http, b"{}", t0 + REPROBE + Duration::from_secs(1)).unwrap();
        assert_eq!(count(&first, WELL_KNOWN), 2);
        assert_eq!(count(&first, "/report"), reports_before, "the wrong host got a report");
        assert_eq!(l.route, Some(1));
        assert_eq!(count(&second, "/report"), 1);
        // After a failure the route is verified again before the next send.
        let mut l = Link::new(hub("h", Some("abc"), &[&first.url]));
        *id.lock().unwrap() = "abc".into();
        l.attempt(&http, b"{}", t0).unwrap();
        l.route = Some(0);
        l.hub.routes = vec![dead_url()];
        assert!(l.attempt(&http, b"{}", t0 + Duration::from_secs(1)).is_err());
        assert_eq!(l.route, None);
        l.hub.routes = vec![first.url.clone()];
        let probes = count(&first, WELL_KNOWN);
        l.attempt(&http, b"{}", t0 + Duration::from_secs(5)).unwrap();
        assert_eq!(count(&first, WELL_KNOWN), probes + 1);
    }
}
