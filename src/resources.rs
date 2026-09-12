//! Crestron `free` and `ramfree` reports and their free-capacity gauges.
use eframe::egui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub free: u64,
    pub total: u64,
}

pub fn parse(report: &str, memory: bool) -> Option<Capacity> {
    let total_label = if memory {
        "total bytes of physical memory"
    } else {
        "bytes available"
    };
    let mut free = None;
    let mut total = None;
    for line in report.lines() {
        let normalized = line
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        let Some((number, label)) = normalized.split_once(' ') else {
            continue;
        };
        let target = if label == "bytes free" {
            &mut free
        } else if label == total_label {
            &mut total
        } else {
            continue;
        };
        if target.is_some() {
            return None;
        }
        *target = Some(number.replace(',', "").parse::<u64>().ok()?);
    }
    let capacity = Capacity {
        free: free?,
        total: total?,
    };
    (capacity.total > 0 && capacity.free <= capacity.total).then_some(capacity)
}

pub fn show(ui: &mut egui::Ui, disk: &str, memory: &str) {
    ui.columns(2, |columns| {
        gauge(&mut columns[0], "Free disk space", disk, false);
        gauge(&mut columns[1], "Free memory", memory, true);
    });
    ui.separator();
}

fn gauge(ui: &mut egui::Ui, title: &str, report: &str, memory: bool) {
    let capacity = parse(report, memory);
    ui.vertical_centered(|ui| {
        ui.strong(title);
        let width = ui.available_width().clamp(40.0, 190.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, width * 0.55), egui::Sense::hover());
        let center = egui::pos2(rect.center().x, rect.bottom() - 5.0);
        let radius = width * 0.45;
        let arc = |fraction: f32| {
            (0..=64)
                .map(|i| {
                    let angle =
                        std::f32::consts::PI + std::f32::consts::PI * fraction * i as f32 / 64.0;
                    center + egui::vec2(angle.cos(), angle.sin()) * radius
                })
                .collect::<Vec<_>>()
        };
        ui.painter().add(egui::Shape::line(
            arc(1.0),
            egui::Stroke::new(8.0, ui.visuals().widgets.noninteractive.bg_fill),
        ));
        if let Some(capacity) = capacity {
            let fraction = capacity.free as f32 / capacity.total as f32;
            let color = if fraction < 0.1 {
                egui::Color32::from_rgb(226, 96, 96)
            } else {
                egui::Color32::from_rgb(68, 180, 110)
            };
            if fraction > 0.0 {
                ui.painter().add(egui::Shape::line(
                    arc(fraction),
                    egui::Stroke::new(8.0, color),
                ));
            }
            ui.painter().text(
                center - egui::vec2(0.0, 12.0),
                egui::Align2::CENTER_BOTTOM,
                format!("{:.0}% free", fraction * 100.0),
                egui::FontId::proportional(16.0),
                ui.visuals().text_color(),
            );
            ui.label(format!(
                "{} free",
                crate::archive::human_size(capacity.free)
            ));
            ui.weak(format!(
                "of {} total",
                crate::archive::human_size(capacity.total)
            ));
        } else {
            ui.label("Unavailable");
            ui.weak("Refresh to query device resources");
        }
        response.on_hover_text(if report.is_empty() {
            "No resource report received"
        } else {
            report
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    // Captured from the user's RMC3 via read-only SSH commands.
    const DISK: &str = "2755432448 bytes free\r\n145522688 bytes actually used\r\n0 bytes reclaimable\r\n2900955136 bytes available\r\n";
    const MEMORY: &str = "62 percent of memory in use\r\n171016192 total bytes of physical memory\r\n105664512 bytes actually used\r\n65351680 bytes free\r\n0 bytes reclaimable\r\n";
    #[test]
    fn parses_live_rmc3_reports() {
        assert_eq!(
            parse(DISK, false),
            Some(Capacity {
                free: 2755432448,
                total: 2900955136
            })
        );
        assert_eq!(
            parse(MEMORY, true),
            Some(Capacity {
                free: 65351680,
                total: 171016192
            })
        );
    }
    #[test]
    fn invalid_reports_do_not_show_fake_zero_capacity() {
        for report in [
            "",
            "Unknown command",
            "1 bytes free",
            "2 bytes free\n1 bytes available",
            "0 bytes free\n0 bytes available",
            "1 bytes free\n2 bytes free\n3 bytes available",
        ] {
            assert_eq!(parse(report, false), None);
        }
        assert_eq!(
            parse("0 bytes free\n1 bytes available", false),
            Some(Capacity { free: 0, total: 1 })
        );
        assert_eq!(
            parse(" 1,024 BYTES FREE\n2,048 bytes available", false),
            Some(Capacity {
                free: 1024,
                total: 2048
            })
        );
    }
    #[test]
    fn renders_both_gauges_with_human_readable_values() {
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| show(ui, DISK, MEMORY));
        let texts: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(t.galley.text()),
                _ => None,
            })
            .collect();
        for expected in [
            "Free disk space".to_owned(),
            "Free memory".to_owned(),
            format!("{} free", crate::archive::human_size(2755432448)),
            format!("{} free", crate::archive::human_size(65351680)),
        ] {
            assert!(texts.contains(&expected.as_str()), "missing {expected}");
        }
        output.drop_without_applying_deltas();
    }
}
