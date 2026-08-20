//! Lock Monitor: who is blocked, who is blocking, and every open session.

use egui::RichText;

use super::*;
use crate::app::{App, LockView};
use crate::db::collector::Command;
use crate::deadlock::{Lock, Party};
use crate::fmt_sql;

impl App {
    pub fn lock_monitor_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Refresh").clicked() {
                self.collector.send(Command::RefreshHeavy);
            }
            ui.separator();
            for v in LockView::ORDER {
                let label = match v {
                    LockView::Blocking => format!("Blocking ({})", self.lock_waits.len()),
                    LockView::Transactions => format!("Transactions ({})", self.transactions.len()),
                    LockView::Metadata => format!("Metadata locks ({})", self.metadata_locks.len()),
                    LockView::Deadlock => match &self.deadlock {
                        Some(_) => "Deadlock (1)".to_string(),
                        None => "Deadlock (0)".to_string(),
                    },
                    LockView::Sessions => format!(
                        "Sessions ({})",
                        self.latest
                            .as_ref()
                            .map(|s| s.processlist.len())
                            .unwrap_or(0)
                    ),
                };
                let mut text = RichText::new(label);
                if v == LockView::Blocking && !self.lock_waits.is_empty() {
                    text = text.color(RED).strong();
                }
                if v == LockView::Deadlock && self.deadlock.is_some() {
                    text = text.color(AMBER);
                }
                if ui.selectable_label(self.lock_view == v, text).clicked() {
                    self.lock_view = v;
                }
            }
            ui.separator();
            let src = if self.caps.ps_data_locks {
                "performance_schema.data_lock_waits (MySQL 8)"
            } else {
                "information_schema.innodb_lock_waits (MySQL 5)"
            };
            ui.label(RichText::new(src).small().weak());
        });
        ui.separator();

        match self.lock_view {
            LockView::Blocking => self.blocking_view(ui),
            LockView::Transactions => self.transactions_view(ui),
            LockView::Metadata => self.metadata_view(ui),
            LockView::Sessions => self.sessions_view(ui),
            LockView::Deadlock => self.deadlock_view(ui),
        }
    }

    /// The latest deadlock InnoDB remembers. There is at most one, it does not
    /// survive a restart, and it says nothing about how often deadlocks happen
    /// -- `SHOW GLOBAL STATUS LIKE 'Innodb_deadlocks'` is not exposed by MySQL,
    /// so frequency has to come from the error log.
    fn deadlock_view(&mut self, ui: &mut egui::Ui) {
        let Some(d) = self.deadlock.clone() else {
            ui.colored_label(GREEN, "No deadlock since this server started.");
            ui.label(
                RichText::new("InnoDB keeps only the most recent one, and clears it on restart.")
                    .small()
                    .weak(),
            );
            return;
        };

        egui::ScrollArea::vertical()
            .id_salt("deadlock_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(AMBER, RichText::new("Latest detected deadlock").strong());
                    ui.separator();
                    ui.label(RichText::new(&d.detected_at).small().weak());
                    ui.separator();
                    let tables = d.tables();
                    if !tables.is_empty() {
                        ui.label(format!("tables: {}", tables.join(", ")));
                    }
                    if ui.small_button("Copy report").clicked() {
                        ui.ctx().copy_text(d.raw.clone());
                    }
                });
                ui.add_space(6.0);

                for party in &d.parties {
                    let victim = d.victim == Some(party.index);
                    deadlock_party(ui, party, victim);
                    ui.add_space(6.0);
                }

                match d.victim {
                    Some(n) => ui.label(
                        RichText::new(format!(
                            "InnoDB rolled back transaction ({n}). The other one \
                             committed -- the application only saw an error on the \
                             rolled-back side."
                        ))
                        .small()
                        .weak(),
                    ),
                    None => ui.label(
                        RichText::new("The report did not name a victim.")
                            .small()
                            .weak(),
                    ),
                };
            });
    }

    fn blocking_view(&mut self, ui: &mut egui::Ui) {
        if self.lock_waits.is_empty() {
            ui.colored_label(GREEN, "No lock waits.");
            return;
        }

        let mut kill: Option<u64> = None;
        let waits = self.lock_waits.clone();

        egui::ScrollArea::vertical()
            .id_salt("lock_waits")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for w in &waits {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        // Header: what is stuck, on what, for how long.
                        ui.horizontal_wrapped(|ui| {
                            ui.colored_label(RED, RichText::new("⚠ Blocking transaction").strong());
                            if !w.table.is_empty() {
                                ui.label(RichText::new(&w.table).monospace());
                            }
                            if !w.lock_mode.is_empty() {
                                ui.label(
                                    RichText::new(format!("{} {}", w.lock_type, w.lock_mode))
                                        .small()
                                        .weak(),
                                );
                            }
                            if !w.index.is_empty() {
                                ui.label(
                                    RichText::new(format!("index {}", w.index)).small().weak(),
                                );
                            }
                        });
                        ui.add_space(2.0);

                        // Facts table: PID / user / database / duration, one
                        // column per side so they line up.
                        egui::Grid::new(format!("lockfacts_{}", w.waiting.trx_id))
                            .num_columns(3)
                            .spacing([18.0, 2.0])
                            .show(ui, |ui| {
                                ui.label("");
                                ui.label(RichText::new("Waiting").strong().color(RED));
                                ui.label(RichText::new("Blocking").strong().color(AMBER));
                                ui.end_row();

                                ui.label(RichText::new("PID").weak());
                                ui.label(w.waiting.thread_id.to_string());
                                ui.label(w.blocking.thread_id.to_string());
                                ui.end_row();

                                ui.label(RichText::new("User").weak());
                                ui.label(w.waiting.who());
                                ui.label(w.blocking.who());
                                ui.end_row();

                                ui.label(RichText::new("Database").weak());
                                ui.label(non_empty(&w.waiting.db));
                                ui.label(non_empty(&w.blocking.db));
                                ui.end_row();

                                ui.label(RichText::new("Duration").weak());
                                ui.label(
                                    RichText::new(fmt_duration(w.waiting.secs.max(0) as u64))
                                        .color(if w.waiting.secs >= 10 { RED } else { AMBER }),
                                )
                                .on_hover_text("how long this statement has been blocked");
                                ui.label(fmt_duration(w.blocking.secs.max(0) as u64))
                                    .on_hover_text(
                                        "how long the blocking transaction has been open",
                                    );
                                ui.end_row();

                                ui.label(RichText::new("Trx").weak());
                                ui.label(RichText::new(&w.waiting.trx_id).small());
                                ui.label(RichText::new(&w.blocking.trx_id).small());
                                ui.end_row();
                            });

                        ui.add_space(6.0);
                        ui.label(RichText::new("Blocking:").strong().color(AMBER));
                        query_block(
                            ui,
                            &w.blocking.query,
                            "(idle — transaction left open with no active statement)",
                        );

                        ui.add_space(4.0);
                        ui.label(RichText::new("Waiting:").strong().color(RED));
                        query_block(ui, &w.waiting.query, "(no statement reported)");

                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .button(RichText::new("Kill blocker").strong())
                                .on_hover_text(format!(
                                    "KILL {} — ends the blocking connection and rolls its \
                                     transaction back",
                                    w.blocking.thread_id
                                ))
                                .clicked()
                            {
                                kill = Some(w.blocking.thread_id);
                            }
                            if ui
                                .button("Kill waiter")
                                .on_hover_text(format!("KILL {}", w.waiting.thread_id))
                                .clicked()
                            {
                                kill = Some(w.waiting.thread_id);
                            }
                            if ui.small_button("Copy blocking SQL").clicked() {
                                ui.ctx().copy_text(w.blocking.query.clone());
                            }
                        });
                    });
                    ui.add_space(8.0);
                }
            });

        if let Some(id) = kill {
            self.collector.send(Command::KillThread(id));
            self.push_log(format!("KILL {id} sent"));
        }
    }

    fn transactions_view(&mut self, ui: &mut egui::Ui) {
        if self.transactions.is_empty() {
            ui.label("No open InnoDB transactions.");
            return;
        }
        let mut kill: Option<u64> = None;

        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("trx_grid")
                .striped(true)
                .num_columns(9)
                .show(ui, |ui| {
                    for h in [
                        "Trx",
                        "State",
                        "Started",
                        "Waiting",
                        "Thread",
                        "Rows locked",
                        "Rows modified",
                        "Isolation",
                        "Query",
                    ] {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();
                    for t in &self.transactions {
                        ui.label(&t.id);
                        let state = RichText::new(&t.state);
                        ui.label(if t.state.contains("LOCK WAIT") {
                            state.color(RED)
                        } else {
                            state
                        });
                        ui.label(&t.started);
                        ui.label(if t.wait_secs > 0 {
                            fmt_duration(t.wait_secs as u64)
                        } else {
                            "-".into()
                        });
                        ui.horizontal(|ui| {
                            ui.label(t.thread_id.to_string());
                            if t.thread_id > 0 && ui.small_button("kill").clicked() {
                                kill = Some(t.thread_id);
                            }
                        });
                        ui.label(fmt_count(t.rows_locked));
                        ui.label(fmt_count(t.rows_modified));
                        ui.label(&t.isolation);
                        ui.label(one_line(&t.query, 90)).on_hover_text(&t.query);
                        ui.end_row();
                    }
                });
        });

        if let Some(id) = kill {
            self.collector.send(Command::KillThread(id));
            self.push_log(format!("KILL {id} sent"));
        }
    }

    fn metadata_view(&mut self, ui: &mut egui::Ui) {
        if !self.caps.metadata_locks {
            ui.label("performance_schema.metadata_locks needs MySQL 5.7.3+.");
            return;
        }
        if !self.caps.perf_schema_on {
            perf_schema_warning(ui, "metadata locks");
            return;
        }
        if self.metadata_locks.is_empty() {
            ui.label(
                RichText::new(
                    "No metadata locks reported. If DDL appears stuck, enable the instrument:\n\
                     UPDATE performance_schema.setup_instruments \
                     SET ENABLED='YES' WHERE NAME='wait/lock/metadata/sql/mdl';",
                )
                .small()
                .weak(),
            );
            return;
        }

        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("mdl_grid")
                .striped(true)
                .num_columns(6)
                .show(ui, |ui| {
                    for h in ["Object", "Schema", "Name", "Lock type", "Status", "Thread"] {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();
                    for m in &self.metadata_locks {
                        ui.label(&m.object_type);
                        ui.label(&m.schema);
                        ui.label(&m.name);
                        ui.label(&m.lock_type);
                        let status = RichText::new(&m.lock_status);
                        ui.label(if m.lock_status.eq_ignore_ascii_case("PENDING") {
                            status.color(RED)
                        } else {
                            status
                        });
                        ui.label(m.thread_id.to_string());
                        ui.end_row();
                    }
                });
        });
    }

    fn sessions_view(&mut self, ui: &mut egui::Ui) {
        let mut kill: Option<u64> = None;
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.add(egui::TextEdit::singleline(&mut self.proc_filter).desired_width(200.0));
            ui.checkbox(&mut self.hide_sleep, "hide Sleep");
        });

        let needle = self.proc_filter.to_ascii_lowercase();
        let hide_sleep = self.hide_sleep;

        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("procs_grid")
                .striped(true)
                .num_columns(9)
                .show(ui, |ui| {
                    for h in [
                        "Id", "User", "Host", "DB", "Command", "Time", "State", "Info", "",
                    ] {
                        ui.label(RichText::new(h).strong());
                    }
                    ui.end_row();

                    let rows = self
                        .latest
                        .as_ref()
                        .map(|s| &s.processlist[..])
                        .unwrap_or(&[]);
                    for p in rows {
                        if hide_sleep && p.command.eq_ignore_ascii_case("sleep") {
                            continue;
                        }
                        if !needle.is_empty() {
                            let hay = format!("{} {} {} {}", p.user, p.db, p.state, p.info)
                                .to_ascii_lowercase();
                            if !hay.contains(&needle) {
                                continue;
                            }
                        }
                        ui.label(p.id.to_string());
                        ui.label(&p.user);
                        ui.label(&p.host);
                        ui.label(&p.db);
                        ui.label(&p.command);
                        let t = RichText::new(p.time.to_string());
                        ui.label(if p.time > 10 { t.color(AMBER) } else { t });
                        ui.label(&p.state);
                        ui.label(one_line(&p.info, 110)).on_hover_text(&p.info);
                        if ui.small_button("kill").clicked() {
                            kill = Some(p.id);
                        }
                        ui.end_row();
                    }
                });
        });

        if let Some(id) = kill {
            self.collector.send(Command::KillThread(id));
            self.push_log(format!("KILL {id} sent"));
        }
    }
}

