# yMySQL

Desktop MySQL performance monitor and light client. Rust, egui/eframe GUI,
`mysql_async` on a Tokio runtime, SQLite for metric history.

Supports **MySQL 5.6 / 5.7 and 8.x**; Percona and MariaDB degrade gracefully
through the same capability matrix. Read-only by default — writes sit behind an
explicit switch that resets on every launch.

```sh
cargo run
```

## Screens

Twelve tabs, in the order they appear:

| Screen | What it does |
| --- | --- |
| **Dashboard** | Live stat cards (QPS, TPS, threads, slow queries, buffer pool hit, network, lock waits, firing alerts), rolling 15-minute plots, the five worst statements, and the raw `SHOW GLOBAL STATUS` table with per-second deltas |
| **Top SQL** | Statement digests sorted by total/avg/max time, executions, rows examined or examined-per-sent; flags temp-disk tables and no-index executions; click a row to inspect |
| **Query Inspector** | One digest in full: counters, statement text re-indented by the SQL formatter, `EXPLAIN` (SELECT only), a hand-off to the SQL tab, and recent executions from `events_statements_history_long` |
| **Lock Monitor** | Blocking/waiting pairs as cards — PID, user, database, lock duration, locked table, index, lock mode, both statements, kill either side — plus open InnoDB transactions, metadata locks, the full session list, and the latest detected deadlock |
| **Index Advisor** | Missing composite indexes proposed from the real workload with estimated impact and ready DDL, plus duplicate/redundant indexes, unused indexes, full table scans, tables without a primary key and function-wrapped predicates |
| **Historical Metrics** | Any metric over 15m–7d from the on-disk store, with rollups, store stats and CSV export |
| **Alerts** | Threshold rules with a hold window, live rule state, a firing banner and a transition log |
| **SQL** | Ad-hoc console: pick a default database, run with Ctrl+Enter, re-indent with Ctrl+Shift+F, sort by clicking a header, copy or save the result as CSV, JSON, Markdown, HTML or `INSERT` statements, click any value to open that row in the Table Browser, save named queries against the active profile |
| **Tables** | Schema/table picker, paged rows with server-side sort, WHERE filter, column metadata, and click-a-cell editing with queued changes |
| **Dump** | Export a database to `.sql` — structure only, data only or both; table picker, optional `DROP TABLE IF EXISTS`, consistent snapshot, live progress, cancel |
| **Connections** | Threads connected/running against `max_connections` with usage bands, breakdown by user and host, long-running and stale-idle session lists with per-row kill |
| **InnoDB** | Buffer pool, dirty ratio, redo log and checkpoint age against redo capacity, history list length, row locks and disk I/O — each band-coloured with what the number means |
| **Replication** | Each channel's IO and SQL threads, lag, apply backlog, relay log space, GTID position and thread errors; plus this server's own binary log position and connected replicas |

## Layout

| Path | Role |
| --- | --- |
| [src/main.rs](src/main.rs) | Tokio runtime, store thread, eframe bootstrap |
| [src/app.rs](src/app.rs) | App state, event pump, chrome |
| [src/ui/](src/ui/) | One module per screen plus shared widgets |
| [src/model.rs](src/model.rs) | `Sample`, `Derived` rates, the `Metric` catalogue |
| [src/db/collector.rs](src/db/collector.rs) | Background sampler, command/event channels |
| [src/db/schedule.rs](src/db/schedule.rs) | Load shaping: what to collect, when, how often |
| [src/db/queries.rs](src/db/queries.rs) | Monitoring SQL, branched on capabilities |
| [src/db/version.rs](src/db/version.rs) | Version parsing + capability matrix |
| [src/db/sql.rs](src/db/sql.rs) | Console, browser and row edits — everything that can write |
| [src/db/dump.rs](src/db/dump.rs) | `mysqldump`-shaped export writer |
| [src/advisor.rs](src/advisor.rs) | Index and workload findings |
| [src/fmt_sql.rs](src/fmt_sql.rs) | SQL lexer + pretty-printer for the console |
| [src/csv.rs](src/csv.rs) | RFC 4180 CSV export of a result grid |
| [src/json_rows.rs](src/json_rows.rs) | JSON and NDJSON export of a result grid |
| [src/markdown_table.rs](src/markdown_table.rs) | Markdown table export of a result grid |
| [src/insert_sql.rs](src/insert_sql.rs) | `INSERT` statement export of a result grid |
| [src/html_table.rs](src/html_table.rs) | HTML table export of a result grid |
| [src/suggest.rs](src/suggest.rs) | Digest parser + composite index proposals |
| [src/innodb.rs](src/innodb.rs) | `SHOW ENGINE INNODB STATUS` parser + health bands |
| [src/deadlock.rs](src/deadlock.rs) | `LATEST DETECTED DEADLOCK` parser |
| [src/replication.rs](src/replication.rs) | Replica/source status across the 5.7 and 8.0 column names |
| [src/connections.rs](src/connections.rs) | Connection-pool grouping and classification |
| [src/alerts.rs](src/alerts.rs) | Threshold state machine |
| [src/store.rs](src/store.rs) | SQLite metric history on its own thread |
| [src/profiles.rs](src/profiles.rs) | Saved connections; passwords via the OS keyring |

