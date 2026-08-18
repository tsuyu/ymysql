//! InnoDB internals: buffer pool, redo log, checkpoint age, row locks.
//!
//! Most of it comes from `SHOW GLOBAL STATUS` counters, but the two numbers
//! that matter most during an incident — history list length and checkpoint age
//! — are only in `SHOW ENGINE INNODB STATUS`, whose output is a text report
//! whose layout differs between 5.7 and 8.0. Parsing it is therefore done here,
//! line by line, and tested against both shapes.

use std::collections::HashMap;

/// Numbers scraped out of `SHOW ENGINE INNODB STATUS`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineStatus {
    /// Undo records still to be purged. Growing = a long transaction is
    /// pinning old row versions.
    pub history_list_length: u64,
    /// Current log sequence number.
    pub lsn: u64,
    pub log_flushed_up_to: u64,
    pub pages_flushed_up_to: u64,
    pub last_checkpoint: u64,
    /// Buffer pool pages, as the engine reports them.
    pub buffer_pool_pages: u64,
    pub free_buffers: u64,
    pub database_pages: u64,
    pub modified_pages: u64,
    /// `OS WAIT ARRAY INFO: reservation count` — contention on internal latches.
    pub os_wait_reservations: u64,
}

impl EngineStatus {
    /// Bytes of redo generated since the last checkpoint. When this approaches
    /// the redo capacity InnoDB starts furious flushing and throughput falls
    /// off a cliff.
    pub fn checkpoint_age(&self) -> u64 {
        self.lsn.saturating_sub(self.last_checkpoint)
    }

    /// Redo written but not yet flushed to disk.
    pub fn log_flush_lag(&self) -> u64 {
        self.lsn.saturating_sub(self.log_flushed_up_to)
    }
}

/// Last integer on a line, e.g. `History list length 42` -> 42.
fn trailing_number(line: &str) -> Option<u64> {
    line.split_whitespace()
        .rev()
        .find_map(|tok| tok.trim_end_matches(&[',', '.'][..]).parse::<u64>().ok())
}

/// First integer after a prefix, for lines that carry more than one number.
fn number_after(line: &str, prefix: &str) -> Option<u64> {
    let rest = line.trim().strip_prefix(prefix)?;
    rest.split_whitespace()
        .find_map(|tok| tok.trim_end_matches(&[',', '.'][..]).parse::<u64>().ok())
}

/// Parses the report. Unknown or missing lines simply stay zero: the layout
/// differs across versions and forks, and a missing number must never cost us
/// the ones we did find.
pub fn parse_engine_status(text: &str) -> EngineStatus {
    let mut s = EngineStatus::default();

    for line in text.lines() {
        let t = line.trim();

        if let Some(v) = number_after(t, "History list length") {
            s.history_list_length = v;
        } else if t.starts_with("Log sequence number") {
            s.lsn = trailing_number(t).unwrap_or(0);
        } else if t.starts_with("Log flushed up to") {
            s.log_flushed_up_to = trailing_number(t).unwrap_or(0);
        } else if t.starts_with("Pages flushed up to") {
            s.pages_flushed_up_to = trailing_number(t).unwrap_or(0);
        } else if t.starts_with("Last checkpoint at") {
            s.last_checkpoint = trailing_number(t).unwrap_or(0);
        } else if let Some(v) = number_after(t, "Buffer pool size") {
            // "Buffer pool size   8192" (pages). The per-instance repeats that
            // follow report the same field; keep the first, which is the total.
            if s.buffer_pool_pages == 0 {
                s.buffer_pool_pages = v;
            }
        } else if let Some(v) = number_after(t, "Free buffers") {
            if s.free_buffers == 0 {
                s.free_buffers = v;
            }
        } else if let Some(v) = number_after(t, "Database pages") {
            if s.database_pages == 0 {
                s.database_pages = v;
            }
        } else if let Some(v) = number_after(t, "Modified db pages") {
            if s.modified_pages == 0 {
                s.modified_pages = v;
            }
        } else if let Some(v) = number_after(t, "OS WAIT ARRAY INFO: reservation count") {
            s.os_wait_reservations = v;
        }
    }
    s
}

/// InnoDB settings that give the counters meaning. Read once per connection.
#[derive(Debug, Clone, Default)]
pub struct InnodbConfig {
    pub buffer_pool_bytes: u64,
    pub buffer_pool_instances: u64,
    /// Total redo capacity in bytes, however this version expresses it.
    pub redo_capacity_bytes: u64,
    pub io_capacity: u64,
    pub io_capacity_max: u64,
    pub flush_log_at_trx_commit: u64,
    pub max_dirty_pages_pct: f64,
    pub page_size: u64,
}

