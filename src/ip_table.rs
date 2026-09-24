use eframe::egui;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Table<'a> {
    pub(crate) title: &'a str,
    pub(crate) headings: Vec<&'a str>,
    pub(crate) rows: Vec<Vec<&'a str>>,
}

/// Headings come from the first pipe-delimited line of each table, not a
/// firmware-specific schema. Empty cells must remain in place.
pub(crate) fn parse(contents: &str) -> Option<Vec<Table<'_>>> {
    let mut tables = Vec::new();
    let mut current: Option<Table<'_>> = None;
    let mut title = "";
    for line in contents.lines().map(str::trim) {
        if let Some(start) = line.strip_prefix("TableStart:") {
            if let Some(table) = current.take() {
                tables.push(table);
            }
            title = start
                .trim()
                .strip_prefix('[')
                .and_then(|text| text.strip_suffix(']'))
                .unwrap_or(start)
                .trim();
            continue;
        }
        if line.starts_with("TableEnd:") {
            if let Some(table) = current.take() {
                tables.push(table);
            }
            title = "";
            continue;
        }
        if !line.contains('|')
            || line
                .chars()
                .all(|c| c.is_whitespace() || matches!(c, '-' | '=' | '+' | '|'))
        {
            continue;
        }
        let cells: Vec<_> = line.split('|').map(str::trim).collect();
        if let Some(table) = &mut current {
            // Do not silently drop extra fields or shift malformed rows.
            if cells.len() != table.headings.len() {
                return None;
            }
            table.rows.push(cells);
        } else {
            current = Some(Table {
                title,
                headings: cells,
                rows: Vec::new(),
            });
        }
    }
    if let Some(table) = current {
        tables.push(table);
    }
    (!tables.is_empty()).then_some(tables)
}

pub fn show(ui: &mut egui::Ui, device_id: &str, contents: &str) {
    ui.push_id(("ip_table", device_id), |ui| {
        egui::CollapsingHeader::new("IP table")
            .default_open(true)
            .show(ui, |ui| {
                if let Some(tables) = parse(contents) {
                    for (index, table) in tables.iter().enumerate() {
                        if !table.title.is_empty() {
                            ui.strong(table.title);
                        }
                        egui::ScrollArea::horizontal()
                            .id_salt(("columns", index))
                            .show(ui, |ui| {
                                egui::Grid::new(("table", index))
                                    .num_columns(table.headings.len())
                                    .striped(true)
                                    .spacing([16.0, 6.0])
                                    .show(ui, |ui| {
                                        for heading in &table.headings {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(*heading).strong(),
                                                )
                                                .extend(),
                                            );
                                        }
                                        ui.end_row();
                                        for row in &table.rows {
                                            for cell in row {
                                                ui.add(egui::Label::new(*cell).extend());
                                            }
                                            ui.end_row();
                                        }
                                    });
                            });
                        if table.rows.is_empty() {
                            ui.small("No entries.");
                        }
                        ui.add_space(8.0);
                    }
                    ui.collapsing("Raw response", |ui| raw_response(ui, contents));
                } else {
                    // Keep errors and unfamiliar firmware responses visible.
                    raw_response(ui, contents);
                }
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

    const SAMPLE: &str = include_str!("../tests/fixtures/ip_table.txt");

    #[test]
    fn parses_received_headings_and_preserves_empty_cells() {
        let tables = parse(SAMPLE).unwrap();
        assert_eq!(tables.len(), 1);
        let table = &tables[0];
        assert_eq!(table.title, "IP Table for program 1");
        assert_eq!(
            table.headings,
            [
                "CIP_ID",
                "Type",
                "Status",
                "DevID",
                "Port",
                "IP Address/SiteName",
                "Model Name",
                "Description",
                "RoomId"
            ]
        );
        assert_eq!(table.rows.len(), 8);
        assert!(
            table
                .rows
                .iter()
                .all(|row| row.len() == 9 && row[3].is_empty())
        );
        assert_eq!(table.rows[0][5], "127.000.000.001");
        assert_eq!(
            table.rows[2][7],
            "RoomView Connected Display (room01_display02)"
        );
        assert_eq!(
            table.rows[5],
            [
                "81",
                "Client",
                "WAITING",
                "",
                "23",
                "140.104.108.231",
                "TCP/IP Client",
                "turtle_av_telnet",
                "Not Specified"
            ]
        );
    }

    #[test]
    fn supports_multiple_programs_dynamic_columns_and_crlf() {
        let tables = parse("console> ipt -t\r\nTableStart:[ Program 1 ]\r\nName | Custom\r\n-----|-----\r\nA |\r\nTableEnd:\r\nTableStart:[ Program 2 ]\r\nOther | Fields | Here\r\n | B | C\r\nTableEnd:\r\nconsole>").unwrap();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].headings, ["Name", "Custom"]);
        assert_eq!(tables[0].rows[0], ["A", ""]);
        assert_eq!(tables[1].title, "Program 2");
        assert_eq!(tables[1].rows[0], ["", "B", "C"]);
    }

    #[test]
    fn supports_empty_tables_and_unmarked_tables_and_falls_back_on_bad_rows() {
        assert!(parse("Name | Status\n--------").unwrap()[0].rows.is_empty());
        assert_eq!(
            parse("Name | Status\nA | ONLINE").unwrap()[0].rows[0],
            ["A", "ONLINE"]
        );
        assert!(parse("Name | Status\nA | ONLINE | extra").is_none());
        assert!(parse("Name | Status | Address\nA | ONLINE").is_none());
        assert!(parse("Command failed: not supported").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn renders_individual_headers_and_cells_in_aligned_columns() {
        let ctx = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(2400.0, 900.0),
            )),
            ..Default::default()
        };
        for _ in 0..2 {
            ctx.run_ui(input(), |ui| show(ui, "test", SAMPLE))
                .drop_without_applying_deltas();
        }
        let output = ctx.run_ui(input(), |ui| show(ui, "test", SAMPLE));
        let position = |value| {
            output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.text() == value
                    {
                        Some(text.pos)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("missing cell: {value}"))
        };
        let heading = position("CIP_ID");
        let first = position("3");
        let second = position("4");
        assert_eq!(heading.x, first.x);
        assert_eq!(first.x, second.x);
        assert!(heading.y < first.y && first.y < second.y);
        assert!(position("Type").x > heading.x);
        position("RoomId");
        position("turtle_av_telnet");
        output.drop_without_applying_deltas();
    }
}
