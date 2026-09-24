use super::*;
use crate::model::{AddressEntry, DiscoveredIdentity};

fn panel() -> Panel {
    let processor = device("192.0.2.10", "CP4", "rack", true);
    let candidates = vec![
        device("192.0.2.20", "TS-770", "panel", false),
        device("192.0.2.21", "TS-1070", "other", false),
    ];
    let mut panel = Panel::new(1, processor, candidates);
    panel.accept_table(Ok(
        "CIP_ID|Model Name\n11|ts-770\n12|TS-770\n13|missing".into()
    ));
    panel
}

#[test]
fn panel_allows_model_mismatches_but_rejects_duplicate_physical_targets() {
    let mut panel = panel();
    assert!(panel.jobs().is_err());
    panel.rows[0].selected = Some(panel.candidates[1].id.clone());
    let jobs = panel.jobs().unwrap();
    assert_eq!(jobs[0].master, "192.0.2.10");
    assert_eq!(jobs[0].ipid, "11");
    assert_eq!(jobs[0].id, panel.candidates[1].id);
    panel.rows[1].selected = panel.rows[0].selected.clone();
    assert!(panel.jobs().is_err());
    let mut alias = panel.candidates[1].clone();
    alias.id = "alias:2222".into();
    alias.port = 2222;
    panel.rows[1].selected = Some(alias.id.clone());
    panel.candidates.push(alias);
    assert!(panel.jobs().is_err());
    panel.rows[1].selected = None;
    panel.candidates[1].discovered = None;
    assert!(panel.jobs().is_err());
}

#[test]
fn mismatched_model_warning_is_per_row_and_does_not_replace_results() {
    use eframe::egui;
    let mut panel = panel();
    let ctx = egui::Context::default();
    for (selected, warning) in [(Some(1), true), (Some(0), false), (None, false)] {
        panel.rows[0].selected = selected.map(|index| panel.candidates[index].id.clone());
        panel.rows[0].status = "Verified".into();
        let mut labels = Vec::new();
        for _ in 0..3 {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1600.0, 1000.0),
                    )),
                    ..Default::default()
                },
                |_| {
                    panel.show(&ctx, false);
                },
            );
            labels = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
        }
        let warnings: Vec<_> = labels
            .iter()
            .filter(|text| text.starts_with("Warning: model mismatch"))
            .collect();
        assert_eq!(warnings.len(), usize::from(warning));
        if warning {
            assert!(warnings[0].contains("ts-770"));
            assert!(warnings[0].contains("TS-1070"));
            assert!(panel.jobs().is_ok());
        }
        assert!(labels.iter().any(|text| text == "Verified"));
    }
}

#[test]
fn panel_revalidates_endpoints_but_allows_new_credentials_and_trust() {
    let mut panel = panel();
    panel.rows[0].selected = Some(panel.candidates[0].id.clone());
    let jobs = panel.jobs().unwrap();
    let mut live = vec![panel.processor.clone(), panel.candidates[0].clone()];
    live[1].credentials.password = "changed".into();
    live[1].ssh_host_key_fingerprint = Some("new trust".into());
    panel.validate_live(&live, &jobs).unwrap();
    live[1].host = "192.0.2.30".into();
    assert!(panel.validate_live(&live, &jobs).is_err());
    assert!(panel.validate_live(&live[..1], &jobs).is_err());
}

#[test]
fn panel_receivers_are_request_scoped_and_closed_windows_keep_results() {
    use std::sync::mpsc;
    let mut old = panel();
    let (sender, receiver) = mpsc::channel();
    old.table_reply = Some(receiver);
    let mut new = Panel::new(2, old.processor.clone(), old.candidates.clone());
    drop(old);
    assert!(
        sender
            .send(Ok("CIP_ID|Model Name\n11|TS-770".into()))
            .is_err()
    );
    new.poll();
    assert!(new.rows.is_empty());
    let (sender, receiver) = mpsc::channel();
    new.table_reply = Some(receiver);
    new.open = false;
    assert!(new.busy());
    drop(sender);
    new.poll();
    assert!(!new.busy());
    assert!(!new.valid);
    assert_eq!(new.message, "Table worker stopped");
    new.accept_table(Ok("CIP_ID|Model Name\n11|TS-770".into()));
    let (sender, receiver) = mpsc::channel();
    new.rows[0].reply = Some(receiver);
    sender.send(Ok("Verified".into())).unwrap();
    new.poll();
    assert_eq!(new.rows[0].status, "Verified");
    assert!(!new.busy());
}

