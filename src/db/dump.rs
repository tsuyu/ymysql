//! Schema and data export — a `mysqldump`-shaped writer that runs inside the
//! app, over the connection that is already open.
//!
//! The statement builders are pure so the escaping (the part that corrupts
//! restores when it is wrong) is unit-tested without a server.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use mysql_async::prelude::*;
use mysql_async::{Conn, Row, Value};

/// Rows per multi-row `INSERT`.
const BATCH_ROWS: usize = 200;
/// Soft cap on the size of one `INSERT` statement.
const BATCH_BYTES: usize = 512 * 1024;

/// What to write out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpMode {
    /// `CREATE TABLE` only.
    Structure,
    /// `INSERT` only.
    Data,
    /// Both, structure first.
    Both,
}

impl DumpMode {
    pub const ORDER: [DumpMode; 3] = [DumpMode::Structure, DumpMode::Data, DumpMode::Both];

    pub fn label(self) -> &'static str {
        match self {
            DumpMode::Structure => "structure only",
            DumpMode::Data => "data only",
            DumpMode::Both => "structure + data",
        }
    }

    pub fn wants_structure(self) -> bool {
        matches!(self, DumpMode::Structure | DumpMode::Both)
    }

    pub fn wants_data(self) -> bool {
        matches!(self, DumpMode::Data | DumpMode::Both)
    }
}

#[derive(Debug, Clone)]
pub struct DumpSpec {
    pub schema: String,
    /// Tables to include. Empty means every table in the schema.
    pub tables: Vec<String>,
    pub mode: DumpMode,
    pub path: PathBuf,
    /// Emit `DROP TABLE IF EXISTS` before each `CREATE`.
    pub drop_tables: bool,
    /// Dump inside a consistent snapshot (InnoDB), so the file is one
    /// point-in-time view rather than a smear across the run.
    pub consistent: bool,
}

impl Default for DumpSpec {
    fn default() -> Self {
        Self {
            schema: String::new(),
            tables: Vec::new(),
            mode: DumpMode::Both,
            path: PathBuf::new(),
            drop_tables: true,
            consistent: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DumpStats {
    pub tables: usize,
    pub rows: u64,
    pub bytes: u64,
    pub path: PathBuf,
    pub cancelled: bool,
}

/// Progress callback payload.
#[derive(Debug, Clone)]
pub struct DumpProgress {
    pub table: String,
    pub table_index: usize,
    pub table_count: usize,
    pub rows: u64,
}

pub fn quote_ident(name: &str) -> Result<String> {
    if name.is_empty() || name.len() > 64 || name.contains('\0') || name.contains('\n') {
        bail!("bad identifier: {name:?}");
    }
    Ok(format!("`{}`", name.replace('`', "``")))
}

/// mysqldump-style escaping. Backslash is an escape character in MySQL string
/// literals, and NUL / ^Z break restores on some clients, so all of them go out
/// as escape sequences.
pub fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            '\x1a' => out.push_str("\\Z"),
            other => out.push(other),
        }
    }
    out
}

/// One value as a SQL literal. Non-UTF-8 bytes become a hex literal so blobs
/// survive the round trip.
pub fn value_literal(v: &Value) -> String {
    match v {
        Value::NULL => "NULL".to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => format!("'{}'", escape_string(s)),
            Err(_) => {
                let mut hex = String::with_capacity(2 + b.len() * 2);
                hex.push_str("0x");
                for byte in b {
                    hex.push_str(&format!("{byte:02X}"));
                }
                hex
            }
        },
        other => other.as_sql(true),
    }
}

/// `INSERT INTO t (cols) VALUES (...),(...);`
pub fn insert_statement(table: &str, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
    if rows.is_empty() {
        return Ok(String::new());
    }
    let cols = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Result<Vec<_>>>()?
        .join(", ");

    let mut out = format!("INSERT INTO {} ({cols}) VALUES\n", quote_ident(table)?);
    for (i, row) in rows.iter().enumerate() {
        out.push('(');
        for (j, v) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&value_literal(v));
        }
        out.push(')');
        out.push_str(if i + 1 == rows.len() { ";\n" } else { ",\n" });
    }
    Ok(out)
}

pub fn preamble(schema: &str, server_version: &str, mode: DumpMode) -> String {
    format!(
        "-- mysql_perf dump\n\
         -- database: {schema}\n\
         -- server:   {server_version}\n\
         -- contents: {}\n\
         -- created:  {}\n\n\
         SET NAMES utf8mb4;\n\
         SET FOREIGN_KEY_CHECKS = 0;\n\
         SET SQL_MODE = 'NO_AUTO_VALUE_ON_ZERO';\n\n",
        mode.label(),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    )
}

pub fn epilogue() -> String {
    "\nSET FOREIGN_KEY_CHECKS = 1;\n".to_string()
}

