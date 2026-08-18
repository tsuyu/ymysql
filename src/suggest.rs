//! Composite index suggestions derived from the workload.
//!
//! Statement digests are already normalised by the server (`WHERE user_id = ?`),
//! which makes them parseable without a full SQL grammar. This module reads the
//! shape of a statement — which columns are compared for equality, which for a
//! range, what it orders by — and proposes the index that shape wants, checked
//! against the indexes that already exist.
//!
//! Everything here is a heuristic. A suggestion says "this shape usually wants
//! this index", never "this will be faster": the real outcome depends on data
//! distribution, cardinality, the optimiser's statistics and the rest of the
//! workload. That caveat is carried in the output, not just in this comment.

use std::collections::BTreeMap;

use crate::db::queries::{DigestRow, IndexDef};

/// How much a suggestion is likely to matter. An estimate, ranked from the
/// digest's own counters — never a measurement of the proposed index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Impact {
    Low,
    Medium,
    High,
}

impl Impact {
    pub fn label(self) -> &'static str {
        match self {
            Impact::Low => "Low",
            Impact::Medium => "Medium",
            Impact::High => "High",
        }
    }
}

/// A proposed index, with what asked for it.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSuggestion {
    pub schema: String,
    pub table: String,
    /// Columns in the order they should appear in the index.
    pub columns: Vec<String>,
    pub impact: Impact,
    /// Statements that would use it.
    pub queries: Vec<String>,
    /// Executions across those statements.
    pub executions: u64,
    /// Total time across those statements, milliseconds.
    pub total_ms: f64,
    /// Set when an existing index is a prefix of the proposal — extend that one
    /// rather than adding a second, overlapping index.
    pub extends: Option<String>,
}

impl IndexSuggestion {
    pub fn index_name(&self) -> String {
        let mut name = format!("idx_{}_{}", self.table, self.columns.join("_"));
        name.retain(|c| c.is_ascii_alphanumeric() || c == '_');
        name.truncate(64);
        name
    }

    /// The DDL to run. `ALGORITHM=INPLACE, LOCK=NONE` keeps the table writable
    /// while the index builds on 5.6+ InnoDB; the server rejects the statement
    /// rather than silently blocking if it cannot honour it.
    pub fn ddl(&self) -> String {
        let cols = self
            .columns
            .iter()
            .map(|c| format!("`{}`", c.replace('`', "``")))
            .collect::<Vec<_>>()
            .join(", ");

        match &self.extends {
            Some(existing) => format!(
                "-- extends the existing index `{existing}`\n\
                 ALTER TABLE `{}`.`{}`\n  DROP INDEX `{existing}`,\n  \
                 ADD INDEX `{}` ({cols}),\n  ALGORITHM=INPLACE, LOCK=NONE;",
                self.schema,
                self.table,
                self.index_name()
            ),
            None => format!(
                "ALTER TABLE `{}`.`{}`\n  ADD INDEX `{}` ({cols}),\n  \
                 ALGORITHM=INPLACE, LOCK=NONE;",
                self.schema,
                self.table,
                self.index_name()
            ),
        }
    }

    pub fn columns_display(&self) -> String {
        format!("INDEX({})", self.columns.join(", "))
    }
}

/// Statement shapes the parser understands enough to advise on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryShape {
    pub schema: Option<String>,
    pub table: String,
    /// Columns compared with `=`, `IN`, `IS NULL`.
    pub equality: Vec<String>,
    /// First column compared with a range operator. Only one can be used.
    pub range: Option<String>,
    pub order_by: Vec<String>,
    /// Columns wrapped in a function, e.g. `DATE(created_at) = ?`, which
    /// prevents any index on them from being used.
    pub wrapped_columns: Vec<String>,
}

