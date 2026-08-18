//! Version-aware data fetches. All MySQL 5 vs 8 SQL divergence lives here and
//! is selected by `Capabilities`, never by ad-hoc version checks.

use anyhow::{Context, Result, bail};
use mysql_async::prelude::*;
use mysql_async::{Conn, Row, Value};
use std::collections::HashMap;

use super::version::{Capabilities, ServerVersion};
use crate::model::ProcessRow;

/// Schemas the user never wants advice about.
const SYSTEM_SCHEMAS: &str = "'mysql','performance_schema','information_schema','sys'";

/// Row of `events_statements_summary_by_digest`.
#[derive(Debug, Clone, Default)]
pub struct DigestRow {
    pub digest: String,
    pub schema: String,
    pub text: String,
    pub count: u64,
    pub total_ms: f64,
    pub avg_ms: f64,
    pub max_ms: f64,
    pub lock_ms: f64,
    pub rows_examined: u64,
    pub rows_sent: u64,
    pub tmp_disk_tables: u64,
    pub no_index_used: u64,
    pub errors: u64,
}

impl DigestRow {
    /// Rows examined per row returned — the classic "is this query reading the
    /// whole table" ratio.
    pub fn examined_per_sent(&self) -> f64 {
        if self.rows_sent == 0 {
            self.rows_examined as f64
        } else {
            self.rows_examined as f64 / self.rows_sent as f64
        }
    }
}

/// One execution from `events_statements_history_long`.
#[derive(Debug, Clone, Default)]
pub struct StatementSample {
    pub sql: String,
    pub ms: f64,
    pub lock_ms: f64,
    pub rows_examined: u64,
    pub rows_sent: u64,
    pub no_index_used: bool,
    pub no_good_index_used: bool,
    pub tmp_disk_tables: u64,
    pub errors: u64,
}

/// One side of a lock wait: who it is and what it is running.
#[derive(Debug, Clone, Default)]
pub struct LockParty {
    pub trx_id: String,
    /// Connection id — the number `KILL` takes.
    pub thread_id: u64,
    pub user: String,
    pub host: String,
    pub db: String,
    pub query: String,
    /// Seconds this side has been waiting (requester) or open (blocker).
    pub secs: i64,
}

impl LockParty {
    /// `user@host`, or just the user when the host is unknown.
    pub fn who(&self) -> String {
        match (self.user.is_empty(), self.host.is_empty()) {
            (true, _) => "?".to_string(),
            (false, true) => self.user.clone(),
            (false, false) => format!("{}@{}", self.user, self.host),
        }
    }
}

/// A blocked transaction, whoever blocks it, and the lock they are fighting
/// over.
#[derive(Debug, Clone, Default)]
pub struct LockWait {
    pub waiting: LockParty,
    pub blocking: LockParty,
    /// `schema.table` the lock sits on.
    pub table: String,
    /// Index the row lock is taken on, when the server reports one.
    pub index: String,
    /// `X`, `S`, `X,GAP`, …
    pub lock_mode: String,
    /// `RECORD`, `TABLE`.
    pub lock_type: String,
}

/// Row of `information_schema.innodb_trx`.
#[derive(Debug, Clone, Default)]
pub struct TrxRow {
    pub id: String,
    pub state: String,
    pub started: String,
    pub wait_secs: i64,
    pub thread_id: u64,
    pub rows_locked: u64,
    pub rows_modified: u64,
    pub isolation: String,
    pub query: String,
}

/// Row of `performance_schema.metadata_locks` (5.7.3+).
#[derive(Debug, Clone, Default)]
pub struct MdlRow {
    pub object_type: String,
    pub schema: String,
    pub name: String,
    pub lock_type: String,
    pub lock_status: String,
    pub thread_id: u64,
}

/// Index usage counters, from `table_io_waits_summary_by_index_usage`.
#[derive(Debug, Clone, Default)]
pub struct IndexUsage {
    pub schema: String,
    pub table: String,
    pub index: String,
    pub reads: u64,
    pub writes: u64,
}

/// Tables being scanned without an index.
#[derive(Debug, Clone, Default)]
pub struct ScanRow {
    pub schema: String,
    pub table: String,
    pub rows_full_scanned: u64,
    pub table_rows: u64,
}

/// An index definition, flattened to its ordered column list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexDef {
    pub schema: String,
    pub table: String,
    pub index: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

