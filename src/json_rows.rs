//! Renders a result grid as JSON.
//!
//! One object per row, keyed by column name. SQL `NULL` becomes JSON `null` —
//! the one export format that can say so without a convention.
//!
//! Values are strings, not numbers. A grid carries no column types, and
//! guessing costs more than it saves: a `BIGINT` id above 2^53 loses precision
//! the moment a JavaScript consumer parses it as a number, and `007` in a
//! `VARCHAR` would come back as `7`. Strings round-trip both.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::db::queries::Grid;

#[derive(Debug, Clone)]
pub struct Options {
    /// Indented and one field per line, rather than a single dense line.
    pub pretty: bool,
    /// Newline-delimited JSON: one compact object per line, no wrapping array.
    /// Streams into log pipelines and survives being cut in half.
    pub ndjson: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            pretty: true,
            ndjson: false,
        }
    }
}

/// Escapes per RFC 8259. `/` is deliberately left alone.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            // Everything below space has to be a numeric escape.
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            other => out.push(other),
        }
    }
    out
}

/// Object keys for the grid. A duplicate column name — two `id` columns from a
/// join, say — is suffixed rather than left to silently overwrite its twin,
/// which is what most JSON parsers would do with a repeated key.
fn keys(grid: &Grid) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(grid.columns.len());
    for name in &grid.columns {
        if !out.contains(name) {
            out.push(name.clone());
            continue;
        }
        let mut n = 2;
        let mut candidate = format!("{name}_{n}");
        while out.contains(&candidate) || grid.columns.contains(&candidate) {
            n += 1;
            candidate = format!("{name}_{n}");
        }
        out.push(candidate);
    }
    out
}

fn push_object(out: &mut String, keys: &[String], row: &[Option<String>], pretty: bool) {
    out.push('{');
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if pretty {
            out.push_str("\n    ");
        }
        out.push('"');
        out.push_str(&escape(key));
        out.push_str(if pretty { "\": " } else { "\":" });
        match row.get(i).and_then(|c| c.as_deref()) {
            None => out.push_str("null"),
            Some(v) => {
                out.push('"');
                out.push_str(&escape(v));
                out.push('"');
            }
        }
    }
    if pretty && !keys.is_empty() {
        out.push_str("\n  ");
    }
    out.push('}');
}

pub fn render(grid: &Grid, opts: &Options) -> String {
    let keys = keys(grid);

    if opts.ndjson {
        let mut out = String::new();
        for row in &grid.rows {
            push_object(&mut out, &keys, row, false);
            out.push('\n');
        }
        return out;
    }

    if grid.rows.is_empty() {
        return "[]\n".to_string();
    }

    let mut out = String::from("[");
    for (i, row) in grid.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if opts.pretty {
            out.push_str("\n  ");
        }
        push_object(&mut out, &keys, row, opts.pretty);
    }
    out.push_str(if opts.pretty { "\n]\n" } else { "]\n" });
    out
}

pub fn write_file(path: &Path, grid: &Grid, opts: &Options) -> Result<()> {
    std::fs::write(path, render(grid, opts))
        .with_context(|| format!("could not write {}", path.display()))
}

