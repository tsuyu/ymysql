//! Top SQL: statement digests ranked by cost.

use egui::RichText;

use super::*;
use crate::app::{App, SqlSort};
use crate::db::collector::Command;

impl App {
    pub fn top_sql_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Refresh").clicked() {
                self.collector.send(Command::RefreshHeavy);
            }
            ui.separator();
            ui.label("Sort by");
            for s in SqlSort::ORDER {
                ui.selectable_value(&mut self.sql_sort, s, s.label());
            }
            ui.separator();
            ui.label("Filter");
            ui.add(egui::TextEdit::singleline(&mut self.sql_filter).desired_width(200.0));
        });

        if !self.caps.perf_schema_on {
            perf_schema_warning(ui, "statement digests");
            return;
        }
        if !self.caps.digest_sample_text {
            ui.label(
                RichText::new(
                    "MySQL 5.x: statements shown normalised (QUERY_SAMPLE_TEXT is 8.0-only)",
                )
                .small()
                .weak(),
            );
        }
        ui.separator();

        // Sort a view, never the source: the collector keeps overwriting it.
        let needle = self.sql_filter.to_ascii_lowercase();
        let mut view: Vec<&crate::db::queries::DigestRow> = self
            .top_sql
            .iter()
            .filter(|q| {
                needle.is_empty()
                    || q.text.to_ascii_lowercase().contains(&needle)
                    || q.schema.to_ascii_lowercase().contains(&needle)
            })
            .collect();
        let sort = self.sql_sort;
        view.sort_by(|a, b| {
            sort.key(b)
                .partial_cmp(&sort.key(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let selected = self.selected_digest.clone();
        let mut inspect_digest = None;

        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("top_sql_grid")
                .striped(true)
                .num_columns(9)
                .show(ui, |ui| {
                    for h in [
                        "Schema",
                        "Count",
                        "Total",
                        "Avg",
                        "Max",
                        "Examined/sent",
                        "Tmp disk",
                        "No index",
                        "Statement",
                    ] {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();

                    for q in view {
                        let is_selected = selected.as_deref() == Some(q.digest.as_str());
                        ui.label(&q.schema);
                        ui.label(fmt_count(q.count));
                        ui.label(fmt_ms(q.total_ms));
                        ui.label(fmt_ms(q.avg_ms));
                        ui.label(fmt_ms(q.max_ms));

                        let ratio = q.examined_per_sent();
                        ui.label(
                            RichText::new(format!("{ratio:.0}")).color(if ratio > 100.0 {
                                RED
                            } else {
                                GREEN
                            }),
                        );
                        ui.label(if q.tmp_disk_tables > 0 {
                            RichText::new(fmt_count(q.tmp_disk_tables)).color(AMBER)
                        } else {
                            RichText::new("0")
                        });
                        ui.label(if q.no_index_used > 0 {
                            RichText::new(fmt_count(q.no_index_used)).color(RED)
                        } else {
                            RichText::new("0")
                        });

                        let mut text = RichText::new(one_line(&q.text, 150));
                        if is_selected {
                            text = text.color(BLUE).strong();
                        }
                        if ui
                            .link(text)
                            .on_hover_text("open in Query Inspector")
                            .clicked()
                        {
                            inspect_digest = Some(q.digest.clone());
                        }
                        ui.end_row();
                    }
                });
        });

        if let Some(d) = inspect_digest {
            self.inspect(&d);
        }
    }
}
