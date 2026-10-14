use super::*;

fn fixture(name: &str, metadata: [u8; 6]) -> Vec<u8> {
    let mut bytes = HEADER.to_vec();
    let utf16: Vec<_> = name.encode_utf16().collect();
    bytes.extend_from_slice(&((2 + utf16.len() * 2 + 6) as u16).to_le_bytes());
    for unit in utf16 {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&metadata);
    bytes
}

#[test]
fn parse_signals_preserves_name_and_number() {
    let bytes = fixture("::hello 世界", [0x10, 0x10, 0, 0, 0, 1]);
    let signals = parse_signals(&bytes).unwrap();
    assert_eq!(signals[0].name, "::hello 世界");
    assert_eq!(signals[0].signum(), 0x1010);
    let mut duplicate = bytes.clone();
    duplicate.extend_from_slice(&bytes[HEADER.len()..]);
    assert_eq!(parse_signals(&duplicate).unwrap().len(), 2);
    assert_eq!(debug_signums(&parse_signals(&duplicate).unwrap()), [0x1010]);
    assert!(parse_signals(b"bad").is_err());
    assert!(parse_signals(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn parse_signals_reads_the_zipped_signature_a_processor_keeps() {
    use crate::test_support::{stamp, zip};
    let sig = fixture("Zipped", [0x10, 0x10, 0, 0, 0, 1]);
    let when = stamp(2026, 10, 13, 20, 26);
    for deflate in [true, false] {
        let zig = zip(&[("training.sig", &sig, deflate, when)]);
        let signals = parse_signals(&zig).unwrap();
        assert_eq!(signals[0].name, "Zipped");
        assert_eq!(signals[0].signum(), 0x1010);
    }
    let alongside = zip(&[
        ("readme.txt", b"not a signature", false, when),
        ("Program.SIG", &sig, true, when),
    ]);
    assert_eq!(parse_signals(&alongside).unwrap()[0].name, "Zipped");

    let none = zip(&[("readme.txt", b"text", false, when)]);
    assert!(parse_signals(&none).unwrap_err().contains("no .sig"));
    let two = zip(&[("a.sig", &sig, true, when), ("b.sig", &sig, true, when)]);
    assert!(parse_signals(&two).unwrap_err().contains("more than one"));
    let wrong = zip(&[("bad.sig", b"[OTHER]", true, when)]);
    assert_eq!(
        parse_signals(&wrong).unwrap_err(),
        "Unknown signature format"
    );
    let zig = zip(&[("training.sig", &sig, true, when)]);
    assert!(parse_signals(&zig[..zig.len() - 8]).is_err());
}

#[test]
fn parse_processor_example_if_present() {
    if let Ok(bytes) = std::fs::read("rmc3_test.sig") {
        let signals = parse_signals(&bytes).unwrap();
        assert_eq!(signals.len(), 5975);
        assert_eq!(signals[0].signum(), 0x1010);
        assert!(signals[0].name.starts_with("::not_held_pre_pulse:"));
        let tree = signal_tree(&signals, "");
        let mut leaves = Vec::new();
        tree_indices(&tree, &mut leaves);
        assert!(leaves.iter().all(|leaf| leaf.len() <= 50));
        let visible: Vec<_> = leaves.into_iter().flatten().collect();
        assert!(
            visible
                .iter()
                .all(|&index| !signals[index].name.starts_with("::"))
        );
        assert_eq!(
            visible.len(),
            signals
                .iter()
                .filter(|signal| !signal.name.starts_with("::"))
                .count()
        );
    }
}

fn tree_indices(nodes: &[SignalTree], leaves: &mut Vec<Vec<usize>>) {
    for node in nodes {
        match node {
            SignalTree::Branch { children, .. } => tree_indices(children, leaves),
            SignalTree::Leaf { indices, .. } => leaves.push(indices.clone()),
        }
    }
}

#[test]
fn signal_tree_has_only_populated_initial_roots_and_excludes_modules() {
    let signals: Vec<_> = [
        "Zebra", "apple", "Alpha", "9room", "1room", "::Module", "_other",
    ]
    .into_iter()
    .map(|name| Signal {
        name: name.into(),
        metadata: [0; 6],
    })
    .collect();
    let tree = signal_tree(&signals, "");
    let labels: Vec<_> = tree
        .iter()
        .map(|node| match node {
            SignalTree::Branch { label, .. } | SignalTree::Leaf { label, .. } => label.as_str(),
        })
        .collect();
    assert_eq!(
        labels,
        [
            "0–9 · 2 signals",
            "A · 2 signals",
            "Z · 1 signal",
            "Other · 1 signal"
        ]
    );
    let mut leaves = Vec::new();
    tree_indices(&tree, &mut leaves);
    assert_eq!(leaves, [vec![4, 3], vec![2, 1], vec![0], vec![6]]);
    assert!(signal_tree(&signals, "::").is_empty());
    let filtered = signal_tree(&signals, "apple");
    assert_eq!(filtered.len(), 1);
    assert!(
        matches!(&filtered[0], SignalTree::Leaf { label, indices } if label == "A · 1 signal" && indices == &[1])
    );
}

#[test]
fn expanded_tree_leaves_use_the_parent_scroll_area_without_height_caps() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = (0..40)
        .map(|n| Signal {
            name: format!("Signal-{n:02}"),
            metadata: [n, 0, 0, 0, 0, 1],
        })
        .collect();
    panel.search = "Signal".into();
    let tree = signal_tree(&panel.signals, &panel.search);
    for pass in 0..3 {
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 2000.0),
                )),
                ..Default::default()
            },
            |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| panel.show_signal_nodes(ui, &tree));
            },
        );
        if pass == 2 {
            let positions: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(text)
                        if text.galley.text().starts_with("Signal-") =>
                    {
                        Some(text.pos.y)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(positions.len(), 40);
            assert!(positions.last().unwrap() - positions.first().unwrap() > 300.0);
        }
        output.drop_without_applying_deltas();
    }
}

