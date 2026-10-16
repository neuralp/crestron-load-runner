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
    ) -> Option<SignalValue> {
        let mut action = None;
        ui.strong("Set signal");
        ui.weak(
            "Changes the running program. Current value and history show processor reports only.",
        );
        if !enabled {
            ui.weak("Controls require an active, synchronized session and a writable signal type.");
        }
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
                    ui.horizontal(|ui| {
                        let hold = ui.button(if held { "Holding — release mouse" } else { "Push and hold" });
                        if hold.is_pointer_button_down_on() && !held
                            && ui.input(|i| i.focused && i.pointer.button_pressed(egui::PointerButton::Primary)) {
                            action = Some(SignalValue::Hold);
                        }
                        if ui.add_enabled(held, egui::Button::new("Release")).clicked() { action = Some(SignalValue::Release); }
                    });
                    ui.weak("Hold releases on mouse-up, focus loss, closing details, or Stop. Network loss can prevent release.");
                }
                SignalKind::Analog => {
                    ui.horizontal(|ui| {
                        let old_signed = self.signed;
                        if ui.checkbox(&mut self.signed, "Signed").changed()
                            && let Ok(bits) = analog_value(&self.analog, old_signed) {
                                self.analog = if self.signed { (bits as i16).to_string() } else { bits.to_string() };
                            }
                        ui.add(egui::TextEdit::singleline(&mut self.analog).desired_width(120.0));
                        let parsed = analog_value(&self.analog, self.signed);
                        if ui.add_enabled(parsed.is_ok(), egui::Button::new("Set value")).clicked() {
                            action = parsed.ok().map(SignalValue::Analog);
                        }
                    });
                    ui.weak(if self.signed { "Range: −32768 to 32767 (16-bit two’s complement)" } else { "Range: 0 to 65535" });
                    if let Err(error) = analog_value(&self.analog, self.signed) { ui.colored_label(egui::Color32::LIGHT_RED, error); }
                }
                SignalKind::Serial => {
                    ui.add(egui::TextEdit::multiline(&mut self.serial).desired_rows(2).desired_width(f32::INFINITY).hint_text("String to send"));
                    if ui.button("Send string").clicked() { action = Some(SignalValue::Serial(self.serial.clone())); }
                    ui.weak("Literal text, sent as UTF-8 bytes; quotes and line breaks are escaped automatically. Empty strings are allowed.");
                }
                SignalKind::Unknown => { ui.label("This signal's type is not supported for writing."); }
            }
        });
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        action
    }
}

impl Panel {
    pub(super) fn writable_kind(&self, signum: u32) -> SignalKind {
        let mut records = self.signals.iter().filter(|s| s.signum() == signum);
        let Some(first) = records.next() else {
            return SignalKind::Unknown;
        };
        let kind = first.kind();
        if first.metadata[4] & 0x80 != 0
            || records.any(|s| s.metadata[4] & 0x80 != 0 || s.kind() != kind)
        {
            SignalKind::Unknown
        } else {
            kind
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
        self.editor.status = result.map_or_else(|error| error, |_| "Command queued…".into());
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
            self.editor.status = if sent {
                "Release queued…"
            } else {
                "Disconnected: held signal release could not be confirmed"
            }
            .into();
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
