//! CSV export for result grids.
//!
//! RFC 4180: CRLF line endings, a field quoted when it holds a separator,
//! a quote, a newline or edge whitespace, and an embedded quote doubled.
//!
//! SQL `NULL` and the empty string are different values, and CSV has no way to
//! say so. The convention here is the one Postgres `COPY ... CSV` uses: `NULL`
//! is written as nothing at all, an empty string as `""`. That round-trips
//! through anything that respects quoting.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::db::queries::Grid;

const EOL: &str = "\r\n";

/// The whole grid, header row first.
pub fn render(grid: &Grid) -> String {
    let mut out = String::new();
    write_row(&mut out, grid.columns.iter().map(|c| Some(c.as_str())));
    for row in &grid.rows {
        write_row(&mut out, row.iter().map(|c| c.as_deref()));
    }
    out
}

fn write_row<'a>(out: &mut String, cells: impl Iterator<Item = Option<&'a str>>) {
    let mut first = true;
    for cell in cells {
        if !first {
            out.push(',');
        }
        first = false;
        match cell {
            // A bare field is NULL; `""` is the empty string.
            None => {}
            Some(v) => out.push_str(&field(v)),
        }
    }
    out.push_str(EOL);
}

fn field(s: &str) -> Cow<'_, str> {
    let edge_space = s.starts_with(' ') || s.ends_with(' ');
    if s.is_empty() || edge_space || s.contains([',', '"', '\n', '\r']) {
        let mut q = String::with_capacity(s.len() + 2);
        q.push('"');
        for c in s.chars() {
            if c == '"' {
                q.push('"');
            }
            q.push(c);
        }
        q.push('"');
        Cow::Owned(q)
    } else {
        Cow::Borrowed(s)
    }
}

/// A timestamped file in the working directory, matching the dump tab.
pub fn suggested_path() -> PathBuf {
    let dir: PathBuf = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    dir.join(suggested_file_name())
}

fn suggested_file_name() -> String {
    let now = chrono::Local::now().format("%Y%m%d_%H%M%S");
    format!("query_{now}.csv")
}

/// Writes `grid` to `path`. `bom` prefixes a UTF-8 byte-order mark, which is
/// what makes Excel read non-ASCII correctly; leave it off for anything that
/// parses the file itself.
pub fn write_file(path: &Path, grid: &Grid, bom: bool) -> Result<()> {
    let body = render(grid);
    let mut bytes = Vec::with_capacity(body.len() + 3);
    if bom {
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    bytes.extend_from_slice(body.as_bytes());
    std::fs::write(path, bytes).with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn header_comes_first_and_lines_end_crlf() {
        let g = grid(&["id", "name"], &[&[Some("1"), Some("ada")]]);
        assert_eq!(render(&g), "id,name\r\n1,ada\r\n");
    }

    #[test]
    fn null_is_bare_and_empty_string_is_quoted() {
        let g = grid(&["a", "b"], &[&[None, Some("")]]);
        assert_eq!(render(&g), "a,b\r\n,\"\"\r\n");
    }

    #[test]
    fn separators_quotes_and_newlines_are_quoted() {
        let g = grid(
            &["v"],
            &[
                &[Some("a,b")],
                &[Some("say \"hi\"")],
                &[Some("line1\nline2")],
                &[Some("tab\tkept")],
            ],
        );
        assert_eq!(
            render(&g),
            "v\r\n\"a,b\"\r\n\"say \"\"hi\"\"\"\r\n\"line1\nline2\"\r\ntab\tkept\r\n"
        );
    }

    #[test]
    fn edge_whitespace_is_preserved_by_quoting() {
        let g = grid(&["v"], &[&[Some("  padded  ")]]);
        assert_eq!(render(&g), "v\r\n\"  padded  \"\r\n");
    }

    #[test]
    fn a_column_name_needing_quotes_gets_them() {
        let g = grid(&["count(*)", "a,b"], &[]);
        assert_eq!(render(&g), "count(*),\"a,b\"\r\n");
    }

    #[test]
    fn an_empty_grid_is_just_the_header() {
        let g = grid(&["a"], &[]);
        assert_eq!(render(&g), "a\r\n");
    }

    #[test]
    fn utf8_survives_and_the_bom_is_opt_in() {
        let g = grid(&["v"], &[&[Some("café ☕")]]);
        let mut path = std::env::temp_dir();
        path.push(format!("mysql_perf_csv_{}.csv", std::process::id()));

        write_file(&path, &g, false).unwrap();
        let plain = std::fs::read(&path).unwrap();
        assert!(plain.starts_with(b"v\r\n"), "unexpected head");
        assert_eq!(String::from_utf8(plain).unwrap(), "v\r\ncafé ☕\r\n");

        write_file(&path, &g, true).unwrap();
        let with_bom = std::fs::read(&path).unwrap();
        assert_eq!(&with_bom[..3], &[0xEF, 0xBB, 0xBF]);
        assert_eq!(&with_bom[3..], "v\r\ncafé ☕\r\n".as_bytes());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn suggested_name_is_timestamped_and_has_the_extension() {
        let name = suggested_file_name();
        assert!(name.starts_with("query_"), "{name}");
        assert!(name.ends_with(".csv"), "{name}");
        assert_eq!(name.len(), "query_20260819_120000.csv".len(), "{name}");
    }
}
