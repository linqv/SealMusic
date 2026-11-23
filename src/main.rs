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

fn is_runtime_check_argument(mut args: impl Iterator<Item = std::ffi::OsString>) -> bool {
    args.next();
    args.next().is_some_and(|arg| arg == "--check-runtime")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--lx-worker") {
        return lx_probe::run_worker().map_err(Into::into);
    }
    if is_runtime_check_argument(std::env::args_os()) {
        return Ok(());
    }
    logger::init_default_logger(None::<std::path::PathBuf>);
    let instance = single_instance::SingleInstance::new(APP_ID)?;
    if instance.is_single() {
        app::run()?;
    } else {
        log::info!("SealMusic is already running");
    }
    Ok(())
}

#[cfg(test)]
mod runtime_check_argument_tests {
    use std::ffi::OsString;

    #[test]
    fn recognizes_the_runtime_check_argument() {
        let args = ["sealmusic", "--check-runtime"].into_iter().map(OsString::from);

        assert!(super::is_runtime_check_argument(args));
    }

    #[test]
    fn ignores_other_arguments() {
        let args = ["sealmusic", "--help"].into_iter().map(OsString::from);

        assert!(!super::is_runtime_check_argument(args));
    }
}