Three threads of control, no shared mutable state:

- The **GUI** never touches the database. It sends `Command`s and drains
  `Event`s once per frame.
- The **collector** owns the connection pool and runs every query on the Tokio
  runtime.
- The **store** thread owns the SQLite connection, because rusqlite is blocking.

Everything analytical — version capabilities, index suggestions, alert state,
InnoDB parsing, connection grouping, dump SQL generation — is a pure function in
its own module, so it can be unit-tested without a server.

## MySQL 5 vs 8

Every version difference is a flag in [`Capabilities`](src/db/version.rs), set
once at connect time and branched on in the query layer. Never compare versions
at a call site.

| Capability | MySQL 5.x | MySQL 8.x |
| --- | --- | --- |
| Process list | `information_schema.PROCESSLIST` | `performance_schema.processlist` (8.0.22+) |
| Lock waits | `innodb_lock_waits` + `innodb_locks` (information_schema) | `data_lock_waits` + `data_locks` (performance_schema) — the IS tables were **removed** in 8.0 |
| Digest sample SQL | `DIGEST_TEXT` only (normalised, so `EXPLAIN` needs literals substituted by hand) | `QUERY_SAMPLE_TEXT` |
| Metadata locks | 5.7.3+ | yes |
| Invisible indexes | no — advisor emits a plain `DROP INDEX` | 8.0.13+ — advisor emits `ALTER INDEX … INVISIBLE` first |
| Redo capacity | `innodb_log_file_size × innodb_log_files_in_group` | `innodb_redo_log_capacity` (8.0.30+) |
| `sys` schema | 5.7+ | yes |
| Replica status | `SHOW SLAVE STATUS` | `SHOW REPLICA STATUS` (8.0.22+) |
| Default auth | `mysql_native_password` | `caching_sha2_password` — enable TLS, or the handshake falls back to an RSA key exchange |

The advisor reads `information_schema.STATISTICS` and computes index redundancy
in Rust rather than using `sys.schema_redundant_indexes`, so it behaves
identically on 5.6, 5.7 and 8.x.

## Connection profiles

The top bar saves named profiles to `<data dir>/profiles.json` — host, port,
user, database, TLS flags and poll interval. The last profile you connected with
is preselected at startup (fields only; it does not auto-connect).

Named SQL statements are saved **inside the profile** they were written for,
each with the database it should run against. Switching profiles switches the
saved-query list, and re-saving a connection never drops its queries.

**Passwords are never written to that file.** Tick `remember password` and it
goes to the OS credential store instead (Windows Credential Manager, macOS
Keychain, Secret Service on \*nix), keyed by profile name *and* target — so
pointing a profile at another host will not hand back the old host's secret.
Untick it, or delete the profile, and the stored secret is removed. If the
credential store is unavailable the profile still saves; only the password is
dropped, and the log says so.

## Sampling load

A monitor that stampedes the server it watches is worse than none, so the
collector shapes its own load ([src/db/schedule.rs](src/db/schedule.rs)):