#[test]
fn paused_timeline_buffers_changes_and_resumes_with_original_timestamps() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("Signal", [1, 0, 0, 0, 0, 1])).unwrap();
    let at = SystemTime::now();
    panel.ingest(b"\x0100000001:1=before\r\n\x03", at);
    panel.toggle_pause();
    panel.ingest(b"\x0100000001:1=during\r\n\x03", at);
    assert_eq!(panel.samples.len(), 1);
    assert_eq!(panel.samples[0].value, "before");
    assert_eq!(panel.pending_samples.len(), 1);
    assert_eq!(panel.values[&1], "during");
    panel.toggle_pause();
    assert_eq!(panel.samples.len(), 2);
    assert_eq!(panel.samples[1].value, "during");
    assert_eq!(panel.samples[1].at, at);
    assert!(panel.pending_samples.is_empty());
    panel.toggle_pause();
    panel.ingest(&b"\x0100000001:1=burst\r\n\x03".repeat(MAX_EVENTS + 1), at);
    assert_eq!(panel.samples.len(), 2);
    assert_eq!(panel.pending_samples.len(), MAX_EVENTS);
    assert_eq!(panel.pending_dropped, 1);
    panel.toggle_pause();
    assert_eq!(panel.samples.len(), MAX_EVENTS);
    panel.toggle_pause();
    panel.ingest(b"\x0100000001:1=clear\r\n\x03", at);
    panel.clear_history();
    panel.toggle_pause();
    assert!(panel.samples.is_empty());
}

#[test]
fn tree_group_targets_include_all_descendants_without_modules_or_duplicates() {
    let mut signals: Vec<_> = (0..120)
        .map(|i| Signal {
            name: format!("Alpha-{i:03}"),
            metadata: [i, 0, 0, 0, 0, 1],
        })
        .collect();
    signals.push(Signal {
        name: "::module".into(),
        metadata: [255, 0, 0, 0, 0, 1],
    });
    let tree = signal_tree(&signals, "");
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].signums(&signals), (0..120).collect());
    if let SignalTree::Branch { children, .. } = &tree[0] {
        assert_eq!(children[0].signums(&signals).len(), 50);
    } else {
        panic!("expected range children");
    }
}

#[test]
fn tree_selection_preserves_context_targets_and_deduplicates_watches() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.select_tree_signal(1, false, false);
    panel.select_tree_signal(2, true, false);
    panel.select_tree_signal(1, false, true);
    assert_eq!(panel.tree_selection, HashSet::from([1, 2]));
    panel.groups.insert("Other".into(), vec![2]);
    panel.add_tree_selection_to_group("Other");
    panel.add_tree_selection_to_group("Other");
    assert_eq!(panel.groups["Other"], [2, 1]);
    assert!(panel.groups["Watched"].is_empty());
    panel.select_tree_signal(2, true, false);
    assert_eq!(panel.tree_selection, HashSet::from([1]));
    panel.select_tree_signal(3, false, true);
    assert_eq!(panel.tree_selection, HashSet::from([3]));
    panel.reset_signals();
    assert!(panel.tree_selection.is_empty());
}

#[test]
fn tree_context_menu_bulk_disables_and_adds_to_chosen_watch_group() {
    fn frame(
        ctx: &egui::Context,
        panel: &mut Panel,
        events: Vec<egui::Event>,
        modifiers: egui::Modifiers,
    ) -> egui::FullOutput {
        let events = std::iter::once(egui::Event::ModifiersChanged(modifiers))
            .chain(events)
            .collect();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                for index in 0..panel.signals.len() {
                    panel.show_signal_row(ui, index);
                }
                let tree = signal_tree(&panel.signals, "");
                panel.show_signal_nodes(ui, &tree);
            },
        );
        output.textures_delta.clear();
        output
    }
    fn click(
        ctx: &egui::Context,
        panel: &mut Panel,
        text: &str,
        button: egui::PointerButton,
        modifiers: egui::Modifiers,
    ) {
        for _ in 0..3 {
            frame(ctx, panel, vec![], modifiers).drop_without_applying_deltas();
        }
        let output = frame(ctx, panel, vec![], modifiers);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(label) if label.galley.text() == text => {
                    Some(label.pos + label.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing {text}"));
        for pressed in [true, false] {
            frame(
                ctx,
                panel,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        modifiers,
                    },
                ],
                modifiers,
            )
            .drop_without_applying_deltas();
        }
    }
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = ["Alpha", "Bravo", "Charlie"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| Signal {
            name: name.into(),
            metadata: [i as u8, 0, 0, 0, 0, 1],
        })
        .collect();
    panel.groups.insert("Custom".into(), vec![]);
    let plain = egui::Modifiers::NONE;
    let ctrl = egui::Modifiers {
        ctrl: true,
        ..Default::default()
    };
    click(
        &ctx,
        &mut panel,
        "Alpha",
        egui::PointerButton::Primary,
        plain,
    );
    click(
        &ctx,
        &mut panel,
        "Bravo",
        egui::PointerButton::Primary,
        ctrl,
    );
    assert_eq!(panel.tree_selection, HashSet::from([0, 1]));
    click(
        &ctx,
        &mut panel,
        "Alpha",
        egui::PointerButton::Secondary,
        plain,
    );
    click(
        &ctx,
        &mut panel,
        "Disable debugging locally",
        egui::PointerButton::Primary,
        plain,
    );
    assert_eq!(panel.disabled_signals, HashSet::from([0, 1]));
    click(
        &ctx,
        &mut panel,
        "Bravo",
        egui::PointerButton::Secondary,
        plain,
    );
    click(
        &ctx,
        &mut panel,
        "Add to watch group",
        egui::PointerButton::Primary,
        plain,
    );
    click(
        &ctx,
        &mut panel,
        "Custom",
        egui::PointerButton::Primary,
        plain,
    );
    assert_eq!(panel.groups["Custom"], [0, 1]);
    assert!(panel.groups["Watched"].is_empty());
    assert!(!panel.disabled_signals.contains(&2));
    click(
        &ctx,
        &mut panel,
        "C · 1 signal",
        egui::PointerButton::Secondary,
        plain,
    );
    click(
        &ctx,
        &mut panel,
        "Disable debugging locally",
        egui::PointerButton::Primary,
        plain,
    );
    assert!(panel.disabled_signals.contains(&2));
    assert_eq!(panel.tree_selection, HashSet::from([2]));
    click(
        &ctx,
        &mut panel,
        "C · 1 signal",
        egui::PointerButton::Secondary,
        plain,
    );
    click(
        &ctx,
        &mut panel,
        "Add to watch group",
        egui::PointerButton::Primary,
        plain,
    );
    click(
        &ctx,
        &mut panel,
        "Custom",
        egui::PointerButton::Primary,
        plain,
    );
    assert_eq!(panel.groups["Custom"], [0, 1, 2]);
}

