use super::*;
use crate::ipid_assignment::Panel;

fn app() -> LoadRunnerApp {
    let mut app = LoadRunnerApp::from_parts(Preferences::default(), None, Vec::new(), None);
    // Opening the editor must never scan; tests start discovery explicitly.
    app.discovery_spawner = |_| panic!("discovery must be requested explicitly");
    app.devices.push(Device::from_address(&AddressEntry {
        host: "192.0.2.10".into(),
        kind: DeviceKind::Processor,
        model: "CP4".into(),
        ..Default::default()
    }));
    app.devices[0].selected = true;
    let packet = b"\x15\0\0\0panel-one\0TS-770 [v1.0] @E-001122334455\0";
    app.merge_discovered(crate::discovery::parse_response(packet, "192.0.2.20".into()).unwrap());
    app
}

#[test]
fn opening_ipids_offers_address_book_devices_without_discovering() {
    let mut app = app();
    app.devices.push(Device::from_address(&AddressEntry {
        host: "192.0.2.30".into(),
        model: "TSW-1070".into(),
        ..Default::default()
    }));
    app.open_ipid_assignment(None);
    assert!(!app.discovering);
    let panel = app.ipid_assignment.as_mut().unwrap();
    assert!(!panel.discovering);
    let hosts: Vec<_> = panel.candidates.iter().map(|d| d.host.as_str()).collect();
    assert_eq!(hosts, ["192.0.2.20", "192.0.2.30"]);
    panel.accept_table(Ok("CIP_ID|Model Name\n11|TSW-1070".into()));
    panel.rows[0].selected = Some("192.0.2.30:22".into());
    let jobs = panel.jobs().unwrap();
    panel.validate_live(&app.devices, &jobs).unwrap();
}

#[test]
fn discover_action_streams_results_into_choices_and_merges_book_entries() {
    let mut app = app();
    app.devices.retain(|d| d.kind == DeviceKind::Processor);
    app.devices.push(Device::from_address(&AddressEntry {
        host: "192.0.2.21".into(),
        name: "Lobby".into(),
        ..Default::default()
    }));
    app.open_ipid_assignment(None);
    assert_eq!(app.ipid_assignment.as_ref().unwrap().candidates.len(), 1);
    app.discovery_spawner = |sender| {
        for (packet, ip) in [
            (
                &b"\x15\0\0\0new-panel\0TS-1070 [v1.0] @E-001122334466\0"[..],
                "192.0.2.21",
            ),
            (
                &b"\x15\0\0\0panel-two\0TS-770 [v1.0] @E-001122334477\0"[..],
                "192.0.2.22",
            ),
        ] {
            sender
                .send(DiscoveryEvent::Found(Box::new(
                    crate::discovery::parse_response(packet, ip.into()).unwrap(),
                )))
                .unwrap();
        }
    };
    app.start_discovery();
    assert!(app.ipid_assignment.as_ref().unwrap().discovering);
    app.discovery_spawner = |_| panic!("must reuse in-progress discovery");
    app.start_discovery();
    app.process_events();
    let panel = app.ipid_assignment.as_ref().unwrap();
    assert_eq!(
        panel.candidates.len(),
        2,
        "book entry and scan result merge"
    );
    assert!(panel.candidates.iter().all(|d| d.discovered.is_some()));
    app.discovery_sender
        .send(DiscoveryEvent::Finished(Ok(())))
        .unwrap();
    app.process_events();
    assert!(!app.ipid_assignment.as_ref().unwrap().discovering);
}