impl IndexDef {
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoPkTable {
    pub schema: String,
    pub table: String,
    pub table_rows: u64,
    pub engine: String,
}

/// Where a result column came from, straight out of the protocol's column
/// metadata. Empty for expressions, literals and aggregates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnOrigin {
    pub schema: String,
    pub table: String,
    /// Column name before any `AS` alias.
    pub column: String,
}

impl ColumnOrigin {
    /// True when this column can be traced back to a real table column.
    pub fn is_table_column(&self) -> bool {
        !self.schema.is_empty() && !self.table.is_empty() && !self.column.is_empty()
    }
}

/// A generic result grid, for EXPLAIN and other ad-hoc output.
#[derive(Debug, Clone, Default)]
pub struct Grid {
    pub columns: Vec<String>,
    /// `None` is a real SQL NULL, not the text "NULL".
    pub rows: Vec<Vec<Option<String>>>,
    /// Source of each column, parallel to `columns`. May be empty when the
    /// producer did not record it.
    pub origins: Vec<ColumnOrigin>,
}

impl Grid {
    pub fn origin(&self, column: usize) -> Option<&ColumnOrigin> {
        self.origins.get(column).filter(|o| o.is_table_column())
    }
}

fn take_str(row: &mut Row, i: usize) -> String {
    row.take::<Option<String>, _>(i)
        .flatten()
        .unwrap_or_default()
}

fn take_u64(row: &mut Row, i: usize) -> u64 {
    row.take::<Option<u64>, _>(i).flatten().unwrap_or(0)
}

fn take_i64(row: &mut Row, i: usize) -> i64 {
    row.take::<Option<i64>, _>(i).flatten().unwrap_or(0)
}

fn take_f64(row: &mut Row, i: usize) -> f64 {
    row.take::<Option<f64>, _>(i).flatten().unwrap_or(0.0)
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::NULL => String::new(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        other => other.as_sql(false),
    }
}

/// Rejects anything that is not a bare identifier, so schema names coming from
/// the server can be interpolated into `USE`.
fn is_safe_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// `VERSION()` + `@@version_comment`, used to build the capability matrix.
pub async fn server_version(conn: &mut Conn) -> Result<ServerVersion> {
    let row: Option<(String, Option<String>)> = conn
        .query_first("SELECT VERSION(), @@version_comment")
        .await
        .context("SELECT VERSION() failed")?;
    let (v, c) = row.context("VERSION() returned no rows")?;
    Ok(ServerVersion::parse(&v, c.as_deref().unwrap_or("")))
}

/// Confirms performance_schema is actually enabled (it can be compiled in but
/// off, in which case all `performance_schema.*` reads return empty).
pub async fn perf_schema_enabled(conn: &mut Conn) -> bool {
    conn.query_first::<i64, _>("SELECT @@performance_schema")
        .await
        .ok()
        .flatten()
        .map(|v| v == 1)
        .unwrap_or(false)
}

/// `SHOW ENGINE INNODB STATUS` — the report text from the third column.
pub async fn engine_innodb_status(conn: &mut Conn) -> Result<String> {
    let row: Option<Row> = conn
        .query_first("SHOW ENGINE INNODB STATUS")
        .await
        .context("SHOW ENGINE INNODB STATUS failed")?;
    let mut row = row.context("SHOW ENGINE INNODB STATUS returned no rows")?;
    // Columns are (Type, Name, Status); the report is the last one.
    Ok(row
        .take::<Option<String>, _>(2)
        .flatten()
        .unwrap_or_default())
}

/// Server limits that shape the connection view. Read once per connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerLimits {
    pub max_connections: u64,
    /// Seconds an idle non-interactive session is kept before the server closes
    /// it — the ceiling on how long a sleeper can hold a slot.
    pub wait_timeout: u64,
    pub interactive_timeout: u64,
}

pub async fn server_limits(conn: &mut Conn) -> Result<ServerLimits> {
    let row: Option<(u64, u64, u64)> = conn
        .query_first("SELECT @@max_connections, @@wait_timeout, @@interactive_timeout")
        .await
        .context("reading connection limits failed")?;
    let (max_connections, wait_timeout, interactive_timeout) = row.unwrap_or((0, 0, 0));
    Ok(ServerLimits {
        max_connections,
        wait_timeout,
        interactive_timeout,
    })
}

