//! Replication: how far behind each channel is, and where this server's own
//! binary log stands.

use egui::RichText;

use super::*;
use crate::app::App;
use crate::db::collector::Command;
use crate::replication::{Health, LAG_BAD_SECS, LAG_WATCH_SECS, Replica};

impl App {
    pub fn replication_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Refresh").clicked() {
                self.collector.send(Command::RefreshHeavy);
            }
            ui.separator();
            let terms = if self.caps.replica_terms {
                "SHOW REPLICA STATUS (MySQL 8.0.22+)"
            } else {
                "SHOW SLAVE STATUS (MySQL 5.x / early 8.0)"
            };
            ui.label(RichText::new(terms).small().weak());
        });
        ui.separator();

        if let Some(err) = self.replication_error.clone() {
            ui.colored_label(RED, err);
            ui.label(
                RichText::new(
                    "Reading replication status needs the REPLICATION CLIENT privilege. \
                     scripts/monitor-user.sql grants it.",
                )
                .small()
                .weak(),
            );
            ui.separator();
        }

        egui::ScrollArea::vertical()
            .id_salt("replication_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.replicas.is_empty() {
                    ui.label(RichText::new("This server is not replicating from anywhere.").weak());
                } else {
                    for r in &self.replicas.clone() {
                        replica_card(ui, r);
                        ui.add_space(8.0);
                    }
                }

                ui.add_space(4.0);
                self.source_section(ui);
            });
    }

    fn source_section(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("As a source").heading());
            match &self.source_status {
                Some(src) if src.logging() => {
                    ui.horizontal_wrapped(|ui| {
                        stat(ui, "Binary log", src.file.clone());
                        stat(ui, "Position", fmt_count(src.position));
                        if !src.binlog_do_db.is_empty() {
                            stat(ui, "Do DB", src.binlog_do_db.clone());
                        }
                        if !src.binlog_ignore_db.is_empty() {
                            stat(ui, "Ignore DB", src.binlog_ignore_db.clone());
                        }
                    });
                    if !src.executed_gtid_set.is_empty() {
                        ui.label(
                            RichText::new(format!("executed GTIDs: {}", src.executed_gtid_set))
                                .small()
                                .weak(),
                        );
                    }
                }
                _ => {
                    ui.colored_label(AMBER, "Binary logging is off.");
                    ui.label(
                        RichText::new(
                            "Without it this server cannot be replicated from, and \
                             point-in-time recovery is not possible.",
                        )
                        .small()
                        .weak(),
                    );
                }
            }

            ui.add_space(6.0);
            let count = self.connected_replicas.rows.len();
            ui.label(RichText::new(format!("Connected replicas: {count}")).strong());
            if count > 0 {
                grid_table(ui, "connected_replicas", &self.connected_replicas.clone());
            }
        });
    }
}

fn health_color(h: Health) -> egui::Color32 {
    match h {
        Health::Good => GREEN,
        Health::Watch => AMBER,
        Health::Bad => RED,
    }
}

fn replica_card(ui: &mut egui::Ui, r: &Replica) {
    let health = r.health();
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            let name = if r.channel.is_empty() {
                "default channel".to_string()
            } else {
                format!("channel {}", r.channel)
            };
            ui.colored_label(health_color(health), RichText::new(name).strong());
            ui.separator();
            ui.colored_label(health_color(health), r.summary());
            ui.separator();
            ui.label(format!(
                "from {}:{} as {}",
                blank(&r.source_host),
                blank(&r.source_port),
                blank(&r.source_user)
            ));
        });

        ui.horizontal_wrapped(|ui| {
            stat_colored(
                ui,
                "IO thread",
                blank(&r.io_running).to_string(),
                Some(if r.io_ok() { GREEN } else { RED }),
            );
            stat_colored(
                ui,
                "SQL thread",
                blank(&r.sql_running).to_string(),
                Some(if r.sql_ok() { GREEN } else { RED }),
            );
            stat_colored(
                ui,
                "Lag",
                match r.seconds_behind {
                    Some(s) => format!("{s}s"),
                    None => "NULL".to_string(),
                },
                Some(health_color(health)),
            );
            if let Some(bytes) = r.apply_backlog_bytes() {
                stat_colored(
                    ui,
                    "Apply backlog",
                    fmt_bytes(bytes as f64),
                    Some(if bytes > 0 { AMBER } else { GREEN }),
                );
            }
            stat(ui, "Relay log space", fmt_bytes(r.relay_log_space as f64));
            if r.sql_delay > 0 {
                stat(ui, "Configured delay", format!("{}s", r.sql_delay));
            }
            stat(
                ui,
                "GTID auto-position",
                if r.auto_position == "1" { "on" } else { "off" }.to_string(),
            );
        });

        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!(
                    "reading {} @ {} · applying {} @ {}",
                    blank(&r.source_log_file),
                    fmt_count(r.read_source_log_pos),
                    blank(&r.relay_source_log_file),
                    fmt_count(r.exec_source_log_pos),
                ))
                .small()
                .weak(),
            );
        });

        if !r.sql_state.is_empty() {
            ui.label(RichText::new(&r.sql_state).small().weak());
        }

        if let Some(err) = r.error_text() {
            ui.colored_label(RED, err);
        }

        // The interpretation, which is the part that is easy to get wrong.
        let note = if !r.io_ok() {
            "The receiver is down: nothing is arriving from the source. Check the \
             network, the credentials and the source's binary log retention."
        } else if !r.sql_ok() {
            "The applier is stopped, usually on a statement that failed. Relay logs \
             keep growing until it is fixed."
        } else if r.seconds_behind.is_none() {
            "Lag reads NULL, which means not replicating rather than caught up."
        } else if r.apply_backlog_bytes().is_some_and(|b| b > 0) {
            "Events have arrived but are not applied yet, so the applier is the \
             bottleneck rather than the network."
        } else if r.sql_delay > 0 {
            "This replica is deliberately delayed, so the lag figure includes that \
             delay and is not a fault."
        } else {
            "Both threads are running and the applier has caught up with what it \
             has received."
        };
        let rich = RichText::new(note).small();
        ui.label(match health {
            Health::Good => rich.weak(),
            other => rich.color(health_color(other)),
        });

        if health == Health::Watch {
            ui.label(
                RichText::new(format!(
                    "amber past {LAG_WATCH_SECS}s, red past {LAG_BAD_SECS}s"
                ))
                .small()
                .weak(),
            );
        }
    });
}

fn blank(s: &str) -> &str {
    if s.is_empty() { "-" } else { s }
}
