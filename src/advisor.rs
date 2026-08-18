//! Index advisor. Pure analysis over what `db::queries` collected, so it is
//! version-independent and unit-testable without a server.

use crate::db::queries::{DigestRow, IndexDef, IndexUsage, NoPkTable, ScanRow};
use crate::db::version::Capabilities;
use crate::suggest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    High,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::High => "high",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// Short category, e.g. `duplicate index`, `unused index`, `full scan`.
    pub kind: &'static str,
    /// What it is about — `schema.table` or `schema.table.index`.
    pub object: String,
    pub detail: String,
    /// Copy-pasteable remediation, when there is a safe one.
    pub action: Option<String>,
}

/// Below this uptime the performance_schema counters are too young to call an
/// index unused.
pub const MIN_UPTIME_FOR_UNUSED_S: u64 = 7 * 24 * 3600;

/// Ratio of rows examined per row returned above which a statement is flagged.
const EXAMINED_RATIO_LIMIT: f64 = 100.0;
/// Ignore statements that have barely run.
const MIN_EXEC_COUNT: u64 = 10;

pub struct AdvisorInput<'a> {
    pub indexes: &'a [IndexDef],
    pub usage: &'a [IndexUsage],
    pub scans: &'a [ScanRow],
    pub no_pk: &'a [NoPkTable],
    pub digests: &'a [DigestRow],
    pub caps: Capabilities,
    pub uptime_s: u64,
}

pub fn analyze(input: &AdvisorInput<'_>) -> Vec<Finding> {
    let mut out = Vec::new();
    redundant_indexes(input, &mut out);
    unused_indexes(input, &mut out);
    full_scans(input, &mut out);
    missing_pk(input, &mut out);
    statement_findings(input, &mut out);
    unindexable_predicates(input, &mut out);
    out.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.object.cmp(&b.object)));
    out
}

/// `DROP INDEX`, preceded by an INVISIBLE step where the server supports it —
/// making an index invisible is instant and reversible, dropping it is not.
fn drop_index_action(caps: &Capabilities, ix: &IndexDef) -> String {
    let (s, t, i) = (&ix.schema, &ix.table, &ix.index);
    if caps.invisible_index {
        format!(
            "ALTER TABLE `{s}`.`{t}` ALTER INDEX `{i}` INVISIBLE;\n\
             -- watch for regressions, then:\n\
             ALTER TABLE `{s}`.`{t}` DROP INDEX `{i}`;"
        )
    } else {
        format!("ALTER TABLE `{s}`.`{t}` DROP INDEX `{i}`;")
    }
}

/// An index is redundant when another index on the same table starts with the
/// same columns in the same order. A unique index is never redundant against a
/// non-unique one: it also enforces a constraint.
fn redundant_indexes(input: &AdvisorInput<'_>, out: &mut Vec<Finding>) {
    for a in input.indexes {
        if a.index.eq_ignore_ascii_case("PRIMARY") {
            continue;
        }
        for b in input.indexes {
            if a.table != b.table || a.schema != b.schema || a.index == b.index {
                continue;
            }
            if a.columns.len() > b.columns.len() {
                continue;
            }
            if a.columns.len() == b.columns.len() && a.index > b.index {
                // Identical pair: report it once, from the lower-named side.
                continue;
            }
            if !b.columns.starts_with(&a.columns) {
                continue;
            }
            if a.unique && !b.unique {
                continue;
            }

            let dup = a.columns.len() == b.columns.len();
            out.push(Finding {
                severity: if dup { Severity::High } else { Severity::Warn },
                kind: if dup {
                    "duplicate index"
                } else {
                    "redundant index"
                },
                object: format!("{}.{}", a.qualified(), a.index),
                detail: format!(
                    "({}) is a prefix of `{}` ({}) — every write maintains both",
                    a.columns.join(", "),
                    b.index,
                    b.columns.join(", ")
                ),
                action: Some(drop_index_action(&input.caps, a)),
            });
            break;
        }
    }
}