- **One expensive fetch per tick.** Digests, lock waits, open transactions,
  metadata locks, the InnoDB engine report and replication status rotate one per
  heavy tick instead of firing together.
- **Only what is on screen.** Each tab declares what it needs; tabs that show
  none of the above poll nothing but `SHOW GLOBAL STATUS`. Switching tabs
  fetches immediately rather than waiting out the rotation, and a task no
  visible screen wants is skipped entirely.
- **Adaptive backoff.** When the average sample exceeds half the interval the
  period doubles (up to 8×), and halves back down as the server recovers. The
  connection bar shows the interval in force and the last sample cost, amber
  while backed off.
- **Bounded pool.** One warm connection, three maximum by default
  (`ConnConfig::max_connections`), shared by sampler, console, browser and dump.
- **No pile-ups.** Missed ticks are delayed, not replayed as a burst.
- **Pause.** A checkbox in the connection bar stops polling without dropping the
  connection.

Interactive work — console, browser, advisor, dump — runs on command, never on
the tick.

## What TPS counts

`Com_commit` counts only *explicit* `COMMIT` statements. Most workloads run in
autocommit, so on a server doing 900 queries a second that counter never moves
and a transactions-per-second card reads `0.00` — correct, and useless.

[src/model.rs](src/model.rs) derives TPS from `Handler_commit + Handler_rollback`
instead, which counts the commit every statement performs, implicit ones
included. Servers without the handler counters fall back to the `Com_` pair.
Expect the number to track QPS on a read-heavy autocommit workload: each
statement really is its own transaction.

This changed after the first live run. Any `tps` history recorded before it is
on the old definition — in practice a flat line at zero, which is why the
storage key was left alone rather than migrated.

## Metric history

Samples land in SQLite (`%LOCALAPPDATA%\yMySQL\metrics.db` on Windows,
`$XDG_DATA_HOME/yMySQL/metrics.db` otherwise), keyed by `user@host:port`.

An install from before the rename (`mysql_perf`) is moved to the new directory
on first launch, and passwords filed under the old credential-store service
name are re-filed under `ymysql` the first time they are read.

- Raw samples: 6 hours.
- One-minute rollups (avg/min/max): 30 days.
- Ranges longer than the raw window read rollups and say so in the UI; every
  read is bucketed to at most 2000 plot points.
- CSV export covers the selected metrics and window.

## Connection monitor

`max_connections`, `wait_timeout` and `interactive_timeout` are read once per
connection; `Threads_connected` comes from every sample, so usage is live.

- Usage bands: green below 70%, amber from 70%, red from 85% — at the limit the
  server refuses new connections outright, which is why the warning arrives
  early.
- **By user** and **by host**, each split into active vs sleeping with the
  longest session in the bucket. Hosts group by address, dropping the ephemeral
  port (`10.0.0.7:51234` and `:51999` are one client) while leaving bare and
  bracketed IPv6 intact.
- **Long-running**: a statement open past the threshold (10s default).
- **Sleeping**: idle past the threshold (60s default) — the usual reason a pool
  fills, since each holds its slot until `wait_timeout` elapses.
- Both lists sort worst-first with a per-row `kill`; both thresholds are
  adjustable in the tab.
- `Connection usage %` is a first-class metric: it plots in Historical Metrics
  and ships with a default alert rule at 80% held for 30s.

The session lists come from the process list, which excludes this monitor's own
connection; `Threads_connected` does not — expect a difference of one.

## InnoDB monitor

Most figures come from `SHOW GLOBAL STATUS`, but the two that matter most during
an incident — **history list length** and **checkpoint age** — exist only in
`SHOW ENGINE INNODB STATUS`, a text report whose layout differs between 5.7 and
8.0. [src/innodb.rs](src/innodb.rs) parses both shapes line by line; missing
lines stay zero rather than costing the numbers that did parse.

- **Buffer pool** — lifetime and live hit ratio, pages total/data/dirty/free,
  dirty ratio against `innodb_max_dirty_pages_pct`, and `wait_free` (threads
  that had to wait for a clean page).
- **Redo and checkpoint** — checkpoint age as a share of redo capacity. Amber at
  75%, red at 90%, where InnoDB starts flushing furiously and write throughput
  collapses. The fix there is a larger redo log, not more I/O.
