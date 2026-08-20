//! Tab bodies and shared widgets. Each submodule adds one `impl App` block.

pub mod alerts_tab;
pub mod connections;
pub mod dashboard;
pub mod dump;
pub mod historical;
pub mod index_advisor;
pub mod innodb;
pub mod inspector;
pub mod lock_monitor;
pub mod replication;
pub mod sql_console;
pub mod table_browser;
pub mod top_sql;

use egui::{Color32, RichText};
use egui_plot::{Legend, Line, Plot, PlotPoints};

use crate::advisor::Severity;
use crate::db::queries::Grid;
use crate::model::{History, Metric};

pub const GREEN: Color32 = Color32::from_rgb(90, 200, 120);
pub const AMBER: Color32 = Color32::from_rgb(230, 190, 80);
pub const RED: Color32 = Color32::from_rgb(220, 100, 100);
pub const BLUE: Color32 = Color32::from_rgb(110, 170, 240);

pub fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Info => BLUE,
        Severity::Warn => AMBER,
        Severity::High => RED,
    }
}

/// A labelled number card.
pub fn stat(ui: &mut egui::Ui, label: &str, value: String) {
    stat_colored(ui, label, value, None);
}

/// A stat card with an explanation on hover, for numbers whose meaning is not
/// obvious from the label.
pub fn stat_help(ui: &mut egui::Ui, label: &str, value: String, help: &str) {
    egui::Frame::group(ui.style())
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(format!("{label} ⓘ")).small().weak());
                ui.label(RichText::new(value).heading());
            });
        })
        .response
        .on_hover_text(help);
}

pub fn stat_colored(ui: &mut egui::Ui, label: &str, value: String, color: Option<Color32>) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(label).small().weak());
            let mut text = RichText::new(value).heading();
            if let Some(c) = color {
                text = text.color(c);
            }
            ui.label(text);
        });
    });
}

/// Plot of live (monotonic-seconds) series straight from `History`.
pub fn live_plot(ui: &mut egui::Ui, title: &str, history: &History, metrics: &[Metric]) {
    ui.label(RichText::new(title).strong());
    Plot::new(title)
        .height(150.0)
        .legend(Legend::default())
        .allow_scroll(false)
        .show(ui, |p| {
            for m in metrics {
                p.line(Line::new(m.label(), PlotPoints::from(history.points(*m))));
            }
        });
    ui.add_space(6.0);
}

/// Plot of stored series, x axis in unix seconds rendered as local clock time.
pub fn time_plot(ui: &mut egui::Ui, id: &str, series: &[(Metric, Vec<[f64; 2]>)], height: f32) {
    Plot::new(id)
        .height(height)
        .legend(Legend::default())
        .allow_scroll(false)
        .x_axis_formatter(|mark, _range| format_clock(mark.value))
        .label_formatter(|hover| {
            let (name, at) = match hover {
                egui_plot::HoverPosition::NearDataPoint {
                    plot_name,
                    position,
                    ..
                } => (*plot_name, *position),
                egui_plot::HoverPosition::Elsewhere { position } => ("", *position),
            };
            Some(format!("{name}  {}  {:.2}", format_clock(at.x), at.y))
        })
        .show(ui, |p| {
            for (metric, points) in series {
                p.line(Line::new(metric.label(), PlotPoints::from(points.clone())));
            }
        });
}

fn format_clock(unix_secs: f64) -> String {
    use chrono::{Local, TimeZone as _};
    match Local.timestamp_opt(unix_secs as i64, 0).single() {
        Some(dt) => dt.format("%H:%M:%S").to_string(),
        None => String::new(),
    }
}

pub fn format_datetime(unix_ms: i64) -> String {
    use chrono::{Local, TimeZone as _};
    match Local.timestamp_millis_opt(unix_ms).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => String::new(),
    }
}

/// Renders a generic result grid (EXPLAIN output and friends).
pub fn grid_table(ui: &mut egui::Ui, id: &str, grid: &Grid) {
    data_grid(ui, id, grid, None, false);
}

/// What the user did to a [`data_grid`] this frame.
#[derive(Debug, Default, Clone, Copy)]
pub struct GridResponse {
    /// Index of the column header that was clicked.
    pub header_clicked: Option<usize>,
    /// `(row, column)` of the clicked cell, when the grid is clickable.
    pub cell_clicked: Option<(usize, usize)>,
}

