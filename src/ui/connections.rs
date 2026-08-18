//! Connection monitor: how full the pool is, who is filling it, and which
//! sessions are worth killing.

use egui::RichText;

use super::*;
use crate::app::App;
use crate::connections::{self, Bucket, UsageLevel};
use crate::db::collector::Command;
use crate::model::ProcessRow;

impl App {
    pub fn connections_tab(&mut self, ui: &mut egui::Ui) {
        if !self.is_connected() && self.latest.is_none() {
            not_connected(ui);
            return;
        }

        let rows: Vec<ProcessRow> = self
            .latest
            .as_ref()
            .map(|s| s.processlist.clone())
            .unwrap_or_default();
        let stats = connections::summarize(&rows, self.long_query_secs, self.idle_secs);

        self.usage_header(ui, &stats);
        ui.separator();
        self.thresholds_row(ui);
        ui.separator();

        let mut kill: Option<u64> = None;
        egui::ScrollArea::vertical()
            .id_salt("connections_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.columns(2, |cols| {
                    bucket_table(&mut cols[0], "By user", &stats.by_user);
                    bucket_table(&mut cols[1], "By host", &stats.by_host);
                });

                ui.add_space(10.0);
                let long = connections::long_running(&rows, self.long_query_secs);
                if let Some(id) = session_table(
                    ui,
                    "long_running",
                    &format!(
                        "Long-running ({}) — statement open ≥ {}s",
                        long.len(),
                        self.long_query_secs
                    ),
                    &long,
                    RED,
                    "no statement has been running that long",
                ) {
                    kill = Some(id);
                }

                ui.add_space(10.0);
                let idle = connections::idle_sessions(&rows, self.idle_secs);
                if let Some(id) = session_table(
                    ui,
                    "idle_sessions",
                    &format!("Sleeping ({}) — idle ≥ {}s", idle.len(), self.idle_secs),
                    &idle,
                    AMBER,
                    "no session has been idle that long",
                ) {
                    kill = Some(id);
                }
            });

        if let Some(id) = kill {
            self.collector.send(Command::KillThread(id));
            self.push_log(format!("KILL {id} sent"));
        }
    }

    fn usage_header(&mut self, ui: &mut egui::Ui, stats: &connections::ConnStats) {
        // Threads_connected from SHOW GLOBAL STATUS counts every session,
        // including this monitor's own; the process list is filtered.
        let connected = self
            .latest
            .as_ref()
            .map(|s| s.stat("Threads_connected"))
            .unwrap_or(0);
        let running = self
            .latest
            .as_ref()
            .map(|s| s.stat("Threads_running"))
            .unwrap_or(0);
        let max_used = self
            .latest
            .as_ref()
            .map(|s| s.stat("Max_used_connections"))
            .unwrap_or(0);
        let max = self.limits.max_connections;
        let pct = connections::usage_pct(connected, max);
        let level = UsageLevel::of(pct);
        let color = match level {
            UsageLevel::Ok => GREEN,
            UsageLevel::Warn => AMBER,
            UsageLevel::Critical => RED,
        };

        ui.horizontal_wrapped(|ui| {
            stat(ui, "Threads Connected", connected.to_string());
            stat_colored(
                ui,
                "Threads Running",
                running.to_string(),
                Some(if running > 40 { RED } else { GREEN }),
            );
            stat(
                ui,
                "Max Connections",
                if max == 0 {
                    "?".into()
                } else {
                    max.to_string()
                },
            );
            stat_colored(ui, "Usage", format!("{pct:.0}%"), Some(color));
            stat(ui, "Sessions", stats.total.to_string());
            stat(ui, "Active", stats.active.to_string());
            stat(ui, "Sleeping", stats.sleeping.to_string());
            stat_colored(
                ui,
                "Idle (stale)",
                stats.idle_stale.to_string(),
                Some(if stats.idle_stale > 0 { AMBER } else { GREEN }),
            );
            stat_colored(
                ui,
                "Long-running",
                stats.long_running.to_string(),
                Some(if stats.long_running > 0 { RED } else { GREEN }),
            );
            stat(ui, "Peak used", max_used.to_string());
        });

        if max > 0 {
            ui.add(
                egui::ProgressBar::new((pct / 100.0) as f32)
                    .fill(color)
                    .text(format!("{connected} / {max} connections")),
            );
        }

        match level {
            UsageLevel::Critical => {
                ui.colored_label(
                    RED,
                    RichText::new(format!(
                        "⚠ Connection usage: {pct:.0}% — new connections will be refused at the \
                         limit; free sleepers or raise max_connections"
                    ))
                    .strong(),
                );
            }
            UsageLevel::Warn => {
                ui.colored_label(
                    AMBER,
                    format!("⚠ Connection usage: {pct:.0}% — watch the sleeping sessions below"),
                );
            }
            UsageLevel::Ok if max > 0 => {
                ui.colored_label(GREEN, format!("Connection usage: {pct:.0}%"));
            }
            UsageLevel::Ok => {
                ui.colored_label(AMBER, "max_connections unknown — usage cannot be computed");
            }
        }

        if self.limits.wait_timeout > 0 {
            ui.label(
                RichText::new(format!(
                    "wait_timeout {}s · interactive_timeout {}s — a sleeper holds its slot until \
                     one of those elapses",
                    self.limits.wait_timeout, self.limits.interactive_timeout
                ))
                .small()
                .weak(),
            );
        }
    }

    fn thresholds_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Long-running after");
            ui.add(
                egui::DragValue::new(&mut self.long_query_secs)
                    .range(1..=3600)
                    .suffix(" s"),
            );
            ui.separator();
            ui.label("Idle after");
            ui.add(
                egui::DragValue::new(&mut self.idle_secs)
                    .range(5..=86400)
                    .suffix(" s"),
            );
            ui.separator();
            ui.label(
                RichText::new(
                    "session list comes from the process list, which excludes this monitor's \
                     own connection",
                )
                .small()
                .weak(),
            );
        });
    }
}

