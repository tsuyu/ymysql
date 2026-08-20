//! Core data model: raw samples, the metric catalogue, ring-buffer series.

use std::collections::{BTreeMap, HashMap, VecDeque};

/// Max points kept per live series (15 min @ 1s).
pub const HISTORY_CAP: usize = 900;

/// One poll of the server, as raw as possible. Rates are derived later.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    /// Seconds since app start (monotonic).
    pub t: f64,
    /// Wall clock of the poll, unix millis — what the on-disk store keys on.
    pub wall_ms: i64,
    /// `SHOW GLOBAL STATUS` rows that parse as integers.
    pub status: HashMap<String, u64>,
    /// All `SHOW GLOBAL STATUS` rows, unparsed.
    pub status_raw: HashMap<String, String>,
    pub processlist: Vec<ProcessRow>,
}

impl Sample {
    pub fn stat(&self, key: &str) -> u64 {
        self.status.get(key).copied().unwrap_or(0)
    }

    /// Whether the server reported this counter at all, which is not the same
    /// as it being zero.
    pub fn has_stat(&self, key: &str) -> bool {
        self.status.contains_key(key)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProcessRow {
    pub id: u64,
    pub user: String,
    pub host: String,
    pub db: String,
    pub command: String,
    pub time: i64,
    pub state: String,
    pub info: String,
}

/// Per-second rates computed from two consecutive samples.
#[derive(Debug, Clone, Copy, Default)]
pub struct Derived {
    pub qps: f64,
    pub tps: f64,
    pub slow_qps: f64,
    pub threads_connected: f64,
    pub threads_running: f64,
    pub bytes_in_s: f64,
    pub bytes_out_s: f64,
    pub innodb_rows_read_s: f64,
    pub innodb_rows_written_s: f64,
    /// InnoDB buffer pool hit ratio over the interval, 0.0..=1.0.
    pub bp_hit_ratio: f64,
    pub table_locks_waited_s: f64,
    pub aborted_connects_s: f64,
    pub created_tmp_disk_tables_s: f64,
    /// `Threads_connected` as a share of `max_connections`. Filled in by the
    /// app, which is where the server's limit is known.
    pub conn_usage_pct: f64,
}

impl Derived {
    /// Rates between `prev` and `cur`. Counter resets (server restart) yield 0.
    pub fn between(prev: &Sample, cur: &Sample) -> Self {
        let dt = cur.t - prev.t;
        if dt <= 0.0 {
            return Self::default();
        }
        let d = |k: &str| -> f64 {
            let (a, b) = (prev.stat(k), cur.stat(k));
            if b >= a { (b - a) as f64 } else { 0.0 }
        };
        let rate = |k: &str| d(k) / dt;

        let bp_reads = d("Innodb_buffer_pool_reads");
        let bp_reqs = d("Innodb_buffer_pool_read_requests");
        let bp_hit_ratio = if bp_reqs > 0.0 {
            (1.0 - bp_reads / bp_reqs).clamp(0.0, 1.0)
        } else {
            1.0
        };

        // `Com_commit` counts only *explicit* COMMIT statements, so on an
        // autocommit workload -- which is most of them -- it stays at zero
        // while the server is busy, and a TPS card reading 0.00 at 900 qps
        // looks broken. `Handler_commit` counts the commit each statement
        // performs, implicit ones included, which is the real transaction
        // rate. Older forks without the handler counters fall back.
        let tps = if cur.has_stat("Handler_commit") {
            (d("Handler_commit") + d("Handler_rollback")) / dt
        } else {
            (d("Com_commit") + d("Com_rollback")) / dt
        };

        Self {
            qps: rate("Queries"),
            tps,
            slow_qps: rate("Slow_queries"),
            threads_connected: cur.stat("Threads_connected") as f64,
            threads_running: cur.stat("Threads_running") as f64,
            bytes_in_s: rate("Bytes_received"),
            bytes_out_s: rate("Bytes_sent"),
            innodb_rows_read_s: rate("Innodb_rows_read"),
            innodb_rows_written_s: (d("Innodb_rows_inserted")
                + d("Innodb_rows_updated")
                + d("Innodb_rows_deleted"))
                / dt,
            bp_hit_ratio,
            table_locks_waited_s: rate("Table_locks_waited"),
            aborted_connects_s: rate("Aborted_connects"),
            created_tmp_disk_tables_s: rate("Created_tmp_disk_tables"),
            // Filled in by the app once the server's limit is known.
            conn_usage_pct: 0.0,
        }
    }
}

/// The metric catalogue. One entry per plottable/alertable number; the `key` is
/// also the on-disk column in the metrics store, so it must stay stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Metric {
    Qps,
    Tps,
    SlowQps,
    ThreadsRunning,
    ThreadsConnected,
    BpHitPct,
    BytesIn,
    BytesOut,
    RowsRead,
    RowsWritten,
    TmpDiskTables,
    TableLockWaits,
    AbortedConnects,
    ConnUsage,
}

impl Metric {
    pub const ALL: [Metric; 14] = [
        Metric::Qps,
        Metric::Tps,
        Metric::SlowQps,
        Metric::ThreadsRunning,
        Metric::ThreadsConnected,
        Metric::BpHitPct,
        Metric::BytesIn,
        Metric::BytesOut,
        Metric::RowsRead,
        Metric::RowsWritten,
        Metric::TmpDiskTables,
        Metric::TableLockWaits,
        Metric::AbortedConnects,
        Metric::ConnUsage,
    ];

