#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app;
mod config;
mod integrations;
mod library;
mod logger;
#[cfg(target_os = "linux")]
mod online;
mod player;
mod slint_types;

const APP_ID: &str = "sealmusic";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--lx-worker") {
        return lx_probe::run_worker().map_err(Into::into);
    }
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--check-runtime") {
        return Ok(());
    }
    logger::init_default_logger();
    let instance = single_instance::SingleInstance::new(APP_ID)?;
    if instance.is_single() {
        app::run()?;
    } else {
        log::info!("SealMusic is already running");
    }
    Ok(())
}
