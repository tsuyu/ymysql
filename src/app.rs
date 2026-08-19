//! Application state and frame loop. Tab bodies live in [`crate::ui`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use egui::{Color32, RichText};

use crate::advisor::{self, AdvisorInput, Finding};
use crate::alerts;
use crate::connections;
use crate::db::ConnConfig;
use crate::db::collector::{AdvisorData, Command, Event, Handle};
use crate::db::dump::{DumpSpec, DumpStats};
use crate::db::queries::{
    DigestRow, Grid, LockWait, MdlRow, ServerLimits, StatementSample, TrxRow,
};
use crate::db::schedule::ViewNeeds;
use crate::db::sql::{BrowseSpec, Change, SqlOutcome, TableInfo, TableSchema};
use crate::db::version::{Capabilities, ServerVersion};
use crate::html_table;
use crate::innodb::{EngineStatus, InnodbConfig};
use crate::insert_sql;
use crate::json_rows;
use crate::model::{Derived, History, Metric, Sample};
use crate::profiles::{Profile, Profiles, SavedQuery};
use crate::store::{self, StoreCmd, StoreEvent};
use crate::suggest::{self, IndexSuggestion};
use crate::ui;

const MAX_LOG: usize = 200;
/// A statement has to run at least this often before its shape is worth an
/// index proposal.
const MIN_SUGGEST_EXECUTIONS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    TopSql,
    Inspector,
    Locks,
    IndexAdvisor,
    Historical,
    Alerts,
    Sql,
    Tables,
    Dump,
    Connections,
    Innodb,
}

impl Tab {
    pub const ORDER: [Tab; 12] = [
        Tab::Dashboard,
        Tab::TopSql,
        Tab::Inspector,
        Tab::Locks,
        Tab::IndexAdvisor,
        Tab::Historical,
        Tab::Alerts,
        Tab::Sql,
        Tab::Tables,
        Tab::Dump,
        Tab::Connections,
        Tab::Innodb,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::TopSql => "Top SQL",
            Tab::Inspector => "Query Inspector",
            Tab::Locks => "Lock Monitor",
            Tab::IndexAdvisor => "Index Advisor",
            Tab::Historical => "Historical Metrics",
            Tab::Alerts => "Alerts",
            Tab::Sql => "SQL",
            Tab::Tables => "Tables",
            Tab::Dump => "Dump",
            Tab::Connections => "Connections",
            Tab::Innodb => "InnoDB",
        }
    }
}

/// Sub-view of the Lock Monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockView {
    Blocking,
    Transactions,
    Metadata,
    Sessions,
}

impl LockView {
    pub const ORDER: [LockView; 4] = [
        LockView::Blocking,
        LockView::Transactions,
        LockView::Metadata,
        LockView::Sessions,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected,
}

/// Sort column for Top SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlSort {
    TotalTime,
    AvgTime,
    MaxTime,
    Count,
    RowsExamined,
    ExaminedRatio,
}

impl SqlSort {
    pub const ORDER: [SqlSort; 6] = [
        SqlSort::TotalTime,
        SqlSort::AvgTime,
        SqlSort::MaxTime,
        SqlSort::Count,
        SqlSort::RowsExamined,
        SqlSort::ExaminedRatio,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SqlSort::TotalTime => "total time",
            SqlSort::AvgTime => "avg time",
            SqlSort::MaxTime => "max time",
            SqlSort::Count => "executions",
            SqlSort::RowsExamined => "rows examined",
            SqlSort::ExaminedRatio => "examined/sent",
        }
    }

    pub fn key(self, d: &DigestRow) -> f64 {
        match self {
            SqlSort::TotalTime => d.total_ms,
            SqlSort::AvgTime => d.avg_ms,
            SqlSort::MaxTime => d.max_ms,
            SqlSort::Count => d.count as f64,
            SqlSort::RowsExamined => d.rows_examined as f64,
            SqlSort::ExaminedRatio => d.examined_per_sent(),
        }
    }
}

/// Time window for the Historical Metrics tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistRange {
    M15,
    H1,
    H6,
    D1,
    D7,
}

impl HistRange {
    pub const ORDER: [HistRange; 5] = [
        HistRange::M15,
        HistRange::H1,
        HistRange::H6,
        HistRange::D1,
        HistRange::D7,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HistRange::M15 => "15m",
            HistRange::H1 => "1h",
            HistRange::H6 => "6h",
            HistRange::D1 => "24h",
            HistRange::D7 => "7d",
        }
    }

    pub fn secs(self) -> i64 {
        match self {
            HistRange::M15 => 15 * 60,
            HistRange::H1 => 3600,
            HistRange::H6 => 6 * 3600,
            HistRange::D1 => 24 * 3600,
            HistRange::D7 => 7 * 24 * 3600,
        }
    }
}

/// A cell the user clicked in the table browser, being edited.
#[derive(Debug, Clone)]
pub struct CellEdit {
    pub column: String,
    /// Primary-key values of the row, captured when the edit started.
    pub key: Vec<(String, Option<String>)>,
    pub original: Option<String>,
    pub value: String,
    pub set_null: bool,
}

