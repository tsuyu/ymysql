//! Turns a result grid into `INSERT` statements.
//!
//! This is the console's export, not a dump. A grid holds text the server
//! already rendered, so every non-`NULL` value goes out as a quoted string and
//! MySQL casts it on the way in. That is deliberate: emitting `007` bare would
//! silently become `7` in a `VARCHAR` column, and the grid carries no column
//! types to decide otherwise. For a type-faithful copy of a whole table, use
//! the Dump tab — it works from the raw protocol values.

use anyhow::Result;

use crate::db::dump::{escape_string, quote_ident};
use crate::db::queries::Grid;

/// Which statement to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verb {
    #[default]
    Insert,
    /// Skips rows that collide with an existing key.
    InsertIgnore,
    /// Deletes the colliding row first, then inserts.
    Replace,
}

impl Verb {
    pub const ALL: [Verb; 3] = [Verb::Insert, Verb::InsertIgnore, Verb::Replace];

    pub fn keyword(self) -> &'static str {
        match self {
            Verb::Insert => "INSERT INTO",
            Verb::InsertIgnore => "INSERT IGNORE INTO",
            Verb::Replace => "REPLACE INTO",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Verb::Insert => "INSERT",
            Verb::InsertIgnore => "INSERT IGNORE",
            Verb::Replace => "REPLACE",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Target table, optionally `schema.table`. Quoted on the way out.
    pub table: String,
    pub verb: Verb,
    /// Rows per statement. 1 gives one statement per row.
    pub rows_per_statement: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            table: String::new(),
            verb: Verb::Insert,
            rows_per_statement: 50,
        }
    }
}

/// The target table the grid came from, as `schema.table`, when every column
/// that traces back to a real table agrees on one. Empty otherwise.
pub fn suggested_table(grid: &Grid) -> String {
    let mut found: Option<(&str, &str)> = None;
    for i in 0..grid.columns.len() {
        let Some(o) = grid.origin(i) else { continue };
        match found {
            None => found = Some((&o.schema, &o.table)),
            Some(prev) if prev == (o.schema.as_str(), o.table.as_str()) => {}
            // Columns from two tables: the caller has to name the target.
            Some(_) => return String::new(),
        }
    }
    match found {
        Some((schema, table)) => format!("{schema}.{table}"),
        None => String::new(),
    }
}

/// Column names to insert into: the origin column where the server traced one,
/// so a `SELECT id AS user_id` still writes to `id`, and the header otherwise.
fn target_columns(grid: &Grid) -> Vec<String> {
    (0..grid.columns.len())
        .map(|i| match grid.origin(i) {
            Some(o) => o.column.clone(),
            None => grid.columns[i].clone(),
        })
        .collect()
}

/// `schema.table` becomes `` `schema`.`table` ``. A name with no dot is used
/// as-is; a name with more than one is rejected rather than guessed at.
fn quote_table(name: &str) -> Result<String> {
    let name = name.trim();
    let parts: Vec<&str> = name.split('.').collect();
    match parts.as_slice() {
        [table] => quote_ident(table),
        [schema, table] => Ok(format!("{}.{}", quote_ident(schema)?, quote_ident(table)?)),
        _ => anyhow::bail!("bad table name: {name:?}"),
    }
}

fn literal(cell: Option<&str>) -> String {
    match cell {
        None => "NULL".to_string(),
        Some(v) => format!("'{}'", escape_string(v)),
    }
}

/// Renders the grid as `INSERT` statements. An empty grid renders nothing.
pub fn render(grid: &Grid, opts: &Options) -> Result<String> {
    if grid.rows.is_empty() || grid.columns.is_empty() {
        return Ok(String::new());
    }
    let table = quote_table(&opts.table)?;
    let cols = target_columns(grid)
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let per = opts.rows_per_statement.max(1);

    let mut out = String::new();
    for chunk in grid.rows.chunks(per) {
        out.push_str(opts.verb.keyword());
        out.push(' ');
        out.push_str(&table);
        out.push_str(" (");
        out.push_str(&cols);
        out.push_str(") VALUES\n");
        for (i, row) in chunk.iter().enumerate() {
            out.push('(');
            for (j, cell) in row.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                out.push_str(&literal(cell.as_deref()));
            }
            out.push(')');
            out.push_str(if i + 1 == chunk.len() { ";\n" } else { ",\n" });
        }
    }
    Ok(out)
}

