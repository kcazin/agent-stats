//! The hub's side of a fleet (docs/FLEET.md): the descriptor's fleet fields, `/sync/*` and `/join-request`, and
//! the sync round that pulls the other replicas every few seconds.

use crate::fleet::{self, PeerState, PAGE};
use crate::http::{Request, Response};
use crate::json::{Obj, J};
use crate::server::Hub;
use crate::sync;
use rusqlite::Connection;
use std::time::{Duration, Instant};

/// Who called a `/sync` route.
enum Caller {
    Replica(String),
    Machine(String),
}

impl Caller {
    fn name(&self) -> &str {
        match self {
            Caller::Replica(n) | Caller::Machine(n) => n,
        }
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A response body, shortened for a log line.
fn snippet(body: &str) -> String {
    let msg = J::parse(body)
        .ok()
        .and_then(|j| j.get("error").and_then(J::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.trim().to_owned());
    msg.chars().take(160).collect()
}

impl Hub {
    fn me_always_on(&self) -> bool {
        self.cfg.fleet.as_ref().is_some_and(|f| f.always_on)
    }

    /// The replica that publishes for the fleet (this hub when it is not in one).
    pub fn publisher(&self, db: &Connection, now: f64) -> String {
        let Some(me) = &self.node else { return String::new() };
        let list = fleet::members(db).unwrap_or_default();
        fleet::publisher(&list, me, self.me_always_on(), now)
    }

    /// Whether this hub publishes, keeps the pile and asks GitHub: always outside a fleet, else when it is the
    /// fleet's publisher.
    pub fn publishes(&self, db: &Connection, now: f64) -> bool {
        match &self.node {
            None => true,
            Some(me) => self.publisher(db, now) == *me,
        }
    }

    /// `GET /.well-known/agent-stats`. A replica adds its node and every replica's routes (`fleetRoutes`), which
    /// reporters learn.
    pub fn descriptor(&self) -> Obj {
        let mut o = Obj::new()
            .with("role", "hub")
            .with("hubId", self.hub_id.as_str())
            .with("name", self.cfg.name.as_str())
            .with("version", agent_stats_common::VERSION)
            .with("protocol", agent_stats_common::PROTOCOL as i64)
            .with("routes", J::Arr(self.cfg.routes().into_iter().map(J::Str).collect()))
            .with("timezone", self.cfg.tz_name.as_str());
        if let Some(me) = &self.node {
            let list = fleet::members(&lock(&self.db)).unwrap_or_default();
            let routes = fleet::fleet_routes(&list, me, self.me_always_on(), &self.cfg.routes());
            o = o.with("node", me.as_str()).with("fleetRoutes", J::Arr(routes.into_iter().map(J::Str).collect()));
        }
        o
    }

    fn caller(&self, req: &Request) -> Option<Caller> {
        let t = req.bearer()?;
        let db = lock(&self.db);
        if let Ok(Some(n)) = fleet::node_of_replica_token(&db, t) {
            return Some(Caller::Replica(n));
        }
        crate::auth::machine_of_token(&db, t).ok().flatten().map(Caller::Machine)
    }

    /// `GET /sync/changes?since=SEQ&limit=N`: a page of this hub's log, for a replica or a machine of the fleet.
    pub(crate) fn sync_changes(&self, req: &Request) -> Response {
        if req.method != "GET" {
            return Response::error(405, "method not allowed");
        }
        if self.caller(req).is_none() {
            return Response::error(401, "unauthorized");
        }
        let since: i64 = req.query_param("since").and_then(|s| s.parse().ok()).unwrap_or(0).max(0);
        let limit: usize = req.query_param("limit").and_then(|s| s.parse().ok()).unwrap_or(PAGE).clamp(1, 5000);
        let db = lock(&self.db);
        let res = sync::changes(&db, since, limit).and_then(|page| Ok((page, sync::db_id(&db)?)));
        match res {
            Ok(((rows, seq, more), id)) => Response::json(
                200,
                J::Obj(
                    Obj::new()
                        .with("node", self.node.clone())
                        .with("db", id)
                        .with("seq", seq)
                        .with("more", more)
                        .with("rows", J::Arr(rows)),
                )
                .stringify(),
            ),
            Err(e) => Response::error(500, &e.to_string()),
        }
    }

    /// `POST /sync/hello`: a replica says where it is and which token it will show.
    pub(crate) fn sync_hello(&self, req: &Request) -> Response {
        if req.method != "POST" {
            return Response::error(405, "method not allowed");
        }
        let Some(who) = self.caller(req) else { return Response::error(401, "unauthorized") };
        let body = std::str::from_utf8(&req.body).ok().and_then(|s| J::parse(s).ok()).unwrap_or(J::Null);
        let node = body.get("node").and_then(J::as_str).unwrap_or("");
        if node != who.name() {
            return Response::error(403, &format!("this token belongs to {}, not {node}", who.name()));
        }
        match fleet::hello(&lock(&self.db), node, &body, self.clock.now()) {
            Ok(()) => {
                if matches!(who, Caller::Machine(_)) {
                    eprintln!("hub: {node} joined as a replica");
                }
                Response::json(200, J::Obj(Obj::new().with("ok", true).with("node", self.node.clone())).stringify())
            }
            Err((status, msg)) => Response::error(status, &msg),
        }
    }

    /// `POST /join-request`: a machine without a code asks to join; someone approves it on any replica.
    pub(crate) fn join_request(&self, req: &Request) -> Response {
        if req.method != "POST" {
            return Response::error(405, "method not allowed");
        }
        if !self.limiter.reserve(req.peer) {
            return Response::error(429, "too many join requests; wait a few minutes");
        }
        let body = std::str::from_utf8(&req.body).ok().and_then(|s| J::parse(s).ok()).unwrap_or(J::Null);
        match fleet::create_request(&lock(&self.db), &body, self.clock.now()) {
            Ok((id, digits, expires)) => {
                eprintln!(
                    "hub: join request from {} ({digits})",
                    crate::admin::printable(body.get("machine").and_then(J::as_str).unwrap_or(""))
                );
                self.note_change();
                Response::json(
                    200,
                    J::Obj(
                        Obj::new()
                            .with("id", id)
                            .with("digits", digits)
                            .with("expires", expires)
                            .with("hubId", self.hub_id.as_str())
                            .with("name", self.cfg.name.as_str()),
                    )
                    .stringify(),
                )
            }
            Err((status, msg)) => {
                self.limiter.refund(req.peer);
                Response::error(status, &msg)
            }
        }
    }

    /// `GET /join-request/<id>?key=<sha256 of the requester's token>`: pending, approved, denied or expired, with
    /// what the requester needs to join once approved.
    pub(crate) fn join_request_status(&self, req: &Request) -> Response {
        if req.method != "GET" {
            return Response::error(405, "method not allowed");
        }
        let id = req.path.trim_start_matches("/join-request/");
        let key = req.query_param("key").unwrap_or_default();
        if id.is_empty() || id.len() > 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Response::error(404, "not found");
        }
        let status = fleet::request_status(&lock(&self.db), id, key, self.clock.now());
        match status {
            Ok(Some(s)) => {
                let routes = self
                    .descriptor()
                    .get("fleetRoutes")
                    .cloned()
                    .unwrap_or_else(|| J::Arr(self.cfg.routes().into_iter().map(J::Str).collect()));
                Response::json(
                    200,
                    J::Obj(
                        Obj::new()
                            .with("status", s)
                            .with("hubId", self.hub_id.as_str())
                            .with("name", self.cfg.name.as_str())
                            .with("timezone", self.cfg.tz_name.as_str())
                            .with("routes", routes),
                    )
                    .stringify(),
                )
            }
            Ok(None) => Response::error(404, "not found"),
            Err(e) => Response::error(500, &e.to_string()),
        }
    }

    // ---- pulling ----

    fn http(
        &self,
        method: &str,
        url: &str,
        token: Option<&str>,
        body: Option<&str>,
        timeout: Duration,
    ) -> Result<(u16, String), String> {
        let res = match (method, body) {
            ("POST", b) => {
                let mut r = self.sync_http.post(url).config().timeout_global(Some(timeout)).build();
                if let Some(t) = token {
                    r = r.header("authorization", format!("Bearer {t}"));
                }
                r.header("content-type", "application/json").send(b.unwrap_or("{}"))
            }
            _ => {
                let mut r = self.sync_http.get(url).config().timeout_global(Some(timeout)).build();
                if let Some(t) = token {
                    r = r.header("authorization", format!("Bearer {t}"));
                }
                r.call()
            }
        }
        .map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        let text = res.into_body().with_config().limit(64 << 20).read_to_string().map_err(|e| e.to_string())?;
        Ok((status, text))
    }

    fn machine_token(&self) -> Option<String> {
        self.cfg.fleet.as_ref()?.token_file.as_deref().and_then(agent_stats_common::read_secret)
    }

    /// The first of `routes` that answers as another replica of this fleet: `(route, node)`.
    fn probe(&self, routes: &[String], want: Option<&str>) -> Result<(String, String), String> {
        let mut why = Vec::new();
        for r in routes {
            let r = r.trim_end_matches('/');
            match self.http(
                "GET",
                &format!("{r}{}", agent_stats_common::WELL_KNOWN),
                None,
                None,
                Duration::from_secs(3),
            ) {
                Ok((200, body)) => {
                    let d = J::parse(&body).unwrap_or(J::Null);
                    let node = d.get("node").and_then(J::as_str);
                    match (d.get("hubId").and_then(J::as_str), node) {
                        (Some(id), Some(n)) if id == self.hub_id && want.is_none_or(|w| w == n) => {
                            return Ok((r.to_string(), n.to_string()))
                        }
                        (Some(id), _) if id != self.hub_id => why.push(format!("{r}: another hub ({id})")),
                        (_, None) => why.push(format!("{r}: not a replica (an older agent-stats, or no [fleet])")),
                        (_, Some(n)) => why.push(format!("{r}: is {n}")),
                    }
                }
                Ok((s, _)) => why.push(format!("{r}: answered {s}")),
                Err(e) => why.push(format!("{r}: {e}")),
            }
        }
        Err(format!("no route answered as this fleet ({})", why.join("; ")))
    }

    fn say_hello(&self, route: &str, me: &str) -> Result<(), String> {
        let body = J::Obj(
            Obj::new()
                .with("node", me)
                .with("routes", J::Arr(self.cfg.routes().into_iter().map(J::Str).collect()))
                .with("version", agent_stats_common::VERSION)
                .with("alwaysOn", self.me_always_on())
                .with("tokenHash", crate::auth::sha256_hex(&self.replica_token)),
        )
        .stringify();
        let url = format!("{route}/sync/hello");
        let t = Duration::from_secs(10);
        let mut res = self.http("POST", &url, Some(&self.replica_token), Some(&body), t)?;
        if res.0 == 401 {
            // The peer doesn't know this replica's token yet: introduce it with this machine's reporting token.
            if let Some(m) = self.machine_token() {
                res = self.http("POST", &url, Some(&m), Some(&body), t)?;
            }
        }
        match res {
            (200, _) => Ok(()),
            (404, _) => Err("hello: the peer runs an agent-stats without fleet sync (update it)".into()),
            (401, _) => {
                Err("hello: unauthorized (this machine has no token the peer knows; join the fleet again)".into())
            }
            (s, b) => Err(format!("hello: {s} {}", snippet(&b))),
        }
    }

    /// Pulls every page `node` has after this hub's cursor (a bounded number per round) and merges it.
    fn pull(&self, route: &str, node: &str, out: &mut sync::Applied) -> Result<(), String> {
        let (mut dbid, mut since) = fleet::cursor(&lock(&self.db), node).map_err(|e| e.to_string())?;
        let mut token = self.replica_token.clone();
        let mut fallback = self.machine_token();
        let mut pages = 0;
        while pages < fleet::PAGES {
            let url = format!("{route}/sync/changes?since={since}&limit={PAGE}");
            let (status, body) = self.http("GET", &url, Some(&token), None, Duration::from_secs(30))?;
            match status {
                200 => {}
                401 => match fallback.take() {
                    Some(m) => {
                        token = m;
                        continue;
                    }
                    None => return Err("changes: unauthorized".into()),
                },
                404 => return Err("changes: the peer runs an agent-stats without fleet sync (update it)".into()),
                s => return Err(format!("changes: {s} {}", snippet(&body))),
            }
            pages += 1;
            let page = J::parse(&body).map_err(|_| "changes: not JSON".to_string())?;
            if page.get("node").and_then(J::as_str) != Some(node) {
                return Err(format!(
                    "changes: answered as {:?}, expected {node}",
                    page.get("node").and_then(J::as_str)
                ));
            }
            let id = page.get("db").and_then(J::as_str).unwrap_or("").to_string();
            if dbid.as_deref().is_some_and(|d| d != id) && since > 0 {
                // Its database was replaced: everything again.
                eprintln!("hub: {node}'s database changed, syncing it again from the start");
                (dbid, since) = (Some(id), 0);
                continue;
            }
            let rows = page.get("rows").and_then(J::as_arr).cloned().unwrap_or_default();
            let seq = page.get("seq").and_then(J::as_f64).unwrap_or(since as f64) as i64;
            {
                let mut db = lock(&self.db);
                let a = sync::apply(&mut db, &rows).map_err(|e| format!("merge: {e}"))?;
                fleet::save_cursor(&db, node, &id, seq, route, self.clock.now()).map_err(|e| e.to_string())?;
                out.rows += a.rows;
                out.pile |= a.pile;
                out.fleet_config |= a.fleet_config;
                out.requests |= a.requests;
            }
            (dbid, since) = (Some(id), seq);
            if !page.get("more").is_some_and(J::truthy) {
                break;
            }
        }
        Ok(())
    }

    fn sync_peer(
        &self,
        key: &str,
        routes: &[String],
        st: &mut PeerState,
        me: &str,
        out: &mut sync::Applied,
    ) -> Result<(), String> {
        let now = Instant::now();
        if st.needs_probe(now) {
            let want = (!key.starts_with("seed ")).then_some(key);
            let (route, node) = self.probe(routes, want)?;
            st.probed(route, node, now);
        }
        let (Some(route), Some(node)) = (st.route.clone(), st.node.clone()) else { return Err("no route".into()) };
        if node == me {
            return Ok(()); // a seed that is this replica
        }
        if st.hello_due(now) {
            self.say_hello(&route, me)?;
            st.said_hello(now);
        }
        self.pull(&route, &node, out)
    }

    /// One round: pull every due peer, then act on what arrived (streams, the pile, newer fleet settings).
    pub fn sync_round(&self) {
        let Some(me) = self.node.clone() else { return };
        let list = match fleet::members(&lock(&self.db)) {
            Ok(l) => l,
            Err(e) => return eprintln!("hub: sync: {e}"),
        };
        let seeds = self.cfg.fleet.as_ref().map(|f| f.peers.clone()).unwrap_or_default();
        let todo = fleet::peer_list(&list, &me, &seeds, &lock(&self.peers));
        let mut out = sync::Applied::default();
        for (key, routes) in todo {
            let mut st = lock(&self.peers).get(&key).cloned().unwrap_or_default();
            if !st.due(Instant::now()) {
                continue;
            }
            match self.sync_peer(&key, &routes, &mut st, &me, &mut out) {
                Ok(()) => st.ok(Instant::now()),
                Err(e) => {
                    let who = st.node.clone().unwrap_or_else(|| key.trim_start_matches("seed ").to_string());
                    if st.last_error.as_deref() != Some(e.as_str()) {
                        eprintln!("hub: sync with {who}: {e}");
                    }
                    if let Some(n) = st.node.as_deref() {
                        let _ = fleet::note_error(&lock(&self.db), n, &e);
                    }
                    st.failed(Instant::now(), e);
                }
            }
            lock(&self.peers).insert(key, st);
        }
        if out.rows > 0 {
            if out.pile {
                *lock(&self.pile) = crate::compute::Pile::load(&lock(&self.db));
            }
            self.note_change();
        }
        if out.fleet_config {
            self.adopt_fleet_settings();
        }
    }

    fn restart(&self, why: &str) {
        if self.exit_on_config_change {
            eprintln!("hub: {why}; restarting to apply them");
            std::process::exit(75);
        }
        eprintln!("hub: {why}; restart the hub to apply them");
    }

    /// Newer fleet settings arrived: written to fleet.toml, then the hub restarts with them.
    fn adopt_fleet_settings(&self) {
        let res = fleet::adopt_settings(&lock(&self.db), &self.paths);
        match res {
            Ok(Some(h)) if Some(&h) != self.settings_at_start.as_ref() => self.restart("the fleet's settings changed"),
            Ok(_) => {}
            Err(e) => eprintln!("hub: {e}"),
        }
    }

    /// fleet.toml edited here: shared with the fleet, then the hub restarts with it.
    pub fn check_local_settings(&self) {
        let Some(me) = &self.node else { return };
        let res = fleet::share_local_settings(&lock(&self.db), &self.paths, me, self.clock.now());
        match res {
            Ok(h) if h != self.settings_at_start => self.restart("fleet.toml changed"),
            Ok(_) => {}
            Err(e) => eprintln!("hub: fleet settings: {e}"),
        }
    }
}
