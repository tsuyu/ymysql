//! Historical Metrics: the on-disk store, plotted over a chosen window.

use egui::RichText;

use super::*;
use crate::app::{App, HistRange};
use crate::model::Metric;
use crate::store::{self, StoreCmd};

impl App {
    pub fn historical_tab(&mut self, ui: &mut egui::Ui) {
        let mut reload = false;

        ui.horizontal_wrapped(|ui| {
            ui.label("Range");
            for r in HistRange::ORDER {
                if ui
                    .selectable_label(self.hist_range == r, r.label())
                    .clicked()
                {
                    self.hist_range = r;
                    reload = true;
                }
            }
            ui.separator();
            if ui.button("Reload").clicked() {
                reload = true;
            }
            if self.hist_pending {
                ui.spinner();
            }
            if self.hist_downsampled {
                ui.label(
                    RichText::new("showing 1-minute rollups")
                        .small()
                        .color(AMBER),
                )
                .on_hover_text("Raw samples are kept for 6 hours, rollups for 30 days");
            }
            ui.separator();
            if ui
                .add_enabled(
                    !self.server_label.is_empty(),
                    egui::Button::new("Export CSV"),
                )
                .clicked()
            {
                self.export_csv();
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("Metrics");
            for m in Metric::ALL {
                let mut on = self.hist_metrics.contains(&m);
                if ui.checkbox(&mut on, m.label()).changed() {
                    if on {
                        self.hist_metrics.insert(m);
                    } else {
                        self.hist_metrics.remove(&m);
                        self.hist_series.remove(&m);
                    }
                    reload = true;
                }
            }
        });

        if self.server_label.is_empty() {
            ui.separator();
            ui.label(RichText::new("Connect once to start recording history for a server.").weak());
            return;
        }
        ui.separator();

        // Byte rates and counts do not share a sensible Y axis, so they are
        // plotted apart.
        let byte_series: Vec<(Metric, Vec<[f64; 2]>)> = self
            .hist_series
            .iter()
            .filter(|(m, _)| m.is_bytes())
            .map(|(m, p)| (*m, p.clone()))
            .collect();
        let rate_series: Vec<(Metric, Vec<[f64; 2]>)> = self
            .hist_series
            .iter()
            .filter(|(m, _)| !m.is_bytes())
            .map(|(m, p)| (*m, p.clone()))
            .collect();
        let empty = byte_series
            .iter()
            .chain(rate_series.iter())
            .all(|(_, p)| p.is_empty());

        egui::ScrollArea::vertical().show(ui, |ui| {
            if empty {
                ui.label(
                    RichText::new(
                        "No stored samples in this window yet — the store fills while connected.",
                    )
                    .weak(),
                );
            }
            if !rate_series.is_empty() {
                ui.label(RichText::new("Rates").strong());
                time_plot(ui, "hist_rates", &rate_series, 260.0);
                ui.add_space(8.0);
            }
            if !byte_series.is_empty() {
                ui.label(RichText::new("Network bytes/s").strong());
                time_plot(ui, "hist_bytes", &byte_series, 200.0);
                ui.add_space(8.0);
            }

            ui.separator();
            self.store_stats_section(ui);
        });

        if reload {
            self.request_history();
        }
    }

    fn store_stats_section(&mut self, ui: &mut egui::Ui) {
        let Some(stats) = self.hist_stats.clone() else {
            return;
        };
        ui.label(RichText::new("Store").strong());
        ui.horizontal_wrapped(|ui| {
            stat(ui, "Raw samples", fmt_count(stats.raw_rows.max(0) as u64));
            stat(
                ui,
                "Rollup rows",
                fmt_count(stats.rollup_rows.max(0) as u64),
            );
            stat(ui, "File size", fmt_bytes(stats.file_bytes as f64));
            stat(
                ui,
                "Oldest point",
                stats
                    .oldest_ms
                    .map(format_datetime)
                    .unwrap_or_else(|| "-".into()),
            );
        });
        ui.label(
            RichText::new(format!("{}", stats.path.display()))
                .small()
                .weak(),
        );
        ui.label(
            RichText::new(
                "Retention: raw samples 6 hours, one-minute rollups 30 days, \
                 recorded for as long as the app is connected.",
            )
            .small()
            .weak(),
        );
    }

    fn export_csv(&mut self) {
        let to_ms = store::now_ms();
        let from_ms = to_ms - self.hist_range.secs() * 1000;
        let name = format!(
            "mysql_perf_{}_{}.csv",
            self.server_label.replace([':', '@', '/', '\\', '.'], "_"),
            chrono::Local::now().format("%Y%m%d_%H%M%S")
        );
        let path = std::env::current_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(name);

        self.store.send(StoreCmd::ExportCsv {
            path: path.clone(),
            server: self.server_label.clone(),
            metrics: self.hist_metrics.iter().copied().collect(),
            from_ms,
            to_ms,
        });
        self.push_log(format!("exporting to {}", path.display()));
    }
}