#[derive(Debug, Clone)]
pub struct StoreStats {
    pub raw_rows: i64,
    pub rollup_rows: i64,
    pub oldest_ms: Option<i64>,
    pub file_bytes: u64,
    pub path: std::path::PathBuf,
}

pub struct App {
    // Connection
    pub cfg: ConnConfig,
    pub profiles: Profiles,
    /// Name the Save button will write to.
    pub profile_name: String,
    pub remember_password: bool,
    pub collector: Handle,
    pub store: store::Handle,
    pub state: ConnState,
    pub version: ServerVersion,
    pub caps: Capabilities,
    pub uptime_s: u64,
    pub server_label: String,
    /// `max_connections` and the idle timeouts, read once at connect.
    pub limits: ServerLimits,
    /// Buffer pool size, redo capacity and flush settings, read once at connect.
    pub innodb_config: InnodbConfig,
    /// Latest parsed `SHOW ENGINE INNODB STATUS`.
    pub engine_status: EngineStatus,
    /// Seconds before a running statement counts as long-running.
    pub long_query_secs: i64,
    /// Seconds before a sleeping session counts as stale.
    pub idle_secs: i64,

    // Live sampling
    pub prev: Option<Sample>,
    pub latest: Option<Sample>,
    pub derived: Derived,
    pub history: History,

    // Top SQL / inspector
    pub top_sql: Vec<DigestRow>,
    pub sql_sort: SqlSort,
    pub sql_filter: String,
    pub selected_digest: Option<String>,
    pub inspected: Option<DigestRow>,
    pub inspect_samples: Vec<StatementSample>,
    pub explain: Option<Grid>,

    // Locks
    pub lock_waits: Vec<LockWait>,
    pub transactions: Vec<TrxRow>,
    pub metadata_locks: Vec<MdlRow>,
    pub proc_filter: String,
    pub hide_sleep: bool,
    pub lock_view: LockView,

    // Advisor
    pub advisor: Vec<Finding>,
    pub advisor_ran: bool,
    pub advisor_kinds: BTreeSet<&'static str>,
    pub advisor_filter: Option<&'static str>,
    /// Composite index proposals derived from the workload.
    pub suggestions: Vec<IndexSuggestion>,

    // Historical
    pub hist_range: HistRange,
    pub hist_metrics: BTreeSet<Metric>,
    pub hist_series: BTreeMap<Metric, Vec<[f64; 2]>>,
    pub hist_downsampled: bool,
    pub hist_stats: Option<StoreStats>,
    pub hist_pending: bool,

    // Alerts
    pub alerts: alerts::Engine,
    pub new_rule_metric: Metric,
    pub new_rule_cmp: alerts::Comparison,
    pub new_rule_threshold: f64,
    pub new_rule_for: f64,

    // SQL console
    pub console_sql: String,
    /// Default database for console statements; `None` runs with no `USE`.
    pub console_schema: Option<String>,
    /// Name the "Save query" button will write to, under the current profile.
    pub query_name: String,
    pub console_result: Option<SqlOutcome>,
    pub console_error: Option<String>,
    pub console_sort: Option<(usize, bool)>,
    pub console_history: Vec<String>,
    /// Target file for the CSV export of the current result.
    pub csv_path_text: String,
    /// Prefix a UTF-8 BOM, which is what Excel needs for non-ASCII.
    pub csv_bom: bool,
    /// Target file for the JSON export of the current result.
    pub json_path_text: String,
    /// Pretty versus compact, and array versus newline-delimited.
    pub json_opts: json_rows::Options,
    /// Target file for the Markdown export of the current result.
    pub markdown_path_text: String,
    /// Target file for the HTML export of the current result.
    pub html_path_text: String,
    /// Standalone document versus a bare `<table>` fragment.
    pub html_opts: html_table::Options,
    /// Target file for the `INSERT` export of the current result.
    pub insert_path_text: String,
    /// Target table and statement shape for the `INSERT` export.
    pub insert_opts: insert_sql::Options,
    /// Outcome of the last export: `(ok, message)`.
    pub export_status: Option<(bool, String)>,

    // Table browser
    pub allow_writes: bool,
    pub schemas: Vec<String>,
    pub tables: Vec<TableInfo>,
    pub table_filter: String,
    pub table_schema: Option<TableSchema>,
    pub browse: BrowseSpec,
    pub browse_result: Option<SqlOutcome>,
    pub row_count: Option<u64>,
    pub editing: Option<CellEdit>,
    /// Column name, value, is-null for a row being inserted.
    pub insert_row: Option<Vec<(String, String, bool)>>,
    pub pending: Vec<Change>,

    // Dump
    pub dump: DumpSpec,
    /// Tables ticked in the dump tab; empty means "everything".
    pub dump_selection: BTreeSet<String>,
    pub dump_tables: Vec<String>,
    pub dump_path_text: String,
    pub dump_running: bool,
    pub dump_progress: Option<(String, usize, usize, u64)>,
    pub dump_result: Option<DumpStats>,

