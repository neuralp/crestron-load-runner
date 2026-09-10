use super::*;
use crate::storage::Preferences;
use crate::test_support::TestDir;

fn app() -> LoadRunnerApp {
    LoadRunnerApp::from_parts(Preferences::default(), None, Vec::new(), None)
}

/// An app whose preference writes land in `dir` instead of the real
/// configuration directory, which no test can relocate.
fn app_in(dir: &TestDir) -> LoadRunnerApp {
    LoadRunnerApp::from_parts(
        Preferences::default(),
        Some(dir.path().join("preferences.json")),
        Vec::new(),
        None,
    )
}

fn input() -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1360.0, 820.0),
        )),
        ..Default::default()
    }
}

#[test]
fn window_close_is_guarded_even_when_minimized() {
    let mut app = app();
    app.address_book_dirty = true;
    let ctx = egui::Context::default();
    let mut raw = input();
    let viewport = raw.viewports.entry(egui::ViewportId::ROOT).or_default();
    viewport.minimized = Some(true);
    viewport.events.push(egui::ViewportEvent::Close);
    let output = ctx.run_logic(&raw, |ctx| app.tick(ctx));
    assert_eq!(app.pending_action, Some(PendingAction::Quit));
    assert!(!app.close_approved);
    assert!(
        output.viewport_commands[&egui::ViewportId::ROOT]
            .contains(&egui::ViewportCommand::CancelClose)
    );
}

#[test]
fn unsaved_modal_renders_and_cancel_works_with_pointer_input() {
    let mut app = app();
    app.address_book_dirty = true;
    let ctx = egui::Context::default();
    app.request_action(PendingAction::Quit, &ctx);
    // The first frame measures and positions the modal.
    ctx.run_ui(input(), |ui| app.show(ui))
        .drop_without_applying_deltas();
    let output = ctx.run_ui(input(), |ui| app.show(ui));
    let cancel = output
        .shapes
        .iter()
        .find_map(|clipped| {
            if let egui::Shape::Text(text) = &clipped.shape
                && text.galley.text() == "Cancel"
            {
                Some(text.pos + text.galley.size() * 0.5)
            } else {
                None
            }
        })
        .expect("Cancel button was not rendered");
    output.drop_without_applying_deltas();
    for pressed in [true, false] {
        let mut raw = input();
        raw.events.push(egui::Event::PointerMoved(cancel));
        raw.events.push(egui::Event::PointerButton {
            pos: cancel,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        ctx.run_ui(raw, |ui| app.show(ui))
            .drop_without_applying_deltas();
    }
    assert!(app.pending_action.is_none());
    assert!(app.address_book_dirty);
    assert!(!app.close_approved);
}

fn discovered(host: &str) -> Device {
    let mut packet = vec![0x15, 0, 0, 0];
    packet.extend_from_slice(b"ROOM\0RMC3 [v1.0] @E-00107f112233\0");
    discovery::parse_response(&packet, host.into()).unwrap()
}

#[test]
fn firmware_models_come_from_discovery_and_survive_clearing_devices() {
    let dir = TestDir::new();
    let root = dir.path().join("firmware");
    let mut app = app();
    app.firmware_editor = crate::firmware::FirmwareEditor::load(Some(root.clone()));
    app.merge_discovered(discovered("192.0.2.1"));
    app.merge_discovered(discovered("192.0.2.2"));
    app.clear_devices();
    let ctx = egui::Context::default();
    let _ = ctx.run_logic(&input(), |ctx| app.tick(ctx));
    let catalog: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("catalog.json")).unwrap()).unwrap();
    assert_eq!(catalog["models"], serde_json::json!({"RMC3": null}));
    assert!(app.devices.is_empty());
    assert!(!app.address_book_dirty);
}

