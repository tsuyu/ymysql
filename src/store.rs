//! On-disk metric store (SQLite). Runs on its own thread because rusqlite is
//! blocking; the GUI talks to it with the same command/event shape as the
//! collector.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use tracing::{info, warn};

use crate::model::Metric;

/// Raw samples are kept this long, then folded into one-minute rollups.
const RAW_RETENTION_S: i64 = 6 * 3600;
/// Rollups are kept this long.
const ROLLUP_RETENTION_S: i64 = 30 * 24 * 3600;
/// Never hand the plot more points than this.
const MAX_PLOT_POINTS: i64 = 2000;

#[derive(Debug)]
pub enum StoreCmd {
    Write {
        server: String,
        wall_ms: i64,
        values: Vec<(Metric, f64)>,
    },
    Query {
        server: String,
        metrics: Vec<Metric>,
        from_ms: i64,
        to_ms: i64,
    },
    ExportCsv {
        path: PathBuf,
        server: String,
        metrics: Vec<Metric>,
        from_ms: i64,
        to_ms: i64,
    },
    Stats,
}

#[derive(Debug)]
pub enum StoreEvent {
    /// Result of a `Query`, one event per metric. `points` are `[unix_secs, v]`.
    Series {
        metric: Metric,
        points: Vec<[f64; 2]>,
        /// True when served from the one-minute rollup rather than raw samples.
        downsampled: bool,
    },
    QueryDone {
        server: String,
    },
    Stats {
        raw_rows: i64,
        rollup_rows: i64,
        oldest_ms: Option<i64>,
        file_bytes: u64,
        path: PathBuf,
    },
    Exported {
        path: PathBuf,
        rows: usize,
    },
    Error(String),
}

pub struct Handle {
    tx: Sender<StoreCmd>,
    rx: Receiver<StoreEvent>,
}

impl Handle {
    pub fn send(&self, cmd: StoreCmd) {
        if let Err(e) = self.tx.send(cmd) {
            warn!("metric store is gone: {e}");
        }
    }

    pub fn drain(&mut self) -> Vec<StoreEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }
}

/// Where this app keeps its files: `%LOCALAPPDATA%\mysql_perf` on Windows,
/// `$XDG_DATA_HOME/mysql_perf` (or `~/.local/share/mysql_perf`) elsewhere.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("mysql_perf")
}

pub fn default_db_path() -> PathBuf {
    data_dir().join("metrics.db")
}

/// Starts the store thread. Failing to open the database is not fatal: the app
/// keeps running with live-only history and reports the error.
pub fn spawn(path: PathBuf) -> Handle {
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let thread_path = path;

    std::thread::Builder::new()
        .name("metric-store".into())
        .spawn(move || match open(&thread_path) {
            Ok(conn) => run(conn, cmd_rx, ev_tx),
            Err(e) => {
                let _ = ev_tx.send(StoreEvent::Error(format!(
                    "metric store disabled: {e:#} ({})",
                    thread_path.display()
                )));
            }
        })
        .expect("spawn metric-store thread");

    Handle {
        tx: cmd_tx,
        rx: ev_rx,
    }
}

fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS samples(
             server TEXT NOT NULL,
             metric TEXT NOT NULL,
             ts     INTEGER NOT NULL, -- unix millis
             value  REAL NOT NULL,
             PRIMARY KEY(server, metric, ts)
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS rollup_1m(
             server TEXT NOT NULL,
             metric TEXT NOT NULL,
             ts     INTEGER NOT NULL, -- unix millis, minute-aligned
             avg    REAL NOT NULL,
             min    REAL NOT NULL,
             max    REAL NOT NULL,
             PRIMARY KEY(server, metric, ts)
         ) WITHOUT ROWID;",
    )?;
    info!(path = %path.display(), "metric store ready");
    Ok(conn)
}

