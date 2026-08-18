//! Ad-hoc SQL: the console, the table browser, and single-row edits.
//!
//! Everything that can modify the monitored server lives here, behind two
//! guards: statements are classified before they run, and every row edit is
//! built from quoted identifiers plus bound parameters keyed on the primary key.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use mysql_async::prelude::*;
use mysql_async::{Conn, Row, Value};

use super::queries::{ColumnOrigin, Grid};

/// Rows a console query or a browser page will pull at most.
pub const DEFAULT_ROW_LIMIT: usize = 500;

const SYSTEM_SCHEMAS: [&str; 4] = ["mysql", "performance_schema", "information_schema", "sys"];

/// What a statement will do to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    /// SELECT/SHOW/EXPLAIN/DESCRIBE — never modifies data.
    Read,
    /// INSERT/UPDATE/DELETE/REPLACE and friends.
    Write,
    /// CREATE/ALTER/DROP/TRUNCATE/RENAME.
    Ddl,
    /// Anything unrecognised. Treated as a write.
    Unknown,
}

impl StatementKind {
    pub fn is_read_only(self) -> bool {
        self == StatementKind::Read
    }

    pub fn label(self) -> &'static str {
        match self {
            StatementKind::Read => "read",
            StatementKind::Write => "write",
            StatementKind::Ddl => "DDL",
            StatementKind::Unknown => "unknown",
        }
    }
}

/// Classifies by leading keyword, skipping comments and parentheses.
pub fn classify(sql: &str) -> StatementKind {
    let mut s = sql.trim();

    // Strip leading comments: -- line, # line, /* block */.
    loop {
        if let Some(rest) = s.strip_prefix("--").or_else(|| s.strip_prefix('#')) {
            s = rest.find('\n').map(|i| &rest[i + 1..]).unwrap_or("").trim();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.find("*/").map(|i| &rest[i + 2..]).unwrap_or("").trim();
        } else {
            break;
        }
    }
    let s = s.trim_start_matches('(').trim_start();

    let word: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();

    match word.as_str() {
        "select" | "show" | "explain" | "describe" | "desc" | "with" | "analyze" | "table"
        | "values" => StatementKind::Read,
        "insert" | "update" | "delete" | "replace" | "load" | "call" | "do" | "set" | "begin"
        | "start" | "commit" | "rollback" | "flush" | "kill" | "grant" | "revoke" => {
            StatementKind::Write
        }
        "create" | "alter" | "drop" | "truncate" | "rename" => StatementKind::Ddl,
        _ => StatementKind::Unknown,
    }
}

/// Result of one console statement.
#[derive(Debug, Clone, Default)]
pub struct SqlOutcome {
    pub grid: Grid,
    /// True when the row limit cut the result short.
    pub truncated: bool,
    pub affected: u64,
    pub last_insert_id: Option<u64>,
    pub info: String,
    pub elapsed_ms: f64,
}