- **Purge / MVCC** — history list length, banded at 100k and 1M. A number that
  climbs and never falls is usually one forgotten open transaction; find it
  under Lock Monitor → Transactions.
- **Row locks** — current waiters, total waits, average and max wait.
- **Disk I/O** — data reads/writes/fsyncs, row rates, `innodb_io_capacity`.

## Deadlocks

Lock Monitor → **Deadlock** shows the last deadlock InnoDB detected: both
transactions, the statement each was running, the locks each held, the lock each
wanted, and which one was rolled back. The statements go through the SQL
formatter, and **Copy report** yields the raw section for a ticket.

It comes out of the same `SHOW ENGINE INNODB STATUS` report the InnoDB tab
already reads, so it costs no extra query — [src/deadlock.rs](src/deadlock.rs)
parses the `LATEST DETECTED DEADLOCK` section by marker lines rather than by
offset, which is what keeps 5.7 and 8.0 on one code path, and anything it does
not recognise stays available as raw text.

Two limits worth knowing. InnoDB remembers exactly **one** deadlock, and forgets
it on restart, so this is a snapshot and not a history — frequency has to come
from the error log with `innodb_print_all_deadlocks=ON`. And the victim is the
transaction that got the error; the other side committed, so the application may
have no idea a deadlock happened at all.

## Replication

The **Replication** tab reads `SHOW REPLICA STATUS` on 8.0.22+ and
`SHOW SLAVE STATUS` before it, one card per channel.

MySQL 8.0.22 renamed nearly every column in that result — `Slave_IO_Running` to
`Replica_IO_Running`, `Seconds_Behind_Master` to `Seconds_Behind_Source`, and so
on — and kept the old spellings only in the *statement*, not in the output.
Rather than branch on version, [src/replication.rs](src/replication.rs) looks up
every field under both names and takes the first one actually present, which
also covers MariaDB.

Each card gives the two thread states, lag, relay log space, GTID auto-position
and any thread error, plus one derived number: the **apply backlog**, the byte
gap between what the receiver has fetched and what the applier has executed.
When both threads are running and that gap is large, the SQL thread is the
bottleneck rather than the network — the distinction that decides whether more
bandwidth or more apply parallelism is the fix. It is only shown when both
threads are on the same binary log file, where the subtraction means something.

`Seconds_Behind_Source` reading NULL is treated as **not replicating**, not as
caught up; that inversion is the classic way a broken replica reads as healthy.
Lag turns amber at 30s and red at 300s, a configured `SOURCE_DELAY` is called out
so a deliberate delay is not read as a fault, and the tab label itself colours
when any channel is unhealthy so a stopped replica is visible from any screen.

Below the channels is what this server looks like *as* a source: its binary log
file and position, executed GTID set, and the replicas currently connected to
it. Reading any of this needs the `REPLICATION CLIENT` privilege, which
`scripts/monitor-user.sql` already grants; without it the tab says so rather
than showing an empty screen.

## Index suggestions

Statement digests arrive already normalised (`WHERE user_id = ?`), so
[src/suggest.rs](src/suggest.rs) can read a statement's shape without a full SQL
grammar and propose the index that shape wants.

- Equality columns first, then **one** range column — the ordering that makes
  the leftmost-prefix rule work. `ORDER BY` columns are appended only when no
  range column precedes them, since a range stops the index serving the sort.
- A proposal is dropped when an existing index already has it as a leftmost
  prefix, and becomes an *extension* (`DROP INDEX` + `ADD INDEX`) when a
  narrower index is a prefix of it — no overlapping duplicates.
- Shapes the heuristics cannot reason about safely are skipped outright: joins,
  `OR`, subqueries, implicit comma joins, non-`SELECT`.
- Predicates that wrap a column in a function (`WHERE DATE(created_at) = ?`) are
  reported separately — no index on that column can be used at all.
- Identical shapes from different statements merge into one suggestion, with
  executions and total time summed.
- DDL carries `ALGORITHM=INPLACE, LOCK=NONE`, so the server refuses rather than
  silently locking the table if it cannot build the index online.