/// `SHOW GLOBAL STATUS` -> (numeric subset, everything as strings).
pub async fn global_status(
    conn: &mut Conn,
) -> Result<(HashMap<String, u64>, HashMap<String, String>)> {
    let rows: Vec<(String, String)> = conn
        .query("SHOW GLOBAL STATUS")
        .await
        .context("SHOW GLOBAL STATUS failed")?;

    let mut nums = HashMap::with_capacity(rows.len());
    let mut raw = HashMap::with_capacity(rows.len());
    for (k, v) in rows {
        if let Ok(n) = v.parse::<u64>() {
            nums.insert(k.clone(), n);
        }
        raw.insert(k, v);
    }
    Ok((nums, raw))
}

#[allow(dead_code)] // for the Variables tab
pub async fn global_variables(conn: &mut Conn) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> = conn
        .query("SHOW GLOBAL VARIABLES")
        .await
        .context("SHOW GLOBAL VARIABLES failed")?;
    Ok(rows.into_iter().collect())
}

/// MySQL 8.0.22+ exposes `performance_schema.processlist` (cheaper, no global
/// mutex). Everything older must read `information_schema.PROCESSLIST`.
pub async fn processlist(conn: &mut Conn, caps: &Capabilities) -> Result<Vec<ProcessRow>> {
    let sql = if caps.ps_processlist && caps.perf_schema_on {
        "SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE, INFO \
         FROM performance_schema.processlist WHERE ID != CONNECTION_ID()"
    } else {
        "SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE, INFO \
         FROM information_schema.PROCESSLIST WHERE ID != CONNECTION_ID()"
    };

    let rows: Vec<Row> = conn.query(sql).await.context("processlist query failed")?;
    Ok(rows
        .into_iter()
        .map(|mut r| ProcessRow {
            id: take_u64(&mut r, 0),
            user: take_str(&mut r, 1),
            host: take_str(&mut r, 2),
            db: take_str(&mut r, 3),
            command: take_str(&mut r, 4),
            time: take_i64(&mut r, 5),
            state: take_str(&mut r, 6),
            info: take_str(&mut r, 7),
        })
        .collect())
}

fn digest_select(caps: &Capabilities) -> String {
    // QUERY_SAMPLE_TEXT is 8.0-only; 5.x has just the normalised digest text.
    let text_col = if caps.digest_sample_text {
        "COALESCE(QUERY_SAMPLE_TEXT, DIGEST_TEXT)"
    } else {
        "DIGEST_TEXT"
    };
    format!(
        "SELECT COALESCE(DIGEST,''), COALESCE(SCHEMA_NAME,''), {text_col}, \
                COUNT_STAR, SUM_TIMER_WAIT/1e9, AVG_TIMER_WAIT/1e9, MAX_TIMER_WAIT/1e9, \
                SUM_LOCK_TIME/1e9, SUM_ROWS_EXAMINED, SUM_ROWS_SENT, \
                SUM_CREATED_TMP_DISK_TABLES, SUM_NO_INDEX_USED, SUM_ERRORS \
         FROM performance_schema.events_statements_summary_by_digest"
    )
}

fn digest_row(mut r: Row) -> DigestRow {
    DigestRow {
        digest: take_str(&mut r, 0),
        schema: take_str(&mut r, 1),
        text: take_str(&mut r, 2),
        count: take_u64(&mut r, 3),
        total_ms: take_f64(&mut r, 4),
        avg_ms: take_f64(&mut r, 5),
        max_ms: take_f64(&mut r, 6),
        lock_ms: take_f64(&mut r, 7),
        rows_examined: take_u64(&mut r, 8),
        rows_sent: take_u64(&mut r, 9),
        tmp_disk_tables: take_u64(&mut r, 10),
        no_index_used: take_u64(&mut r, 11),
        errors: take_u64(&mut r, 12),
    }
}

/// Statement digests, worst total time first. Timers are picoseconds on both
/// major versions, hence the /1e9 to milliseconds.
pub async fn top_queries(
    conn: &mut Conn,
    caps: &Capabilities,
    limit: u32,
) -> Result<Vec<DigestRow>> {
    if !caps.statement_digest || !caps.perf_schema_on {
        return Ok(Vec::new());
    }
    let sql = format!(
        "{} WHERE DIGEST_TEXT IS NOT NULL ORDER BY SUM_TIMER_WAIT DESC LIMIT {limit}",
        digest_select(caps)
    );
    let rows: Vec<Row> = conn.query(sql).await.context("digest query failed")?;
    Ok(rows.into_iter().map(digest_row).collect())
}