/// Runs one statement, capping how many rows are pulled into memory.
pub async fn run(conn: &mut Conn, sql: &str, limit: usize) -> Result<SqlOutcome> {
    let stmt = sql.trim().trim_end_matches(';').trim();
    if stmt.is_empty() {
        bail!("nothing to run");
    }

    let started = Instant::now();
    let mut result = conn.query_iter(stmt).await.context("query failed")?;

    let columns: Vec<String> = result
        .columns_ref()
        .iter()
        .map(|c| c.name_str().to_string())
        .collect();
    // The protocol tells us which table each column really came from, which is
    // what lets a console result jump into the table browser.
    let origins: Vec<ColumnOrigin> = result
        .columns_ref()
        .iter()
        .map(|c| ColumnOrigin {
            schema: c.schema_str().to_string(),
            table: c.org_table_str().to_string(),
            column: c.org_name_str().to_string(),
        })
        .collect();

    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut truncated = false;
    while let Some(row) = result.next().await? {
        if rows.len() >= limit {
            truncated = true;
            break;
        }
        rows.push(cells(row));
    }

    let affected = result.affected_rows();
    let last_insert_id = result.last_insert_id();
    let info = result.info().to_string();
    // Anything left unread must be drained before the connection is reused.
    result.drop_result().await?;

    Ok(SqlOutcome {
        grid: Grid {
            columns,
            rows,
            origins,
        },
        truncated,
        affected,
        last_insert_id,
        info,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

fn cells(row: Row) -> Vec<Option<String>> {
    row.unwrap()
        .iter()
        .map(|v| match v {
            Value::NULL => None,
            Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Value::Int(i) => Some(i.to_string()),
            Value::UInt(u) => Some(u.to_string()),
            Value::Float(f) => Some(f.to_string()),
            Value::Double(d) => Some(d.to_string()),
            other => Some(other.as_sql(false)),
        })
        .collect()
}

/// Backtick-quotes an identifier, doubling any embedded backtick. Rejects the
/// characters that cannot appear in a MySQL identifier at all.
fn quote_ident(name: &str) -> Result<String> {
    if name.is_empty() || name.len() > 64 {
        bail!("bad identifier: {name:?}");
    }
    if name.contains('\0') || name.contains('\n') {
        bail!("bad identifier: {name:?}");
    }
    Ok(format!("`{}`", name.replace('`', "``")))
}

/// `USE <schema>` with the name quoted, for the console's database picker.
pub fn use_schema_sql(schema: &str) -> Result<String> {
    Ok(format!("USE {}", quote_ident(schema)?))
}

/// Switches the default database for this connection.
pub async fn use_schema(conn: &mut Conn, schema: &str) -> Result<()> {
    let sql = use_schema_sql(schema)?;
    conn.query_drop(&sql)
        .await
        .with_context(|| format!("{sql} failed"))
}

/// Escapes a value for a WHERE literal. Backslash is an escape character in
/// MySQL string literals by default, so it has to be doubled as well.
fn escape_literal(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\'', "''")
}

/// `col = 'value'`, or `col IS NULL`, for the browser's filter box.
pub fn equality_filter(column: &str, value: Option<&str>) -> Result<String> {
    let col = quote_ident(column)?;
    Ok(match value {
        Some(v) => format!("{col} = '{}'", escape_literal(v)),
        None => format!("{col} IS NULL"),
    })
}

fn qualified(schema: &str, table: &str) -> Result<String> {
    Ok(format!("{}.{}", quote_ident(schema)?, quote_ident(table)?))
}

#[derive(Debug, Clone, Default)]
pub struct TableInfo {
    pub name: String,
    pub engine: String,
    pub rows: u64,
    pub is_view: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub is_pk: bool,
    pub extra: String,
}

/// Columns plus whether the table can be edited row-by-row.
#[derive(Debug, Clone, Default)]
pub struct TableSchema {
    pub schema: String,
    pub table: String,
    pub columns: Vec<ColumnInfo>,
}

impl TableSchema {
    pub fn primary_key(&self) -> Vec<&ColumnInfo> {
        self.columns.iter().filter(|c| c.is_pk).collect()
    }

    /// Editing needs a primary key: it is the only WHERE clause that is
    /// guaranteed to address exactly one row.
    pub fn editable(&self) -> bool {
        !self.primary_key().is_empty()
    }
}

pub async fn list_schemas(conn: &mut Conn) -> Result<Vec<String>> {
    let rows: Vec<String> = conn
        .query("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME")
        .await
        .context("listing schemas failed")?;
    Ok(rows
        .into_iter()
        .filter(|s| !SYSTEM_SCHEMAS.contains(&s.as_str()))
        .collect())
}

pub async fn list_tables(conn: &mut Conn, schema: &str) -> Result<Vec<TableInfo>> {
    let rows: Vec<Row> = conn
        .exec(
            "SELECT TABLE_NAME, COALESCE(ENGINE,''), COALESCE(TABLE_ROWS,0), TABLE_TYPE \
             FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
            (schema,),
        )
        .await
        .context("listing tables failed")?;

    Ok(rows
        .into_iter()
        .map(|mut r| {
            let name = r.take::<Option<String>, _>(0).flatten().unwrap_or_default();
            let engine = r.take::<Option<String>, _>(1).flatten().unwrap_or_default();
            let rows = r.take::<Option<u64>, _>(2).flatten().unwrap_or(0);
            let ttype = r.take::<Option<String>, _>(3).flatten().unwrap_or_default();
            TableInfo {
                name,
                engine,
                rows,
                is_view: ttype.eq_ignore_ascii_case("VIEW"),
            }
        })
        .collect())
}

pub async fn describe(conn: &mut Conn, schema: &str, table: &str) -> Result<TableSchema> {
    let rows: Vec<Row> = conn
        .exec(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COALESCE(EXTRA,'') \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            (schema, table),
        )
        .await
        .context("describing table failed")?;

    let columns = rows
        .into_iter()
        .map(|mut r| {
            let name = r.take::<Option<String>, _>(0).flatten().unwrap_or_default();
            let data_type = r.take::<Option<String>, _>(1).flatten().unwrap_or_default();
            let nullable = r
                .take::<Option<String>, _>(2)
                .flatten()
                .unwrap_or_default()
                .eq_ignore_ascii_case("YES");
            let key = r.take::<Option<String>, _>(3).flatten().unwrap_or_default();
            let extra = r.take::<Option<String>, _>(4).flatten().unwrap_or_default();
            ColumnInfo {
                name,
                data_type,
                nullable,
                is_pk: key.eq_ignore_ascii_case("PRI"),
                extra,
            }
        })
        .collect();

    Ok(TableSchema {
        schema: schema.to_string(),
        table: table.to_string(),
        columns,
    })
}

/// How a browser page is ordered and filtered.
#[derive(Debug, Clone, Default)]
pub struct BrowseSpec {
    pub schema: String,
    pub table: String,
    /// Column name to sort on, from a header click.
    pub order_by: Option<String>,
    pub descending: bool,
    /// Raw WHERE text typed by the user, without the `WHERE` keyword.
    pub filter: String,
    pub limit: usize,
    pub offset: usize,
}

pub async fn browse(conn: &mut Conn, spec: &BrowseSpec) -> Result<SqlOutcome> {
    let mut sql = format!("SELECT * FROM {}", qualified(&spec.schema, &spec.table)?);
    if !spec.filter.trim().is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(spec.filter.trim());
    }
    if let Some(col) = &spec.order_by {
        sql.push_str(" ORDER BY ");
        sql.push_str(&quote_ident(col)?);
        sql.push_str(if spec.descending { " DESC" } else { " ASC" });
    }
    // One extra row tells the pager whether a next page exists.
    sql.push_str(&format!(" LIMIT {} OFFSET {}", spec.limit + 1, spec.offset));

    let mut out = run(conn, &sql, spec.limit + 1).await?;
    if out.grid.rows.len() > spec.limit {
        out.grid.rows.truncate(spec.limit);
        out.truncated = true;
    }
    Ok(out)
}

pub async fn count_rows(conn: &mut Conn, spec: &BrowseSpec) -> Result<u64> {
    let mut sql = format!(
        "SELECT COUNT(*) FROM {}",
        qualified(&spec.schema, &spec.table)?
    );
    if !spec.filter.trim().is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(spec.filter.trim());
    }
    let n: Option<u64> = conn.query_first(sql).await.context("count failed")?;
    Ok(n.unwrap_or(0))
}

