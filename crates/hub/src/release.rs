//! `GET /release` and `GET /release/<version>/<file>`: the hub serves the release tarballs it mirrors on disk
//! (`<releases_dir>/<version>/`, complete once `.complete` exists) to its reporters. Only the hub's own running
//! version is ever advertised or served, so a reporter never gets ahead of the hub's protocol.

use crate::http::{Request, Response};
use crate::json::{Obj, J};
use crate::server::Hub;
use agent_stats_common::VERSION;
use std::path::PathBuf;

/// Larger files are refused (500) rather than read into memory.
const MAX_FILE: u64 = 200 << 20;

/// `^[a-z0-9_]+(-[a-z0-9_]+)*$`
fn valid_target(t: &str) -> bool {
    t.split('-').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
}

/// The target of `agent-stats-<VERSION>-<target>.tar.gz`.
fn tarball_target(file: &str) -> Option<&str> {
    let t = file.strip_prefix("agent-stats-")?.strip_prefix(VERSION)?.strip_prefix('-')?.strip_suffix(".tar.gz")?;
    valid_target(t).then_some(t)
}

impl Hub {
    fn release_authorized(&self, req: &Request) -> bool {
        self.token_valid(req) || self.is_admin(req)
    }

    /// The directory of this hub's version, if its mirror is complete.
    fn release_dir(&self) -> Option<PathBuf> {
        let dir = self.paths.releases_dir.join(VERSION);
        dir.join(".complete").is_file().then_some(dir)
    }

    pub(crate) fn release(&self, req: &Request) -> Response {
        if !self.release_authorized(req) {
            return Response::error(401, "unauthorized");
        }
        if req.method != "GET" {
            return Response::error(405, "method not allowed");
        }
        let Some(dir) = self.release_dir() else {
            return Response::json(200, r#"{"version":null,"targets":[]}"#).header("cache-control", "no-store");
        };
        let mut targets: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(tarball_target).map(str::to_string))
            .collect();
        targets.sort();
        let body =
            Obj::new().with("version", VERSION).with("targets", J::Arr(targets.into_iter().map(J::Str).collect()));
        Response::json(200, J::Obj(body).stringify()).header("cache-control", "no-store")
    }

    pub(crate) fn release_file(&self, req: &Request) -> Response {
        if !self.release_authorized(req) {
            return Response::error(401, "unauthorized");
        }
        if req.method != "GET" {
            return Response::error(405, "method not allowed");
        }
        let not_found = || Response::error(404, "not found");
        let rest = req.path.strip_prefix("/release/").unwrap_or("");
        let Some((version, file)) = rest.split_once('/') else { return not_found() };
        let content_type = if file == "SHA256SUMS" {
            "text/plain; charset=utf-8"
        } else if tarball_target(file).is_some() {
            "application/gzip"
        } else {
            return not_found();
        };
        // `file` is one of two exact shapes without a separator, and `version` is ours: nothing else reaches the path.
        let Some(dir) = self.release_dir().filter(|_| version == VERSION) else { return not_found() };
        let path = dir.join(file);
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() > MAX_FILE => return Response::error(500, "release file too large"),
            Ok(m) if m.is_file() => {}
            _ => return not_found(),
        }
        match std::fs::read(&path) {
            Ok(bytes) => Response::new(200, content_type, bytes).header("cache-control", "no-store"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => not_found(),
            Err(e) => Response::error(500, &e.to_string()),
        }
    }
}
