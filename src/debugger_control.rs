use super::*;
use crate::ssh::{SignalValue, SignalWrite};

#[cfg(test)]
#[path = "debugger_control_tests.rs"]
mod ui_tests;

pub(super) struct Editor {
    pub signum: Option<u32>,
    pub status: String,
    analog: String,
    signed: bool,
    serial: String,
    pulse_ms: u16,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            signum: None,
            status: String::new(),
            analog: "0".into(),
            signed: false,
            serial: String::new(),
            pulse_ms: 100,
        }
    }
}

fn analog_value(text: &str, signed: bool) -> Result<u16, String> {
    if signed {
        text.trim()
            .parse::<i16>()
            .map(|value| value as u16)
            .map_err(|_| "Enter a signed value from −32768 to 32767".into())
    } else {
        text.trim()
            .parse::<u16>()
            .map_err(|_| "Enter a value from 0 to 65535".into())
    }
}

impl Editor {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        kind: SignalKind,
        enabled: bool,
        held: bool,
        read_only: Option<&str>,
    ) -> Option<SignalValue> {
        ui.strong("Set signal");
        if let Some(reason) = read_only {
            ui.weak(reason);
            return None;
        }
        ui.weak(
            "Changes the running program. Current value and history show processor reports only.",
        );
        if !enabled {
            ui.weak("Controls require an active, synchronized session.");
        }
        let action = self.controls(ui, kind, enabled, held, false);
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        action
    }

    /// One signal's settings for the editor's state. `compact` fits them to
    /// the foot of the signal tree: notes become tooltips, serial text takes
    /// one line, and Enter sends analog and serial values.
    fn controls(
        &mut self,
        ui: &mut egui::Ui,
        kind: SignalKind,
        enabled: bool,
        held: bool,
        compact: bool,
    ) -> Option<SignalValue> {
        let mut action = None;
        let entered = |response: &egui::Response, ui: &egui::Ui| {
            response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
        };
        ui.add_enabled_ui(enabled, |ui| {
            match kind {
                SignalKind::Digital => {
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!held, |ui| {
                            if ui.button("High").clicked() { action = Some(SignalValue::Digital(true)); }
                            if ui.button("Low").clicked() { action = Some(SignalValue::Digital(false)); }
                            if ui.button("Pulse").clicked() { action = Some(SignalValue::Pulse(self.pulse_ms)); }
                            ui.add(egui::DragValue::new(&mut self.pulse_ms).range(1..=65535).suffix(" ms"));
                        });
                    });
                    let note = "Hold releases on mouse-up, focus loss, closing details, or Stop. Network loss can prevent release.";
                    ui.horizontal(|ui| {
                        let mut hold = ui.button(if held { "Holding — release mouse" } else { "Push and hold" });
                        if compact { hold = hold.on_hover_text(note); }
                        if hold.is_pointer_button_down_on() && !held
                            && ui.input(|i| i.focused && i.pointer.button_pressed(egui::PointerButton::Primary)) {
                            action = Some(SignalValue::Hold);
                        }
                        if ui.add_enabled(held, egui::Button::new("Release")).clicked() { action = Some(SignalValue::Release); }
                    });
                    if !compact { ui.weak(note); }
                }
                SignalKind::Analog => {
                    let range = if self.signed { "Range: −32768 to 32767 (16-bit two’s complement)" } else { "Range: 0 to 65535" };
                    ui.horizontal(|ui| {
                        let old_signed = self.signed;
                        if ui.checkbox(&mut self.signed, "Signed").changed()
                            && let Ok(bits) = analog_value(&self.analog, old_signed) {
                                self.analog = if self.signed { (bits as i16).to_string() } else { bits.to_string() };
                            }
                        let mut field = ui.add(egui::TextEdit::singleline(&mut self.analog).desired_width(if compact { 80.0 } else { 120.0 }));
                        if compact { field = field.on_hover_text(range); }
                        let parsed = analog_value(&self.analog, self.signed);
                        if ui.add_enabled(parsed.is_ok(), egui::Button::new(if compact { "Set" } else { "Set value" })).clicked()
                            || (compact && entered(&field, ui)) {
                            action = parsed.ok().map(SignalValue::Analog);
                        }
                    });
                    if !compact { ui.weak(range); }
                    if let Err(error) = analog_value(&self.analog, self.signed) { ui.colored_label(egui::Color32::LIGHT_RED, error); }
                }
                SignalKind::Serial => {
                    let note = "Literal text, sent as UTF-8 bytes; quotes and line breaks are escaped automatically. Empty strings are allowed.";
                    if compact {
                        // Right to left, so the field fills what the button leaves.
                        let row = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
                        ui.allocate_ui_with_layout(row, egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let send = ui.button("Send");
                            let field = ui.add(egui::TextEdit::singleline(&mut self.serial).desired_width(f32::INFINITY).hint_text("String to send"))
                                .on_hover_text(note);
                            if send.clicked() || entered(&field, ui) { action = Some(SignalValue::Serial(self.serial.clone())); }
                        });
                    } else {
                        ui.add(egui::TextEdit::multiline(&mut self.serial).desired_rows(2).desired_width(f32::INFINITY).hint_text("String to send"));
                        if ui.button("Send string").clicked() { action = Some(SignalValue::Serial(self.serial.clone())); }
                        ui.weak(note);
                    }
                }
                SignalKind::Unknown => { ui.label("This signal's type is not supported for writing."); }
            }
        });
        action
    }
}