/// A dash instead of a blank cell, so the layout does not look broken.
fn non_empty(s: &str) -> String {
    if s.is_empty() {
        "-".to_string()
    } else {
        s.to_string()
    }
}

/// Statement text in a selectable monospace box, or a note when there is none.
fn query_block(ui: &mut egui::Ui, sql: &str, empty_note: &str) {
    if sql.trim().is_empty() {
        ui.label(RichText::new(empty_note).italics().weak());
        return;
    }
    let mut text = sql.to_string();
    ui.add(
        egui::TextEdit::multiline(&mut text)
            .code_editor()
            .desired_width(f32::INFINITY)
            .desired_rows(2)
            .interactive(false),
    );
}
/// One side of a deadlock as a card: who it was, what it ran, what it held and
/// what it wanted.
fn deadlock_party(ui: &mut egui::Ui, p: &Party, victim: bool) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            let title = format!("({}) transaction {}", p.index, p.trx_id);
            if victim {
                ui.colored_label(RED, RichText::new(title).strong());
                ui.colored_label(RED, "rolled back");
            } else {
                ui.label(RichText::new(title).strong());
                ui.colored_label(GREEN, "survived");
            }
        });
        ui.horizontal_wrapped(|ui| {
            stat(ui, "Thread", p.thread_id.to_string());
            stat(
                ui,
                "User",
                if p.user.is_empty() { "-" } else { &p.user }.to_string(),
            );
            stat(
                ui,
                "Host",
                if p.host.is_empty() { "-" } else { &p.host }.to_string(),
            );
            stat(ui, "Active", format!("{}s", p.active_secs));
            stat(ui, "Row locks", p.row_locks.to_string());
            if !p.state.is_empty() {
                stat(ui, "State", p.state.clone());
            }
        });
        if !p.activity.is_empty() {
            ui.label(RichText::new(&p.activity).small().weak());
        }

        if p.query.is_empty() {
            ui.label(RichText::new("(no statement in the report)").small().weak());
        } else {
            let mut text = fmt_sql::format(&p.query);
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(2)
                    .interactive(false),
            );
            if ui.small_button("Copy statement").clicked() {
                ui.ctx().copy_text(p.query.clone());
            }
        }

        if let Some(w) = &p.waiting {
            ui.colored_label(RED, format!("waiting for: {}", lock_summary(w)));
        }
        for h in &p.holds {
            ui.colored_label(AMBER, format!("holds: {}", lock_summary(h)));
        }
    });
}

fn lock_summary(l: &Lock) -> String {
    let mut out = String::new();
    if l.record {
        out.push_str("record lock");
    } else {
        out.push_str("table lock");
    }
    if !l.table.is_empty() {
        out.push_str(&format!(" on {}", l.table));
    }
    if !l.index.is_empty() {
        out.push_str(&format!(" ({})", l.index));
    }
    if !l.mode.is_empty() {
        out.push_str(&format!(" mode {}", l.mode));
    }
    out
}