fn bucket_table(ui: &mut egui::Ui, title: &str, buckets: &[Bucket]) {
    ui.label(RichText::new(title).strong());
    if buckets.is_empty() {
        ui.label(RichText::new("no sessions").weak());
        return;
    }
    egui::Grid::new(title)
        .striped(true)
        .num_columns(5)
        .show(ui, |ui| {
            for h in ["", "Total", "Active", "Sleeping", "Longest"] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for b in buckets {
                ui.label(&b.key);
                ui.label(b.total.to_string());
                ui.label(b.active.to_string());
                ui.label(b.sleeping.to_string());
                ui.label(fmt_duration(b.longest_secs.max(0) as u64));
                ui.end_row();
            }
        });
}

/// Returns the connection id to kill, if the user clicked one.
fn session_table(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    rows: &[&ProcessRow],
    accent: egui::Color32,
    empty_note: &str,
) -> Option<u64> {
    let mut kill = None;
    ui.colored_label(accent, RichText::new(title).strong());
    if rows.is_empty() {
        ui.label(RichText::new(empty_note).weak());
        return None;
    }

    egui::Grid::new(id)
        .striped(true)
        .num_columns(8)
        .show(ui, |ui| {
            for h in ["Id", "User", "Host", "DB", "Command", "Time", "State", ""] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for p in rows {
                ui.label(p.id.to_string());
                ui.label(&p.user);
                ui.label(&p.host);
                ui.label(&p.db);
                ui.label(&p.command);
                ui.colored_label(accent, fmt_duration(p.time.max(0) as u64));
                ui.label(one_line(&p.state, 40));
                if ui.small_button("kill").clicked() {
                    kill = Some(p.id);
                }
                ui.end_row();
            }
        });
    kill
}