#[test]
fn assigned_firmware_is_queued_for_selected_matching_models() {
    let dir = TestDir::new();
    let root = dir.path().join("firmware");
    std::fs::create_dir_all(&root).unwrap();
    let checksum = "c3bf47ea1f4a4a605470313cacb3a44f4a461f68c6faeab07e737610cb5ac835";
    let stored_file = format!("{checksum}.firmware");
    std::fs::write(root.join(&stored_file), b"firmware").unwrap();
    std::fs::write(
        root.join("catalog.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "models": {
                "RMC3": {
                    "original_name": "rmc3_firmware.puf",
                    "stored_file": stored_file,
                    "bytes": 8,
                    "sha256": checksum
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let mut app = app();
    app.firmware_editor = crate::firmware::FirmwareEditor::load(Some(root));
    let mut entry = AddressEntry {
        host: "192.0.2.1".into(),
        model: "rmc3".into(),
        ..Default::default()
    };
    let mut device = Device::from_address(&entry);
    device.selected = true;
    let id = device.id.clone();
    app.devices.push(device);
    app.load_assigned_firmware();

    assert!(app.notice.is_none());
    assert!(app.worker_pool.is_busy(&id));

    entry.model = "unassigned".into();
    let mut unassigned = Device::from_address(&entry);
    unassigned.id = endpoint_id("192.0.2.2", 22);
    unassigned.host = "192.0.2.2".into();
    unassigned.selected = true;
    app.devices[0].selected = false;
    app.devices.push(unassigned);
    app.load_assigned_firmware();
    assert!(
        app.notice
            .as_deref()
            .is_some_and(|notice| notice.contains("assigned firmware"))
    );
}

#[test]
fn assigned_config_files_are_queued_and_missing_ones_are_reported() {
    let dir = TestDir::new();
    let config = dir.path().join("control-room.json");
    std::fs::write(&config, b"{}").unwrap();

    let mut app = app();
    let mut entry = AddressEntry {
        host: "192.0.2.1".into(),
        model: "RMC3".into(),
        kind: DeviceKind::Processor,
        ..Default::default()
    };
    let mut device = Device::from_address(&entry);
    device.selected = true;
    let id = device.id.clone();
    app.devices.push(device);

    // Nothing assigned yet.
    app.load_assigned_configs();
    assert!(
        app.notice
            .take()
            .is_some_and(|notice| notice.contains("assigned configuration file"))
    );
    assert!(!app.worker_pool.is_busy(&id));

    app.devices[0].config_slots[3] = Some(config);
    app.load_assigned_configs();
    assert!(app.notice.is_none());
    assert!(app.worker_pool.is_busy(&id));

    entry.host = "192.0.2.2".into();
    let mut missing = Device::from_address(&entry);
    missing.id = endpoint_id("192.0.2.2", 22);
    missing.config_slots[0] = Some(dir.path().join("deleted.json"));
    missing.selected = true;
    app.devices[0].selected = false;
    app.devices.push(missing);
    app.load_assigned_configs();
    assert!(
        app.notice
            .as_deref()
            .is_some_and(|notice| notice.contains("configuration file is missing"))
    );
}

#[test]
fn forgetting_a_host_key_clears_it_and_restores_trust_on_first_use() {
    let mut app = app();
    let entry = AddressEntry {
        host: "192.0.2.1".into(),
        ssh_host_key_fingerprint: Some("SHA256:stored-rsa-key".into()),
        ..Default::default()
    };
    app.devices.push(Device::from_address(&entry));
    let id = app.devices[0].id.clone();
    assert_eq!(
        app.connection_spec(&id)
            .unwrap()
            .trusted_fingerprint
            .as_deref(),
        Some("SHA256:stored-rsa-key")
    );

    app.forget_host_key(&id);
    assert!(app.devices[0].ssh_host_key_fingerprint.is_none());
    assert!(
        app.connection_spec(&id)
            .unwrap()
            .trusted_fingerprint
            .is_none()
    );
    assert!(app.address_book_dirty);
    assert!(app.status_message.contains("forgotten"));
    assert!(!app.status_is_error);

    // A second call reports rather than pretending something changed.
    app.address_book_dirty = false;
    app.forget_host_key(&id);
    assert!(!app.address_book_dirty);
    assert!(app.status_message.contains("no trusted SSH host key"));

    // An unknown key is then trusted again through the normal flow.
    app.trust_host_key(&id, "SHA256:new-ecdsa-key".into());
    assert_eq!(
        app.devices[0].ssh_host_key_fingerprint.as_deref(),
        Some("SHA256:new-ecdsa-key")
    );
}

#[test]
fn load_results_become_an_indicator_and_the_text_goes_to_the_log() {
    let mut app = app();
    let entry = AddressEntry {
        host: "192.0.2.1".into(),
        name: "W223-CP".into(),
        model: "RMC4".into(),
        kind: DeviceKind::Processor,
        ..Default::default()
    };
    app.devices.push(Device::from_address(&entry));
    let id = app.devices[0].id.clone();
    app.selected_id = Some(id.clone());

    // What the worker threads would report for one successful load.
    let events = [
        WorkerEvent::Log {
            id: id.clone(),
            direction: crate::device_log::Direction::Sent,
            text: "progload -p:1".into(),
        },
        WorkerEvent::Log {
            id: id.clone(),
            direction: crate::device_log::Direction::Received,
            text: "Program load complete".into(),
        },
        WorkerEvent::Complete {
            id: id.clone(),
            message: "Uploaded W classrooms.lpz: Program load complete".into(),
        },
    ];
    for event in events {
        app.apply_worker_event(event);
    }

    assert_eq!(app.devices[0].last_outcome, Some(Outcome::Succeeded));
    assert_eq!(app.device_log.len(), 2);

    // The card shows the indicator, not the load's output text.
    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text.contains("Succeeded")));
    assert!(
        !texts
            .iter()
            .any(|text| text.contains("Program load complete"))
    );

    // The log window shows both directions once opened.
    app.log_view_open = true;
    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text == "progload -p:1"));
    assert!(texts.iter().any(|text| text == "Program load complete"));
    assert!(texts.iter().any(|text| text.contains("W223-CP")));

    // A failure flips the indicator.
    app.apply_worker_event(WorkerEvent::Error {
        id,
        message: "Could not create remote file /program01/x.lpz".into(),
    });
    assert_eq!(app.devices[0].last_outcome, Some(Outcome::Failed));
    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text.contains("Failed")));
}