    // Sampling load
    pub paused: bool,
    pub sample_ms: f64,
    pub effective_interval_ms: u64,
    pub backed_off: bool,
    /// Last `ViewNeeds` sent to the collector, so it is only resent on change.
    sent_views: Option<ViewNeeds>,

    // Chrome
    pub tab: Tab,
    pub status_filter: String,
    pub log: VecDeque<String>,
    pub busy: bool,
    pub last_error: Option<String>,
}

impl App {
    pub fn new(collector: Handle, store: store::Handle) -> Self {
        let mut hist_metrics = BTreeSet::new();
        hist_metrics.insert(Metric::Qps);
        hist_metrics.insert(Metric::ThreadsRunning);

        // Preselect whatever was connected last, password included when the
        // user asked for it to be remembered.
        let profiles = Profiles::load();
        let mut cfg = ConnConfig::default();
        let mut remember_password = false;
        if let Some(name) = profiles.last_used.clone()
            && let Some(p) = profiles.get(&name)
        {
            cfg = p.conn.clone();
            remember_password = p.remember_password;
            if remember_password {
                cfg.password = crate::profiles::load_password(&cfg).unwrap_or_default();
            }
        }
        let profile_name = cfg.name.clone();

        Self {
            profiles,
            profile_name,
            remember_password,
            cfg,
            collector,
            store,
            state: ConnState::Disconnected,
            version: ServerVersion::default(),
            caps: Capabilities::default(),
            uptime_s: 0,
            server_label: String::new(),
            limits: ServerLimits::default(),
            innodb_config: InnodbConfig::default(),
            engine_status: EngineStatus::default(),
            long_query_secs: connections::DEFAULT_LONG_SECS,
            idle_secs: connections::DEFAULT_IDLE_SECS,
            prev: None,
            latest: None,
            derived: Derived::default(),
            history: History::default(),
            top_sql: Vec::new(),
            sql_sort: SqlSort::TotalTime,
            sql_filter: String::new(),
            selected_digest: None,
            inspected: None,
            inspect_samples: Vec::new(),
            explain: None,
            lock_waits: Vec::new(),
            transactions: Vec::new(),
            metadata_locks: Vec::new(),
            proc_filter: String::new(),
            hide_sleep: true,
            lock_view: LockView::Blocking,
            advisor: Vec::new(),
            advisor_ran: false,
            advisor_kinds: BTreeSet::new(),
            advisor_filter: None,
            suggestions: Vec::new(),
            hist_range: HistRange::H1,
            hist_metrics,
            hist_series: BTreeMap::new(),
            hist_downsampled: false,
            hist_stats: None,
            hist_pending: false,
            alerts: alerts::Engine::default(),
            new_rule_metric: Metric::Qps,
            new_rule_cmp: alerts::Comparison::Above,
            new_rule_threshold: 1000.0,
            new_rule_for: 30.0,
            console_sql: "SELECT 1;".to_string(),
            console_schema: None,
            query_name: String::new(),
            console_result: None,
            console_error: None,
            console_sort: None,
            console_history: Vec::new(),
            csv_path_text: String::new(),
            csv_bom: true,
            json_path_text: String::new(),
            json_opts: json_rows::Options::default(),
            markdown_path_text: String::new(),
            html_path_text: String::new(),
            html_opts: html_table::Options::default(),
            insert_path_text: String::new(),
            insert_opts: insert_sql::Options::default(),
            export_status: None,
            allow_writes: false,
            schemas: Vec::new(),
            tables: Vec::new(),
            table_filter: String::new(),
            table_schema: None,
            browse: BrowseSpec {
                limit: crate::db::sql::DEFAULT_ROW_LIMIT.min(100),
                ..Default::default()
            },
            browse_result: None,
            row_count: None,
            editing: None,
            insert_row: None,
            pending: Vec::new(),
            dump: DumpSpec::default(),
            dump_selection: BTreeSet::new(),
            dump_tables: Vec::new(),
            dump_path_text: String::new(),
            dump_running: false,
            dump_progress: None,
            dump_result: None,
            paused: false,
            sample_ms: 0.0,
            effective_interval_ms: 0,
            backed_off: false,
            sent_views: None,
            tab: Tab::Dashboard,
            status_filter: String::new(),
            log: VecDeque::new(),
            busy: false,
            last_error: None,
        }
    }

    /// Copies the connection form into a named profile.
    pub fn save_profile(&mut self) {
        let name = self.profile_name.trim().to_string();
        if name.is_empty() {
            self.push_log("profile needs a name");
            return;
        }
        self.cfg.name = name.clone();

        if self.remember_password {
            if let Err(e) = crate::profiles::save_password(&self.cfg) {
                // Saving the profile itself must still succeed.
                self.push_log(format!("password not stored: {e:#}"));
                self.remember_password = false;
            }
        } else {
            crate::profiles::forget_password(&self.cfg);
        }

        self.profiles.upsert(Profile {
            conn: self.cfg.clone(),
            remember_password: self.remember_password,
            // upsert carries the existing queries over.
            queries: Vec::new(),
        });
        self.profiles.last_used = Some(name.clone());
        self.persist_profiles();
        self.push_log(format!(
            "saved profile {name}{}",
            if self.remember_password {
                " (password in OS credential store)"
            } else {
                ""
            }
        ));
    }

