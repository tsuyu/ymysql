//! SQL console: run one statement, sort the result by clicking a column.

use egui::RichText;

use super::*;
use crate::app::App;
use crate::db::collector::Command;
use crate::db::sql::{self, StatementKind};

impl App {
    pub fn sql_console_tab(&mut self, ui: &mut egui::Ui) {
        let kind = sql::classify(&self.console_sql);
        let blocked = !kind.is_read_only() && !self.allow_writes;

        ui.horizontal_wrapped(|ui| {
            let can_run = self.is_connected() && !self.busy && !blocked;
            if ui
                .add_enabled(can_run, egui::Button::new("Run  (Ctrl+Enter)"))
                .clicked()
            {
                self.run_console_sql();
            }
            if self.busy {
                ui.spinner();
            }

            ui.separator();
            self.schema_picker(ui);

            ui.separator();
            let mut writes = self.allow_writes;
            if ui
                .checkbox(&mut writes, "Allow writes")
                .on_hover_text(
                    "Off: only SELECT/SHOW/EXPLAIN run, and row edits are refused.\n\
                     On: this app can modify the connected server.",
                )
                .changed()
            {
                self.set_write_mode(writes);
            }

            ui.separator();
            let (color, label) = match kind {
                StatementKind::Read => (GREEN, "read"),
                StatementKind::Write => (AMBER, "write"),
                StatementKind::Ddl => (RED, "DDL"),
                StatementKind::Unknown => (AMBER, "unrecognised"),
            };
            ui.colored_label(color, format!("statement: {label}"));
            if blocked {
                ui.colored_label(RED, "blocked in read-only mode");
            }

            ui.separator();
            ui.label(
                RichText::new(format!("rows capped at {}", sql::DEFAULT_ROW_LIMIT))
                    .small()
                    .weak(),
            );
        });

        // Ctrl+Enter runs, matching every other SQL client.
        let run_shortcut = ui.input(|i| {
            i.key_pressed(egui::Key::Enter) && (i.modifiers.ctrl || i.modifiers.command)
        });

        ui.add(
            egui::TextEdit::multiline(&mut self.console_sql)
                .code_editor()
                .desired_width(f32::INFINITY)
                .desired_rows(6)
                .hint_text("SELECT * FROM demo.orders WHERE created_at > NOW() - INTERVAL 1 DAY"),
        );

        if run_shortcut && self.is_connected() && !self.busy && !blocked {
            self.run_console_sql();
        }

        self.saved_queries_section(ui);

        if !self.console_history.is_empty() {
            let mut pick: Option<String> = None;
            egui::CollapsingHeader::new(format!("Recent ({})", self.console_history.len()))
                .default_open(false)
                .show(ui, |ui| {
                    for h in self.console_history.iter().rev() {
                        if ui.link(one_line(h, 120)).clicked() {
                            pick = Some(h.clone());
                        }
                    }
                });
            if let Some(sql) = pick {
                self.console_sql = sql;
            }
        }

        ui.separator();

        if let Some(err) = &self.console_error {
            ui.colored_label(RED, err);
            ui.add_space(4.0);
        }

        let Some(out) = &self.console_result else {
            ui.label(RichText::new("No result yet.").weak());
            return;
        };

        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} rows", out.grid.rows.len()));
            ui.separator();
            ui.label(fmt_ms(out.elapsed_ms));
            if out.affected > 0 {
                ui.separator();
                ui.colored_label(AMBER, format!("{} rows affected", out.affected));
            }
            if let Some(id) = out.last_insert_id {
                ui.separator();
                ui.label(format!("last insert id {id}"));
            }
            if out.truncated {
                ui.separator();
                ui.colored_label(
                    AMBER,
                    format!("truncated at {} rows", sql::DEFAULT_ROW_LIMIT),
                );
            }
            if !out.info.trim().is_empty() {
                ui.separator();
                ui.label(RichText::new(out.info.trim()).small().weak());
            }
        });
        ui.add_space(4.0);

        if out.grid.columns.is_empty() {
            ui.label(RichText::new("Statement returned no result set.").weak());
            return;
        }

        // Any column the server traced back to a real table can be opened for
        // editing, so those cells are clickable.
        let jumpable = (0..out.grid.columns.len()).any(|i| out.grid.origin(i).is_some());
        if jumpable {
            ui.label(
                RichText::new(
                    "click a value to open its row in the Table Browser · click a header to sort",
                )
                .small()
                .weak(),
            );
        }

        // Console results are already in memory, so sorting is local.
        let order = sorted_order(&out.grid, self.console_sort);
        let view = reordered(&out.grid, &order);
        let resp = data_grid(ui, "console_result", &view, self.console_sort, jumpable);

        if let Some(col) = resp.header_clicked {
            self.console_sort = match self.console_sort {
                Some((c, false)) if c == col => Some((col, true)),
                Some((c, true)) if c == col => None,
                _ => Some((col, false)),
            };
        }

        if let Some((row, col)) = resp.cell_clicked {
            // `view` is a sorted copy: map the click back to the source row.
            let value = view.rows[row][col].clone();
            if let Some(origin) = out.grid.origin(col).cloned() {
                self.open_table_filtered(
                    &origin.schema,
                    &origin.table,
                    &origin.column,
                    value.as_deref(),
                );
            } else {
                self.push_log("that column is not a plain table column — nothing to open");
            }
        }
    }

    /// Statements saved against the connection profile in the top bar.
    fn saved_queries_section(&mut self, ui: &mut egui::Ui) {
        let profile = self.profile_name.clone();
        let known_profile = self.profiles.get(&profile).is_some();
        let saved: Vec<crate::profiles::SavedQuery> = self.saved_queries().to_vec();

        let mut load: Option<String> = None;
        let mut delete: Option<String> = None;
        let mut save = false;

        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.query_name)
                    .hint_text("query name")
                    .desired_width(160.0),
            );
            if ui
                .add_enabled(known_profile, egui::Button::new("Save query"))
                .on_hover_text(if known_profile {
                    "store this statement under the current connection profile"
                } else {
                    "save the connection as a profile first"
                })
                .clicked()
            {
                save = true;
            }
            if known_profile {
                ui.label(RichText::new(format!("profile: {profile}")).small().weak());
            } else {
                ui.colored_label(
                    AMBER,
                    RichText::new("no profile selected — queries cannot be saved").small(),
                );
            }
        });

        if !saved.is_empty() {
            egui::CollapsingHeader::new(format!("Saved queries ({})", saved.len()))
                .default_open(true)
                .show(ui, |ui| {
                    egui::Grid::new("saved_queries")
                        .striped(true)
                        .num_columns(4)
                        .show(ui, |ui| {
                            for q in &saved {
                                if ui
                                    .link(RichText::new(&q.name).strong())
                                    .on_hover_text("load into the editor")
                                    .clicked()
                                {
                                    load = Some(q.name.clone());
                                }
                                ui.label(
                                    RichText::new(q.schema.as_deref().unwrap_or("(none)"))
                                        .small()
                                        .weak(),
                                );
                                ui.label(RichText::new(one_line(&q.sql, 90)).monospace().small())
                                    .on_hover_text(&q.sql);
                                if ui.small_button("×").on_hover_text("delete").clicked() {
                                    delete = Some(q.name.clone());
                                }
                                ui.end_row();
                            }
                        });
                });
        }

        if save {
            self.save_query();
        }
        if let Some(name) = load {
            self.load_query(&name);
        }
        if let Some(name) = delete {
            self.delete_query(&name);
        }
    }

    /// Default database for unqualified table names.
    fn schema_picker(&mut self, ui: &mut egui::Ui) {
        ui.label("Database");
        let selected = self
            .console_schema
            .clone()
            .unwrap_or_else(|| "(none)".to_string());

        egui::ComboBox::from_id_salt("console_schema")
            .width(160.0)
            .selected_text(selected)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.console_schema.is_none(), "(none)")
                    .on_hover_text("run without USE — table names must be qualified")
                    .clicked()
                {
                    self.console_schema = None;
                }
                let schemas = self.schemas.clone();
                for s in schemas {
                    if ui
                        .selectable_label(self.console_schema.as_deref() == Some(s.as_str()), &s)
                        .clicked()
                    {
                        self.console_schema = Some(s);
                    }
                }
            });

        if ui
            .small_button("⟳")
            .on_hover_text("reload database list")
            .clicked()
        {
            self.collector.send(Command::ListSchemas);
        }
        if self.schemas.is_empty() {
            ui.label(RichText::new("no databases listed").small().weak());
        }
    }

    fn run_console_sql(&mut self) {
        let stmt = self.console_sql.trim().to_string();
        if stmt.is_empty() {
            return;
        }
        self.console_error = None;
        if self.console_history.last() != Some(&stmt) {
            self.console_history.push(stmt.clone());
            if self.console_history.len() > 50 {
                self.console_history.remove(0);
            }
        }
        self.collector.send(Command::RunSql {
            schema: self.console_schema.clone(),
            sql: stmt,
        });
    }
}
