//! Background sampler. Owns the connection pool and does all database work off
//! the UI thread; the GUI only drains `Event`s and sends `Command`s.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use mysql_async::Pool;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{info, warn};

use super::ConnConfig;
use super::dump::{self, DumpSpec, DumpStats};
use super::queries::{
    self, DigestRow, Grid, IndexDef, IndexUsage, LockWait, MdlRow, NoPkTable, ScanRow,
    ServerLimits, StatementSample, TrxRow,
};
use super::schedule::{HeavyTask, Scheduler, ViewNeeds};
use super::sql::{self, BrowseSpec, Change, SqlOutcome, StatementKind, TableSchema};
use super::version::{Capabilities, ServerVersion};
use crate::deadlock::{self, Deadlock};
use crate::innodb::{EngineStatus, InnodbConfig, parse_engine_status};
use crate::model::Sample;
use crate::replication::{self, Replica, Source};
use crate::store;

/// Digest rows pulled per heavy tick.
const TOP_SQL_LIMIT: u32 = 100;
/// Executions shown in the query inspector.
const SAMPLE_LIMIT: u32 = 20;

/// How long a connect attempt may run before it is abandoned. Without this the
/// wait is the operating system's TCP timeout, which on an unreachable host is
/// twenty seconds or more with nothing to cancel it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A sample that takes this long is abandoned and the next tick tries again. A
/// wedged server must never hold the sampler open indefinitely.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(20);
/// Bound for metadata reads: schema and table lists, `EXPLAIN`, the inspector
/// and the advisor. Statements the user wrote are deliberately not bounded —
/// see `Command::CancelSql`.
const META_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `fut`, giving up after `limit`. The abandoned future is dropped, which
/// returns its pooled connection to the recycler for cleanup.
async fn with_timeout<T>(
    limit: Duration,
    what: &str,
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => anyhow::bail!("{what} timed out after {}s", limit.as_secs()),
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Connect(Box<ConnConfig>),
    Disconnect,
    SetInterval(u64),
    /// Force the digest/lock fetch on the next tick.
    RefreshHeavy,
    /// `KILL <id>`.
    KillThread(u64),
    /// Full detail for one digest (Query Inspector).
    InspectDigest(String),
    /// `EXPLAIN` a statement in the given schema.
    Explain {
        schema: String,
        sql: String,
    },
    /// Collect everything the index advisor needs. Expensive — on demand only.
    RunAdvisor,
    /// Tell the sampler which panels are visible, so the rest is not collected.
    SetViews(ViewNeeds),
    /// Export structure and/or data to a file.
    RunDump(Box<DumpSpec>),
    /// Ask a running dump to stop at the next batch.
    CancelDump,
    /// `KILL QUERY` the console statement that is running, if any. The
    /// connection survives, so the session and its default database do not.
    CancelSql,
    /// Stop sampling entirely without dropping the connection.
    SetPaused(bool),

    /// Allow statements that modify the server. Off until the user opts in.
    SetWriteMode(bool),
    /// Run one statement from the SQL console, optionally against a chosen
    /// default database.
    RunSql {
        schema: Option<String>,
        sql: String,
    },
    ListSchemas,
    ListTables(String),
    DescribeTable {
        schema: String,
        table: String,
    },
    Browse(Box<BrowseSpec>),
    CountRows(Box<BrowseSpec>),
    /// Apply queued row edits in a single transaction.
    ApplyChanges(Vec<Change>),
}

/// Everything the advisor analyses, fetched in one pass.
#[derive(Debug, Default)]
pub struct AdvisorData {
    pub indexes: Vec<IndexDef>,
    pub usage: Vec<IndexUsage>,
    pub scans: Vec<ScanRow>,
    pub no_pk: Vec<NoPkTable>,
    pub uptime_s: u64,
}

