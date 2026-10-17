//! Admin-side helpers for the `hub` CLI that need no running hub: `hub purge`, and printing hub-held free text safely.

use crate::validate::is_unsafe_char;
use rusqlite::{params, Connection};

/// Rows `purge` removed.
#[derive(Debug, PartialEq)]
pub struct Purged {
    pub usage: usize,
    pub repos: usize,
}

/// Deletes `machine`'s per-day token rows (`usage`) and per-day repo rows (`repos`) with `from <= day <= to`
/// (either bound optional). Machines, tokens and history of other machines are untouched.
pub fn purge(db: &mut Connection, machine: &str, from: Option<&str>, to: Option<&str>) -> rusqlite::Result<Purged> {
    let tx = db.transaction()?;
    let mut n = [0; 2];
    for (i, table) in ["usage", "repos"].into_iter().enumerate() {
        n[i] = tx.execute(
            &format!(
                "DELETE FROM {table} WHERE machine = ?1 AND (?2 IS NULL OR day >= ?2) AND (?3 IS NULL OR day <= ?3)"
            ),
            params![machine, from, to],
        )?;
    }
    tx.commit()?;
    Ok(Purged { usage: n[0], repos: n[1] })
}

/// `s` with control and bidi characters shown as `\u{XXXX}`, for terminals.
pub fn printable(s: &str) -> String {
    s.chars().fold(String::with_capacity(s.len()), |mut out, c| {
        if is_unsafe_char(c) {
            out.push_str(&format!("\\u{{{:04x}}}", c as u32));
        } else {
            out.push(c);
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_is_scoped_to_machine_and_range() {
        let mut db = crate::db::open(std::path::Path::new(":memory:")).unwrap();
        db.execute_batch(
            "INSERT INTO usage VALUES ('a','2026-10-01','claude','m',5,1), ('a','2026-10-05','claude','m',5,1),
               ('a','9999-12-31','claude','m',5,1), ('b','2026-10-05','claude','m',5,1);
             INSERT INTO repos VALUES ('a','2026-10-05','r',3), ('a','2026-10-09','r',3), ('b','2026-10-05','r',3);",
        )
        .unwrap();
        assert_eq!(purge(&mut db, "a", Some("2026-10-03"), Some("2026-10-31")).unwrap(), Purged { usage: 1, repos: 2 });
        assert_eq!(purge(&mut db, "a", None, None).unwrap(), Purged { usage: 2, repos: 0 });
        let left: i64 = db
            .query_row("SELECT (SELECT count(*) FROM usage) + (SELECT count(*) FROM repos)", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 2, "machine b is untouched");
    }

    #[test]
    fn terminal_text_is_escaped() {
        assert_eq!(printable("Lap\x1b[31mRED\u{202e}\n"), "Lap\\u{001b}[31mRED\\u{202e}\\u{000a}");
        assert_eq!(printable("MacBook Air é"), "MacBook Air é");
    }
}
