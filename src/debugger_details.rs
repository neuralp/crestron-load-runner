use super::*;

#[cfg(test)]
#[path = "debugger_details_tests.rs"]
mod tests;

impl Panel {
    pub(super) fn signal_history(&self, signum: u32) -> Vec<&Sample> {
        self.samples
            .iter()
            .chain(self.pending_samples.iter())
            .rev()
            .filter(|sample| sample.signum == signum)
            .collect()
    }

    pub(super) fn show_signal_details(&mut self, ctx: &egui::Context) {
        let Some(signum) = self.detail_signal else {
            return;
        };
        let mode = self.display_types.get(&signum).copied().unwrap_or_default();
        let history = self.signal_history(signum);
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new(("signal_details", &self.title, signum))).show(ctx, |ui| {
            ui.set_width(640.0_f32.min((ctx.content_rect().width() - 48.0).max(240.0)));
            ui.horizontal(|ui| {
                ui.heading("Signal details");
                close = ui.button("Close").clicked();
            });
            ui.label(&self.title);
            ui.separator();
            egui::ScrollArea::vertical().id_salt("signal_metadata").max_height(180.0).show(ui, |ui| {
                egui::Grid::new("signal_detail_fields").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                    ui.strong("Signal"); ui.label(self.signal_name(signum)); ui.end_row();
                    ui.strong("Signal ID"); ui.monospace(format!("0x{signum:08X} ({signum})")); ui.end_row();
                    ui.strong("Local debugging");
                    ui.label(if self.disabled_signals.contains(&signum) { "Disabled" } else if self.persistence.pending_disabled.contains(&signum) { "Disable pending initial synchronization" } else { "Enabled" }); ui.end_row();
                    ui.strong("Display format"); ui.label(match mode { ValueDisplay::Ascii => "ASCII", ValueDisplay::Hex => "Hex", ValueDisplay::Mixed => "Mixed" }); ui.end_row();
                    ui.strong("Current value");
                    ui.monospace(self.values.get(&signum).map(|value| {
                        if value.is_empty() { "\"\"".into() } else { display_value(value, mode) }
                    }).unwrap_or_else(|| "Not received".into())); ui.end_row();
                    if let Some(value) = self.values.get(&signum) {
                        ui.strong("Raw value"); ui.monospace(value); ui.end_row();
                    }
                    if let Some(sample) = history.first() {
                        ui.strong("Last recorded"); ui.label(format_received(sample.at)); ui.end_row();
                    }
                    ui.strong("Watch groups");
                    let groups: Vec<_> = self.groups.iter().filter(|(_, members)| members.contains(&signum)).map(|(group, _)| group.as_str()).collect();
                    ui.label(if groups.is_empty() { "None".into() } else { groups.join(", ") }); ui.end_row();
                });
                ui.separator();
                ui.strong("Signature records / aliases");
                for signal in self.signals.iter().filter(|signal| signal.signum() == signum) {
                    ui.label(&signal.name);
                    ui.monospace(signal.metadata.iter().map(|byte| format!("{byte:02X}")).collect::<Vec<_>>().join(" "));
                }
                ui.weak("Metadata is shown as raw signature bytes; unknown fields are not interpreted.");
            });
            ui.separator();
            ui.strong(format!("Recorded history · {} updates", history.len()));
            ui.weak("Newest first · Local receipt time · Includes buffered updates while paused");
            ui.weak("Only retained timeline records are available; cleared or evicted records are not recoverable.");
            ui.horizontal(|ui| { ui.label("Received (local)"); ui.label("Value received"); });
            if history.is_empty() { ui.label("No recorded changes for this signal"); }
            egui::ScrollArea::vertical().id_salt(("signal_detail_history", signum))
                .max_height((ctx.content_rect().height() * 0.4).clamp(80.0, 400.0))
                .show_rows(ui, 24.0, history.len(), |ui, rows| {
                    for row in rows {
                        let sample = history[row];
                        let value = if sample.value.is_empty() { "\"\"".into() } else { display_value(&sample.value, mode) };
                        ui.horizontal(|ui| {
                            ui.add_sized([230.0, 24.0], egui::Label::new(format_received(sample.at)).halign(egui::Align::Min));
                            ui.add(egui::Label::new(egui::RichText::new(&value).monospace()).truncate()).on_hover_text(format!("{value}\nRaw: {}", sample.value));
                        });
                    }
                });
        });
        if close || response.should_close() {
            self.detail_signal = None;
        }
    }
}