#[test]
fn program_signatures_are_found_beside_the_program() {
    let dir = TestDir::new();
    let program = dir.path().join("W classrooms.lpz");
    std::fs::write(&program, b"program").unwrap();

    // Nothing beside it yet.
    assert!(signature_beside(&program).is_none());

    // A directory of the right name is not a signature file.
    let signature = dir.path().join("W classrooms.sig");
    std::fs::create_dir(&signature).unwrap();
    assert!(signature_beside(&program).is_none());
    std::fs::remove_dir(&signature).unwrap();

    std::fs::write(&signature, b"signature").unwrap();
    assert_eq!(signature_beside(&program), Some(signature));

    // A signature for a different program is not picked up.
    let other = dir.path().join("other.lpz");
    std::fs::write(&other, b"program").unwrap();
    assert!(signature_beside(&other).is_none());
}

#[test]
fn a_missing_program_signature_is_recorded_and_does_not_block_the_load() {
    let dir = TestDir::new();
    let program = dir.path().join("W classrooms.lpz");
    std::fs::write(&program, b"program").unwrap();

    let mut app = app();
    let mut entry = AddressEntry {
        host: "192.0.2.1".into(),
        model: "RMC4".into(),
        kind: DeviceKind::Processor,
        ..Default::default()
    };
    entry.program_slots[0] = Some(program.clone());
    let mut device = Device::from_address(&entry);
    device.selected = true;
    let id = device.id.clone();
    app.devices.push(device);

    app.load_assigned_programs();
    assert!(app.notice.is_none());
    assert!(app.worker_pool.is_busy(&id));
    assert!(app.device_log.entries().any(|entry| {
        entry
            .text
            .contains("No signature file beside W classrooms.lpz")
    }));

    // With the signature present, nothing is noted.
    std::fs::write(dir.path().join("W classrooms.sig"), b"signature").unwrap();
    app.device_log.clear();
    app.load_assigned_programs();
    assert!(app.notice.is_none());
    assert!(
        !app.device_log
            .entries()
            .any(|entry| entry.text.contains("No signature file"))
    );
}

#[test]
fn manual_add_promotes_discovered_endpoint_instead_of_duplicating_it() {
    let mut app = app();
    app.merge_discovered(discovered("192.0.2.1"));
    app.devices[0].ssh_host_key_fingerprint = Some("SHA256:discovered-host-key".into());
    app.address_draft.host = "192.0.2.1".into();
    app.address_draft.name = "My room".into();
    app.address_draft.username = "admin".into();
    app.add_address();
    assert_eq!(app.devices.len(), 1);
    assert_eq!(app.devices[0].source, DeviceSource::AddressBook);
    assert_eq!(app.devices[0].id, "192.0.2.1:22");
    assert_eq!(app.devices[0].name, "My room");
    assert_eq!(app.devices[0].model, "RMC3");
    assert_eq!(app.devices[0].credentials.username, "admin");
    assert_eq!(app.address_book.len(), 1);
    assert_eq!(
        app.address_book[0].ssh_host_key_fingerprint.as_deref(),
        Some("SHA256:discovered-host-key")
    );
    assert!(app.address_book_dirty);
    app.address_draft.host = "192.0.2.1".into();
    app.add_address();
    assert_eq!(app.devices.len(), 1);
}

#[test]
fn promotion_merges_existing_endpoint_without_losing_assignments() {
    let mut app = app();
    let mut entry = AddressEntry {
        host: "192.0.2.1".into(),
        ..Default::default()
    };
    entry.program_slots[2] = Some("room.lpz".into());
    app.devices.push(Device::from_address(&entry));
    let device = discovered("192.0.2.1");
    let id = device.id.clone();
    app.devices.push(device);
    app.add_discovered_to_address_book(&id);
    assert_eq!(app.devices.len(), 1);
    assert_eq!(app.devices[0].program_slots[2], Some("room.lpz".into()));
    assert_eq!(app.selected_id.as_deref(), Some("192.0.2.1:22"));
}