#[derive(Debug)]
pub enum Event {
    Connecting(String),
    Connected {
        version: ServerVersion,
        caps: Capabilities,
        uptime_s: u64,
        label: String,
        limits: ServerLimits,
        innodb_config: Box<InnodbConfig>,
    },
    Disconnected,
    Error(String),
    Sample(Box<Sample>),
    TopQueries(Vec<DigestRow>),
    LockWaits(Vec<LockWait>),
    Transactions(Vec<TrxRow>),
    MetadataLocks(Vec<MdlRow>),
    DigestDetail {
        digest: String,
        row: Option<Box<DigestRow>>,
        samples: Vec<StatementSample>,
    },
    Explain(Grid),
    Advisor(Box<AdvisorData>),
    /// Long-running work started/finished, for the busy indicator.
    Busy(bool),
    Replication {
        replicas: Vec<Replica>,
        source: Option<Source>,
        /// `SHOW REPLICAS` as it came back — the columns differ by version and
        /// there is little to derive from them.
        connected: Grid,
        /// Set when the server refused the statement, which is almost always a
        /// missing `REPLICATION CLIENT` grant.
        error: Option<String>,
    },
    Innodb {
        status: Box<EngineStatus>,
        /// The latest deadlock InnoDB remembers, if it has seen one since the
        /// server started.
        deadlock: Option<Box<Deadlock>>,
    },
    DumpProgress {
        table: String,
        table_index: usize,
        table_count: usize,
        rows: u64,
    },
    DumpDone(Box<DumpStats>),
    /// The dump ended badly. Its own event because jobs now run side by side:
    /// a failing table list must not clear the running dump's state.
    DumpFailed(String),
    /// Sampling cost feedback: how long a poll took and the interval in force.
    Load {
        sample_ms: f64,
        interval_ms: u64,
        backed_off: bool,
    },

    /// A console statement started or finished. Separate from `Busy`, which
    /// now covers several jobs at once: only this one can be cancelled.
    SqlBusy(bool),
    SqlResult(Box<SqlOutcome>),
    /// Console/browser failure, kept apart from the sampler's error stream.
    SqlError(String),
    Schemas(Vec<String>),
    Tables {
        schema: String,
        tables: Vec<sql::TableInfo>,
    },
    TableSchema(Box<TableSchema>),
    BrowseResult {
        spec: Box<BrowseSpec>,
        outcome: Box<SqlOutcome>,
    },
    RowCount(u64),
    ChangesApplied(u64),
}

/// GUI-side end of the collector.
pub struct Handle {
    tx: UnboundedSender<Command>,
    rx: UnboundedReceiver<Event>,
}

impl Handle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    /// Non-blocking drain, called once per frame.
    pub fn drain(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }
}

#[derive(Clone)]
struct Session {
    pool: Pool,
    caps: Capabilities,
}

impl Session {
    async fn close(self) {
        let _ = self.pool.disconnect().await;
    }
}

/// Event sink for a spawned job.
///
/// Long work runs off the command loop, so a result can arrive after the user
/// has disconnected or connected somewhere else. Each job carries the epoch of
/// the session it was started for; once that epoch is stale its events are
/// dropped rather than shown against a different server.
#[derive(Clone)]
struct JobEv {
    ev: UnboundedSender<Event>,
    epoch: u64,
    current: Arc<AtomicU64>,
}

impl JobEv {
    fn is_current(&self) -> bool {
        self.epoch == self.current.load(Ordering::SeqCst)
    }

    fn send(&self, e: Event) {
        if self.is_current() {
            let _ = self.ev.send(e);
        }
    }
}

/// Raises the busy indicator while at least one job is running. Jobs overlap
/// now, so this counts rather than toggling: the indicator clears when the
/// last one finishes, including when a job is dropped or panics.
struct BusyGuard {
    n: Arc<AtomicUsize>,
    ev: UnboundedSender<Event>,
}

impl BusyGuard {
    fn new(n: &Arc<AtomicUsize>, ev: &UnboundedSender<Event>) -> Self {
        if n.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = ev.send(Event::Busy(true));
        }
        Self {
            n: n.clone(),
            ev: ev.clone(),
        }
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if self.n.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _ = self.ev.send(Event::Busy(false));
        }
    }
}

/// Connection id of the console statement that is running, if any, so that
/// `Command::CancelSql` can `KILL QUERY` it. Zero means nothing is running.
/// Only the console registers here: sampler and metadata reads are bounded by
/// a timeout instead, and cancelling those would just break the display.
#[derive(Clone, Default)]
struct Inflight(Arc<AtomicU32>);