impl QueryShape {
    /// Equality columns first, then one range column, then ORDER BY columns —
    /// only useful as a suffix when no range column precedes them.
    pub fn candidate_index(&self) -> Vec<String> {
        let mut cols: Vec<String> = Vec::new();
        for c in &self.equality {
            if !cols.iter().any(|x| eq(x, c)) {
                cols.push(c.clone());
            }
        }
        match &self.range {
            Some(r) => {
                if !cols.iter().any(|x| eq(x, r)) {
                    cols.push(r.clone());
                }
            }
            None => {
                for c in &self.order_by {
                    if !cols.iter().any(|x| eq(x, c)) {
                        cols.push(c.clone());
                    }
                }
            }
        }
        cols
    }
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

// ---------------------------------------------------------------- tokenizer

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// Backticked or bare identifier.
    Ident(String),
    /// Reserved word, upper-cased.
    Word(String),
    Op(String),
    Punct(char),
    /// `?` — a normalised literal.
    Placeholder,
    Other,
}

const KEYWORDS: [&str; 34] = [
    "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "IN", "IS", "NULL", "LIKE", "BETWEEN", "ORDER",
    "GROUP", "BY", "LIMIT", "HAVING", "JOIN", "INNER", "LEFT", "RIGHT", "OUTER", "CROSS", "ON",
    "UNION", "AS", "ASC", "DESC", "UPDATE", "DELETE", "INSERT", "SET", "VALUES", "FOR", "OFFSET",
];

fn tokenize(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }

        if c == '`' {
            let mut name = String::new();
            i += 1;
            while i < chars.len() {
                if chars[i] == '`' {
                    if chars.get(i + 1) == Some(&'`') {
                        name.push('`');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                name.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Ident(name));
            continue;
        }

        if c.is_ascii_alphabetic() || c == '_' {
            let mut word = String::new();
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                word.push(chars[i]);
                i += 1;
            }
            let upper = word.to_ascii_uppercase();
            if KEYWORDS.contains(&upper.as_str()) {
                out.push(Tok::Word(upper));
            } else {
                out.push(Tok::Ident(word));
            }
            continue;
        }

        if c == '?' {
            out.push(Tok::Placeholder);
            i += 1;
            continue;
        }

        // Operators, longest first.
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        let three: String = chars[i..(i + 3).min(chars.len())].iter().collect();
        if three == "<=>" {
            out.push(Tok::Op(three));
            i += 3;
            continue;
        }
        if matches!(two.as_str(), ">=" | "<=" | "<>" | "!=") {
            out.push(Tok::Op(two));
            i += 2;
            continue;
        }
        if matches!(c, '=' | '>' | '<') {
            out.push(Tok::Op(c.to_string()));
            i += 1;
            continue;
        }
        if matches!(c, '(' | ')' | ',' | '.' | ';' | '*') {
            out.push(Tok::Punct(c));
            i += 1;
            continue;
        }

        out.push(Tok::Other);
        i += 1;
    }
    out
}

fn ident_at(toks: &[Tok], i: usize) -> Option<&String> {
    match toks.get(i) {
        Some(Tok::Ident(s)) => Some(s),
        _ => None,
    }
}

fn is_word(toks: &[Tok], i: usize, w: &str) -> bool {
    matches!(toks.get(i), Some(Tok::Word(x)) if x == w)
}

/// Column reference, possibly qualified: `t`.`col` → `col`.
/// Returns the bare column name and the index just past it.
fn read_column(toks: &[Tok], mut i: usize) -> Option<(String, usize)> {
    let mut name = ident_at(toks, i)?.clone();
    i += 1;
    while matches!(toks.get(i), Some(Tok::Punct('.'))) {
        name = ident_at(toks, i + 1)?.clone();
        i += 2;
    }
    Some((name, i))
}