fn run(mut conn: Connection, rx: Receiver<StoreCmd>, ev: Sender<StoreEvent>) {
    let mut last_maintenance = Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(cmd) => {
                if let Err(e) = handle(&mut conn, cmd, &ev) {
                    let _ = ev.send(StoreEvent::Error(format!("{e:#}")));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if last_maintenance.elapsed() >= Duration::from_secs(60) {
            last_maintenance = Instant::now();
            if let Err(e) = maintain(&conn) {
                let _ = ev.send(StoreEvent::Error(format!("store maintenance: {e:#}")));
            }
        }
    }
}

fn handle(conn: &mut Connection, cmd: StoreCmd, ev: &Sender<StoreEvent>) -> Result<()> {
    match cmd {
        StoreCmd::Write {
            server,
            wall_ms,
            values,
        } => {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO samples(server, metric, ts, value) \
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for (metric, value) in values {
                    stmt.execute(params![server, metric.key(), wall_ms, value])?;
                }
            }
            tx.commit()?;
        }

        StoreCmd::Query {
            server,
            metrics,
            from_ms,
            to_ms,
        } => {
            for metric in metrics {
                let (points, downsampled) = read_series(conn, &server, metric, from_ms, to_ms)?;
                let _ = ev.send(StoreEvent::Series {
                    metric,
                    points,
                    downsampled,
                });
            }
            let _ = ev.send(StoreEvent::QueryDone { server });
        }

        StoreCmd::ExportCsv {
            path,
            server,
            metrics,
            from_ms,
            to_ms,
        } => {
            let rows = export_csv(conn, &path, &server, &metrics, from_ms, to_ms)?;
            let _ = ev.send(StoreEvent::Exported { path, rows });
        }

        StoreCmd::Stats => {
            let raw_rows: i64 = conn.query_row("SELECT COUNT(*) FROM samples", [], |r| r.get(0))?;
            let rollup_rows: i64 =
                conn.query_row("SELECT COUNT(*) FROM rollup_1m", [], |r| r.get(0))?;
            let oldest_ms: Option<i64> = conn.query_row(
                "SELECT MIN(ts) FROM (SELECT MIN(ts) ts FROM samples \
                 UNION ALL SELECT MIN(ts) FROM rollup_1m)",
                [],
                |r| r.get(0),
            )?;
            let path: String = conn.query_row("PRAGMA database_list", [], |r| r.get(2))?;
            let path = PathBuf::from(path);
            let file_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let _ = ev.send(StoreEvent::Stats {
                raw_rows,
                rollup_rows,
                oldest_ms,
                file_bytes,
                path,
            });
        }
    }
    Ok(())
}

/// Reads raw samples for short ranges and the rollup for long ones, bucketing
/// so the plot never gets more than `MAX_PLOT_POINTS`.
fn read_series(
    conn: &Connection,
    server: &str,
    metric: Metric,
    from_ms: i64,
    to_ms: i64,
) -> Result<(Vec<[f64; 2]>, bool)> {
    let span_ms = (to_ms - from_ms).max(1);
    let use_rollup = span_ms > RAW_RETENTION_S * 1000;
    let bucket_ms = (span_ms / MAX_PLOT_POINTS).max(1);

    let sql = if use_rollup {
        "SELECT (ts / ?4) * ?4 AS bucket, AVG(avg) FROM rollup_1m \
         WHERE server = ?1 AND metric = ?2 AND ts BETWEEN ?3 AND ?5 \
         GROUP BY bucket ORDER BY bucket"
    } else {
        "SELECT (ts / ?4) * ?4 AS bucket, AVG(value) FROM samples \
         WHERE server = ?1 AND metric = ?2 AND ts BETWEEN ?3 AND ?5 \
         GROUP BY bucket ORDER BY bucket"
    };

    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(
        params![server, metric.key(), from_ms, bucket_ms, to_ms],
        |r| {
            let ts: i64 = r.get(0)?;
            let v: f64 = r.get(1)?;
            Ok([ts as f64 / 1000.0, v])
        },
    )?;

    let mut points = Vec::new();
    for row in rows {
        points.push(row?);
    }

    // A long range can still be empty in the rollup if the app has only been
    // running minutes; fall back to raw rather than showing nothing.
    if use_rollup && points.is_empty() {
        let mut stmt = conn.prepare_cached(
            "SELECT (ts / ?4) * ?4 AS bucket, AVG(value) FROM samples \
             WHERE server = ?1 AND metric = ?2 AND ts BETWEEN ?3 AND ?5 \
             GROUP BY bucket ORDER BY bucket",
        )?;
        let rows = stmt.query_map(
            params![server, metric.key(), from_ms, bucket_ms, to_ms],
            |r| {
                let ts: i64 = r.get(0)?;
                let v: f64 = r.get(1)?;
                Ok([ts as f64 / 1000.0, v])
            },
        )?;
        for row in rows {
            points.push(row?);
        }
        return Ok((points, false));
    }

    Ok((points, use_rollup))
}

