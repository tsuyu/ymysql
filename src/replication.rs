//! Replication status, from `SHOW REPLICA STATUS` / `SHOW SLAVE STATUS`.
//!
//! MySQL 8.0.22 renamed nearly every column in that result — `Slave_IO_Running`
//! became `Replica_IO_Running`, `Seconds_Behind_Master` became
//! `Seconds_Behind_Source`, and so on — while keeping the old names as
//! deprecated aliases only in the *statement*, not in the output. So rather
//! than branching on version, every field here is looked up under both names
//! and the first one present wins. That also covers MariaDB, which kept the
//! 5.7 spelling.
//!
//! Everything in this module works from a [`Grid`], so it is tested without a
//! server.

use crate::db::queries::Grid;

/// How healthy one replication channel is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Good,
    Watch,
    Bad,
}

/// Lag thresholds in seconds.
pub const LAG_WATCH_SECS: i64 = 30;
pub const LAG_BAD_SECS: i64 = 300;

/// One replication channel as the replica sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Replica {
    /// Empty for the default channel.
    pub channel: String,
    pub source_host: String,
    pub source_port: String,
    pub source_user: String,
    /// `Yes`, `No`, or `Connecting`.
    pub io_running: String,
    /// `Yes` or `No`.
    pub sql_running: String,
    /// `None` when the server reports NULL, which means it is not replicating
    /// — not that it is caught up.
    pub seconds_behind: Option<i64>,
    pub source_log_file: String,
    pub read_source_log_pos: u64,
    pub relay_source_log_file: String,
    pub exec_source_log_pos: u64,
    pub relay_log_space: u64,
    pub last_io_errno: String,
    pub last_io_error: String,
    pub last_sql_errno: String,
    pub last_sql_error: String,
    /// `Replica_SQL_Running_State`, e.g. "Waiting for dependent transaction".
    pub sql_state: String,
    pub auto_position: String,
    pub retrieved_gtid_set: String,
    pub executed_gtid_set: String,
    /// Configured `SOURCE_DELAY`, in seconds.
    pub sql_delay: u64,
}

impl Replica {
    pub fn io_ok(&self) -> bool {
        self.io_running.eq_ignore_ascii_case("yes")
    }

    pub fn sql_ok(&self) -> bool {
        self.sql_running.eq_ignore_ascii_case("yes")
    }

    /// Bytes the applier is behind the receiver, when both are reading the same
    /// binary log file. This separates a slow network from a slow apply: a
    /// large gap with both threads running means the SQL thread is the
    /// bottleneck. `None` when they are on different files, where a byte
    /// difference would be meaningless.
    pub fn apply_backlog_bytes(&self) -> Option<u64> {
        if self.source_log_file.is_empty() || self.source_log_file != self.relay_source_log_file {
            return None;
        }
        Some(
            self.read_source_log_pos
                .saturating_sub(self.exec_source_log_pos),
        )
    }

    /// True when either thread reported an error code other than 0.
    pub fn has_error(&self) -> bool {
        let bad = |e: &str| !e.is_empty() && e != "0";
        bad(&self.last_io_errno) || bad(&self.last_sql_errno)
    }

    pub fn health(&self) -> Health {
        if !self.io_ok() || !self.sql_ok() || self.has_error() {
            return Health::Bad;
        }
        match self.seconds_behind {
            // NULL with both threads running is a brief window during
            // connection; without them it means stopped.
            None => Health::Bad,
            Some(s) if s >= LAG_BAD_SECS => Health::Bad,
            Some(s) if s >= LAG_WATCH_SECS => Health::Watch,
            _ => Health::Good,
        }
    }

    /// One line fit for a status bar.
    pub fn summary(&self) -> String {
        if !self.io_ok() || !self.sql_ok() {
            return format!(
                "IO {} / SQL {}",
                blank_as_dash(&self.io_running),
                blank_as_dash(&self.sql_running)
            );
        }
        match self.seconds_behind {
            Some(s) => format!("{s}s behind"),
            None => "not replicating".to_string(),
        }
    }

    /// The error worth showing, if any.
    pub fn error_text(&self) -> Option<String> {
        for (errno, text, side) in [
            (&self.last_io_errno, &self.last_io_error, "IO"),
            (&self.last_sql_errno, &self.last_sql_error, "SQL"),
        ] {
            if !errno.is_empty() && errno != "0" {
                return Some(format!("{side} thread error {errno}: {text}"));
            }
            if !text.is_empty() {
                return Some(format!("{side} thread: {text}"));
            }
        }
        None
    }
}