#[test]
fn discovery_enriches_processor_and_preserves_assignment_choices_and_snapshots() {
    let mut app = app();
    let (token, target) = loaded(&mut app);
    // Initial discovery of the processor must not stale an unassigned table.
    app.ipid_assignment.as_mut().unwrap().rows[0].selected = None;
    let packet = b"\x15\0\0\0rack-cp4\0CP4 [v1.0] @E-112233445566\0";
    app.discovery_sender
        .send(DiscoveryEvent::Found(Box::new(
            crate::discovery::parse_response(packet, "192.0.2.10".into()).unwrap(),
        )))
        .unwrap();
    app.process_events();
    let panel = app.ipid_assignment.as_mut().unwrap();
    panel.mode = crate::ipid_assignment::AddressMode::Hostname;
    panel.rows[0].selected = Some(target.clone());
    panel.rows[0].status = "Verified".into();
    assert_eq!(panel.jobs().unwrap()[0].master, "rack-cp4");
    let packet = b"\x15\0\0\0new-panel\0TS-1070 [v1.0] @E-001122334466\0";
    app.discovery_sender
        .send(DiscoveryEvent::Found(Box::new(
            crate::discovery::parse_response(packet, "192.0.2.21".into()).unwrap(),
        )))
        .unwrap();
    app.process_events();
    let panel = app.ipid_assignment.as_ref().unwrap();
    assert_eq!(panel.token, token);
    assert_eq!(panel.candidates.len(), 2);
    assert_eq!(panel.rows[0].selected.as_ref(), Some(&target));
    assert_eq!(panel.rows[0].status, "Verified");
    panel
        .validate_live(&app.devices, &panel.jobs().unwrap())
        .unwrap();
    // A selected peripheral moving to a new IP still requires explicit review.
    let packet = b"\x15\0\0\0panel-one\0TS-770 [v1.0] @E-001122334455\0";
    app.discovery_sender
        .send(DiscoveryEvent::Found(Box::new(
            crate::discovery::parse_response(packet, "192.0.2.99".into()).unwrap(),
        )))
        .unwrap();
    app.process_events();
    let panel = app.ipid_assignment.as_ref().unwrap();
    assert!(
        panel
            .validate_live(&app.devices, &panel.jobs().unwrap())
            .is_err()
    );
}

#[test]
fn failed_scan_can_be_retried_and_clear_discards_a_draining_scan() {
    let mut app = app();
    app.discovery_spawner = |_| {};
    app.open_ipid_assignment(None);
    app.start_discovery();
    app.discovery_sender
        .send(DiscoveryEvent::Finished(Err("no interface".into())))
        .unwrap();
    app.process_events();
    assert!(!app.discovering);
    assert!(app.notice.as_ref().unwrap().contains("no interface"));
    app.start_discovery();
    assert!(app.discovering);
    app.cancel_credential_prompt();
    app.clear_devices();
    assert!(app.discard_discovery_results);
    let packet = b"\x15\0\0\0panel-one\0TS-770 [v1.0] @E-001122334455\0";
    app.discovery_sender
        .send(DiscoveryEvent::Found(Box::new(
            crate::discovery::parse_response(packet, "192.0.2.20".into()).unwrap(),
        )))
        .unwrap();
    app.discovery_sender
        .send(DiscoveryEvent::Finished(Ok(())))
        .unwrap();
    app.process_events();
    assert!(!app.discovering);
    assert!(app.devices.is_empty(), "discarded scan results are dropped");
}

#[test]
fn invalid_processor_selection_does_not_open_the_editor() {
    let mut app = app();
    for device in &mut app.devices {
        device.selected = false;
    }
    app.open_ipid_assignment(None);
    assert!(!app.discovering);
    assert!(app.ipid_assignment.is_none());
}

fn loaded(app: &mut LoadRunnerApp) -> (u64, String) {
    let target = app
        .devices
        .iter()
        .find(|d| d.model == "TS-770")
        .unwrap()
        .id
        .clone();
    app.open_ipid_assignment(Some("192.0.2.10:22"));
    app.cancel_credential_prompt();
    let panel = app.ipid_assignment.as_mut().unwrap();
    panel.accept_table(Ok("CIP_ID|Model Name\n11|TS-770".into()));
    panel.rows[0].selected = Some(target.clone());
    (panel.token, target)
}

#[test]
fn entry_requires_exactly_one_processor_but_context_ignores_checkboxes() {
    let mut app = app();
    assert_eq!(app.ipid_processor(None).unwrap().host, "192.0.2.10");
    for device in &mut app.devices {
        device.selected = true;
    }
    assert!(app.ipid_processor(None).is_err());
    assert!(app.ipid_processor(Some("192.0.2.10:22")).is_ok());
    for device in &mut app.devices {
        device.selected = false;
    }
    assert!(app.ipid_processor(None).is_err());
    let processor = app.device_mut("192.0.2.10:22").unwrap();
    processor.model = "VC-4".into();
    assert!(app.ipid_processor(Some("192.0.2.10:22")).is_err());
}

