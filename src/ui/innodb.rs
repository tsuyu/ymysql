//! InnoDB monitor: buffer pool, redo log, purge, row locks, disk I/O.

use egui::RichText;

use super::*;
use crate::app::App;
use crate::db::collector::Command;
use crate::innodb::{self, Health, InnodbSnapshot};
use crate::model::Metric;

impl App {
    pub fn innodb_tab(&mut self, ui: &mut egui::Ui) {
        let Some(sample) = self.latest.clone() else {
            not_connected(ui);
            return;
        };
        let snap = innodb::snapshot(&sample.status, &self.engine_status, &self.innodb_config);
        let d = self.derived;

        ui.horizontal_wrapped(|ui| {
            if ui.button("Refresh engine status").clicked() {
                self.collector.send(Command::RefreshHeavy);
            }
            ui.label(
                RichText::new(format!(
                    "buffer pool {} · redo capacity {} · innodb_flush_log_at_trx_commit={}",
                    fmt_bytes(self.innodb_config.buffer_pool_bytes as f64),
                    if self.innodb_config.redo_capacity_bytes == 0 {
                        "?".to_string()
                    } else {
                        fmt_bytes(self.innodb_config.redo_capacity_bytes as f64)
                    },
                    self.innodb_config.flush_log_at_trx_commit
                ))
                .small()
                .weak(),
            );
        });
        ui.separator();

        egui::ScrollArea::vertical()
            .id_salt("innodb_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.buffer_pool_section(ui, &snap);
                ui.add_space(8.0);
                self.redo_section(ui, &snap);
                ui.add_space(8.0);
                self.purge_section(ui, &snap);
                ui.add_space(8.0);
                self.row_lock_section(ui, &snap);
                ui.add_space(8.0);
                self.io_section(ui, &snap, &d);
                ui.add_space(8.0);

                ui.label(RichText::new("Trends").strong());
                live_plot(ui, "Buffer pool hit %", &self.history, &[Metric::BpHitPct]);
                live_plot(
                    ui,
                    "InnoDB rows/s",
                    &self.history,
                    &[Metric::RowsRead, Metric::RowsWritten],
                );
            });
    }

    fn buffer_pool_section(&self, ui: &mut egui::Ui, snap: &InnodbSnapshot) {
        section(ui, "Buffer pool", |ui| {
            ui.horizontal_wrapped(|ui| {
                stat_colored(
                    ui,
                    "Hit ratio (lifetime)",
                    format!("{:.2}%", snap.hit_ratio_pct),
                    Some(health_color(snap.hit_ratio_health())),
                );
                stat(
                    ui,
                    "Hit ratio (now)",
                    format!("{:.2}%", self.derived.bp_hit_ratio * 100.0),
                );
                stat(ui, "Pages total", fmt_count(snap.pages_total));
                stat(ui, "Pages data", fmt_count(snap.pages_data));
                stat_colored(
                    ui,
                    "Dirty pages",
                    format!("{} ({:.1}%)", fmt_count(snap.pages_dirty), snap.dirty_pct),
                    Some(health_color(snap.dirty_health(&self.innodb_config))),
                );
                stat(ui, "Pages free", fmt_count(snap.pages_free));
                stat(
                    ui,
                    "Page size",
                    fmt_bytes(self.innodb_config.page_size as f64),
                );
                stat(
                    ui,
                    "Pool instances",
                    self.innodb_config.buffer_pool_instances.to_string(),
                );
                stat_colored(
                    ui,
                    "Wait free",
                    fmt_count(snap.wait_free),
                    Some(if snap.wait_free > 0 { AMBER } else { GREEN }),
                );
            });

            if snap.pages_total > 0 {
                ui.add(
                    egui::ProgressBar::new((snap.dirty_pct / 100.0) as f32)
                        .fill(health_color(snap.dirty_health(&self.innodb_config)))
                        .text(format!(
                            "dirty {:.1}% of pool (max {:.0}%)",
                            snap.dirty_pct, self.innodb_config.max_dirty_pages_pct
                        )),
                );
            }
            note(
                ui,
                snap.hit_ratio_health(),
                "Lifetime hit ratio is the whole uptime, so it moves slowly — the \"now\" figure \
                 reacts to the current workload. A sustained drop means the working set no \
                 longer fits the pool.",
            );
            if snap.wait_free > 0 {
                ui.colored_label(
                    AMBER,
                    "Innodb_buffer_pool_wait_free is non-zero: threads have waited for a clean \
                     page — the flusher is behind, or the pool is too small.",
                );
            }
        });
    }

    fn redo_section(&self, ui: &mut egui::Ui, snap: &InnodbSnapshot) {
        section(ui, "Redo log and checkpoint", |ui| {
            ui.horizontal_wrapped(|ui| {
                stat_colored(
                    ui,
                    "Checkpoint age",
                    fmt_bytes(snap.checkpoint_age as f64),
                    Some(health_color(snap.checkpoint_health())),
                );
                stat_colored(
                    ui,
                    "of redo capacity",
                    if self.innodb_config.redo_capacity_bytes == 0 {
                        "?".to_string()
                    } else {
                        format!("{:.1}%", snap.checkpoint_age_pct)
                    },
                    Some(health_color(snap.checkpoint_health())),
                );
                stat(ui, "Unflushed redo", fmt_bytes(snap.log_flush_lag as f64));
                stat_colored(
                    ui,
                    "Log waits",
                    fmt_count(snap.log_waits),
                    Some(if snap.log_waits > 0 { AMBER } else { GREEN }),
                );
                stat(ui, "LSN", fmt_count(self.engine_status.lsn));
            });

            if self.innodb_config.redo_capacity_bytes > 0 {
                ui.add(
                    egui::ProgressBar::new((snap.checkpoint_age_pct / 100.0).min(1.0) as f32)
                        .fill(health_color(snap.checkpoint_health()))
                        .text(format!(
                            "{} of {}",
                            fmt_bytes(snap.checkpoint_age as f64),
                            fmt_bytes(self.innodb_config.redo_capacity_bytes as f64)
                        )),
                );
            }
            note(
                ui,
                snap.checkpoint_health(),
                "Checkpoint age is redo written since the last checkpoint. Past ~75% of capacity \
                 InnoDB flushes aggressively and write throughput drops; past 90% it is \
                 effectively stalling. The fix is a larger redo log, not more I/O.",
            );
            if snap.log_waits > 0 {
                ui.colored_label(
                    AMBER,
                    "Innodb_log_waits is non-zero: the log buffer filled before it could be \
                     flushed — consider a larger innodb_log_buffer_size.",
                );
            }
        });
    }

    fn purge_section(&self, ui: &mut egui::Ui, snap: &InnodbSnapshot) {
        section(ui, "Purge / MVCC", |ui| {
            ui.horizontal_wrapped(|ui| {
                stat_colored(
                    ui,
                    "History list length",
                    fmt_count(snap.history_list_length),
                    Some(health_color(snap.history_health())),
                );
                stat(ui, "Open transactions", self.transactions.len().to_string());
                stat_colored(
                    ui,
                    "OS waits (latches)",
                    fmt_count(snap.os_wait_reservations),
                    None,
                );
            });
            note(
                ui,
                snap.history_health(),
                "History list length counts undo records still needed by open read views. A \
                 number that climbs and never falls is almost always one forgotten transaction \
                 holding a snapshot — find it under Lock Monitor → Transactions.",
            );
        });
    }

    fn row_lock_section(&self, ui: &mut egui::Ui, snap: &InnodbSnapshot) {
        section(ui, "Row locks", |ui| {
            ui.horizontal_wrapped(|ui| {
                stat_colored(
                    ui,
                    "Waiting now",
                    fmt_count(snap.row_lock_current_waits),
                    Some(if snap.row_lock_current_waits > 0 {
                        RED
                    } else {
                        GREEN
                    }),
                );
                stat(ui, "Total waits", fmt_count(snap.row_lock_waits));
                stat(ui, "Avg wait", format!("{} ms", snap.row_lock_time_avg_ms));
                stat_colored(
                    ui,
                    "Max wait",
                    format!("{} ms", snap.row_lock_time_max_ms),
                    Some(if snap.row_lock_time_max_ms > 10_000 {
                        RED
                    } else {
                        GREEN
                    }),
                );
                stat(
                    ui,
                    "Lock waits/s",
                    format!("{:.2}", self.derived.table_locks_waited_s),
                );
            });
            if snap.row_lock_current_waits > 0 {
                ui.colored_label(
                    RED,
                    "Sessions are blocked on row locks right now — the Lock Monitor tab names \
                     both sides.",
                );
            }
        });
    }

    fn io_section(&self, ui: &mut egui::Ui, snap: &InnodbSnapshot, d: &crate::model::Derived) {
        section(ui, "Disk I/O", |ui| {
            ui.horizontal_wrapped(|ui| {
                stat(ui, "Data reads", fmt_count(snap.data_reads));
                stat(ui, "Data writes", fmt_count(snap.data_writes));
                stat(ui, "fsyncs", fmt_count(snap.data_fsyncs));
                stat(ui, "Rows read/s", format!("{:.0}", d.innodb_rows_read_s));
                stat(
                    ui,
                    "Rows written/s",
                    format!("{:.0}", d.innodb_rows_written_s),
                );
                stat(
                    ui,
                    "io_capacity",
                    if self.innodb_config.io_capacity == 0 {
                        "?".to_string()
                    } else {
                        format!(
                            "{} / {}",
                            self.innodb_config.io_capacity, self.innodb_config.io_capacity_max
                        )
                    },
                );
            });
        });
    }
}

fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(RichText::new(title).heading());
        ui.add_space(2.0);
        body(ui);
    });
}

fn health_color(h: Health) -> egui::Color32 {
    match h {
        Health::Good => GREEN,
        Health::Watch => AMBER,
        Health::Bad => RED,
    }
}

/// Explanatory line, coloured only when the section is unhealthy.
fn note(ui: &mut egui::Ui, health: Health, text: &str) {
    let rich = RichText::new(text).small();
    ui.label(match health {
        Health::Good => rich.weak(),
        other => rich.color(health_color(other)),
    });
}