**Impact is an estimate, not a promise.** It is ranked from executions, rows
examined per row returned and total time — properties of the *current*
statement, not a measurement of the proposed index. Real improvement depends on
data distribution, cardinality, optimiser statistics and the rest of the
workload. Verify with `EXPLAIN` on a copy before shipping.

## From a query result to an edit

Console results carry the protocol's column metadata, so every column the server
traced back to a real table (`schema` + `org_table` + `org_name`, i.e. before
any alias) is clickable. Clicking a value opens the Table Browser on that table
with the filter `` `column` = 'value' `` (or `IS NULL`) already applied — the
row is then editable in the normal way, primary key permitting.

Columns that are expressions, literals or aggregates have no origin and stay
inert; clicking a header still sorts. The filter literal is escaped for both
quotes and backslashes.

## Formatting SQL

**Format** in the SQL tab, or Ctrl+Shift+F, re-indents the editor contents.
[src/fmt_sql.rs](src/fmt_sql.rs) lexes the text rather than pattern-matching it,
so the transformation is limited to two things: whitespace, and the case of
reserved words. String literals, backtick-quoted identifiers, numbers,
placeholders and comments come back byte-for-byte.

Clause heads (`SELECT`, `FROM`, `WHERE`, `GROUP BY`, …) start a line, joins and
`AND`/`OR` indent one level under them, a `SELECT` or `SET` list with more than
one item goes one item per line, and a parenthesis that opens a subquery becomes
its own block while a function call stays inline.

Case handling is deliberately conservative. Only *reserved* words are
upper-cased, plus a word sitting directly in front of `(`, which can only be a
routine call. Neither can be a bare table name — and that is the point, because
table names are case-sensitive on Linux while column and routine names never
are. A table that happens to be called `offset` or `count` keeps its case and
its meaning.

Two unit tests hold the contract: one re-lexes the output and asserts the token
stream is identical to the input except for word case, and one asserts
formatting twice gives the same result as formatting once.

The Query Inspector uses the same formatter. `performance_schema` stores a
digest as one long line, so **Format** is on by default there; the checkbox
turns it off to see exactly what the server holds. **Copy** and **Send to SQL
tab** both hand on whichever version is on screen, and the latter also selects
the digest's schema in the console, so a slow statement goes from Top SQL to an
editable, runnable query in two clicks. Hovering a row under *Recent executions*
shows that sample formatted; it is rendered only for the row under the pointer,
and the inspected statement is formatted once per digest rather than once per
frame.

## Exporting a result

The **Export** section under a result offers five formats: CSV, JSON, Markdown,
HTML and `INSERT` statements. Each has **Copy** (to the clipboard) and **Save**
(to the path in the box, pre-filled with a timestamped name in the working
directory). All of them export the grid *as displayed*, so a column sort is
reflected in the output.

Only JSON can express a SQL `NULL` directly. The other four need a convention,
and each one keeps `NULL` distinct from the empty string rather than flattening
the two together — that difference is usually the reason someone is looking at
the row in the first place.

### CSV

[src/csv.rs](src/csv.rs) follows RFC 4180: CRLF line endings, and a field is
quoted when it holds a comma, a quote, a newline or edge whitespace, with an
embedded quote doubled.

SQL `NULL` and the empty string are different values and CSV cannot say so on
its own, so this uses the `COPY ... CSV` convention: `NULL` is written as
nothing at all, an empty string as `""`.

**Excel BOM** is on by default and prefixes a UTF-8 byte-order mark, which is
what makes Excel read accented and non-Latin text correctly. Turn it off for
anything that parses the file itself.

Cell values are exported verbatim, including any that begin with `=`, `+`, `-`
or `@`. Spreadsheets treat those as formulas — a known CSV injection route. The
export stays faithful to the data rather than mangling it, so treat a CSV built
from untrusted rows the way you would treat any untrusted file.

### JSON

[src/json_rows.rs](src/json_rows.rs) writes one object per row, keyed by column
name. `NULL` becomes JSON `null` — this is the one format that can say so
without a convention.

**NDJSON** switches to one compact object per line with no wrapping array, which
streams into log pipelines and survives being cut in half; the suggested
filename follows with a `.ndjson` extension. **pretty** indents the array form.