#[test]
fn signal_tree_alphabetical_order_ignores_case() {
    let signals: Vec<_> = ["zebra", "Bravo", "alpha", "ALPHA", "charlie"]
        .into_iter()
        .map(|name| Signal {
            name: name.into(),
            metadata: [0; 6],
        })
        .collect();
    let mut leaves = Vec::new();
    tree_indices(&signal_tree(&signals, ""), &mut leaves);
    let names: Vec<_> = leaves
        .into_iter()
        .flatten()
        .map(|i| signals[i].name.as_str())
        .collect();
    assert_eq!(names, ["ALPHA", "alpha", "Bravo", "charlie", "zebra"]);
}

#[test]
fn signal_search_requires_three_characters_and_clear_restores_tree() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = ["Alpha", "Bravo"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| Signal {
            name: name.into(),
            metadata: [i as u8, 0, 0, 0, 0, 1],
        })
        .collect();
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
            |ui| panel.contents(ui),
        );
        output.textures_delta.clear();
        output
    };
    for (query, count) in [
        ("", 2),
        ("A", 2),
        ("Al", 2),
        ("Alp", 1),
        ("éé", 2),
        ("ééé", 0),
        ("Bra", 1),
    ] {
        panel.search = query.into();
        render(&mut panel, vec![]).drop_without_applying_deltas();
        assert_eq!(
            panel
                .tree
                .as_ref()
                .unwrap()
                .iter()
                .map(SignalTree::count)
                .sum::<usize>(),
            count,
            "{query}"
        );
    }
    let output = render(&mut panel, vec![]);
    let pos = output
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t) if t.galley.text() == "×" => {
                Some(t.pos + t.galley.size() * 0.5)
            }
            _ => None,
        })
        .expect("inline clear button");
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
    assert!(panel.search.is_empty());
    assert_eq!(
        panel
            .tree
            .as_ref()
            .unwrap()
            .iter()
            .map(SignalTree::count)
            .sum::<usize>(),
        2
    );
}

#[test]
fn expanded_signal_tree_only_lays_out_visible_signal_text() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.search = "Signal".into();
    panel.signals = (0u32..6000)
        .map(|n| {
            let mut metadata = [0; 6];
            metadata[..4].copy_from_slice(&n.to_le_bytes());
            Signal {
                name: format!("Signal-{n:04}"),
                metadata,
            }
        })
        .collect();
    let tree = signal_tree(&panel.signals, "Signal");
    for offset in [0.0, 50000.0] {
        for _ in 0..3 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(500.0, 600.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let response = egui::ScrollArea::vertical()
                            .vertical_scroll_offset(offset)
                            .show(ui, |ui| panel.show_signal_nodes(ui, &tree));
                        assert!(response.content_size.y > 100000.0);
                    });
                },
            );
            output.textures_delta.clear();
            let rows = output.shapes.iter().filter(|s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == "Not received")).count();
            assert!(
                rows > 0 && rows < 30,
                "rendered {rows} signal values at offset {offset}"
            );
        }
    }
}

#[test]
fn snapshot_populates_all_values_even_in_one_large_frame() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = (0u32..5975)
        .map(|n| {
            let mut metadata = [0; 6];
            metadata[..4].copy_from_slice(&n.to_le_bytes());
            Signal {
                name: format!("Signal-{n}"),
                metadata,
            }
        })
        .collect();
    let mut snapshot = String::from("\x01\r\n");
    for n in 0..5975 {
        snapshot.push_str(&format!("{n:08X}:1={n}\r\n"));
    }
    snapshot.push('\x03');
    assert!(snapshot.len() > 64 * 1024);
    for chunk in snapshot.as_bytes().chunks(113) {
        panel.ingest(chunk, SystemTime::now());
    }
    assert_eq!(panel.values.len(), 5975);
    for n in 0..5975 {
        assert_eq!(panel.values[&n], n.to_string());
    }
    assert_eq!(panel.samples.len(), 5975);
    panel.clear_history();
    assert_eq!(panel.values.len(), 5975);
    panel.reset_signals();
    assert!(panel.values.is_empty());
}