fn blank_as_dash(s: &str) -> &str {
    if s.is_empty() { "-" } else { s }
}

/// What this server is as a source: the binary log position replicas read from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Source {
    pub file: String,
    pub position: u64,
    pub binlog_do_db: String,
    pub binlog_ignore_db: String,
    pub executed_gtid_set: String,
}

impl Source {
    /// A server with binary logging off reports no file.
    pub fn logging(&self) -> bool {
        !self.file.is_empty()
    }
}

/// Case-insensitive lookup of the first name present in the row.
fn field(columns: &[String], row: &[Option<String>], names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(i) = columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .filter(|i| *i < row.len())
        {
            // A present-but-NULL cell counts as absent, so a 5.7 alias that
            // exists and is empty does not mask a populated 8.0 column.
            if let Some(v) = &row[i] {
                return Some(v.clone());
            }
        }
    }
    None
}

fn text(columns: &[String], row: &[Option<String>], names: &[&str]) -> String {
    field(columns, row, names).unwrap_or_default()
}

fn number(columns: &[String], row: &[Option<String>], names: &[&str]) -> u64 {
    field(columns, row, names)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// Parses `SHOW REPLICA STATUS` / `SHOW SLAVE STATUS`. One entry per channel;
/// an empty grid means this server is not a replica.
pub fn parse_replicas(grid: &Grid) -> Vec<Replica> {
    grid.rows
        .iter()
        .map(|row| Replica {
            channel: text(&grid.columns, row, &["Channel_Name"]),
            source_host: text(&grid.columns, row, &["Source_Host", "Master_Host"]),
            source_port: text(&grid.columns, row, &["Source_Port", "Master_Port"]),
            source_user: text(&grid.columns, row, &["Source_User", "Master_User"]),
            io_running: text(
                &grid.columns,
                row,
                &["Replica_IO_Running", "Slave_IO_Running"],
            ),
            sql_running: text(
                &grid.columns,
                row,
                &["Replica_SQL_Running", "Slave_SQL_Running"],
            ),
            // NULL here is meaningful, so it is read through `field` rather
            // than defaulted to zero.
            seconds_behind: field(
                &grid.columns,
                row,
                &["Seconds_Behind_Source", "Seconds_Behind_Master"],
            )
            .and_then(|v| v.trim().parse().ok()),
            source_log_file: text(&grid.columns, row, &["Source_Log_File", "Master_Log_File"]),
            read_source_log_pos: number(
                &grid.columns,
                row,
                &["Read_Source_Log_Pos", "Read_Master_Log_Pos"],
            ),
            relay_source_log_file: text(
                &grid.columns,
                row,
                &["Relay_Source_Log_File", "Relay_Master_Log_File"],
            ),
            exec_source_log_pos: number(
                &grid.columns,
                row,
                &["Exec_Source_Log_Pos", "Exec_Master_Log_Pos"],
            ),
            relay_log_space: number(&grid.columns, row, &["Relay_Log_Space"]),
            last_io_errno: text(&grid.columns, row, &["Last_IO_Errno"]),
            last_io_error: text(&grid.columns, row, &["Last_IO_Error"]),
            last_sql_errno: text(&grid.columns, row, &["Last_SQL_Errno"]),
            last_sql_error: text(&grid.columns, row, &["Last_SQL_Error"]),
            sql_state: text(
                &grid.columns,
                row,
                &["Replica_SQL_Running_State", "Slave_SQL_Running_State"],
            ),
            auto_position: text(&grid.columns, row, &["Auto_Position"]),
            retrieved_gtid_set: text(&grid.columns, row, &["Retrieved_Gtid_Set"]),
            executed_gtid_set: text(&grid.columns, row, &["Executed_Gtid_Set"]),
            sql_delay: number(&grid.columns, row, &["SQL_Delay"]),
        })
        .collect()
}

/// Parses `SHOW MASTER STATUS` / `SHOW BINARY LOG STATUS`.
pub fn parse_source(grid: &Grid) -> Option<Source> {
    let row = grid.rows.first()?;
    Some(Source {
        file: text(&grid.columns, row, &["File"]),
        position: number(&grid.columns, row, &["Position"]),
        binlog_do_db: text(&grid.columns, row, &["Binlog_Do_DB"]),
        binlog_ignore_db: text(&grid.columns, row, &["Binlog_Ignore_DB"]),
        executed_gtid_set: text(&grid.columns, row, &["Executed_Gtid_Set"]),
    })
}

/// The worst health across every channel, for the tab label.
pub fn overall(replicas: &[Replica]) -> Option<Health> {
    replicas.iter().map(|r| r.health()).max_by_key(|h| match h {
        Health::Good => 0,
        Health::Watch => 1,
        Health::Bad => 2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(columns: &[&str], row: &[Option<&str>]) -> Grid {
        Grid {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows: vec![row.iter().map(|c| c.map(str::to_string)).collect()],
            origins: Vec::new(),
        }
    }

    /// MySQL 5.7 column spelling.
    fn replica_57() -> Grid {
        grid(
            &[
                "Slave_IO_State",
                "Master_Host",
                "Master_User",
                "Master_Port",
                "Master_Log_File",
                "Read_Master_Log_Pos",
                "Relay_Master_Log_File",
                "Exec_Master_Log_Pos",
                "Slave_IO_Running",
                "Slave_SQL_Running",
                "Last_Errno",
                "Seconds_Behind_Master",
                "Last_IO_Errno",
                "Last_IO_Error",
                "Last_SQL_Errno",
                "Last_SQL_Error",
                "Relay_Log_Space",
                "SQL_Delay",
                "Channel_Name",
            ],
            &[
                Some("Waiting for master to send event"),
                Some("10.0.0.1"),
                Some("repl"),
                Some("3306"),
                Some("binlog.000012"),
                Some("70000"),
                Some("binlog.000012"),
                Some("64000"),
                Some("Yes"),
                Some("Yes"),
                Some("0"),
                Some("4"),
                Some("0"),
                Some(""),
                Some("0"),
                Some(""),
                Some("1024"),
                Some("0"),
                Some(""),
            ],
        )
    }

    /// MySQL 8.0.22+ column spelling.
    fn replica_80() -> Grid {
        grid(
            &[
                "Replica_IO_State",
                "Source_Host",
                "Source_User",
                "Source_Port",
                "Source_Log_File",
                "Read_Source_Log_Pos",
                "Relay_Source_Log_File",
                "Exec_Source_Log_Pos",
                "Replica_IO_Running",
                "Replica_SQL_Running",
                "Seconds_Behind_Source",
                "Last_IO_Errno",
                "Last_IO_Error",
                "Last_SQL_Errno",
                "Last_SQL_Error",
                "Replica_SQL_Running_State",
                "Auto_Position",
                "Retrieved_Gtid_Set",
                "Executed_Gtid_Set",
                "Channel_Name",
            ],
            &[
                Some("Waiting for source to send event"),
                Some("10.0.0.2"),
                Some("repl"),
                Some("3306"),
                Some("binlog.000090"),
                Some("500"),
                Some("binlog.000090"),
                Some("500"),
                Some("Yes"),
                Some("Yes"),
                Some("0"),
                Some("0"),
                Some(""),
                Some("0"),
                Some(""),
                Some("Replica has read all relay log"),
                Some("1"),
                Some("aaaa:1-9"),
                Some("aaaa:1-9"),
                Some("ch1"),
            ],
        )
    }

    #[test]
    fn the_5_7_column_names_are_understood() {
        let r = &parse_replicas(&replica_57())[0];
        assert_eq!(r.source_host, "10.0.0.1");
        assert_eq!(r.source_port, "3306");
        assert_eq!(r.io_running, "Yes");
        assert_eq!(r.sql_running, "Yes");
        assert_eq!(r.seconds_behind, Some(4));
        assert_eq!(r.read_source_log_pos, 70000);
        assert_eq!(r.exec_source_log_pos, 64000);
        assert_eq!(r.relay_log_space, 1024);
        assert_eq!(r.health(), Health::Good);
    }

    #[test]
    fn the_8_0_column_names_are_understood() {
        let r = &parse_replicas(&replica_80())[0];
        assert_eq!(r.source_host, "10.0.0.2");
        assert_eq!(r.seconds_behind, Some(0));
        assert_eq!(r.channel, "ch1");
        assert_eq!(r.auto_position, "1");
        assert_eq!(r.sql_state, "Replica has read all relay log");
        assert_eq!(r.health(), Health::Good);
    }

    #[test]
    fn the_apply_backlog_is_the_gap_between_receiver_and_applier() {
        let r = &parse_replicas(&replica_57())[0];
        assert_eq!(r.apply_backlog_bytes(), Some(6000));
        let caught_up = &parse_replicas(&replica_80())[0];
        assert_eq!(caught_up.apply_backlog_bytes(), Some(0));
    }

    #[test]
    fn a_backlog_across_two_files_is_not_reported_as_bytes() {
        let mut g = replica_57();
        let i = g
            .columns
            .iter()
            .position(|c| c == "Relay_Master_Log_File")
            .unwrap();
        g.rows[0][i] = Some("binlog.000011".to_string());
        assert_eq!(parse_replicas(&g)[0].apply_backlog_bytes(), None);
    }

    #[test]
    fn a_stopped_sql_thread_is_bad_however_small_the_lag() {
        let mut g = replica_57();
        let i = g
            .columns
            .iter()
            .position(|c| c == "Slave_SQL_Running")
            .unwrap();
        g.rows[0][i] = Some("No".to_string());
        let r = &parse_replicas(&g)[0];
        assert_eq!(r.health(), Health::Bad);
        assert_eq!(r.summary(), "IO Yes / SQL No");
    }

    #[test]
    fn a_null_lag_is_not_treated_as_caught_up() {
        let mut g = replica_57();
        let i = g
            .columns
            .iter()
            .position(|c| c == "Seconds_Behind_Master")
            .unwrap();
        g.rows[0][i] = None;
        let r = &parse_replicas(&g)[0];
        assert_eq!(r.seconds_behind, None);
        assert_eq!(r.health(), Health::Bad);
        assert_eq!(r.summary(), "not replicating");
    }

    #[test]
    fn lag_crosses_the_thresholds() {
        let lag = |secs: i64| {
            let mut g = replica_57();
            let i = g
                .columns
                .iter()
                .position(|c| c == "Seconds_Behind_Master")
                .unwrap();
            g.rows[0][i] = Some(secs.to_string());
            parse_replicas(&g)[0].health()
        };
        assert_eq!(lag(0), Health::Good);
        assert_eq!(lag(LAG_WATCH_SECS - 1), Health::Good);
        assert_eq!(lag(LAG_WATCH_SECS), Health::Watch);
        assert_eq!(lag(LAG_BAD_SECS - 1), Health::Watch);
        assert_eq!(lag(LAG_BAD_SECS), Health::Bad);
    }

    #[test]
    fn an_error_number_makes_it_bad_and_is_reported() {
        let mut g = replica_57();
        let errno = g
            .columns
            .iter()
            .position(|c| c == "Last_SQL_Errno")
            .unwrap();
        let err = g
            .columns
            .iter()
            .position(|c| c == "Last_SQL_Error")
            .unwrap();
        g.rows[0][errno] = Some("1062".to_string());
        g.rows[0][err] = Some("Duplicate entry '7' for key 'PRIMARY'".to_string());
        let r = &parse_replicas(&g)[0];
        assert!(r.has_error());
        assert_eq!(r.health(), Health::Bad);
        assert_eq!(
            r.error_text().unwrap(),
            "SQL thread error 1062: Duplicate entry '7' for key 'PRIMARY'"
        );
    }

    #[test]
    fn a_healthy_replica_reports_no_error() {
        assert!(parse_replicas(&replica_57())[0].error_text().is_none());
        assert!(!parse_replicas(&replica_80())[0].has_error());
    }

    #[test]
    fn a_server_that_is_not_a_replica_yields_nothing() {
        let empty = Grid::default();
        assert!(parse_replicas(&empty).is_empty());
        assert_eq!(overall(&[]), None);
    }

    #[test]
    fn the_worst_channel_decides_the_overall_health() {
        let mut good = parse_replicas(&replica_57());
        let mut bad = good.clone();
        bad[0].sql_running = "No".to_string();
        good.extend(bad);
        assert_eq!(overall(&good), Some(Health::Bad));
    }

    #[test]
    fn the_source_position_parses() {
        let g = grid(
            &[
                "File",
                "Position",
                "Binlog_Do_DB",
                "Binlog_Ignore_DB",
                "Executed_Gtid_Set",
            ],
            &[
                Some("binlog.000012"),
                Some("70000"),
                Some(""),
                Some(""),
                Some("aaaa:1-9"),
            ],
        );
        let s = parse_source(&g).unwrap();
        assert_eq!(s.file, "binlog.000012");
        assert_eq!(s.position, 70000);
        assert!(s.logging());
    }

    #[test]
    fn a_server_with_binary_logging_off_reports_no_source() {
        assert_eq!(parse_source(&Grid::default()), None);
        assert!(!Source::default().logging());
    }

    #[test]
    fn an_alias_column_that_is_null_does_not_mask_the_real_one() {
        // A row carrying both spellings, the old one NULL.
        let g = grid(
            &["Seconds_Behind_Master", "Seconds_Behind_Source"],
            &[None, Some("7")],
        );
        assert_eq!(parse_replicas(&g)[0].seconds_behind, Some(7));
    }
}
