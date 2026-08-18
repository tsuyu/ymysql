//! Query Inspector: one digest in full — counters, recent executions, EXPLAIN.

use egui::RichText;

use super::*;
use crate::app::{App, Tab};
use crate::db::collector::Command;

impl App {
    pub fn inspector_tab(&mut self, ui: &mut egui::Ui) {
        let Some(digest) = self.selected_digest.clone() else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No statement selected").weak());
                if ui.button("Pick one in Top SQL").clicked() {
                    self.tab = Tab::TopSql;
                }
            });
            return;
        };

        let Some(row) = self.inspected.clone() else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("loading digest {}…", one_line(&digest, 24)));
            });
            return;
        };

        ui.horizontal_wrapped(|ui| {
            if ui.button("Refresh").clicked() {
                self.collector.send(Command::InspectDigest(digest.clone()));
            }
            if ui.button("Back to Top SQL").clicked() {
                self.tab = Tab::TopSql;
            }
            ui.separator();
            ui.label(
                RichText::new(format!(
                    "schema: {}",
                    if row.schema.is_empty() {
                        "-"
                    } else {
                        &row.schema
                    }
                ))
                .small(),
            );
            ui.label(
                RichText::new(format!("digest: {}", one_line(&digest, 32)))
                    .small()
                    .weak(),
            );
        });
        ui.separator();

        ui.horizontal_wrapped(|ui| {
            stat(ui, "Executions", fmt_count(row.count));
            stat(ui, "Total time", fmt_ms(row.total_ms));
            stat(ui, "Avg time", fmt_ms(row.avg_ms));
            stat(ui, "Max time", fmt_ms(row.max_ms));
            stat(ui, "Lock time", fmt_ms(row.lock_ms));
            stat_colored(
                ui,
                "Examined/sent",
                format!("{:.0}", row.examined_per_sent()),
                Some(if row.examined_per_sent() > 100.0 {
                    RED
                } else {
                    GREEN
                }),
            );
            stat_colored(
                ui,
                "No index used",
                fmt_count(row.no_index_used),
                Some(if row.no_index_used > 0 { RED } else { GREEN }),
            );
            stat_colored(
                ui,
                "Tmp disk tables",
                fmt_count(row.tmp_disk_tables),
                Some(if row.tmp_disk_tables > 0 {
                    AMBER
                } else {
                    GREEN
                }),
            );
            stat_colored(
                ui,
                "Errors",
                fmt_count(row.errors),
                Some(if row.errors > 0 { RED } else { GREEN }),
            );
        });
        ui.separator();

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.label(RichText::new("Statement").strong());
            let mut text = row.text.clone();
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(4)
                    .interactive(false),
            );
            ui.horizontal(|ui| {
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(row.text.clone());
                }

                let explainable = row.text.trim_start().len() >= 6
                    && row.text.trim_start()[..6].eq_ignore_ascii_case("select");
                let has_placeholders = row.text.contains('?');
                let btn = ui.add_enabled(
                    explainable && !has_placeholders,
                    egui::Button::new("EXPLAIN"),
                );
                if btn.clicked() {
                    self.explain = None;
                    self.collector.send(Command::Explain {
                        schema: row.schema.clone(),
                        sql: row.text.clone(),
                    });
                }
                if !explainable {
                    ui.label(RichText::new("EXPLAIN is limited to SELECT").small().weak());
                } else if has_placeholders {
                    // 5.x has no QUERY_SAMPLE_TEXT, so the only text available is
                    // the normalised digest, which EXPLAIN cannot parse.
                    ui.label(
                        RichText::new(
                            "normalised text contains `?` — copy it, substitute literals, \
                             then EXPLAIN by hand",
                        )
                        .small()
                        .color(AMBER),
                    );
                }
            });

            if let Some(grid) = &self.explain {
                ui.add_space(8.0);
                ui.label(RichText::new("EXPLAIN").strong());
                grid_table(ui, "explain_grid", grid);
            }

            ui.add_space(12.0);
            ui.label(RichText::new("Recent executions").strong());
            if !self.caps.history_long {
                ui.label(RichText::new("history_long not available on this server").weak());
            } else if self.inspect_samples.is_empty() {
                ui.label(
                    RichText::new(
                        "no rows — enable the events_statements_history_long consumer:\n\
                         UPDATE performance_schema.setup_consumers \
                         SET ENABLED='YES' WHERE NAME='events_statements_history_long';",
                    )
                    .small()
                    .weak(),
                );
            } else {
                egui::Grid::new("samples_grid")
                    .striped(true)
                    .num_columns(8)
                    .show(ui, |ui| {
                        for h in [
                            "Duration", "Lock", "Examined", "Sent", "No index", "Tmp disk",
                            "Errors", "SQL",
                        ] {
                            ui.label(RichText::new(h).strong());
                        }
                        ui.end_row();
                        for s in &self.inspect_samples {
                            ui.label(fmt_ms(s.ms));
                            ui.label(fmt_ms(s.lock_ms));
                            ui.label(fmt_count(s.rows_examined));
                            ui.label(fmt_count(s.rows_sent));
                            let flag = if s.no_good_index_used {
                                RichText::new("no good index").color(RED)
                            } else if s.no_index_used {
                                RichText::new("yes").color(RED)
                            } else {
                                RichText::new("-")
                            };
                            ui.label(flag);
                            ui.label(fmt_count(s.tmp_disk_tables));
                            ui.label(if s.errors > 0 {
                                RichText::new(fmt_count(s.errors)).color(RED)
                            } else {
                                RichText::new("0")
                            });
                            ui.label(one_line(&s.sql, 90)).on_hover_text(&s.sql);
                            ui.end_row();
                        }
                    });
            }
        });
    }
}
