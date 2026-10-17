//! Online-visitor counter behind `GET /presence`: id -> last-seen, in memory only, never persisted.
//! Time is passed in (monotonic `Instant`s) so expiry and the cap are testable without sleeping.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// An id counts as online while it was seen less than this long ago.
pub const PRESENCE_TTL: Duration = Duration::from_secs(45);
/// At most this many ids are tracked; past it new ids are counted by nobody until old ones expire.
pub const PRESENCE_MAX: usize = 10_000;

#[derive(Default)]
pub struct Presence {
    seen: HashMap<String, Instant>,
}

/// `^[A-Za-z0-9_-]{8,64}$`
pub fn valid_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Presence {
    /// Records `id` as seen at `now` and returns how many ids are online, this one included
    /// (unless the table is full of live ids, in which case `id` is not tracked).
    pub fn touch(&mut self, id: &str, now: Instant) -> usize {
        self.prune(now);
        if self.seen.len() < PRESENCE_MAX || self.seen.contains_key(id) {
            self.seen.insert(id.to_string(), now);
        }
        self.seen.len()
    }

    /// How many ids are online at `now`; records nothing.
    pub fn count(&mut self, now: Instant) -> usize {
        self.prune(now);
        self.seen.len()
    }

    fn prune(&mut self, now: Instant) {
        self.seen.retain(|_, t| now.saturating_duration_since(*t) < PRESENCE_TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_syntax() {
        assert!(valid_id("abcdefgh"));
        assert!(valid_id("A_b-9Zzz"));
        assert!(valid_id(&"a".repeat(64)));
        assert!(!valid_id("short"));
        assert!(!valid_id(&"a".repeat(65)));
        assert!(!valid_id(""));
        assert!(!valid_id("abcdefg!"));
        assert!(!valid_id("abcdefg%20"));
        assert!(!valid_id("abcdefgé"));
    }

    #[test]
    fn counts_distinct_ids_once() {
        let t0 = Instant::now();
        let mut p = Presence::default();
        assert_eq!(p.count(t0), 0);
        assert_eq!(p.touch("aaaaaaaa", t0), 1);
        assert_eq!(p.touch("bbbbbbbb", t0), 2);
        assert_eq!(p.touch("aaaaaaaa", t0), 2);
        assert_eq!(p.count(t0), 2);
    }

    #[test]
    fn expires_after_ttl_and_refresh_extends() {
        let t0 = Instant::now();
        let mut p = Presence::default();
        p.touch("aaaaaaaa", t0);
        p.touch("bbbbbbbb", t0);
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(p.touch("aaaaaaaa", t1), 2);
        assert_eq!(p.count(t0 + PRESENCE_TTL - Duration::from_millis(1)), 2);
        // b is exactly TTL old: expired. a was refreshed at 30 s.
        assert_eq!(p.count(t0 + PRESENCE_TTL), 1);
        assert_eq!(p.count(t1 + PRESENCE_TTL), 0);
    }

    #[test]
    fn cap_prunes_expired_first_then_refuses_new_ids() {
        let t0 = Instant::now();
        let mut p = Presence::default();
        for i in 0..PRESENCE_MAX {
            p.touch(&format!("id{i:08}"), t0);
        }
        assert_eq!(p.count(t0), PRESENCE_MAX);
        // Full of live ids: a new id is not added, the count is still returned.
        assert_eq!(p.touch("newcomer1", t0), PRESENCE_MAX);
        assert_eq!(p.count(t0), PRESENCE_MAX);
        // A tracked id can still refresh while full.
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(p.touch("id00000000", t1), PRESENCE_MAX);
        // Once the rest expired, the table frees up and the new id is added.
        let t2 = t0 + PRESENCE_TTL;
        assert_eq!(p.touch("newcomer1", t2), 2);
    }
}