#[test]
fn native_assignment_window_paints_an_opaque_background_in_both_themes() {
    use eframe::egui;
    use std::{cell::Cell, rc::Rc};

    for visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
        let ctx = egui::Context::default();
        let expected_fill = visuals.panel_fill;
        ctx.set_visuals(visuals);
        ctx.set_embed_viewports(false);
        let painted = Rc::new(Cell::new(false));
        let observed = painted.clone();
        egui::Context::set_immediate_viewport_renderer(move |ctx, mut viewport| {
            assert_eq!(viewport.builder.title.as_deref(), Some("Assign IPIDs"));
            let rect =
                egui::Rect::from_min_size(egui::Pos2::ZERO, viewport.builder.inner_size.unwrap());
            let mut input = egui::RawInput {
                viewport_id: viewport.ids.this,
                screen_rect: Some(rect),
                ..Default::default()
            };
            input.viewports.entry(viewport.ids.this).or_default().parent =
                Some(viewport.ids.parent);
            let output = ctx.run_ui(input, |ui| (viewport.viewport_ui_cb)(ui));
            let background = output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Rect(background) if background.rect.contains_rect(rect) => {
                    Some(background.fill)
                }
                _ => None,
            });
            observed.set(background.is_some_and(|fill| fill == expected_fill && fill.a() == 255));
            output.drop_without_applying_deltas();
        });
        let mut panel = panel();
        ctx.run_ui(egui::RawInput::default(), |_| {
            panel.show(&ctx, false);
        })
        .drop_without_applying_deltas();
        egui::Context::set_immediate_viewport_renderer(|_, _| {});
        assert!(
            painted.get(),
            "Native assignment viewport must paint its entire background"
        );
    }
}

#[test]
fn assignment_window_scrolls_as_one_page_in_both_themes() {
    use eframe::egui;
    use std::{cell::RefCell, rc::Rc};

    for visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
        let ctx = egui::Context::default();
        ctx.set_visuals(visuals);
        ctx.set_embed_viewports(false);
        let labels = Rc::new(RefCell::new(Vec::new()));
        let observed = labels.clone();
        let events = Rc::new(RefCell::new(Vec::new()));
        let pending_events = events.clone();
        egui::Context::set_immediate_viewport_renderer(move |ctx, mut viewport| {
            let rect =
                egui::Rect::from_min_size(egui::Pos2::ZERO, viewport.builder.inner_size.unwrap());
            let mut input = egui::RawInput {
                viewport_id: viewport.ids.this,
                screen_rect: Some(rect),
                ..Default::default()
            };
            input.viewports.entry(viewport.ids.this).or_default().parent =
                Some(viewport.ids.parent);
            input.events = pending_events.take();
            let output = ctx.run_ui(input, |ui| (viewport.viewport_ui_cb)(ui));
            *observed.borrow_mut() = output
                .shapes
                .iter()
                .filter_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape {
                        let bounds = egui::Rect::from_min_size(text.pos, text.galley.size());
                        Some((
                            text.galley.text().to_owned(),
                            rect.intersect(shape.clip_rect).contains_rect(bounds),
                            bounds,
                        ))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            output.drop_without_applying_deltas();
        });
        let mut panel = panel();
        let mut table = String::from("CIP_ID|Model Name\n");
        for ipid in 1..=24 {
            table.push_str(&format!("{ipid:02X}|TS-770\n"));
        }
        panel.accept_table(Ok(table));
        panel.rows[0].selected = Some(panel.candidates[0].id.clone());
        panel.rows[0].status = "Verified".into();
        panel.discovering = true;
        for _ in 0..4 {
            ctx.run_ui(egui::RawInput::default(), |_| {
                panel.show(&ctx, false);
            })
            .drop_without_applying_deltas();
        }
        let visible_bounds = |expected: &str| {
            labels
                .borrow()
                .iter()
                .find(|(text, visible, _)| text == expected && *visible)
                .map(|(_, _, bounds)| *bounds)
                .unwrap_or_else(|| panic!("{expected} must be visible: {:?}", labels.borrow()))
        };
        let processor_before = visible_bounds("Processor & program");
        let row_before = visible_bounds("Verified");
        assert!(
            !labels
                .borrow()
                .iter()
                .any(|(text, visible, _)| text == "GO" && *visible)
        );

        // Wheel input over a device row must move the processor section too,
        // rather than being consumed by a nested assignment-table scrollbar.
        events.borrow_mut().extend([
            egui::Event::PointerMoved(row_before.center()),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -30.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        for _ in 0..20 {
            ctx.run_ui(egui::RawInput::default(), |_| {
                panel.show(&ctx, false);
            })
            .drop_without_applying_deltas();
        }
        let processor_shift = processor_before.top() - visible_bounds("Processor & program").top();
        let row_shift = row_before.top() - visible_bounds("Verified").top();
        assert!(processor_shift > 1.0);
        assert!((processor_shift - row_shift).abs() < 1.0);

        events.borrow_mut().push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -10000.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        for _ in 0..30 {
            ctx.run_ui(egui::RawInput::default(), |_| {
                panel.show(&ctx, false);
            })
            .drop_without_applying_deltas();
        }
        egui::Context::set_immediate_viewport_renderer(|_, _| {});
        for expected in [
            "18",
            "Review & apply",
            "1 selected  ·  23 skipped",
            "GO",
            "Raw processor response",
        ] {
            visible_bounds(expected);
        }
    }
}

#[test]
fn panel_renders_configuration_rows_and_go() {
    let mut panel = panel();
    panel.discovering = true;
    let ctx = eframe::egui::Context::default();
    let input = || eframe::egui::RawInput {
        screen_rect: Some(eframe::egui::Rect::from_min_size(
            eframe::egui::Pos2::ZERO,
            eframe::egui::vec2(1600.0, 1000.0),
        )),
        ..Default::default()
    };
    for _ in 0..3 {
        ctx.run_ui(input(), |_| {
            panel.show(&ctx, false);
        })
        .drop_without_applying_deltas();
    }
    let output = ctx.run_ui(input(), |_| {
        panel.show(&ctx, false);
    });
    let labels: Vec<_> = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            eframe::egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        })
        .collect();
    for expected in [
        "Program",
        "IP address",
        "HOSTNAME",
        "GO",
        "CIP_ID",
        "Model Name",
        "No assignments selected",
        "Select devices, then press GO",
        "Discovering devices… Choices update as devices are found.",
    ] {
        assert!(
            labels.iter().any(|text| text == expected),
            "Missing label: {expected}"
        );
    }
    output.drop_without_applying_deltas();
}