#[test]
fn discovery_identity_survives_custom_labels_and_promotion() {
    let mut app = app();
    app.devices[0].name = "Custom rack label".into();
    let packet = b"\x15\0\0\0rack-cp4\0CP4 [v1.0] @E-112233445566\0";
    app.merge_discovered(crate::discovery::parse_response(packet, "192.0.2.10".into()).unwrap());
    let processor = app.devices.iter().find(|d| d.host == "192.0.2.10").unwrap();
    assert_eq!(processor.name, "Custom rack label");
    assert_eq!(processor.discovered.as_ref().unwrap().hostname, "rack-cp4");
    assert!(
        !serde_json::to_string(&processor.to_address_entry())
            .unwrap()
            .contains("discovered")
    );
    let target = app
        .devices
        .iter()
        .find(|d| d.host == "192.0.2.20")
        .unwrap()
        .id
        .clone();
    app.add_discovered_to_address_book(&target);
    app.open_ipid_assignment(Some("192.0.2.10:22"));
    let panel = app.ipid_assignment.as_ref().unwrap();
    assert_eq!(panel.candidates.len(), 1);
    assert_eq!(panel.candidates[0].source, DeviceSource::AddressBook);
}

#[test]
fn credentials_are_gated_once_cancelled_and_never_resumed_for_closed_windows() {
    let mut app = app();
    app.open_ipid_assignment(None);
    assert!(!app.worker_pool.has_pending());
    let token = app.ipid_assignment.as_ref().unwrap().token;
    assert!(
        matches!(app.credential_prompt.as_ref().unwrap().resume, Some(CredentialGate::ReadIpids(t)) if t == token)
    );
    app.cancel_credential_prompt();
    assert!(!app.ipid_assignment.as_ref().unwrap().busy());
    app.queue_ipid_table(token);
    app.ipid_assignment.as_mut().unwrap().open = false;
    let mut prompt = app.credential_prompt.take().unwrap();
    prompt.username = "test".into();
    prompt.password = "test".into();
    app.answer_credential_prompt(prompt);
    assert!(!app.worker_pool.has_pending());
    assert!(!app.ipid_assignment.as_ref().unwrap().busy());
}

#[test]
fn stale_credential_tokens_do_not_queue_a_new_window() {
    let mut app = app();
    app.open_ipid_assignment(None);
    let token = app.ipid_assignment.as_ref().unwrap().token;
    let prompt = app.credential_prompt.take().unwrap();
    let panel = app.ipid_assignment.as_ref().unwrap();
    app.ipid_assignment = Some(Panel::new(
        token + 1,
        panel.processor.clone(),
        panel.candidates.clone(),
    ));
    app.answer_credential_prompt(prompt);
    assert!(!app.worker_pool.has_pending());
    assert!(app.credential_prompt.is_none());
    assert!(!app.ipid_assignment.as_ref().unwrap().busy());
}

#[test]
fn batch_preflight_checks_all_devices_before_requesting_credentials_or_queueing() {
    let mut app = app();
    let (token, target) = loaded(&mut app);
    app.queue_ipid_assignments(token);
    assert!(
        matches!(app.credential_prompt.as_ref().unwrap().resume, Some(CredentialGate::AssignIpids(t)) if t == token)
    );
    assert!(!app.worker_pool.has_pending());
    app.cancel_credential_prompt();
    app.devices.retain(|d| d.id != target);
    app.queue_ipid_assignments(token);
    assert!(app.credential_prompt.is_none());
    assert!(!app.worker_pool.has_pending());
    assert!(
        app.ipid_assignment
            .as_ref()
            .unwrap()
            .message
            .contains("removed")
    );
}

#[test]
fn closed_running_window_retains_results_and_blocks_clear_and_quit() {
    let mut app = app();
    let (_, target) = loaded(&mut app);
    let (sender, receiver) = mpsc::channel();
    let panel = app.ipid_assignment.as_mut().unwrap();
    panel.rows[0].reply = Some(receiver);
    panel.open = false;
    app.clear_devices();
    assert!(!app.devices.is_empty());
    app.request_action(PendingAction::Quit, &egui::Context::default());
    assert!(!app.close_approved);
    app.apply_worker_event(WorkerEvent::Complete {
        id: target,
        message: "unrelated script".into(),
    });
    assert!(app.ipid_assignment.as_ref().unwrap().busy());
    app.open_ipid_assignment(None);
    assert!(app.ipid_assignment.as_ref().unwrap().open);
    sender.send(Ok("Verified".into())).unwrap();
    app.process_events();
    assert_eq!(
        app.ipid_assignment.as_ref().unwrap().rows[0].status,
        "Verified"
    );
    app.clear_devices();
    assert!(app.ipid_assignment.is_none());
}