#[test]
fn signal_tree_renders_unknown_and_current_formatted_values() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("Signal", [1, 0, 0, 0, 0, 1])).unwrap();
    let render = |panel: &mut Panel, expected: &str| {
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| panel.show_signal_row(ui, 0));
        });
        output.textures_delta.clear();
        assert!(
            output.shapes.iter().any(
                |s| matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == expected)
            ),
            "missing {expected}"
        );
    };
    render(&mut panel, "Not received");
    panel.ingest(b"\x0100000001:1=[48][69][0D]\r\n\x03", SystemTime::now());
    render(&mut panel, "Hi.");
    panel.display_types.insert(1, ValueDisplay::Hex);
    render(&mut panel, "48 69 0D");
    panel.display_types.insert(1, ValueDisplay::Mixed);
    render(&mut panel, "Hi\\x0D");
    panel.toggle_pause();
    panel.disabled_signals.insert(1);
    panel.ingest(b"\x0100000001:1=latest\r\n\x03", SystemTime::now());
    render(&mut panel, "latest");
    assert_eq!(panel.samples.len(), 1);
}

#[test]
fn watch_group_members_collapse_without_changing_timeline_filter() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("Only watched", [1, 0, 0, 0, 0, 1])).unwrap();
    panel.groups.get_mut("Watched").unwrap().push(1);
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
            |ui| panel.contents(ui),
        );
        output.textures_delta.clear();
        output
    };
    for _ in 0..3 {
        render(&mut panel, vec![]).drop_without_applying_deltas();
    }
    for expected_open in [false, true] {
        let output = render(&mut panel, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == "Watched" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .expect("collapsible group header");
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
        for _ in 0..10 {
            render(&mut panel, vec![]).drop_without_applying_deltas();
        }
        let output = render(&mut panel, vec![]);
        assert_eq!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.text() == "Only watched")), expected_open);
        assert_eq!(panel.timeline_group.as_deref(), Some("Watched"));
        assert_eq!(panel.groups["Watched"], [1]);
    }
}

#[test]
fn watch_group_clicks_filter_history_without_discarding_other_signals() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = ["Alpha", "Bravo"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| Signal {
            name: name.into(),
            metadata: [i as u8, 0, 0, 0, 0, 1],
        })
        .collect();
    panel.groups.insert("First".into(), vec![0]);
    panel.groups.insert("Second".into(), vec![1]);
    panel.ingest(
        b"\x0100000000:1=111\r\n00000001:1=222\r\n\x03",
        SystemTime::now(),
    );
    for (label, expected) in [
        ("First · 1 signals", vec![0]),
        ("Second · 1 signals", vec![1]),
        ("Watched · 0 signals", vec![]),
        ("All signals", vec![1, 0]),
    ] {
        let mut position = None;
        for _ in 0..3 {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1600.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ui| panel.contents(ui),
            );
            position = output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            });
            output.drop_without_applying_deltas();
        }
        let pos = position.expect("group selector visible");
        for pressed in [true, false] {
            ctx.run_ui(
                egui::RawInput {
                    events: vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            pressed,
                            button: egui::PointerButton::Primary,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ui| panel.contents(ui),
            )
            .drop_without_applying_deltas();
        }
        assert_eq!(panel.timeline_indices(), expected);
        assert_eq!(panel.samples.len(), 2);
    }
    panel.timeline_group = Some("First".into());
    panel.ingest(b"\x0100000001:1=333\r\n\x03", SystemTime::now());
    assert_eq!(panel.timeline_indices(), [0]);
    panel.groups.get_mut("First").unwrap().push(1);
    assert_eq!(panel.timeline_indices(), [2, 1, 0]);
    panel.clear_history();
    assert!(panel.timeline_indices().is_empty());
}

#[test]
fn tree_selection_footer_stays_at_bottom_with_long_and_filtered_lists() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = (0..100)
        .map(|n| Signal {
            name: format!("Signal-{n:03}"),
            metadata: [n, 0, 0, 0, 0, 1],
        })
        .collect();
    panel.tree_selection.insert(1);
    let mut positions = Vec::new();
    for query in ["Signal", "missing"] {
        panel.search = query.into();
        for pass in 0..3 {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1600.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ui| panel.contents(ui),
            );
            if pass == 2 {
                let pos = output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::epaint::Shape::Text(text)
                            if text.galley.text() == "Clear selection" =>
                        {
                            Some(text.pos)
                        }
                        _ => None,
                    })
                    .expect("footer button visible");
                assert!(pos.y > 740.0, "footer must be at the panel bottom: {pos:?}");
                positions.push(pos);
                assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.text() == "☆")));
            }
            output.drop_without_applying_deltas();
        }
    }
    assert_eq!(positions[0], positions[1]);
}