#[test]
fn go_and_configuration_actions_obey_busy_and_modal_guards() {
    use eframe::egui;
    let mut panel = panel();
    panel.accept_table(Ok("CIP_ID|Model Name\n11|TS-770".into()));
    let ctx = egui::Context::default();
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1600.0, 1000.0),
        )),
        ..Default::default()
    };
    let click = |panel: &mut Panel, label: &str, blocked| {
        for _ in 0..3 {
            ctx.run_ui(input(), |_| {
                panel.show(&ctx, blocked);
            })
            .drop_without_applying_deltas();
        }
        let output = ctx.run_ui(input(), |_| {
            panel.show(&ctx, blocked);
        });
        let position = output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape
                    && text.galley.text() == label
                {
                    Some(text.pos + text.galley.size() * 0.5)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("missing {label}"));
        output.drop_without_applying_deltas();
        let mut result = None;
        for pressed in [true, false] {
            let mut raw = input();
            raw.events.push(egui::Event::PointerMoved(position));
            raw.events.push(egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            ctx.run_ui(raw, |_| {
                result = result.take().or(panel.show(&ctx, blocked));
            })
            .drop_without_applying_deltas();
        }
        result
    };
    assert_eq!(click(&mut panel, "GO", false), None);
    assert_eq!(click(&mut panel, "Skip", false), None);
    assert_eq!(click(&mut panel, "User label (192.0.2.21)", false), None);
    assert_eq!(
        panel.rows[0].selected.as_ref(),
        Some(&panel.candidates[1].id)
    );
    assert_eq!(click(&mut panel, "GO", false), Some(Action::Go));
    assert_eq!(click(&mut panel, "HOSTNAME", false), None);
    assert_eq!(panel.mode, AddressMode::Hostname);
    assert_eq!(click(&mut panel, "GO", true), None);
    assert_eq!(click(&mut panel, "IP address", true), None);
    assert_eq!(panel.mode, AddressMode::Hostname);
    let (_reply, receiver) = std::sync::mpsc::channel();
    panel.rows[0].reply = Some(receiver);
    assert_eq!(click(&mut panel, "GO", false), None);
    assert_eq!(click(&mut panel, "Reload table", false), None);
    panel.rows[0].reply = None;
    assert_eq!(
        click(&mut panel, "Reload table", false),
        Some(Action::Reload)
    );
    assert_eq!(click(&mut panel, "1", false), None);
    assert_eq!(click(&mut panel, "10", false), Some(Action::Reload));
    assert_eq!(panel.program, 10);
}