/// Writes the dump. `progress` is called once per table and every batch.
pub async fn run(
    conn: &mut Conn,
    spec: &DumpSpec,
    server_version: &str,
    cancel: Arc<AtomicBool>,
    mut progress: impl FnMut(DumpProgress),
) -> Result<DumpStats> {
    if spec.schema.is_empty() {
        bail!("no database selected");
    }
    let tables = if spec.tables.is_empty() {
        list_dumpable_tables(conn, &spec.schema).await?
    } else {
        spec.tables.clone()
    };
    if tables.is_empty() {
        bail!("no tables to dump in {}", spec.schema);
    }

    let file = std::fs::File::create(&spec.path)
        .with_context(|| format!("creating {}", spec.path.display()))?;
    let mut out = std::io::BufWriter::new(file);
    let mut stats = DumpStats {
        path: spec.path.clone(),
        ..Default::default()
    };

    // One connection, one snapshot: without this the file mixes rows from
    // before and after concurrent writes.
    if spec.consistent {
        conn.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")
            .await
            .context("starting consistent snapshot failed")?;
    }

    let result = write_all(
        conn,
        spec,
        server_version,
        &tables,
        &mut out,
        &cancel,
        &mut progress,
        &mut stats,
    )
    .await;

    if spec.consistent {
        let _ = conn.query_drop("COMMIT").await;
    }
    out.flush().context("flushing the dump file")?;
    result?;

    stats.bytes = std::fs::metadata(&spec.path).map(|m| m.len()).unwrap_or(0);
    stats.cancelled = cancel.load(Ordering::Relaxed);
    Ok(stats)
}

#[allow(clippy::too_many_arguments)]
async fn write_all(
    conn: &mut Conn,
    spec: &DumpSpec,
    server_version: &str,
    tables: &[String],
    out: &mut impl std::io::Write,
    cancel: &Arc<AtomicBool>,
    progress: &mut impl FnMut(DumpProgress),
    stats: &mut DumpStats,
) -> Result<()> {
    out.write_all(preamble(&spec.schema, server_version, spec.mode).as_bytes())?;

    for (i, table) in tables.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            out.write_all(b"\n-- cancelled by the user\n")?;
            return Ok(());
        }

        progress(DumpProgress {
            table: table.clone(),
            table_index: i,
            table_count: tables.len(),
            rows: 0,
        });
        out.write_all(format!("\n--\n-- Table: {table}\n--\n\n").as_bytes())?;

        if spec.mode.wants_structure() {
            if spec.drop_tables {
                out.write_all(
                    format!("DROP TABLE IF EXISTS {};\n", quote_ident(table)?).as_bytes(),
                )?;
            }
            let ddl = show_create(conn, &spec.schema, table).await?;
            out.write_all(ddl.as_bytes())?;
            out.write_all(b";\n\n")?;
        }

        if spec.mode.wants_data() {
            let rows =
                write_table_data(conn, spec, table, out, cancel, progress, i, tables.len()).await?;
            stats.rows += rows;
        }
        stats.tables += 1;
    }

    out.write_all(epilogue().as_bytes())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn write_table_data(
    conn: &mut Conn,
    spec: &DumpSpec,
    table: &str,
    out: &mut impl std::io::Write,
    cancel: &Arc<AtomicBool>,
    progress: &mut impl FnMut(DumpProgress),
    table_index: usize,
    table_count: usize,
) -> Result<u64> {
    let sql = format!(
        "SELECT * FROM {}.{}",
        quote_ident(&spec.schema)?,
        quote_ident(table)?
    );
    let mut result = conn.query_iter(sql).await.context("reading table failed")?;
    let columns: Vec<String> = result
        .columns_ref()
        .iter()
        .map(|c| c.name_str().to_string())
        .collect();

    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(BATCH_ROWS);
    let mut batch_bytes = 0usize;
    let mut total = 0u64;

    while let Some(row) = result.next().await? {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let values: Vec<Value> = row.unwrap();
        batch_bytes += values.iter().map(rough_size).sum::<usize>();
        batch.push(values);
        total += 1;

        if batch.len() >= BATCH_ROWS || batch_bytes >= BATCH_BYTES {
            out.write_all(insert_statement(table, &columns, &batch)?.as_bytes())?;
            batch.clear();
            batch_bytes = 0;
            progress(DumpProgress {
                table: table.to_string(),
                table_index,
                table_count,
                rows: total,
            });
        }
    }

    if !batch.is_empty() {
        out.write_all(insert_statement(table, &columns, &batch)?.as_bytes())?;
    }
    // The reader must be drained before the connection is used again.
    result.drop_result().await?;

    progress(DumpProgress {
        table: table.to_string(),
        table_index,
        table_count,
        rows: total,
    });
    Ok(total)
}

fn rough_size(v: &Value) -> usize {
    match v {
        Value::Bytes(b) => b.len() + 3,
        Value::NULL => 4,
        _ => 12,
    }
}