impl Inflight {
    fn running_id(&self) -> Option<u32> {
        match self.0.load(Ordering::SeqCst) {
            0 => None,
            id => Some(id),
        }
    }

    /// Registers `id` until the returned guard drops.
    fn register(&self, id: u32) -> InflightGuard {
        self.0.store(id, Ordering::SeqCst);
        InflightGuard(self.0.clone(), id)
    }
}

struct InflightGuard(Arc<AtomicU32>, u32);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        // Compare first: a slower statement may already have claimed the slot.
        let _ = self
            .0
            .compare_exchange(self.1, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// Spawns the collector on the given runtime handle.
pub fn spawn(rt: &tokio::runtime::Handle) -> Handle {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (ev_tx, ev_rx) = unbounded_channel();
    rt.spawn(run(cmd_rx, ev_tx));
    Handle {
        tx: cmd_tx,
        rx: ev_rx,
    }
}

fn new_ticker(ms: u64) -> tokio::time::Interval {
    let mut t = tokio::time::interval(Duration::from_millis(ms.max(200)));
    // Delay, not Burst: a slow server must never cause a pile-up of catch-up
    // ticks that all fire at once.
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    t
}

/// Starts `f` off the command loop.
///
/// Long work — a dump, a console statement, the advisor — used to be awaited
/// inside the `select!`, which stopped sampling and left every later command,
/// cancellation included, sitting unread in the channel. Each job now gets its
/// own task and its own pooled connection; the loop returns to the select
/// immediately.
fn spawn_job<F, Fut>(
    session: &Session,
    ev: &UnboundedSender<Event>,
    epoch: &Arc<AtomicU64>,
    busy: &Arc<AtomicUsize>,
    show_busy: bool,
    f: F,
) where
    F: FnOnce(Session, JobEv) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let session = session.clone();
    let jev = JobEv {
        ev: ev.clone(),
        epoch: epoch.load(Ordering::SeqCst),
        current: epoch.clone(),
    };
    // Taken before the spawn so the indicator is up by the time the UI looks.
    let guard = show_busy.then(|| BusyGuard::new(busy, ev));
    tokio::spawn(async move {
        let _guard = guard;
        f(session, jev).await;
    });
}

