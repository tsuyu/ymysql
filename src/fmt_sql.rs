//! SQL pretty-printer for the console.
//!
//! The contract is narrow on purpose: this changes **whitespace and the case
//! of reserved words, nothing else**. The lexer keeps string literals, quoted
//! identifiers, numbers and comments byte-for-byte, and the formatter never
//! reorders, adds or drops a token.
//!
//! Only *reserved* words are upper-cased. A reserved word cannot be a bare
//! identifier in MySQL, so re-casing one can never rename a table — which
//! matters, because table names are case-sensitive on Linux while column names
//! never are.

/// One level of indent.
const INDENT: &str = "  ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Quoted,
    Str,
    Num,
    Param,
    Punct,
    LineComment,
    BlockComment,
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    text: String,
    /// Upper-cased `text` for `Word` tokens, empty otherwise.
    upper: String,
}

/// Reserved words, upper-cased on output. Kept to MySQL's reserved list so a
/// bare identifier can never appear here.
#[rustfmt::skip]
const RESERVED: [&str; 112] = [
    "ADD", "ALL", "ALTER", "ANALYZE", "AND", "AS", "ASC", "BETWEEN", "BOTH", "BY", "CASCADE",
    "CASE", "CHANGE", "CHECK", "COLLATE", "COLUMN", "CONSTRAINT", "CONVERT", "CREATE", "CROSS",
    "CURRENT_DATE", "CURRENT_TIME", "CURRENT_TIMESTAMP", "CURRENT_USER", "DATABASE", "DEFAULT",
    "DELETE", "DESC", "DESCRIBE", "DISTINCT", "DIV", "DROP", "DUAL", "EACH", "ELSE", "ELSEIF",
    "EXISTS", "EXPLAIN", "FALSE", "FOR", "FORCE", "FOREIGN", "FROM", "FULLTEXT", "GRANT", "GROUP",
    "HAVING", "IF", "IGNORE", "IN", "INDEX", "INNER", "INSERT", "INTERVAL", "INTO", "IS", "JOIN",
    "KEY", "KEYS", "KILL", "LEADING", "LEFT", "LIKE", "LIMIT", "LOCK", "MATCH", "MOD", "NATURAL",
    "NOT", "NULL", "ON", "OPTIMIZE", "OR", "ORDER", "OUTER", "PRIMARY", "PROCEDURE", "RANGE",
    "READ", "REFERENCES", "REGEXP", "RENAME", "REPLACE", "REQUIRE", "RESTRICT", "REVOKE", "RIGHT",
    "RLIKE", "SCHEMA", "SELECT", "SET", "SHOW", "STRAIGHT_JOIN", "TABLE", "THEN", "TO", "TRAILING",
    "TRIGGER", "TRUE", "UNION", "UNIQUE", "UNLOCK", "UPDATE", "USAGE", "USE", "USING", "VALUES",
    "WHEN", "WHERE", "WITH", "WRITE", "XOR",
];

fn is_reserved(upper: &str) -> bool {
    RESERVED.binary_search(&upper).is_ok()
}

/// A word sitting directly in front of `(` is a function or procedure call.
/// Routine names are case-insensitive in MySQL even where table names are not,
/// so upper-casing one is safe.
fn is_call(toks: &[Token], i: usize) -> bool {
    toks[i].kind == Kind::Word
        && toks
            .get(i + 1)
            .is_some_and(|n| n.kind == Kind::Punct && n.text == "(")
}

/// Clause heads. Each starts a fresh line at the current base indent.
/// Longer phrases come first so `UNION ALL` wins over `UNION`.
const TOP_LEVEL: &[&[&str]] = &[
    &["ON", "DUPLICATE", "KEY", "UPDATE"],
    &["INSERT", "IGNORE", "INTO"],
    &["GROUP", "BY"],
    &["ORDER", "BY"],
    &["PARTITION", "BY"],
    &["UNION", "ALL"],
    &["UNION", "DISTINCT"],
    &["INSERT", "INTO"],
    &["DELETE", "FROM"],
    &["REPLACE", "INTO"],
    &["SELECT"],
    &["FROM"],
    &["WHERE"],
    &["HAVING"],
    &["LIMIT"],
    &["OFFSET"],
    &["UNION"],
    &["VALUES"],
    &["UPDATE"],
    &["SET"],
    &["INSERT"],
    &["DELETE"],
    &["RETURNING"],
];