Values are strings, not numbers. A grid carries no column types and guessing
costs more than it saves: a `BIGINT` id above 2^53 loses precision the moment a
JavaScript consumer parses it as a number, and `007` from a `VARCHAR` would come
back as `7`. Strings round-trip both.

Two columns with the same name — `id` from either side of a join — would be one
repeated JSON key, and most parsers keep only the last. The second is suffixed
`id_2` instead, skipping any suffix that would collide with a real column.

### Markdown

[src/markdown_table.rs](src/markdown_table.rs) writes a GitHub-flavoured pipe
table for pasting into an issue, a pull request or a ticket. Cells are padded to
a common width so the raw text lines up before anything renders it, and a column
whose values all parse as numbers is right-aligned — presentation only, never a
change to a value.

A `|` would end the cell and a newline would end the row, so both are rewritten:
backslashes and pipes are escaped, and a line break becomes `<br>` rather than
being dropped. `NULL` renders as `_NULL_`, which an empty cell cannot be
mistaken for.

### HTML

[src/html_table.rs](src/html_table.rs) writes a `<table>` with a caption naming
the source table and the row count. **Standalone page** is on by default and
wraps it in a document with a charset, a title and an inline stylesheet, so the
file opens correctly on its own and follows the reader's light or dark theme.
Turn it off for a bare `<table>` to paste into a page of your own.

Result rows are server data, so every cell and column name is escaped —
`&`, `<`, `>`, `"` and `'` all go out as entities, which is safe in both text
and attribute positions. A value of `<script>alert(1)</script>` renders as that
text and nothing else. Tests cover the values, the headers and the document
title.

`NULL` gets its own dimmed cell so it cannot be confused with the literal text
`NULL` or with the empty string.

### INSERT statements

[src/insert_sql.rs](src/insert_sql.rs) writes the rows as `INSERT`, `INSERT
IGNORE` or `REPLACE`, batched at the chosen rows per statement (1 gives one
statement per row). Table and column identifiers are backtick-quoted, and string
values are escaped the way a restore expects — backslash, quote, newline, NUL
and `^Z` all go out as escape sequences.

The target table is pre-filled from the column origins when every column traces
back to one table; a join leaves it blank for you to name. Columns are written
under their *origin* names, so `SELECT id AS user_id` still inserts into `id`.
An expression column has no origin and keeps its header.

Every non-`NULL` value is emitted as a quoted string and MySQL casts it on
insert. Emitting numbers bare would turn `007` into `7` in a `VARCHAR` column,
and a grid carries no column types to decide otherwise. For the same reason this
is a convenience export, not a faithful copy — binary columns have already been
stringified by the time they reach the grid. Use the Dump tab for that; it works
from the raw protocol values.

## Editing data

The SQL tab and the Table Browser can change the connected server, so both sit
behind one switch.

- **Allow writes** is off at startup and resets to off on every restart. It is
  reachable from the SQL tab, the Table Browser toolbar, and inline wherever an
  edit is blocked. While off, the console runs `SELECT`/`SHOW`/`EXPLAIN` only
  and row edits are refused — by the UI *and* again in the collector, which
  classifies every statement before it reaches the server.
- Statement classification skips leading `--`, `#` and `/* */` comments, so a
  comment cannot disguise a `DELETE`.
- Row edits need a **primary key**. Tables and views without one open read-only,
  because no other WHERE clause is guaranteed to hit exactly one row.
- Edits queue. Each shows the exact statement it will run before you apply, and
  applying runs the whole queue in one transaction — any failure rolls the batch
  back.
- Generated SQL uses backtick-quoted identifiers (embedded backticks doubled)
  and bound parameters, so a value containing `'; DROP TABLE …` stays a value.
  Key matching uses `<=>`, so a NULL key column still matches.
- Console results are capped at 500 rows; browser pages fetch one extra row to
  decide whether a next page exists.
- The console's **Database** picker issues `USE` on whichever pooled connection
  the statement lands on, with the name quoted. `(none)` runs without `USE`, so
  table names must be schema-qualified. The picker resets on reconnect and drops
  a selection the server no longer reports.

