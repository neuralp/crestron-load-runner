use super::*;
use crate::test_support::TestDir;

fn app() -> LoadRunnerApp {
    LoadRunnerApp::from_config(AppConfig::default(), None)
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
    assert_eq!(app.config.address_book.len(), 1);
    assert_eq!(
        app.config.address_book[0]
            .ssh_host_key_fingerprint
            .as_deref(),
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
    let mut config = AppConfig {
        default_username: "default-user".into(),
        default_password: "default-password".into(),
        ..Default::default()
    };
    config.address_book.push(AddressEntry {
        host: "192.0.2.1".into(),
        ssh_host_key_fingerprint: Some("SHA256:address-book-host-key".into()),
        ..Default::default()
    });
    let mut app = LoadRunnerApp::from_config(config, None);
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
    let config = AppConfig {
        address_book: vec![AddressEntry {
            host: "192.0.2.1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut app = LoadRunnerApp::from_config(config, None);
    let id = app.devices[0].id.clone();
    app.pending_host_keys
        .insert(id.clone(), "SHA256:new-host-key".into());

    app.trust_host_key(&id, "SHA256:new-host-key".into());

    assert_eq!(
        app.devices[0].ssh_host_key_fingerprint.as_deref(),
        Some("SHA256:new-host-key")
    );
    assert_eq!(
        app.config.address_book[0]
            .ssh_host_key_fingerprint
            .as_deref(),
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
    assert!(app.config.address_book.is_empty());
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