async fn run(mut cmd_rx: UnboundedReceiver<Command>, ev: UnboundedSender<Event>) {
    let started = Instant::now();
    let mut session: Option<Session> = None;
    let mut sched = Scheduler::new(1000);
    let mut ticker = new_ticker(sched.interval_ms());
    let mut allow_writes = false;
    let mut server_version = ServerVersion::default();
    let dump_cancel = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicUsize::new(0));
    let inflight = Inflight::default();
    // Bumped on every connect and disconnect. Jobs started against an earlier
    // session finish into a stale epoch and their results are discarded.
    let epoch = Arc::new(AtomicU64::new(0));

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Command::Connect(cfg) => {
                        epoch.fetch_add(1, Ordering::SeqCst);
                        close_in_background(session.take());
                        let _ = ev.send(Event::Connecting(cfg.label()));
                        match with_timeout(CONNECT_TIMEOUT, "connect", connect(&cfg)).await {
                            Ok((s, version, uptime_s, limits, innodb_config)) => {
                                info!(%version, "connected");
                                server_version = version.clone();
                                let _ = ev.send(Event::Connected {
                                    version,
                                    caps: s.caps,
                                    uptime_s,
                                    label: cfg.label(),
                                    limits,
                                    innodb_config: Box::new(innodb_config),
                                });
                                session = Some(s);
                                sched = Scheduler::new(cfg.interval_ms);
                                ticker = new_ticker(sched.interval_ms());
                            }
                            Err(e) => {
                                warn!("connect failed: {e:#}");
                                let _ = ev.send(Event::Error(format!("{e:#}")));
                                let _ = ev.send(Event::Disconnected);
                            }
                        }
                    }
                    Command::Disconnect => {
                        epoch.fetch_add(1, Ordering::SeqCst);
                        // Not awaited: closing the pool waits for every
                        // connection to come back, and a console statement
                        // still running holds one.
                        close_in_background(session.take());
                        let _ = ev.send(Event::Disconnected);
                    }
                    Command::SetInterval(ms) => {
                        sched.set_base_interval(ms);
                        ticker = new_ticker(sched.interval_ms());
                    }
                    Command::SetViews(needs) => sched.set_needs(needs),

                    Command::CancelDump => {
                        dump_cancel.store(true, Ordering::Relaxed);
                    }

                    Command::CancelSql => {
                        let Some(s) = &session else { continue };
                        let Some(id) = inflight.running_id() else { continue };
                        spawn_job(s, &ev, &epoch, &busy, false, move |s, jev| async move {
                            // The statement itself reports the interruption, so
                            // only a failure to cancel is worth saying.
                            if let Err(e) = kill_query(&s, id).await {
                                jev.send(Event::SqlError(format!("cancel: {e:#}")));
                            }
                        });
                    }

                    Command::RunDump(spec) => {
                        let Some(s) = &session else { continue };
                        dump_cancel.store(false, Ordering::Relaxed);
                        let cancel = dump_cancel.clone();
                        let version = server_version.to_string();
                        spawn_job(s, &ev, &epoch, &busy, true, move |s, jev| async move {
                            match run_dump(&s, &spec, &version, cancel, &jev).await {
                                Ok(stats) => jev.send(Event::DumpDone(Box::new(stats))),
                                Err(e) => jev.send(Event::DumpFailed(format!("{e:#}"))),
                            }
                        });
                    }
                    Command::SetPaused(paused) => {
                        sched.set_paused(paused);
                        info!(paused, "sampling pause toggled");
                    }
                    Command::RefreshHeavy => sched.force_heavy(),
                    Command::KillThread(id) => {
                        let Some(s) = &session else { continue };
                        spawn_job(s, &ev, &epoch, &busy, false, move |s, jev| async move {
                            if let Err(e) = kill(&s, id).await {
                                jev.send(Event::Error(format!("KILL {id}: {e:#}")));
                            }
                        });
                    }
                    Command::InspectDigest(digest) => {
                        let Some(s) = &session else { continue };
                        spawn_job(s, &ev, &epoch, &busy, false, move |s, jev| async move {
                            match with_timeout(META_TIMEOUT, "inspect", inspect(&s, &digest)).await {
                                Ok((row, samples)) => jev.send(Event::DigestDetail {
                                    digest,
                                    row: row.map(Box::new),
                                    samples,
                                }),
                                Err(e) => jev.send(Event::Error(format!("inspect: {e:#}"))),
                            }
                        });
                    }
                    Command::Explain { schema, sql } => {
                        let Some(s) = &session else { continue };
                        spawn_job(s, &ev, &epoch, &busy, false, move |s, jev| async move {
                            let fut = run_explain(&s, &schema, &sql);
                            match with_timeout(META_TIMEOUT, "EXPLAIN", fut).await {
                                Ok(grid) => jev.send(Event::Explain(grid)),
                                Err(e) => jev.send(Event::Error(format!("explain: {e:#}"))),
                            }
                        });
                    }
                    Command::RunAdvisor => {
                        let Some(s) = &session else { continue };
                        spawn_job(s, &ev, &epoch, &busy, true, move |s, jev| async move {
                            match with_timeout(META_TIMEOUT, "advisor", advisor_data(&s)).await {
                                Ok(data) => jev.send(Event::Advisor(Box::new(data))),
                                Err(e) => jev.send(Event::Error(format!("advisor: {e:#}"))),
                            }
                        });
                    }

                    Command::SetWriteMode(on) => {
                        allow_writes = on;
                        info!(allow_writes = on, "write mode changed");
                    }

                    Command::RunSql { schema, sql: stmt } => {
                        let Some(s) = &session else { continue };
                        let kind = sql::classify(&stmt);
                        if !kind.is_read_only() && !allow_writes {
                            let _ = ev.send(Event::SqlError(read_only_message(kind)));
                            continue;
                        }
                        // Deliberately unbounded: only the user knows how long
                        // their own statement should take. Cancel kills it.
                        let inflight = inflight.clone();
                        spawn_job(s, &ev, &epoch, &busy, true, move |s, jev| async move {
                            jev.send(Event::SqlBusy(true));
                            match run_sql(&s, schema.as_deref(), &stmt, &inflight).await {
                                Ok(out) => jev.send(Event::SqlResult(Box::new(out))),
                                Err(e) => jev.send(Event::SqlError(format!("{e:#}"))),
                            }
                            jev.send(Event::SqlBusy(false));
                        });
                    }

                    Command::ListSchemas => {
                        let Some(s) = &session else { continue };
                        spawn_job(s, &ev, &epoch, &busy, false, move |s, jev| async move {
                            match with_timeout(META_TIMEOUT, "schema list", list_schemas(&s)).await {
                                Ok(v) => jev.send(Event::Schemas(v)),
                                Err(e) => jev.send(Event::SqlError(format!("{e:#}"))),
                            }
                        });
                    }

                    Command::ListTables(schema) => {
                        let Some(s) = &session else { continue };
                        spawn_job(s, &ev, &epoch, &busy, false, move |s, jev| async move {
                            let fut = list_tables(&s, &schema);
                            match with_timeout(META_TIMEOUT, "table list", fut).await {
                                Ok(tables) => jev.send(Event::Tables { schema, tables }),
                                Err(e) => jev.send(Event::SqlError(format!("{e:#}"))),
                            }
                        });
                    }

                    Command::DescribeTable { schema, table } => {
                        let Some(s) = &session else { continue };
                        spawn_job(s, &ev, &epoch, &busy, false, move |s, jev| async move {
                            let fut = describe(&s, &schema, &table);
                            match with_timeout(META_TIMEOUT, "DESCRIBE", fut).await {
                                Ok(t) => jev.send(Event::TableSchema(Box::new(t))),
                                Err(e) => jev.send(Event::SqlError(format!("{e:#}"))),
                            }
                        });
                    }

                    Command::Browse(spec) => {
                        let Some(s) = &session else { continue };
                        spawn_job(s, &ev, &epoch, &busy, true, move |s, jev| async move {
                            match browse(&s, &spec).await {
                                Ok(out) => jev.send(Event::BrowseResult {
                                    spec,
                                    outcome: Box::new(out),
                                }),
                                Err(e) => jev.send(Event::SqlError(format!("{e:#}"))),
                            }
                        });
                    }

                    Command::CountRows(spec) => {
                        let Some(s) = &session else { continue };
                        spawn_job(s, &ev, &epoch, &busy, false, move |s, jev| async move {
                            match count_rows(&s, &spec).await {
                                Ok(n) => jev.send(Event::RowCount(n)),
                                Err(e) => jev.send(Event::SqlError(format!("{e:#}"))),
                            }
                        });
                    }

                    Command::ApplyChanges(changes) => {
                        let Some(s) = &session else { continue };
                        if !allow_writes {
                            let _ = ev.send(Event::SqlError(
                                "read-only mode: enable writes before applying edits".into(),
                            ));
                            continue;
                        }
                        spawn_job(s, &ev, &epoch, &busy, true, move |s, jev| async move {
                            match apply_changes(&s, &changes).await {
                                Ok(n) => jev.send(Event::ChangesApplied(n)),
                                Err(e) => jev.send(Event::SqlError(format!("{e:#}"))),
                            }
                        });
                    }
                }
            }

            _ = ticker.tick(), if session.is_some() && !sched.is_paused() => {
                let s = session.as_ref().expect("guarded by ticker condition");
                let heavy = sched.next_heavy();
                let want_processlist = sched.needs_processlist();

                let began = Instant::now();
                let fut = sample_once(s, started, want_processlist, heavy, &ev);
                let outcome = with_timeout(SAMPLE_TIMEOUT, "sample", fut).await;
                let elapsed_ms = began.elapsed().as_secs_f64() * 1000.0;

                match outcome {
                    Ok(sample) => { let _ = ev.send(Event::Sample(Box::new(sample))); }
                    Err(e) => {
                        warn!("sample failed: {e:#}");
                        let _ = ev.send(Event::Error(format!("{e:#}")));
                    }
                }

                // Feed the cost back: a server that answers slowly gets polled
                // less often, on its own, until it recovers.
                let before = sched.interval_ms();
                sched.record_sample(elapsed_ms);
                if sched.interval_ms() != before {
                    info!(
                        interval_ms = sched.interval_ms(),
                        backoff = sched.backoff(),
                        avg_sample_ms = sched.avg_sample_ms(),
                        "sampling interval adjusted"
                    );
                    ticker = new_ticker(sched.interval_ms());
                }
                let _ = ev.send(Event::Load {
                    sample_ms: elapsed_ms,
                    interval_ms: sched.interval_ms(),
                    backed_off: sched.is_backed_off(),
                });
            }
        }
    }

    if let Some(s) = session.take() {
        // Bounded: a connection wedged mid-statement must not hold up exit.
        let _ = tokio::time::timeout(Duration::from_secs(2), s.close()).await;
    }
}