#[test]
fn rediscovery_updates_discovered_ip_but_preserves_manual_host() {
    let mut app = app();
    app.merge_discovered(discovered("192.0.2.1"));
    app.merge_discovered(discovered("192.0.2.2"));
    assert_eq!(app.devices.len(), 1);
    assert_eq!(app.devices[0].host, "192.0.2.2");
    let id = app.devices[0].id.clone();
    assert_eq!(app.connection_spec(&id).unwrap().host, "192.0.2.2");
    app.add_discovered_to_address_book(&id);
    app.devices[0].host = "room.example".into();
    app.devices[0].id = endpoint_id("room.example", 22);
    let mut response = discovered("room.example");
    response.name = "Network name".into();
    app.merge_discovered(response);
    assert_eq!(app.devices[0].host, "room.example");
    assert_eq!(app.devices[0].name, "ROOM");
}

#[test]
fn different_ssh_ports_are_distinct_endpoints() {
    let mut app = app();
    app.address_draft.host = "192.0.2.1".into();
    app.address_draft.port = 2222;
    app.add_address();
    app.merge_discovered(discovered("192.0.2.1"));
    assert_eq!(app.devices.len(), 2);
    assert!(app.devices.iter().any(|device| device.port == 22));
    assert!(app.devices.iter().any(|device| device.port == 2222));
}

#[test]
fn connection_uses_defaults_only_for_missing_credentials() {
    let preferences = Preferences {
        default_username: "default-user".into(),
        default_password: "default-password".into(),
        ..Default::default()
    };
    let address_book = vec![AddressEntry {
        host: "192.0.2.1".into(),
        ssh_host_key_fingerprint: Some("SHA256:address-book-host-key".into()),
        ..Default::default()
    }];
    let mut app = LoadRunnerApp::from_parts(preferences, None, address_book, None);
    let id = app.devices[0].id.clone();
    let connection = app.connection_spec(&id).unwrap();
    assert_eq!(
        connection.trusted_fingerprint.as_deref(),
        Some("SHA256:address-book-host-key")
    );
    let defaults = connection.credentials;
    assert_eq!(defaults.username, "default-user");
    assert_eq!(defaults.password, "default-password");

    app.devices[0].credentials.username = "device-user".into();
    let mixed = app.connection_spec(&id).unwrap().credentials;
    assert_eq!(mixed.username, "device-user");
    assert_eq!(mixed.password, "default-password");
}

#[test]
fn trusting_host_key_updates_the_device_address_book_entry() {
    let address_book = vec![AddressEntry {
        host: "192.0.2.1".into(),
        ..Default::default()
    }];
    let mut app = LoadRunnerApp::from_parts(Preferences::default(), None, address_book, None);
    let id = app.devices[0].id.clone();
    app.pending_host_keys
        .insert(id.clone(), "SHA256:new-host-key".into());

    app.trust_host_key(&id, "SHA256:new-host-key".into());

    assert_eq!(
        app.devices[0].ssh_host_key_fingerprint.as_deref(),
        Some("SHA256:new-host-key")
    );
    assert_eq!(
        app.address_book[0].ssh_host_key_fingerprint.as_deref(),
        Some("SHA256:new-host-key")
    );
    assert_eq!(
        app.connection_spec(&id)
            .unwrap()
            .trusted_fingerprint
            .as_deref(),
        Some("SHA256:new-host-key")
    );
    assert!(!app.pending_host_keys.contains_key(&id));
    assert!(app.address_book_dirty);
}

#[test]
fn clearing_devices_immediately_removes_every_source() {
    let mut app = app();
    app.address_draft.host = "room.example".into();
    app.add_address();
    app.address_book_dirty = false;
    app.merge_discovered(discovered("192.0.2.1"));
    let address_id = app
        .devices
        .iter()
        .find(|device| device.source == DeviceSource::AddressBook)
        .unwrap()
        .id
        .clone();
    app.refresh_device(&address_id);

    app.clear_devices();

    assert!(app.devices.is_empty());
    assert!(app.address_book.is_empty());
    assert!(app.selected_id.is_none());
    assert!(app.address_book_dirty);
    assert!(app.worker_pool.has_pending());

    app.discovering = true;
    app.clear_devices();
    app.discovery_sender
        .send(DiscoveryEvent::Found(Box::new(discovered("192.0.2.2"))))
        .unwrap();
    app.process_events();
    assert!(app.devices.is_empty());
}

#[test]
fn clearing_an_empty_list_does_not_create_an_unsaved_change() {
    let mut app = app();

    app.clear_devices();

    assert!(app.devices.is_empty());
    assert!(!app.address_book_dirty);
}

