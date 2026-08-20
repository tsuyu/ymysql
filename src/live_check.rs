//! A read-only smoke test against a real server.
//!
//! Everything else in this crate is tested without a database. This module is
//! the exception: it connects to a saved profile and runs each monitoring query
//! once, reporting what came back and what it cost. It exists because the SQL
//! is the part unit tests cannot cover — version-gated statements, privilege
//! requirements and column names are only real once a server answers them.
//!
//! It is `#[ignore]`d, so `cargo test` never touches a server. To run it:
//!
//! ```text
//! MYSQL_PERF_PROFILE="ioffiice" cargo test live_check -- --ignored --nocapture
//! ```
//!
//! The password comes from the OS credential store, the same place the app
//! keeps it — nothing is typed on a command line or written to the output.
//!
//! **Every statement here reads.** No writes, no DDL, no `KILL`, no dump. The
//! heaviest are the index-advisor queries against `information_schema`, which
//! are reported with their timings so the cost is visible.

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use mysql_async::Pool;

    use crate::db::version::Capabilities;
    use crate::db::{ConnConfig, queries, sql};
    use crate::{deadlock, innodb, profiles, replication};

    /// Widest column that keeps the report readable.
    const NAME_W: usize = 34;

    struct Report {
        pass: usize,
        fail: usize,
    }

    impl Report {
        /// Records one check. `Ok(note)` prints the note, `Err` prints the
        /// error but does not stop the run — the point is to see every failure
        /// in one pass, not the first.
        fn check<T>(
            &mut self,
            name: &str,
            started: Instant,
            r: &anyhow::Result<T>,
            note: impl FnOnce(&T) -> String,
        ) {
            let ms = started.elapsed().as_millis();
            match r {
                Ok(v) => {
                    self.pass += 1;
                    println!("  ok    {name:<NAME_W$} {ms:>6}ms  {}", note(v));
                }
                Err(e) => {
                    self.fail += 1;
                    println!("  FAIL  {name:<NAME_W$} {ms:>6}ms  {e:#}");
                }
            }
        }

        fn note(&mut self, name: &str, started: Instant, text: String) {
            let ms = started.elapsed().as_millis();
            self.pass += 1;
            println!("  ok    {name:<NAME_W$} {ms:>6}ms  {text}");
        }

        fn bad(&mut self, name: &str, text: String) {
            self.fail += 1;
            println!("  FAIL  {name:<NAME_W$}         {text}");
        }
    }

    fn profile() -> Option<ConnConfig> {
        let wanted = std::env::var("MYSQL_PERF_PROFILE").ok()?;
        let store = profiles::Profiles::load();
        let mut cfg = store
            .items
            .iter()
            .find(|p| p.conn.name == wanted)
            .map(|p| p.conn.clone())?;
        cfg.password = profiles::load_password(&cfg)?;
        Some(cfg)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "connects to a real server; set MYSQL_PERF_PROFILE to run"]
    async fn live_check() {
        let Some(cfg) = profile() else {
            println!(
                "skipped: set MYSQL_PERF_PROFILE to a saved profile name with a \
                 stored password"
            );
            return;
        };
        println!(
            "\nchecking {}@{}:{}  (read-only)\n",
            cfg.user, cfg.host, cfg.port
        );

        let pool = Pool::new(cfg.to_opts());
        let mut conn = match pool.get_conn().await {
            Ok(c) => c,
            Err(e) => {
                println!("  FAIL  connect: {e:#}");
                return;
            }
        };
        let mut r = Report { pass: 0, fail: 0 };

        // ---- identity and capabilities -------------------------------------
        let version = match queries::server_version(&mut conn).await {
            Ok(v) => v,
            Err(e) => {
                println!("  FAIL  server_version: {e:#}");
                return;
            }
        };
        let mut caps = Capabilities::detect(&version);
        caps.perf_schema_on = queries::perf_schema_enabled(&mut conn).await;
        println!("  server  {version}");
        println!("  caps    {}\n", caps_summary(&caps));

        // ---- dashboard ------------------------------------------------------
        println!("  -- dashboard --");
        let t = Instant::now();
        let status = queries::global_status(&mut conn).await;
        r.check("global_status", t, &status, |(s, _)| {
            format!(
                "{} counters, Handler_commit {}",
                s.len(),
                if s.contains_key("Handler_commit") {
                    "present"
                } else {
                    "MISSING -- tps falls back to Com_commit"
                }
            )
        });

        let t = Instant::now();
        let vars = queries::global_variables(&mut conn).await;
        r.check("global_variables", t, &vars, |v| {
            format!("{} variables", v.len())
        });
        if let Ok(v) = &vars {
            println!(
                "        long_query_time {}  performance_schema {}",
                v.get("long_query_time").map(String::as_str).unwrap_or("?"),
                v.get("performance_schema")
                    .map(String::as_str)
                    .unwrap_or("?")
            );
        }

        let t = Instant::now();
        let limits = queries::server_limits(&mut conn).await;
        r.check("server_limits", t, &limits, |l| {
            format!("max_connections {}", l.max_connections)
        });

        let t = Instant::now();
        let procs = queries::processlist(&mut conn, &caps).await;
        r.check("processlist", t, &procs, |p| {
            format!("{} sessions", p.len())
        });

        // ---- top sql / inspector -------------------------------------------
        println!("\n  -- top sql / inspector --");
        let t = Instant::now();
        let digests = queries::top_queries(&mut conn, &caps, 20).await;
        let first = digests.as_ref().ok().and_then(|d| d.first().cloned());
        r.check("top_queries", t, &digests, |d| {
            format!("{} statements", d.len())
        });

        if let Some(d) = &first {
            println!("        worst: {}", crate::ui::one_line(&d.text, 70));
            println!(
                "        count {} avg {:.1}ms examined/sent {:.0}",
                d.count,
                d.avg_ms,
                d.examined_per_sent()
            );
            let t = Instant::now();
            let detail = queries::digest_detail(&mut conn, &caps, &d.digest).await;
            r.check("digest_detail", t, &detail, |x| match x {
                Some(_) => "found".to_string(),
                None => "digest vanished between calls".to_string(),
            });
            let t = Instant::now();
            let samples = queries::statement_samples(&mut conn, &caps, &d.digest, 20).await;
            r.check("statement_samples", t, &samples, |s| {
                if s.is_empty() {
                    "0 -- events_statements_history_long consumer likely off".to_string()
                } else {
                    format!("{} recent executions", s.len())
                }
            });
        }

        // ---- locks -----------------------------------------------------------
        println!("\n  -- locks --");
        let t = Instant::now();
        let waits = queries::lock_waits(&mut conn, &caps).await;
        r.check("lock_waits", t, &waits, |w| {
            format!("{} blocked sessions", w.len())
        });
        let t = Instant::now();
        let trx = queries::transactions(&mut conn).await;
        r.check("transactions", t, &trx, |x| {
            format!("{} open transactions", x.len())
        });
        let t = Instant::now();
        let mdl = queries::metadata_locks(&mut conn, &caps).await;
        r.check("metadata_locks", t, &mdl, |m| {
            format!("{} metadata locks", m.len())
        });

        // ---- innodb + deadlock ----------------------------------------------
        println!("\n  -- innodb --");
        let t = Instant::now();
        let report = queries::engine_innodb_status(&mut conn).await;
        match &report {
            Ok(text) => {
                let st = innodb::parse_engine_status(text);
                r.note("engine_innodb_status", t, format!("{} bytes", text.len()));
                println!(
                    "        parsed: lsn {} checkpoint_age {} history {} pool_pages {}",
                    st.lsn,
                    st.checkpoint_age(),
                    st.history_list_length,
                    st.buffer_pool_pages
                );
                if st.lsn == 0 || st.buffer_pool_pages == 0 {
                    r.bad(
                        "engine status parse",
                        "parsed as zeroes -- layout not recognised".to_string(),
                    );
                }
                match deadlock::parse(text) {
                    Some(d) => println!(
                        "        deadlock: {} at {} victim {:?} tables {:?}",
                        d.parties.len(),
                        d.detected_at,
                        d.victim,
                        d.tables()
                    ),
                    None => println!("        deadlock: none recorded since startup"),
                }
            }
            Err(e) => r.bad("engine_innodb_status", format!("{e:#}")),
        }

        // ---- replication -----------------------------------------------------
        println!("\n  -- replication --");
        let t = Instant::now();
        let repl = queries::replica_status(&mut conn, &caps).await;
        match &repl {
            Ok(grid) => {
                let parsed = replication::parse_replicas(grid);
                r.note(
                    "replica_status",
                    t,
                    format!(
                        "{} channel(s), {} columns",
                        grid.rows.len(),
                        grid.columns.len()
                    ),
                );
                for p in &parsed {
                    println!(
                        "        channel {:?}: {} health {:?} backlog {:?}",
                        p.channel,
                        p.summary(),
                        p.health(),
                        p.apply_backlog_bytes()
                    );
                }
                // A row that parses to nothing means the column names moved.
                if !grid.rows.is_empty() && parsed.iter().all(|p| p.source_host.is_empty()) {
                    r.bad(
                        "replica_status parse",
                        format!("row present but no host parsed; columns {:?}", grid.columns),
                    );
                }
            }
            Err(e) => r.bad("replica_status", format!("{e:#}")),
        }
        let t = Instant::now();
        let src = queries::source_status(&mut conn).await;
        r.check(
            "source_status",
            t,
            &src,
            |g| match replication::parse_source(g) {
                Some(s) if s.logging() => format!("binlog {} @ {}", s.file, s.position),
                _ => "binary logging off".to_string(),
            },
        );
        let t = Instant::now();
        let hosts = queries::connected_replicas(&mut conn, &caps).await;
        r.check("connected_replicas", t, &hosts, |g| {
            format!("{} replicas connected", g.rows.len())
        });

        // ---- index advisor ---------------------------------------------------
        println!("\n  -- index advisor (information_schema; the heavy ones) --");
        let t = Instant::now();
        let usage = queries::index_usage(&mut conn, &caps).await;
        r.check("index_usage", t, &usage, |i| format!("{} indexes", i.len()));
        let t = Instant::now();
        let scans = queries::full_table_scans(&mut conn, &caps).await;
        r.check("full_table_scans", t, &scans, |x| {
            format!("{} scanned tables", x.len())
        });
        let t = Instant::now();
        let defs = queries::index_definitions(&mut conn).await;
        r.check("index_definitions", t, &defs, |x| {
            format!("{} index columns", x.len())
        });
        let t = Instant::now();
        let nopk = queries::tables_without_pk(&mut conn).await;
        r.check("tables_without_pk", t, &nopk, |x| {
            format!("{} tables without a primary key", x.len())
        });

        // ---- schema browsing --------------------------------------------------
        println!("\n  -- schema --");
        let t = Instant::now();
        let schemas = sql::list_schemas(&mut conn).await;
        let first = schemas.as_ref().ok().and_then(|s| s.first().cloned());
        r.check("list_schemas", t, &schemas, |s| {
            format!("{} databases", s.len())
        });
        if let Some(schema) = first {
            let t = Instant::now();
            let tables = sql::list_tables(&mut conn, &schema).await;
            r.check("list_tables", t, &tables, |x| {
                format!("{} tables in {schema}", x.len())
            });
        }

        println!("\n  {} passed, {} failed\n", r.pass, r.fail);
        drop(conn);
        let _ = pool.disconnect().await;
        let failures = r.fail;
        assert_eq!(
            failures, 0,
            "{failures} live checks failed -- see output above"
        );
    }

    fn caps_summary(c: &Capabilities) -> String {
        let flag = |on: bool, name: &str| {
            if on {
                format!("+{name}")
            } else {
                format!("-{name}")
            }
        };
        [
            flag(c.perf_schema_on, "perf_schema"),
            flag(c.ps_processlist, "ps_processlist"),
            flag(c.ps_data_locks, "ps_data_locks"),
            flag(c.statement_digest, "digests"),
            flag(c.digest_sample_text, "sample_text"),
            flag(c.history_long, "history_long"),
            flag(c.metadata_locks, "metadata_locks"),
            flag(c.index_usage, "index_usage"),
            flag(c.replica_terms, "replica_terms"),
        ]
        .join(" ")
    }
}