/// Primary-key values identifying one row.
pub type RowKey = Vec<(String, Option<String>)>;

/// A pending row edit. Built by the UI, rendered as SQL before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Update {
        schema: String,
        table: String,
        key: RowKey,
        column: String,
        value: Option<String>,
    },
    Delete {
        schema: String,
        table: String,
        key: RowKey,
    },
    Insert {
        schema: String,
        table: String,
        values: Vec<(String, Option<String>)>,
    },
}

impl Change {
    pub fn verb(&self) -> &'static str {
        match self {
            Change::Update { .. } => "UPDATE",
            Change::Delete { .. } => "DELETE",
            Change::Insert { .. } => "INSERT",
        }
    }
}

fn param(v: &Option<String>) -> Value {
    match v {
        Some(s) => Value::Bytes(s.clone().into_bytes()),
        None => Value::NULL,
    }
}

/// Renders the statement with `?` placeholders — what the UI shows before the
/// user confirms. The same builder produces what actually runs.
fn build(change: &Change) -> Result<(String, Vec<Value>)> {
    match change {
        Change::Update {
            schema,
            table,
            key,
            column,
            value,
        } => {
            if key.is_empty() {
                bail!("refusing to UPDATE without a primary key");
            }
            let mut params = vec![param(value)];
            // `<=>` is NULL-safe, so a nullable key column still matches.
            let mut wheres = Vec::new();
            for (col, val) in key {
                wheres.push(format!("{} <=> ?", quote_ident(col)?));
                params.push(param(val));
            }
            let sql = format!(
                "UPDATE {} SET {} = ? WHERE {} LIMIT 1",
                qualified(schema, table)?,
                quote_ident(column)?,
                wheres.join(" AND ")
            );
            Ok((sql, params))
        }

        Change::Delete { schema, table, key } => {
            if key.is_empty() {
                bail!("refusing to DELETE without a primary key");
            }
            let mut params = Vec::new();
            let mut wheres = Vec::new();
            for (col, val) in key {
                wheres.push(format!("{} <=> ?", quote_ident(col)?));
                params.push(param(val));
            }
            let sql = format!(
                "DELETE FROM {} WHERE {} LIMIT 1",
                qualified(schema, table)?,
                wheres.join(" AND ")
            );
            Ok((sql, params))
        }

        Change::Insert {
            schema,
            table,
            values,
        } => {
            if values.is_empty() {
                bail!("nothing to insert");
            }
            let mut cols = Vec::new();
            let mut params = Vec::new();
            for (col, val) in values {
                cols.push(quote_ident(col)?);
                params.push(param(val));
            }
            let placeholders = vec!["?"; cols.len()].join(", ");
            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({placeholders})",
                qualified(schema, table)?,
                cols.join(", ")
            );
            Ok((sql, params))
        }
    }
}