#[test]
fn signal_tree_click_only_selects_without_toggling_monitoring_or_watches() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("Toggle", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    let mut position = egui::Pos2::ZERO;
    for _ in 0..3 {
        ctx.run_ui(Default::default(), |ui| {
            position = ui.next_widget_position() + egui::vec2(8.0, 9.0);
            panel.show_signal_row(ui, 0);
        })
        .drop_without_applying_deltas();
    }
    for count in 1..=2 {
        for pressed in [true, false] {
            ctx.run_ui(
                egui::RawInput {
                    events: vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ui| {
                    panel.show_signal_row(ui, 0);
                },
            )
            .drop_without_applying_deltas();
        }
        assert!(panel.disabled_signals.is_empty());
        assert_eq!(panel.tree_selection, HashSet::from([0x1010]));
        assert!(panel.groups["Watched"].is_empty());
        panel.ingest(b"\x0100001010:1=42\r\n\x03", SystemTime::now());
        assert_eq!(panel.samples.len(), count);
    }
}

#[test]
fn signal_tree_sorts_names_and_splits_every_leaf_at_fifty() {
    let signals: Vec<_> = (0..123)
        .rev()
        .map(|index| Signal {
            name: format!("signal-{index:03}"),
            metadata: [index as u8, 0, 0, 0, 0, 1],
        })
        .collect();
    let tree = signal_tree(&signals, "");
    let mut leaves = Vec::new();
    tree_indices(&tree, &mut leaves);
    assert_eq!(
        leaves.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![50, 50, 23]
    );
    let names: Vec<_> = leaves
        .into_iter()
        .flatten()
        .map(|index| signals[index].name.as_str())
        .collect();
    assert_eq!(names.first(), Some(&"signal-000"));
    assert_eq!(names.last(), Some(&"signal-122"));
    assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
}

#[test]
fn signal_tree_search_preserves_duplicate_records_and_balances_branches() {
    let signals: Vec<_> = (0..551)
        .map(|index| Signal {
            name: if index < 3 { "Same" } else { "Other" }.into(),
            metadata: [index as u8, (index >> 8) as u8, 0, 0, 0, 1],
        })
        .collect();
    let tree = signal_tree(&signals, "sAm");
    let mut leaves = Vec::new();
    tree_indices(&tree, &mut leaves);
    assert_eq!(leaves, vec![vec![0, 1, 2]]);
    let tree = signal_tree(&signals, "");
    assert!(tree.len() <= 10);
    let mut leaves = Vec::new();
    tree_indices(&tree, &mut leaves);
    assert!(leaves.iter().all(|leaf| leaf.len() <= 50));
    assert_eq!(leaves.into_iter().flatten().count(), 551);
}

#[test]
fn signal_tree_boundary_and_empty_search() {
    for count in [50, 51] {
        let signals = vec![
            Signal {
                name: "same name".into(),
                metadata: [1, 0, 0, 0, 0, 1],
            };
            count
        ];
        let mut leaves = Vec::new();
        tree_indices(&signal_tree(&signals, ""), &mut leaves);
        assert_eq!(leaves.len(), count.div_ceil(50));
        assert!(leaves.iter().all(|leaf| leaf.len() <= 50));
        assert!(signal_tree(&signals, "missing").is_empty());
    }
}

#[test]
fn choose_signature_requires_unambiguous_remote_name() {
    let entries = vec!["other.zig".to_owned(), "room.zig".to_owned()];
    assert_eq!(
        choose_signature(3, &entries, Some(Path::new("room.lpz"))).unwrap(),
        "room.zig"
    );
    assert!(choose_signature(3, &entries, None).is_err());
    assert_eq!(
        choose_signature(1, &["room.zig".into()], None).unwrap(),
        "room.zig"
    );
    assert!(choose_signature(0, &entries, None).is_err());
    assert!(choose_signature(1, &["../bad.zig".into()], None).is_err());
}

#[test]
fn large_batches_preserve_frames() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("hello", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    let frame = b"\x01\r\n00001010:1=42\r\n\x03";
    let batch = frame.repeat(10_000);
    assert!(batch.len() > 64 * 1024);
    panel.ingest(&batch, SystemTime::now());
    assert_eq!(panel.samples.len(), 10_000);
    let mut oversized = vec![b'x'; 100_000];
    oversized[0] = 1;
    oversized.extend_from_slice(frame);
    panel.ingest(&oversized, SystemTime::now());
    assert_eq!(panel.samples.len(), 10_001);
    assert!(panel.frames.is_empty());
}

#[test]
fn polling_is_bounded_without_losing_events() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("hello", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    panel.connect();
    let (_, output) = panel.pending_session.take().unwrap();
    for _ in 0..256 {
        output
            .try_send(DebugEvent::Output {
                bytes: b"\x01\r\n00001010:1=42\r\n\x03".to_vec(),
                received_at: SystemTime::now(),
            })
            .unwrap();
    }
    assert!(output.try_send(DebugEvent::Opened).is_err());
    panel.poll();
    assert!(panel.samples.len() <= 64);
    assert!(!panel.output.is_empty());
    for _ in 0..256 {
        panel.poll();
    }
    assert_eq!(panel.samples.len(), 256);
}

#[test]
fn framed_values_survive_chunk_boundaries() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("hello", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    panel.ingest(b"banner\x01\r\n00001010:1=", SystemTime::now());
    assert!(panel.samples.is_empty());
    panel.ingest(b"42\r\n\x03RMC3>", SystemTime::now());
    assert_eq!(panel.values[&0x1010], "42");
    assert_eq!(panel.samples.len(), 1);
}

#[test]
fn worker_receive_timestamp_is_preserved_for_a_split_debug_frame() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("hello", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    panel.connect();
    let (_, output) = panel.pending_session.take().unwrap();
    let first = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let completed = first + std::time::Duration::from_millis(42);
    output
        .try_send(DebugEvent::Output {
            bytes: b"\x01\r\n00001010:1=".to_vec(),
            received_at: first,
        })
        .unwrap();
    output
        .try_send(DebugEvent::Output {
            bytes: b"0\r\n\x03".to_vec(),
            received_at: completed,
        })
        .unwrap();
    panel.poll();
    assert_eq!(panel.samples[0].value, "0");
    assert_eq!(panel.samples[0].at, completed);
    assert!(format_received(completed).contains("2023"));
}

#[test]
fn bounded_events_and_empty_values_keep_their_signums() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("hello", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    panel.ingest(
        b"\x0100001010:1=\r\n\x03\x0100001010:1=2\r\n\x03",
        SystemTime::now(),
    );
    assert_eq!(panel.samples[0].value, "");
    assert_eq!(panel.samples[1].value, "2");
    for _ in 0..MAX_EVENTS {
        panel.ingest(b"\x0100001010:1=3\r\n\x03", SystemTime::now());
    }
    assert_eq!(panel.samples.len(), MAX_EVENTS);
    assert_eq!(panel.values[&0x1010], "3");
}

#[test]
fn clearing_timeline_keeps_watched_values_and_monitoring_active() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("hello", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    panel.groups.get_mut("Watched").unwrap().push(0x1010);
    panel.ingest(b"\x0100001010:1=42\r\n\x03", SystemTime::now());
    panel.clear_history();
    assert!(panel.samples.is_empty());
    assert_eq!(panel.values[&0x1010], "42");
    assert_eq!(panel.groups["Watched"], [0x1010]);
    panel.ingest(b"\x0100001010:1=43\r\n\x03", SystemTime::now());
    assert_eq!(panel.samples[0].value, "43");
}

#[test]
fn loaded_signature_monitors_every_unique_signum_without_selection() {
    let mut panel = Panel::new("processor".into(), "Processor");
    let mut bytes = fixture("A", [0x10, 0x10, 0, 0, 0, 1]);
    bytes.extend_from_slice(&fixture("B", [0x11, 0x10, 0, 0, 0, 1])[HEADER.len()..]);
    bytes.extend_from_slice(&fixture("A alias", [0x10, 0x10, 0, 0, 0, 1])[HEADER.len()..]);
    panel.signals = parse_signals(&bytes).unwrap();
    assert_eq!(debug_signums(&panel.signals), [0x1010, 0x1011]);
    panel.ingest(b"\x01\r\n00001011:1=42\r\n\x03", SystemTime::now());
    assert_eq!(panel.samples[0].signum, 0x1011);
    assert_eq!(panel.samples[0].value, "42");
    assert!(panel.groups["Watched"].is_empty());
}

#[test]
fn value_received_cells_render_the_selected_format_for_existing_samples() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("Serial", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    panel.ingest(b"\x0100001010:1=\\x48\\x69\\x0D\r\n\x03", SystemTime::now());
    for (mode, expected) in [
        (ValueDisplay::Ascii, "Hi."),
        (ValueDisplay::Hex, "48 69 0D"),
        (ValueDisplay::Mixed, r"Hi\x0D"),
    ] {
        panel.display_types.insert(0x1010, mode);
        for pass in 0..3 {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1800.0, 1100.0),
                    )),
                    ..Default::default()
                },
                |_| {
                    panel.show(&ctx);
                },
            );
            // No hover or watch group: this text must be the actual table cell.
            if pass == 2 {
                assert!(
                    output.shapes.iter().any(|shape| matches!(&shape.shape,
                    egui::epaint::Shape::Text(text) if text.galley.text() == expected)),
                    "missing formatted cell {expected}"
                );
                assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape,
                    egui::epaint::Shape::Text(text) if text.galley.text() == panel.samples[0].value)));
            }
            output.drop_without_applying_deltas();
        }
    }
}