/// Parses the shape of a `SELECT`. Returns `None` for anything the heuristics
/// cannot reason about safely: joins, `OR`, subqueries, non-SELECT statements.
pub fn parse_shape(sql: &str) -> Option<QueryShape> {
    let toks = tokenize(sql);
    if !is_word(&toks, 0, "SELECT") {
        return None;
    }
    // Subqueries and derived tables need real cardinality reasoning.
    if toks
        .iter()
        .filter(|t| matches!(t, Tok::Word(w) if w == "SELECT"))
        .count()
        > 1
    {
        return None;
    }
    if toks
        .iter()
        .any(|t| matches!(t, Tok::Word(w) if w == "JOIN" || w == "UNION"))
    {
        return None;
    }

    let from = toks
        .iter()
        .position(|t| matches!(t, Tok::Word(w) if w == "FROM"))?;
    let (schema, table, mut i) = read_table(&toks, from + 1)?;

    // A comma in the FROM list is an implicit join.
    if matches!(toks.get(i), Some(Tok::Punct(','))) {
        return None;
    }
    // Skip an alias.
    if is_word(&toks, i, "AS") {
        i += 2;
    } else if matches!(toks.get(i), Some(Tok::Ident(_))) {
        i += 1;
    }
    if matches!(toks.get(i), Some(Tok::Punct(','))) {
        return None;
    }

    let mut shape = QueryShape {
        schema,
        table,
        ..Default::default()
    };

    if let Some(where_at) = toks
        .iter()
        .position(|t| matches!(t, Tok::Word(w) if w == "WHERE"))
    {
        let end = toks
            .iter()
            .skip(where_at)
            .position(|t| matches!(t, Tok::Word(w) if w == "ORDER" || w == "GROUP" || w == "LIMIT" || w == "HAVING"))
            .map(|p| p + where_at)
            .unwrap_or(toks.len());
        let clause = &toks[where_at + 1..end];
        // `OR` breaks the leftmost-prefix reasoning entirely.
        if clause
            .iter()
            .any(|t| matches!(t, Tok::Word(w) if w == "OR"))
        {
            return None;
        }
        parse_predicates(clause, &mut shape);
    }

    if let Some(order_at) = toks
        .iter()
        .position(|t| matches!(t, Tok::Word(w) if w == "ORDER"))
    {
        parse_order_by(&toks[order_at..], &mut shape);
    }

    if shape.table.is_empty() {
        return None;
    }
    Some(shape)
}

fn read_table(toks: &[Tok], i: usize) -> Option<(Option<String>, String, usize)> {
    let first = ident_at(toks, i)?.clone();
    if matches!(toks.get(i + 1), Some(Tok::Punct('.'))) {
        let table = ident_at(toks, i + 2)?.clone();
        Some((Some(first), table, i + 3))
    } else {
        Some((None, first, i + 1))
    }
}

fn parse_predicates(clause: &[Tok], shape: &mut QueryShape) {
    let mut i = 0;
    while i < clause.len() {
        // `FUNC(col)` — an index on col cannot be used through a function.
        if let Some(Tok::Ident(name)) = clause.get(i)
            && matches!(clause.get(i + 1), Some(Tok::Punct('(')))
        {
            if let Some((col, _)) = read_column(clause, i + 2)
                && !shape.wrapped_columns.iter().any(|c| eq(c, &col))
            {
                shape.wrapped_columns.push(col);
            }
            let _ = name;
            // Skip to the matching close paren.
            let mut depth = 0;
            while i < clause.len() {
                match clause.get(i) {
                    Some(Tok::Punct('(')) => depth += 1,
                    Some(Tok::Punct(')')) => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    None => break,
                    _ => {}
                }
                i += 1;
            }
            continue;
        }

        let Some((column, next)) = read_column(clause, i) else {
            i += 1;
            continue;
        };

        match clause.get(next) {
            Some(Tok::Op(op)) => {
                if op == "=" || op == "<=>" {
                    push_equality(shape, &column);
                } else if matches!(op.as_str(), ">" | "<" | ">=" | "<=") {
                    push_range(shape, &column);
                }
                i = next + 1;
            }
            Some(Tok::Word(w)) if w == "IN" => {
                push_equality(shape, &column);
                i = next + 1;
            }
            Some(Tok::Word(w)) if w == "BETWEEN" || w == "LIKE" => {
                push_range(shape, &column);
                i = next + 1;
            }
            Some(Tok::Word(w)) if w == "IS" => {
                push_equality(shape, &column);
                i = next + 1;
            }
            _ => i = next,
        }
    }
}

