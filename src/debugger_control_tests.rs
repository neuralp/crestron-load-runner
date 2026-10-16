use super::*;

fn signal(number: u8, kind: u8) -> Signal {
    Signal {
        name: format!("Signal {number}"),
        metadata: [number, 0, 0, 0, kind, 1],
    }
}

fn connected() -> (
    Panel,
    tokio::sync::mpsc::UnboundedReceiver<DebugCommand>,
    tokio::sync::mpsc::Sender<DebugEvent>,
) {
    let mut panel = Panel::new("control-test".into(), "Control test");
    panel.signals = vec![
        signal(1, 0),
        signal(2, 1),
        signal(3, 2),
        signal(4, 0x80),
        signal(5, 99),
    ];
    panel.connect();
    let (commands, output) = panel.pending_session.take().unwrap();
    panel.running = true;
    panel.write_ready = true;
    (panel, commands, output)
}

fn take_write(commands: &mut tokio::sync::mpsc::UnboundedReceiver<DebugCommand>) -> SignalWrite {
    match commands.try_recv().unwrap() {
        DebugCommand::Write(write) => write,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn writing_requires_ready_session_and_matching_nonconstant_type() {
    let (mut panel, mut commands, _output) = connected();
    for (signum, value) in [
        (4, SignalValue::Digital(true)),
        (5, SignalValue::Digital(true)),
        (9, SignalValue::Analog(1)),
        (1, SignalValue::Serial("wrong".into())),
    ] {
        panel.set_signal(signum, value);
        assert!(commands.try_recv().is_err());
    }
    panel.write_ready = false;
    panel.set_signal(1, SignalValue::Digital(true));
    assert!(commands.try_recv().is_err());
    panel.write_ready = true;
    panel.set_signal(2, SignalValue::Analog(65535));
    assert_eq!(take_write(&mut commands).value, SignalValue::Analog(65535));
    assert!(panel.values.is_empty());
    assert!(
        panel.samples.is_empty(),
        "sending is not a received transition"
    );
    panel.set_signal(3, SignalValue::Serial("x".repeat(1000)));
    assert!(commands.try_recv().is_err());
    assert!(panel.editor.status.contains("too long"));
    panel.stop();
    panel.set_signal(1, SignalValue::Digital(true));
    assert!(matches!(commands.try_recv(), Ok(DebugCommand::Stop)));
    assert!(commands.try_recv().is_err());
}

struct Ui {
    ctx: egui::Context,
    time: f64,
}
impl Ui {
    fn new() -> Self {
        Self {
            ctx: Default::default(),
            time: 0.0,
        }
    }
    fn frame(
        &mut self,
        panel: &mut Panel,
        events: Vec<egui::Event>,
        focused: bool,
    ) -> egui::FullOutput {
        self.time += 0.05;
        let mut output = self.ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1600.0, 1400.0),
                )),
                time: Some(self.time),
                events,
                focused,
                ..Default::default()
            },
            |ui| panel.show_signal_details(ui.ctx()),
        );
        output.textures_delta.clear();
        output
    }
    fn position(&mut self, panel: &mut Panel, label: &str) -> egui::Pos2 {
        for _ in 0..3 {
            self.frame(panel, vec![], true)
                .drop_without_applying_deltas();
        }
        self.frame(panel, vec![], true)
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) if t.galley.text() == label => {
                    Some(t.pos + t.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing {label}"))
    }
    fn pointer(&mut self, panel: &mut Panel, pos: egui::Pos2, pressed: bool) {
        self.frame(
            panel,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    pressed,
                    button: egui::PointerButton::Primary,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            true,
        )
        .drop_without_applying_deltas();
    }
    fn click(&mut self, panel: &mut Panel, label: &str) {
        let pos = self.position(panel, label);
        self.pointer(panel, pos, true);
        self.pointer(panel, pos, false);
    }
}

#[test]
fn modal_buttons_send_digital_analog_and_serial_values() {
    let (mut panel, mut commands, _output) = connected();
    let mut ui = Ui::new();
    panel.detail_signal = Some(1);
    for (label, value) in [
        ("High", SignalValue::Digital(true)),
        ("Low", SignalValue::Digital(false)),
        ("Pulse", SignalValue::Pulse(100)),
    ] {
        ui.click(&mut panel, label);
        assert_eq!(take_write(&mut commands), SignalWrite { signum: 1, value });
    }
    panel.detail_signal = Some(2);
    ui.position(&mut panel, "Set value");
    panel.editor.analog = "65535".into();
    ui.click(&mut panel, "Set value");
    assert_eq!(take_write(&mut commands).value, SignalValue::Analog(65535));
    ui.click(&mut panel, "Signed");
    assert_eq!(panel.editor.analog, "-1");
    panel.editor.analog = "-32768".into();
    ui.click(&mut panel, "Set value");
    assert_eq!(take_write(&mut commands).value, SignalValue::Analog(32768));
    panel.editor.analog = "32768".into();
    ui.click(&mut panel, "Set value");
    assert!(commands.try_recv().is_err());
    panel.detail_signal = Some(3);
    ui.position(&mut panel, "Send string");
    panel.editor.serial = "hello\"\r\n".into();
    ui.click(&mut panel, "Send string");
    assert_eq!(
        take_write(&mut commands).value,
        SignalValue::Serial("hello\"\r\n".into())
    );
}

#[test]
fn hold_releases_on_mouse_up_outside_focus_loss_close_switch_and_stop() {
    for path in ["mouse", "focus", "close", "switch", "stop"] {
        let (mut panel, mut commands, _output) = connected();
        let mut ui = Ui::new();
        panel.detail_signal = Some(1);
        let pos = ui.position(&mut panel, "Push and hold");
        ui.pointer(&mut panel, pos, true);
        assert_eq!(take_write(&mut commands).value, SignalValue::Hold);
        assert_eq!(panel.held_signal, Some(1));
        ui.frame(&mut panel, vec![], true)
            .drop_without_applying_deltas();
        assert!(commands.try_recv().is_err(), "one High per press");
        match path {
            "mouse" => ui.pointer(&mut panel, egui::pos2(1.0, 1.0), false),
            "focus" => ui
                .frame(&mut panel, vec![], false)
                .drop_without_applying_deltas(),
            "close" => {
                ui.frame(
                    &mut panel,
                    vec![egui::Event::Key {
                        key: egui::Key::Escape,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    true,
                )
                .drop_without_applying_deltas();
                assert!(panel.detail_signal.is_none());
            }
            "switch" => {
                panel.detail_signal = Some(2);
                ui.frame(&mut panel, vec![], true)
                    .drop_without_applying_deltas();
            }
            _ => panel.stop(),
        }
        assert_eq!(
            take_write(&mut commands),
            SignalWrite {
                signum: 1,
                value: SignalValue::Release
            },
            "{path}"
        );
        assert_eq!(panel.held_signal, None);
    }
}

#[test]
fn worker_result_and_disconnection_are_visible_without_fabricated_values() {
    let (mut panel, mut commands, output) = connected();
    panel.editor.signum = Some(1);
    panel.set_signal(1, SignalValue::Hold);
    take_write(&mut commands);
    output
        .try_send(DebugEvent::WriteResult {
            signum: 1,
            result: Err("Processor rejected write".into()),
        })
        .unwrap();
    panel.poll();
    assert!(panel.editor.status.contains("Processor rejected write"));
    assert!(panel.values.is_empty());
    drop(commands);
    panel.release_held();
    assert!(
        panel
            .editor
            .status
            .contains("release could not be confirmed")
    );
}