/// Drops a session without waiting for it. `Pool::disconnect` waits for every
/// connection to return, and a job still running holds one.
fn close_in_background(session: Option<Session>) {
    if let Some(s) = session {
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(5), s.close()).await;
        });
    }
}

async fn connect(
    cfg: &ConnConfig,
) -> anyhow::Result<(Session, ServerVersion, u64, ServerLimits, InnodbConfig)> {
    let pool = Pool::new(cfg.to_opts());
    let mut conn = pool.get_conn().await?;

    let version = queries::server_version(&mut conn).await?;
    let mut caps = Capabilities::detect(&version);
    caps.perf_schema_on = queries::perf_schema_enabled(&mut conn).await;

    let (status, _) = queries::global_status(&mut conn).await?;
    let uptime_s = status.get("Uptime").copied().unwrap_or(0);
    let limits = queries::server_limits(&mut conn).await.unwrap_or_default();
    // Buffer pool size and redo capacity are what make the InnoDB counters
    // mean anything; they only change on restart, so read them once.
    let innodb_config = match queries::global_variables(&mut conn).await {
        Ok(vars) => InnodbConfig::from_variables(&vars),
        Err(e) => {
            warn!("could not read InnoDB variables: {e:#}");
            InnodbConfig::default()
        }
    };

    drop(conn);
    Ok((
        Session { pool, caps },
        version,
        uptime_s,
        limits,
        innodb_config,
    ))
}