## Dumping

The Dump tab writes a restorable `.sql` file over the connection already open.

- **Structure only / data only / both.** Structure is `SHOW CREATE TABLE`
  (views included), optionally preceded by `DROP TABLE IF EXISTS`.
- **Table picker.** Tick specific tables, or leave the selection empty for the
  whole database.
- **Consistent snapshot** (default on): the run happens inside
  `START TRANSACTION WITH CONSISTENT SNAPSHOT`, so the file is one point in time
  rather than a smear. InnoDB only, and it holds a long read transaction.
- **Streaming.** Rows go out in multi-row `INSERT`s of 200 rows / 512 KB,
  written as they arrive rather than buffered whole.
- **Progress and cancel.** Cancelling stops at the next batch and leaves a
  valid, truncated file marked `-- cancelled by the user`.
- Values are escaped mysqldump-style — backslash, single quote, newline,
  carriage return, NUL and ^Z become escape sequences, and non-UTF-8 columns are
  written as `0x…` hex so blobs survive the round trip.
- A dump only reads; it is not gated behind the write switch.

## Safety

- Read-only by default, enforced in the UI and re-checked in the collector.
- `EXPLAIN` accepts `SELECT` only, refuses multi-statement strings, and
  validates the schema name before `USE`.
- `KILL` is always explicit and per row (Lock Monitor, Connections, Sessions).
- With writes disabled the app issues nothing but reads and `KILL`.
- A red **WRITES ENABLED** flag sits in the connection bar whenever the switch
  is on.
- Passwords never reach disk in plaintext; the profile file holds no secret.

## Test servers

```sh
docker compose up -d          # 5.7 on :3357, 8.0 on :3380
# user monitor / monitorpw, or root / rootpw
```

The `monitor` account gets `PROCESS`, `REPLICATION CLIENT` and `SELECT` on
`performance_schema` — enough for every panel — plus kill rights (`SUPER` on
5.7, `CONNECTION_ADMIN` on 8.0). It is read-only on data, so row edits need a
grant of their own, or connect as `root`:

```sql
GRANT INSERT, UPDATE, DELETE ON demo.* TO 'monitor'@'%';
```

Some panels need consumers that ship disabled on 5.7:

```sql
UPDATE performance_schema.setup_consumers
   SET ENABLED = 'YES'
 WHERE NAME = 'events_statements_history_long';
UPDATE performance_schema.setup_instruments
   SET ENABLED = 'YES', TIMED = 'YES'
 WHERE NAME = 'wait/lock/metadata/sql/mdl';
```

## Development

```sh
cargo test                    # 202 unit tests, no server required
cargo clippy --all-targets
cargo fmt
cargo build --release
```

The suite covers the pure layers: the version/capability matrix, index
redundancy and suggestion rules, the alert state machine, the metric store and
its rollups, SQL and dump builders (including escaping and injection cases),
`SHOW ENGINE INNODB STATUS` parsing for both 5.7 and 8.0 layouts, connection
grouping, and the sampling scheduler.

**Not yet exercised against a live server.** Everything compiles and the pure
logic is tested, but the queries themselves have not been run end to end against
real 5.7 and 8.0 instances — start the compose rig and point the app at both
ports before trusting this in production.

Requires **Rust 1.92+** — that is eframe 0.35 and egui_plot 0.36's own MSRV, and
the code uses edition 2024 with let-chains. On macOS, add the
`apple-native-keyring-store` feature to the `keyring` dependency for password
storage; Windows and Linux are covered by its defaults.

## Next

- **Validation against a live server.** Nothing here has been run against real
  MySQL yet; `docker compose up -d` brings up 5.7 on 3357 and 8.0 on 3380 for
  exactly that.
- Variables tab and a config-vs-workload advisor.
- `EXPLAIN ANALYZE` and `FORMAT=JSON` in the Query Inspector (8.0.18+).
- Schema sizes from `information_schema.TABLES` — data and index bytes per table.
- Default the long-running threshold to the server's own `long_query_time`.
- Alert delivery beyond the app window (desktop toast, webhook).
- Multi-server view: several profiles sampled side by side.