fn push_equality(shape: &mut QueryShape, column: &str) {
    if !shape.equality.iter().any(|c| eq(c, column)) {
        shape.equality.push(column.to_string());
    }
}

fn push_range(shape: &mut QueryShape, column: &str) {
    // Only the first range column can use the index; later ones are filters.
    if shape.range.is_none() && !shape.equality.iter().any(|c| eq(c, column)) {
        shape.range = Some(column.to_string());
    }
}

fn parse_order_by(toks: &[Tok], shape: &mut QueryShape) {
    // toks starts at ORDER; expect BY next.
    let mut i = if is_word(toks, 1, "BY") { 2 } else { 1 };
    while i < toks.len() {
        match toks.get(i) {
            Some(Tok::Word(w)) if w == "LIMIT" || w == "FOR" => break,
            Some(Tok::Word(w)) if w == "ASC" || w == "DESC" => i += 1,
            Some(Tok::Punct(',')) => i += 1,
            Some(Tok::Ident(_)) => {
                let Some((col, next)) = read_column(toks, i) else {
                    break;
                };
                if !shape.order_by.iter().any(|c| eq(c, &col)) {
                    shape.order_by.push(col);
                }
                i = next;
            }
            _ => break,
        }
    }
}

// ------------------------------------------------------------------ scoring

/// Ranks a digest's cost. Deliberately coarse: this orders suggestions, it does
/// not predict a speed-up.
pub fn estimate_impact(d: &DigestRow) -> Impact {
    let ratio = d.examined_per_sent();
    let scans = d.no_index_used > 0;

    if (scans && d.count >= 100) || ratio >= 1000.0 || d.total_ms >= 60_000.0 {
        Impact::High
    } else if scans || ratio >= 100.0 || d.count >= 100 || d.total_ms >= 5_000.0 {
        Impact::Medium
    } else {
        Impact::Low
    }
}

/// True when an existing index already serves `candidate` — i.e. the candidate
/// is a leftmost prefix of it.
fn covered_by(existing: &IndexDef, candidate: &[String]) -> bool {
    existing.columns.len() >= candidate.len()
        && existing
            .columns
            .iter()
            .zip(candidate)
            .all(|(a, b)| eq(a, b))
}

/// An existing index the candidate would extend: its columns are a proper
/// prefix of the candidate.
fn extends<'a>(existing: &'a [IndexDef], candidate: &[String]) -> Option<&'a IndexDef> {
    existing.iter().find(|ix| {
        !ix.columns.is_empty()
            && ix.columns.len() < candidate.len()
            && ix.columns.iter().zip(candidate).all(|(a, b)| eq(a, b))
    })
}