/// Renders and writes to `path`.
pub fn write_file(path: &std::path::Path, grid: &Grid, opts: &Options) -> Result<()> {
    use anyhow::Context as _;
    let body = render(grid, opts)?;
    std::fs::write(path, body).with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

/// A timestamped `.sql` file in the working directory.
pub fn suggested_path() -> std::path::PathBuf {
    let dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    dir.join(format!(
        "rows_{}.sql",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::queries::ColumnOrigin;

    fn grid(columns: &[&str], rows: &[&[Option<&str>]]) -> Grid {
        Grid {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|c| c.map(str::to_string)).collect())
                .collect(),
            origins: Vec::new(),
        }
    }

    fn origin(schema: &str, table: &str, column: &str) -> ColumnOrigin {
        ColumnOrigin {
            schema: schema.into(),
            table: table.into(),
            column: column.into(),
        }
    }

    fn opts(table: &str) -> Options {
        Options {
            table: table.into(),
            ..Options::default()
        }
    }

    #[test]
    fn batches_rows_into_one_statement() {
        let g = grid(
            &["id", "name"],
            &[&[Some("1"), Some("ada")], &[Some("2"), None]],
        );
        assert_eq!(
            render(&g, &opts("users")).unwrap(),
            "INSERT INTO `users` (`id`, `name`) VALUES\n('1','ada'),\n('2',NULL);\n"
        );
    }

    #[test]
    fn one_row_per_statement_when_asked() {
        let g = grid(&["id"], &[&[Some("1")], &[Some("2")]]);
        let o = Options {
            rows_per_statement: 1,
            ..opts("t")
        };
        assert_eq!(
            render(&g, &o).unwrap(),
            "INSERT INTO `t` (`id`) VALUES\n('1');\nINSERT INTO `t` (`id`) VALUES\n('2');\n"
        );
    }

    #[test]
    fn zero_rows_per_statement_does_not_divide_by_zero() {
        let g = grid(&["id"], &[&[Some("1")]]);
        let o = Options {
            rows_per_statement: 0,
            ..opts("t")
        };
        assert_eq!(
            render(&g, &o).unwrap(),
            "INSERT INTO `t` (`id`) VALUES\n('1');\n"
        );
    }

    #[test]
    fn verbs_change_the_keyword() {
        let g = grid(&["id"], &[&[Some("1")]]);
        for (verb, head) in [
            (Verb::Insert, "INSERT INTO `t`"),
            (Verb::InsertIgnore, "INSERT IGNORE INTO `t`"),
            (Verb::Replace, "REPLACE INTO `t`"),
        ] {
            let o = Options { verb, ..opts("t") };
            assert!(render(&g, &o).unwrap().starts_with(head));
        }
    }

    #[test]
    fn values_are_escaped_the_way_a_restore_expects() {
        let g = grid(&["v"], &[&[Some("it's\\ a\nline")]]);
        assert_eq!(
            render(&g, &opts("t")).unwrap(),
            "INSERT INTO `t` (`v`) VALUES\n('it\\'s\\\\ a\\nline');\n"
        );
    }

    #[test]
    fn numbers_stay_quoted_so_leading_zeroes_survive() {
        let g = grid(&["code"], &[&[Some("007")]]);
        assert!(render(&g, &opts("t")).unwrap().contains("('007')"));
    }

    #[test]
    fn identifiers_are_backtick_quoted() {
        let mut g = grid(&["we`ird"], &[&[Some("1")]]);
        g.columns = vec!["we`ird".into()];
        let sql = render(&g, &opts("ta`ble")).unwrap();
        assert!(
            sql.starts_with("INSERT INTO `ta``ble` (`we``ird`) VALUES"),
            "{sql}"
        );
    }

    #[test]
    fn a_schema_qualified_target_is_quoted_in_both_halves() {
        let g = grid(&["id"], &[&[Some("1")]]);
        assert!(
            render(&g, &opts("demo.users"))
                .unwrap()
                .starts_with("INSERT INTO `demo`.`users`")
        );
    }

    #[test]
    fn a_target_with_two_dots_is_refused() {
        let g = grid(&["id"], &[&[Some("1")]]);
        assert!(render(&g, &opts("a.b.c")).is_err());
    }

    #[test]
    fn an_empty_target_is_refused() {
        let g = grid(&["id"], &[&[Some("1")]]);
        assert!(render(&g, &opts("")).is_err());
    }

    #[test]
    fn aliases_are_written_back_to_the_real_column() {
        let mut g = grid(&["user_id"], &[&[Some("1")]]);
        g.origins = vec![origin("demo", "users", "id")];
        assert!(
            render(&g, &opts("users"))
                .unwrap()
                .starts_with("INSERT INTO `users` (`id`)")
        );
    }

    #[test]
    fn a_computed_column_keeps_its_header() {
        let mut g = grid(&["total"], &[&[Some("1")]]);
        g.origins = vec![ColumnOrigin::default()];
        assert!(
            render(&g, &opts("t"))
                .unwrap()
                .starts_with("INSERT INTO `t` (`total`)")
        );
    }

    #[test]
    fn the_target_is_suggested_from_the_column_origins() {
        let mut g = grid(&["id", "name"], &[]);
        g.origins = vec![
            origin("demo", "users", "id"),
            origin("demo", "users", "name"),
        ];
        assert_eq!(suggested_table(&g), "demo.users");
    }

    #[test]
    fn a_join_suggests_nothing() {
        let mut g = grid(&["id", "total"], &[]);
        g.origins = vec![
            origin("demo", "users", "id"),
            origin("demo", "orders", "total"),
        ];
        assert_eq!(suggested_table(&g), "");
    }

    #[test]
    fn an_empty_grid_renders_nothing() {
        assert_eq!(render(&grid(&["a"], &[]), &opts("t")).unwrap(), "");
    }
}