impl InnodbConfig {
    /// Builds the config from `SHOW GLOBAL VARIABLES`.
    ///
    /// Redo capacity moved in 8.0.30: `innodb_redo_log_capacity` replaced
    /// `innodb_log_file_size × innodb_log_files_in_group`. Both are handled.
    pub fn from_variables(vars: &HashMap<String, String>) -> Self {
        let num = |k: &str| -> u64 { vars.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) };
        let float = |k: &str| -> f64 {
            vars.get(k)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
        };

        let redo_capacity_bytes = match num("innodb_redo_log_capacity") {
            0 => {
                let files = num("innodb_log_files_in_group").max(1);
                num("innodb_log_file_size") * files
            }
            capacity => capacity,
        };

        Self {
            buffer_pool_bytes: num("innodb_buffer_pool_size"),
            buffer_pool_instances: num("innodb_buffer_pool_instances").max(1),
            redo_capacity_bytes,
            io_capacity: num("innodb_io_capacity"),
            io_capacity_max: num("innodb_io_capacity_max"),
            flush_log_at_trx_commit: num("innodb_flush_log_at_trx_commit"),
            max_dirty_pages_pct: float("innodb_max_dirty_pages_pct"),
            page_size: match num("innodb_page_size") {
                0 => 16384,
                p => p,
            },
        }
    }
}

/// Everything the InnoDB tab shows, one sample's worth.
#[derive(Debug, Clone, Default)]
pub struct InnodbSnapshot {
    // Buffer pool
    pub pages_total: u64,
    pub pages_data: u64,
    pub pages_dirty: u64,
    pub pages_free: u64,
    pub dirty_pct: f64,
    pub hit_ratio_pct: f64,
    /// Times a thread waited for a free page — never zero on a healthy server
    /// under load, but growth means the pool is too small.
    pub wait_free: u64,

    // Redo log
    pub log_waits: u64,
    pub checkpoint_age: u64,
    pub checkpoint_age_pct: f64,
    pub log_flush_lag: u64,

    // Transactions
    pub history_list_length: u64,

    // Row locks
    pub row_lock_waits: u64,
    pub row_lock_current_waits: u64,
    pub row_lock_time_avg_ms: u64,
    pub row_lock_time_max_ms: u64,

    // I/O totals (cumulative counters; the tab shows per-second rates too)
    pub data_reads: u64,
    pub data_writes: u64,
    pub data_fsyncs: u64,
    pub os_wait_reservations: u64,
}

/// Combines the status counters, the parsed engine report and the config.
pub fn snapshot(
    status: &HashMap<String, u64>,
    engine: &EngineStatus,
    config: &InnodbConfig,
) -> InnodbSnapshot {
    let get = |k: &str| status.get(k).copied().unwrap_or(0);

    let pages_total = match get("Innodb_buffer_pool_pages_total") {
        0 => engine.buffer_pool_pages,
        p => p,
    };
    let pages_dirty = match get("Innodb_buffer_pool_pages_dirty") {
        0 => engine.modified_pages,
        p => p,
    };
    let dirty_pct = if pages_total > 0 {
        pages_dirty as f64 / pages_total as f64 * 100.0
    } else {
        0.0
    };

    // Lifetime hit ratio. The dashboard's is per-interval; this one answers
    // "is the pool big enough for the working set", which is a longer question.
    let reqs = get("Innodb_buffer_pool_read_requests");
    let reads = get("Innodb_buffer_pool_reads");
    let hit_ratio_pct = if reqs > 0 {
        (1.0 - reads as f64 / reqs as f64).clamp(0.0, 1.0) * 100.0
    } else {
        100.0
    };

    let checkpoint_age = engine.checkpoint_age();
    let checkpoint_age_pct = if config.redo_capacity_bytes > 0 {
        checkpoint_age as f64 / config.redo_capacity_bytes as f64 * 100.0
    } else {
        0.0
    };

    InnodbSnapshot {
        pages_total,
        pages_data: get("Innodb_buffer_pool_pages_data"),
        pages_dirty,
        pages_free: match get("Innodb_buffer_pool_pages_free") {
            0 => engine.free_buffers,
            p => p,
        },
        dirty_pct,
        hit_ratio_pct,
        wait_free: get("Innodb_buffer_pool_wait_free"),
        log_waits: get("Innodb_log_waits"),
        checkpoint_age,
        checkpoint_age_pct,
        log_flush_lag: engine.log_flush_lag(),
        history_list_length: engine.history_list_length,
        row_lock_waits: get("Innodb_row_lock_waits"),
        row_lock_current_waits: get("Innodb_row_lock_current_waits"),
        row_lock_time_avg_ms: get("Innodb_row_lock_time_avg"),
        row_lock_time_max_ms: get("Innodb_row_lock_time_max"),
        data_reads: get("Innodb_data_reads"),
        data_writes: get("Innodb_data_writes"),
        data_fsyncs: get("Innodb_data_fsyncs"),
        os_wait_reservations: engine.os_wait_reservations,
    }
}

