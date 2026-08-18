//! Connection-pool analysis over the process list.
//!
//! Pure aggregation so the grouping and the classification rules can be tested
//! without a server.

use std::collections::BTreeMap;

use crate::model::ProcessRow;

/// A session is "long running" once its current statement has been going this
/// long. Configurable in the UI.
pub const DEFAULT_LONG_SECS: i64 = 10;
/// A sleeping session becomes noteworthy after this long.
pub const DEFAULT_IDLE_SECS: i64 = 60;

/// Usage bands for `Threads_connected / max_connections`.
pub const USAGE_WARN_PCT: f64 = 70.0;
pub const USAGE_CRIT_PCT: f64 = 85.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    Ok,
    Warn,
    Critical,
}

impl UsageLevel {
    pub fn of(pct: f64) -> Self {
        if pct >= USAGE_CRIT_PCT {
            UsageLevel::Critical
        } else if pct >= USAGE_WARN_PCT {
            UsageLevel::Warn
        } else {
            UsageLevel::Ok
        }
    }
}

/// Percentage of `max_connections` in use. Zero when the limit is unknown.
pub fn usage_pct(connected: u64, max_connections: u64) -> f64 {
    if max_connections == 0 {
        0.0
    } else {
        (connected as f64 / max_connections as f64) * 100.0
    }
}

/// One row of the by-user / by-host breakdown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bucket {
    pub key: String,
    pub total: usize,
    pub active: usize,
    pub sleeping: usize,
    /// Longest `Time` seen in this bucket.
    pub longest_secs: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ConnStats {
    pub total: usize,
    pub active: usize,
    pub sleeping: usize,
    /// Sleeping longer than the idle threshold.
    pub idle_stale: usize,
    /// Running a statement longer than the long threshold.
    pub long_running: usize,
    pub by_user: Vec<Bucket>,
    pub by_host: Vec<Bucket>,
}

/// A session with no statement in flight.
pub fn is_sleeping(p: &ProcessRow) -> bool {
    p.command.eq_ignore_ascii_case("sleep")
}

/// Host without the ephemeral port: `10.0.0.7:51234` → `10.0.0.7`.
///
/// A bare IPv6 address is full of colons, so the port is only stripped when
/// what precedes it has none — or when the address is bracketed, which is how
/// MySQL writes `[::1]:3306`.
pub fn host_key(host: &str) -> String {
    if host.is_empty() {
        return "(unknown)".to_string();
    }
    if let Some(rest) = host.strip_prefix('[')
        && let Some((addr, _)) = rest.split_once(']')
    {
        return addr.to_string();
    }
    match host.rsplit_once(':') {
        Some((head, tail))
            if !head.is_empty()
                && !head.contains(':')
                && !tail.is_empty()
                && tail.chars().all(|c| c.is_ascii_digit()) =>
        {
            head.to_string()
        }
        _ => host.to_string(),
    }
}

fn user_key(user: &str) -> String {
    if user.is_empty() {
        "(unknown)".to_string()
    } else {
        user.to_string()
    }
}

fn add(map: &mut BTreeMap<String, Bucket>, key: String, p: &ProcessRow) {
    let b = map.entry(key.clone()).or_insert_with(|| Bucket {
        key,
        ..Default::default()
    });
    b.total += 1;
    if is_sleeping(p) {
        b.sleeping += 1;
    } else {
        b.active += 1;
    }
    b.longest_secs = b.longest_secs.max(p.time);
}

fn sorted(map: BTreeMap<String, Bucket>) -> Vec<Bucket> {
    let mut v: Vec<Bucket> = map.into_values().collect();
    v.sort_by(|a, b| b.total.cmp(&a.total).then(a.key.cmp(&b.key)));
    v
}

/// Groups the process list and counts the interesting states.
pub fn summarize(rows: &[ProcessRow], long_secs: i64, idle_secs: i64) -> ConnStats {
    let mut by_user = BTreeMap::new();
    let mut by_host = BTreeMap::new();
    let mut stats = ConnStats {
        total: rows.len(),
        ..Default::default()
    };

    for p in rows {
        if is_sleeping(p) {
            stats.sleeping += 1;
            if p.time >= idle_secs {
                stats.idle_stale += 1;
            }
        } else {
            stats.active += 1;
            if p.time >= long_secs {
                stats.long_running += 1;
            }
        }
        add(&mut by_user, user_key(&p.user), p);
        add(&mut by_host, host_key(&p.host), p);
    }

    stats.by_user = sorted(by_user);
    stats.by_host = sorted(by_host);
    stats
}

/// Sessions running a statement for longer than the threshold, longest first.
pub fn long_running(rows: &[ProcessRow], long_secs: i64) -> Vec<&ProcessRow> {
    let mut v: Vec<&ProcessRow> = rows
        .iter()
        .filter(|p| !is_sleeping(p) && p.time >= long_secs)
        .collect();
    v.sort_by(|a, b| b.time.cmp(&a.time));
    v
}

