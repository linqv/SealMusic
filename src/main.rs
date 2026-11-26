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

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn trim_allocator() {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> std::ffi::c_int;
    }
    // SAFETY: `malloc_trim(0)` is a thread-safe glibc extension that returns
    // free top-of-heap pages to the OS without affecting live allocations.
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn trim_allocator() {}

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
