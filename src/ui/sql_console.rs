//! SQL console: run one statement, sort the result by clicking a column.

use egui::RichText;

use super::*;
use crate::app::App;
use crate::csv;
use crate::db::collector::Command;
use crate::db::sql::{self, StatementKind};
use crate::fmt_sql;
use crate::html_table;
use crate::insert_sql;
use crate::json_rows;
use crate::markdown_table;

impl App {
    pub fn sql_console_tab(&mut self, ui: &mut egui::Ui) {
        let kind = sql::classify(&self.console_sql);
        let blocked = !kind.is_read_only() && !self.allow_writes;

        ui.horizontal_wrapped(|ui| {
            // Other jobs no longer gate this one: only another console
            // statement does, because there is one cancel slot.
            let can_run = self.is_connected() && !self.console_running && !blocked;
            if ui
                .add_enabled(can_run, egui::Button::new("Run  (Ctrl+Enter)"))
                .clicked()
            {
                self.run_console_sql();
            }
            if self.console_running {
                ui.spinner();
                if ui
                    .button("Cancel")
                    .on_hover_text(
                        "KILL QUERY on the connection running this statement. The
                         connection stays open, so the session is not lost.",
                    )
                    .clicked()
                {
                    self.collector.send(Command::CancelSql);
                    self.push_log("cancelling statement");
                }
            } else if self.busy {
                ui.spinner();
            }

            if ui
                .add_enabled(
                    !self.console_sql.trim().is_empty(),
                    egui::Button::new("Format  (Ctrl+Shift+F)"),
                )
                .on_hover_text(
                    "Re-indents the editor text. Only whitespace and the case of reserved 
                     words change -- literals, quoted identifiers and comments are left alone.",
                )
                .clicked()
            {
                self.console_sql = fmt_sql::format(&self.console_sql);
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

        // Ctrl+Enter runs and Ctrl+Shift+F formats, matching every other SQL
        // client. Both are read before the editor so a keypress is not eaten.
        let (run_shortcut, format_shortcut) = ui.input(|i| {
            let cmd = i.modifiers.ctrl || i.modifiers.command;
            (
                i.key_pressed(egui::Key::Enter) && cmd,
                i.key_pressed(egui::Key::F) && cmd && i.modifiers.shift,
            )
        });
        if format_shortcut && !self.console_sql.trim().is_empty() {
            self.console_sql = fmt_sql::format(&self.console_sql);
        }

        ui.add(
            egui::TextEdit::multiline(&mut self.console_sql)
                .code_editor()
                .desired_width(f32::INFINITY)
                .desired_rows(6)
                .hint_text("SELECT * FROM demo.orders WHERE created_at > NOW() - INTERVAL 1 DAY"),
        );

        if run_shortcut && self.is_connected() && !self.console_running && !blocked {
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

        // Export what is on screen, sorted order included. Borrow the export
        // fields on their own so the `console_result` borrow above stays valid.
        let csv_path = &mut self.csv_path_text;
        let bom = &mut self.csv_bom;
        let json_path = &mut self.json_path_text;
        let json_opts = &mut self.json_opts;
        let markdown_path = &mut self.markdown_path_text;
        let html_path = &mut self.html_path_text;
        let html_opts = &mut self.html_opts;
        let insert_path = &mut self.insert_path_text;
        let insert_opts = &mut self.insert_opts;
        let status = &mut self.export_status;
        if csv_path.is_empty() {
            *csv_path = csv::suggested_path().display().to_string();
        }
        if json_path.is_empty() {
            *json_path = json_rows::suggested_path(json_opts.ndjson)
                .display()
                .to_string();
        }
        if markdown_path.is_empty() {
            *markdown_path = markdown_table::suggested_path().display().to_string();
        }
        if html_path.is_empty() {
            *html_path = html_table::suggested_path().display().to_string();
        }
        if insert_path.is_empty() {
            *insert_path = insert_sql::suggested_path().display().to_string();
        }
        if insert_opts.table.is_empty() {
            insert_opts.table = insert_sql::suggested_table(&view);
        }
        // Label the exported page with the source table when there is one, and
        // follow the field if the user renames the target.
        let table = insert_opts.table.trim();
        html_opts.title = if table.is_empty() {
            "Query result".to_string()
        } else {
            table.to_string()
        };

        egui::CollapsingHeader::new("Export")
            .id_salt("console_export")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("CSV");
                    if ui
                        .button("Copy")
                        .on_hover_text("Puts the whole result on the clipboard, header row first.")
                        .clicked()
                    {
                        ui.ctx().copy_text(csv::render(&view));
                        *status = Some((true, format!("{} rows copied as CSV", view.rows.len())));
                    }
                    let path = std::path::PathBuf::from(csv_path.trim());
                    let usable =
                        !csv_path.trim().is_empty() && crate::db::dump::path_is_usable(&path);
                    if ui.add_enabled(usable, egui::Button::new("Save")).clicked() {
                        *status = Some(match csv::write_file(&path, &view, *bom) {
                            Ok(()) => (
                                true,
                                format!("{} rows written to {}", view.rows.len(), path.display()),
                            ),
                            Err(e) => (false, format!("{e:#}")),
                        });
                    }
                    ui.add(
                        egui::TextEdit::singleline(csv_path)
                            .desired_width(300.0)
                            .hint_text("path to the .csv file to write"),
                    );
                    ui.checkbox(bom, "Excel BOM").on_hover_text(
                        "Prefixes a UTF-8 byte-order mark so Excel reads accented and \
                         non-Latin text correctly. Turn it off for anything that parses \
                         the file itself.",
                    );
                    if !csv_path.trim().is_empty() && !usable {
                        ui.colored_label(RED, "no such directory");
                    }
                });

                ui.horizontal_wrapped(|ui| {
                    ui.label("SQL");
                    let named = !insert_opts.table.trim().is_empty();
                    if ui
                        .add_enabled(named, egui::Button::new("Copy"))
                        .on_hover_text("Puts the rows on the clipboard as INSERT statements.")
                        .clicked()
                    {
                        *status = Some(match insert_sql::render(&view, insert_opts) {
                            Ok(sql) => {
                                ui.ctx().copy_text(sql);
                                (true, format!("{} rows copied as SQL", view.rows.len()))
                            }
                            Err(e) => (false, format!("{e:#}")),
                        });
                    }
                    let path = std::path::PathBuf::from(insert_path.trim());
                    let usable = named
                        && !insert_path.trim().is_empty()
                        && crate::db::dump::path_is_usable(&path);
                    if ui.add_enabled(usable, egui::Button::new("Save")).clicked() {
                        *status = Some(match insert_sql::write_file(&path, &view, insert_opts) {
                            Ok(()) => (
                                true,
                                format!("{} rows written to {}", view.rows.len(), path.display()),
                            ),
                            Err(e) => (false, format!("{e:#}")),
                        });
                    }
                    ui.add(
                        egui::TextEdit::singleline(insert_path)
                            .desired_width(300.0)
                            .hint_text("path to the .sql file to write"),
                    );
                });

                ui.horizontal_wrapped(|ui| {
                    ui.label("JSON");
                    if ui
                        .button("Copy")
                        .on_hover_text("Puts the rows on the clipboard as JSON objects.")
                        .clicked()
                    {
                        ui.ctx().copy_text(json_rows::render(&view, json_opts));
                        *status = Some((true, format!("{} rows copied as JSON", view.rows.len())));
                    }
                    let path = std::path::PathBuf::from(json_path.trim());
                    let usable =
                        !json_path.trim().is_empty() && crate::db::dump::path_is_usable(&path);
                    if ui.add_enabled(usable, egui::Button::new("Save")).clicked() {
                        *status = Some(match json_rows::write_file(&path, &view, json_opts) {
                            Ok(()) => (
                                true,
                                format!("{} rows written to {}", view.rows.len(), path.display()),
                            ),
                            Err(e) => (false, format!("{e:#}")),
                        });
                    }
                    ui.add(
                        egui::TextEdit::singleline(json_path)
                            .desired_width(300.0)
                            .hint_text("path to the .json file to write"),
                    );
                    ui.add_enabled(
                        !json_opts.ndjson,
                        egui::Checkbox::new(&mut json_opts.pretty, "pretty"),
                    )
                    .on_hover_text("Indented, one field per line.");
                    if ui
                        .checkbox(&mut json_opts.ndjson, "NDJSON")
                        .on_hover_text(
                            "One compact object per line with no wrapping array, for \
                             streaming into log pipelines.",
                        )
                        .changed()
                    {
                        // The extension has to follow the shape being written.
                        *json_path = json_rows::suggested_path(json_opts.ndjson)
                            .display()
                            .to_string();
                    }
                });

                ui.horizontal_wrapped(|ui| {
                    ui.label("Markdown");
                    if ui
                        .button("Copy")
                        .on_hover_text("Puts the result on the clipboard as a Markdown table.")
                        .clicked()
                    {
                        ui.ctx().copy_text(markdown_table::render(&view));
                        *status =
                            Some((true, format!("{} rows copied as Markdown", view.rows.len())));
                    }
                    let path = std::path::PathBuf::from(markdown_path.trim());
                    let usable =
                        !markdown_path.trim().is_empty() && crate::db::dump::path_is_usable(&path);
                    if ui.add_enabled(usable, egui::Button::new("Save")).clicked() {
                        *status = Some(match markdown_table::write_file(&path, &view) {
                            Ok(()) => (
                                true,
                                format!("{} rows written to {}", view.rows.len(), path.display()),
                            ),
                            Err(e) => (false, format!("{e:#}")),
                        });
                    }
                    ui.add(
                        egui::TextEdit::singleline(markdown_path)
                            .desired_width(300.0)
                            .hint_text("path to the .md file to write"),
                    );
                });

                ui.horizontal_wrapped(|ui| {
                    ui.label("HTML");
                    if ui
                        .button("Copy")
                        .on_hover_text("Puts the result on the clipboard as an HTML table.")
                        .clicked()
                    {
                        ui.ctx().copy_text(html_table::render(&view, html_opts));
                        *status = Some((true, format!("{} rows copied as HTML", view.rows.len())));
                    }
                    let path = std::path::PathBuf::from(html_path.trim());
                    let usable =
                        !html_path.trim().is_empty() && crate::db::dump::path_is_usable(&path);
                    if ui.add_enabled(usable, egui::Button::new("Save")).clicked() {
                        *status = Some(match html_table::write_file(&path, &view, html_opts) {
                            Ok(()) => (
                                true,
                                format!("{} rows written to {}", view.rows.len(), path.display()),
                            ),
                            Err(e) => (false, format!("{e:#}")),
                        });
                    }
                    ui.add(
                        egui::TextEdit::singleline(html_path)
                            .desired_width(300.0)
                            .hint_text("path to the .html file to write"),
                    );
                    ui.checkbox(&mut html_opts.full_document, "standalone page")
                        .on_hover_text(
                            "On: a complete page with a stylesheet. Off: a bare <table> \
                             to paste into a page of your own.",
                        );
                });

                ui.horizontal_wrapped(|ui| {
                    ui.label("     into");
                    ui.add(
                        egui::TextEdit::singleline(&mut insert_opts.table)
                            .desired_width(220.0)
                            .hint_text("table or schema.table"),
                    )
                    .on_hover_text(
                        "Pre-filled when every column traces back to one table. A join \
                         leaves it blank -- name the target yourself.",
                    );
                    egui::ComboBox::from_id_salt("insert_verb")
                        .selected_text(insert_opts.verb.label())
                        .show_ui(ui, |ui| {
                            for v in insert_sql::Verb::ALL {
                                ui.selectable_value(&mut insert_opts.verb, v, v.label());
                            }
                        });
                    ui.add(
                        egui::DragValue::new(&mut insert_opts.rows_per_statement)
                            .range(1..=1000)
                            .prefix("rows/statement "),
                    );
                    ui.label(
                        RichText::new("values are quoted strings; MySQL casts them on insert")
                            .small()
                            .weak(),
                    );
                });
            });

        if let Some((ok, msg)) = status {
            ui.colored_label(if *ok { GREEN } else { RED }, msg.as_str());
        }
        ui.add_space(4.0);

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