    /// Loads a saved profile into the connection form.
    pub fn load_profile(&mut self, name: &str) {
        let Some(p) = self.profiles.get(name).cloned() else {
            return;
        };
        self.cfg = p.conn;
        self.remember_password = p.remember_password;
        self.profile_name = self.cfg.name.clone();
        self.cfg.password = if p.remember_password {
            crate::profiles::load_password(&self.cfg).unwrap_or_default()
        } else {
            String::new()
        };
        self.push_log(format!("loaded profile {name}"));
    }

    pub fn delete_profile(&mut self, name: &str) {
        if let Some(p) = self.profiles.remove(name) {
            crate::profiles::forget_password(&p.conn);
            self.persist_profiles();
            self.push_log(format!("deleted profile {name}"));
        }
    }

    /// Statements saved against the profile currently loaded in the form.
    pub fn saved_queries(&self) -> &[SavedQuery] {
        self.profiles
            .get(&self.profile_name)
            .map(|p| p.queries.as_slice())
            .unwrap_or(&[])
    }

    /// Saves the console text against the current profile.
    pub fn save_query(&mut self) {
        let name = self.query_name.trim().to_string();
        let sql = self.console_sql.trim().to_string();
        if name.is_empty() || sql.is_empty() {
            self.push_log("a saved query needs a name and a statement");
            return;
        }
        let profile_name = self.profile_name.clone();
        let schema = self.console_schema.clone();

        let Some(profile) = self.profiles.get_mut(&profile_name) else {
            self.push_log("save the connection as a profile first — queries hang off it");
            return;
        };
        profile.put_query(SavedQuery {
            name: name.clone(),
            sql,
            schema,
        });
        self.persist_profiles();
        self.push_log(format!("saved query \"{name}\" to profile {profile_name}"));
    }

    pub fn load_query(&mut self, name: &str) {
        let Some(q) = self
            .profiles
            .get(&self.profile_name)
            .and_then(|p| p.queries.iter().find(|q| q.name == name))
            .cloned()
        else {
            return;
        };
        self.console_sql = q.sql;
        self.query_name = q.name;
        // Only adopt the saved database if the server still reports it.
        if let Some(schema) = q.schema
            && self.schemas.contains(&schema)
        {
            self.console_schema = Some(schema);
        }
    }

    pub fn delete_query(&mut self, name: &str) {
        let profile_name = self.profile_name.clone();
        let removed = self
            .profiles
            .get_mut(&profile_name)
            .and_then(|p| p.remove_query(name))
            .is_some();
        if removed {
            self.persist_profiles();
            self.push_log(format!("deleted query \"{name}\""));
        }
    }

    fn persist_profiles(&mut self) {
        if let Err(e) = self.profiles.save() {
            self.push_log(format!("could not write profiles: {e:#}"));
        }
    }

    pub fn push_log(&mut self, msg: impl Into<String>) {
        if self.log.len() >= MAX_LOG {
            self.log.pop_front();
        }
        self.log.push_back(msg.into());
    }

    pub fn is_connected(&self) -> bool {
        self.state == ConnState::Connected
    }

    /// Asks the collector for one digest and switches to the inspector.
    pub fn inspect(&mut self, digest: &str) {
        self.selected_digest = Some(digest.to_string());
        self.inspected = None;
        self.inspect_samples.clear();
        self.explain = None;
        self.collector
            .send(Command::InspectDigest(digest.to_string()));
        self.tab = Tab::Inspector;
    }

    /// Re-runs the Historical Metrics query for the current range/selection.
    pub fn request_history(&mut self) {
        if self.server_label.is_empty() {
            return;
        }
        let to_ms = store::now_ms();
        let from_ms = to_ms - self.hist_range.secs() * 1000;
        self.hist_pending = true;
        self.store.send(StoreCmd::Query {
            server: self.server_label.clone(),
            metrics: self.hist_metrics.iter().copied().collect(),
            from_ms,
            to_ms,
        });
        self.store.send(StoreCmd::Stats);
    }

    /// Re-runs the current browser page.
    pub fn refresh_browse(&mut self) {
        if self.browse.table.is_empty() {
            return;
        }
        self.collector
            .send(Command::Browse(Box::new(self.browse.clone())));
    }

    /// Opens a table filtered to one column value — the jump from a console
    /// result cell into something editable.
    pub fn open_table_filtered(
        &mut self,
        schema: &str,
        table: &str,
        column: &str,
        value: Option<&str>,
    ) {
        let filter = match crate::db::sql::equality_filter(column, value) {
            Ok(f) => f,
            Err(e) => {
                self.push_log(format!("cannot filter on {column}: {e:#}"));
                return;
            }
        };
        self.open_table(schema, table);
        self.browse.filter = filter.clone();
        self.refresh_browse();
        self.push_log(format!("opened {schema}.{table} WHERE {filter}"));
    }