#[test]
fn book_replacement_discards_idle_windows_only_on_success() {
    let mut app = app();
    loaded(&mut app);
    let dir = crate::test_support::TestDir::new();
    app.open_address_book(&dir.path().join("missing.json"));
    assert!(app.ipid_assignment.is_some());
    app.new_address_book();
    assert!(app.ipid_assignment.is_none());
}

#[test]
fn context_menu_assigns_only_clicked_processor_and_preserves_targets() {
    let mut app = app();
    let ctx = egui::Context::default();
    let id = app.devices[0].id.clone();
    app.devices[0].selected = false;
    app.devices[1].selected = true;
    let position = |output: &egui::FullOutput, label: &str| {
        output
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
            .unwrap_or_else(|| panic!("missing {label}"))
    };
    for _ in 0..2 {
        ctx.run_ui(input(), |ui| app.device_card(ui, &id))
            .drop_without_applying_deltas();
    }
    let output = ctx.run_ui(input(), |ui| app.device_card(ui, &id));
    let point = position(&output, "192.0.2.10") + egui::vec2(300.0, 20.0);
    output.drop_without_applying_deltas();
    let click = |app: &mut LoadRunnerApp, point, button| {
        for pressed in [true, false] {
            let mut raw = input();
            raw.events.push(egui::Event::PointerMoved(point));
            raw.events.push(egui::Event::PointerButton {
                pos: point,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            ctx.run_ui(raw, |ui| app.device_card(ui, &id))
                .drop_without_applying_deltas();
        }
    };
    click(&mut app, point, egui::PointerButton::Secondary);
    ctx.run_ui(input(), |ui| app.device_card(ui, &id))
        .drop_without_applying_deltas();
    let output = ctx.run_ui(input(), |ui| app.device_card(ui, &id));
    let point = position(&output, "Assign IPIDs…");
    output.drop_without_applying_deltas();
    click(&mut app, point, egui::PointerButton::Primary);
    assert_eq!(app.ipid_assignment.as_ref().unwrap().processor.id, id);
    assert!(!app.devices[0].selected);
    assert!(app.devices[1].selected);
    assert!(!app.worker_pool.has_pending());
}

#[test]
fn waiting_assignment_credentials_cancel_without_queueing_or_losing_choices() {
    let mut app = app();
    loaded(&mut app);
    let selected = app.ipid_assignment.as_ref().unwrap().rows[0]
        .selected
        .clone();
    app.queue_ipid_assignments(1);
    assert!(app.ipid_busy());
    assert!(matches!(
        app.credential_prompt.as_ref().unwrap().resume,
        Some(CredentialGate::AssignIpids(1))
    ));
    assert!(!app.worker_pool.has_pending());
    app.cancel_credential_prompt();
    assert!(!app.ipid_busy());
    assert_eq!(
        app.ipid_assignment.as_ref().unwrap().rows[0].selected,
        selected
    );
    app.queue_ipid_assignments(1);
    app.ipid_assignment.as_mut().unwrap().open = false;
    let mut prompt = app.credential_prompt.take().unwrap();
    prompt.username = "test".into();
    prompt.password = "test".into();
    app.answer_credential_prompt(prompt);
    assert!(!app.worker_pool.has_pending());
    assert!(!app.ipid_busy());
}

fn input() -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1800.0, 1100.0),
        )),
        ..Default::default()
    }
}

fn click_bar(app: &mut LoadRunnerApp, ctx: &egui::Context, label: &str) {
    for _ in 0..3 {
        ctx.run_ui(input(), |ui| app.action_bar(ui))
            .drop_without_applying_deltas();
    }
    let output = ctx.run_ui(input(), |ui| app.action_bar(ui));
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
        .expect("button label");
    output.drop_without_applying_deltas();
    for pressed in [true, false] {
        let mut raw = input();
        raw.events.push(egui::Event::PointerMoved(position));
        raw.events.push(egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        ctx.run_ui(raw, |ui| app.action_bar(ui))
            .drop_without_applying_deltas();
    }
}

#[test]
fn toolbar_button_is_gated_and_opens_the_assignment_window() {
    let mut app = app();
    let ctx = egui::Context::default();
    for device in &mut app.devices {
        device.selected = false;
    }
    click_bar(&mut app, &ctx, "Assign IPIDs");
    assert!(app.ipid_assignment.is_none());
    app.device_mut("192.0.2.10:22").unwrap().selected = true;
    click_bar(&mut app, &ctx, "Assign IPIDs");
    assert!(app.ipid_assignment.is_some());
    assert!(!app.worker_pool.has_pending());
}