fn unused_indexes(input: &AdvisorInput<'_>, out: &mut Vec<Finding>) {
    if input.uptime_s < MIN_UPTIME_FOR_UNUSED_S {
        return;
    }
    for u in input.usage {
        if u.reads > 0 || u.index.eq_ignore_ascii_case("PRIMARY") {
            continue;
        }
        // A unique index may exist purely to enforce a constraint.
        let def = input
            .indexes
            .iter()
            .find(|i| i.schema == u.schema && i.table == u.table && i.index == u.index);
        if def.map(|d| d.unique).unwrap_or(false) {
            continue;
        }

        out.push(Finding {
            severity: Severity::Warn,
            kind: "unused index",
            object: format!("{}.{}.{}", u.schema, u.table, u.index),
            detail: format!(
                "0 reads and {} writes since the counters were reset ({} of uptime)",
                u.writes,
                crate::ui::fmt_duration(input.uptime_s)
            ),
            action: def.map(|d| drop_index_action(&input.caps, d)),
        });
    }
}

fn full_scans(input: &AdvisorInput<'_>, out: &mut Vec<Finding>) {
    for s in input.scans {
        let sev = if s.table_rows > 10_000 {
            Severity::High
        } else {
            Severity::Info
        };
        out.push(Finding {
            severity: sev,
            kind: "full table scan",
            object: format!("{}.{}", s.schema, s.table),
            detail: format!(
                "{} reads with no index; table holds ~{} rows",
                s.rows_full_scanned, s.table_rows
            ),
            action: None,
        });
    }
}

fn missing_pk(input: &AdvisorInput<'_>, out: &mut Vec<Finding>) {
    for t in input.no_pk {
        out.push(Finding {
            severity: if t.table_rows > 100_000 {
                Severity::High
            } else {
                Severity::Warn
            },
            kind: "no primary key",
            object: format!("{}.{}", t.schema, t.table),
            detail: format!(
                "{} table with ~{} rows and no PRIMARY KEY — InnoDB adds a hidden \
                 row id, and row-based replication has to scan for every change",
                t.engine, t.table_rows
            ),
            action: None,
        });
    }
}

fn statement_findings(input: &AdvisorInput<'_>, out: &mut Vec<Finding>) {
    for d in input.digests {
        if d.count < MIN_EXEC_COUNT {
            continue;
        }
        let object = if d.schema.is_empty() {
            format!("digest {}", short_digest(&d.digest))
        } else {
            format!("{} · digest {}", d.schema, short_digest(&d.digest))
        };

        if d.no_index_used > 0 {
            out.push(Finding {
                severity: Severity::High,
                kind: "statement without index",
                object: object.clone(),
                detail: format!(
                    "{} of {} executions used no index — {}",
                    d.no_index_used,
                    d.count,
                    one_line(&d.text, 120)
                ),
                action: None,
            });
        }

        let ratio = d.examined_per_sent();
        if ratio > EXAMINED_RATIO_LIMIT && d.rows_examined > 0 {
            out.push(Finding {
                severity: Severity::Warn,
                kind: "low selectivity",
                object: object.clone(),
                detail: format!(
                    "{ratio:.0} rows examined per row returned — {}",
                    one_line(&d.text, 120)
                ),
                action: None,
            });
        }

        if d.tmp_disk_tables > 0 {
            out.push(Finding {
                severity: Severity::Warn,
                kind: "temp table on disk",
                object,
                detail: format!(
                    "{} on-disk temp tables — check sort/group columns and tmp_table_size — {}",
                    d.tmp_disk_tables,
                    one_line(&d.text, 100)
                ),
                action: None,
            });
        }
    }
}

