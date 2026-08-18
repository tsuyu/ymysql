//! Dump: export a database's structure, data, or both, to a `.sql` file.

use egui::RichText;

use super::*;
use crate::app::App;
use crate::db::collector::Command;
use crate::db::dump::{self, DumpMode};

impl App {
    pub fn dump_tab(&mut self, ui: &mut egui::Ui) {
        if !self.is_connected() {
            not_connected(ui);
            return;
        }

        self.dump_target_row(ui);
        self.dump_options_row(ui);
        ui.separator();
        self.dump_table_picker(ui);
        ui.separator();
        self.dump_run_row(ui);
        self.dump_status(ui);
    }

    fn dump_target_row(&mut self, ui: &mut egui::Ui) {
        let mut pick: Option<String> = None;

        ui.horizontal_wrapped(|ui| {
            ui.label("Database");
            let current = if self.dump.schema.is_empty() {
                "(pick one)".to_string()
            } else {
                self.dump.schema.clone()
            };
            egui::ComboBox::from_id_salt("dump_schema")
                .width(180.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for s in self.schemas.clone() {
                        if ui.selectable_label(self.dump.schema == s, &s).clicked() {
                            pick = Some(s);
                        }
                    }
                    if self.schemas.is_empty() {
                        ui.label(RichText::new("no databases listed").weak());
                    }
                });
            if ui.small_button("⟳").on_hover_text("reload").clicked() {
                self.collector.send(Command::ListSchemas);
            }

            ui.separator();
            ui.label("Contents");
            for mode in DumpMode::ORDER {
                ui.selectable_value(&mut self.dump.mode, mode, mode.label());
            }
        });

        if let Some(schema) = pick {
            self.set_dump_schema(&schema);
        }
    }

    fn dump_options_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled(
                self.dump.mode.wants_structure(),
                egui::Checkbox::new(&mut self.dump.drop_tables, "DROP TABLE IF EXISTS"),
            )
            .on_hover_text("prefix each CREATE with a DROP, so the file restores over itself");

            ui.checkbox(&mut self.dump.consistent, "consistent snapshot")
                .on_hover_text(
                    "Dump inside START TRANSACTION WITH CONSISTENT SNAPSHOT so the file is one \
                     point in time. InnoDB only; costs a long-running read transaction.",
                );

            ui.separator();
            ui.label("File");
            ui.add(
                egui::TextEdit::singleline(&mut self.dump_path_text)
                    .desired_width(420.0)
                    .hint_text("path to the .sql file to write"),
            );
            if ui
                .small_button("default name")
                .on_hover_text("reset to <database>_<timestamp>.sql in the working directory")
                .clicked()
            {
                self.dump_path_text = dump::suggested_path(&self.dump.schema)
                    .display()
                    .to_string();
            }
        });
    }

    fn dump_table_picker(&mut self, ui: &mut egui::Ui) {
        if self.dump.schema.is_empty() {
            ui.label(RichText::new("Pick a database above.").weak());
            return;
        }

        let total = self.dump_tables.len();
        let chosen = self.dump_selection.len();
        let mut select_all = false;
        let mut select_none = false;

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Tables").strong());
            ui.label(
                RichText::new(if chosen == 0 {
                    format!("all {total}")
                } else {
                    format!("{chosen} of {total}")
                })
                .small()
                .weak(),
            );
            if ui.small_button("select all").clicked() {
                select_all = true;
            }
            if ui
                .small_button("clear")
                .on_hover_text("clear = dump everything")
                .clicked()
            {
                select_none = true;
            }
        });

        if select_all {
            self.dump_selection = self.dump_tables.iter().cloned().collect();
        }
        if select_none {
            self.dump_selection.clear();
        }

        egui::ScrollArea::vertical()
            .id_salt("dump_tables")
            .max_height(240.0)
            .show(ui, |ui| {
                if self.dump_tables.is_empty() {
                    ui.label(RichText::new("loading…").weak());
                    return;
                }
                let names = self.dump_tables.clone();
                ui.horizontal_wrapped(|ui| {
                    for name in names {
                        let mut on = self.dump_selection.contains(&name);
                        if ui.checkbox(&mut on, &name).changed() {
                            if on {
                                self.dump_selection.insert(name);
                            } else {
                                self.dump_selection.remove(&name);
                            }
                        }
                    }
                });
            });
    }

    fn dump_run_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let ready = !self.dump.schema.is_empty() && !self.dump_path_text.trim().is_empty();
            if ui
                .add_enabled(
                    ready && !self.dump_running,
                    egui::Button::new(RichText::new("Start dump").strong()),
                )
                .clicked()
            {
                self.start_dump();
            }
            if ui
                .add_enabled(self.dump_running, egui::Button::new("Cancel"))
                .clicked()
            {
                self.cancel_dump();
            }
            if self.dump_running {
                ui.spinner();
            }
            ui.label(
                RichText::new("reads only — a dump never modifies the server")
                    .small()
                    .weak(),
            );
        });
    }

    fn dump_status(&mut self, ui: &mut egui::Ui) {
        if let Some((table, index, count, rows)) = self.dump_progress.clone() {
            ui.add_space(6.0);
            let done = index as f32 / count.max(1) as f32;
            ui.add(
                egui::ProgressBar::new(done)
                    .show_percentage()
                    .text(format!("{table} · {} rows", fmt_count(rows))),
            );
            ui.label(
                RichText::new(format!("table {} of {count}", index + 1))
                    .small()
                    .weak(),
            );
        }

        let Some(stats) = self.dump_result.clone() else {
            return;
        };
        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if stats.cancelled {
                    ui.colored_label(AMBER, RichText::new("cancelled").strong());
                } else {
                    ui.colored_label(GREEN, RichText::new("finished").strong());
                }
                ui.label(format!("{} tables", stats.tables));
                ui.label(format!("{} rows", fmt_count(stats.rows)));
                ui.label(fmt_bytes(stats.bytes as f64));
            });
            ui.label(
                RichText::new(stats.path.display().to_string())
                    .monospace()
                    .small(),
            );
            ui.horizontal(|ui| {
                if ui.small_button("Copy path").clicked() {
                    ui.ctx().copy_text(stats.path.display().to_string());
                }
                if stats.cancelled {
                    ui.label(
                        RichText::new("the file holds everything written before the stop")
                            .small()
                            .weak(),
                    );
                }
            });
        });
    }
}