/// Suggestions for the whole workload, worst first, one per proposed index.
pub fn suggest(
    digests: &[DigestRow],
    indexes: &[IndexDef],
    min_executions: u64,
) -> Vec<IndexSuggestion> {
    let mut merged: BTreeMap<(String, String, String), IndexSuggestion> = BTreeMap::new();

    for d in digests {
        if d.count < min_executions {
            continue;
        }
        let Some(shape) = parse_shape(&d.text) else {
            continue;
        };
        let candidate = shape.candidate_index();
        if candidate.is_empty() {
            continue;
        }

        let schema = shape
            .schema
            .clone()
            .or_else(|| (!d.schema.is_empty()).then(|| d.schema.clone()))
            .unwrap_or_default();
        if schema.is_empty() {
            // Without a schema the DDL would be ambiguous.
            continue;
        }

        // Only weigh indexes that belong to this table.
        let table_indexes: Vec<IndexDef> = indexes
            .iter()
            .filter(|ix| eq(&ix.schema, &schema) && eq(&ix.table, &shape.table))
            .cloned()
            .collect();

        if table_indexes.iter().any(|ix| covered_by(ix, &candidate)) {
            continue;
        }

        let key = (
            schema.clone(),
            shape.table.clone(),
            candidate.join(",").to_ascii_lowercase(),
        );
        let impact = estimate_impact(d);
        let entry = merged.entry(key).or_insert_with(|| IndexSuggestion {
            schema: schema.clone(),
            table: shape.table.clone(),
            columns: candidate.clone(),
            impact,
            queries: Vec::new(),
            executions: 0,
            total_ms: 0.0,
            extends: extends(&table_indexes, &candidate).map(|ix| ix.index.clone()),
        });

        entry.impact = entry.impact.max(impact);
        entry.executions += d.count;
        entry.total_ms += d.total_ms;
        if entry.queries.len() < 3 {
            entry.queries.push(d.text.clone());
        }
    }

    let mut out: Vec<IndexSuggestion> = merged.into_values().collect();
    out.sort_by(|a, b| {
        b.impact.cmp(&a.impact).then(
            b.total_ms
                .partial_cmp(&a.total_ms)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(text: &str, count: u64) -> DigestRow {
        DigestRow {
            digest: format!("d{count}"),
            schema: "attendance".into(),
            text: text.into(),
            count,
            total_ms: 10_000.0,
            rows_examined: 1_000_000,
            rows_sent: 10,
            no_index_used: count,
            ..Default::default()
        }
    }

    fn index(table: &str, name: &str, cols: &[&str]) -> IndexDef {
        IndexDef {
            schema: "attendance".into(),
            table: table.into(),
            index: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            unique: false,
        }
    }

    #[test]
    fn equality_then_range_is_the_suggested_order() {
        let shape =
            parse_shape("SELECT * FROM `attendance` WHERE `user_id` = ? AND `checkin_date` >= ?")
                .unwrap();
        assert_eq!(shape.table, "attendance");
        assert_eq!(shape.equality, vec!["user_id"]);
        assert_eq!(shape.range.as_deref(), Some("checkin_date"));
        assert_eq!(shape.candidate_index(), vec!["user_id", "checkin_date"]);
    }

    #[test]
    fn range_column_never_precedes_an_equality_column() {
        let shape =
            parse_shape("SELECT * FROM t WHERE created_at > ? AND status = ? AND tenant_id = ?")
                .unwrap();
        assert_eq!(
            shape.candidate_index(),
            vec!["status", "tenant_id", "created_at"],
            "everything compared for equality has to come first"
        );
    }

    #[test]
    fn order_by_is_only_used_when_there_is_no_range() {
        let sorted =
            parse_shape("SELECT * FROM t WHERE tenant_id = ? ORDER BY created_at DESC").unwrap();
        assert_eq!(sorted.candidate_index(), vec!["tenant_id", "created_at"]);

        let ranged =
            parse_shape("SELECT * FROM t WHERE tenant_id = ? AND id > ? ORDER BY created_at")
                .unwrap();
        assert_eq!(
            ranged.candidate_index(),
            vec!["tenant_id", "id"],
            "a range column stops the index being useful for sorting"
        );
    }

    #[test]
    fn in_and_between_are_classified() {
        let s = parse_shape("SELECT * FROM t WHERE a IN (?) AND b BETWEEN ? AND ?").unwrap();
        assert_eq!(s.equality, vec!["a"]);
        assert_eq!(s.range.as_deref(), Some("b"));
    }

    #[test]
    fn qualified_columns_lose_the_alias() {
        let s = parse_shape("SELECT * FROM `demo`.`attendance` a WHERE `a`.`user_id` = ?").unwrap();
        assert_eq!(s.schema.as_deref(), Some("demo"));
        assert_eq!(s.table, "attendance");
        assert_eq!(s.equality, vec!["user_id"]);
    }

    #[test]
    fn shapes_we_cannot_reason_about_are_skipped() {
        assert!(parse_shape("SELECT * FROM a JOIN b ON a.id = b.a_id WHERE a.x = ?").is_none());
        assert!(parse_shape("SELECT * FROM t WHERE a = ? OR b = ?").is_none());
        assert!(parse_shape("SELECT * FROM t WHERE id IN (SELECT id FROM u)").is_none());
        assert!(parse_shape("SELECT * FROM a, b WHERE a.id = b.id").is_none());
        assert!(parse_shape("UPDATE t SET a = ? WHERE b = ?").is_none());
    }

    #[test]
    fn function_wrapped_columns_are_recorded() {
        let s = parse_shape("SELECT * FROM t WHERE DATE(`created_at`) = ? AND `x` = ?").unwrap();
        assert_eq!(s.wrapped_columns, vec!["created_at"]);
        assert_eq!(
            s.equality,
            vec!["x"],
            "the wrapped column cannot use an index"
        );
    }

    #[test]
    fn suggests_the_composite_index_for_the_example_workload() {
        let digests = vec![digest(
            "SELECT * FROM `attendance` WHERE `user_id` = ? AND `checkin_date` >= ?",
            500,
        )];
        let out = suggest(&digests, &[], 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].columns, vec!["user_id", "checkin_date"]);
        assert_eq!(out[0].impact, Impact::High);
        assert_eq!(out[0].columns_display(), "INDEX(user_id, checkin_date)");
        assert!(
            out[0]
                .ddl()
                .contains("ADD INDEX `idx_attendance_user_id_checkin_date`")
        );
        assert!(out[0].ddl().contains("ALGORITHM=INPLACE, LOCK=NONE"));
    }

    #[test]
    fn an_existing_prefix_index_suppresses_the_suggestion() {
        let digests = vec![digest("SELECT * FROM `t` WHERE `a` = ? AND `b` >= ?", 500)];
        let covering = vec![index("t", "idx_a_b", &["a", "b"])];
        assert!(suggest(&digests, &covering, 10).is_empty());

        let wider = vec![index("t", "idx_a_b_c", &["a", "b", "c"])];
        assert!(
            suggest(&digests, &wider, 10).is_empty(),
            "a leftmost prefix of a wider index already serves the query"
        );
    }

    #[test]
    fn a_narrower_index_is_extended_rather_than_duplicated() {
        let digests = vec![digest("SELECT * FROM `t` WHERE `a` = ? AND `b` >= ?", 500)];
        let narrow = vec![index("t", "idx_a", &["a"])];
        let out = suggest(&digests, &narrow, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].extends.as_deref(), Some("idx_a"));
        let ddl = out[0].ddl();
        assert!(ddl.contains("DROP INDEX `idx_a`"), "{ddl}");
        assert!(ddl.contains("ADD INDEX"), "{ddl}");
    }

    #[test]
    fn identical_shapes_merge_into_one_suggestion() {
        let digests = vec![
            digest("SELECT * FROM `t` WHERE `a` = ? AND `b` >= ?", 100),
            digest("SELECT `x` FROM `t` WHERE `a` = ? AND `b` > ?", 300),
        ];
        let out = suggest(&digests, &[], 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].executions, 400);
        assert_eq!(out[0].queries.len(), 2);
    }

    #[test]
    fn rare_statements_are_ignored() {
        let digests = vec![digest("SELECT * FROM `t` WHERE `a` = ?", 3)];
        assert!(suggest(&digests, &[], 10).is_empty());
    }

    #[test]
    fn impact_is_ranked_from_the_digest_counters() {
        let mut cheap = digest("SELECT * FROM t WHERE a = ?", 5);
        cheap.no_index_used = 0;
        cheap.rows_examined = 10;
        cheap.rows_sent = 10;
        cheap.total_ms = 5.0;
        cheap.count = 5;
        assert_eq!(estimate_impact(&cheap), Impact::Low);

        let mut heavy = cheap.clone();
        heavy.rows_examined = 5_000_000;
        heavy.rows_sent = 1;
        assert_eq!(estimate_impact(&heavy), Impact::High);
    }

    #[test]
    fn index_name_is_a_legal_identifier() {
        let s = IndexSuggestion {
            schema: "d".into(),
            table: "we`ird".into(),
            columns: vec!["a-b".into(), "c".into()],
            impact: Impact::Low,
            queries: vec![],
            executions: 0,
            total_ms: 0.0,
            extends: None,
        };
        let name = s.index_name();
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(name.len() <= 64);
        assert!(s.ddl().contains("`a-b`"), "column names stay quoted");
    }
}
