//! The processor's ordinary console output, shown in the signal timeline
//! between value reports.
use super::*;

#[cfg(test)]
#[path = "debugger_console_tests.rs"]
mod tests;

/// No console line is this long; never grow unbounded.
const MAX_CONSOLE_LINE: usize = 4096;

/// Whether a console line belongs in the timeline: not blank, not a bare
/// prompt, and not the debugger's own commands echoed back or their replies,
/// which would bury the processor's messages at every start.
fn is_console_message(line: &str) -> bool {
    let line = line.trim();
    // A prompt such as `CP3>` runs on into the next line.
    let rest = match line.split_once('>') {
        Some((prompt, rest)) if !prompt.is_empty() && !prompt.chars().any(char::is_whitespace) => {
            rest.trim()
        }
        _ => line,
    };
    let upper = rest.to_ascii_uppercase();
    // Signal flag listings look like `00000020:DEBUG=ON:IGNORE=OFF`.
    let flag_listing = upper.get(..8).is_some_and(|number| {
        number.bytes().all(|byte| byte.is_ascii_hexdigit()) && upper[8..].starts_with(":DEBUG=")
    });
    !(rest.is_empty()
        || upper.starts_with("MDBGSIGNAL:")
        || upper.starts_with("SETSIGNAL:")
        || upper == "GLOBAL:ON"
        || upper == "GLOBAL:OFF"
        || flag_listing)
}

impl Panel {
    /// One byte of console output outside a value report. A line is timed
    /// from its first byte and recorded once it ends.
    pub(super) fn console_byte(&mut self, byte: u8, at: SystemTime) {
        match byte {
            b'\r' | b'\n' => {
                let at = self.console_at.take().unwrap_or(at);
                let text = crate::terminal::readable(&self.console_line);
                self.console_line.clear();
                if is_console_message(&text) {
                    self.record(None, text.trim().to_owned(), at);
                }
            }
            _ if self.console_line.len() < MAX_CONSOLE_LINE => {
                self.console_at.get_or_insert(at);
                self.console_line.push(byte);
            }
            _ => {}
        }
    }

    /// Take `byte` if it belongs to a program's message. On a CP3, messages a
    /// running program prints arrive wrapped as `FA E0 01 <text> FB`, with no
    /// line ending. Neither FA nor FB occurs in UTF-8 text, but the 01 would
    /// otherwise open a value report and swallow the text. A value report
    /// arriving inside an unfinished message ends it.
    pub(super) fn program_message_byte(&mut self, byte: u8, at: SystemTime) -> bool {
        if self.program_message.is_some() {
            if byte == 0xFB || byte == 1 {
                let (message, at) = self.program_message.take().expect("checked above");
                for line in crate::terminal::readable(&message).lines() {
                    let line = line.trim();
                    if !line.is_empty() {
                        self.record(None, line.to_owned(), at);
                    }
                }
                return byte == 0xFB;
            }
            let (message, _) = self.program_message.as_mut().expect("checked above");
            if message.len() < MAX_CONSOLE_LINE {
                message.push(byte);
            }
            return true;
        }
        match (self.program_marker, byte) {
            (_, 0xFA) => self.program_marker = 1,
            (1, 0xE0) => self.program_marker = 2,
            // Only 01 has been seen after FA E0; take any.
            (2, _) => {
                self.program_marker = 0;
                self.program_message = Some((Vec::new(), at));
            }
            _ => {
                self.program_marker = 0;
                return false;
            }
        }
        true
    }

    /// A timeline row for a console line: tinted rather than striped, marked
    /// **Console**, with the text running across the name and value columns.
    pub(super) fn show_console_row(
        &mut self,
        ui: &mut egui::Ui,
        sample: &Sample,
        width: f32,
        row_height: f32,
    ) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row_height), egui::Sense::hover());
        let response = ui.interact(
            rect,
            ui.id().with(("signal_sample", sample.id)),
            egui::Sense::click(),
        );
        let selected = self.selected_sample == Some(sample.id);
        let visuals = ui.visuals();
        let fill = if selected {
            visuals.selection.bg_fill
        } else if response.hovered() {
            visuals.widgets.hovered.bg_fill
        } else {
            visuals.code_bg_color
        };
        let color = if selected {
            visuals.selection.stroke.color
        } else {
            visuals.text_color()
        };
        let badge_fill = visuals.widgets.inactive.bg_fill;
        let badge_stroke = visuals.widgets.noninteractive.fg_stroke;
        let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        painter.rect_filled(rect, 4.0, fill);
        let time = egui::Rect::from_min_size(
            rect.min + egui::vec2(6.0, 0.0),
            egui::vec2(208.0, row_height),
        );
        painter.with_clip_rect(time.intersect(ui.clip_rect())).text(
            time.left_center(),
            egui::Align2::LEFT_CENTER,
            format_received(sample.at),
            egui::FontId::proportional(13.0),
            color,
        );
        // The badge stands where a signal's name would.
        let label = painter.layout_no_wrap(
            "Console".into(),
            egui::FontId::proportional(11.0),
            badge_stroke.color,
        );
        let badge = egui::Rect::from_center_size(
            egui::pos2(
                rect.left() + 234.0 + label.size().x / 2.0 + 6.0,
                rect.center().y,
            ),
            label.size() + egui::vec2(12.0, 4.0),
        );
        painter.rect(
            badge,
            6.0,
            badge_fill,
            badge_stroke,
            egui::StrokeKind::Inside,
        );
        painter.galley(
            badge.center() - label.size() / 2.0,
            label,
            badge_stroke.color,
        );
        let text = egui::Rect::from_min_max(
            egui::pos2(badge.right() + 8.0, rect.top()),
            rect.max - egui::vec2(6.0, 0.0),
        );
        painter.with_clip_rect(text.intersect(ui.clip_rect())).text(
            text.left_center(),
            egui::Align2::LEFT_CENTER,
            &sample.value,
            self.fonts.value.clone(),
            color,
        );
        let response = response.on_hover_text(format!(
            "Console output\n{}\n{}",
            format_received(sample.at),
            sample.value
        ));
        if response.clicked() || response.secondary_clicked() {
            self.selected_sample = Some(sample.id);
        }
        response.context_menu(|ui| {
            ui.strong("Console output");
            ui.separator();
            if ui.button("Copy line").clicked() {
                ui.ctx().copy_text(sample.value.clone());
                ui.close();
            }
        });
    }
}