/// The statement a change will run, for display.
pub fn preview(change: &Change) -> String {
    match build(change) {
        Ok((sql, params)) => {
            let mut out = sql;
            for p in params {
                let literal = match p {
                    Value::NULL => "NULL".to_string(),
                    Value::Bytes(b) => {
                        format!("'{}'", String::from_utf8_lossy(&b).replace('\'', "''"))
                    }
                    other => other.as_sql(false),
                };
                out = out.replacen('?', &literal, 1);
            }
            out
        }
        Err(e) => format!("-- {e}"),
    }
}

/// Applies changes in one transaction: either all land or none do.
pub async fn apply(conn: &mut Conn, changes: &[Change]) -> Result<u64> {
    if changes.is_empty() {
        return Ok(0);
    }
    let mut built = Vec::with_capacity(changes.len());
    for c in changes {
        built.push(build(c)?);
    }

    conn.query_drop("START TRANSACTION").await?;
    let mut affected = 0u64;
    for (sql, params) in built {
        match conn.exec_drop(&sql, params).await {
            Ok(()) => affected += conn.affected_rows(),
            Err(e) => {
                let _ = conn.query_drop("ROLLBACK").await;
                return Err(anyhow::Error::new(e).context(format!("rolled back at: {sql}")));
            }
        }
    }
    conn.query_drop("COMMIT").await?;
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_leading_keyword() {
        assert_eq!(classify("SELECT 1"), StatementKind::Read);
        assert_eq!(classify("  select * from t"), StatementKind::Read);
        assert_eq!(
            classify("WITH x AS (SELECT 1) SELECT * FROM x"),
            StatementKind::Read
        );
        assert_eq!(classify("SHOW ENGINE INNODB STATUS"), StatementKind::Read);
        assert_eq!(classify("UPDATE t SET a=1"), StatementKind::Write);
        assert_eq!(classify("DELETE FROM t"), StatementKind::Write);
        assert_eq!(classify("DROP TABLE t"), StatementKind::Ddl);
        assert_eq!(classify("VACUUM"), StatementKind::Unknown);
    }

    #[test]
    fn comments_do_not_disguise_a_write() {
        assert_eq!(classify("-- harmless\nDELETE FROM t"), StatementKind::Write);
        assert_eq!(classify("/* x */ DROP TABLE t"), StatementKind::Ddl);
        assert_eq!(classify("# note\nSELECT 1"), StatementKind::Read);
    }

    #[test]
    fn use_statement_quotes_the_schema() {
        assert_eq!(use_schema_sql("demo").unwrap(), "USE `demo`");
        assert_eq!(use_schema_sql("we`ird").unwrap(), "USE `we``ird`");
        assert!(use_schema_sql("bad\nname").is_err());
    }

    #[test]
    fn identifiers_are_quoted_and_escaped() {
        assert_eq!(quote_ident("users").unwrap(), "`users`");
        assert_eq!(quote_ident("we`ird").unwrap(), "`we``ird`");
        assert!(quote_ident("").is_err());
        assert!(quote_ident("a\nb").is_err());
        assert_eq!(qualified("demo", "or`ders").unwrap(), "`demo`.`or``ders`");
    }

    fn key() -> RowKey {
        vec![("id".into(), Some("7".into()))]
    }

    #[test]
    fn equality_filter_quotes_both_sides() {
        assert_eq!(equality_filter("id", Some("7")).unwrap(), "`id` = '7'");
        assert_eq!(equality_filter("note", None).unwrap(), "`note` IS NULL");
        assert_eq!(
            equality_filter("na`me", Some("O'Brien")).unwrap(),
            "`na``me` = 'O''Brien'"
        );
        assert_eq!(
            equality_filter("p", Some("c:\\tmp")).unwrap(),
            "`p` = 'c:\\\\tmp'",
            "backslash is an escape character in MySQL literals"
        );
        assert!(equality_filter("bad\nname", Some("x")).is_err());
    }

    #[test]
    fn update_binds_values_and_keys_on_the_pk() {
        let c = Change::Update {
            schema: "demo".into(),
            table: "users".into(),
            key: key(),
            column: "email".into(),
            value: Some("a@b.c".into()),
        };
        let (sql, params) = build(&c).unwrap();
        assert_eq!(
            sql,
            "UPDATE `demo`.`users` SET `email` = ? WHERE `id` <=> ? LIMIT 1"
        );
        assert_eq!(params.len(), 2);
        assert_eq!(
            preview(&c),
            "UPDATE `demo`.`users` SET `email` = 'a@b.c' WHERE `id` <=> '7' LIMIT 1"
        );
    }

    #[test]
    fn null_round_trips_as_a_literal_null() {
        let c = Change::Update {
            schema: "demo".into(),
            table: "users".into(),
            key: key(),
            column: "nickname".into(),
            value: None,
        };
        assert!(preview(&c).contains("SET `nickname` = NULL"));
    }

    #[test]
    fn no_primary_key_means_no_write() {
        let c = Change::Delete {
            schema: "demo".into(),
            table: "users".into(),
            key: vec![],
        };
        assert!(build(&c).is_err());
    }

    #[test]
    fn injection_in_a_value_stays_a_value() {
        let c = Change::Update {
            schema: "demo".into(),
            table: "users".into(),
            key: key(),
            column: "email".into(),
            value: Some("x'; DROP TABLE users; --".into()),
        };
        let (sql, _) = build(&c).unwrap();
        assert!(!sql.contains("DROP"), "value must never reach the SQL text");
        assert!(preview(&c).contains("'x''; DROP TABLE users; --'"));
    }

    #[test]
    fn editable_requires_a_primary_key() {
        let mut t = TableSchema {
            schema: "demo".into(),
            table: "t".into(),
            columns: vec![ColumnInfo {
                name: "a".into(),
                ..Default::default()
            }],
        };
        assert!(!t.editable());
        t.columns[0].is_pk = true;
        assert!(t.editable());
    }
}
