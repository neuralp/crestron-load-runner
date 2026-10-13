use super::*;

fn app() -> LoadRunnerApp {
    let mut app = LoadRunnerApp::from_parts(Preferences::default(), None, Vec::new(), None);
    app.discovery_spawner = |_| panic!("the debugger must not scan the network");
    app.devices.push(Device::from_address(&AddressEntry {
        host: "192.0.2.10".into(),
        kind: DeviceKind::Processor,
        model: "CP4".into(),
        ..Default::default()
    }));
    app.devices.push(Device::from_address(&AddressEntry {
        host: "192.0.2.20".into(),
        kind: DeviceKind::Touchpanel,
        ..Default::default()
    }));
    app
}

#[test]
fn debugger_targets_only_one_console_processor_without_enabling_signals() {
    let mut app = app();
    let id = app.devices[0].id.clone();
    app.devices[0].selected = true;
    app.open_debugger(None);
    assert!(app.debuggers.contains_key(&id));
    assert!(!app.debuggers[&id].awaiting_session);
    assert!(app.debuggers[&id].input.is_none());
    app.devices[1].selected = true;
    app.open_debugger(None);
    assert_eq!(app.debuggers.len(), 1);
    app.devices[0].selected = false;
    app.open_debugger(None);
    assert_eq!(app.debuggers.len(), 1);
    app.open_debugger(Some(&id));
    assert_eq!(app.debuggers.len(), 1);
}

#[test]
fn debugger_credentials_gate_the_remote_read_and_cancel_clears_start() {
    let mut app = app();
    let id = app.devices[0].id.clone();
    app.open_debugger(Some(&id));
    app.debuggers.get_mut(&id).unwrap().awaiting_session = true;
    app.start_debugger(&id);
    assert!(app.credential_prompt.is_some());
    assert!(app.debuggers[&id].fetch.is_none());
    assert!(!app.worker_pool.has_pending());
    app.cancel_credential_prompt();
    assert!(!app.debuggers[&id].awaiting_session);
}

#[test]
fn quit_waits_for_debugger_off_acknowledgement() {
    let mut app = app();
    let id = app.devices[0].id.clone();
    app.open_debugger(Some(&id));
    let panel = app.debuggers.get_mut(&id).unwrap();
    panel.session_sent = true;
    let (input, mut commands) = tokio::sync::mpsc::unbounded_channel();
    panel.input = Some(input);
    let ctx = egui::Context::default();
    app.execute_action(PendingAction::Quit, &ctx);
    assert!(!app.close_approved);
    assert_eq!(app.pending_debug_action, Some(PendingAction::Quit));
    assert!(app.debuggers[&id].stopping);
    assert!(matches!(
        commands.try_recv(),
        Ok(crate::ssh::DebugCommand::Stop)
    ));
}

#[test]
fn active_debugger_prevents_removing_or_clearing_its_processor() {
    let mut app = app();
    let id = app.devices[0].id.clone();
    app.open_debugger(Some(&id));
    app.debuggers.get_mut(&id).unwrap().session_sent = true;
    app.remove_devices(std::slice::from_ref(&id));
    assert!(app.devices.iter().any(|device| device.id == id));
    assert!(app.status_is_error);
    app.clear_devices();
    assert!(!app.devices.is_empty());
    assert!(app.status_is_error);
}