    /// Opens a table in the browser: schema, first page, and column metadata.
    pub fn open_table(&mut self, schema: &str, table: &str) {
        self.browse.schema = schema.to_string();
        self.browse.table = table.to_string();
        self.browse.filter.clear();
        self.browse.offset = 0;
        self.browse.order_by = None;
        self.browse.descending = false;
        self.browse_result = None;
        self.table_schema = None;
        self.row_count = None;
        self.editing = None;
        self.pending
            .retain(|c| !matches!(c, Change::Update { .. } | Change::Delete { .. }));
        self.collector.send(Command::DescribeTable {
            schema: schema.to_string(),
            table: table.to_string(),
        });
        self.refresh_browse();
        self.tab = Tab::Tables;
    }

    /// Primary-key values for one row of the current page.
    pub fn row_key(&self, row: usize) -> Option<Vec<(String, Option<String>)>> {
        let schema = self.table_schema.as_ref()?;
        let grid = &self.browse_result.as_ref()?.grid;
        let cells = grid.rows.get(row)?;

        let mut key = Vec::new();
        for pk in schema.primary_key() {
            let idx = grid.columns.iter().position(|c| c == &pk.name)?;
            key.push((pk.name.clone(), cells.get(idx)?.clone()));
        }
        (!key.is_empty()).then_some(key)
    }

    pub fn set_write_mode(&mut self, on: bool) {
        self.allow_writes = on;
        self.collector.send(Command::SetWriteMode(on));
        if !on {
            self.pending.clear();
            self.editing = None;
        }
        self.push_log(if on {
            "writes ENABLED — statements and row edits can modify this server"
        } else {
            "read-only mode"
        });
    }

    /// What the visible screen actually needs collected. Everything else is
    /// left alone, which is most of the per-tick cost on a busy server.
    fn view_needs(&self) -> ViewNeeds {
        match self.tab {
            Tab::Dashboard => ViewNeeds {
                processlist: false,
                digests: true,
                locks: true,
                innodb: false,
            },
            Tab::TopSql | Tab::IndexAdvisor => ViewNeeds {
                processlist: false,
                digests: true,
                locks: false,
                innodb: false,
            },
            Tab::Locks => ViewNeeds {
                processlist: true,
                digests: false,
                locks: true,
                innodb: false,
            },
            Tab::Connections => ViewNeeds {
                processlist: true,
                digests: false,
                locks: false,
                innodb: false,
            },
            Tab::Innodb => ViewNeeds {
                processlist: false,
                digests: false,
                locks: false,
                innodb: true,
            },
            Tab::Inspector | Tab::Historical | Tab::Alerts | Tab::Sql | Tab::Tables | Tab::Dump => {
                ViewNeeds::minimal()
            }
        }
    }

    /// Pushes the collection scope down when the active screen changes.
    fn sync_views(&mut self) {
        let needs = self.view_needs();
        if self.sent_views != Some(needs) {
            self.sent_views = Some(needs);
            self.collector.send(Command::SetViews(needs));
        }
    }

    /// Points the dump at a database and loads its table list.
    pub fn set_dump_schema(&mut self, schema: &str) {
        self.dump.schema = schema.to_string();
        self.dump_tables.clear();
        self.dump_selection.clear();
        self.dump_path_text = crate::db::dump::suggested_path(schema)
            .display()
            .to_string();
        self.collector.send(Command::ListTables(schema.to_string()));
    }

    pub fn start_dump(&mut self) {
        if self.dump.schema.is_empty() {
            self.push_log("pick a database to dump");
            return;
        }
        let path = std::path::PathBuf::from(self.dump_path_text.trim());
        if path.as_os_str().is_empty() {
            self.push_log("the dump needs a file path");
            return;
        }
        if !crate::db::dump::path_is_usable(&path) {
            self.push_log(format!(
                "cannot write there — {} does not exist",
                path.parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            ));
            return;
        }

        self.dump.path = path;
        self.dump.tables = self.dump_selection.iter().cloned().collect();
        self.dump_result = None;
        self.dump_running = true;
        self.dump_progress = None;
        self.collector
            .send(Command::RunDump(Box::new(self.dump.clone())));
        self.push_log(format!(
            "dumping {} ({}) to {}",
            self.dump.schema,
            self.dump.mode.label(),
            self.dump.path.display()
        ));
    }

    pub fn cancel_dump(&mut self) {
        self.collector.send(Command::CancelDump);
        self.push_log("cancelling dump…");
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.collector.send(Command::SetPaused(paused));
        self.push_log(if paused {
            "sampling paused"
        } else {
            "sampling resumed"
        });
    }