/// Sleeping sessions, longest first — the ones holding a connection slot for
/// nothing, and the usual reason a pool fills up.
pub fn idle_sessions(rows: &[ProcessRow], idle_secs: i64) -> Vec<&ProcessRow> {
    let mut v: Vec<&ProcessRow> = rows
        .iter()
        .filter(|p| is_sleeping(p) && p.time >= idle_secs)
        .collect();
    v.sort_by(|a, b| b.time.cmp(&a.time));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u64, user: &str, host: &str, command: &str, time: i64) -> ProcessRow {
        ProcessRow {
            id,
            user: user.into(),
            host: host.into(),
            db: "demo".into(),
            command: command.into(),
            time,
            state: String::new(),
            info: String::new(),
        }
    }

    fn sample() -> Vec<ProcessRow> {
        vec![
            row(1, "app", "10.0.0.7:51234", "Query", 42),
            row(2, "app", "10.0.0.7:51999", "Sleep", 300),
            row(3, "app", "10.0.0.8:5000", "Sleep", 5),
            row(4, "report", "10.0.0.9:6000", "Query", 3),
            row(5, "", "", "Query", 1),
        ]
    }

    #[test]
    fn usage_is_a_percentage_of_the_limit() {
        assert_eq!(usage_pct(284, 500).round(), 57.0);
        assert_eq!(usage_pct(0, 500), 0.0);
        assert_eq!(
            usage_pct(100, 0),
            0.0,
            "unknown limit must not divide by zero"
        );
    }

    #[test]
    fn usage_bands() {
        assert_eq!(UsageLevel::of(10.0), UsageLevel::Ok);
        assert_eq!(UsageLevel::of(57.0), UsageLevel::Ok);
        assert_eq!(UsageLevel::of(70.0), UsageLevel::Warn);
        assert_eq!(UsageLevel::of(90.0), UsageLevel::Critical);
    }

    #[test]
    fn splits_active_from_sleeping() {
        let s = summarize(&sample(), DEFAULT_LONG_SECS, DEFAULT_IDLE_SECS);
        assert_eq!(s.total, 5);
        assert_eq!(s.active, 3);
        assert_eq!(s.sleeping, 2);
        assert_eq!(s.long_running, 1, "only the 42s query counts");
        assert_eq!(s.idle_stale, 1, "only the 300s sleeper counts");
    }

    #[test]
    fn groups_by_user_biggest_first() {
        let s = summarize(&sample(), DEFAULT_LONG_SECS, DEFAULT_IDLE_SECS);
        assert_eq!(s.by_user[0].key, "app");
        assert_eq!(s.by_user[0].total, 3);
        assert_eq!(s.by_user[0].active, 1);
        assert_eq!(s.by_user[0].sleeping, 2);
        assert_eq!(s.by_user[0].longest_secs, 300);
        assert!(s.by_user.iter().any(|b| b.key == "(unknown)"));
    }

    #[test]
    fn hosts_group_without_the_ephemeral_port() {
        let s = summarize(&sample(), DEFAULT_LONG_SECS, DEFAULT_IDLE_SECS);
        let h = &s.by_host[0];
        assert_eq!(h.key, "10.0.0.7", "two ports on one host are one client");
        assert_eq!(h.total, 2);
    }

    #[test]
    fn host_key_leaves_names_and_ipv6_alone() {
        assert_eq!(host_key("localhost"), "localhost");
        assert_eq!(host_key("db.internal:3306"), "db.internal");
        assert_eq!(host_key("::1"), "::1", "a bare IPv6 address is all colons");
        assert_eq!(
            host_key("[::1]:3306"),
            "::1",
            "bracketed IPv6 keeps its address"
        );
        assert_eq!(
            host_key("10.0.0.7:"),
            "10.0.0.7:",
            "no port, nothing to strip"
        );
        assert_eq!(host_key(""), "(unknown)");
    }

    #[test]
    fn long_running_lists_worst_first_and_skips_sleepers() {
        let rows = vec![
            row(1, "a", "h", "Query", 5),
            row(2, "b", "h", "Query", 90),
            row(3, "c", "h", "Sleep", 9999),
        ];
        let long = long_running(&rows, 10);
        assert_eq!(long.len(), 1);
        assert_eq!(long[0].id, 2);
    }

    #[test]
    fn idle_sessions_are_the_long_sleepers() {
        let rows = vec![
            row(1, "a", "h", "Sleep", 10),
            row(2, "b", "h", "Sleep", 600),
            row(3, "c", "h", "Query", 600),
        ];
        let idle = idle_sessions(&rows, 60);
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].id, 2);
    }

    #[test]
    fn empty_process_list_is_all_zeroes() {
        let s = summarize(&[], DEFAULT_LONG_SECS, DEFAULT_IDLE_SECS);
        assert_eq!(s.total, 0);
        assert!(s.by_user.is_empty() && s.by_host.is_empty());
    }
}