/// Join heads. One level in from the clause they hang off.
const JOINS: &[&[&str]] = &[
    &["LEFT", "OUTER", "JOIN"],
    &["RIGHT", "OUTER", "JOIN"],
    &["FULL", "OUTER", "JOIN"],
    &["NATURAL", "LEFT", "JOIN"],
    &["NATURAL", "RIGHT", "JOIN"],
    &["LEFT", "JOIN"],
    &["RIGHT", "JOIN"],
    &["INNER", "JOIN"],
    &["CROSS", "JOIN"],
    &["NATURAL", "JOIN"],
    &["STRAIGHT_JOIN"],
    &["JOIN"],
];

/// Which clause we are inside, for the comma-per-line decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    None,
    /// A list worth exploding one item per line.
    List,
    Other,
}

// ---------------------------------------------------------------- lexer

fn lex(sql: &str) -> Vec<Token> {
    let c: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let kind;

        // MySQL only treats `--` as a comment when whitespace follows it,
        // which keeps `a--b` as a double negation.
        if ch == '#'
            || (ch == '-'
                && c.get(i + 1) == Some(&'-')
                && c.get(i + 2).is_none_or(|n| n.is_whitespace()))
        {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            kind = Kind::LineComment;
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(c.len());
            kind = Kind::BlockComment;
        } else if ch == '\'' || ch == '"' {
            i = scan_delimited(&c, i, ch, true);
            kind = Kind::Str;
        } else if ch == '`' {
            i = scan_delimited(&c, i, '`', false);
            kind = Kind::Quoted;
        } else if ch == '0' && matches!(c.get(i + 1), Some('x') | Some('X')) {
            i += 2;
            while i < c.len() && c[i].is_ascii_hexdigit() {
                i += 1;
            }
            kind = Kind::Num;
        } else if ch.is_ascii_digit()
            || (ch == '.' && c.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            while i < c.len() {
                if (c[i] == 'e' || c[i] == 'E') && matches!(c.get(i + 1), Some('+') | Some('-')) {
                    i += 2;
                    continue;
                }
                if c[i].is_ascii_alphanumeric() || c[i] == '.' {
                    i += 1;
                    continue;
                }
                break;
            }
            kind = Kind::Num;
        } else if ch.is_alphabetic() || ch == '_' || ch == '$' {
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                i += 1;
            }
            kind = Kind::Word;
        } else if ch == '?' {
            i += 1;
            kind = Kind::Param;
        } else if ch == '@' {
            i += 1;
            if c.get(i) == Some(&'@') {
                i += 1;
            }
            while i < c.len() && (c[i].is_alphanumeric() || matches!(c[i], '_' | '.' | '$')) {
                i += 1;
            }
            kind = Kind::Param;
        } else {
            let three: String = c[i..(i + 3).min(c.len())].iter().collect();
            let two: String = c[i..(i + 2).min(c.len())].iter().collect();
            if three == "<=>" {
                i += 3;
            } else if matches!(
                two.as_str(),
                "<<" | ">>" | "<=" | ">=" | "<>" | "!=" | ":=" | "&&" | "||" | "->"
            ) {
                i += 2;
            } else {
                i += 1;
            }
            kind = Kind::Punct;
        }

        let text: String = c[start..i].iter().collect();
        let upper = if kind == Kind::Word {
            text.to_ascii_uppercase()
        } else {
            String::new()
        };
        out.push(Token { kind, text, upper });
    }
    out
}