    fn on_sample(&mut self, s: Sample) {
        if let Some(prev) = &self.latest {
            self.derived = Derived::between(prev, &s);
            self.derived.conn_usage_pct =
                connections::usage_pct(s.stat("Threads_connected"), self.limits.max_connections);
            self.history.push(s.t, &self.derived);

            for ev in self.alerts.evaluate(s.t, s.wall_ms, &self.derived) {
                let verb = match ev.transition {
                    alerts::Transition::Fired => "FIRING",
                    alerts::Transition::Resolved => "resolved",
                };
                self.push_log(format!("{verb}: {} (value {:.2})", ev.rule, ev.value));
            }

            if !self.server_label.is_empty() {
                self.store.send(StoreCmd::Write {
                    server: self.server_label.clone(),
                    wall_ms: s.wall_ms,
                    values: Metric::ALL
                        .iter()
                        .map(|m| (*m, m.value(&self.derived)))
                        .collect(),
                });
            }
        }
        self.uptime_s = s.stat("Uptime");
        self.prev = self.latest.take();
        self.latest = Some(s);
    }

    fn on_advisor(&mut self, data: AdvisorData) {
        self.advisor = advisor::analyze(&AdvisorInput {
            indexes: &data.indexes,
            usage: &data.usage,
            scans: &data.scans,
            no_pk: &data.no_pk,
            digests: &self.top_sql,
            caps: self.caps,
            uptime_s: data.uptime_s,
        });
        // Digest text plus the real index list is everything the suggester
        // needs; a statement that already has a usable index is dropped there.
        self.suggestions = suggest::suggest(&self.top_sql, &data.indexes, MIN_SUGGEST_EXECUTIONS);
        self.advisor_kinds = self.advisor.iter().map(|f| f.kind).collect();
        if let Some(k) = self.advisor_filter
            && !self.advisor_kinds.contains(k)
        {
            self.advisor_filter = None;
        }
        self.advisor_ran = true;
        self.push_log(format!(
            "index advisor: {} findings, {} index suggestions",
            self.advisor.len(),
            self.suggestions.len()
        ));
    }

    fn pump(&mut self) {
        for ev in self.collector.drain() {
            match ev {
                Event::Connecting(label) => {
                    self.state = ConnState::Connecting;
                    self.push_log(format!("connecting to {label}"));
                }
                Event::Connected {
                    version,
                    caps,
                    uptime_s,
                    label,
                    limits,
                    innodb_config,
                } => {
                    self.push_log(format!("connected: {version} [{}]", caps.summary()));
                    self.state = ConnState::Connected;
                    self.version = version;
                    self.caps = caps;
                    self.uptime_s = uptime_s;
                    self.server_label = label;
                    self.limits = limits;
                    self.innodb_config = *innodb_config;
                    self.engine_status = EngineStatus::default();
                    if limits.max_connections == 0 {
                        self.push_log("could not read max_connections — usage % unavailable");
                    }
                    self.prev = None;
                    self.latest = None;
                    self.history.clear();
                    self.alerts.reset_states();
                    self.advisor.clear();
                    self.suggestions.clear();
                    self.advisor_ran = false;
                    self.request_history();
                    self.collector
                        .send(Command::SetWriteMode(self.allow_writes));
                    self.collector.send(Command::ListSchemas);
                }
                Event::Disconnected => {
                    self.state = ConnState::Disconnected;
                    self.push_log("disconnected");
                }
                Event::Error(e) => {
                    self.last_error = Some(e.clone());
                    self.push_log(format!("error: {e}"));
                }
                Event::Sample(s) => self.on_sample(*s),
                Event::TopQueries(rows) => self.top_sql = rows,
                Event::LockWaits(rows) => self.lock_waits = rows,
                Event::Transactions(rows) => self.transactions = rows,
                Event::MetadataLocks(rows) => self.metadata_locks = rows,
                Event::DigestDetail {
                    digest,
                    row,
                    samples,
                } => {
                    if self.selected_digest.as_deref() == Some(digest.as_str()) {
                        self.inspected = row.map(|b| *b);
                        self.inspect_samples = samples;
                    }
                }
                Event::Explain(grid) => self.explain = Some(grid),
                Event::Advisor(data) => self.on_advisor(*data),
                Event::Busy(b) => self.busy = b,
                Event::Innodb(status) => self.engine_status = *status,
                Event::DumpProgress {
                    table,
                    table_index,
                    table_count,
                    rows,
                } => {
                    self.dump_running = true;
                    self.dump_progress = Some((table, table_index, table_count, rows));
                }
                Event::DumpDone(stats) => {
                    self.dump_running = false;
                    self.dump_progress = None;
                    self.push_log(format!(
                        "dump {}: {} tables, {} rows, {} → {}",
                        if stats.cancelled {
                            "cancelled"
                        } else {
                            "finished"
                        },
                        stats.tables,
                        stats.rows,
                        ui::fmt_bytes(stats.bytes as f64),
                        stats.path.display()
                    ));
                    self.dump_result = Some(*stats);
                }
                Event::Load {
                    sample_ms,
                    interval_ms,
                    backed_off,
                } => {
                    self.sample_ms = sample_ms;
                    self.effective_interval_ms = interval_ms;
                    if backed_off && !self.backed_off {
                        self.push_log(format!(
                            "server is slow to answer — polling every {:.1}s until it recovers",
                            interval_ms as f64 / 1000.0
                        ));
                    }
                    self.backed_off = backed_off;
                }
                Event::SqlResult(out) => {
                    self.console_error = None;
                    self.console_sort = None;
                    self.console_result = Some(*out);
                    // A new result makes the last export message stale, and
                    // the suggested target table belongs to the old grid.
                    self.export_status = None;
                    self.insert_opts.table.clear();
                }
                Event::SqlError(e) => {
                    self.dump_running = false;
                    self.dump_progress = None;
                    self.console_error = Some(e.clone());
                    self.push_log(format!("sql: {e}"));
                }
                Event::Schemas(v) => {
                    if let Some(cur) = &self.console_schema
                        && !v.contains(cur)
                    {
                        self.console_schema = None;
                    }
                    self.schemas = v;
                }
                Event::Tables { schema, tables } => {
                    if schema == self.dump.schema {
                        self.dump_tables = tables.iter().map(|t| t.name.clone()).collect();
                        self.dump_selection
                            .retain(|name| self.dump_tables.contains(name));
                    }
                    if schema == self.browse.schema {
                        self.tables = tables;
                    }
                }
                Event::TableSchema(t) => self.table_schema = Some(*t),
                Event::BrowseResult { spec, outcome } => {
                    // Ignore a page that a newer request already superseded.
                    if spec.schema == self.browse.schema && spec.table == self.browse.table {
                        self.browse_result = Some(*outcome);
                    }
                }
                Event::RowCount(n) => self.row_count = Some(n),
                Event::ChangesApplied(n) => {
                    self.pending.clear();
                    self.editing = None;
                    self.push_log(format!("applied edits: {n} rows affected"));
                    self.refresh_browse();
                }
            }
        }

        for ev in self.store.drain() {
            match ev {
                StoreEvent::Series {
                    metric,
                    points,
                    downsampled,
                } => {
                    self.hist_series.insert(metric, points);
                    self.hist_downsampled = downsampled;
                }
                StoreEvent::QueryDone { server } => {
                    if server == self.server_label {
                        self.hist_pending = false;
                    }
                }
                StoreEvent::Stats {
                    raw_rows,
                    rollup_rows,
                    oldest_ms,
                    file_bytes,
                    path,
                } => {
                    self.hist_stats = Some(StoreStats {
                        raw_rows,
                        rollup_rows,
                        oldest_ms,
                        file_bytes,
                        path,
                    });
                }
                StoreEvent::Exported { path, rows } => {
                    self.push_log(format!("exported {rows} rows to {}", path.display()));
                }
                StoreEvent::Error(e) => {
                    self.last_error = Some(e.clone());
                    self.push_log(format!("store: {e}"));
                }
            }
        }
    }