#[test]
fn bracketed_processor_serial_bytes_render_as_text_hex_and_mixed() {
    let received = "[31][30][31][32][32][30][32][36][30][30][33][35][32][37][30][30][30][31]";
    assert_eq!(
        display_value(received, ValueDisplay::Ascii),
        "101220260035270001"
    );
    assert_eq!(
        display_value(received, ValueDisplay::Hex),
        "31 30 31 32 32 30 32 36 30 30 33 35 32 37 30 30 30 31"
    );
    assert_eq!(
        display_value("[48][69][0d][00][FF]", ValueDisplay::Mixed),
        r"Hi\x0D\x00\xFF"
    );
    assert_eq!(
        display_value("[48][69][0d][00][FF]", ValueDisplay::Ascii),
        "Hi..."
    );
    for literal in ["[31][ZZ]", "[31", "[text]", "[31]literal"] {
        assert_eq!(display_value(literal, ValueDisplay::Ascii), literal);
    }
}

#[test]
fn serial_display_modes_decode_bytes_without_changing_raw_values() {
    for encoded in [
        r"\x48\x69\x0D\x00\xFF",
        "48 69 0D 00 FF",
        "0x48 0x69 0x0d 0x00 0xff",
    ] {
        assert_eq!(display_value(encoded, ValueDisplay::Ascii), "Hi...");
        assert_eq!(display_value(encoded, ValueDisplay::Hex), "48 69 0D 00 FF");
        assert_eq!(
            display_value(encoded, ValueDisplay::Mixed),
            r"Hi\x0D\x00\xFF"
        );
    }
    for value in ["0", "42", "1234", "hello world", "", "  text  ", r"\xZZ"] {
        assert_eq!(display_value(value, ValueDisplay::Ascii), value);
    }
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("Serial", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    panel.ingest(
        b"\x0100001010:1=\\x48\\x69\\x0D  \r\n\x03",
        SystemTime::now(),
    );
    assert_eq!(panel.samples[0].value, r"\x48\x69\x0D  ");
    panel.display_types.insert(0x1010, ValueDisplay::Mixed);
    assert_eq!(
        display_value(&panel.samples[0].value, panel.display_types[&0x1010]),
        "Hi\\x0D  "
    );
    panel.clear_history();
    assert!(panel.display_types.contains_key(&0x1010));
    panel.reset_signals();
    assert!(panel.display_types.is_empty());
}

#[test]
fn timeline_selection_and_context_menu_disable_and_reenable_a_signal() {
    fn frame(ctx: &egui::Context, panel: &mut Panel, events: Vec<egui::Event>) -> egui::FullOutput {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1800.0, 1100.0),
                )),
                events,
                ..Default::default()
            },
            |_| {
                panel.show(ctx);
            },
        );
        output.textures_delta.clear();
        output
    }
    fn position(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
        output
            .shapes
            .iter()
            .rev()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing text {label}"))
    }
    fn click(ctx: &egui::Context, panel: &mut Panel, pos: egui::Pos2, button: egui::PointerButton) {
        for pressed in [true, false] {
            frame(
                ctx,
                panel,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            )
            .drop_without_applying_deltas();
        }
    }
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("Selectable signal", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    panel.ingest(b"\x0100001010:1=42\r\n\x03", SystemTime::now());
    let original = panel.samples[0].id;
    for _ in 0..3 {
        frame(&ctx, &mut panel, vec![]).drop_without_applying_deltas();
    }
    let output = frame(&ctx, &mut panel, vec![]);
    click(
        &ctx,
        &mut panel,
        position(&output, "Selectable signal"),
        egui::PointerButton::Primary,
    );
    assert_eq!(panel.selected_sample, Some(original));
    let selected_output = frame(&ctx, &mut panel, vec![]);
    assert_eq!(
        position(&selected_output, "Selectable signal"),
        position(&output, "Selectable signal"),
        "selection must not insert a detail panel or move the rows"
    );
    // New events must not move selection to another record.
    panel.ingest(b"\x0100001010:1=43\r\n\x03", SystemTime::now());
    assert_eq!(panel.selected_sample, Some(original));
    for _ in 0..2 {
        frame(&ctx, &mut panel, vec![]).drop_without_applying_deltas();
    }
    let output = frame(&ctx, &mut panel, vec![]);
    click(
        &ctx,
        &mut panel,
        position(&output, "Selectable signal"),
        egui::PointerButton::Secondary,
    );
    for _ in 0..2 {
        frame(&ctx, &mut panel, vec![]).drop_without_applying_deltas();
    }
    let output = frame(&ctx, &mut panel, vec![]);
    click(
        &ctx,
        &mut panel,
        position(&output, "Disable debugging locally"),
        egui::PointerButton::Primary,
    );
    assert!(panel.disabled_signals.contains(&0x1010));
    panel.ingest(b"\x0100001010:1=44\r\n\x03", SystemTime::now());
    assert_eq!(panel.samples.len(), 2);
    assert_eq!(panel.values[&0x1010], "44");
    for _ in 0..2 {
        frame(&ctx, &mut panel, vec![]).drop_without_applying_deltas();
    }
    let output = frame(&ctx, &mut panel, vec![]);
    click(
        &ctx,
        &mut panel,
        position(&output, "Selectable signal"),
        egui::PointerButton::Secondary,
    );
    for _ in 0..2 {
        frame(&ctx, &mut panel, vec![]).drop_without_applying_deltas();
    }
    let output = frame(&ctx, &mut panel, vec![]);
    click(
        &ctx,
        &mut panel,
        position(&output, "Enable debugging locally"),
        egui::PointerButton::Primary,
    );
    assert!(panel.disabled_signals.is_empty());
    panel.ingest(b"\x0100001010:1=45\r\n\x03", SystemTime::now());
    assert_eq!(panel.samples.len(), 3);
    let output = frame(&ctx, &mut panel, vec![]);
    click(
        &ctx,
        &mut panel,
        position(&output, "Selectable signal"),
        egui::PointerButton::Secondary,
    );
    for _ in 0..2 {
        frame(&ctx, &mut panel, vec![]).drop_without_applying_deltas();
    }
    let output = frame(&ctx, &mut panel, vec![]);
    click(
        &ctx,
        &mut panel,
        position(&output, "Display type"),
        egui::PointerButton::Primary,
    );
    for _ in 0..2 {
        frame(&ctx, &mut panel, vec![]).drop_without_applying_deltas();
    }
    let output = frame(&ctx, &mut panel, vec![]);
    click(
        &ctx,
        &mut panel,
        position(&output, "Mixed"),
        egui::PointerButton::Primary,
    );
    assert!(panel.display_types.get(&0x1010) == Some(&ValueDisplay::Mixed));
    panel.groups.insert("Custom".into(), vec![]);
    let tree_selection = panel.tree_selection.clone();
    for group in ["Custom", "Custom", "Watched"] {
        let output = frame(&ctx, &mut panel, vec![]);
        click(
            &ctx,
            &mut panel,
            position(&output, "Selectable signal"),
            egui::PointerButton::Secondary,
        );
        for _ in 0..2 {
            frame(&ctx, &mut panel, vec![]).drop_without_applying_deltas();
        }
        let output = frame(&ctx, &mut panel, vec![]);
        click(
            &ctx,
            &mut panel,
            position(&output, "Add to watch group"),
            egui::PointerButton::Primary,
        );
        for _ in 0..2 {
            frame(&ctx, &mut panel, vec![]).drop_without_applying_deltas();
        }
        let output = frame(&ctx, &mut panel, vec![]);
        click(
            &ctx,
            &mut panel,
            position(&output, group),
            egui::PointerButton::Primary,
        );
        assert_eq!(panel.groups[group], [0x1010]);
        assert_eq!(panel.tree_selection, tree_selection);
        assert_eq!(panel.timeline_group, None);
        assert!(panel.disabled_signals.is_empty());
    }
    panel.clear_history();
    assert_eq!(panel.selected_sample, None);
}