/// `SHOW CREATE TABLE` also answers for views, where the DDL is in the same
/// second column.
async fn show_create(conn: &mut Conn, schema: &str, table: &str) -> Result<String> {
    let sql = format!(
        "SHOW CREATE TABLE {}.{}",
        quote_ident(schema)?,
        quote_ident(table)?
    );
    let row: Option<Row> = conn
        .query_first(sql)
        .await
        .with_context(|| format!("SHOW CREATE TABLE for {schema}.{table} failed"))?;
    let mut row = row.with_context(|| format!("{schema}.{table} vanished mid-dump"))?;
    row.take::<Option<String>, _>(1)
        .flatten()
        .with_context(|| format!("no DDL returned for {schema}.{table}"))
}

/// Base tables and views in a schema, in name order.
pub async fn list_dumpable_tables(conn: &mut Conn, schema: &str) -> Result<Vec<String>> {
    let rows: Vec<String> = conn
        .exec(
            "SELECT TABLE_NAME FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
            (schema,),
        )
        .await
        .context("listing tables for the dump failed")?;
    Ok(rows)
}

/// Default file name for a dump, e.g. `demo_20260818_141530.sql`.
pub fn suggested_file_name(schema: &str) -> String {
    format!(
        "{}_{}.sql",
        if schema.is_empty() { "dump" } else { schema },
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    )
}

pub fn suggested_path(schema: &str) -> PathBuf {
    let dir: PathBuf = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    dir.join(suggested_file_name(schema))
}

/// True when the target looks writable (parent exists).
pub fn path_is_usable(path: &Path) -> bool {
    match path.parent() {
        Some(p) if p.as_os_str().is_empty() => true,
        Some(p) => p.is_dir(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(s: &str) -> Value {
        Value::Bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn strings_are_escaped_for_restore() {
        assert_eq!(value_literal(&bytes("plain")), "'plain'");
        assert_eq!(value_literal(&bytes("O'Brien")), "'O\\'Brien'");
        assert_eq!(value_literal(&bytes(r"c:\tmp")), r"'c:\\tmp'");
        assert_eq!(value_literal(&bytes("two\nlines")), "'two\\nlines'");
        assert_eq!(value_literal(&Value::NULL), "NULL");
        assert_eq!(value_literal(&Value::Int(-7)), "-7");
        assert_eq!(value_literal(&Value::UInt(7)), "7");
    }

    #[test]
    fn injection_in_data_cannot_break_out_of_the_literal() {
        let nasty = bytes("'); DROP TABLE users; -- ");
        let lit = value_literal(&nasty);
        assert!(lit.starts_with('\'') && lit.ends_with('\''));
        assert_eq!(lit, "'\\'); DROP TABLE users; -- '");
    }

    #[test]
    fn binary_columns_become_hex() {
        let blob = Value::Bytes(vec![0x00, 0xFF, 0x10, 0x80]);
        assert_eq!(value_literal(&blob), "0x00FF1080");
    }

    #[test]
    fn insert_statement_batches_rows() {
        let cols = vec!["id".to_string(), "name".to_string()];
        let rows = vec![
            vec![Value::Int(1), bytes("a")],
            vec![Value::Int(2), Value::NULL],
        ];
        let sql = insert_statement("users", &cols, &rows).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO `users` (`id`, `name`) VALUES\n(1,'a'),\n(2,NULL);\n"
        );
    }

    #[test]
    fn identifiers_are_quoted_in_inserts() {
        let cols = vec!["we`ird".to_string()];
        let rows = vec![vec![Value::Int(1)]];
        let sql = insert_statement("ta`ble", &cols, &rows).unwrap();
        assert!(sql.starts_with("INSERT INTO `ta``ble` (`we``ird`) VALUES"));
    }

    #[test]
    fn empty_batch_writes_nothing() {
        assert_eq!(insert_statement("t", &["a".into()], &[]).unwrap(), "");
    }

    #[test]
    fn mode_controls_the_sections() {
        assert!(DumpMode::Structure.wants_structure() && !DumpMode::Structure.wants_data());
        assert!(!DumpMode::Data.wants_structure() && DumpMode::Data.wants_data());
        assert!(DumpMode::Both.wants_structure() && DumpMode::Both.wants_data());
    }

    #[test]
    fn preamble_names_the_database_and_contents() {
        let p = preamble("demo", "MySQL 8.0.36", DumpMode::Data);
        assert!(p.contains("-- database: demo"));
        assert!(p.contains("MySQL 8.0.36"));
        assert!(p.contains("data only"));
        assert!(p.contains("SET FOREIGN_KEY_CHECKS = 0;"));
        assert!(epilogue().contains("SET FOREIGN_KEY_CHECKS = 1;"));
    }

    #[test]
    fn suggested_name_uses_the_schema() {
        let n = suggested_file_name("demo");
        assert!(n.starts_with("demo_") && n.ends_with(".sql"));
        assert!(suggested_file_name("").starts_with("dump_"));
    }
}
