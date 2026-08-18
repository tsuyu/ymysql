//! Load shaping for the sampler.
//!
//! The naive loop fetched status, processlist, digests, lock waits, open
//! transactions and metadata locks all on the same tick, against a server that
//! is by definition already busy. This module spreads that work out:
//!
//! * only the panels the user is actually looking at get collected,
//! * the expensive tables are rotated one per tick instead of fired together,
//! * and when the server is slow to answer, the interval backs off on its own.
//!
//! It is pure logic so the policy can be tested without a server.

/// The expensive fetches, rotated one per heavy tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeavyTask {
    Digests,
    LockWaits,
    Transactions,
    MetadataLocks,
    /// `SHOW ENGINE INNODB STATUS` — cheap, but a whole report to parse.
    Innodb,
}

impl HeavyTask {
    const ROTATION: [HeavyTask; 5] = [
        HeavyTask::Digests,
        HeavyTask::LockWaits,
        HeavyTask::Transactions,
        HeavyTask::MetadataLocks,
        HeavyTask::Innodb,
    ];
}

/// What the visible screen needs. Anything false is not collected at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewNeeds {
    /// Session list (Lock Monitor → Sessions, and the Dashboard counter).
    pub processlist: bool,
    /// Statement digests (Top SQL, Dashboard, Index Advisor input).
    pub digests: bool,
    /// Lock waits, transactions, metadata locks.
    pub locks: bool,
    /// The InnoDB engine report.
    pub innodb: bool,
}

impl Default for ViewNeeds {
    fn default() -> Self {
        // Dashboard is the landing tab.
        Self {
            processlist: true,
            digests: true,
            locks: true,
            innodb: false,
        }
    }
}

impl ViewNeeds {
    /// Nothing but `SHOW GLOBAL STATUS`, which every screen's charts need.
    pub fn minimal() -> Self {
        Self {
            processlist: false,
            digests: false,
            locks: false,
            innodb: false,
        }
    }

    fn wants(&self, task: HeavyTask) -> bool {
        match task {
            HeavyTask::Digests => self.digests,
            HeavyTask::LockWaits | HeavyTask::Transactions | HeavyTask::MetadataLocks => self.locks,
            HeavyTask::Innodb => self.innodb,
        }
    }

    fn any_heavy(&self) -> bool {
        self.digests || self.locks || self.innodb
    }
}

/// How many ticks pass between two heavy fetches.
const HEAVY_EVERY: u64 = 5;
/// Share of the interval a sample may consume before the loop backs off.
const BUDGET: f64 = 0.5;
/// Ceiling for the adaptive interval.
const MAX_BACKOFF: u32 = 8;
/// Samples averaged when judging how slow the server is.
const WINDOW: usize = 5;

#[derive(Debug, Clone)]
pub struct Scheduler {
    base_interval_ms: u64,
    needs: ViewNeeds,
    tick: u64,
    rotation: usize,
    /// Recent sample durations in milliseconds.
    recent: Vec<f64>,
    backoff: u32,
    forced_heavy: bool,
    paused: bool,
}

impl Scheduler {
    pub fn new(base_interval_ms: u64) -> Self {
        Self {
            base_interval_ms: base_interval_ms.max(200),
            needs: ViewNeeds::default(),
            tick: 0,
            rotation: 0,
            recent: Vec::new(),
            backoff: 1,
            forced_heavy: true,
            paused: false,
        }
    }

    pub fn set_base_interval(&mut self, ms: u64) {
        self.base_interval_ms = ms.max(200);
    }