#[test]
fn dirty_open_and_quit_require_confirmation_and_cancel_preserves_edits() {
    let mut app = app();
    app.address_draft.host = "192.0.2.1".into();
    app.add_address();
    let ctx = egui::Context::default();
    let open = PendingAction::Open("other.json".into());
    app.request_action(open.clone(), &ctx);
    assert_eq!(app.pending_action, Some(open));
    assert_eq!(app.devices.len(), 1);
    assert!(app.current_address_book.is_none());
    app.pending_action = None; // Cancel button
    assert!(app.address_book_dirty);
    assert_eq!(app.devices.len(), 1);
    app.request_action(PendingAction::Quit, &ctx);
    assert_eq!(app.pending_action, Some(PendingAction::Quit));
    assert!(!app.close_approved);
    app.confirm_pending_action(false, &ctx);
    assert!(app.close_approved);
    assert!(app.pending_action.is_none());
}

#[test]
fn save_failure_keeps_confirmation_and_unsaved_changes() {
    let dir = TestDir::new();
    let mut app = app();
    app.current_address_book = Some(dir.path().join("missing/book.json"));
    app.address_book_dirty = true;
    let ctx = egui::Context::default();
    app.request_action(PendingAction::Quit, &ctx);
    app.confirm_pending_action(true, &ctx);
    assert_eq!(app.pending_action, Some(PendingAction::Quit));
    assert!(!app.close_approved);
    assert!(app.address_book_dirty);
    assert!(app.status_is_error);
    assert!(app.notice.is_none());
}

#[test]
fn invalid_import_does_not_replace_current_devices() {
    let dir = TestDir::new();
    let path = dir.path().join("duplicates.json");
    std::fs::write(
        &path,
        r#"[{"name":"A","host":"room"},{"name":"B","host":"ROOM"}]"#,
    )
    .unwrap();
    let mut app = app();
    app.address_draft.host = "192.0.2.1".into();
    app.add_address();
    app.open_address_book(&path);
    assert_eq!(app.devices.len(), 1);
    assert_eq!(app.devices[0].host, "192.0.2.1");
    assert!(app.address_book_dirty);
    assert!(app.status_is_error);
}

#[test]
fn queued_jobs_block_remove_open_quit_and_promotion_until_drained() {
    let mut app = app();
    app.merge_discovered(discovered("192.0.2.1"));
    let discovered_id = app.devices[0].id.clone();
    app.add_discovered_to_address_book(&discovered_id);
    let id = app.devices[0].id.clone();
    // Empty credentials cause deterministic worker errors without network access.
    app.refresh_device(&id);
    app.refresh_device(&id);
    assert!(app.worker_pool.is_busy(&id));
    app.remove_selected_address();
    assert_eq!(app.devices.len(), 1);
    let ctx = egui::Context::default();
    app.request_action(PendingAction::Quit, &ctx);
    assert!(app.pending_action.is_none());
    assert!(!app.close_approved);
    app.open_address_book(Path::new("not-opened.json"));
    assert_eq!(app.devices.len(), 1);
    assert!(app.status_message.contains("operations"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while app.worker_pool.has_pending() {
        app.process_events();
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    app.remove_selected_address();
    assert!(app.devices.is_empty());

    app.merge_discovered(discovered("192.0.2.2"));
    let id = app.devices[0].id.clone();
    app.refresh_device(&id);
    app.add_discovered_to_address_book(&id);
    assert_eq!(app.devices[0].source, DeviceSource::Discovered);
    app.merge_discovered(discovered("192.0.2.3"));
    assert_eq!(app.devices[0].host, "192.0.2.2");
}

fn searchable(host: &str, name: &str, model: &str, firmware: &str, mac: &str) -> Device {
    let mut device = discovered(host);
    device.id = format!("E-{}", mac.replace(':', "").to_ascii_lowercase());
    device.name = name.into();
    device.model = model.into();
    device.firmware = firmware.into();
    device.mac = mac.into();
    device
}

fn rendered_texts(app: &mut LoadRunnerApp) -> Vec<String> {
    let ctx = egui::Context::default();
    // The first frame measures and positions the panels.
    ctx.run_ui(input(), |ui| app.show(ui))
        .drop_without_applying_deltas();
    let output = ctx.run_ui(input(), |ui| app.show(ui));
    let texts = output
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        })
        .collect();
    output.drop_without_applying_deltas();
    texts
}

#[test]
fn touchpanel_cards_show_the_project_in_a_slot_instead_of_a_button() {
    let mut app = app();
    let entry = AddressEntry {
        host: "192.0.2.41".into(),
        name: "LOBBY-TSW".into(),
        model: "TSW-1070".into(),
        kind: DeviceKind::Touchpanel,
        touchpanel_project: Some(PathBuf::from("projects/lobby.vtz")),
        ..Default::default()
    };
    app.devices.push(Device::from_address(&entry));

    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text == "PROJECT"));
    assert!(texts.iter().any(|text| text == "lobby.vtz"));
    assert!(!texts.iter().any(|text| {
        text == "Replace touchpanel project…" || text == "Assign touchpanel project…"
    }));

    app.devices[0].touchpanel_project = None;
    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text == "PROJECT"));
    assert!(texts.iter().any(|text| text == "Unassigned"));
}

