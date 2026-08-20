//! Parses the `LATEST DETECTED DEADLOCK` section of `SHOW ENGINE INNODB
//! STATUS`.
//!
//! InnoDB keeps only the most recent deadlock, and only until the server
//! restarts, so this is a snapshot rather than a log. It is still the one place
//! that names both sides: each transaction, the statement it was running, the
//! locks it held, the lock it wanted, and which one InnoDB rolled back.
//!
//! The layout is stable enough across 5.7 and 8.0 to parse by marker lines
//! rather than by offset, which is what this does — anything unrecognised is
//! kept as raw text instead of being dropped.

/// One side of a deadlock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Party {
    /// 1 or 2, as InnoDB numbers them.
    pub index: u8,
    pub trx_id: String,
    pub active_secs: u64,
    /// What the transaction was doing, from the `ACTIVE n sec ...` tail.
    pub activity: String,
    /// `MySQL thread id` — the same id the process list and `KILL` use.
    pub thread_id: u64,
    /// Host and user, as far as the report gives them.
    pub host: String,
    pub user: String,
    /// Connection state from the end of the thread line, e.g. `updating`.
    pub state: String,
    pub tables_in_use: u64,
    pub tables_locked: u64,
    pub row_locks: u64,
    pub lock_structs: u64,
    /// The statement, which may span several lines.
    pub query: String,
    pub holds: Vec<Lock>,
    pub waiting: Option<Lock>,
}

/// A lock line, kept whole and picked apart as far as it parses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lock {
    /// `RECORD LOCKS ...` or `TABLE LOCK ...`, verbatim.
    pub raw: String,
    /// `` `demo`.`attendance` `` with the backticks removed.
    pub table: String,
    /// Index name for a record lock, empty for a table lock.
    pub index: String,
    /// `X`, `S`, `IX`, plus any gap qualifier.
    pub mode: String,
    pub record: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deadlock {
    /// The timestamp line InnoDB prints under the header, verbatim.
    pub detected_at: String,
    pub parties: Vec<Party>,
    /// Which party InnoDB rolled back, by its index.
    pub victim: Option<u8>,
    /// The whole section, for the times the parse misses something.
    pub raw: String,
}

impl Deadlock {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn party(&self, index: u8) -> Option<&Party> {
        self.parties.iter().find(|p| p.index == index)
    }

    /// Tables named by any lock on either side, deduplicated.
    pub fn tables(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in &self.parties {
            for l in p.holds.iter().chain(p.waiting.iter()) {
                if !l.table.is_empty() && !out.contains(&l.table) {
                    out.push(l.table.clone());
                }
            }
        }
        out
    }
}

/// Where subsequent lock lines belong.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    None,
    Holds(u8),
    Waiting(u8),
}

/// Extracts the deadlock section from a full engine report.
///
/// The section runs from its header to the next `-----` rule, which is how
/// every section in the report ends — the deadlock body itself contains none.
fn section(text: &str) -> Option<String> {
    let mut lines = text
        .lines()
        .skip_while(|l| !l.contains("LATEST DETECTED DEADLOCK"));
    lines.next()?;
    // The header is followed by its own underline; step past it.
    let mut body: Vec<&str> = Vec::new();
    for line in lines {
        let t = line.trim();
        if is_rule(t) {
            if body.is_empty() {
                continue;
            }
            break;
        }
        body.push(line);
    }
    if body.is_empty() {
        return None;
    }
    Some(body.join("\n"))
}

fn is_rule(line: &str) -> bool {
    line.len() >= 4 && line.chars().all(|c| c == '-')
}

/// First integer following `prefix` anywhere in the line.
fn number_after(line: &str, prefix: &str) -> Option<u64> {
    let rest = line.split_once(prefix)?.1;
    rest.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())?
        .parse()
        .ok()
}

/// Pulls `` `db`.`tbl` `` out of a lock line and strips the backticks.
fn table_of(line: &str) -> String {
    let Some(rest) = line.split_once("of table ").map(|x| x.1) else {
        // A table lock reads `TABLE LOCK table `db`.`t` trx id ...`.
        return match line.split_once("table ") {
            Some((_, rest)) => first_token(rest).replace('`', ""),
            None => String::new(),
        };
    };
    first_token(rest).replace('`', "")
}

fn first_token(s: &str) -> String {
    s.split_whitespace().next().unwrap_or("").to_string()
}