#[test]
fn a_batch_keeps_independent_success_and_failure_results() {
    let mut panel = panel();
    panel.accept_table(Ok("CIP_ID|Model Name\n11|TS-770\n12|TS-770".into()));
    let (one, rx) = std::sync::mpsc::channel();
    panel.rows[0].reply = Some(rx);
    let (two, rx) = std::sync::mpsc::channel();
    panel.rows[1].reply = Some(rx);
    one.send(Err("Removal failed".into())).unwrap();
    panel.poll();
    assert!(panel.busy());
    two.send(Ok("Verified".into())).unwrap();
    panel.poll();
    assert!(!panel.busy());
    assert!(panel.rows[0].status.contains("Removal failed"));
    assert_eq!(panel.rows[1].status, "Verified");
}

fn device(host: &str, model: &str, hostname: &str, processor: bool) -> Device {
    let mut device = Device::from_address(&AddressEntry {
        host: host.into(),
        model: model.into(),
        name: "User label".into(),
        kind: if processor {
            DeviceKind::Processor
        } else {
            DeviceKind::Unknown
        },
        ..Default::default()
    });
    device.discovered = Some(DiscoveredIdentity {
        ip: host.into(),
        hostname: hostname.into(),
        model: model.into(),
    });
    device
}

#[test]
fn projection_uses_shared_parser_and_preserves_tokens() {
    let rows = program_rows(include_str!("../tests/fixtures/ip_table.txt"), 1).unwrap();
    assert_eq!(
        rows[0],
        RowKey {
            ipid: "3".into(),
            model: "DM-TX-201-C".into()
        }
    );
    assert_eq!(rows[1].model, rows[0].model);
    assert!(rows.iter().any(|r| r.ipid == "11" && r.model == "TS-770"));
    assert!(program_rows("CIP_ID|Model Name", 10).unwrap().is_empty());
    for raw in [
        "",
        "error: unsupported",
        "CIP_ID|Model Name\n11|A|extra",
        "CIP_ID|Model Name\n11|A\n11|B",
        "CIP_ID|Model Name\nA|A\n0a|A",
        "CIP_ID|Model Name\nZZ|A",
        "CIP_ID|CIP_ID|Model Name",
        "TableStart:[ Program 2 ]\nCIP_ID|Model Name",
        "TableStart:[ Program 1 ]\nCIP_ID|Model Name\nTableEnd:\nTableStart:[ Program 2 ]\nCIP_ID|Model Name",
    ] {
        assert!(program_rows(raw, 1).is_err(), "accepted {raw:?}");
    }
    for slot in [0, 11, 255] {
        assert!(program_rows("CIP_ID|Model Name", slot).is_err());
    }
}

#[test]
fn tokens_are_validated_without_command_normalization() {
    assert_eq!(ipid_key("0a").unwrap(), "A");
    assert_eq!(ipid_key("11").unwrap(), "11");
    for bad in ["", "0x11", "111", "11\nreboot", "11;reboot", " 11"] {
        assert!(ipid_key(bad).is_err());
    }
    assert_eq!(address_key("127.000.000.001").unwrap(), "127.0.0.1");
    assert_eq!(address_key("Rack-Cp4.").unwrap(), "rack-cp4");
    for bad in [
        "",
        "-bad",
        "rack cp4",
        "x;reboot",
        "x\nreboot",
        "999.1.1.1",
        "1.2.3.4:22",
    ] {
        assert!(address_key(bad).is_err());
    }
}

#[test]
fn master_address_never_uses_display_label() {
    let mut processor = device("192.0.2.10", "CP4", "rack-cp4", true);
    assert_eq!(
        master_address(&processor, AddressMode::Ip).unwrap(),
        "192.0.2.10"
    );
    assert_eq!(
        master_address(&processor, AddressMode::Hostname).unwrap(),
        "rack-cp4"
    );
    processor.discovered = None;
    assert!(master_address(&processor, AddressMode::Hostname).is_err());
    processor.host = "configured-cp4.example.test".into();
    assert_eq!(
        master_address(&processor, AddressMode::Hostname).unwrap(),
        processor.host
    );
    assert!(master_address(&processor, AddressMode::Ip).is_err());
}

