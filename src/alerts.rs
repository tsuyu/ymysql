//! Threshold alert engine. Pure state machine over `Derived` samples: a rule
//! must stay breached for its whole `for_secs` window before it fires, so a
//! single spiky sample cannot page anyone.

use std::collections::{HashMap, VecDeque};

use crate::model::{Derived, Metric};

const LOG_CAP: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Above,
    Below,
}

impl Comparison {
    pub fn label(self) -> &'static str {
        match self {
            Comparison::Above => ">",
            Comparison::Below => "<",
        }
    }

    fn breached(self, value: f64, threshold: f64) -> bool {
        match self {
            Comparison::Above => value > threshold,
            Comparison::Below => value < threshold,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: u32,
    pub enabled: bool,
    pub metric: Metric,
    pub cmp: Comparison,
    pub threshold: f64,
    /// How long the breach must persist before firing.
    pub for_secs: f64,
}

impl Rule {
    pub fn describe(&self) -> String {
        let threshold = if self.threshold.fract().abs() < 1e-9 {
            format!("{:.0}", self.threshold)
        } else {
            format!("{:.2}", self.threshold)
        };
        format!(
            "{} {} {threshold} for {:.0}s",
            self.metric.label(),
            self.cmp.label(),
            self.for_secs
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Ok,
    /// Breached, but not yet for long enough.
    Pending {
        since: f64,
    },
    Firing {
        since: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Fired,
    Resolved,
}

#[derive(Debug, Clone)]
pub struct AlertEvent {
    pub at_ms: i64,
    pub rule: String,
    pub transition: Transition,
    pub value: f64,
}

pub struct Engine {
    pub rules: Vec<Rule>,
    states: HashMap<u32, State>,
    pub log: VecDeque<AlertEvent>,
    next_id: u32,
}

impl Default for Engine {
    fn default() -> Self {
        let mut e = Self {
            rules: Vec::new(),
            states: HashMap::new(),
            log: VecDeque::new(),
            next_id: 1,
        };
        // Starting set: the things that actually wake people up.
        e.add(Metric::ConnUsage, Comparison::Above, 80.0, 30.0);
        e.add(Metric::ThreadsRunning, Comparison::Above, 40.0, 15.0);
        e.add(Metric::SlowQps, Comparison::Above, 5.0, 30.0);
        e.add(Metric::BpHitPct, Comparison::Below, 95.0, 60.0);
        e.add(Metric::TableLockWaits, Comparison::Above, 1.0, 30.0);
        e.add(Metric::TmpDiskTables, Comparison::Above, 5.0, 60.0);
        e.add(Metric::AbortedConnects, Comparison::Above, 1.0, 30.0);
        e
    }
}

impl Engine {
    pub fn add(&mut self, metric: Metric, cmp: Comparison, threshold: f64, for_secs: f64) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.rules.push(Rule {
            id,
            enabled: true,
            metric,
            cmp,
            threshold,
            for_secs,
        });
        id
    }

    pub fn remove(&mut self, id: u32) {
        self.rules.retain(|r| r.id != id);
        self.states.remove(&id);
    }

    pub fn state(&self, id: u32) -> State {
        self.states.get(&id).copied().unwrap_or(State::Ok)
    }

    pub fn firing(&self) -> impl Iterator<Item = (&Rule, f64)> {
        self.rules
            .iter()
            .filter_map(move |r| match self.state(r.id) {
                State::Firing { since } => Some((r, since)),
                _ => None,
            })
    }

    pub fn firing_count(&self) -> usize {
        self.firing().count()
    }

    /// Feeds one sample through every rule. `t` is monotonic seconds, `wall_ms`
    /// only labels the log. Returns the transitions this sample caused.
    pub fn evaluate(&mut self, t: f64, wall_ms: i64, d: &Derived) -> Vec<AlertEvent> {
        let mut events = Vec::new();

        for rule in &self.rules {
            if !rule.enabled {
                self.states.insert(rule.id, State::Ok);
                continue;
            }

            let value = rule.metric.value(d);
            let breached = rule.cmp.breached(value, rule.threshold);
            let prev = self.states.get(&rule.id).copied().unwrap_or(State::Ok);

            let next = match (prev, breached) {
                (State::Ok, true) => {
                    if rule.for_secs <= 0.0 {
                        events.push(AlertEvent {
                            at_ms: wall_ms,
                            rule: rule.describe(),
                            transition: Transition::Fired,
                            value,
                        });
                        State::Firing { since: t }
                    } else {
                        State::Pending { since: t }
                    }
                }
                (State::Pending { since }, true) => {
                    if t - since >= rule.for_secs {
                        events.push(AlertEvent {
                            at_ms: wall_ms,
                            rule: rule.describe(),
                            transition: Transition::Fired,
                            value,
                        });
                        State::Firing { since: t }
                    } else {
                        State::Pending { since }
                    }
                }
                (State::Firing { since }, true) => State::Firing { since },
                (State::Firing { .. }, false) => {
                    events.push(AlertEvent {
                        at_ms: wall_ms,
                        rule: rule.describe(),
                        transition: Transition::Resolved,
                        value,
                    });
                    State::Ok
                }
                (_, false) => State::Ok,
            };
            self.states.insert(rule.id, next);
        }

        for ev in &events {
            if self.log.len() >= LOG_CAP {
                self.log.pop_front();
            }
            self.log.push_back(ev.clone());
        }
        events
    }

    /// Drops all rule state, e.g. after reconnecting to a different server.
    pub fn reset_states(&mut self) {
        self.states.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn derived_with_threads(n: f64) -> Derived {
        Derived {
            threads_running: n,
            ..Default::default()
        }
    }

    fn engine_with_one_rule(for_secs: f64) -> Engine {
        let mut e = Engine {
            rules: Vec::new(),
            states: HashMap::new(),
            log: VecDeque::new(),
            next_id: 1,
        };
        e.add(Metric::ThreadsRunning, Comparison::Above, 10.0, for_secs);
        e
    }

    #[test]
    fn holds_pending_until_the_window_elapses() {
        let mut e = engine_with_one_rule(30.0);
        let hot = derived_with_threads(50.0);

        assert!(e.evaluate(0.0, 0, &hot).is_empty());
        assert!(matches!(e.state(1), State::Pending { .. }));

        assert!(e.evaluate(29.0, 0, &hot).is_empty(), "not yet 30s");
        let fired = e.evaluate(30.0, 0, &hot);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].transition, Transition::Fired);
        assert!(matches!(e.state(1), State::Firing { .. }));
    }

    #[test]
    fn a_single_spike_never_fires() {
        let mut e = engine_with_one_rule(30.0);
        e.evaluate(0.0, 0, &derived_with_threads(99.0));
        e.evaluate(1.0, 0, &derived_with_threads(1.0));
        assert_eq!(e.state(1), State::Ok);
        assert!(e.log.is_empty());
        e.evaluate(60.0, 0, &derived_with_threads(99.0));
        assert!(
            matches!(e.state(1), State::Pending { .. }),
            "window restarts"
        );
    }

    #[test]
    fn resolves_and_logs_both_transitions() {
        let mut e = engine_with_one_rule(0.0);
        assert_eq!(e.evaluate(0.0, 111, &derived_with_threads(50.0)).len(), 1);
        let resolved = e.evaluate(5.0, 222, &derived_with_threads(0.0));
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].transition, Transition::Resolved);
        assert_eq!(e.log.len(), 2);
        assert_eq!(e.firing_count(), 0);
    }

    #[test]
    fn below_rules_work_on_percentages() {
        let mut e = Engine {
            rules: Vec::new(),
            states: HashMap::new(),
            log: VecDeque::new(),
            next_id: 1,
        };
        let id = e.add(Metric::BpHitPct, Comparison::Below, 95.0, 0.0);
        let cold = Derived {
            bp_hit_ratio: 0.80,
            ..Default::default()
        };
        assert_eq!(e.evaluate(0.0, 0, &cold).len(), 1);
        assert!(matches!(e.state(id), State::Firing { .. }));
    }

    #[test]
    fn disabled_rules_never_fire() {
        let mut e = engine_with_one_rule(0.0);
        e.rules[0].enabled = false;
        assert!(e.evaluate(0.0, 0, &derived_with_threads(999.0)).is_empty());
        assert_eq!(e.state(1), State::Ok);
    }
}
