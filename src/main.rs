// Release builds run without a console so launching the .exe on Windows does not
// open a terminal. Debug builds keep it for panic output and logging.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod discovery;
mod firmware;
mod model;
mod ssh;
mod storage;

#[cfg(test)]
mod test_support;

use app::LoadRunnerApp;

fn main() -> eframe::Result {
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
