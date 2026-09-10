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

    eframe::run_native(
        "Crestron Load Runner",
        options,
        Box::new(|cc| Ok(Box::new(LoadRunnerApp::new(cc)))),
    )
}