/// One digest, for the inspector.
pub async fn digest_detail(
    conn: &mut Conn,
    caps: &Capabilities,
    digest: &str,
) -> Result<Option<DigestRow>> {
    if !caps.statement_digest || !caps.perf_schema_on {
        return Ok(None);
    }
    let sql = format!("{} WHERE DIGEST = ? LIMIT 1", digest_select(caps));
    let row: Option<Row> = conn
        .exec_first(sql, (digest,))
        .await
        .context("digest detail failed")?;
    Ok(row.map(digest_row))
}

/// Recent executions of one digest. Requires the
/// `events_statements_history_long` consumer, which is off by default on 5.7
/// and on by default on 8.0 — an empty result is the normal "consumer off" case.
pub async fn statement_samples(
    conn: &mut Conn,
    caps: &Capabilities,
    digest: &str,
    limit: u32,
) -> Result<Vec<StatementSample>> {
    if !caps.history_long || !caps.perf_schema_on {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT COALESCE(SQL_TEXT,''), TIMER_WAIT/1e9, LOCK_TIME/1e9, \
                ROWS_EXAMINED, ROWS_SENT, NO_INDEX_USED, NO_GOOD_INDEX_USED, \
                CREATED_TMP_DISK_TABLES, ERRORS \
         FROM performance_schema.events_statements_history_long \
         WHERE DIGEST = ? ORDER BY TIMER_START DESC LIMIT {limit}"
    );
    let rows: Vec<Row> = conn
        .exec(sql, (digest,))
        .await
        .context("statement history failed")?;
    Ok(rows
        .into_iter()
        .map(|mut r| StatementSample {
            sql: take_str(&mut r, 0),
            ms: take_f64(&mut r, 1),
            lock_ms: take_f64(&mut r, 2),
            rows_examined: take_u64(&mut r, 3),
            rows_sent: take_u64(&mut r, 4),
            no_index_used: take_u64(&mut r, 5) != 0,
            no_good_index_used: take_u64(&mut r, 6) != 0,
            tmp_disk_tables: take_u64(&mut r, 7),
            errors: take_u64(&mut r, 8),
        })
        .collect())
}

/// Runs `EXPLAIN` against a sample statement.
///
/// Read-only guard: only `SELECT` is accepted. `EXPLAIN UPDATE/DELETE` is legal
/// on 5.6+ and does not execute, but a mis-parsed statement would be
/// destructive, so the inspector refuses anything else.
pub async fn explain(conn: &mut Conn, schema: &str, sql: &str) -> Result<Grid> {
    let stmt = sql.trim().trim_end_matches(';').trim();
    let head = stmt.trim_start_matches('(').trim_start();
    if head.len() < 6 || !head[..6].eq_ignore_ascii_case("select") {
        bail!("EXPLAIN is limited to SELECT statements");
    }
    if stmt.contains(';') {
        bail!("refusing to EXPLAIN a multi-statement string");
    }

    if !schema.is_empty() {
        if !is_safe_ident(schema) {
            bail!("unsafe schema name: {schema}");
        }
        conn.query_drop(format!("USE `{schema}`"))
            .await
            .with_context(|| format!("USE `{schema}` failed"))?;
    }

    let rows: Vec<Row> = conn
        .query(format!("EXPLAIN {stmt}"))
        .await
        .context("EXPLAIN failed")?;
    Ok(to_grid(rows))
}