/// A timestamped file in the working directory, `.ndjson` when that is the
/// shape being written.
pub fn suggested_path(ndjson: bool) -> PathBuf {
    let dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    let ext = if ndjson { "ndjson" } else { "json" };
    dir.join(format!(
        "query_{}.{ext}",
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

    fn compact() -> Options {
        Options {
            pretty: false,
            ndjson: false,
        }
    }

    #[test]
    fn one_object_per_row() {
        let g = grid(
            &["id", "name"],
            &[&[Some("1"), Some("ada")], &[Some("2"), Some("bob")]],
        );
        assert_eq!(
            render(&g, &compact()),
            "[{\"id\":\"1\",\"name\":\"ada\"},{\"id\":\"2\",\"name\":\"bob\"}]\n"
        );
    }

    #[test]
    fn null_is_json_null_and_the_empty_string_is_a_string() {
        let g = grid(&["a", "b"], &[&[None, Some("")]]);
        assert_eq!(render(&g, &compact()), "[{\"a\":null,\"b\":\"\"}]\n");
    }

    #[test]
    fn numbers_stay_strings_so_big_ids_and_leading_zeroes_survive() {
        let g = grid(&["id", "code"], &[&[Some("9007199254740993"), Some("007")]]);
        let out = render(&g, &compact());
        assert!(out.contains("\"id\":\"9007199254740993\""), "{out}");
        assert!(out.contains("\"code\":\"007\""), "{out}");
    }

    #[test]
    fn quotes_backslashes_and_control_characters_are_escaped() {
        let g = grid(&["v"], &[&[Some("say \"hi\"\\ a\nb\tc\u{1}")]]);
        assert_eq!(
            render(&g, &compact()),
            "[{\"v\":\"say \\\"hi\\\"\\\\ a\\nb\\tc\\u0001\"}]\n"
        );
    }

    #[test]
    fn a_forward_slash_is_left_alone() {
        let g = grid(&["v"], &[&[Some("a/b")]]);
        assert!(render(&g, &compact()).contains("\"a/b\""));
    }

    #[test]
    fn non_ascii_is_emitted_as_itself() {
        let g = grid(&["v"], &[&[Some("café ☕")]]);
        assert!(render(&g, &compact()).contains("\"café ☕\""));
    }

    #[test]
    fn duplicate_column_names_are_suffixed_rather_than_lost() {
        let g = grid(&["id", "id"], &[&[Some("1"), Some("2")]]);
        assert_eq!(render(&g, &compact()), "[{\"id\":\"1\",\"id_2\":\"2\"}]\n");
    }

    #[test]
    fn a_suffix_that_would_collide_moves_on() {
        let g = grid(&["id", "id", "id_2"], &[&[Some("1"), Some("2"), Some("3")]]);
        let out = render(&g, &compact());
        assert_eq!(
            out, "[{\"id\":\"1\",\"id_3\":\"2\",\"id_2\":\"3\"}]\n",
            "{out}"
        );
    }

    #[test]
    fn a_key_needing_escapes_gets_them() {
        let g = grid(&["a\"b"], &[&[Some("1")]]);
        assert!(render(&g, &compact()).contains("\"a\\\"b\":\"1\""));
    }

    #[test]
    fn pretty_output_is_indented() {
        let g = grid(&["id"], &[&[Some("1")], &[Some("2")]]);
        assert_eq!(
            render(&g, &Options::default()),
            "[\n  {\n    \"id\": \"1\"\n  },\n  {\n    \"id\": \"2\"\n  }\n]\n"
        );
    }

    #[test]
    fn ndjson_is_one_compact_object_per_line() {
        let g = grid(&["id"], &[&[Some("1")], &[Some("2")]]);
        let o = Options {
            pretty: true,
            ndjson: true,
        };
        assert_eq!(render(&g, &o), "{\"id\":\"1\"}\n{\"id\":\"2\"}\n");
    }

    #[test]
    fn an_empty_result_is_an_empty_array() {
        assert_eq!(render(&grid(&["a"], &[]), &Options::default()), "[]\n");
        assert_eq!(render(&grid(&["a"], &[]), &compact()), "[]\n");
    }

    #[test]
    fn an_empty_ndjson_result_is_nothing_at_all() {
        let o = Options {
            pretty: false,
            ndjson: true,
        };
        assert_eq!(render(&grid(&["a"], &[]), &o), "");
    }

    #[test]
    fn the_suggested_extension_follows_the_shape() {
        assert!(suggested_path(false).to_string_lossy().ends_with(".json"));
        assert!(suggested_path(true).to_string_lossy().ends_with(".ndjson"));
    }
}