fn parse_lock(line: &str) -> Lock {
    let record = line.starts_with("RECORD LOCKS");
    let index = match line.split_once(" index ") {
        Some((_, rest)) => first_token(rest),
        None => String::new(),
    };
    // Both `lock_mode X` and `lock mode IX` occur; the tail after it carries
    // the gap qualifiers, which matter as much as the mode itself.
    let mode = line
        .split_once("lock_mode ")
        .or_else(|| line.split_once("lock mode "))
        .map(|(_, rest)| rest.trim_end_matches(" waiting").trim().to_string())
        .unwrap_or_default();
    Lock {
        raw: line.to_string(),
        table: table_of(line),
        index,
        mode,
        record,
    }
}

/// `MySQL thread id 921, OS thread handle 0x7f, query id 4455 host user state`
fn parse_thread_line(line: &str, party: &mut Party) {
    party.thread_id = number_after(line, "MySQL thread id ").unwrap_or(0);
    let Some((_, tail)) = line.split_once("query id ") else {
        return;
    };
    let mut parts = tail.split_whitespace();
    // Skip the query id itself.
    parts.next();
    let rest: Vec<&str> = parts.collect();
    match rest.as_slice() {
        [] => {}
        // Local connections print no host.
        [user] => party.user = (*user).to_string(),
        [host, user, state @ ..] => {
            party.host = (*host).to_string();
            party.user = (*user).to_string();
            party.state = state.join(" ");
        }
    }
}

/// Parses the deadlock out of a full `SHOW ENGINE INNODB STATUS` report.
/// Returns `None` when the server has not seen one since it started.
pub fn parse(report: &str) -> Option<Deadlock> {
    let raw = section(report)?;
    let mut d = Deadlock {
        raw: raw.clone(),
        ..Default::default()
    };

    let mut current: Option<usize> = None;
    let mut target = Target::None;
    // A statement can run to several lines, so it accumulates until the next
    // marker rather than being taken from one line.
    let mut collecting_query = false;

    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }

        if let Some(rest) = t.strip_prefix("*** ") {
            collecting_query = false;
            if let Some(n) = marker_index(rest) {
                if rest.contains("TRANSACTION:") {
                    d.parties.push(Party {
                        index: n,
                        ..Default::default()
                    });
                    current = Some(d.parties.len() - 1);
                    target = Target::None;
                } else if rest.contains("HOLDS THE LOCK") {
                    target = Target::Holds(n);
                } else if rest.contains("WAITING FOR THIS LOCK") {
                    target = Target::Waiting(n);
                } else {
                    target = Target::None;
                }
            } else if rest.contains("WE ROLL BACK TRANSACTION") {
                d.victim = roll_back_index(rest);
            } else {
                target = Target::None;
            }
            continue;
        }

        if t.starts_with("RECORD LOCKS") || t.starts_with("TABLE LOCK") {
            collecting_query = false;
            let lock = parse_lock(t);
            match target {
                Target::Holds(n) => {
                    if let Some(p) = d.parties.iter_mut().find(|p| p.index == n) {
                        p.holds.push(lock);
                    }
                }
                Target::Waiting(n) => {
                    if let Some(p) = d.parties.iter_mut().find(|p| p.index == n) {
                        p.waiting = Some(lock);
                    }
                }
                Target::None => {}
            }
            continue;
        }

        let Some(i) = current else {
            // Everything before the first marker is the detection timestamp.
            if d.detected_at.is_empty() {
                d.detected_at = t.to_string();
            }
            continue;
        };
        let party = &mut d.parties[i];

        if let Some(rest) = t.strip_prefix("TRANSACTION ") {
            party.trx_id = rest
                .split(',')
                .next()
                .unwrap_or("")
                .trim()
                .trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_string();
            party.active_secs = number_after(t, "ACTIVE ").unwrap_or(0);
            if let Some((_, tail)) = t.split_once(" sec ") {
                party.activity = tail.trim().to_string();
            }
        } else if t.starts_with("mysql tables in use") {
            party.tables_in_use = number_after(t, "in use ").unwrap_or(0);
            party.tables_locked = number_after(t, "locked ").unwrap_or(0);
        } else if t.contains("lock struct(s)") {
            party.lock_structs = number_after(t, "lock struct").unwrap_or(0);
            // `LOCK WAIT 3 lock struct(s), heap size 1136, 2 row lock(s)`
            party.lock_structs = t
                .split_whitespace()
                .zip(t.split_whitespace().skip(1))
                .find(|(_, next)| next.starts_with("lock"))
                .and_then(|(n, _)| n.parse().ok())
                .unwrap_or(party.lock_structs);
            party.row_locks = t
                .split_whitespace()
                .zip(t.split_whitespace().skip(1))
                .find(|(_, next)| next.starts_with("row"))
                .and_then(|(n, _)| n.parse().ok())
                .unwrap_or(0);
        } else if t.starts_with("MySQL thread id") {
            parse_thread_line(t, party);
            collecting_query = true;
        } else if collecting_query {
            if !party.query.is_empty() {
                party.query.push('\n');
            }
            party.query.push_str(t);
        }
    }

    if d.parties.is_empty() { None } else { Some(d) }
}