#[test]
fn timeline_shows_newest_changes_in_a_virtualized_table() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("Example signal", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    for n in 0..MAX_EVENTS {
        panel.samples.push_back(Sample {
            id: n as u64,
            signum: 0x1010,
            value: format!("change-{n:05}"),
            at: SystemTime::now(),
        });
    }
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1800.0, 1100.0),
        )),
        ..Default::default()
    };
    for _ in 0..3 {
        ctx.run_ui(input(), |_| {
            panel.show(&ctx);
        })
        .drop_without_applying_deltas();
    }
    let output = ctx.run_ui(input(), |_| {
        panel.show(&ctx);
    });
    let text: Vec<_> = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        })
        .collect();
    assert!(text.iter().any(|s| s.contains("Clear signals")));
    assert!(text.iter().any(|s| s.contains("Received (local)")));
    assert!(
        text.iter()
            .any(|s| s.contains(&format_received(panel.samples.back().unwrap().at)))
    );
    assert!(text.iter().any(|s| s.contains("Signal name")));
    assert!(text.iter().any(|s| s.contains("Value received")));
    assert!(text.iter().any(|s| s.contains("change-19999")));
    assert!(text.iter().any(|s| s.contains("Example signal")));
    assert!(text.iter().filter(|s| s.contains("change-")).count() < 100);
    let position = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == "Clear signals" => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        })
        .expect("clear button is visible");
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
        ctx.run_ui(raw, |_| {
            panel.show(&ctx);
        })
        .drop_without_applying_deltas();
    }
    assert!(panel.samples.is_empty());
}

