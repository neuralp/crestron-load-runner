use eframe::egui;

/// Shows an editor in its own operating-system window, returning whether it is
/// still open.
///
/// Backends that cannot open a second window — and every headless test context
/// — draw the same contents in a window inside the main one instead.
pub fn window(
    ctx: &egui::Context,
    title: &str,
    size: [f32; 2],
    mut contents: impl FnMut(&mut egui::Ui),
) -> bool {
    let mut open = true;
    if ctx.embed_viewports() {
        egui::Window::new(title)
            .open(&mut open)
            .default_size(size)
            .collapsible(false)
            .show(ctx, |ui| {
                // Panels divide up a definite rectangle, which an auto-sizing
                // window does not give them.
                ui.allocate_ui(egui::vec2(size[0], size[1]), |ui| contents(ui));
            });
        return open;
    }
    ctx.show_viewport_immediate(
        egui::ViewportId::from_hash_of(title),
        egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size(size),
        |ui, _class| {
            contents(ui);
            open = !ui.ctx().input(|input| input.viewport().close_requested());
        },
    );
    open
}
