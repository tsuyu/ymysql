#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod advisor;
mod alerts;
mod app;
mod connections;
mod csv;
mod db;
mod deadlock;
mod fmt_sql;
mod html_table;
mod innodb;
mod insert_sql;
mod json_rows;
mod live_check;
mod markdown_table;
mod model;
mod profiles;
mod replication;
mod store;
mod suggest;
mod ui;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // Carries an install from before the mysql_perf -> yMySQL rename.
    store::migrate_legacy_dir();

    // The GUI runs on the main thread; all MySQL I/O runs on this runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let collector = db::collector::spawn(rt.handle());
    let store = store::spawn(store::default_db_path());

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("yMySQL"),
        ..Default::default()
    };

    eframe::run_native(
        "yMySQL",
        native_options,
        Box::new(|_cc| Ok(Box::new(app::App::new(collector, store)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;

    rt.shutdown_timeout(std::time::Duration::from_millis(500));
    Ok(())
}