fn to_grid(rows: Vec<Row>) -> Grid {
    let columns = rows
        .first()
        .map(|r| {
            r.columns_ref()
                .iter()
                .map(|c| c.name_str().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let rows = rows.into_iter().map(row_cells).collect();
    Grid {
        columns,
        rows,
        origins: Vec::new(),
    }
}

fn row_cells(row: Row) -> Vec<Option<String>> {
    row.unwrap()
        .iter()
        .map(|v| match v {
            Value::NULL => None,
            other => Some(value_to_string(other)),
        })
        .collect()
}

/// Blocked/blocking transactions, with the session behind each side and the
/// object being locked.
///
/// MySQL 8.0 REMOVED `information_schema.INNODB_LOCK_WAITS` and
/// `INNODB_LOCKS`; the replacements are `performance_schema.data_lock_waits`
/// and `data_locks`, with different column names. MySQL 5.x has only the
/// information_schema pair. Both branches return the same shape.
pub async fn lock_waits(conn: &mut Conn, caps: &Capabilities) -> Result<Vec<LockWait>> {
    // The session columns (user/host/db) come from the process list, which is
    // the only place that knows who owns a transaction.
    let processlist = if caps.ps_processlist && caps.perf_schema_on {
        "performance_schema.processlist"
    } else {
        "information_schema.PROCESSLIST"
    };

    let sql = if caps.ps_data_locks {
        format!(
            "SELECT rt.trx_id, COALESCE(rt.trx_mysql_thread_id, 0), \
                    COALESCE(rp.USER,''), COALESCE(rp.HOST,''), COALESCE(rp.DB,''), \
                    COALESCE(rt.trx_query,''), \
                    COALESCE(TIMESTAMPDIFF(SECOND, rt.trx_wait_started, NOW()), 0), \
                    bt.trx_id, COALESCE(bt.trx_mysql_thread_id, 0), \
                    COALESCE(bp.USER,''), COALESCE(bp.HOST,''), COALESCE(bp.DB,''), \
                    COALESCE(bt.trx_query,''), \
                    COALESCE(TIMESTAMPDIFF(SECOND, bt.trx_started, NOW()), 0), \
                    COALESCE(CONCAT(rl.OBJECT_SCHEMA, '.', rl.OBJECT_NAME), ''), \
                    COALESCE(rl.INDEX_NAME,''), COALESCE(rl.LOCK_MODE,''), \
                    COALESCE(rl.LOCK_TYPE,'') \
             FROM performance_schema.data_lock_waits w \
             JOIN information_schema.innodb_trx rt \
               ON rt.trx_id = w.REQUESTING_ENGINE_TRANSACTION_ID \
             JOIN information_schema.innodb_trx bt \
               ON bt.trx_id = w.BLOCKING_ENGINE_TRANSACTION_ID \
             LEFT JOIN performance_schema.data_locks rl \
               ON rl.ENGINE_LOCK_ID = w.REQUESTING_ENGINE_LOCK_ID \
             LEFT JOIN {processlist} rp ON rp.ID = rt.trx_mysql_thread_id \
             LEFT JOIN {processlist} bp ON bp.ID = bt.trx_mysql_thread_id"
        )
    } else {
        format!(
            "SELECT rt.trx_id, COALESCE(rt.trx_mysql_thread_id, 0), \
                    COALESCE(rp.USER,''), COALESCE(rp.HOST,''), COALESCE(rp.DB,''), \
                    COALESCE(rt.trx_query,''), \
                    COALESCE(TIMESTAMPDIFF(SECOND, rt.trx_wait_started, NOW()), 0), \
                    bt.trx_id, COALESCE(bt.trx_mysql_thread_id, 0), \
                    COALESCE(bp.USER,''), COALESCE(bp.HOST,''), COALESCE(bp.DB,''), \
                    COALESCE(bt.trx_query,''), \
                    COALESCE(TIMESTAMPDIFF(SECOND, bt.trx_started, NOW()), 0), \
                    COALESCE(rl.lock_table,''), COALESCE(rl.lock_index,''), \
                    COALESCE(rl.lock_mode,''), COALESCE(rl.lock_type,'') \
             FROM information_schema.innodb_lock_waits w \
             JOIN information_schema.innodb_trx rt ON rt.trx_id = w.requesting_trx_id \
             JOIN information_schema.innodb_trx bt ON bt.trx_id = w.blocking_trx_id \
             LEFT JOIN information_schema.innodb_locks rl \
               ON rl.lock_id = w.requested_lock_id \
             LEFT JOIN {processlist} rp ON rp.ID = rt.trx_mysql_thread_id \
             LEFT JOIN {processlist} bp ON bp.ID = bt.trx_mysql_thread_id"
        )
    };

    let rows: Vec<Row> = conn.query(sql).await.context("lock wait query failed")?;
    Ok(rows
        .into_iter()
        .map(|mut r| LockWait {
            waiting: LockParty {
                trx_id: take_str(&mut r, 0),
                thread_id: take_u64(&mut r, 1),
                user: take_str(&mut r, 2),
                host: take_str(&mut r, 3),
                db: take_str(&mut r, 4),
                query: take_str(&mut r, 5),
                secs: take_i64(&mut r, 6),
            },
            blocking: LockParty {
                trx_id: take_str(&mut r, 7),
                thread_id: take_u64(&mut r, 8),
                user: take_str(&mut r, 9),
                host: take_str(&mut r, 10),
                db: take_str(&mut r, 11),
                query: take_str(&mut r, 12),
                secs: take_i64(&mut r, 13),
            },
            // 5.7 reports `db`.`tbl` with backticks; strip them so both
            // versions read the same.
            table: take_str(&mut r, 14).replace('`', ""),
            index: take_str(&mut r, 15),
            lock_mode: take_str(&mut r, 16),
            lock_type: take_str(&mut r, 17),
        })
        .collect())
}

/// Open InnoDB transactions, longest-running first.
pub async fn transactions(conn: &mut Conn) -> Result<Vec<TrxRow>> {
    let sql = "SELECT trx_id, trx_state, COALESCE(CAST(trx_started AS CHAR),''), \
                      COALESCE(TIMESTAMPDIFF(SECOND, trx_wait_started, NOW()), 0), \
                      COALESCE(trx_mysql_thread_id, 0), trx_rows_locked, trx_rows_modified, \
                      COALESCE(trx_isolation_level,''), COALESCE(trx_query,'') \
               FROM information_schema.innodb_trx ORDER BY trx_started";
    let rows: Vec<Row> = conn.query(sql).await.context("innodb_trx failed")?;
    Ok(rows
        .into_iter()
        .map(|mut r| TrxRow {
            id: take_str(&mut r, 0),
            state: take_str(&mut r, 1),
            started: take_str(&mut r, 2),
            wait_secs: take_i64(&mut r, 3),
            thread_id: take_u64(&mut r, 4),
            rows_locked: take_u64(&mut r, 5),
            rows_modified: take_u64(&mut r, 6),
            isolation: take_str(&mut r, 7),
            query: take_str(&mut r, 8),
        })
        .collect())
}

/// Metadata locks — the DDL blockers that never show up in innodb_trx.
pub async fn metadata_locks(conn: &mut Conn, caps: &Capabilities) -> Result<Vec<MdlRow>> {
    if !caps.metadata_locks || !caps.perf_schema_on {
        return Ok(Vec::new());
    }
    let sql = "SELECT OBJECT_TYPE, COALESCE(OBJECT_SCHEMA,''), COALESCE(OBJECT_NAME,''), \
                      LOCK_TYPE, LOCK_STATUS, COALESCE(OWNER_THREAD_ID,0) \
               FROM performance_schema.metadata_locks \
               WHERE OBJECT_SCHEMA IS NULL OR OBJECT_SCHEMA NOT IN ('performance_schema')";
    let rows: Vec<Row> = conn.query(sql).await.context("metadata_locks failed")?;
    Ok(rows
        .into_iter()
        .map(|mut r| MdlRow {
            object_type: take_str(&mut r, 0),
            schema: take_str(&mut r, 1),
            name: take_str(&mut r, 2),
            lock_type: take_str(&mut r, 3),
            lock_status: take_str(&mut r, 4),
            thread_id: take_u64(&mut r, 5),
        })
        .collect())
}

/// Per-index read/write counters. `index_name IS NULL` rows are table scans and
/// are excluded here; `full_table_scans` reads those.
pub async fn index_usage(conn: &mut Conn, caps: &Capabilities) -> Result<Vec<IndexUsage>> {
    if !caps.index_usage || !caps.perf_schema_on {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT OBJECT_SCHEMA, OBJECT_NAME, INDEX_NAME, COUNT_READ, COUNT_WRITE \
         FROM performance_schema.table_io_waits_summary_by_index_usage \
         WHERE INDEX_NAME IS NOT NULL AND OBJECT_SCHEMA NOT IN ({SYSTEM_SCHEMAS})"
    );
    let rows: Vec<Row> = conn.query(sql).await.context("index usage failed")?;
    Ok(rows
        .into_iter()
        .map(|mut r| IndexUsage {
            schema: take_str(&mut r, 0),
            table: take_str(&mut r, 1),
            index: take_str(&mut r, 2),
            reads: take_u64(&mut r, 3),
            writes: take_u64(&mut r, 4),
        })
        .collect())
}

/// Tables read without any index, worst first.
pub async fn full_table_scans(conn: &mut Conn, caps: &Capabilities) -> Result<Vec<ScanRow>> {
    if !caps.index_usage || !caps.perf_schema_on {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT i.OBJECT_SCHEMA, i.OBJECT_NAME, i.COUNT_READ, COALESCE(t.TABLE_ROWS, 0) \
         FROM performance_schema.table_io_waits_summary_by_index_usage i \
         LEFT JOIN information_schema.TABLES t \
           ON t.TABLE_SCHEMA = i.OBJECT_SCHEMA AND t.TABLE_NAME = i.OBJECT_NAME \
         WHERE i.INDEX_NAME IS NULL AND i.COUNT_READ > 0 \
           AND i.OBJECT_SCHEMA NOT IN ({SYSTEM_SCHEMAS}) \
         ORDER BY i.COUNT_READ DESC LIMIT 100"
    );
    let rows: Vec<Row> = conn.query(sql).await.context("full table scans failed")?;
    Ok(rows
        .into_iter()
        .map(|mut r| ScanRow {
            schema: take_str(&mut r, 0),
            table: take_str(&mut r, 1),
            rows_full_scanned: take_u64(&mut r, 2),
            table_rows: take_u64(&mut r, 3),
        })
        .collect())
}

/// Every index, flattened to an ordered column list. Feeds the redundancy
/// analysis, which is done in Rust so it works identically on 5.x and 8.x.
pub async fn index_definitions(conn: &mut Conn) -> Result<Vec<IndexDef>> {
    let sql = format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, INDEX_NAME, \
                GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX SEPARATOR ','), \
                MIN(NON_UNIQUE) \
         FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA NOT IN ({SYSTEM_SCHEMAS}) \
         GROUP BY TABLE_SCHEMA, TABLE_NAME, INDEX_NAME"
    );
    let rows: Vec<Row> = conn.query(sql).await.context("index definitions failed")?;
    Ok(rows
        .into_iter()
        .map(|mut r| {
            let cols = take_str(&mut r, 3);
            IndexDef {
                schema: take_str(&mut r, 0),
                table: take_str(&mut r, 1),
                index: take_str(&mut r, 2),
                columns: cols.split(',').map(|c| c.to_string()).collect(),
                unique: take_u64(&mut r, 4) == 0,
            }
        })
        .collect())
}

/// Base tables with no primary key. On InnoDB these get a hidden 6-byte row id,
/// which breaks row-based replication performance and online schema tools.
pub async fn tables_without_pk(conn: &mut Conn) -> Result<Vec<NoPkTable>> {
    let sql = format!(
        "SELECT t.TABLE_SCHEMA, t.TABLE_NAME, COALESCE(t.TABLE_ROWS,0), COALESCE(t.ENGINE,'') \
         FROM information_schema.TABLES t \
         LEFT JOIN information_schema.TABLE_CONSTRAINTS c \
           ON c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME \
          AND c.CONSTRAINT_TYPE = 'PRIMARY KEY' \
         WHERE t.TABLE_TYPE = 'BASE TABLE' AND c.CONSTRAINT_NAME IS NULL \
           AND t.TABLE_SCHEMA NOT IN ({SYSTEM_SCHEMAS})"
    );
    let rows: Vec<Row> = conn.query(sql).await.context("tables without pk failed")?;
    Ok(rows
        .into_iter()
        .map(|mut r| NoPkTable {
            schema: take_str(&mut r, 0),
            table: take_str(&mut r, 1),
            table_rows: take_u64(&mut r, 2),
            engine: take_str(&mut r, 3),
        })
        .collect())
}

/// Terminology changed in 8.0.22; the old form still works there but is
/// deprecated, and the new form does not exist on 5.x or MariaDB.
#[allow(dead_code)] // for the Replication tab
pub fn replica_status_sql(caps: &Capabilities) -> &'static str {
    if caps.replica_terms {
        "SHOW REPLICA STATUS"
    } else {
        "SHOW SLAVE STATUS"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_party_names_the_session() {
        let mut p = LockParty {
            user: "app".into(),
            host: "10.0.0.7:51234".into(),
            ..Default::default()
        };
        assert_eq!(p.who(), "app@10.0.0.7:51234");
        p.host.clear();
        assert_eq!(p.who(), "app");
        p.user.clear();
        assert_eq!(p.who(), "?", "an unknown session must not render blank");
    }

    #[test]
    fn identifier_guard() {
        assert!(is_safe_ident("demo"));
        assert!(is_safe_ident("my_db$1"));
        assert!(!is_safe_ident("demo`; DROP"));
        assert!(!is_safe_ident(""));
    }
}