/// Consumes a quoted run starting at `i`, returning the index just past it.
/// Doubling the delimiter escapes it; `backslash` also enables `\'`.
fn scan_delimited(c: &[char], mut i: usize, delim: char, backslash: bool) -> usize {
    i += 1;
    while i < c.len() {
        if backslash && c[i] == '\\' {
            i += 2;
            continue;
        }
        if c[i] == delim {
            if c.get(i + 1) == Some(&delim) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    c.len()
}

// ---------------------------------------------------------------- matching

fn match_phrase(toks: &[Token], i: usize, phrase: &[&str]) -> bool {
    phrase.iter().enumerate().all(|(k, w)| {
        toks.get(i + k)
            .is_some_and(|t| t.kind == Kind::Word && t.upper == *w)
    })
}

fn match_any<'a>(toks: &[Token], i: usize, table: &[&'a [&'a str]]) -> Option<&'a [&'a str]> {
    table.iter().copied().find(|p| match_phrase(toks, i, p))
}

/// True when the previous word demands an identifier next, so a word that
/// merely *looks* like a clause head (a table actually named `offset`) is not
/// treated as one.
fn ident_expected(prev: &str, prev_text: &str) -> bool {
    prev_text == "." || matches!(prev, "FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE" | "AS")
}

/// Does the clause starting at `i` hold a top-level comma before its next
/// clause head? A single-item list stays on one line.
fn list_has_comma(toks: &[Token], mut i: usize) -> bool {
    let mut depth = 0i32;
    while i < toks.len() {
        let t = &toks[i];
        match t.kind {
            Kind::Punct if t.text == "(" => depth += 1,
            Kind::Punct if t.text == ")" => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            Kind::Punct if t.text == "," && depth == 0 => return true,
            Kind::Punct if t.text == ";" => return false,
            Kind::Word if depth == 0 => {
                if match_any(toks, i, TOP_LEVEL).is_some() || match_any(toks, i, JOINS).is_some() {
                    return false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

// ---------------------------------------------------------------- emitter

#[derive(Default)]
struct Out {
    buf: String,
    indent: usize,
    /// The next token starts a fresh line.
    fresh: bool,
}

impl Out {
    fn newline(&mut self) {
        if !self.buf.is_empty() {
            self.fresh = true;
        }
    }

    fn blank_line(&mut self) {
        if !self.buf.is_empty() {
            self.buf.push('\n');
            self.fresh = true;
        }
    }

    fn push(&mut self, text: &str, space_before: bool) {
        if self.fresh {
            if !self.buf.is_empty() {
                self.buf.push('\n');
            }
            for _ in 0..self.indent {
                self.buf.push_str(INDENT);
            }
            self.fresh = false;
        } else if space_before && !self.buf.is_empty() && !self.buf.ends_with(' ') {
            self.buf.push(' ');
        }
        self.buf.push_str(text);
    }
}

/// An open parenthesis and what to restore when it closes.
struct Frame {
    indent: usize,
    /// The paren was expanded across lines (a subquery), rather than inlined.
    block: bool,
    clause: Clause,
}

/// Reformats `sql`. Input that lexes to nothing comes back trimmed.
pub fn format(sql: &str) -> String {
    let toks = lex(sql);
    if toks.is_empty() {
        return sql.trim().to_string();
    }

    let mut out = Out::default();
    let mut parens: Vec<Frame> = Vec::new();
    let mut cases: Vec<usize> = Vec::new();
    let mut clause = Clause::None;

    let mut prev_upper = String::new();
    let mut prev_text = String::new();
    let mut prev_kind: Option<Kind> = None;
    // Set after a unary sign so the operand stays glued to it.
    let mut unary = false;
    // The previous token sat in a table position, so a `(` after it opens a
    // column list rather than an argument list.
    let mut prev_is_table = false;

    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        // Indent that clause heads return to inside the innermost subquery.
        let base = parens
            .iter()
            .rev()
            .find(|f| f.block)
            .map_or(0, |f| f.indent + 1);

        match t.kind {
            Kind::LineComment => {
                out.push(&t.text, true);
                out.fresh = true;
                i += 1;
                continue;
            }
            Kind::BlockComment => {
                out.push(&t.text, !prev_text.is_empty());
                advance(&mut prev_upper, &mut prev_text, &mut prev_kind, t);
                unary = false;
                prev_is_table = false;
                i += 1;
                continue;
            }
            _ => {}
        }

        if t.kind == Kind::Punct {
            match t.text.as_str() {
                ";" => {
                    out.push(";", false);
                    out.indent = 0;
                    parens.clear();
                    cases.clear();
                    clause = Clause::None;
                    out.blank_line();
                    advance(&mut prev_upper, &mut prev_text, &mut prev_kind, t);
                    unary = false;
                    prev_is_table = false;
                    i += 1;
                    continue;
                }
                "(" => {
                    // Only a paren wrapping a query gets its own block.
                    let block = toks.get(i + 1).is_some_and(|n| {
                        n.kind == Kind::Word && matches!(n.upper.as_str(), "SELECT" | "WITH")
                    });
                    // `count(` is a call and stays glued; `IN (` and the
                    // column list in `INSERT INTO t (a, b)` are not.
                    let glued = matches!(prev_kind, Some(Kind::Word))
                        && !is_reserved(&prev_upper)
                        && !prev_is_table;
                    out.push(
                        "(",
                        !glued && !prev_text.is_empty() && prev_text != "(" && !unary,
                    );
                    parens.push(Frame {
                        indent: out.indent,
                        block,
                        clause,
                    });
                    if block {
                        out.indent += 1;
                        out.newline();
                    }
                    clause = Clause::None;
                    advance(&mut prev_upper, &mut prev_text, &mut prev_kind, t);
                    unary = false;
                    prev_is_table = false;
                    i += 1;
                    continue;
                }
                ")" => {
                    if let Some(f) = parens.pop() {
                        out.indent = f.indent;
                        if f.block {
                            out.newline();
                        }
                        clause = f.clause;
                    }
                    out.push(")", false);
                    advance(&mut prev_upper, &mut prev_text, &mut prev_kind, t);
                    unary = false;
                    prev_is_table = false;
                    i += 1;
                    continue;
                }
                "," => {
                    out.push(",", false);
                    let breakable = parens.last().is_none_or(|f| f.block);
                    if breakable && clause == Clause::List {
                        out.newline();
                    }
                    advance(&mut prev_upper, &mut prev_text, &mut prev_kind, t);
                    unary = false;
                    prev_is_table = false;
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }

        if t.kind == Kind::Word && !ident_expected(&prev_upper, &prev_text) {
            if let Some(p) = match_any(&toks, i, TOP_LEVEL) {
                out.indent = base;
                out.newline();
                emit_words(&mut out, &toks, i, p.len());
                i += p.len();
                let head = p[0];
                // SELECT and SET read best one item per line, but only when
                // there is more than one item.
                clause = if matches!(head, "SELECT" | "SET") && list_has_comma(&toks, i) {
                    out.indent = base + 1;
                    out.newline();
                    Clause::List
                } else {
                    Clause::Other
                };
                set_prev(
                    &mut prev_upper,
                    &mut prev_text,
                    &mut prev_kind,
                    p[p.len() - 1],
                );
                unary = false;
                prev_is_table = false;
                continue;
            }
            if let Some(p) = match_any(&toks, i, JOINS) {
                out.indent = base + 1;
                out.newline();
                emit_words(&mut out, &toks, i, p.len());
                i += p.len();
                clause = Clause::Other;
                set_prev(
                    &mut prev_upper,
                    &mut prev_text,
                    &mut prev_kind,
                    p[p.len() - 1],
                );
                unary = false;
                prev_is_table = false;
                continue;
            }
            if matches!(t.upper.as_str(), "AND" | "OR" | "XOR")
                && parens.last().is_none_or(|f| f.block)
            {
                out.indent = base + 1;
                out.newline();
                out.push(&t.upper, false);
                advance(&mut prev_upper, &mut prev_text, &mut prev_kind, t);
                unary = false;
                prev_is_table = false;
                i += 1;
                continue;
            }
            if t.upper == "CASE" {
                out.push("CASE", spaced(&prev_text, unary));
                // Keep the body clear of the clause column, so `END` never
                // lines up with `FROM`.
                let body = out.indent.max(base + 1);
                cases.push(body);
                out.indent = body + 1;
                advance(&mut prev_upper, &mut prev_text, &mut prev_kind, t);
                unary = false;
                prev_is_table = false;
                i += 1;
                continue;
            }
            if matches!(t.upper.as_str(), "WHEN" | "ELSE") && !cases.is_empty() {
                out.newline();
                out.push(&t.upper, false);
                advance(&mut prev_upper, &mut prev_text, &mut prev_kind, t);
                unary = false;
                prev_is_table = false;
                i += 1;
                continue;
            }
            if t.upper == "END"
                && let Some(ind) = cases.pop()
            {
                out.indent = ind;
                out.newline();
                out.push("END", false);
                advance(&mut prev_upper, &mut prev_text, &mut prev_kind, t);
                unary = false;
                prev_is_table = false;
                i += 1;
                continue;
            }
        }

        // Ordinary token.
        let table_position = t.kind != Kind::Punct && ident_expected(&prev_upper, &prev_text);
        // A word in a table position keeps its case even when a `(` follows
        // it -- that is a column list, and table names are case-sensitive.
        let text = if t.kind == Kind::Word
            && (is_reserved(&t.upper) || (is_call(&toks, i) && !table_position))
        {
            t.upper.clone()
        } else {
            t.text.clone()
        };
        let no_space = matches!(t.text.as_str(), "." | "," | ")" | ";")
            || prev_text == "."
            || prev_text == "("
            || unary
            || prev_text.is_empty();
        out.push(&text, !no_space);

        // A sign directly after an operator, an open paren or a comma is unary.
        unary = t.kind == Kind::Punct
            && matches!(t.text.as_str(), "-" | "+" | "~" | "!")
            && (prev_text.is_empty()
                || prev_text == "("
                || prev_text == ","
                || matches!(prev_kind, Some(Kind::Punct)) && prev_text != ")");
        prev_is_table = table_position;
        advance(&mut prev_upper, &mut prev_text, &mut prev_kind, t);
        i += 1;
    }

    out.buf.trim_end().to_string()
}

fn spaced(prev_text: &str, unary: bool) -> bool {
    !prev_text.is_empty() && prev_text != "(" && prev_text != "." && !unary
}

/// Emits `n` words of a matched phrase, upper-cased, on the current line.
fn emit_words(out: &mut Out, toks: &[Token], i: usize, n: usize) {
    for k in 0..n {
        out.push(&toks[i + k].upper, k > 0);
    }
}

fn advance(
    prev_upper: &mut String,
    prev_text: &mut String,
    prev_kind: &mut Option<Kind>,
    t: &Token,
) {
    prev_upper.clone_from(&t.upper);
    prev_text.clone_from(&t.text);
    *prev_kind = Some(t.kind);
}

fn set_prev(
    prev_upper: &mut String,
    prev_text: &mut String,
    prev_kind: &mut Option<Kind>,
    word: &str,
) {
    prev_upper.clear();
    prev_upper.push_str(word);
    prev_text.clear();
    prev_text.push_str(word);
    *prev_kind = Some(Kind::Word);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The safety net: formatting may only change whitespace and the case of
    /// words. Every other token must survive byte-for-byte, in order.
    fn assert_token_preserving(src: &str) {
        let before = lex(src);
        let after = lex(&format(src));
        assert_eq!(
            before.len(),
            after.len(),
            "token count changed\nin:  {src}\nout: {}",
            format(src)
        );
        for (a, b) in before.iter().zip(after.iter()) {
            assert_eq!(a.kind, b.kind, "kind changed: {:?} -> {:?}", a, b);
            if a.kind == Kind::Word {
                assert_eq!(a.upper, b.upper, "word changed: {} -> {}", a.text, b.text);
            } else {
                assert_eq!(a.text, b.text, "literal changed");
            }
        }
    }

    #[test]
    fn reserved_list_is_sorted_for_binary_search() {
        let mut sorted = RESERVED;
        sorted.sort_unstable();
        assert_eq!(RESERVED, sorted);
    }

    #[test]
    fn breaks_clauses_onto_their_own_lines() {
        let got = format("select a, b from t where x = 1 and y = 2 order by a desc limit 10");
        assert_eq!(
            got,
            "SELECT\n  a,\n  b\nFROM t\nWHERE x = 1\n  AND y = 2\nORDER BY a DESC\nLIMIT 10"
        );
    }

    #[test]
    fn single_item_select_stays_on_one_line() {
        assert_eq!(format("select 1"), "SELECT 1");
        assert_eq!(format("select count(*) from t"), "SELECT COUNT(*)\nFROM t");
    }

    #[test]
    fn joins_indent_under_from() {
        let got = format("SELECT u.id FROM users u LEFT JOIN orders o ON o.user_id = u.id");
        assert_eq!(
            got,
            "SELECT u.id\nFROM users u\n  LEFT JOIN orders o ON o.user_id = u.id"
        );
    }

    #[test]
    fn subquery_gets_its_own_block() {
        let got = format("select * from (select id from t) x");
        assert_eq!(got, "SELECT *\nFROM (\n  SELECT id\n  FROM t\n) x");
    }

    #[test]
    fn function_call_parens_stay_inline() {
        assert_eq!(
            format("select coalesce(a, b, c) from t"),
            "SELECT COALESCE(a, b, c)\nFROM t"
        );
    }

    #[test]
    fn in_list_keeps_a_space_before_the_paren() {
        assert_eq!(
            format("select a from t where id in (1,2,3)"),
            "SELECT a\nFROM t\nWHERE id IN (1, 2, 3)"
        );
    }

    #[test]
    fn string_literals_are_untouched() {
        let src = "select 'from  WHERE', \"a, b\" from t";
        let got = format(src);
        assert!(got.contains("'from  WHERE'"), "{got}");
        assert!(got.contains("\"a, b\""), "{got}");
        assert_token_preserving(src);
    }

    #[test]
    fn quoted_identifiers_keep_their_case() {
        let src = "select `MyCol` from `MyTable`";
        assert_eq!(format(src), "SELECT `MyCol`\nFROM `MyTable`");
    }

    #[test]
    fn a_table_named_like_a_clause_word_is_not_a_clause() {
        // `offset` is not reserved, so it can legally be a table.
        assert_eq!(format("select a from offset"), "SELECT a\nFROM offset");
    }

    #[test]
    fn non_reserved_words_keep_their_case() {
        // Table names are case-sensitive on Linux; only reserved words move.
        assert_eq!(
            format("select Total from Orders"),
            "SELECT Total\nFROM Orders"
        );
    }

    #[test]
    fn statements_are_separated_by_a_blank_line() {
        assert_eq!(format("select 1; select 2;"), "SELECT 1;\n\nSELECT 2;");
    }

    #[test]
    fn comments_survive() {
        let src = "-- lead\nselect a -- trailing\nfrom t /* block */";
        let got = format(src);
        assert!(got.contains("-- lead"), "{got}");
        assert!(got.contains("-- trailing"), "{got}");
        assert!(got.contains("/* block */"), "{got}");
        assert_token_preserving(src);
    }

    #[test]
    fn a_dash_dash_without_space_is_not_a_comment() {
        let toks = lex("select 1--2");
        assert_eq!(toks.len(), 5, "{toks:?}");
        assert!(toks.iter().all(|t| t.kind != Kind::LineComment));
    }

    #[test]
    fn case_expression_indents() {
        let got = format("select case when a = 1 then 'x' else 'y' end from t");
        assert_eq!(
            got,
            "SELECT CASE\n    WHEN a = 1 THEN 'x'\n    ELSE 'y'\n  END\nFROM t"
        );
    }

    #[test]
    fn update_set_list_breaks() {
        let got = format("update t set a = 1, b = 2 where id = 3");
        assert_eq!(got, "UPDATE t\nSET\n  a = 1,\n  b = 2\nWHERE id = 3");
    }

    #[test]
    fn insert_values_stay_inline() {
        let got = format("insert into t (a, b) values (1, 'x')");
        assert_eq!(got, "INSERT INTO t (a, b)\nVALUES (1, 'x')");
    }

    #[test]
    fn placeholders_and_variables_survive() {
        let src = "select * from t where id = ? and u = @user";
        assert_token_preserving(src);
        assert!(format(src).contains("id = ?"));
    }

    #[test]
    fn formatting_is_idempotent() {
        for src in [
            "select a, b from t where x = 1 and y = 2",
            "select * from (select id from t) x order by id",
            "update t set a = 1 where id = 2",
            "select case when a then 1 else 2 end from t",
            "insert into t (a, b) values (1, 2)",
        ] {
            let once = format(src);
            assert_eq!(once, format(&once), "not idempotent: {src}");
        }
    }

    #[test]
    fn never_loses_a_token() {
        for src in [
            "SELECT * FROM t",
            "select a,b,c from t join u on t.id=u.id where a in (1,2) group by a having count(*)>1",
            "delete from t where id = 1",
            "insert into t values (-1, +2, 3e-4, 0xFF)",
            "select 'it''s', `we``ird`, \"q\\\"uote\" from t",
            "explain select 1",
            "select a from t /* c1 */ where b = 1 # c2\n",
            "",
            "   ",
            "select (select max(x) from u) from t",
        ] {
            assert_token_preserving(src);
        }
    }

    #[test]
    fn empty_input_comes_back_empty() {
        assert_eq!(format(""), "");
        assert_eq!(format("   \n  "), "");
        assert_eq!(format("-- only a comment"), "-- only a comment");
    }
}