#[test]
fn search_matches_every_card_field_case_insensitively() {
    let device = searchable(
        "192.0.2.40",
        "LOBBY-TSW",
        "TSW-1070",
        "3.002.1063",
        "00:10:7F:11:22:33",
    );

    for query in ["", "   "] {
        assert!(
            SearchQuery::new(query).matches(&device),
            "an empty query must match everything, got a miss for {query:?}"
        );
    }
    for query in [
        "tsw",          // model
        "TSW-10",       // model, as typed
        "lobby",        // hostname
        "192.0.2",      // ip address
        "00:10:7f",     // mac as stored
        "00107f112233", // mac without separators
        "00-10-7F",     // mac with other separators
        "3.002",        // firmware
    ] {
        assert!(SearchQuery::new(query).matches(&device), "missed {query:?}");
    }
    for query in ["192.0.3", "ffffff", "lobbyy"] {
        assert!(
            !SearchQuery::new(query).matches(&device),
            "unexpected hit for {query:?}"
        );
    }
}

#[test]
fn a_query_containing_non_hex_characters_is_not_treated_as_a_mac() {
    let device = searchable(
        "192.0.2.40",
        "LOBBY",
        "TSW-1070",
        "3.0",
        "0C:03:00:00:00:00",
    );
    // "cp3" holds a non-hex character, so it must never reach the MAC comparison
    // even though the separator-stripped MAC would otherwise be a tempting target.
    assert!(!SearchQuery::new("cp3").matches(&device));
    // An all-hex query does reach it, and matches across the stored separators.
    assert!(SearchQuery::new("c030").matches(&device));
}

#[test]
fn search_narrows_the_visible_device_list() {
    let mut app = app();
    app.merge_discovered(searchable(
        "192.0.2.41",
        "LOBBY-TSW",
        "TSW-1070",
        "3.002.1063",
        "00:10:7F:11:22:33",
    ));
    app.merge_discovered(searchable(
        "192.0.2.42",
        "RACK-RMC",
        "RMC4",
        "2.001.0010",
        "00:10:7F:44:55:66",
    ));

    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text == "TSW-1070"));
    assert!(texts.iter().any(|text| text == "RMC4"));

    app.search = "rmc".into();
    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text == "RMC4"));
    assert!(!texts.iter().any(|text| text == "TSW-1070"));

    app.search = "192.0.2.41".into();
    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text == "TSW-1070"));
    assert!(!texts.iter().any(|text| text == "RMC4"));

    app.search = "no-such-device".into();
    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text == "No matching devices"));
}

#[test]
fn clicking_the_clear_glyph_inside_the_search_box_resets_the_filter() {
    let mut app = app();
    app.merge_discovered(searchable(
        "192.0.2.41",
        "LOBBY-TSW",
        "TSW-1070",
        "3.002.1063",
        "00:10:7F:11:22:33",
    ));
    app.search = "rmc".into();

    let ctx = egui::Context::default();
    // The first frame measures and positions the panels.
    ctx.run_ui(input(), |ui| app.show(ui))
        .drop_without_applying_deltas();
    let output = ctx.run_ui(input(), |ui| app.show(ui));
    let clear = output
        .shapes
        .iter()
        .find_map(|clipped| {
            if let egui::Shape::Text(text) = &clipped.shape
                && text.galley.text() == "✕"
            {
                Some(text.pos + text.galley.size() * 0.5)
            } else {
                None
            }
        })
        .expect("the clear glyph was not rendered while the search box held text");
    output.drop_without_applying_deltas();

    for pressed in [true, false] {
        let mut raw = input();
        raw.events.push(egui::Event::PointerMoved(clear));
        raw.events.push(egui::Event::PointerButton {
            pos: clear,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        ctx.run_ui(raw, |ui| app.show(ui))
            .drop_without_applying_deltas();
    }

    assert!(
        app.search.is_empty(),
        "clicking the glyph must clear the search"
    );
    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text == "TSW-1070"));
    assert!(!texts.iter().any(|text| text == "✕"));
}