fn table(rows: &str) -> String {
    format!("CIP_ID|IP Address/SiteName\n{rows}")
}

fn run_replace(
    ipid: &str,
    master: &str,
    responses: Vec<Result<String, String>>,
) -> (Result<String, String>, Vec<String>) {
    let mut replies: std::collections::VecDeque<_> = responses.into();
    let mut commands = Vec::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(replace(ipid, master, |command| {
        commands.push(command);
        std::future::ready(replies.pop_front().expect("unexpected command"))
    }));
    assert!(replies.is_empty(), "missing expected commands");
    (result, commands)
}

#[test]
fn replacement_verifies_every_mutation_and_preserves_other_ids() {
    let (result, commands) = run_replace(
        "11",
        "192.0.2.10",
        vec![
            Ok(table("11|old-master\n22|other-master")),
            Ok(String::new()),
            Ok(table("22|other-master")),
            Ok(String::new()),
            Ok(table("11|192.0.2.10\n22|other-master")),
        ],
    );
    assert_eq!(result.unwrap(), "Verified");
    assert_eq!(
        commands,
        [
            "ipt -t",
            "remmaster 11 old-master",
            "ipt -t",
            "addmaster 11 192.0.2.10",
            "ipt -t"
        ]
    );
}

#[test]
fn replacement_stops_after_unverified_removal_or_concurrent_changes() {
    for after in [
        table("11|old-master\n22|other-master"),
        table("22|changed"),
        table("11|unexpected-master\n22|other-master"),
        "malformed".into(),
    ] {
        let (result, commands) = run_replace(
            "11",
            "192.0.2.10",
            vec![
                Ok(table("11|old-master\n22|other-master")),
                Ok(String::new()),
                Ok(after),
            ],
        );
        assert!(result.is_err());
        assert_eq!(commands, ["ipt -t", "remmaster 11 old-master", "ipt -t"]);
    }
}

#[test]
fn replacement_removes_multiple_masters_once_and_keeps_exact_tokens() {
    let (result, commands) = run_replace(
        "0a",
        "Rack-Cp4",
        vec![
            Ok(table("A|Old-One\n0a|old-two\n0A|old-two\n22|other")),
            Ok(String::new()),
            Ok(table("0a|old-two\n0A|old-two\n22|other")),
            Ok(String::new()),
            Ok(table("22|other")),
            Ok(String::new()),
            Ok(table("A|rack-cp4\n22|other")),
        ],
    );
    assert_eq!(result.unwrap(), "Verified");
    assert_eq!(
        commands,
        [
            "ipt -t",
            "remmaster A Old-One",
            "ipt -t",
            "remmaster 0a old-two",
            "ipt -t",
            "addmaster 0a Rack-Cp4",
            "ipt -t"
        ]
    );
}

#[test]
fn correct_mapping_is_a_read_only_noop() {
    let (result, commands) = run_replace(
        "11",
        "192.0.2.10",
        vec![Ok(table("11|192.000.002.010\n22|other"))],
    );
    assert_eq!(result.unwrap(), "Already correct");
    assert_eq!(commands, ["ipt -t"]);
}

#[test]
fn unknown_tables_and_invalid_arguments_never_write() {
    for raw in [
        "",
        "unknown output",
        "CIP_ID|Address",
        "CIP_ID|IP Address/SiteName\n11|",
    ] {
        let (result, commands) = run_replace("11", "192.0.2.10", vec![Ok(raw.into())]);
        assert!(result.is_err());
        assert_eq!(commands, ["ipt -t"]);
    }
    let (result, commands) = run_replace("11;reboot", "192.0.2.10", vec![]);
    assert!(result.is_err());
    assert!(commands.is_empty());
}

#[test]
fn add_is_not_success_until_verified() {
    for raw in [
        table(""),
        table("11|wrong-master"),
        table("11|rack\n11|rack"),
        "bad".into(),
    ] {
        let (result, commands) = run_replace(
            "11",
            "rack",
            vec![Ok(table("")), Ok(String::new()), Ok(raw)],
        );
        assert!(result.is_err());
        assert_eq!(commands, ["ipt -t", "addmaster 11 rack", "ipt -t"]);
    }
}
