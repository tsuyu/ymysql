//! Renders a result grid as a GitHub-flavoured Markdown table, for pasting
//! into an issue, a pull request or a ticket.
//!
//! Cells are padded to a common width so the raw text is readable too, and a
//! column whose values all parse as numbers is right-aligned in the delimiter
//! row. Alignment is presentation only — it never changes a value.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::db::queries::Grid;

/// What a `NULL` looks like, kept distinct from an empty cell.
const NULL: &str = "_NULL_";

/// A pipe would end the cell and a newline would end the row, so both are
/// rewritten. The line break becomes `<br>` rather than a space: a Markdown
/// table cannot hold a real newline, and dropping it loses information.
fn cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Escape the backslash first, so `a\|b` cannot become `a\\|b`
            // and re-open the cell.
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            '\r' => {
                // Treat CRLF as one break.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("<br>");
            }
            '\n' => out.push_str("<br>"),
            other => out.push(other),
        }
    }
    out
}

fn width(s: &str) -> usize {
    s.chars().count()
}

/// True when every value present in the column parses as a number.
fn is_numeric(grid: &Grid, col: usize) -> bool {
    let mut seen = false;
    for row in &grid.rows {
        match row.get(col).and_then(|c| c.as_deref()) {
            None => {}
            Some(v) => {
                if v.trim().parse::<f64>().is_err() {
                    return false;
                }
                seen = true;
            }
        }
    }
    seen
}

pub fn render(grid: &Grid) -> String {
    if grid.columns.is_empty() {
        return String::new();
    }

    let headers: Vec<String> = grid.columns.iter().map(|c| cell(c)).collect();
    let body: Vec<Vec<String>> = grid
        .rows
        .iter()
        .map(|row| {
            (0..grid.columns.len())
                .map(|i| match row.get(i) {
                    Some(Some(v)) => cell(v),
                    // A short row and a NULL read the same way here; neither is
                    // a value the server returned.
                    _ => NULL.to_string(),
                })
                .collect()
        })
        .collect();

    let numeric: Vec<bool> = (0..grid.columns.len())
        .map(|i| is_numeric(grid, i))
        .collect();

    // Three characters is the narrowest delimiter used here, so no column
    // renders thinner than `---` (or `--:` when right-aligned).
    let widths: Vec<usize> = (0..grid.columns.len())
        .map(|i| {
            body.iter()
                .map(|r| width(&r[i]))
                .chain(std::iter::once(width(&headers[i])))
                .chain(std::iter::once(3))
                .max()
                .unwrap_or(3)
        })
        .collect();

    let mut out = String::new();
    push_row(&mut out, &headers, &widths, &numeric);

    out.push('|');
    for (i, w) in widths.iter().enumerate() {
        out.push(' ');
        if numeric[i] {
            out.push_str(&"-".repeat(w - 1));
            out.push(':');
        } else {
            out.push_str(&"-".repeat(*w));
        }
        out.push_str(" |");
    }
    out.push('\n');

    for row in &body {
        push_row(&mut out, row, &widths, &numeric);
    }
    out
}

fn push_row(out: &mut String, cells: &[String], widths: &[usize], numeric: &[bool]) {
    out.push('|');
    for (i, c) in cells.iter().enumerate() {
        let pad = widths[i].saturating_sub(width(c));
        out.push(' ');
        if numeric[i] {
            out.push_str(&" ".repeat(pad));
            out.push_str(c);
        } else {
            out.push_str(c);
            out.push_str(&" ".repeat(pad));
        }
        out.push_str(" |");
    }
    out.push('\n');
}

pub fn write_file(path: &Path, grid: &Grid) -> Result<()> {
    std::fs::write(path, render(grid))
        .with_context(|| format!("could not write {}", path.display()))
}

/// A timestamped `.md` file in the working directory.
pub fn suggested_path() -> PathBuf {
    let dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    dir.join(format!(
        "query_{}.md",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    ))
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
    fn columns_are_padded_to_a_common_width() {
        let g = grid(
            &["id", "name"],
            &[&[Some("1"), Some("ada")], &[Some("2"), Some("bartholomew")]],
        );
        assert_eq!(
            render(&g),
            "|  id | name        |\n\
             | --: | ----------- |\n\
             |   1 | ada         |\n\
             |   2 | bartholomew |\n"
        );
    }

    #[test]
    fn a_numeric_column_is_right_aligned() {
        let g = grid(&["n", "s"], &[&[Some("10"), Some("x")]]);
        let out = render(&g);
        assert!(out.contains("| --: | --- |"), "{out}");
        assert!(out.contains("|  10 | x   |"), "{out}");
    }

    #[test]
    fn a_column_of_nulls_only_is_not_numeric() {
        let g = grid(&["n"], &[&[None]]);
        assert!(render(&g).contains("| ------ |"), "{}", render(&g));
    }

    #[test]
    fn a_mixed_column_is_left_aligned() {
        let g = grid(&["n"], &[&[Some("1")], &[Some("x")]]);
        assert!(!render(&g).contains("---:"));
    }

    #[test]
    fn pipes_and_backslashes_are_escaped() {
        let g = grid(&["v"], &[&[Some("a|b")], &[Some("c\\d")]]);
        let out = render(&g);
        assert!(out.contains("a\\|b"), "{out}");
        assert!(out.contains("c\\\\d"), "{out}");
    }

    #[test]
    fn a_newline_becomes_a_break_rather_than_a_new_row() {
        let g = grid(&["v"], &[&[Some("one\ntwo")]]);
        let out = render(&g);
        assert_eq!(out.lines().count(), 3, "{out}");
        assert!(out.contains("one<br>two"), "{out}");
    }

    #[test]
    fn a_crlf_is_one_break() {
        let g = grid(&["v"], &[&[Some("one\r\ntwo")]]);
        assert!(render(&g).contains("one<br>two"));
    }

    #[test]
    fn null_is_marked_and_the_empty_string_is_blank() {
        let g = grid(&["a", "b"], &[&[None, Some("")]]);
        let out = render(&g);
        assert!(out.contains("| _NULL_ |"), "{out}");
        assert!(out.contains("|     |\n"), "{out}");
    }

    #[test]
    fn a_pipe_in_a_header_is_escaped() {
        let g = grid(&["a|b"], &[]);
        assert!(render(&g).starts_with("| a\\|b |"));
    }

    #[test]
    fn no_column_is_narrower_than_the_delimiter() {
        // The delimiter needs at least one dash; three keeps every column a
        // readable width, and the colon replaces the last one.
        let g = grid(&["a"], &[&[Some("1")]]);
        let out = render(&g);
        assert!(out.contains("| --: |"), "{out}");
        for line in out.lines() {
            assert_eq!(line.chars().count(), 7, "ragged line: {line:?}");
        }
    }

    #[test]
    fn an_empty_result_is_a_header_and_a_delimiter() {
        let out = render(&grid(&["a", "b"], &[]));
        assert_eq!(out, "| a   | b   |\n| --- | --- |\n");
    }

    #[test]
    fn a_grid_with_no_columns_renders_nothing() {
        assert_eq!(render(&grid(&[], &[])), "");
    }
}