    /// Stable storage key. Never rename without a migration.
    pub fn key(self) -> &'static str {
        match self {
            Metric::Qps => "qps",
            Metric::Tps => "tps",
            Metric::SlowQps => "slow_qps",
            Metric::ThreadsRunning => "threads_running",
            Metric::ThreadsConnected => "threads_connected",
            Metric::BpHitPct => "bp_hit_pct",
            Metric::BytesIn => "bytes_in_s",
            Metric::BytesOut => "bytes_out_s",
            Metric::RowsRead => "rows_read_s",
            Metric::RowsWritten => "rows_written_s",
            Metric::TmpDiskTables => "tmp_disk_tables_s",
            Metric::TableLockWaits => "table_lock_waits_s",
            Metric::AbortedConnects => "aborted_connects_s",
            Metric::ConnUsage => "conn_usage_pct",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Metric::Qps => "Queries/s",
            Metric::Tps => "Transactions/s",
            Metric::SlowQps => "Slow queries/s",
            Metric::ThreadsRunning => "Threads running",
            Metric::ThreadsConnected => "Threads connected",
            Metric::BpHitPct => "Buffer pool hit %",
            Metric::BytesIn => "Network in",
            Metric::BytesOut => "Network out",
            Metric::RowsRead => "InnoDB rows read/s",
            Metric::RowsWritten => "InnoDB rows written/s",
            Metric::TmpDiskTables => "Tmp disk tables/s",
            Metric::TableLockWaits => "Table lock waits/s",
            Metric::AbortedConnects => "Aborted connects/s",
            Metric::ConnUsage => "Connection usage %",
        }
    }

    pub fn value(self, d: &Derived) -> f64 {
        match self {
            Metric::Qps => d.qps,
            Metric::Tps => d.tps,
            Metric::SlowQps => d.slow_qps,
            Metric::ThreadsRunning => d.threads_running,
            Metric::ThreadsConnected => d.threads_connected,
            Metric::BpHitPct => d.bp_hit_ratio * 100.0,
            Metric::BytesIn => d.bytes_in_s,
            Metric::BytesOut => d.bytes_out_s,
            Metric::RowsRead => d.innodb_rows_read_s,
            Metric::RowsWritten => d.innodb_rows_written_s,
            Metric::TmpDiskTables => d.created_tmp_disk_tables_s,
            Metric::TableLockWaits => d.table_locks_waited_s,
            Metric::AbortedConnects => d.aborted_connects_s,
            Metric::ConnUsage => d.conn_usage_pct,
        }
    }

    /// True when the metric is a byte rate (formatted as KiB/MiB).
    pub fn is_bytes(self) -> bool {
        matches!(self, Metric::BytesIn | Metric::BytesOut)
    }
}

/// Fixed-capacity time series for plotting.
#[derive(Debug, Clone, Default)]
pub struct Series {
    pub points: VecDeque<[f64; 2]>,
}

impl Series {
    pub fn push(&mut self, t: f64, v: f64) {
        if self.points.len() >= HISTORY_CAP {
            self.points.pop_front();
        }
        self.points.push_back([t, v]);
    }

    pub fn as_vec(&self) -> Vec<[f64; 2]> {
        self.points.iter().copied().collect()
    }
}

/// Live in-memory history, one series per metric.
#[derive(Debug, Clone, Default)]
pub struct History {
    series: BTreeMap<Metric, Series>,
}

impl History {
    pub fn push(&mut self, t: f64, d: &Derived) {
        for m in Metric::ALL {
            self.series.entry(m).or_default().push(t, m.value(d));
        }
    }

    pub fn points(&self, m: Metric) -> Vec<[f64; 2]> {
        self.series.get(&m).map(Series::as_vec).unwrap_or_default()
    }

    pub fn clear(&mut self) {
        self.series.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t: f64, stats: &[(&str, u64)]) -> Sample {
        Sample {
            t,
            wall_ms: (t * 1000.0) as i64,
            status: stats.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            status_raw: HashMap::new(),
            processlist: Vec::new(),
        }
    }

