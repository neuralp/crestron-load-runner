// Release builds run without a console so launching the .exe on Windows does not
// open a terminal. Debug builds keep it for panic output and logging.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod device_log;
mod discovery;
mod firmware;
mod model;
mod ssh;
mod storage;

#[cfg(test)]
mod test_support;

use app::LoadRunnerApp;

fn main() -> eframe::Result {
    match cli::parse(std::env::args().skip(1)) {
        Ok(cli::Outcome::Help) => {
            report(cli::USAGE);
            return Ok(());
        }
        Ok(cli::Outcome::Run(options)) => {
            if let Some(dir) = options.config_dir
                && let Err(error) = use_config_dir(dir)
            {
                report(&format!("{error}\n\n{}", cli::USAGE));
                std::process::exit(2);
            }
        }
        Err(error) => {
            report(&format!("{error}\n\n{}", cli::USAGE));
            std::process::exit(2);
        }
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 820.0])
            .with_min_inner_size([1000.0, 620.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        "Crestron Load Runner",
        options,
        Box::new(|cc| Ok(Box::new(LoadRunnerApp::new(cc)))),
    );

    // Without a console there is nowhere for a startup failure to be printed.
    if let Err(error) = &result {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Crestron Load Runner")
            .set_description(format!("The application could not start.\n\n{error}"))
            .show();
    }
    result
}

/// Rejects a path that cannot hold the configuration before the address book is
/// read, so the mistake is reported instead of surfacing as a failed save.
fn use_config_dir(dir: std::path::PathBuf) -> Result<(), String> {
    if dir.exists() && !dir.is_dir() {
        return Err(format!(
            "--config-dir is not a directory: {}",
            dir.display()
        ));
    }
    if let Err(dir) = storage::set_config_dir(dir) {
        return Err(format!(
            "The configuration directory was already set: {}",
            dir.display()
        ));
    }
    Ok(())
}

/// Windows release builds have no console, so usage output goes to a dialog.
fn report(message: &str) {
    if cfg!(all(not(debug_assertions), windows)) {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Info)
            .set_title("Crestron Load Runner")
            .set_description(message)
            .show();
    } else {
        eprintln!("{message}");
    }
}
