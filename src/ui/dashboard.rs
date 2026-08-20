//! Dashboard: the single screen you leave open.

use egui::RichText;

use super::*;
use crate::app::App;
use crate::model::Metric;

impl App {
    pub fn dashboard_tab(&mut self, ui: &mut egui::Ui) {
        if !self.is_connected() && self.latest.is_none() {
            not_connected(ui);
            return;
        }

        let d = self.derived;
        ui.horizontal_wrapped(|ui| {
            stat(ui, "Queries/s", format!("{:.1}", d.qps));
            stat_help(
                ui,
                "Transactions/s",
                format!("{:.1}", d.tps),
                "Handler_commit + Handler_rollback per second.

                 This counts every transaction, including the implicit one 
                 around each autocommit statement -- so on a typical workload 
                 it tracks queries per second rather than sitting near zero. 
                 Com_commit, which many tools use, counts only explicit 
                 COMMIT statements and reads 0 on an autocommit server.",
            );
            stat_colored(
                ui,
                "Threads running",
                format!("{:.0}", d.threads_running),
                Some(if d.threads_running > 40.0 { RED } else { GREEN }),
            );
            stat(
                ui,
                "Threads connected",
                format!("{:.0}", d.threads_connected),
            );
            stat_colored(
                ui,
                "Slow queries/s",
                format!("{:.2}", d.slow_qps),
                Some(if d.slow_qps > 1.0 { AMBER } else { GREEN }),
            );
            stat_colored(
                ui,
                "Buffer pool hit",
                format!("{:.2}%", d.bp_hit_ratio * 100.0),
                Some(if d.bp_hit_ratio < 0.95 { AMBER } else { GREEN }),
            );
            stat(ui, "Net in", fmt_bytes(d.bytes_in_s) + "/s");
            stat(ui, "Net out", fmt_bytes(d.bytes_out_s) + "/s");
            let longest_wait = self
                .lock_waits
                .iter()
                .map(|w| w.waiting.secs.max(0))
                .max()
                .unwrap_or(0);
            stat_colored(
                ui,
                "Lock waits",
                if self.lock_waits.is_empty() {
                    "0".to_string()
                } else {
                    format!(
                        "{} · {}",
                        self.lock_waits.len(),
                        fmt_duration(longest_wait as u64)
                    )
                },
                Some(if self.lock_waits.is_empty() {
                    GREEN
                } else {
                    RED
                }),
            );
            stat_colored(
                ui,
                "Alerts firing",
                self.alerts.firing_count().to_string(),
                Some(if self.alerts.firing_count() == 0 {
                    GREEN
                } else {
                    RED
                }),
            );
        });
        ui.separator();

        let mut inspect_digest = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            live_plot(ui, "Throughput", &self.history, &[Metric::Qps, Metric::Tps]);
            live_plot(
                ui,
                "Threads",
                &self.history,
                &[Metric::ThreadsRunning, Metric::ThreadsConnected],
            );
            live_plot(
                ui,
                "InnoDB rows/s",
                &self.history,
                &[Metric::RowsRead, Metric::RowsWritten],
            );
            live_plot(ui, "Buffer pool hit %", &self.history, &[Metric::BpHitPct]);
            live_plot(
                ui,
                "Network bytes/s",
                &self.history,
                &[Metric::BytesIn, Metric::BytesOut],
            );

            ui.separator();
            ui.label(RichText::new("Worst statements right now").strong());
            egui::Grid::new("dash_top_sql")
                .striped(true)
                .num_columns(4)
                .show(ui, |ui| {
                    for h in ["Total", "Avg", "Count", "Statement"] {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();
                    for q in self.top_sql.iter().take(5) {
                        ui.label(fmt_ms(q.total_ms));
                        ui.label(fmt_ms(q.avg_ms));
                        ui.label(fmt_count(q.count));
                        if ui
                            .link(one_line(&q.text, 110))
                            .on_hover_text("open in Query Inspector")
                            .clicked()
                        {
                            inspect_digest = Some(q.digest.clone());
                        }
                        ui.end_row();
                    }
                });
            if self.top_sql.is_empty() {
                if self.caps.perf_schema_on {
                    ui.label(RichText::new("no statement digests yet").weak());
                } else {
                    perf_schema_warning(ui, "statement digests");
                }
            }

            ui.add_space(8.0);
            self.raw_status_section(ui);
        });

        if let Some(d) = inspect_digest {
            self.inspect(&d);
        }
    }

    /// The full `SHOW GLOBAL STATUS` table, with a per-second delta column.
    fn raw_status_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Raw SHOW GLOBAL STATUS")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Filter");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.status_filter).desired_width(220.0),
                    );
                });

                let Some(cur) = &self.latest else {
                    ui.label("No sample yet.");
                    return;
                };
                let needle = self.status_filter.to_ascii_lowercase();
                let mut keys: Vec<&String> = cur.status_raw.keys().collect();
                keys.sort();

                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .id_salt("raw_status")
                    .show(ui, |ui| {
                        egui::Grid::new("status_grid")
                            .striped(true)
                            .num_columns(3)
                            .show(ui, |ui| {
                                for h in ["Variable", "Value", "Δ/s"] {
                                    ui.label(RichText::new(h).strong());
                                }
                                ui.end_row();
                                for k in keys {
                                    if !needle.is_empty()
                                        && !k.to_ascii_lowercase().contains(&needle)
                                    {
                                        continue;
                                    }
                                    ui.label(k);
                                    ui.label(cur.status_raw.get(k).cloned().unwrap_or_default());
                                    let delta = match (&self.prev, cur.status.get(k)) {
                                        (Some(p), Some(&now)) => {
                                            let dt = cur.t - p.t;
                                            let before = p.stat(k);
                                            if dt > 0.0 && now >= before {
                                                format!("{:.2}", (now - before) as f64 / dt)
                                            } else {
                                                "-".into()
                                            }
                                        }
                                        _ => "-".into(),
                                    };
                                    ui.label(delta);
                                    ui.end_row();
                                }
                            });
                    });
            });
    }
}