/// A result grid with clickable headers (sorting) and optionally clickable
/// cells (editing). `sort` is `(column, descending)` and only draws the arrow —
/// the caller decides whether sorting happens here or on the server.
pub fn data_grid(
    ui: &mut egui::Ui,
    id: &str,
    grid: &Grid,
    sort: Option<(usize, bool)>,
    clickable_cells: bool,
) -> GridResponse {
    let mut resp = GridResponse::default();
    if grid.columns.is_empty() {
        ui.label(RichText::new("(no columns)").weak());
        return resp;
    }

    // Fill the space the caller gave us instead of shrinking to content, so
    // the scrollbars sit at the edges of the area rather than mid-panel.
    egui::ScrollArea::both()
        .id_salt(id)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new(format!("{id}_grid"))
                .striped(true)
                .num_columns(grid.columns.len())
                .show(ui, |ui| {
                    for (i, c) in grid.columns.iter().enumerate() {
                        let arrow = match sort {
                            Some((col, desc)) if col == i => {
                                if desc {
                                    " ▼"
                                } else {
                                    " ▲"
                                }
                            }
                            _ => "",
                        };
                        let label = RichText::new(format!("{c}{arrow}")).strong();
                        if ui
                            .add(egui::Label::new(label).sense(egui::Sense::click()))
                            .on_hover_text("sort by this column")
                            .clicked()
                        {
                            resp.header_clicked = Some(i);
                        }
                    }
                    ui.end_row();

                    for (r, row) in grid.rows.iter().enumerate() {
                        for (c, cell) in row.iter().enumerate() {
                            let text = match cell {
                                Some(v) => RichText::new(one_line(v, 80)),
                                None => RichText::new("NULL").italics().weak(),
                            };
                            let widget = egui::Label::new(text).sense(if clickable_cells {
                                egui::Sense::click()
                            } else {
                                egui::Sense::hover()
                            });
                            let mut handle = ui.add(widget);
                            if let Some(v) = cell
                                && v.chars().count() > 80
                            {
                                handle = handle.on_hover_text(v);
                            }
                            if clickable_cells {
                                handle = handle.on_hover_text("click to edit");
                                if handle.clicked() {
                                    resp.cell_clicked = Some((r, c));
                                }
                            }
                        }
                        ui.end_row();
                    }
                });
        });

    resp
}

/// Sorted row order for client-side sorting, numeric when both cells parse.
pub fn sorted_order(grid: &Grid, sort: Option<(usize, bool)>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..grid.rows.len()).collect();
    let Some((col, desc)) = sort else {
        return order;
    };
    order.sort_by(|&a, &b| {
        let x = grid.rows[a].get(col).and_then(|c| c.as_deref());
        let y = grid.rows[b].get(col).and_then(|c| c.as_deref());
        let ord = match (x, y) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => match (a.parse::<f64>(), b.parse::<f64>()) {
                (Ok(a), Ok(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
                _ => a.cmp(b),
            },
        };
        if desc { ord.reverse() } else { ord }
    });
    order
}

/// Reorders a grid's rows without touching the original.
pub fn reordered(grid: &Grid, order: &[usize]) -> Grid {
    Grid {
        columns: grid.columns.clone(),
        rows: order.iter().map(|&i| grid.rows[i].clone()).collect(),
        origins: grid.origins.clone(),
    }
}

/// Collapses whitespace and clips, keeping the full text as a hover.
pub fn one_line(s: &str, max: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= max {
        joined
    } else {
        joined.chars().take(max).collect::<String>() + "…"
    }
}

pub fn fmt_bytes(b: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

pub fn fmt_count(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Milliseconds, scaled to whatever unit keeps it readable.
pub fn fmt_ms(ms: f64) -> String {
    if ms >= 60_000.0 {
        format!("{:.1} min", ms / 60_000.0)
    } else if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 1.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{:.0} µs", ms * 1000.0)
    }
}

pub fn fmt_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{secs}s")
    }
}

/// Shown wherever a panel needs performance_schema and it is off.
pub fn perf_schema_warning(ui: &mut egui::Ui, what: &str) {
    ui.colored_label(
        AMBER,
        format!("performance_schema is OFF — {what} unavailable. Enable it in my.cnf and restart."),
    );
}

pub fn not_connected(ui: &mut egui::Ui) {
    ui.add_space(20.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("Not connected").weak());
    });
}
