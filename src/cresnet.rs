//! What a processor reports about the devices on its Cresnet bus.
//!
//! `REPORTCRESNET` answers with a line per device and no heading to take column
//! names from, so the columns are named here:
//!
//! ```text
//! 0A: STATUSSIGN [v1.3443.00016, #01415779]
//! 65: C2N-SPWS300 Power Supply With Boost [v1.6.0, #00390667]
//! ```
//!
//! A model name can be several words and a device may report neither a version
//! nor a serial, so a line is taken apart from its punctuation outwards rather
//! than by counting words.

use eframe::egui;

const COLUMNS: [&str; 4] = ["ID", "Model", "Firmware", "Serial"];

#[derive(Debug, Default, PartialEq, Eq)]
struct Device<'a> {
    id: &'a str,
    model: &'a str,
    firmware: &'a str,
    serial: &'a str,
}

impl<'a> Device<'a> {
    fn cells(&self) -> [&'a str; 4] {
        [self.id, self.model, self.firmware, self.serial]
    }
}

/// The devices in a report. Lines that are not one — a heading, a total, a
/// message that nothing is connected — are left out and stay in the raw
/// response, since guessing at them would put nonsense in the table.
fn parse(contents: &str) -> Vec<Device<'_>> {
    contents.lines().filter_map(device).collect()
}

fn device(line: &str) -> Option<Device<'_>> {
    let (id, rest) = line.trim().split_once(':')?;
    let id = id.trim();
    // A Cresnet address is two hex digits. Requiring that is what keeps a
    // sentence with a colon in it out of the table.
    if id.len() != 2 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let rest = rest.trim();
    // The reported values are bracketed at the end, behind a model name that
    // may itself contain anything but brackets.
    let (model, reported) = match rest.rsplit_once('[') {
        Some((model, reported)) => (model.trim(), reported.trim_end().trim_end_matches(']')),
        None => (rest, ""),
    };
    if model.is_empty() {
        return None;
    }
    let mut values = reported.split(',').map(str::trim).filter(|v| !v.is_empty());
    // Order is not assumed: the serial is the one marked as a number.
    let (serial, firmware) = (
        values.clone().find(|value| value.starts_with('#')),
        values.find(|value| !value.starts_with('#')),
    );
    Some(Device {
        id,
        model,
        firmware: firmware.unwrap_or_default(),
        serial: serial.unwrap_or_default(),
    })
}

pub fn show(ui: &mut egui::Ui, device_id: &str, contents: &str) {
    ui.push_id(("cresnet", device_id), |ui| {
        egui::CollapsingHeader::new("Cresnet devices")
            .default_open(true)
            .show(ui, |ui| {
                let devices = parse(contents);
                if devices.is_empty() {
                    // An error, or a report in a shape this does not know.
                    raw_response(ui, contents);
                    return;
                }
                ui.small(format!("{} device(s) on the bus", devices.len()));
                egui::ScrollArea::horizontal()
                    .id_salt("cresnet_columns")
                    .show(ui, |ui| {
                        egui::Grid::new("cresnet_devices")
                            .num_columns(COLUMNS.len())
                            .striped(true)
                            .spacing([16.0, 6.0])
                            .show(ui, |ui| {
                                for column in COLUMNS {
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(column).strong())
                                            .extend(),
                                    );
                                }
                                ui.end_row();
                                for device in &devices {
                                    for cell in device.cells() {
                                        ui.add(
                                            egui::Label::new(egui::RichText::new(cell).monospace())
                                                .extend(),
                                        );
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                ui.add_space(8.0);
                ui.collapsing("Raw response", |ui| raw_response(ui, contents));
            });
    });
}

fn raw_response(ui: &mut egui::Ui, contents: &str) {
    let mut text = contents.trim();
    ui.add(
        egui::TextEdit::multiline(&mut text)
            .frame(
                egui::Frame::new()
                    .inner_margin(4)
                    .fill(ui.visuals().text_edit_bg_color())
                    .stroke(egui::Stroke::new(1.0, egui::Color32::BLACK)),
            )
            .font(egui::TextStyle::Monospace)
            .desired_width(f32::INFINITY),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = concat!(
        "0A: STATUSSIGN [v1.3443.00016, #01415779]\r\n",
        "0B: STATUSSIGN [v1.3443.00016, #0142D714]\r\n",
        "0C: STATUSSIGN [v1.3443.00012, #0114D308]\r\n",
        "0D: STATUSSIGN [v1.3443.00016, #0142D719]\r\n",
        "65: C2N-SPWS300 Power Supply With Boost [v1.6.0, #00390667]\r\n",
    );

    #[test]
    fn a_report_becomes_a_device_per_line() {
        let devices = parse(SAMPLE);
        assert_eq!(devices.len(), 5);
        assert_eq!(
            devices[0].cells(),
            ["0A", "STATUSSIGN", "v1.3443.00016", "#01415779"]
        );
        // A model name of several words stays whole, and the values behind it
        // are still found.
        assert_eq!(
            devices[4].cells(),
            [
                "65",
                "C2N-SPWS300 Power Supply With Boost",
                "v1.6.0",
                "#00390667"
            ]
        );
        // Two devices of the same model differ only where they should.
        assert_eq!(devices[1].model, devices[2].model);
        assert_ne!(devices[1].firmware, devices[2].firmware);
    }

    #[test]
    fn devices_that_report_less_than_the_rest_still_appear() {
        let devices = parse(
            "10: CNSMPLUS\n\
             11: CN-TVAV [v2.0]\n\
             12: DIN-1DIMU4 [#00ABCDEF]\n\
             13: C2N-DB12 [#00112233, v1.2.3]\n",
        );
        assert_eq!(devices.len(), 4);
        assert_eq!(devices[0].cells(), ["10", "CNSMPLUS", "", ""]);
        assert_eq!(devices[1].cells(), ["11", "CN-TVAV", "v2.0", ""]);
        assert_eq!(devices[2].cells(), ["12", "DIN-1DIMU4", "", "#00ABCDEF"]);
        // Whichever way round they are reported, the serial is the one marked.
        assert_eq!(
            devices[3].cells(),
            ["13", "C2N-DB12", "v1.2.3", "#00112233"]
        );
    }

    #[test]
    fn everything_that_is_not_a_device_is_left_to_the_raw_response() {
        for line in [
            "",
            "No Cresnet devices found",
            // A sentence with a colon is not an address.
            "Note: the bus is terminated",
            "Total: 5",
            // An address without a model says nothing worth tabulating.
            "0A:",
            "0A: [v1.0, #001]",
            // Neither too short, too long, nor not hexadecimal.
            "0: STATUSSIGN",
            "0AB: STATUSSIGN",
            "0G: STATUSSIGN",
        ] {
            assert!(parse(line).is_empty(), "{line:?}");
        }
        // A report with chatter around it keeps only the devices.
        let noisy = format!("Reporting Cresnet devices...\n{SAMPLE}\nDone.\n");
        assert_eq!(parse(&noisy).len(), 5);
    }
}
