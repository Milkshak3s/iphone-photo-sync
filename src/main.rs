#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod config;
mod icon;
mod instance;
mod library;
mod logger;
mod shell;
mod transfer;
mod tray;
mod update;
mod walk;
mod worker;
mod wpd;

use std::time::Duration;

use eframe::egui;

use crate::app::{App, TITLE};
use crate::config::Config;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    logger::init();
    if has("--quit") {
        instance::request_quit();
        return Ok(());
    }
    if has("--diagnose") {
        if let Err(e) = shell::diagnose() {
            println!("diagnose failed: {e:#}");
            log::error!("diagnose: {e:#}");
        }
        return Ok(());
    }
    // After a self-update the old copy may still be shutting down.
    let wait = if has("--after-update") {
        Duration::from_secs(15)
    } else {
        Duration::ZERO
    };
    if !instance::acquire(wait) {
        instance::show_existing(TITLE);
        return Ok(());
    }
    log::info!("starting v{}", env!("CARGO_PKG_VERSION"));

    let config = Config::load();
    if config.dest.is_some() && config.start_on_login {
        // Keeps the Run entry pointing at this exe if it was moved.
        let _ = autostart::set(true);
    }

    // Start hidden in the tray when launched at sign-in, once set up.
    let visible = !(has("--minimized") && config.dest.is_some());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([520.0, 600.0])
            .with_min_inner_size([420.0, 420.0])
            .with_visible(visible)
            .with_icon(egui::IconData {
                rgba: icon::rgba(),
                width: icon::SIZE,
                height: icon::SIZE,
            }),
        ..Default::default()
    };
    eframe::run_native(
        TITLE,
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc, config)?))),
    )
}