/// Health bands for the numbers where a threshold is well established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Good,
    Watch,
    Bad,
}

impl InnodbSnapshot {
    /// Checkpoint age past ~75% of redo capacity is where InnoDB starts
    /// aggressive flushing; past 90% it is effectively stalling on redo.
    pub fn checkpoint_health(&self) -> Health {
        band(self.checkpoint_age_pct, 75.0, 90.0)
    }

    /// A dirty ratio at or above the configured maximum means the flusher is
    /// behind the workload.
    pub fn dirty_health(&self, config: &InnodbConfig) -> Health {
        let limit = if config.max_dirty_pages_pct > 0.0 {
            config.max_dirty_pages_pct
        } else {
            75.0
        };
        band(self.dirty_pct, limit * 0.8, limit)
    }

    pub fn hit_ratio_health(&self) -> Health {
        // Inverted: lower is worse.
        if self.hit_ratio_pct < 95.0 {
            Health::Bad
        } else if self.hit_ratio_pct < 99.0 {
            Health::Watch
        } else {
            Health::Good
        }
    }

    /// A history list in the millions means purge cannot keep up — usually one
    /// forgotten open transaction.
    pub fn history_health(&self) -> Health {
        if self.history_list_length > 1_000_000 {
            Health::Bad
        } else if self.history_list_length > 100_000 {
            Health::Watch
        } else {
            Health::Good
        }
    }
}