impl Panel {
    /// Why `signum` cannot be set from the debugger, or None when it can.
    pub(super) fn read_only_reason(&self, signum: u32) -> Option<&'static str> {
        let records: Vec<_> = self
            .signals
            .iter()
            .filter(|s| s.signum() == signum)
            .collect();
        let first = records.first()?;
        if records.iter().any(|s| s.metadata[4] & 0x80 != 0) {
            Some("Constant signals cannot be set")
        } else if first.kind() == SignalKind::Unknown
            || records.iter().any(|s| s.kind() != first.kind())
        {
            Some("Signals of unknown or conflicting type cannot be set")
        } else if !records.iter().all(|s| s.settable()) {
            Some("Read-only: the program does not allow this digital signal to be set")
        } else {
            None
        }
    }

    /// The type to set `signum` as, or Unknown when it cannot be set.
    pub(super) fn writable_kind(&self, signum: u32) -> SignalKind {
        match self.signals.iter().find(|s| s.signum() == signum) {
            Some(signal) if self.read_only_reason(signum).is_none() => signal.kind(),
            _ => SignalKind::Unknown,
        }
    }

    pub(super) fn set_signal(&mut self, signum: u32, value: SignalValue) {
        let valid = matches!(
            (self.writable_kind(signum), &value),
            (
                SignalKind::Digital,
                SignalValue::Digital(_)
                    | SignalValue::Pulse(_)
                    | SignalValue::Hold
                    | SignalValue::Release
            ) | (SignalKind::Analog, SignalValue::Analog(_))
                | (SignalKind::Serial, SignalValue::Serial(_))
        );
        let write = SignalWrite { signum, value };
        let result = if !self.running || !self.write_ready || self.stopping || !valid {
            Err("Signal is not writable in this session".into())
        } else {
            write.command(self.slot).and_then(|_| {
                self.input
                    .as_ref()
                    .ok_or_else(|| "Debugger is disconnected".to_owned())?
                    .send(DebugCommand::Write(write.clone()))
                    .map_err(|_| "Debugger is disconnected".to_owned())
            })
        };
        if result.is_ok() {
            if matches!(write.value, SignalValue::Hold) {
                self.held_signal = Some(signum);
            }
            if matches!(write.value, SignalValue::Release) {
                self.held_signal = None;
            }
        }
        self.report(
            signum,
            result.map_or_else(|error| error, |_| "Command queued…".into()),
        );
    }

    /// Show `status` in whichever editors, details or tree, are on `signum`.
    pub(super) fn report(&mut self, signum: u32, status: String) {
        for editor in [&mut self.editor, &mut self.tree_editor] {
            if editor.signum == Some(signum) {
                editor.status.clone_from(&status);
            }
        }
    }

    /// Show `status` in every editor on a signal.
    pub(super) fn report_all(&mut self, status: &str) {
        for editor in [&mut self.editor, &mut self.tree_editor] {
            if editor.signum.is_some() {
                editor.status = status.into();
            }
        }
    }

    pub(super) fn release_held(&mut self) {
        if let Some(signum) = self.held_signal.take() {
            let sent = self.input.as_ref().is_some_and(|input| {
                input
                    .send(DebugCommand::Write(SignalWrite {
                        signum,
                        value: SignalValue::Release,
                    }))
                    .is_ok()
            });
            self.report(
                signum,
                if sent {
                    "Release queued…"
                } else {
                    "Disconnected: held signal release could not be confirmed"
                }
                .into(),
            );
        }
    }

    /// Release a hold once the mouse is up, the window loses focus, or
    /// neither the details nor the tree is showing the held signal.
    pub(super) fn release_stale_hold(&mut self, ctx: &egui::Context) {
        if self.held_signal.is_some()
            && ((self.held_signal != self.detail_signal
                && self.held_signal != self.tree_control_signal())
                || !ctx.input(|i| i.focused && i.pointer.primary_down() && i.pointer.has_pointer()))
        {
            self.release_held();
        }
    }

    /// Whether this session can write signals of a writable type.
    pub(super) fn can_write(&self) -> bool {
        self.running && self.write_ready && !self.stopping
    }

    /// The signal the tree's controls set: the only one selected.
    pub(super) fn tree_control_signal(&self) -> Option<u32> {
        let mut selected = self.tree_selection.iter().copied();
        selected.next().filter(|_| selected.next().is_none())
    }

    /// Controls at the foot of the signal tree for its one selected signal,
    /// matched to the signal's type.
    pub(super) fn show_tree_controls(&mut self, ui: &mut egui::Ui) {
        let Some(signum) = self.tree_control_signal() else {
            ui.weak(if self.tree_selection.is_empty() {
                "Select a signal to set it"
            } else {
                "Select only one signal to set it"
            });
            return;
        };
        if self.tree_editor.signum != Some(signum) {
            // Keep the pulse length and signedness while moving between signals.
            self.tree_editor = Editor {
                signum: Some(signum),
                pulse_ms: self.tree_editor.pulse_ms,
                signed: self.tree_editor.signed,
                ..Default::default()
            };
        }
        let kind = self.writable_kind(signum);
        let enabled = self.can_write() && kind != SignalKind::Unknown;
        let held = self.held_signal == Some(signum);
        ui.horizontal(|ui| {
            let declared = self.signal_kind(signum);
            ui.weak(declared.label());
            let mut name =
                egui::RichText::new(self.signal_name(signum)).font(self.fonts.name.clone());
            if let Some(color) = declared.color(ui.visuals().dark_mode) {
                name = name.color(color);
            }
            ui.add(egui::Label::new(name).truncate());
        });
        if let Some(reason) = self.read_only_reason(signum) {
            ui.weak(reason);
            return;
        }
        if !self.can_write() {
            ui.weak("Start the debugger and wait for synchronization to set signals");
        }
        let mut editor = std::mem::take(&mut self.tree_editor);
        let action = editor.controls(ui, kind, enabled, held, true);
        if !editor.status.is_empty() {
            ui.add(egui::Label::new(egui::RichText::new(&editor.status).weak()).truncate())
                .on_hover_text(&editor.status);
        }
        self.tree_editor = editor;
        if let Some(value) = action {
            self.set_signal(signum, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn analog_boundaries_and_signed_conversion_are_exact() {
        for (text, signed, expected) in [
            ("0", false, 0),
            ("65535", false, 65535),
            ("-32768", true, 32768),
            ("32767", true, 32767),
            ("-1", true, 65535),
        ] {
            assert_eq!(analog_value(text, signed), Ok(expected));
        }
        for (text, signed) in [
            ("65536", false),
            ("-1", false),
            ("-32769", true),
            ("32768", true),
            ("", false),
            ("1.5", false),
        ] {
            assert!(analog_value(text, signed).is_err());
        }
    }
}
