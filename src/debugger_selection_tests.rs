use super::*;

#[test]
fn both_trees_support_ranges_toggles_and_watch_removal() {
    for watch in [false, true] {
        let ctx = egui::Context::default();
        let mut panel = Panel::new("processor".into(), "Processor");
        panel.signals = (0..4)
            .map(|i| Signal {
                name: format!("Signal-{i}"),
                metadata: [i, 0, 0, 0, 0, 1],
            })
            .collect();
        panel.groups.insert("Watched".into(), vec![0, 1, 2, 3]);
        panel.groups.insert("Other".into(), vec![0, 1, 2, 3]);
        panel.timeline_group = Some("Watched".into());
        let mut time = 0.0;
        let mut render = |panel: &mut Panel, events| {
            time += 0.05;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1600.0, 900.0),
                    )),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ui| {
                    if watch {
                        panel.contents(ui);
                    } else {
                        egui::CentralPanel::default().show(ui, |ui| {
                            for i in 0..4 {
                                panel.show_signal_row(ui, i);
                            }
                        });
                    }
                },
            );
            output.textures_delta.clear();
            output
        };
        let mut click = |panel: &mut Panel, label: &str, button, modifiers| {
            for _ in 0..3 {
                render(panel, vec![]).drop_without_applying_deltas();
            }
            let output = render(panel, vec![]);
            let pos = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                        Some(text.pos + text.galley.size() * 0.5)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing {label}"));
            for pressed in [true, false] {
                render(
                    panel,
                    vec![
                        egui::Event::ModifiersChanged(modifiers),
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button,
                            pressed,
                            modifiers,
                        },
                    ],
                )
                .drop_without_applying_deltas();
            }
        };
        let primary = egui::PointerButton::Primary;
        let secondary = egui::PointerButton::Secondary;
        let plain = egui::Modifiers::NONE;
        let shift = egui::Modifiers {
            shift: true,
            ..plain
        };
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..plain
        };
        let selected = |panel: &Panel| {
            if watch {
                panel.watch_selections["Watched"].selected.clone()
            } else {
                panel.tree_selection.clone()
            }
        };
        click(&mut panel, "Signal-0", primary, plain);
        click(&mut panel, "Signal-2", primary, shift);
        assert_eq!(selected(&panel), HashSet::from([0, 1, 2]));
        click(&mut panel, "Signal-1", primary, ctrl);
        assert_eq!(selected(&panel), HashSet::from([0, 2]));
        click(
            &mut panel,
            "Signal-3",
            primary,
            egui::Modifiers {
                shift: true,
                ..ctrl
            },
        );
        assert_eq!(selected(&panel), HashSet::from([0, 1, 2, 3]));
        click(&mut panel, "Signal-0", primary, plain);
        click(&mut panel, "Signal-2", primary, ctrl);
        click(&mut panel, "Signal-0", secondary, plain);
        assert_eq!(selected(&panel), HashSet::from([0, 2]));
        if watch {
            click(&mut panel, "Remove 2 signals from group", primary, plain);
            assert_eq!(panel.groups["Watched"], [1, 3]);
            assert!(selected(&panel).is_empty());
            // Right-clicking an unselected signal acts on that one alone.
            click(&mut panel, "Signal-3", secondary, plain);
            click(&mut panel, "Remove from group", primary, plain);
            assert_eq!(panel.groups["Watched"], [1]);
            click(&mut panel, "Watched", secondary, plain);
            click(&mut panel, "Remove all signals", primary, plain);
            assert!(panel.groups["Watched"].is_empty());
            assert_eq!(panel.groups["Other"], [0, 1, 2, 3]);
            assert!(panel.disabled_signals.is_empty());
        }
    }
}
