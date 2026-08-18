//! Server version parsing and per-version capability matrix.
//!
//! This is where MySQL 5.x / 8.x divergence is centralised. Every query that
//! differs across versions must branch on a `Capabilities` flag, never on a
//! raw version compare at the call site.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    MySql,
    MariaDb,
    Percona,
    Unknown,
}

impl fmt::Display for Flavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Flavor::MySql => "MySQL",
            Flavor::MariaDb => "MariaDB",
            Flavor::Percona => "Percona",
            Flavor::Unknown => "Unknown",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerVersion {
    pub raw: String,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub flavor: Flavor,
}

impl Default for ServerVersion {
    fn default() -> Self {
        Self {
            raw: String::new(),
            major: 0,
            minor: 0,
            patch: 0,
            flavor: Flavor::Unknown,
        }
    }
}

impl ServerVersion {
    /// Parses `VERSION()` output, e.g. `8.0.36`, `5.7.44-log`,
    /// `10.11.6-MariaDB-1:10.11.6+maria~ubu2204`, `5.7.44-48-log` (Percona).
    pub fn parse(raw: &str, comment: &str) -> Self {
        let num: String = raw
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let mut it = num.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        let (major, minor, patch) = (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        );

        let hay = format!("{raw} {comment}").to_ascii_lowercase();
        let flavor = if hay.contains("mariadb") {
            Flavor::MariaDb
        } else if hay.contains("percona") {
            Flavor::Percona
        } else if major > 0 {
            Flavor::MySql
        } else {
            Flavor::Unknown
        };

        Self {
            raw: raw.to_string(),
            major,
            minor,
            patch,
            flavor,
        }
    }

    pub fn at_least(&self, major: u32, minor: u32, patch: u32) -> bool {
        (self.major, self.minor, self.patch) >= (major, minor, patch)
    }

    pub fn is_mariadb(&self) -> bool {
        self.flavor == Flavor::MariaDb
    }

    #[allow(dead_code)] // version-gated UI hints
    /// MySQL/Percona 8.x line (MariaDB 10/11 is *not* MySQL 8).
    pub fn is_mysql8(&self) -> bool {
        !self.is_mariadb() && self.major >= 8
    }

    #[allow(dead_code)] // version-gated UI hints
    /// MySQL/Percona 5.x line.
    pub fn is_mysql5(&self) -> bool {
        !self.is_mariadb() && self.major == 5
    }
}

impl fmt::Display for ServerVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}.{}.{}",
            self.flavor, self.major, self.minor, self.patch
        )
    }
}

/// What this server supports. Probed from version, then narrowed at runtime by
/// actual query failures (see `Capabilities::disable_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// `performance_schema.processlist` exists (MySQL 8.0.22+).
    /// Otherwise use `information_schema.PROCESSLIST`.
    pub ps_processlist: bool,
    /// `performance_schema.data_locks` / `data_lock_waits` (MySQL 8.0+).
    /// MySQL 5.x uses `information_schema.INNODB_LOCKS` / `INNODB_LOCK_WAITS`,
    /// which were REMOVED in 8.0.
    pub ps_data_locks: bool,
    /// `information_schema.INNODB_TRX` (5.6+, and 8.0).
    pub innodb_trx: bool,
    /// `performance_schema.events_statements_summary_by_digest` (5.6+).
    pub statement_digest: bool,
    /// Digest table has `QUERY_SAMPLE_TEXT` (MySQL 8.0+ only).
    pub digest_sample_text: bool,
    /// `sys` schema shipped by default (MySQL 5.7+).
    pub sys_schema: bool,
    /// `SHOW REPLICA STATUS` instead of `SHOW SLAVE STATUS` (MySQL 8.0.22+).
    pub replica_terms: bool,
    /// `information_schema.INNODB_METRICS` (5.6+; MariaDB has it too).
    pub innodb_metrics: bool,
    /// `performance_schema` compiled in and enabled. Confirmed at connect.
    pub perf_schema_on: bool,
    /// `performance_schema.metadata_locks` (MySQL 5.7.3+).
    pub metadata_locks: bool,
    /// `performance_schema.events_statements_history_long` (5.6+; the consumer
    /// can still be off, which just yields no rows).
    pub history_long: bool,
    /// `performance_schema.table_io_waits_summary_by_index_usage` (5.6+) —
    /// the basis of the index advisor.
    pub index_usage: bool,
    /// Invisible indexes (MySQL 8.0.13+): lets the advisor suggest hiding an
    /// index before dropping it.
    pub invisible_index: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        // Conservative: assume the oldest supported target (MySQL 5.6).
        Self {
            ps_processlist: false,
            ps_data_locks: false,
            innodb_trx: true,
            statement_digest: true,
            digest_sample_text: false,
            sys_schema: false,
            replica_terms: false,
            innodb_metrics: true,
            perf_schema_on: false,
            metadata_locks: false,
            history_long: true,
            index_usage: true,
            invisible_index: false,
        }
    }
}