/// One poll: always `SHOW GLOBAL STATUS`, the session list only when a screen
/// shows it, and at most one expensive table. Heavy results go out as their own
/// events so a failure there never costs us the sample.
async fn sample_once(
    s: &Session,
    started: Instant,
    want_processlist: bool,
    heavy: Option<HeavyTask>,
    ev: &UnboundedSender<Event>,
) -> anyhow::Result<Sample> {
    let mut conn = s.pool.get_conn().await?;

    let t = started.elapsed().as_secs_f64();
    let (status, status_raw) = queries::global_status(&mut conn).await?;
    let processlist = if want_processlist {
        queries::processlist(&mut conn, &s.caps).await?
    } else {
        Vec::new()
    };

    match heavy {
        Some(HeavyTask::Digests) => {
            match queries::top_queries(&mut conn, &s.caps, TOP_SQL_LIMIT).await {
                Ok(rows) => {
                    let _ = ev.send(Event::TopQueries(rows));
                }
                Err(e) => warn!("top queries unavailable: {e:#}"),
            }
        }
        Some(HeavyTask::LockWaits) => match queries::lock_waits(&mut conn, &s.caps).await {
            Ok(rows) => {
                let _ = ev.send(Event::LockWaits(rows));
            }
            Err(e) => warn!("lock waits unavailable: {e:#}"),
        },
        Some(HeavyTask::Transactions) => match queries::transactions(&mut conn).await {
            Ok(rows) => {
                let _ = ev.send(Event::Transactions(rows));
            }
            Err(e) => warn!("innodb_trx unavailable: {e:#}"),
        },
        Some(HeavyTask::Replication) => {
            let mut error = None;
            let replicas = match queries::replica_status(&mut conn, &s.caps).await {
                Ok(grid) => replication::parse_replicas(&grid),
                Err(e) => {
                    error = Some(format!("{e:#}"));
                    Vec::new()
                }
            };
            // A replica is usually also a source, and a source that is not a
            // replica still has a position worth showing, so both are read.
            let source = match queries::source_status(&mut conn).await {
                Ok(grid) => replication::parse_source(&grid),
                Err(e) => {
                    warn!("source status unavailable: {e:#}");
                    None
                }
            };
            let connected = queries::connected_replicas(&mut conn, &s.caps)
                .await
                .unwrap_or_default();
            let _ = ev.send(Event::Replication {
                replicas,
                source,
                connected,
                error,
            });
        }
        Some(HeavyTask::Innodb) => match queries::engine_innodb_status(&mut conn).await {
            Ok(text) => {
                let _ = ev.send(Event::Innodb {
                    status: Box::new(parse_engine_status(&text)),
                    deadlock: deadlock::parse(&text).map(Box::new),
                });
            }
            Err(e) => warn!("engine status unavailable: {e:#}"),
        },
        Some(HeavyTask::MetadataLocks) => match queries::metadata_locks(&mut conn, &s.caps).await {
            Ok(rows) => {
                let _ = ev.send(Event::MetadataLocks(rows));
            }
            Err(e) => warn!("metadata locks unavailable: {e:#}"),
        },
        None => {}
    }

    Ok(Sample {
        t,
        wall_ms: store::now_ms(),
        status,
        status_raw,
        processlist,
    })
}