    #[test]
    fn rates_are_per_second_over_the_real_interval() {
        // The gap matters: a resumed collector must not turn a large delta
        // into a spike by dividing by the nominal interval.
        let a = sample(0.0, &[("Queries", 100)]);
        let b = sample(10.0, &[("Queries", 200)]);
        assert_eq!(Derived::between(&a, &b).qps, 10.0);
    }

    #[test]
    fn a_counter_reset_yields_zero_rather_than_a_negative_rate() {
        let a = sample(0.0, &[("Queries", 5_000)]);
        let b = sample(1.0, &[("Queries", 7)]);
        assert_eq!(Derived::between(&a, &b).qps, 0.0);
    }

    #[test]
    fn a_zero_or_backwards_interval_is_ignored() {
        let a = sample(5.0, &[("Queries", 100)]);
        let b = sample(5.0, &[("Queries", 200)]);
        assert_eq!(Derived::between(&a, &b).qps, 0.0);
        let c = sample(4.0, &[("Queries", 200)]);
        assert_eq!(Derived::between(&a, &c).qps, 0.0);
    }

    #[test]
    fn tps_counts_autocommit_transactions() {
        // An autocommit workload: Com_commit never moves, Handler_commit does.
        let a = sample(
            0.0,
            &[
                ("Com_commit", 3),
                ("Com_rollback", 0),
                ("Handler_commit", 1_000),
            ],
        );
        let b = sample(
            1.0,
            &[
                ("Com_commit", 3),
                ("Com_rollback", 0),
                ("Handler_commit", 1_050),
            ],
        );
        assert_eq!(
            Derived::between(&a, &b).tps,
            50.0,
            "implicit commits have to count, or a busy server reads as idle"
        );
    }

    #[test]
    fn tps_includes_rollbacks() {
        let a = sample(0.0, &[("Handler_commit", 10), ("Handler_rollback", 1)]);
        let b = sample(1.0, &[("Handler_commit", 14), ("Handler_rollback", 3)]);
        assert_eq!(Derived::between(&a, &b).tps, 6.0);
    }

    #[test]
    fn tps_falls_back_to_com_counters_when_the_handler_ones_are_absent() {
        let a = sample(0.0, &[("Com_commit", 10), ("Com_rollback", 0)]);
        let b = sample(1.0, &[("Com_commit", 17), ("Com_rollback", 1)]);
        assert_eq!(Derived::between(&a, &b).tps, 8.0);
    }

    #[test]
    fn a_present_zero_counter_is_not_treated_as_missing() {
        // Handler_commit exists but has not moved: the answer is 0 tps, and
        // the fallback must not kick in and report Com_commit instead.
        let a = sample(0.0, &[("Handler_commit", 5), ("Com_commit", 100)]);
        let b = sample(1.0, &[("Handler_commit", 5), ("Com_commit", 200)]);
        assert_eq!(Derived::between(&a, &b).tps, 0.0);
    }

    #[test]
    fn the_buffer_pool_hit_ratio_is_a_fraction_of_requests() {
        let a = sample(
            0.0,
            &[
                ("Innodb_buffer_pool_reads", 0),
                ("Innodb_buffer_pool_read_requests", 0),
            ],
        );
        let b = sample(
            1.0,
            &[
                ("Innodb_buffer_pool_reads", 1),
                ("Innodb_buffer_pool_read_requests", 100),
            ],
        );
        assert!((Derived::between(&a, &b).bp_hit_ratio - 0.99).abs() < 1e-9);
    }

    #[test]
    fn an_idle_buffer_pool_reads_as_a_perfect_hit_ratio() {
        // No requests at all: reporting 0% would light up every alert.
        let a = sample(0.0, &[("Innodb_buffer_pool_read_requests", 10)]);
        let b = sample(1.0, &[("Innodb_buffer_pool_read_requests", 10)]);
        assert_eq!(Derived::between(&a, &b).bp_hit_ratio, 1.0);
    }

    #[test]
    fn rows_written_sums_the_three_write_counters() {
        let a = sample(
            0.0,
            &[
                ("Innodb_rows_inserted", 1),
                ("Innodb_rows_updated", 1),
                ("Innodb_rows_deleted", 1),
            ],
        );
        let b = sample(
            2.0,
            &[
                ("Innodb_rows_inserted", 3),
                ("Innodb_rows_updated", 5),
                ("Innodb_rows_deleted", 7),
            ],
        );
        assert_eq!(Derived::between(&a, &b).innodb_rows_written_s, 6.0);
    }

    #[test]
    fn every_metric_has_a_distinct_stable_key() {
        let mut keys: Vec<&str> = Metric::ALL.iter().map(|m| m.key()).collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "duplicate metric key");
        assert_eq!(count, Metric::ALL.len());
    }
}
