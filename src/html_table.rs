//! Renders a result grid as an HTML table.
//!
//! Every cell is escaped, headers included. Result rows are server data, and a
//! value like `<script>` has to survive as text rather than becoming markup —
//! so all five of `& < > " '` go out as entities, which is safe in both text
//! and attribute positions.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::db::queries::Grid;

#[derive(Debug, Clone)]
pub struct Options {
    /// Wrap the table in a standalone document with a stylesheet. Off gives a
    /// bare `<table>` to paste into a page of your own.
    pub full_document: bool,
    /// Shown as the document title and the table caption.
    pub title: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            full_document: true,
            title: "Query result".to_string(),
        }
    }
}

/// The document's stylesheet. Kept inline so the file works on its own, and
/// theme-aware because a result dumped at night is usually read at night.
const STYLE: &str = "\
:root { color-scheme: light dark; }
body { font: 14px/1.5 system-ui, sans-serif; margin: 2rem; }
caption { text-align: left; font-weight: 600; padding-bottom: .5rem; }
table { border-collapse: collapse; }
th, td { border: 1px solid #8884; padding: .25rem .6rem; text-align: left;
         vertical-align: top; white-space: pre-wrap; }
th { background: #8882; position: sticky; top: 0; }
tbody tr:nth-child(even) { background: #8881; }
td.null { opacity: .5; font-style: italic; }
";

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The `<table>` element on its own.
pub fn render_table(grid: &Grid, caption: &str) -> String {
    let mut out = String::from("<table>\n");
    if !caption.is_empty() {
        out.push_str(&format!("  <caption>{}</caption>\n", escape(caption)));
    }

    out.push_str("  <thead>\n    <tr>");
    for c in &grid.columns {
        out.push_str(&format!("<th>{}</th>", escape(c)));
    }
    out.push_str("</tr>\n  </thead>\n  <tbody>\n");

    for row in &grid.rows {
        out.push_str("    <tr>");
        for cell in row {
            match cell {
                // A real SQL NULL, marked so it cannot be read as the text
                // "NULL" or confused with the empty string.
                None => out.push_str("<td class=\"null\">NULL</td>"),
                Some(v) => out.push_str(&format!("<td>{}</td>", escape(v))),
            }
        }
        out.push_str("</tr>\n");
    }
    out.push_str("  </tbody>\n</table>\n");
    out
}

/// The grid, as a fragment or a standalone document.
pub fn render(grid: &Grid, opts: &Options) -> String {
    let caption = format!(
        "{} — {} row{}",
        opts.title,
        grid.rows.len(),
        if grid.rows.len() == 1 { "" } else { "s" }
    );
    let table = render_table(grid, &caption);
    if !opts.full_document {
        return table;
    }
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <meta charset=\"utf-8\">\n\
         <title>{}</title>\n\
         <style>\n{STYLE}</style>\n\
         {table}</html>\n",
        escape(&opts.title)
    )
}

pub fn write_file(path: &Path, grid: &Grid, opts: &Options) -> Result<()> {
    std::fs::write(path, render(grid, opts))
        .with_context(|| format!("could not write {}", path.display()))
}

/// A timestamped `.html` file in the working directory.
pub fn suggested_path() -> PathBuf {
    let dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    dir.join(format!(
        "query_{}.html",
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
    fn header_and_rows_land_in_the_right_elements() {
        let g = grid(&["id", "name"], &[&[Some("1"), Some("ada")]]);
        let html = render_table(&g, "");
        assert!(html.contains("<tr><th>id</th><th>name</th></tr>"), "{html}");
        assert!(html.contains("<tr><td>1</td><td>ada</td></tr>"), "{html}");
    }

    #[test]
    fn markup_in_a_value_stays_text() {
        let g = grid(&["v"], &[&[Some("<script>alert('x')</script>")]]);
        let html = render_table(&g, "");
        assert!(!html.contains("<script>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"),
            "{html}"
        );
    }

    #[test]
    fn markup_in_a_column_name_stays_text() {
        let g = grid(&["<b>"], &[]);
        assert!(render_table(&g, "").contains("<th>&lt;b&gt;</th>"));
    }

    #[test]
    fn ampersands_are_escaped_once() {
        let g = grid(&["v"], &[&[Some("a & b")], &[Some("&amp;")]]);
        let html = render_table(&g, "");
        assert!(html.contains("<td>a &amp; b</td>"), "{html}");
        assert!(html.contains("<td>&amp;amp;</td>"), "{html}");
    }

    #[test]
    fn null_and_the_empty_string_look_different() {
        let g = grid(&["a", "b"], &[&[None, Some("")]]);
        let html = render_table(&g, "");
        assert!(html.contains("<td class=\"null\">NULL</td>"), "{html}");
        assert!(html.contains("<td></td>"), "{html}");
    }

    #[test]
    fn the_caption_is_escaped_too() {
        let g = grid(&["a"], &[]);
        assert!(render_table(&g, "<x>").contains("<caption>&lt;x&gt;</caption>"));
    }

    #[test]
    fn a_fragment_has_no_document_wrapper() {
        let g = grid(&["a"], &[&[Some("1")]]);
        let opts = Options {
            full_document: false,
            ..Options::default()
        };
        let html = render(&g, &opts);
        assert!(html.starts_with("<table>"), "{html}");
        assert!(!html.contains("<!doctype"), "{html}");
    }

    #[test]
    fn a_document_carries_a_charset_and_a_title() {
        let g = grid(&["a"], &[&[Some("café")]]);
        let opts = Options {
            title: "demo.users".to_string(),
            ..Options::default()
        };
        let html = render(&g, &opts);
        assert!(html.starts_with("<!doctype html>"), "{html}");
        assert!(html.contains("<meta charset=\"utf-8\">"), "{html}");
        assert!(html.contains("<title>demo.users</title>"), "{html}");
        assert!(html.contains("café"), "{html}");
    }

    #[test]
    fn a_title_with_markup_cannot_break_out_of_the_title_element() {
        let g = grid(&["a"], &[]);
        let opts = Options {
            title: "</title><script>".to_string(),
            ..Options::default()
        };
        let html = render(&g, &opts);
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("</title><"), "{html}");
    }

    #[test]
    fn the_caption_counts_rows() {
        let one = render(&grid(&["a"], &[&[Some("1")]]), &Options::default());
        assert!(one.contains("1 row<"), "{one}");
        let two = render(
            &grid(&["a"], &[&[Some("1")], &[Some("2")]]),
            &Options::default(),
        );
        assert!(two.contains("2 rows<"), "{two}");
    }

    #[test]
    fn an_empty_grid_is_a_table_with_no_body_rows() {
        let html = render_table(&grid(&["a"], &[]), "");
        assert!(html.contains("<tbody>\n  </tbody>"), "{html}");
    }
}