/// A column wrapped in a function (`WHERE DATE(created_at) = ?`) cannot use an
/// index on that column, however good the index is.
fn unindexable_predicates(input: &AdvisorInput<'_>, out: &mut Vec<Finding>) {
    for d in input.digests {
        if d.count < MIN_EXEC_COUNT {
            continue;
        }
        let Some(shape) = suggest::parse_shape(&d.text) else {
            continue;
        };
        for column in &shape.wrapped_columns {
            out.push(Finding {
                severity: Severity::Warn,
                kind: "function on column",
                object: format!("{}.{column}", shape.table),
                detail: format!(
                    "the predicate wraps `{column}` in a function, so no index on it can be                      used — rewrite it as a range over the bare column, or add a generated                      column and index that — {}",
                    one_line(&d.text, 110)
                ),
                action: None,
            });
        }
    }
}

fn short_digest(d: &str) -> String {
    d.chars().take(12).collect()
}

fn one_line(s: &str, max: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= max {
        joined
    } else {
        joined.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ix(table: &str, name: &str, cols: &[&str], unique: bool) -> IndexDef {
        IndexDef {
            schema: "demo".into(),
            table: table.into(),
            index: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            unique,
        }
    }

    fn input<'a>(indexes: &'a [IndexDef], caps: Capabilities) -> AdvisorInput<'a> {
        AdvisorInput {
            indexes,
            usage: &[],
            scans: &[],
            no_pk: &[],
            digests: &[],
            caps,
            uptime_s: 0,
        }
    }

    #[test]
    fn flags_prefix_redundancy_once() {
        let ixs = vec![
            ix("orders", "idx_cust", &["customer_id"], false),
            ix(
                "orders",
                "idx_cust_date",
                &["customer_id", "created_at"],
                false,
            ),
        ];
        let f = analyze(&input(&ixs, Capabilities::default()));
        assert_eq!(f.len(), 1, "one finding, on the shorter index");
        assert_eq!(f[0].kind, "redundant index");
        assert_eq!(f[0].object, "demo.orders.idx_cust");
    }

    #[test]
    fn duplicate_pair_reported_once() {
        let ixs = vec![
            ix("t", "a_idx", &["col"], false),
            ix("t", "b_idx", &["col"], false),
        ];
        let f = analyze(&input(&ixs, Capabilities::default()));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "duplicate index");
        assert_eq!(f[0].object, "demo.t.a_idx");
    }

    #[test]
    fn unique_index_not_redundant_against_non_unique() {
        let ixs = vec![
            ix("t", "uq_email", &["email"], true),
            ix("t", "idx_email_name", &["email", "name"], false),
        ];
        assert!(analyze(&input(&ixs, Capabilities::default())).is_empty());
    }

    #[test]
    fn different_tables_never_collide() {
        let ixs = vec![
            ix("a", "idx", &["x"], false),
            ix("b", "idx2", &["x", "y"], false),
        ];
        assert!(analyze(&input(&ixs, Capabilities::default())).is_empty());
    }

    #[test]
    fn invisible_step_only_on_8_0_13() {
        let ixs = vec![
            ix("t", "a_idx", &["col"], false),
            ix("t", "b_idx", &["col"], false),
        ];
        let mut caps = Capabilities::default();
        let old = analyze(&input(&ixs, caps));
        assert!(!old[0].action.as_ref().unwrap().contains("INVISIBLE"));

        caps.invisible_index = true;
        let new = analyze(&input(&ixs, caps));
        assert!(
            new[0]
                .action
                .as_ref()
                .unwrap()
                .contains("ALTER INDEX `a_idx` INVISIBLE")
        );
    }

    #[test]
    fn unused_index_needs_a_week_of_uptime() {
        let ixs = vec![ix("t", "idx", &["c"], false)];
        let usage = vec![IndexUsage {
            schema: "demo".into(),
            table: "t".into(),
            index: "idx".into(),
            reads: 0,
            writes: 900,
        }];
        let mut inp = input(&ixs, Capabilities::default());
        inp.usage = &usage;

        inp.uptime_s = 3600;
        assert!(analyze(&inp).iter().all(|f| f.kind != "unused index"));

        inp.uptime_s = MIN_UPTIME_FOR_UNUSED_S + 1;
        assert!(analyze(&inp).iter().any(|f| f.kind == "unused index"));
    }
}
