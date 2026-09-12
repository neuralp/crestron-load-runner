// Release builds run without a console so launching the .exe on Windows does not
// open a terminal. Debug builds keep it for panic output and logging.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod archive;
mod backdrop;
mod cli;
mod cresnet;
mod device_log;
mod discovery;
mod firmware;
mod firmware_version;
mod ip_table;
/// The application mark, shared with `build.rs` by direct inclusion.
mod logo;
mod model;
mod popout;
mod puf;
mod resources;
mod scripts;
mod ssh;
mod storage;
mod terminal;
mod vc4;

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
            .with_min_inner_size([1000.0, 620.0])
            .with_icon(window_icon()),
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

/// The mark for the title bar and the task switcher. The executable carries its
/// own copy for the file on disk; this one is what the running window shows.
fn window_icon() -> eframe::egui::IconData {
    const SIZE: usize = 256;
    eframe::egui::IconData {
        rgba: logo::rasterize(SIZE),
        width: SIZE as u32,
        height: SIZE as u32,
    }
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
