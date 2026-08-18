//! Table browser: pick a table, page through rows, click a cell to edit.
//!
//! Nothing is written until the user applies queued changes, and every change
//! is addressed by primary key.

use egui::RichText;

use super::*;
use crate::app::{App, CellEdit};
use crate::db::collector::Command;
use crate::db::sql::Change;

impl App {
    pub fn table_browser_tab(&mut self, ui: &mut egui::Ui) {
        if !self.is_connected() {
            not_connected(ui);
            return;
        }

        egui::Panel::left("browser_tables")
            .resizable(true)
            .show(ui, |ui| self.table_list(ui));

        if self.browse.table.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Pick a table on the left").weak());
            });
            return;
        }

        // The editor lives in its own bottom panel: a tall result grid used to
        // push it off screen, which made a clicked cell look unresponsive.
        let editing =
            self.editing.is_some() || self.insert_row.is_some() || !self.pending.is_empty();
        if editing {
            egui::Panel::bottom("browser_editor")
                .resizable(true)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("editor_scroll")
                        .max_height(ui.available_height())
                        .show(ui, |ui| {
                            self.edit_panel(ui);
                            self.pending_panel(ui);
                        });
                });
        }

        self.browser_toolbar(ui);
        self.columns_section(ui);
        ui.separator();
        self.browser_grid(ui);
    }

    fn table_list(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Schema");
            let current = if self.browse.schema.is_empty() {
                "(none)".to_string()
            } else {
                self.browse.schema.clone()
            };
            egui::ComboBox::from_id_salt("schema_pick")
                .width(150.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    let schemas = self.schemas.clone();
                    for s in schemas {
                        if ui.selectable_label(self.browse.schema == s, &s).clicked() {
                            self.browse.schema = s.clone();
                            self.tables.clear();
                            self.collector.send(Command::ListTables(s));
                        }
                    }
                });
            if ui.small_button("⟳").on_hover_text("reload list").clicked() {
                self.collector.send(Command::ListSchemas);
                if !self.browse.schema.is_empty() {
                    self.collector
                        .send(Command::ListTables(self.browse.schema.clone()));
                }
            }
        });

        ui.add(
            egui::TextEdit::singleline(&mut self.table_filter)
                .hint_text("filter tables")
                .desired_width(f32::INFINITY),
        );
        ui.separator();

        let needle = self.table_filter.to_ascii_lowercase();
        let schema = self.browse.schema.clone();
        let mut open: Option<String> = None;

        egui::ScrollArea::vertical()
            .id_salt("table_list")
            .show(ui, |ui| {
                for t in &self.tables {
                    if !needle.is_empty() && !t.name.to_ascii_lowercase().contains(&needle) {
                        continue;
                    }
                    let selected = self.browse.table == t.name;
                    let mut label = RichText::new(&t.name);
                    if t.is_view {
                        label = label.italics();
                    }
                    if ui
                        .selectable_label(selected, label)
                        .on_hover_text(format!(
                            "{} · ~{} rows{}",
                            if t.is_view { "view" } else { &t.engine },
                            fmt_count(t.rows),
                            if t.is_view { " (read-only)" } else { "" }
                        ))
                        .clicked()
                    {
                        open = Some(t.name.clone());
                    }
                }
                if self.tables.is_empty() {
                    ui.label(RichText::new("no tables").weak());
                }
            });

        if let Some(table) = open {
            self.open_table(&schema, &table);
        }
    }

    fn browser_toolbar(&mut self, ui: &mut egui::Ui) {
        let editable = self
            .table_schema
            .as_ref()
            .map(|t| t.editable())
            .unwrap_or(false);
        let pk: Vec<String> = self
            .table_schema
            .as_ref()
            .map(|t| t.primary_key().iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();

        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!("{}.{}", self.browse.schema, self.browse.table)).strong(),
            );
            if editable {
                ui.colored_label(GREEN, format!("PK: {}", pk.join(", ")));
            } else {
                ui.colored_label(AMBER, "no primary key — read-only");
            }
            ui.separator();
            let mut writes = self.allow_writes;
            if ui
                .checkbox(&mut writes, "Allow writes")
                .on_hover_text(
                    "Off: this tab only reads.\n\
                     On: cell edits, row deletes and inserts can be applied.\n\
                     Resets to off every time the app starts.",
                )
                .changed()
            {
                self.set_write_mode(writes);
            }

            ui.separator();
            if ui.button("Refresh").clicked() {
                self.refresh_browse();
            }
            if ui.button("Count rows").clicked() {
                self.collector
                    .send(Command::CountRows(Box::new(self.browse.clone())));
            }
            if let Some(n) = self.row_count {
                ui.label(format!("{} rows", fmt_count(n)));
            }
            if editable
                && ui
                    .add_enabled(self.allow_writes, egui::Button::new("New row"))
                    .on_hover_text(if self.allow_writes {
                        "insert a row"
                    } else {
                        "enable writes in the SQL tab first"
                    })
                    .clicked()
            {
                self.start_insert();
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("WHERE");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.browse.filter)
                    .hint_text("status = 'open' AND amount > 100")
                    .desired_width(340.0),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.browse.offset = 0;
                self.row_count = None;
                self.refresh_browse();
            }
            if ui.button("Apply filter").clicked() {
                self.browse.offset = 0;
                self.row_count = None;
                self.refresh_browse();
            }

            ui.separator();
            ui.label("Page size");
            let mut limit = self.browse.limit as u32;
            if ui
                .add(egui::DragValue::new(&mut limit).range(10..=500))
                .changed()
            {
                self.browse.limit = limit as usize;
                self.refresh_browse();
            }

            let page = self.browse.offset / self.browse.limit.max(1) + 1;
            if ui
                .add_enabled(self.browse.offset > 0, egui::Button::new("◀ prev"))
                .clicked()
            {
                self.browse.offset = self.browse.offset.saturating_sub(self.browse.limit);
                self.refresh_browse();
            }
            ui.label(format!("page {page}"));
            let has_more = self
                .browse_result
                .as_ref()
                .map(|r| r.truncated)
                .unwrap_or(false);
            if ui
                .add_enabled(has_more, egui::Button::new("next ▶"))
                .clicked()
            {
                self.browse.offset += self.browse.limit;
                self.refresh_browse();
            }
        });
    }

    /// Column metadata, collapsed by default.
    fn columns_section(&mut self, ui: &mut egui::Ui) {
        let Some(schema) = self.table_schema.clone() else {
            return;
        };
        egui::CollapsingHeader::new(format!(
            "Columns of {}.{} ({})",
            schema.schema,
            schema.table,
            schema.columns.len()
        ))
        .default_open(false)
        .show(ui, |ui| {
            egui::Grid::new("column_meta")
                .striped(true)
                .num_columns(4)
                .show(ui, |ui| {
                    for h in ["Column", "Type", "Null", "Key"] {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();
                    for c in &schema.columns {
                        ui.label(&c.name);
                        ui.label(RichText::new(&c.data_type).monospace().small());
                        ui.label(if c.nullable { "YES" } else { "NO" });
                        let mut marks = Vec::new();
                        if c.is_pk {
                            marks.push("PRI");
                        }
                        if !c.extra.is_empty() {
                            marks.push(c.extra.as_str());
                        }
                        ui.label(marks.join(" · "));
                        ui.end_row();
                    }
                });
        });
    }

    fn browser_grid(&mut self, ui: &mut egui::Ui) {
        let Some(out) = &self.browse_result else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("loading…");
            });
            return;
        };
        if out.grid.rows.is_empty() {
            ui.label(RichText::new("No rows.").weak());
            return;
        }

        let editable = self
            .table_schema
            .as_ref()
            .map(|t| t.editable())
            .unwrap_or(false);
        let sort = self.browse.order_by.as_ref().and_then(|col| {
            out.grid
                .columns
                .iter()
                .position(|c| c == col)
                .map(|i| (i, self.browse.descending))
        });

        let grid = out.grid.clone();
        let resp = data_grid(ui, "browse_grid", &grid, sort, editable);

        // Sorting is server-side: the page must be re-fetched in the new order.
        if let Some(col) = resp.header_clicked
            && let Some(name) = grid.columns.get(col)
        {
            if self.browse.order_by.as_deref() == Some(name.as_str()) {
                self.browse.descending = !self.browse.descending;
            } else {
                self.browse.order_by = Some(name.clone());
                self.browse.descending = false;
            }
            self.browse.offset = 0;
            self.refresh_browse();
        }

        if let Some((row, col)) = resp.cell_clicked {
            self.start_edit(row, col);
        }
    }

    fn start_edit(&mut self, row: usize, col: usize) {
        let Some(key) = self.row_key(row) else {
            self.push_log("cannot edit: row has no primary key value");
            return;
        };
        let Some(out) = &self.browse_result else {
            return;
        };
        let Some(column) = out.grid.columns.get(col).cloned() else {
            return;
        };
        let original = out.grid.rows[row][col].clone();

        self.editing = Some(CellEdit {
            column,
            key,
            value: original.clone().unwrap_or_default(),
            set_null: original.is_none(),
            original,
        });
    }

    fn start_insert(&mut self) {
        let Some(schema) = &self.table_schema else {
            return;
        };
        // Auto-increment and generated columns are left out; the server fills them.
        self.insert_row = Some(
            schema
                .columns
                .iter()
                .filter(|c| !c.extra.to_ascii_lowercase().contains("auto_increment"))
                .filter(|c| !c.extra.to_ascii_lowercase().contains("generated"))
                .map(|c| (c.name.clone(), String::new(), c.nullable))
                .collect(),
        );
    }

    fn edit_panel(&mut self, ui: &mut egui::Ui) {
        if let Some(cells) = self.insert_row.clone() {
            self.insert_panel(ui, cells);
        }

        let Some(edit) = self.editing.clone() else {
            return;
        };

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Editing").strong());
                ui.label(format!(
                    "{}.{} · {} · row {}",
                    self.browse.schema,
                    self.browse.table,
                    edit.column,
                    edit.key
                        .iter()
                        .map(|(k, v)| format!("{k}={}", v.clone().unwrap_or("NULL".into())))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            });

            let mut edit = edit;
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled(
                    !edit.set_null,
                    egui::TextEdit::singleline(&mut edit.value).desired_width(420.0),
                );
                ui.checkbox(&mut edit.set_null, "NULL");
            });

            let new_value = if edit.set_null {
                None
            } else {
                Some(edit.value.clone())
            };
            let changed = new_value != edit.original;

            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        changed && self.allow_writes,
                        egui::Button::new("Queue change"),
                    )
                    .on_hover_text(if self.allow_writes {
                        "add to the pending list"
                    } else {
                        "enable writes in the SQL tab first"
                    })
                    .clicked()
                {
                    self.pending.push(Change::Update {
                        schema: self.browse.schema.clone(),
                        table: self.browse.table.clone(),
                        key: edit.key.clone(),
                        column: edit.column.clone(),
                        value: new_value.clone(),
                    });
                    self.editing = None;
                }
                if ui.button("Cancel").clicked() {
                    self.editing = None;
                }
                ui.separator();
                if ui
                    .add_enabled(self.allow_writes, egui::Button::new("Delete row"))
                    .clicked()
                {
                    self.pending.push(Change::Delete {
                        schema: self.browse.schema.clone(),
                        table: self.browse.table.clone(),
                        key: edit.key.clone(),
                    });
                    self.editing = None;
                }
                if !changed {
                    ui.label(RichText::new("value unchanged").small().weak());
                }
            });

            if !self.allow_writes {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(AMBER, "read-only mode — edits cannot be queued or applied");
                    if ui.button("Enable writes").clicked() {
                        self.set_write_mode(true);
                    }
                });
            }

            if self.editing.is_some() {
                self.editing = Some(edit);
            }
        });
    }

    fn insert_panel(&mut self, ui: &mut egui::Ui, cells: Vec<(String, String, bool)>) {
        let mut cells = cells;
        let mut close = false;
        let mut queue = false;

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new("New row").strong());
            egui::Grid::new("insert_form")
                .num_columns(3)
                .striped(true)
                .show(ui, |ui| {
                    for (name, value, is_null) in cells.iter_mut() {
                        ui.label(name.as_str());
                        ui.add_enabled(
                            !*is_null,
                            egui::TextEdit::singleline(value).desired_width(320.0),
                        );
                        ui.checkbox(is_null, "NULL");
                        ui.end_row();
                    }
                });
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(self.allow_writes, egui::Button::new("Queue insert"))
                    .clicked()
                {
                    queue = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                if !self.allow_writes {
                    ui.colored_label(AMBER, "read-only mode");
                    if ui.button("Enable writes").clicked() {
                        self.set_write_mode(true);
                    }
                }
            });
        });

        if queue {
            self.pending.push(Change::Insert {
                schema: self.browse.schema.clone(),
                table: self.browse.table.clone(),
                values: cells
                    .iter()
                    .map(|(n, v, null)| (n.clone(), if *null { None } else { Some(v.clone()) }))
                    .collect(),
            });
            close = true;
        }
        self.insert_row = if close { None } else { Some(cells) };
    }

    fn pending_panel(&mut self, ui: &mut egui::Ui) {
        if self.pending.is_empty() {
            return;
        }
        ui.add_space(4.0);

        let mut discard: Option<usize> = None;
        let mut apply = false;
        let mut clear = false;
        let mut enable_writes = false;

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(
                    AMBER,
                    RichText::new(format!("{} pending change(s)", self.pending.len())).strong(),
                );
                if ui
                    .add_enabled(self.allow_writes, egui::Button::new("Apply"))
                    .on_hover_text("runs in one transaction; any failure rolls all of them back")
                    .clicked()
                {
                    apply = true;
                }
                if ui.button("Discard all").clicked() {
                    clear = true;
                }
                if !self.allow_writes {
                    ui.colored_label(AMBER, "read-only — nothing will be applied");
                    if ui.button("Enable writes").clicked() {
                        enable_writes = true;
                    }
                }
            });

            egui::ScrollArea::vertical()
                .max_height(180.0)
                .id_salt("pending_list")
                .show(ui, |ui| {
                    for (i, c) in self.pending.iter().enumerate() {
                        ui.horizontal_wrapped(|ui| {
                            ui.colored_label(
                                match c {
                                    Change::Delete { .. } => RED,
                                    _ => AMBER,
                                },
                                c.verb(),
                            );
                            ui.label(
                                RichText::new(crate::db::sql::preview(c))
                                    .monospace()
                                    .small(),
                            );
                            if ui.small_button("×").clicked() {
                                discard = Some(i);
                            }
                        });
                    }
                });
        });

        if enable_writes {
            self.set_write_mode(true);
        }
        if let Some(i) = discard {
            self.pending.remove(i);
        }
        if clear {
            self.pending.clear();
        }
        if apply {
            self.collector
                .send(Command::ApplyChanges(self.pending.clone()));
        }
    }
}
