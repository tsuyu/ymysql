//! Index Advisor: schema and workload findings with copy-pasteable fixes.

use egui::RichText;

use super::*;
use crate::advisor::{MIN_UPTIME_FOR_UNUSED_S, Severity};
use crate::app::App;
use crate::db::collector::Command;
use crate::suggest::Impact;

impl App {
    pub fn index_advisor_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let enabled = self.is_connected() && !self.busy;
            if ui
                .add_enabled(enabled, egui::Button::new("Run analysis"))
                .on_hover_text(
                    "Reads information_schema.STATISTICS and the performance_schema \
                     index counters — heavier than a normal poll",
                )
                .clicked()
            {
                self.collector.send(Command::RunAdvisor);
            }
            if self.busy {
                ui.spinner();
                ui.label("collecting…");
            }

            if self.advisor_ran {
                ui.separator();
                let high = self.count_severity(Severity::High);
                let warn = self.count_severity(Severity::Warn);
                let info = self.count_severity(Severity::Info);
                ui.colored_label(RED, format!("{high} high"));
                ui.colored_label(AMBER, format!("{warn} warn"));
                ui.colored_label(BLUE, format!("{info} info"));
            }
        });

        if self.uptime_s > 0 && self.uptime_s < MIN_UPTIME_FOR_UNUSED_S {
            ui.label(
                RichText::new(format!(
                    "server uptime is {} — unused-index detection is suppressed below {}",
                    fmt_duration(self.uptime_s),
                    fmt_duration(MIN_UPTIME_FOR_UNUSED_S)
                ))
                .small()
                .color(AMBER),
            );
        }
        if !self.caps.perf_schema_on {
            ui.label(
                RichText::new(
                    "performance_schema is OFF — index usage and scan counters are unavailable; \
                     only schema-level findings (duplicate indexes, missing primary keys) apply",
                )
                .small()
                .color(AMBER),
            );
        }

        if self.advisor_ran {
            self.suggestions_section(ui);
        }

        if !self.advisor_ran {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Run the analysis to see findings").weak());
            });
            return;
        }
        ui.separator();

        // Kind filter chips.
        let kinds: Vec<&'static str> = self.advisor_kinds.iter().copied().collect();
        ui.horizontal_wrapped(|ui| {
            if ui
                .selectable_label(self.advisor_filter.is_none(), "all")
                .clicked()
            {
                self.advisor_filter = None;
            }
            for k in kinds {
                if ui
                    .selectable_label(self.advisor_filter == Some(k), k)
                    .clicked()
                {
                    self.advisor_filter = Some(k);
                }
            }
        });
        ui.separator();

        let filter = self.advisor_filter;
        let mut copy: Option<String> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut shown = 0;
            for f in self.advisor.iter().filter(|f| match filter {
                Some(k) => f.kind == k,
                None => true,
            }) {
                shown += 1;
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(
                            severity_color(f.severity),
                            RichText::new(f.severity.label().to_uppercase()).strong(),
                        );
                        ui.label(RichText::new(f.kind).strong());
                        ui.label(RichText::new(&f.object).color(BLUE));
                    });
                    ui.label(&f.detail);
                    if let Some(action) = &f.action {
                        ui.add_space(2.0);
                        let mut sql = action.clone();
                        ui.add(
                            egui::TextEdit::multiline(&mut sql)
                                .code_editor()
                                .desired_width(f32::INFINITY)
                                .desired_rows(action.lines().count().min(4))
                                .interactive(false),
                        );
                        if ui.small_button("Copy SQL").clicked() {
                            copy = Some(action.clone());
                        }
                    }
                });
                ui.add_space(4.0);
            }
            if shown == 0 {
                ui.colored_label(GREEN, "Nothing to report.");
            }
        });

        if let Some(sql) = copy {
            ui.ctx().copy_text(sql);
            self.push_log("copied remediation SQL");
        }
    }

    /// Proposed composite indexes, worst first.
    fn suggestions_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Suggested indexes").heading());
            ui.label(
                RichText::new(format!("{}", self.suggestions.len()))
                    .small()
                    .weak(),
            );
        });
        ui.label(
            RichText::new(
                "Estimates, not promises: impact is ranked from how often each statement runs \n                 and how many rows it reads. Real improvement depends on data \n                 distribution, cardinality, optimiser statistics and the rest of the \n                 workload — verify with EXPLAIN on a copy before shipping.",
            )
            .small()
            .weak(),
        );
        ui.add_space(4.0);

        if self.suggestions.is_empty() {
            ui.colored_label(
                GREEN,
                "No missing composite indexes found in the statements collected so far.",
            );
            return;
        }

        let suggestions = self.suggestions.clone();
        let mut copy: Option<String> = None;

        for sug in &suggestions {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        impact_color(sug.impact),
                        RichText::new("⚠ Possible missing composite index").strong(),
                    );
                    ui.label(RichText::new(format!("{}.{}", sug.schema, sug.table)).monospace());
                    ui.separator();
                    ui.colored_label(
                        impact_color(sug.impact),
                        RichText::new(format!("Estimated impact: {}", sug.impact.label())).strong(),
                    )
                    .on_hover_text("ranked from executions, rows examined and total time");
                });

                ui.add_space(2.0);
                ui.label(RichText::new("Suggested:").strong());
                ui.label(RichText::new(sug.columns_display()).monospace().size(15.0));
                if let Some(existing) = &sug.extends {
                    ui.label(
                        RichText::new(format!(
                            "extends the existing index `{existing}` — replace it rather than                              adding an overlapping one"
                        ))
                        .small()
                        .color(AMBER),
                    );
                }

                ui.label(
                    RichText::new(format!(
                        "{} executions · {} total across {} statement(s)",
                        fmt_count(sug.executions),
                        fmt_ms(sug.total_ms),
                        sug.queries.len()
                    ))
                    .small()
                    .weak(),
                );

                ui.add_space(4.0);
                ui.label(RichText::new("Query:").strong());
                for q in &sug.queries {
                    ui.label(RichText::new(one_line(q, 150)).monospace().small())
                        .on_hover_text(q);
                }

                ui.add_space(4.0);
                let mut ddl = sug.ddl();
                ui.add(
                    egui::TextEdit::multiline(&mut ddl)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .desired_rows(sug.ddl().lines().count().min(5))
                        .interactive(false),
                );
                if ui.small_button("Copy SQL").clicked() {
                    copy = Some(sug.ddl());
                }
            });
            ui.add_space(6.0);
        }

        if let Some(sql) = copy {
            ui.ctx().copy_text(sql);
            self.push_log("copied index DDL");
        }
    }

    fn count_severity(&self, s: Severity) -> usize {
        self.advisor.iter().filter(|f| f.severity == s).count()
    }
}

fn impact_color(i: Impact) -> egui::Color32 {
    match i {
        Impact::High => RED,
        Impact::Medium => AMBER,
        Impact::Low => BLUE,
    }
}