fn band(value: f64, watch: f64, bad: f64) -> Health {
    if value >= bad {
        Health::Bad
    } else if value >= watch {
        Health::Watch
    } else {
        Health::Good
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Abridged 8.0 report.
    const STATUS_80: &str = r#"
=====================================
2026-08-18 14:05:01 140234 INNODB MONITOR OUTPUT
=====================================
Per second averages calculated from the last 41 seconds
----------------
BACKGROUND THREAD
----------------
srv_master_thread loops: 1 srv_active, 0 srv_shutdown, 5 srv_idle
----------
SEMAPHORES
----------
OS WAIT ARRAY INFO: reservation count 9412
OS WAIT ARRAY INFO: signal count 8390
------------
TRANSACTIONS
------------
Trx id counter 47892112
Purge done for trx's n:o < 47892100 undo n:o < 0 state: running but idle
History list length 3821
---
LOG
---
Log sequence number          1289365423
Log buffer assigned up to    1289365423
Log written up to            1289365423
Log flushed up to            1289360000
Added dirty pages up to      1289365423
Pages flushed up to          1288000000
Last checkpoint at           1287000000
----------------------
BUFFER POOL AND MEMORY
----------------------
Total large memory allocated 2198863872
Buffer pool size   131072
Free buffers       8192
Database pages     120000
Old database pages 44000
Modified db pages  9000
"#;

    /// Abridged 5.7 report — same facts, different spacing and extra lines.
    const STATUS_57: &str = r#"
=====================================
2026-08-18 14:05:01 0x7f INNODB MONITOR OUTPUT
=====================================
----------
SEMAPHORES
----------
OS WAIT ARRAY INFO: reservation count 512
------------
TRANSACTIONS
------------
Trx id counter 900123
Purge done for trx's n:o < 900000 undo n:o < 0 state: running but idle
History list length 77
---
LOG
---
Log sequence number 500000000
Log flushed up to   499999000
Pages flushed up to 499000000
Last checkpoint at  498000000
0 pending log flushes, 0 pending chkp writes
----------------------
BUFFER POOL AND MEMORY
----------------------
Buffer pool size   8192
Free buffers       1024
Database pages     7000
Modified db pages  120
"#;

    #[test]
    fn parses_the_8_0_report() {
        let s = parse_engine_status(STATUS_80);
        assert_eq!(s.history_list_length, 3821);
        assert_eq!(s.lsn, 1_289_365_423);
        assert_eq!(s.log_flushed_up_to, 1_289_360_000);
        assert_eq!(s.pages_flushed_up_to, 1_288_000_000);
        assert_eq!(s.last_checkpoint, 1_287_000_000);
        assert_eq!(s.buffer_pool_pages, 131_072);
        assert_eq!(s.free_buffers, 8192);
        assert_eq!(s.database_pages, 120_000);
        assert_eq!(s.modified_pages, 9000);
        assert_eq!(s.os_wait_reservations, 9412);
    }

    #[test]
    fn parses_the_5_7_report() {
        let s = parse_engine_status(STATUS_57);
        assert_eq!(s.history_list_length, 77);
        assert_eq!(s.lsn, 500_000_000);
        assert_eq!(s.last_checkpoint, 498_000_000);
        assert_eq!(s.modified_pages, 120);
        assert_eq!(s.checkpoint_age(), 2_000_000);
        assert_eq!(s.log_flush_lag(), 1000);
    }

    #[test]
    fn a_truncated_report_yields_zeroes_not_garbage() {
        let s = parse_engine_status("=====\nnothing useful here\n");
        assert_eq!(s, EngineStatus::default());
        assert_eq!(s.checkpoint_age(), 0);
    }

    #[test]
    fn checkpoint_age_never_underflows() {
        let s = EngineStatus {
            lsn: 100,
            last_checkpoint: 500,
            ..Default::default()
        };
        assert_eq!(s.checkpoint_age(), 0, "a stale read must not wrap around");
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn redo_capacity_comes_from_whichever_variable_exists() {
        let modern =
            InnodbConfig::from_variables(&vars(&[("innodb_redo_log_capacity", "1073741824")]));
        assert_eq!(modern.redo_capacity_bytes, 1_073_741_824);

        let legacy = InnodbConfig::from_variables(&vars(&[
            ("innodb_log_file_size", "536870912"),
            ("innodb_log_files_in_group", "2"),
        ]));
        assert_eq!(
            legacy.redo_capacity_bytes, 1_073_741_824,
            "5.7 multiplies file size by the file count"
        );

        let unknown = InnodbConfig::from_variables(&HashMap::new());
        assert_eq!(unknown.redo_capacity_bytes, 0);
        assert_eq!(unknown.page_size, 16384, "assume the default page size");
    }

    fn status(pairs: &[(&str, u64)]) -> HashMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn snapshot_computes_the_ratios() {
        let st = status(&[
            ("Innodb_buffer_pool_pages_total", 1000),
            ("Innodb_buffer_pool_pages_dirty", 250),
            ("Innodb_buffer_pool_read_requests", 1000),
            ("Innodb_buffer_pool_reads", 10),
            ("Innodb_row_lock_waits", 7),
        ]);
        let engine = parse_engine_status(STATUS_57);
        let config =
            InnodbConfig::from_variables(&vars(&[("innodb_redo_log_capacity", "4000000")]));

        let snap = snapshot(&st, &engine, &config);
        assert_eq!(snap.dirty_pct, 25.0);
        assert_eq!(snap.hit_ratio_pct, 99.0);
        assert_eq!(snap.row_lock_waits, 7);
        assert_eq!(snap.history_list_length, 77);
        assert_eq!(snap.checkpoint_age, 2_000_000);
        assert_eq!(snap.checkpoint_age_pct, 50.0);
    }

    #[test]
    fn snapshot_falls_back_to_the_engine_report() {
        // A server where the status counters are missing still shows a pool.
        let engine = parse_engine_status(STATUS_57);
        let snap = snapshot(&HashMap::new(), &engine, &InnodbConfig::default());
        assert_eq!(snap.pages_total, 8192);
        assert_eq!(snap.pages_dirty, 120);
        assert_eq!(snap.pages_free, 1024);
    }

    #[test]
    fn health_bands_flag_the_classic_failure_modes() {
        let mut snap = InnodbSnapshot {
            checkpoint_age_pct: 10.0,
            dirty_pct: 10.0,
            hit_ratio_pct: 99.9,
            history_list_length: 100,
            ..Default::default()
        };
        let config = InnodbConfig {
            max_dirty_pages_pct: 75.0,
            ..Default::default()
        };
        assert_eq!(snap.checkpoint_health(), Health::Good);
        assert_eq!(snap.dirty_health(&config), Health::Good);
        assert_eq!(snap.hit_ratio_health(), Health::Good);
        assert_eq!(snap.history_health(), Health::Good);

        snap.checkpoint_age_pct = 92.0;
        snap.dirty_pct = 76.0;
        snap.hit_ratio_pct = 80.0;
        snap.history_list_length = 5_000_000;
        assert_eq!(snap.checkpoint_health(), Health::Bad);
        assert_eq!(snap.dirty_health(&config), Health::Bad);
        assert_eq!(snap.hit_ratio_health(), Health::Bad);
        assert_eq!(snap.history_health(), Health::Bad);

        snap.checkpoint_age_pct = 80.0;
        snap.history_list_length = 200_000;
        assert_eq!(snap.checkpoint_health(), Health::Watch);
        assert_eq!(snap.history_health(), Health::Watch);
    }
}