/// `(1) TRANSACTION:` -> 1
fn marker_index(rest: &str) -> Option<u8> {
    let inner = rest.strip_prefix('(')?.split_once(')')?.0;
    inner.parse().ok()
}

/// `WE ROLL BACK TRANSACTION (2)` -> 2
fn roll_back_index(rest: &str) -> Option<u8> {
    let inner = rest.rsplit_once('(')?.1;
    inner.split_once(')')?.0.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 5.7 report. Two sections around the deadlock so the extraction has to
    /// find its own boundaries.
    const REPORT_57: &str = "\
=====================================
2026-08-20 10:11:12 0x7f3c INNODB MONITOR OUTPUT
=====================================
------------------------
LATEST DETECTED DEADLOCK
------------------------
2026-08-20 10:10:59 0x7f3c1c0f7700
*** (1) TRANSACTION:
TRANSACTION 12345, ACTIVE 5 sec starting index read
mysql tables in use 1, locked 1
LOCK WAIT 3 lock struct(s), heap size 1136, 2 row lock(s)
MySQL thread id 921, OS thread handle 0x7f3c, query id 4455 10.0.0.5 app updating
UPDATE attendance SET checkout = NOW()
WHERE id = 7
*** (1) WAITING FOR THIS LOCK TO BE GRANTED:
RECORD LOCKS space id 42 page no 4 n bits 80 index PRIMARY of table `demo`.`attendance` trx id 12345 lock_mode X locks rec but not gap waiting
*** (2) TRANSACTION:
TRANSACTION 12346, ACTIVE 6 sec starting index read
mysql tables in use 1, locked 1
4 lock struct(s), heap size 1136, 3 row lock(s)
MySQL thread id 922, OS thread handle 0x7f3d, query id 4456 10.0.0.6 app updating
UPDATE attendance SET checkin = NOW() WHERE id = 9
*** (2) HOLDS THE LOCK(S):
RECORD LOCKS space id 42 page no 4 n bits 80 index PRIMARY of table `demo`.`attendance` trx id 12346 lock_mode X locks rec but not gap
*** (2) WAITING FOR THIS LOCK TO BE GRANTED:
RECORD LOCKS space id 42 page no 5 n bits 80 index idx_user of table `demo`.`attendance` trx id 12346 lock_mode X waiting
*** WE ROLL BACK TRANSACTION (1)
------------
TRANSACTIONS
------------
Trx id counter 12350
";

    #[test]
    fn finds_both_sides() {
        let d = parse(REPORT_57).expect("deadlock present");
        assert_eq!(d.parties.len(), 2);
        assert_eq!(d.detected_at, "2026-08-20 10:10:59 0x7f3c1c0f7700");
        assert_eq!(d.victim, Some(1));
    }

    #[test]
    fn reads_the_transaction_facts() {
        let d = parse(REPORT_57).unwrap();
        let one = d.party(1).unwrap();
        assert_eq!(one.trx_id, "12345");
        assert_eq!(one.active_secs, 5);
        assert_eq!(one.activity, "starting index read");
        assert_eq!(one.thread_id, 921);
        assert_eq!(one.host, "10.0.0.5");
        assert_eq!(one.user, "app");
        assert_eq!(one.state, "updating");
        assert_eq!(one.tables_in_use, 1);
        assert_eq!(one.tables_locked, 1);
        assert_eq!(one.lock_structs, 3);
        assert_eq!(one.row_locks, 2);
    }

    #[test]
    fn a_multi_line_statement_is_kept_whole() {
        let d = parse(REPORT_57).unwrap();
        assert_eq!(
            d.party(1).unwrap().query,
            "UPDATE attendance SET checkout = NOW()\nWHERE id = 7"
        );
        assert_eq!(
            d.party(2).unwrap().query,
            "UPDATE attendance SET checkin = NOW() WHERE id = 9"
        );
    }

    #[test]
    fn locks_are_split_by_side() {
        let d = parse(REPORT_57).unwrap();
        let one = d.party(1).unwrap();
        assert!(one.holds.is_empty(), "party 1 holds nothing here");
        let w = one.waiting.as_ref().expect("party 1 waits");
        assert_eq!(w.table, "demo.attendance");
        assert_eq!(w.index, "PRIMARY");
        assert_eq!(w.mode, "X locks rec but not gap");
        assert!(w.record);

        let two = d.party(2).unwrap();
        assert_eq!(two.holds.len(), 1);
        assert_eq!(two.holds[0].index, "PRIMARY");
        assert_eq!(two.waiting.as_ref().unwrap().index, "idx_user");
    }

    #[test]
    fn the_tables_involved_are_deduplicated() {
        assert_eq!(parse(REPORT_57).unwrap().tables(), vec!["demo.attendance"]);
    }

    #[test]
    fn the_section_stops_at_the_next_rule() {
        let d = parse(REPORT_57).unwrap();
        assert!(!d.raw.contains("Trx id counter"), "{}", d.raw);
        assert!(d.raw.contains("WE ROLL BACK"), "{}", d.raw);
    }

    #[test]
    fn a_report_without_a_deadlock_gives_nothing() {
        let report = "\
=====================================
2026-08-20 10:11:12 INNODB MONITOR OUTPUT
=====================================
------------
TRANSACTIONS
------------
Trx id counter 12350
";
        assert!(parse(report).is_none());
    }

    #[test]
    fn a_table_lock_is_understood() {
        let report = "\
------------------------
LATEST DETECTED DEADLOCK
------------------------
2026-08-20 10:10:59
*** (1) TRANSACTION:
TRANSACTION 1, ACTIVE 2 sec
MySQL thread id 5, OS thread handle 0x1, query id 9 localhost root
LOCK TABLES demo.t WRITE
*** (1) HOLDS THE LOCK(S):
TABLE LOCK table `demo`.`t` trx id 1 lock mode IX
*** WE ROLL BACK TRANSACTION (1)
------------
TRANSACTIONS
------------
";
        let d = parse(report).unwrap();
        let hold = &d.party(1).unwrap().holds[0];
        assert_eq!(hold.table, "demo.t");
        assert_eq!(hold.mode, "IX");
        assert!(!hold.record);
        assert_eq!(hold.index, "");
    }

    #[test]
    fn a_local_connection_without_a_host_still_parses() {
        let mut p = Party::default();
        parse_thread_line(
            "MySQL thread id 5, OS thread handle 0x1, query id 9 localhost root",
            &mut p,
        );
        assert_eq!(p.thread_id, 5);
        assert_eq!(p.host, "localhost");
        assert_eq!(p.user, "root");
        assert_eq!(p.state, "");
    }

    #[test]
    fn an_8_0_report_with_holds_on_both_sides_parses() {
        // 8.0.20+ prints HOLDS for party 1 as well.
        let report = "\
------------------------
LATEST DETECTED DEADLOCK
------------------------
2026-08-20 11:00:00 140234
*** (1) TRANSACTION:
TRANSACTION 900, ACTIVE 1 sec starting index read
mysql tables in use 1, locked 1
LOCK WAIT 2 lock struct(s), heap size 1136, 1 row lock(s)
MySQL thread id 10, OS thread handle 1, query id 20 10.0.0.1 svc updating
UPDATE t SET a = 1 WHERE id = 1
*** (1) HOLDS THE LOCK(S):
RECORD LOCKS space id 1 page no 4 n bits 72 index PRIMARY of table `d`.`t` trx id 900 lock_mode X locks rec but not gap
*** (1) WAITING FOR THIS LOCK TO BE GRANTED:
RECORD LOCKS space id 1 page no 4 n bits 72 index PRIMARY of table `d`.`t` trx id 900 lock_mode X locks rec but not gap waiting
*** (2) TRANSACTION:
TRANSACTION 901, ACTIVE 1 sec starting index read
MySQL thread id 11, OS thread handle 2, query id 21 10.0.0.2 svc updating
UPDATE t SET a = 2 WHERE id = 2
*** (2) HOLDS THE LOCK(S):
RECORD LOCKS space id 1 page no 4 n bits 72 index PRIMARY of table `d`.`t` trx id 901 lock_mode X locks rec but not gap
*** (2) WAITING FOR THIS LOCK TO BE GRANTED:
RECORD LOCKS space id 1 page no 4 n bits 72 index PRIMARY of table `d`.`t` trx id 901 lock_mode X waiting
*** WE ROLL BACK TRANSACTION (2)
-----------------
FILE I/O
-----------------
";
        let d = parse(report).unwrap();
        assert_eq!(d.victim, Some(2));
        assert_eq!(d.party(1).unwrap().holds.len(), 1);
        assert_eq!(d.party(2).unwrap().holds.len(), 1);
        assert!(d.party(1).unwrap().waiting.is_some());
        assert!(d.party(2).unwrap().waiting.is_some());
        assert_eq!(d.tables(), vec!["d.t"]);
    }

    #[test]
    fn the_waiting_marker_strips_the_trailing_word() {
        let l = parse_lock(
            "RECORD LOCKS space id 1 page no 4 n bits 72 index PRIMARY of table `d`.`t` trx id 9 lock_mode X waiting",
        );
        assert_eq!(l.mode, "X");
    }
}
