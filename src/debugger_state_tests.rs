use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestFile(PathBuf);
impl TestFile {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clr-debug-state-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir.join("debugger-state.json"))
    }
}
impl Drop for TestFile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0.parent().unwrap());
    }
}
fn signal(signum: u32, name: &str) -> Signal {
    let mut metadata = [0; 6];
    metadata[..4].copy_from_slice(&signum.to_le_bytes());
    Signal {
        name: name.into(),
        metadata,
    }
}
fn panel(file: &TestFile, name: &str, slot: u8, signals: Vec<Signal>) -> Panel {
    let mut panel = Panel::new("address-not-the-key".into(), name);
    panel.persistence.path = Some(file.0.clone());
    panel.slot = slot;
    panel.signals = signals;
    panel.restore_settings();
    assert!(
        panel.persistence.error.is_none(),
        "{:?}",
        panel.persistence.error
    );
    panel
}

#[test]
fn settings_round_trip_and_exclusions_wait_for_all_initial_values() {
    let file = TestFile::new();
    let signals = vec![
        signal(1, "Alpha"),
        signal(2, "Bravo"),
        signal(1, "Alpha alias"),
    ];
    let mut first = panel(&file, "Room processor", 3, signals.clone());
    first.disabled_signals.insert(1);
    first.groups.insert("Audio".into(), vec![2, 1]);
    first.groups.insert("Empty".into(), vec![]);
    first.stop();
    let document = Document::load(&file.0).unwrap();
    assert_eq!(
        document.processors["Room processor"][&3].disabled_signals,
        [SignalRef {
            signum: 1,
            name: "Alpha".into()
        }]
    );
    let mut restored = panel(&file, "Room processor", 3, signals);
    assert_eq!(restored.groups["Audio"], [2, 1]);
    assert!(restored.groups["Empty"].is_empty());
    assert!(restored.disabled_signals.is_empty());
    assert_eq!(restored.persistence.pending_disabled, HashSet::from([1]));
    let at = SystemTime::now();
    restored.ingest(b"\x0100000001:1=initial\r\n\x03", at);
    assert!(restored.disabled_signals.is_empty());
    // An interrupted sync must not erase pending exclusions on disk.
    restored.stop();
    assert_eq!(
        Document::load(&file.0).unwrap().processors["Room processor"][&3]
            .disabled_signals
            .len(),
        1
    );
    restored.ingest(b"\x0100000002:1=0\r\n\x03", at);
    assert!(restored.disabled_signals.is_empty());
    restored.connect();
    let (_, output) = restored.pending_session.take().unwrap();
    output.try_send(DebugEvent::SyncComplete).unwrap();
    restored.poll();
    assert_eq!(restored.disabled_signals, HashSet::from([1]));
    assert!(restored.persistence.pending_disabled.is_empty());
    assert_eq!(restored.samples.len(), 2);
    restored.ingest(b"\x0100000001:1=updated\r\n\x03", at);
    assert_eq!(restored.samples.len(), 2);
    assert_eq!(restored.values[&1], "updated");
    // Removing members / enabling signals is persisted, including empty groups.
    restored.disabled_signals.clear();
    restored.groups.get_mut("Audio").unwrap().clear();
    restored.stop();
    let reopened = panel(
        &file,
        "Room processor",
        3,
        vec![signal(1, "Alpha"), signal(2, "Bravo")],
    );
    assert!(reopened.persistence.pending_disabled.is_empty());
    assert!(reopened.groups["Audio"].is_empty());
}

#[test]
fn exclusions_restore_after_sync_even_when_a_module_never_reports() {
    let file = TestFile::new();
    let signals = vec![signal(1, "Alpha"), signal(2, "::module")];
    let mut first = panel(&file, "Processor", 1, signals.clone());
    first.disabled_signals.insert(1);
    first.stop();
    let mut restored = panel(&file, "Processor", 1, signals);
    restored.connect();
    let (_, output) = restored.pending_session.take().unwrap();
    output
        .try_send(DebugEvent::Output {
            bytes: b"\x0100000001:1=initial\r\n\x03".to_vec(),
            received_at: SystemTime::now(),
        })
        .unwrap();
    restored.poll();
    assert!(restored.disabled_signals.is_empty());
    output.try_send(DebugEvent::SyncComplete).unwrap();
    output
        .try_send(DebugEvent::Output {
            bytes: b"\x0100000001:1=after\r\n\x03".to_vec(),
            received_at: SystemTime::now(),
        })
        .unwrap();
    restored.poll();
    assert_eq!(restored.disabled_signals, HashSet::from([1]));
    assert_eq!(restored.samples.len(), 1);
    assert_eq!(restored.values[&1], "after");
    assert!(!restored.values.contains_key(&2));
    assert!(restored.persistence.pending_disabled.is_empty());
}