fn recent_menu_texts(app: &mut LoadRunnerApp) -> Vec<String> {
    let ctx = egui::Context::default();
    // The first frame measures and positions the items.
    ctx.run_ui(input(), |ui| app.recent_menu(ui))
        .drop_without_applying_deltas();
    let output = ctx.run_ui(input(), |ui| app.recent_menu(ui));
    let texts = output
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        })
        .collect();
    output.drop_without_applying_deltas();
    texts
}

fn saved_book(app: &mut LoadRunnerApp, path: &Path, host: &str) {
    app.address_draft.host = host.into();
    app.add_address();
    assert!(app.write_address_book_to(path), "{}", app.status_message);
}

#[test]
fn saving_writes_the_chosen_file_and_adopts_it_as_the_current_one() {
    let dir = TestDir::new();
    let path = dir.path().join("book.json");
    let mut app = app_in(&dir);
    app.address_draft.host = "192.0.2.1".into();
    app.address_draft.name = "Control room".into();
    app.add_address();
    assert!(app.address_book_dirty);

    assert!(app.write_address_book_to(&path));

    assert_eq!(app.current_address_book.as_deref(), Some(path.as_path()));
    assert!(!app.address_book_dirty);
    assert!(!app.status_is_error);
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["devices"][0]["name"], "Control room");

    // With a file open, Save writes in place instead of asking for one.
    app.devices[0].credentials.username = "admin".into();
    app.mark_address_book_dirty();
    assert!(app.save_address_book());
    assert_eq!(app.current_address_book.as_deref(), Some(path.as_path()));
    assert_eq!(
        crate::storage::load_address_book(&path).unwrap()[0].username,
        "admin"
    );
}

#[test]
fn a_failed_save_leaves_the_current_address_book_unchanged() {
    let dir = TestDir::new();
    let book = dir.path().join("book.json");
    let mut app = app_in(&dir);
    saved_book(&mut app, &book, "192.0.2.1");

    app.address_draft.host = "192.0.2.2".into();
    app.add_address();
    assert!(!app.write_address_book_to(&dir.path().join("missing/book.json")));

    assert_eq!(app.current_address_book.as_deref(), Some(book.as_path()));
    assert!(app.address_book_dirty);
    assert!(app.status_is_error);
    assert_eq!(crate::storage::load_address_book(&book).unwrap().len(), 1);
}

#[test]
fn an_untitled_address_book_shows_as_untitled_until_it_is_saved() {
    let dir = TestDir::new();
    let mut app = app_in(&dir);
    assert!(
        rendered_texts(&mut app)
            .iter()
            .any(|text| text == "Untitled")
    );

    app.address_draft.host = "192.0.2.1".into();
    app.add_address();
    assert!(
        rendered_texts(&mut app)
            .iter()
            .any(|text| text == "Untitled *")
    );

    assert!(app.write_address_book_to(&dir.path().join("book.json")));
    assert!(
        !rendered_texts(&mut app)
            .iter()
            .any(|text| text.starts_with("Untitled"))
    );
}

#[test]
fn a_new_address_book_keeps_discovered_devices_and_forgets_the_file() {
    let dir = TestDir::new();
    let path = dir.path().join("book.json");
    let mut app = app_in(&dir);
    saved_book(&mut app, &path, "192.0.2.1");
    app.merge_discovered(discovered("192.0.2.9"));

    app.new_address_book();

    assert_eq!(app.devices.len(), 1);
    assert_eq!(app.devices[0].source, DeviceSource::Discovered);
    assert!(app.address_book.is_empty());
    assert!(app.current_address_book.is_none());
    assert!(!app.address_book_dirty);
    // Starting a new book does not touch the one on disk.
    assert_eq!(crate::storage::load_address_book(&path).unwrap().len(), 1);
}

#[test]
fn starting_a_new_address_book_with_unsaved_changes_asks_first() {
    let mut app = app();
    app.address_draft.host = "192.0.2.1".into();
    app.add_address();
    let ctx = egui::Context::default();

    app.request_action(PendingAction::New, &ctx);

    assert_eq!(app.pending_action, Some(PendingAction::New));
    assert_eq!(app.devices.len(), 1);
    app.confirm_pending_action(false, &ctx);
    assert!(app.pending_action.is_none());
    assert!(app.devices.is_empty());
    assert!(app.current_address_book.is_none());
}