async fn inspect(
    s: &Session,
    digest: &str,
) -> anyhow::Result<(Option<DigestRow>, Vec<StatementSample>)> {
    let mut conn = s.pool.get_conn().await?;
    let row = queries::digest_detail(&mut conn, &s.caps, digest).await?;
    let samples = queries::statement_samples(&mut conn, &s.caps, digest, SAMPLE_LIMIT).await?;
    Ok((row, samples))
}

async fn run_explain(s: &Session, schema: &str, sql: &str) -> anyhow::Result<Grid> {
    let mut conn = s.pool.get_conn().await?;
    queries::explain(&mut conn, schema, sql).await
}

async fn advisor_data(s: &Session) -> anyhow::Result<AdvisorData> {
    let mut conn = s.pool.get_conn().await?;
    let (status, _) = queries::global_status(&mut conn).await?;
    Ok(AdvisorData {
        indexes: queries::index_definitions(&mut conn).await?,
        usage: queries::index_usage(&mut conn, &s.caps).await?,
        scans: queries::full_table_scans(&mut conn, &s.caps).await?,
        no_pk: queries::tables_without_pk(&mut conn).await?,
        uptime_s: status.get("Uptime").copied().unwrap_or(0),
    })
}

fn read_only_message(kind: StatementKind) -> String {
    format!(
        "read-only mode: that is a {} statement. Enable writes in the SQL tab to run it.",
        kind.label()
    )
}

async fn run_dump(
    s: &Session,
    spec: &DumpSpec,
    server_version: &str,
    cancel: Arc<AtomicBool>,
    ev: &JobEv,
) -> anyhow::Result<DumpStats> {
    let mut conn = s.pool.get_conn().await?;
    let ev = ev.clone();
    dump::run(&mut conn, spec, server_version, cancel, move |p| {
        ev.send(Event::DumpProgress {
            table: p.table,
            table_index: p.table_index,
            table_count: p.table_count,
            rows: p.rows,
        });
    })
    .await
}

async fn run_sql(
    s: &Session,
    schema: Option<&str>,
    stmt: &str,
    inflight: &Inflight,
) -> anyhow::Result<SqlOutcome> {
    let mut conn = s.pool.get_conn().await?;
    // Published so Cancel can KILL QUERY this exact connection. Held until the
    // statement is done, whichever way it ends.
    let _running = inflight.register(conn.id());
    // Pooled connections are reused, so the default database has to be set on
    // whichever one this statement lands on.
    if let Some(schema) = schema {
        sql::use_schema(&mut conn, schema).await?;
    }
    sql::run(&mut conn, stmt, sql::DEFAULT_ROW_LIMIT).await
}

async fn list_schemas(s: &Session) -> anyhow::Result<Vec<String>> {
    let mut conn = s.pool.get_conn().await?;
    sql::list_schemas(&mut conn).await
}

async fn list_tables(s: &Session, schema: &str) -> anyhow::Result<Vec<sql::TableInfo>> {
    let mut conn = s.pool.get_conn().await?;
    sql::list_tables(&mut conn, schema).await
}