fn export_csv(
    conn: &Connection,
    path: &Path,
    server: &str,
    metrics: &[Metric],
    from_ms: i64,
    to_ms: i64,
) -> Result<usize> {
    use std::io::Write as _;

    let mut file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    writeln!(file, "timestamp_ms,server,metric,value")?;

    let mut count = 0usize;
    for metric in metrics {
        let mut stmt = conn.prepare_cached(
            "SELECT ts, value FROM samples \
             WHERE server = ?1 AND metric = ?2 AND ts BETWEEN ?3 AND ?4 ORDER BY ts",
        )?;
        let rows = stmt.query_map(params![server, metric.key(), from_ms, to_ms], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        })?;
        for row in rows {
            let (ts, v) = row?;
            writeln!(file, "{ts},{server},{},{v}", metric.key())?;
            count += 1;
        }
    }
    Ok(count)
}

/// Folds completed minutes into `rollup_1m`, then drops aged-out rows.
fn maintain(conn: &Connection) -> Result<()> {
    let now_ms = now_ms();
    let raw_cutoff = now_ms - RAW_RETENTION_S * 1000;
    let rollup_cutoff = now_ms - ROLLUP_RETENTION_S * 1000;

    conn.execute(
        "INSERT OR REPLACE INTO rollup_1m(server, metric, ts, avg, min, max) \
         SELECT server, metric, (ts / 60000) * 60000, AVG(value), MIN(value), MAX(value) \
         FROM samples WHERE ts < ?1 GROUP BY server, metric, ts / 60000",
        params![now_ms - 60_000],
    )?;
    conn.execute("DELETE FROM samples WHERE ts < ?1", params![raw_cutoff])?;
    conn.execute(
        "DELETE FROM rollup_1m WHERE ts < ?1",
        params![rollup_cutoff],
    )?;
    Ok(())
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mysql_perf_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn writes_and_reads_back_a_series() {
        let path = temp_db();
        let mut conn = open(&path).unwrap();
        let (ev_tx, ev_rx) = channel();

        let t0 = now_ms();
        for i in 0..10 {
            handle(
                &mut conn,
                StoreCmd::Write {
                    server: "s1".into(),
                    wall_ms: t0 + i * 1000,
                    values: vec![(Metric::Qps, i as f64)],
                },
                &ev_tx,
            )
            .unwrap();
        }

        handle(
            &mut conn,
            StoreCmd::Query {
                server: "s1".into(),
                metrics: vec![Metric::Qps],
                from_ms: t0 - 1000,
                to_ms: t0 + 60_000,
            },
            &ev_tx,
        )
        .unwrap();

        let series = ev_rx.recv().unwrap();
        match series {
            StoreEvent::Series {
                metric,
                points,
                downsampled,
            } => {
                assert_eq!(metric, Metric::Qps);
                assert_eq!(points.len(), 10);
                assert!(!downsampled);
                assert_eq!(points[9][1], 9.0);
            }
            other => panic!("expected a series, got {other:?}"),
        }
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rollup_folds_old_raw_rows() {
        let mut path = std::env::temp_dir();
        path.push(format!("mysql_perf_rollup_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut conn = open(&path).unwrap();
        let (ev_tx, _rx) = channel();
        let old = now_ms() - 3 * 60_000;
        for i in 0..6 {
            handle(
                &mut conn,
                StoreCmd::Write {
                    server: "s1".into(),
                    wall_ms: old + i * 1000,
                    values: vec![(Metric::Tps, 4.0)],
                },
                &ev_tx,
            )
            .unwrap();
        }

        maintain(&conn).unwrap();
        let rolled: i64 = conn
            .query_row("SELECT COUNT(*) FROM rollup_1m", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rolled, 1, "six samples in one minute fold to one row");

        let avg: f64 = conn
            .query_row("SELECT avg FROM rollup_1m", [], |r| r.get(0))
            .unwrap();
        assert_eq!(avg, 4.0);

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}