#[test]
fn saving_and_opening_record_the_file_in_the_recent_list() {
    let dir = TestDir::new();
    let first = dir.path().join("first.json");
    let second = dir.path().join("second.json");
    let mut app = app_in(&dir);
    saved_book(&mut app, &first, "192.0.2.1");
    assert!(app.write_address_book_to(&second));
    assert_eq!(
        app.preferences.recent_address_books,
        vec![second.clone(), first.clone()]
    );

    app.open_address_book(&first);

    assert_eq!(app.current_address_book.as_deref(), Some(first.as_path()));
    assert_eq!(
        app.preferences.recent_address_books,
        vec![first, second.clone()]
    );
    let stored = Preferences::load_from(&dir.path().join("preferences.json")).unwrap();
    assert_eq!(
        stored.recent_address_books,
        app.preferences.recent_address_books
    );
    assert!(stored.default_address_book.is_none());
}

#[test]
fn a_recent_address_book_that_has_been_deleted_is_reported_and_dropped_from_the_list() {
    let dir = TestDir::new();
    let path = dir.path().join("book.json");
    let mut app = app_in(&dir);
    saved_book(&mut app, &path, "192.0.2.1");
    std::fs::remove_file(&path).unwrap();

    app.open_address_book(&path);

    assert!(app.status_is_error);
    assert!(app.status_message.contains("Could not load"));
    assert!(app.preferences.recent_address_books.is_empty());
    let stored = Preferences::load_from(&dir.path().join("preferences.json")).unwrap();
    assert!(stored.recent_address_books.is_empty());
}

#[test]
fn a_malformed_recent_address_book_is_reported_but_stays_in_the_list() {
    let dir = TestDir::new();
    let path = dir.path().join("book.json");
    let mut app = app_in(&dir);
    saved_book(&mut app, &path, "192.0.2.1");
    std::fs::write(&path, "not json").unwrap();

    app.open_address_book(&path);

    assert!(app.status_is_error);
    // It can still be repaired, so it stays one click away.
    assert_eq!(app.preferences.recent_address_books, vec![path]);
    assert_eq!(app.devices.len(), 1);
}

#[test]
fn the_recent_menu_lists_the_five_most_recent_files_most_recent_first() {
    let dir = TestDir::new();
    let mut app = app_in(&dir);
    for index in 0..6 {
        crate::storage::remember_recent(
            &mut app.preferences.recent_address_books,
            &dir.path().join(format!("book{index}.json")),
        );
    }

    let texts = recent_menu_texts(&mut app);
    let listed: Vec<&String> = texts
        .iter()
        .filter(|text| text.starts_with("book"))
        .collect();
    assert_eq!(listed.len(), crate::storage::RECENT_ADDRESS_BOOKS);
    assert_eq!(listed[0], "book5.json");
    assert_eq!(listed[4], "book1.json");
    assert!(texts.iter().any(|text| text == "Clear recent list"));

    app.preferences.recent_address_books.clear();
    assert!(
        recent_menu_texts(&mut app)
            .iter()
            .any(|text| text == "No recent address books")
    );
}

#[test]
fn a_missing_default_address_book_is_reported_in_the_status_bar() {
    let dir = TestDir::new();
    let preferences = Preferences {
        startup: StartupBook::Specific,
        default_address_book: Some(dir.path().join("gone.json")),
        ..Default::default()
    };
    let (opened, error) = crate::storage::startup_address_book(&preferences);
    assert!(opened.is_none());

    let mut app = LoadRunnerApp::from_parts(preferences, None, Vec::new(), error);

    assert!(app.status_is_error);
    assert!(app.address_book.is_empty());
    let texts = rendered_texts(&mut app);
    assert!(texts.iter().any(|text| text.contains("gone.json")));
    assert!(texts.iter().any(|text| text == "Untitled"));
}

#[test]
fn choosing_a_default_address_book_does_not_lose_the_recent_list() {
    let dir = TestDir::new();
    let book = dir.path().join("book.json");
    let mut app = app_in(&dir);
    saved_book(&mut app, &book, "192.0.2.1");
    assert_eq!(app.preferences.recent_address_books, vec![book.clone()]);

    app.open_preferences();
    app.preferences_draft.startup = StartupBook::Specific;
    app.preferences_draft.default_address_book = Some(book.clone());
    app.preferences_draft.default_username = "admin".into();
    app.save_preferences();

    assert!(app.notice.is_none());
    assert!(!app.preferences_open);
    let stored = Preferences::load_from(&dir.path().join("preferences.json")).unwrap();
    assert_eq!(stored.recent_address_books, vec![book.clone()]);
    assert_eq!(stored.default_address_book, Some(book));
    assert_eq!(stored.default_username, "admin");
    assert_eq!(stored.startup, StartupBook::Specific);

    // A file that is not there cannot be the one opened at startup.
    app.open_preferences();
    app.preferences_draft.startup = StartupBook::Specific;
    app.preferences_draft.default_address_book = Some(dir.path().join("gone.json"));
    app.save_preferences();
    assert!(app.notice.is_some());
    assert!(app.preferences_open);
}
