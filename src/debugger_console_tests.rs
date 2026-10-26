use super::*;

fn panel() -> Panel {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = vec![Signal {
        name: "Power".into(),
        metadata: [0x10, 0, 0, 0, 0, 1],
    }];
    panel
}

fn console(panel: &Panel) -> Vec<&str> {
    panel
        .samples
        .iter()
        .filter(|sample| sample.signum.is_none())
        .map(|sample| sample.value.as_str())
        .collect()
}

#[test]
fn only_the_processors_own_messages_count_as_console_output() {
    for line in [
        "Program started",
        "ERROR: release rejected",
        "CP3>Program started",
        "Value > limit",
    ] {
        assert!(is_console_message(line), "{line}");
    }
    for line in [
        "",
        "   ",
        "CP3>",
        "RMC3> ",
        "CP3>MDBGSIGNAL:01 -A:SHOW",
        "mdbgsignal:01 -S:ON 0x10",
        "SETSIGNAL:1 0x10 \"error\"",
        "GLOBAL:ON",
        "CP3>GLOBAL:OFF",
        "00000020:DEBUG=ON:IGNORE=OFF",
        "CP3>0000001A:DEBUG=OFF:IGNORE=OFF",
    ] {
        assert!(!is_console_message(line), "{line:?}");
    }
}

#[test]
fn console_lines_between_value_reports_join_the_timeline() {
    let mut panel = panel();
    let first = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
    let later = first + std::time::Duration::from_secs(1);
    panel.ingest(
        b"\r\nCP3>MDBGSIGNAL:01 -A:SHOW\r\nGLOBAL:OFF\r\n\x1b[1mProgram\x01\r\n00000010:1=1\r\n\x03",
        first,
    );
    // The line resumes after the report, and is timed from its first byte.
    panel.ingest(b" started\r\nCP3>", later);
    assert_eq!(console(&panel), ["Program started"]);
    assert_eq!(panel.samples[0].signum, Some(0x10));
    assert_eq!(panel.samples[1].at, first);
    assert_eq!(panel.values[&0x10], "1");
    // The prompt waits for the end of its line.
    panel.ingest(b"Done\r\n", later);
    assert_eq!(console(&panel), ["Program started", "CP3>Done"]);
}

#[test]
fn console_output_waits_while_paused_and_is_cleared_with_the_timeline() {
    let mut panel = panel();
    panel.toggle_pause();
    panel.ingest(b"Message\r\n", SystemTime::now());
    assert!(panel.samples.is_empty());
    assert_eq!(panel.pending_samples.len(), 1);
    panel.toggle_pause();
    assert_eq!(console(&panel), ["Message"]);
    panel.clear_history();
    assert!(panel.samples.is_empty());
}

#[test]
fn an_oversized_frame_is_not_taken_for_console_output() {
    let mut panel = panel();
    let mut bytes = vec![1];
    bytes.extend(std::iter::repeat_n(b'x', 70 * 1024));
    bytes.extend(b"\r\nnot console\r\n\x03After\r\n");
    panel.ingest(&bytes, SystemTime::now());
    assert_eq!(console(&panel), ["After"]);
}

#[test]
fn overlong_console_lines_are_cut_short() {
    let mut panel = panel();
    let mut bytes = vec![b'y'; MAX_CONSOLE_LINE * 2];
    bytes.extend(b"\r\n");
    panel.ingest(&bytes, SystemTime::now());
    assert_eq!(console(&panel), ["y".repeat(MAX_CONSOLE_LINE)]);
}

#[test]
fn watch_group_filters_leave_console_output_out() {
    let mut panel = panel();
    panel.ingest(
        b"Message\r\n\x01\r\n00000010:1=1\r\n\x03",
        SystemTime::now(),
    );
    assert_eq!(panel.timeline_indices().len(), 2);
    panel.groups.get_mut("Watched").unwrap().push(0x10);
    panel.timeline_group = Some("Watched".into());
    let visible = panel.timeline_indices();
    assert_eq!(visible.len(), 1);
    assert_eq!(panel.samples[visible[0]].signum, Some(0x10));
    assert!(
        panel.signal_history(0x10).len() == 1,
        "not in a signal's history"
    );
}

#[test]
fn console_rows_are_marked_and_span_the_name_and_value_columns() {
    let ctx = egui::Context::default();
    let mut panel = panel();
    let message = "Program started on slot 1";
    panel.ingest(
        format!("{message}\r\n\x01\r\n00000010:1=1\r\n\x03").as_bytes(),
        SystemTime::now(),
    );
    let mut output: Option<egui::FullOutput> = None;
    for _ in 0..3 {
        if let Some(previous) = output.take() {
            previous.drop_without_applying_deltas();
        }
        output = Some(ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1600.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| panel.contents(ui),
        ));
    }
    let mut output = output.unwrap();
    output.textures_delta.clear();
    // The rightmost, since the tree has a "Signal name" heading too.
    let position = |label: &str| {
        output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == label => Some(text.pos),
                _ => None,
            })
            .max_by(|a, b| a.x.total_cmp(&b.x))
            .unwrap_or_else(|| panic!("{label} drawn"))
    };
    let badge = position("Console");
    let text = position(message);
    let names = position("Signal name");
    let values = position("Value received");
    assert!(badge.x < values.x && badge.x >= names.x - 8.0, "{badge:?}");
    assert!(text.x > badge.x && text.x < values.x, "{text:?}");
    // The signal's row still has its own name.
    position("Power");
}

#[test]
fn program_messages_wrapped_by_the_processor_join_the_timeline() {
    let mut panel = panel();
    let first = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
    let later = first + std::time::Duration::from_secs(1);
    // As a CP3 sent them: back to back, without line endings, the second
    // split across reads, with a value report after.
    panel.ingest(
        b"CP3>\xfa\xe0\x01BiampTesira1[ Error: No suitable authentication method found ]\xfb\xfa",
        first,
    );
    panel.ingest(
        b"\xe0\x01BiampTesira1[ ConnectStatusChange: False ]\xfb\x01\r\n00000010:1=1\r\n\x03",
        later,
    );
    assert_eq!(
        console(&panel),
        [
            "BiampTesira1[ Error: No suitable authentication method found ]",
            "BiampTesira1[ ConnectStatusChange: False ]",
        ]
    );
    assert_eq!(panel.samples[1].at, later);
    assert_eq!(panel.values[&0x10], "1");
    // The prompt before them is still a prompt.
    panel.ingest(b"Done\r\n", later);
    assert_eq!(console(&panel)[2], "CP3>Done");
}

#[test]
fn an_unfinished_program_message_does_not_swallow_value_reports() {
    let mut panel = panel();
    panel.ingest(
        b"\xfa\xe0\x01Cut short\x01\r\n00000010:1=1\r\n\x03",
        SystemTime::now(),
    );
    assert_eq!(console(&panel), ["Cut short"]);
    assert_eq!(panel.values[&0x10], "1");
}