#[test]
fn inventory_remaps_unique_names_rejects_recycled_ids_and_ambiguous_names() {
    let file = TestFile::new();
    let mut old = panel(
        &file,
        "Processor",
        1,
        vec![
            signal(1, "Moved"),
            signal(2, "Gone"),
            signal(3, "Duplicate"),
            signal(4, "Unchanged"),
        ],
    );
    old.disabled_signals.extend([1, 2, 3, 4]);
    old.groups.insert("Test".into(), vec![1, 1, 2, 3, 4]);
    old.stop();
    let restored = panel(
        &file,
        "Processor",
        1,
        vec![
            signal(1, "Unrelated"),
            signal(10, "Moved"),
            signal(8, "Duplicate"),
            signal(9, "Duplicate"),
            signal(4, "Unchanged"),
        ],
    );
    assert_eq!(restored.groups["Test"], [10, 4]);
    assert_eq!(
        restored.persistence.pending_disabled,
        HashSet::from([10, 4])
    );
    assert_eq!(
        restored.persistence.notice,
        "Settings inventory: 4 checked; 1 remapped; 1 missing; 1 ambiguous"
    );
    assert!(restored.persistence.details.contains("Missing: Gone"));
    assert!(
        restored
            .persistence
            .details
            .contains("Ambiguous: Duplicate")
    );
    let cleaned = Document::load(&file.0).unwrap();
    assert_eq!(
        cleaned.processors["Processor"][&1].disabled_signals.len(),
        2
    );
    assert_eq!(
        cleaned.processors["Processor"][&1].watch_groups["Test"].len(),
        2
    );
}

#[test]
fn processors_and_slots_are_isolated_and_slot_switch_saves_old_slot() {
    let file = TestFile::new();
    let signals = vec![signal(1, "Alpha")];
    let mut one = panel(&file, "Processor A", 1, signals.clone());
    one.disabled_signals.insert(1);
    one.groups.insert("First slot".into(), vec![1]);
    // UI selects the next slot before reset is called; save to the active slot.
    one.slot = 2;
    one.reset_signals();
    one.signals = signals.clone();
    one.restore_settings();
    assert!(!one.groups.contains_key("First slot"));
    one.groups.insert("Second slot".into(), vec![1]);
    one.stop();
    let mut other = panel(&file, "Processor B", 1, signals.clone());
    other.groups.insert("Other processor".into(), vec![1]);
    other.stop();
    let doc = Document::load(&file.0).unwrap();
    assert_eq!(doc.processors.len(), 2);
    assert_eq!(doc.processors["Processor A"].len(), 2);
    assert_eq!(doc.processors["Processor A"][&1].disabled_signals.len(), 1);
    assert!(
        doc.processors["Processor A"][&2]
            .disabled_signals
            .is_empty()
    );
    assert!(
        doc.processors["Processor B"][&1]
            .watch_groups
            .contains_key("Other processor")
    );
    // Empty signature on a failed fetch/reset must not erase saved preferences.
    other.reset_signals();
    other.stop();
    assert_eq!(Document::load(&file.0).unwrap(), doc);
}

#[test]
fn malformed_and_future_files_are_reported_and_never_overwritten() {
    let file = TestFile::new();
    for content in ["not json", "{\"version\":2,\"processors\":{}}"] {
        fs::write(&file.0, content).unwrap();
        let mut panel = Panel::new("id".into(), "Processor");
        panel.persistence.path = Some(file.0.clone());
        panel.signals = vec![signal(1, "Alpha")];
        panel.restore_settings();
        assert!(panel.persistence.error.is_some());
        panel.disabled_signals.insert(1);
        panel.stop();
        assert_eq!(fs::read_to_string(&file.0).unwrap(), content);
    }
}

#[test]
fn save_failure_is_visible_and_preserves_previous_file() {
    let file = TestFile::new();
    let mut panel = panel(&file, "Processor", 1, vec![signal(1, "Alpha")]);
    let original = fs::read(&file.0).unwrap();
    fs::create_dir(file.0.with_extension("json.tmp")).unwrap();
    panel.disabled_signals.insert(1);
    panel.stop();
    assert!(
        panel
            .persistence
            .error
            .as_ref()
            .unwrap()
            .contains("Could not save")
    );
    assert_eq!(fs::read(&file.0).unwrap(), original);
    fs::remove_dir(file.0.with_extension("json.tmp")).unwrap();
    panel.persistence.last_attempt = None;
    panel.save_settings();
    assert!(panel.persistence.error.is_none());
    assert_eq!(
        Document::load(&file.0).unwrap().processors["Processor"][&1]
            .disabled_signals
            .len(),
        1
    );
}

#[test]
fn signature_fetch_restores_settings_and_ui_changes_autosave() {
    let file = TestFile::new();
    let mut first = panel(&file, "Processor", 1, vec![signal(1, "Alpha")]);
    first.groups.insert("Saved".into(), vec![1]);
    first.disabled_signals.insert(1);
    first.stop();
    let mut restored = Panel::new("new-address".into(), "Processor");
    restored.persistence.path = Some(file.0.clone());
    let mut signature = b"[LOGOSSIG001.000]".to_vec();
    let name: Vec<u8> = "Alpha".encode_utf16().flat_map(u16::to_le_bytes).collect();
    signature.extend_from_slice(&((name.len() + 8) as u16).to_le_bytes());
    signature.extend(name);
    signature.extend(signal(1, "Alpha").metadata);
    let (sender, receiver) = std::sync::mpsc::channel();
    restored.fetch = Some(receiver);
    sender.send(Ok(signature)).unwrap();
    restored.poll();
    assert_eq!(restored.groups["Saved"], [1]);
    assert!(restored.disabled_signals.is_empty());
    assert!(restored.pending_session.is_some());
    restored.groups.insert("New group".into(), vec![1]);
    let ctx = egui::Context::default();
    ctx.run_ui(egui::RawInput::default(), |ui| restored.contents(ui))
        .drop_without_applying_deltas();
    let doc = Document::load(&file.0).unwrap();
    assert!(
        doc.processors["Processor"][&1]
            .watch_groups
            .contains_key("New group")
    );
    assert_eq!(doc.processors["Processor"][&1].disabled_signals.len(), 1);
}