impl Capabilities {
    pub fn detect(v: &ServerVersion) -> Self {
        if v.is_mariadb() {
            return Self {
                ps_processlist: false,
                ps_data_locks: false,
                innodb_trx: true,
                statement_digest: true,
                digest_sample_text: false,
                sys_schema: v.at_least(10, 6, 0),
                replica_terms: false,
                innodb_metrics: true,
                perf_schema_on: false,
                metadata_locks: v.at_least(10, 5, 0),
                history_long: true,
                index_usage: true,
                invisible_index: false,
            };
        }

        Self {
            ps_processlist: v.at_least(8, 0, 22),
            ps_data_locks: v.major >= 8,
            innodb_trx: v.at_least(5, 6, 0),
            statement_digest: v.at_least(5, 6, 0),
            digest_sample_text: v.major >= 8,
            sys_schema: v.at_least(5, 7, 0),
            replica_terms: v.at_least(8, 0, 22),
            innodb_metrics: v.at_least(5, 6, 0),
            perf_schema_on: false,
            metadata_locks: v.at_least(5, 7, 3),
            history_long: v.at_least(5, 6, 0),
            index_usage: v.at_least(5, 6, 0),
            invisible_index: v.at_least(8, 0, 13),
        }
    }

    /// One-line summary for the UI.
    pub fn summary(&self) -> String {
        let mut on = Vec::new();
        if self.perf_schema_on {
            on.push("perf_schema");
        }
        if self.ps_processlist {
            on.push("ps.processlist");
        }
        if self.ps_data_locks {
            on.push("data_locks");
        } else {
            on.push("innodb_locks");
        }
        if self.statement_digest {
            on.push("digest");
        }
        if self.sys_schema {
            on.push("sys");
        }
        on.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mysql_versions() {
        let v = ServerVersion::parse("8.0.36", "MySQL Community Server - GPL");
        assert!(v.is_mysql8() && v.at_least(8, 0, 22));

        let v = ServerVersion::parse("5.7.44-log", "MySQL Community Server (GPL)");
        assert!(v.is_mysql5());
        assert_eq!((v.major, v.minor, v.patch), (5, 7, 44));

        let v = ServerVersion::parse("5.6.51", "");
        assert!(!v.at_least(5, 7, 0));

        let v = ServerVersion::parse("10.11.6-MariaDB", "mariadb.org binary distribution");
        assert!(v.is_mariadb() && !v.is_mysql8());

        let v = ServerVersion::parse("8.0.35-27", "Percona Server (GPL), Release 27");
        assert_eq!(v.flavor, Flavor::Percona);
        assert!(v.is_mysql8());
    }

    #[test]
    fn caps_split_5_and_8() {
        let c5 = Capabilities::detect(&ServerVersion::parse("5.7.44-log", ""));
        assert!(!c5.ps_data_locks && !c5.ps_processlist && c5.sys_schema);
        assert!(!c5.digest_sample_text);

        let c8 = Capabilities::detect(&ServerVersion::parse("8.0.36", ""));
        assert!(c8.ps_data_locks && c8.ps_processlist && c8.digest_sample_text);
        assert!(c8.replica_terms);

        let c8_early = Capabilities::detect(&ServerVersion::parse("8.0.11", ""));
        assert!(c8_early.ps_data_locks && !c8_early.ps_processlist);
        assert!(
            !c8_early.invisible_index,
            "invisible indexes land in 8.0.13"
        );

        let c56 = Capabilities::detect(&ServerVersion::parse("5.6.51", ""));
        assert!(c56.index_usage && !c56.metadata_locks && !c56.sys_schema);
    }
}