#[test]
fn stale_fetch_is_ignored_after_stop_and_disconnected_fetch_reports_error() {
    let mut panel = Panel::new("processor".into(), "Processor");
    let (reply, fetch) = mpsc::channel();
    panel.fetch = Some(fetch);
    panel.stop();
    assert!(
        reply
            .send(Ok(fixture("stale", [0x10, 0x10, 0, 0, 0, 1])))
            .is_err()
    );
    panel.poll();
    assert!(panel.signals.is_empty());
    let (reply, fetch) = mpsc::channel();
    panel.fetch = Some(fetch);
    drop(reply);
    panel.poll();
    assert!(panel.fetch.is_none());
    assert!(panel.status.contains("unexpectedly"));
}

#[test]
fn stop_waits_for_worker_acknowledgement() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.connect();
    panel.session_sent = true;
    let (mut commands, output) = panel.pending_session.take().unwrap();
    panel.stop();
    assert!(panel.stopping);
    assert!(matches!(commands.try_recv(), Ok(DebugCommand::Stop)));
    output.try_send(DebugEvent::Closed(String::new())).unwrap();
    panel.poll();
    assert!(!panel.stopping);
    assert_eq!(panel.status, "Debugger stopped");
}

#[test]
fn lost_debugger_worker_reports_unconfirmed_cleanup() {
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.connect();
    panel.session_sent = true;
    let (_, output) = panel.pending_session.take().unwrap();
    panel.stop();
    drop(output);
    panel.poll();
    assert!(!panel.stopping);
    assert!(!panel.session_sent);
    assert!(
        panel
            .cleanup_error
            .as_deref()
            .unwrap()
            .contains("not confirmed")
    );
}

#[test]
fn windows_program_assignment_chooses_matching_signature() {
    assert_eq!(
        choose_signature(
            1,
            &["W classrooms.zig".into(), "other.zig".into()],
            Some(Path::new(r"C:\Working Directory\W classrooms.lpz"))
        )
        .unwrap(),
        "W classrooms.zig"
    );
}

#[test]
fn window_renders_three_panels_and_program_slot() {
    let ctx = egui::Context::default();
    let mut panel = Panel::new("processor".into(), "Processor");
    panel.signals = parse_signals(&fixture("hello", [0x10, 0x10, 0, 0, 0, 1])).unwrap();
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1800.0, 1100.0),
        )),
        ..Default::default()
    };
    for _ in 0..3 {
        ctx.run_ui(input(), |_| {
            panel.show(&ctx);
        })
        .drop_without_applying_deltas();
    }
    let output = ctx.run_ui(input(), |_| {
        panel.show(&ctx);
    });
    let text: Vec<_> = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        })
        .collect();
    let result = ["Signals", "Watch groups", "Signal timeline", "Slot 1"];
    let missing: Vec<_> = result
        .into_iter()
        .filter(|label| !text.iter().any(|value| value.contains(label)))
        .collect();
    output.drop_without_applying_deltas();
    assert!(missing.is_empty(), "missing {missing:?}: {text:?}");
}