    pub fn set_needs(&mut self, needs: ViewNeeds) {
        // A screen that just became visible should not wait a full rotation.
        if needs != self.needs {
            self.forced_heavy = true;
        }
        self.needs = needs;
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn force_heavy(&mut self) {
        self.forced_heavy = true;
    }

    pub fn needs_processlist(&self) -> bool {
        self.needs.processlist
    }

    pub fn interval_ms(&self) -> u64 {
        self.base_interval_ms * self.backoff as u64
    }

    pub fn backoff(&self) -> u32 {
        self.backoff
    }

    /// Mean of the recent sample durations, for the UI.
    pub fn avg_sample_ms(&self) -> f64 {
        if self.recent.is_empty() {
            0.0
        } else {
            self.recent.iter().sum::<f64>() / self.recent.len() as f64
        }
    }

    /// Picks the heavy fetch for this tick, if any is due and wanted.
    ///
    /// Only one runs per tick, so a busy server never gets four expensive
    /// `information_schema` / `performance_schema` reads at once.
    pub fn next_heavy(&mut self) -> Option<HeavyTask> {
        self.tick = self.tick.wrapping_add(1);
        if !self.needs.any_heavy() {
            self.forced_heavy = false;
            return None;
        }

        let due = self.forced_heavy || self.tick.is_multiple_of(HEAVY_EVERY);
        if !due {
            return None;
        }
        self.forced_heavy = false;

        // Walk the rotation until a task the current screen wants turns up.
        for _ in 0..HeavyTask::ROTATION.len() {
            let task = HeavyTask::ROTATION[self.rotation % HeavyTask::ROTATION.len()];
            self.rotation = self.rotation.wrapping_add(1);
            if self.needs.wants(task) {
                return Some(task);
            }
        }
        None
    }

    /// Feeds back how long the last sample took, adjusting the backoff.
    ///
    /// Above half the interval the period doubles (capped); comfortably below
    /// it, the period halves back towards the configured value.
    pub fn record_sample(&mut self, elapsed_ms: f64) {
        if self.recent.len() == WINDOW {
            self.recent.remove(0);
        }
        self.recent.push(elapsed_ms);

        let avg = self.avg_sample_ms();
        let budget = self.base_interval_ms as f64 * BUDGET;

        if avg > budget * self.backoff as f64 {
            self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        } else if self.backoff > 1 && avg < budget * (self.backoff as f64 / 4.0) {
            self.backoff /= 2;
        }
    }

    /// True when the loop is running slower than configured.
    pub fn is_backed_off(&self) -> bool {
        self.backoff > 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heavy_ticks(s: &mut Scheduler, n: usize) -> Vec<Option<HeavyTask>> {
        (0..n).map(|_| s.next_heavy()).collect()
    }

    #[test]
    fn heavy_work_is_rotated_one_per_tick() {
        let mut s = Scheduler::new(1000);
        // A screen that wants everything sees the whole rotation.
        s.set_needs(ViewNeeds {
            processlist: true,
            digests: true,
            locks: true,
            innodb: true,
        });
        let picked: Vec<HeavyTask> = heavy_ticks(&mut s, 30).into_iter().flatten().collect();

        assert!(
            picked.len() >= 6,
            "heavy work should still happen regularly: {picked:?}"
        );
        assert_eq!(picked[0], HeavyTask::Digests);
        assert_eq!(picked[1], HeavyTask::LockWaits);
        assert_eq!(picked[2], HeavyTask::Transactions);
        assert_eq!(picked[3], HeavyTask::MetadataLocks);
        assert_eq!(picked[4], HeavyTask::Innodb);
        assert_eq!(picked[5], HeavyTask::Digests, "rotation wraps");
    }

    #[test]
    fn a_task_no_screen_wants_is_skipped_in_the_rotation() {
        // The default needs do not include the InnoDB report.
        let mut s = Scheduler::new(1000);
        let picked: Vec<HeavyTask> = heavy_ticks(&mut s, 30).into_iter().flatten().collect();
        assert!(!picked.contains(&HeavyTask::Innodb));
        assert_eq!(
            picked[4],
            HeavyTask::Digests,
            "rotation closes over the gap"
        );
    }

    #[test]
    fn at_most_one_heavy_task_per_tick() {
        let mut s = Scheduler::new(1000);
        for r in heavy_ticks(&mut s, 50) {
            // The type says it: one Option, never a batch. Guard the count of
            // ticks that do any work at all.
            let _ = r;
        }
        let worked = heavy_ticks(&mut Scheduler::new(1000), 20)
            .into_iter()
            .filter(|t| t.is_some())
            .count();
        assert!(
            worked <= 20 / HEAVY_EVERY as usize + 1,
            "{worked} heavy ticks in 20"
        );
    }

    #[test]
    fn invisible_panels_are_not_collected() {
        let mut s = Scheduler::new(1000);
        s.set_needs(ViewNeeds {
            processlist: false,
            digests: true,
            locks: false,
            innodb: false,
        });
        let picked: Vec<HeavyTask> = heavy_ticks(&mut s, 40).into_iter().flatten().collect();
        assert!(!picked.is_empty());
        assert!(
            picked.iter().all(|t| *t == HeavyTask::Digests),
            "only the visible panel's data: {picked:?}"
        );
        assert!(!s.needs_processlist());
    }

    #[test]
    fn nothing_heavy_when_no_panel_wants_it() {
        let mut s = Scheduler::new(1000);
        s.set_needs(ViewNeeds::minimal());
        assert!(heavy_ticks(&mut s, 40).into_iter().all(|t| t.is_none()));
    }

    #[test]
    fn switching_screens_fetches_immediately() {
        let mut s = Scheduler::new(1000);
        s.next_heavy(); // consume the initial forced fetch
        assert!(s.next_heavy().is_none(), "not due yet");

        s.set_needs(ViewNeeds {
            processlist: true,
            digests: true,
            locks: false,
            innodb: false,
        });
        assert!(
            s.next_heavy().is_some(),
            "a newly opened screen fills at once"
        );
    }

    #[test]
    fn slow_server_backs_the_interval_off() {
        let mut s = Scheduler::new(1000);
        assert_eq!(s.interval_ms(), 1000);

        for _ in 0..WINDOW {
            s.record_sample(900.0); // way over the 500 ms budget
        }
        assert!(s.is_backed_off());
        assert_eq!(s.interval_ms(), 2000);

        for _ in 0..WINDOW {
            s.record_sample(1500.0);
        }
        assert_eq!(s.interval_ms(), 4000, "still slow, back off further");
    }

    #[test]
    fn backoff_recovers_when_the_server_does() {
        let mut s = Scheduler::new(1000);
        for _ in 0..WINDOW {
            s.record_sample(2000.0);
        }
        let slow = s.backoff();
        assert!(slow > 1);

        for _ in 0..WINDOW * 4 {
            s.record_sample(5.0);
        }
        assert_eq!(s.backoff(), 1, "fast samples return to the configured rate");
        assert_eq!(s.interval_ms(), 1000);
    }

    #[test]
    fn backoff_is_capped() {
        let mut s = Scheduler::new(1000);
        for _ in 0..100 {
            s.record_sample(100_000.0);
        }
        assert_eq!(s.backoff(), MAX_BACKOFF);
    }

    #[test]
    fn fast_server_never_backs_off() {
        let mut s = Scheduler::new(1000);
        for _ in 0..50 {
            s.record_sample(20.0);
        }
        assert_eq!(s.backoff(), 1);
        assert!(!s.is_backed_off());
    }
}
