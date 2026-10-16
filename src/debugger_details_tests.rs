use super::*;

#[test]
fn double_click_opens_details_from_each_signal_view() {
    for source in ["tree", "watch", "timeline"] {
        let ctx = egui::Context::default();
        let mut panel = Panel::new("processor".into(), "Processor");
        panel.signals = vec![Signal {
            name: "Detail signal".into(),
            metadata: [1, 0, 0, 0, 0, 1],
        }];
        panel.search = "Detail".into();
        panel.groups.get_mut("Watched").unwrap().push(1);
        panel.ingest(b"\x0100000001:1=before\r\n\x03", SystemTime::now());
        let mut time = 0.0;
        let mut render = |panel: &mut Panel, events| {
            time += 0.03;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1600.0, 1000.0),
                    )),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ui| panel.contents(ui),
            );
            output.textures_delta.clear();
            output
        };
        for _ in 0..10 {
            render(&mut panel, vec![]).drop_without_applying_deltas();
        }
        let output = render(&mut panel, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t)
                    if t.galley.text() == "Detail signal"
                        && match source {
                            "tree" => t.pos.x < 350.0,
                            "watch" => t.pos.x > 1250.0,
                            _ => t.pos.x >= 350.0 && t.pos.x < 1250.0,
                        } =>
                {
                    Some(t.pos + t.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing signal in {source}"));
        for click in 0..2 {
            for pressed in [true, false] {
                render(
                    &mut panel,
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            pressed,
                            button: egui::PointerButton::Primary,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                )
                .drop_without_applying_deltas();
            }
            if click == 0 {
                assert_eq!(panel.detail_signal, None);
            }
        }
        assert_eq!(panel.detail_signal, Some(1), "{source}");
        panel.ingest(b"\x0100000001:1=after\r\n\x03", SystemTime::now());
        for _ in 0..3 {
            render(&mut panel, vec![]).drop_without_applying_deltas();
        }
        let output = render(&mut panel, vec![]);
        for label in ["Signal details", "Signal ID", "Raw value"] {
            assert!(
                output.shapes.iter().any(
                    |s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == label)
                ),
                "missing {label} from {source}"
            );
        }
        // Controls add content above the history; scroll the shared body.
        let body = output
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) if t.galley.text() == "Signal ID" => Some(t.pos),
                _ => None,
            })
            .unwrap();
        for _ in 0..10 {
            render(
                &mut panel,
                vec![
                    egui::Event::PointerMoved(body),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -500.0),
                        modifiers: egui::Modifiers::NONE,
                        phase: egui::TouchPhase::Move,
                    },
                ],
            )
            .drop_without_applying_deltas();
        }
        let output = render(&mut panel, vec![]);
        for label in ["Recorded history · 2 updates", "before", "after"] {
            assert!(
                output.shapes.iter().any(
                    |s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == label)
                ),
                "missing {label} from {source}"
            );
        }
        let close = output
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) if t.galley.text() == "Close" => {
                    Some(t.pos + t.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap();
        for pressed in [true, false] {
            render(
                &mut panel,
                vec![
                    egui::Event::PointerMoved(close),
                    egui::Event::PointerButton {
                        pos: close,
                        pressed,
                        button: egui::PointerButton::Primary,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            )
            .drop_without_applying_deltas();
        }
        assert_eq!(panel.detail_signal, None);
        assert_eq!(panel.samples.len(), 2);
    }
}

#[test]
fn detail_history_ignores_watch_filter_and_includes_paused_records() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = vec![
        Signal {
            name: "Alpha".into(),
            metadata: [1, 0, 0, 0, 0, 1],
        },
        Signal {
            name: "Bravo".into(),
            metadata: [2, 0, 0, 0, 0, 1],
        },
    ];
    panel.ingest(
        b"\x0100000001:1=first\r\n00000002:1=other\r\n00000001:1=second\r\n\x03",
        SystemTime::now(),
    );
    panel.timeline_group = Some("Watched".into());
    assert!(panel.timeline_indices().is_empty());
    panel.toggle_pause();
    panel.ingest(b"\x0100000001:1=buffered\r\n\x03", SystemTime::now());
    assert_eq!(
        panel
            .signal_history(1)
            .iter()
            .map(|s| s.value.as_str())
            .collect::<Vec<_>>(),
        ["buffered", "second", "first"]
    );
    panel.toggle_pause();
    assert_eq!(panel.signal_history(1).len(), 3);
    panel.detail_signal = Some(1);
    panel.clear_history();
    assert!(panel.signal_history(1).is_empty());
    assert_eq!(panel.values[&1], "buffered");
    panel.reset_signals();
    assert_eq!(panel.detail_signal, None);
}

#[test]
fn long_history_scrolls_with_the_whole_details_body() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = vec![Signal {
        name: "Busy".into(),
        metadata: [1, 0, 0, 0, 0, 1],
    }];
    let mut frame = b"\x01".to_vec();
    for n in 0..500 {
        frame.extend_from_slice(format!("00000001:1=value-{n:03}\r\n").as_bytes());
    }
    frame.push(3);
    panel.ingest(&frame, SystemTime::now());
    panel.detail_signal = Some(1);
    let mut time = 0.0;
    let mut render = |panel: &mut Panel, events| {
        time += 0.03;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1600.0, 1000.0),
                )),
                time: Some(time),
                events,
                ..Default::default()
            },
            |ui| panel.contents(ui),
        );
        output.textures_delta.clear();
        output
    };
    let texts = |output: &egui::FullOutput| -> Vec<(String, egui::Pos2)> {
        output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) => Some((t.galley.text().to_owned(), t.pos)),
                _ => None,
            })
            .collect()
    };
    for _ in 0..3 {
        render(&mut panel, vec![]).drop_without_applying_deltas();
    }
    let output = render(&mut panel, vec![]);
    let shown = texts(&output);
    let controls: Vec<_> = shown
        .iter()
        .filter(|(text, _)| {
            matches!(
                text.as_str(),
                "Set signal" | "High" | "Low" | "Pulse" | "Push and hold" | "Release"
            )
        })
        .cloned()
        .collect();
    assert_eq!(controls.len(), 6);
    let heading = shown
        .iter()
        .find(|(text, _)| text == "Signal ID")
        .expect("details body visible")
        .1;
    // The modal's height goes to the history: far more than the old
    // fixed box of roughly a dozen rows, yet only the rows in view are drawn.
    let rows = shown
        .iter()
        .filter(|(text, _)| text.starts_with("value-"))
        .count();
    assert!(rows > 20, "{rows} history rows visible");
    assert!(rows < 100, "{rows} history rows laid out");
    assert!(shown.iter().any(|(text, _)| text == "value-499"));
    assert!(!shown.iter().any(|(text, _)| text == "value-000"));

    // Scrolling anywhere in the body moves the fields and history together.
    for _ in 0..40 {
        render(
            &mut panel,
            vec![
                egui::Event::PointerMoved(heading),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -2000.0),
                    modifiers: egui::Modifiers::NONE,
                    phase: egui::TouchPhase::Move,
                },
            ],
        )
        .drop_without_applying_deltas();
    }
    let shown = texts(&render(&mut panel, vec![]));
    assert!(shown.iter().any(|(text, _)| text == "value-000"));
    assert!(!shown.iter().any(|(text, _)| text == "Signal ID"));
    for (label, position) in controls {
        let after = shown
            .iter()
            .find(|(text, _)| text == &label)
            .unwrap_or_else(|| panic!("header control {label} scrolled away"));
        assert!(
            (after.1.y - position.y).abs() < 1.0,
            "header control {label} moved"
        );
    }
}
