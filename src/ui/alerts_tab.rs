//! Alerts: threshold rules, their live state, and the transition log.

use egui::RichText;

use super::*;
use crate::alerts::{Comparison, State, Transition};
use crate::app::App;
use crate::model::Metric;

impl App {
    pub fn alerts_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let firing = self.alerts.firing_count();
            if firing == 0 {
                ui.colored_label(GREEN, "● all clear");
            } else {
                ui.colored_label(RED, RichText::new(format!("● {firing} firing")).strong());
            }
            ui.separator();
            ui.label(
                RichText::new("A rule fires only after its condition holds for the whole window.")
                    .small()
                    .weak(),
            );
        });
        ui.separator();

        self.rules_section(ui);
        ui.add_space(10.0);
        self.new_rule_section(ui);
        ui.add_space(10.0);
        self.alert_log_section(ui);
    }

    fn rules_section(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Rules").strong());

        let derived = self.derived;
        let mut remove: Option<u32> = None;

        egui::Grid::new("alert_rules")
            .striped(true)
            .num_columns(7)
            .show(ui, |ui| {
                for h in ["On", "Metric", "", "Threshold", "For", "Now", "State"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.label("");
                ui.end_row();

                let states: Vec<(u32, State)> = self
                    .alerts
                    .rules
                    .iter()
                    .map(|r| (r.id, self.alerts.state(r.id)))
                    .collect();

                for (i, rule) in self.alerts.rules.iter_mut().enumerate() {
                    ui.checkbox(&mut rule.enabled, "");
                    ui.label(rule.metric.label());
                    egui::ComboBox::from_id_salt(format!("cmp{i}"))
                        .width(40.0)
                        .selected_text(rule.cmp.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut rule.cmp, Comparison::Above, ">");
                            ui.selectable_value(&mut rule.cmp, Comparison::Below, "<");
                        });
                    ui.add(
                        egui::DragValue::new(&mut rule.threshold)
                            .speed(0.5)
                            .range(0.0..=1e9),
                    );
                    ui.add(
                        egui::DragValue::new(&mut rule.for_secs)
                            .speed(1.0)
                            .range(0.0..=3600.0)
                            .suffix(" s"),
                    );

                    let value = rule.metric.value(&derived);
                    ui.label(format!("{value:.2}"));

                    let state = states
                        .iter()
                        .find(|(id, _)| *id == rule.id)
                        .map(|(_, s)| *s)
                        .unwrap_or(State::Ok);
                    let (color, text) = match state {
                        State::Ok => (GREEN, "ok".to_string()),
                        State::Pending { .. } => (AMBER, "pending".to_string()),
                        State::Firing { .. } => (RED, "FIRING".to_string()),
                    };
                    ui.colored_label(color, RichText::new(text).strong());

                    if ui.small_button("remove").clicked() {
                        remove = Some(rule.id);
                    }
                    ui.end_row();
                }
            });

        if let Some(id) = remove {
            self.alerts.remove(id);
        }
        if self.alerts.rules.is_empty() {
            ui.label(RichText::new("No rules.").weak());
        }
    }

    fn new_rule_section(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Add rule").strong());
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("new_rule_metric")
                .width(180.0)
                .selected_text(self.new_rule_metric.label())
                .show_ui(ui, |ui| {
                    for m in Metric::ALL {
                        ui.selectable_value(&mut self.new_rule_metric, m, m.label());
                    }
                });
            egui::ComboBox::from_id_salt("new_rule_cmp")
                .width(50.0)
                .selected_text(self.new_rule_cmp.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.new_rule_cmp, Comparison::Above, ">");
                    ui.selectable_value(&mut self.new_rule_cmp, Comparison::Below, "<");
                });
            ui.add(
                egui::DragValue::new(&mut self.new_rule_threshold)
                    .speed(0.5)
                    .range(0.0..=1e9),
            );
            ui.label("for");
            ui.add(
                egui::DragValue::new(&mut self.new_rule_for)
                    .speed(1.0)
                    .range(0.0..=3600.0)
                    .suffix(" s"),
            );
            if ui.button("Add").clicked() {
                let (m, c, t, f) = (
                    self.new_rule_metric,
                    self.new_rule_cmp,
                    self.new_rule_threshold,
                    self.new_rule_for,
                );
                self.alerts.add(m, c, t, f);
                self.push_log(format!("added rule: {} {} {t}", m.label(), c.label()));
            }
        });
    }

    fn alert_log_section(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("History").strong());
        if self.alerts.log.is_empty() {
            ui.label(RichText::new("Nothing has fired yet.").weak());
            return;
        }

        egui::ScrollArea::vertical()
            .max_height(260.0)
            .id_salt("alert_log")
            .show(ui, |ui| {
                egui::Grid::new("alert_log_grid")
                    .striped(true)
                    .num_columns(4)
                    .show(ui, |ui| {
                        for h in ["When", "Transition", "Value", "Rule"] {
                            ui.label(RichText::new(h).strong());
                        }
                        ui.end_row();
                        for ev in self.alerts.log.iter().rev() {
                            ui.label(format_datetime(ev.at_ms));
                            match ev.transition {
                                Transition::Fired => {
                                    ui.colored_label(RED, RichText::new("FIRED").strong())
                                }
                                Transition::Resolved => ui.colored_label(GREEN, "resolved"),
                            };
                            ui.label(format!("{:.2}", ev.value));
                            ui.label(&ev.rule);
                            ui.end_row();
                        }
                    });
            });
    }
}