async fn describe(s: &Session, schema: &str, table: &str) -> anyhow::Result<TableSchema> {
    let mut conn = s.pool.get_conn().await?;
    sql::describe(&mut conn, schema, table).await
}

async fn browse(s: &Session, spec: &BrowseSpec) -> anyhow::Result<SqlOutcome> {
    let mut conn = s.pool.get_conn().await?;
    sql::browse(&mut conn, spec).await
}

async fn count_rows(s: &Session, spec: &BrowseSpec) -> anyhow::Result<u64> {
    let mut conn = s.pool.get_conn().await?;
    sql::count_rows(&mut conn, spec).await
}

async fn apply_changes(s: &Session, changes: &[Change]) -> anyhow::Result<u64> {
    let mut conn = s.pool.get_conn().await?;
    sql::apply(&mut conn, changes).await
}

async fn kill(s: &Session, id: u64) -> anyhow::Result<()> {
    use mysql_async::prelude::Queryable;
    let mut conn = s.pool.get_conn().await?;
    conn.query_drop(format!("KILL {id}")).await?;
    Ok(())
}

/// Interrupts the statement running on `id` and leaves the connection open, so
/// it goes back to the pool with its session and default database intact.
async fn kill_query(s: &Session, id: u32) -> anyhow::Result<()> {
    use mysql_async::prelude::Queryable;
    let mut conn = s.pool.get_conn().await?;
    conn.query_drop(format!("KILL QUERY {id}")).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn drain(rx: &mut UnboundedReceiver<Event>) -> Vec<bool> {
        let mut out = Vec::new();
        while let Ok(Event::Busy(b)) = rx.try_recv() {
            out.push(b);
        }
        out
    }

    #[test]
    fn busy_is_raised_once_and_cleared_by_the_last_job() {
        let (tx, mut rx) = unbounded_channel();
        let n = Arc::new(AtomicUsize::new(0));

        let a = BusyGuard::new(&n, &tx);
        let b = BusyGuard::new(&n, &tx);
        assert_eq!(drain(&mut rx), vec![true], "only the first job raises it");

        drop(a);
        assert!(drain(&mut rx).is_empty(), "one job still running");

        drop(b);
        assert_eq!(drain(&mut rx), vec![false]);
    }

    #[test]
    fn a_stale_job_cannot_send() {
        let (tx, mut rx) = unbounded_channel();
        let current = Arc::new(AtomicU64::new(7));
        let jev = JobEv {
            ev: tx,
            epoch: 7,
            current: current.clone(),
        };

        jev.send(Event::RowCount(1));
        assert!(matches!(rx.try_recv(), Ok(Event::RowCount(1))));

        // The user disconnected while the job was running.
        current.store(8, Ordering::SeqCst);
        assert!(!jev.is_current());
        jev.send(Event::RowCount(2));
        assert!(
            rx.try_recv().is_err(),
            "a result from the old session is dropped"
        );
    }

    #[test]
    fn nothing_to_cancel_when_no_statement_is_running() {
        let inflight = Inflight::default();
        assert_eq!(inflight.running_id(), None);

        let guard = inflight.register(42);
        assert_eq!(inflight.running_id(), Some(42));

        drop(guard);
        assert_eq!(inflight.running_id(), None);
    }

    #[test]
    fn a_finished_statement_does_not_clear_its_successor() {
        let inflight = Inflight::default();
        let first = inflight.register(1);
        let second = inflight.register(2);
        assert_eq!(inflight.running_id(), Some(2));

        // The first statement ends late; the slot belongs to the second now.
        drop(first);
        assert_eq!(
            inflight.running_id(),
            Some(2),
            "cancel still reaches the live one"
        );

        drop(second);
        assert_eq!(inflight.running_id(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_operation_is_given_up_on() {
        let slow = async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        };
        let e = with_timeout(Duration::from_secs(5), "sample", slow)
            .await
            .unwrap_err();
        assert_eq!(e.to_string(), "sample timed out after 5s");
    }

    #[tokio::test(start_paused = true)]
    async fn work_inside_the_limit_still_returns() {
        let quick = async { Ok(7) };
        let v = with_timeout(Duration::from_secs(5), "sample", quick)
            .await
            .unwrap();
        assert_eq!(v, 7);
    }
}