    fn profile_bar(&mut self, ui: &mut egui::Ui) {
        let mut load: Option<String> = None;
        let mut delete: Option<String> = None;

        ui.horizontal_wrapped(|ui| {
            ui.label("Profile");
            let selected = if self.profile_name.is_empty() {
                "(unsaved)".to_string()
            } else {
                self.profile_name.clone()
            };
            egui::ComboBox::from_id_salt("profile_pick")
                .width(150.0)
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    for name in self.profiles.names() {
                        if ui
                            .selectable_label(self.profile_name == name, &name)
                            .clicked()
                        {
                            load = Some(name);
                        }
                    }
                    if self.profiles.items.is_empty() {
                        ui.label(RichText::new("none saved").weak());
                    }
                });

            ui.add(
                egui::TextEdit::singleline(&mut self.profile_name)
                    .hint_text("name")
                    .desired_width(120.0),
            );
            if ui
                .button("Save")
                .on_hover_text("store these connection settings under this name")
                .clicked()
            {
                self.save_profile();
            }
            let known = self.profiles.get(&self.profile_name).is_some();
            if ui.add_enabled(known, egui::Button::new("Delete")).clicked() {
                delete = Some(self.profile_name.clone());
            }

            let mut remember = self.remember_password;
            if ui
                .checkbox(&mut remember, "remember password")
                .on_hover_text(
                    "Password goes to the OS credential store (Windows Credential \
                     Manager / Keychain / Secret Service), never to the profile file.",
                )
                .changed()
            {
                self.remember_password = remember;
                if !remember {
                    crate::profiles::forget_password(&self.cfg);
                }
            }
        });

        if let Some(name) = load {
            self.load_profile(&name);
        }
        if let Some(name) = delete {
            self.delete_profile(&name);
        }
    }

    fn connection_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Host");
            ui.add(egui::TextEdit::singleline(&mut self.cfg.host).desired_width(120.0));
            ui.label("Port");
            ui.add(egui::DragValue::new(&mut self.cfg.port).range(1..=65535));
            ui.label("User");
            ui.add(egui::TextEdit::singleline(&mut self.cfg.user).desired_width(90.0));
            ui.label("Pass");
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.password)
                    .password(true)
                    .desired_width(110.0),
            );
            ui.checkbox(&mut self.cfg.use_tls, "TLS");

            ui.label("Every");
            let mut secs = self.cfg.interval_ms as f64 / 1000.0;
            if ui
                .add(
                    egui::DragValue::new(&mut secs)
                        .range(0.2..=60.0)
                        .speed(0.1)
                        .suffix(" s"),
                )
                .changed()
            {
                self.cfg.interval_ms = (secs * 1000.0) as u64;
                self.collector
                    .send(Command::SetInterval(self.cfg.interval_ms));
            }

            ui.separator();
            match self.state {
                ConnState::Connected => {
                    if ui.button("Disconnect").clicked() {
                        self.collector.send(Command::Disconnect);
                    }
                }
                ConnState::Connecting => {
                    ui.add_enabled(false, egui::Button::new("Connecting…"));
                }
                ConnState::Disconnected => {
                    if ui.button("Connect").clicked() {
                        self.collector
                            .send(Command::Connect(Box::new(self.cfg.clone())));
                    }
                }
            }

            ui.separator();
            let (dot, text) = match self.state {
                ConnState::Connected => (
                    ui::GREEN,
                    format!(
                        "{}  ·  up {}  ·  {}",
                        self.version,
                        ui::fmt_duration(self.uptime_s),
                        self.caps.summary()
                    ),
                ),
                ConnState::Connecting => (ui::AMBER, "connecting…".to_string()),
                ConnState::Disconnected => (ui::RED, "not connected".to_string()),
            };
            ui.colored_label(dot, "●");
            ui.label(RichText::new(text).small());
            if self.allow_writes {
                ui.colored_label(ui::RED, RichText::new("WRITES ENABLED").small().strong());
            }

            if self.state == ConnState::Connected {
                ui.separator();
                let mut paused = self.paused;
                if ui
                    .checkbox(&mut paused, "pause")
                    .on_hover_text("stop polling without dropping the connection")
                    .changed()
                {
                    self.set_paused(paused);
                }
                if self.effective_interval_ms > 0 {
                    let text = RichText::new(format!(
                        "poll {:.1}s · sample {:.0} ms",
                        self.effective_interval_ms as f64 / 1000.0,
                        self.sample_ms
                    ))
                    .small();
                    ui.label(if self.backed_off {
                        text.color(ui::AMBER)
                    } else {
                        text.weak()
                    })
                    .on_hover_text(
                        "Collection is spread across ticks and backs off on its own when \
                         the server answers slowly.",
                    );
                }
            }
            if self.busy {
                ui.spinner();
            }
        });
    }

    fn alert_banner(&mut self, ui: &mut egui::Ui) {
        let firing: Vec<String> = self
            .alerts
            .firing()
            .map(|(r, _)| {
                format!(
                    "{} = {:.1}",
                    r.metric.label(),
                    r.metric.value(&self.derived)
                )
            })
            .collect();
        if firing.is_empty() {
            return;
        }

        let mut open_alerts = false;
        egui::Frame::new()
            .fill(Color32::from_rgb(70, 26, 26))
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui::RED, RichText::new("● ALERT").strong());
                    ui.label(firing.join("   ·   "));
                    if ui.small_button("open Alerts").clicked() {
                        open_alerts = true;
                    }
                });
            });
        if open_alerts {
            self.tab = Tab::Alerts;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        self.sync_views();

        egui::Panel::top("conn").show(ui, |ui| {
            ui.add_space(4.0);
            self.profile_bar(ui);
            self.connection_bar(ui);
            self.alert_banner(ui);
            ui.add_space(4.0);
        });

        egui::Panel::bottom("log").show(ui, |ui| {
            ui.horizontal(|ui| {
                let last = self.log.back().cloned().unwrap_or_default();
                ui.label(RichText::new(last).small());
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let mut switch_to = None;
            ui.horizontal_wrapped(|ui| {
                for tab in Tab::ORDER {
                    let mut text = RichText::new(tab.label());
                    if tab == Tab::Alerts && self.alerts.firing_count() > 0 {
                        text = text.color(ui::RED).strong();
                    }
                    if ui.selectable_label(self.tab == tab, text).clicked() {
                        switch_to = Some(tab);
                    }
                }
            });
            if let Some(tab) = switch_to {
                self.tab = tab;
                if tab == Tab::Historical {
                    self.request_history();
                }
            }
            ui.separator();

            match self.tab {
                Tab::Dashboard => self.dashboard_tab(ui),
                Tab::TopSql => self.top_sql_tab(ui),
                Tab::Inspector => self.inspector_tab(ui),
                Tab::Locks => self.lock_monitor_tab(ui),
                Tab::IndexAdvisor => self.index_advisor_tab(ui),
                Tab::Historical => self.historical_tab(ui),
                Tab::Alerts => self.alerts_tab(ui),
                Tab::Sql => self.sql_console_tab(ui),
                Tab::Tables => self.table_browser_tab(ui),
                Tab::Dump => self.dump_tab(ui),
                Tab::Connections => self.connections_tab(ui),
                Tab::Innodb => self.innodb_tab(ui),
            }
        });

        if self.state != ConnState::Disconnected {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
    }
}
