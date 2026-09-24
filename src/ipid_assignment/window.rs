use std::sync::mpsc::{Receiver, TryRecvError};

use eframe::egui;

use super::{AddressMode, Device, RowKey, address_key, master_address, program_rows};

pub(crate) struct Row {
    pub key: RowKey,
    pub selected: Option<String>,
    pub status: String,
    pub reply: Option<Receiver<Result<String, String>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct Job {
    pub row: usize,
    pub id: String,
    pub ipid: String,
    pub master: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Reload,
    Go,
    Discover,
}

pub(crate) struct Panel {
    pub token: u64,
    pub open: bool,
    pub processor: Device,
    pub candidates: Vec<Device>,
    pub program: u8,
    pub mode: AddressMode,
    pub rows: Vec<Row>,
    pub valid: bool,
    pub raw: String,
    pub message: String,
    pub table_reply: Option<Receiver<Result<String, String>>>,
    pub awaiting_credentials: bool,
    pub discovering: bool,
}

/// The observed model when discovered, otherwise the address book's.
fn device_model(device: &Device) -> &str {
    device
        .discovered
        .as_ref()
        .map_or(&device.model, |d| &d.model)
        .trim()
}

pub(super) fn model_matches(expected: &str, device: &Device) -> bool {
    !expected.trim().is_empty() && expected.trim().eq_ignore_ascii_case(device_model(device))
}

fn device_label(device: &Device) -> String {
    let source = if device.discovered.is_some() {
        "discovered"
    } else {
        "address book"
    };
    format!("{} ({})  ·  {source}", device.display_name(), device.host)
}

impl Panel {
    pub fn new(token: u64, processor: Device, candidates: Vec<Device>) -> Self {
        Self {
            token,
            open: true,
            processor,
            candidates,
            program: 1,
            mode: AddressMode::Ip,
            rows: Vec::new(),
            valid: false,
            raw: String::new(),
            message: String::new(),
            table_reply: None,
            awaiting_credentials: false,
            discovering: false,
        }
    }

    pub fn busy(&self) -> bool {
        self.awaiting_credentials
            || self.table_reply.is_some()
            || self.rows.iter().any(|row| row.reply.is_some())
    }

    pub fn accept_table(&mut self, result: Result<String, String>) {
        self.valid = false;
        self.rows.clear();
        match result {
            Err(error) => self.message = error,
            Ok(raw) => {
                self.raw = raw;
                match program_rows(&self.raw, self.program) {
                    Err(error) => self.message = error,
                    Ok(rows) => {
                        self.rows = rows
                            .into_iter()
                            .map(|key| Row {
                                key,
                                selected: None,
                                status: "Skip".into(),
                                reply: None,
                            })
                            .collect();
                        self.valid = true;
                        self.message = if self.rows.is_empty() {
                            "No entries in this program".into()
                        } else {
                            "Select devices, then press GO".into()
                        };
                    }
                }
            }
        }
    }

    pub fn poll(&mut self) {
        if let Some(reply) = self.table_reply.take() {
            match reply.try_recv() {
                Ok(result) => self.accept_table(result),
                Err(TryRecvError::Empty) => self.table_reply = Some(reply),
                Err(TryRecvError::Disconnected) => {
                    self.accept_table(Err("Table worker stopped".into()))
                }
            }
        }
        for row in &mut self.rows {
            if let Some(reply) = row.reply.take() {
                match reply.try_recv() {
                    Ok(Ok(message)) => row.status = message,
                    Ok(Err(error)) => row.status = format!("Failed: {error}"),
                    Err(TryRecvError::Empty) => row.reply = Some(reply),
                    Err(TryRecvError::Disconnected) => row.status = "Failed: worker stopped".into(),
                }
            }
        }
    }

    pub fn jobs(&self) -> Result<Vec<Job>, String> {
        if !self.valid {
            return Err("Load a valid program IP table first".into());
        }
        let master = master_address(&self.processor, self.mode)?;
        let mut endpoints = std::collections::BTreeSet::new();
        let mut macs = std::collections::BTreeSet::new();
        let mut jobs = Vec::new();
        for (index, row) in self.rows.iter().enumerate() {
            let Some(id) = &row.selected else {
                continue;
            };
            let device = self
                .candidates
                .iter()
                .find(|d| &d.id == id)
                .ok_or_else(|| "Selected device is no longer available".to_owned())?;
            let endpoint = address_key(
                device
                    .discovered
                    .as_ref()
                    .map_or(&device.host, |identity| &identity.ip),
            )?;
            if device.id == self.processor.id
                || (!device.mac.is_empty() && device.mac.eq_ignore_ascii_case(&self.processor.mac))
                || self
                    .processor
                    .discovered
                    .as_ref()
                    .is_some_and(|p| address_key(&p.ip).ok().as_ref() == Some(&endpoint))
                || address_key(&self.processor.host).ok().as_ref() == Some(&endpoint)
            {
                return Err("The processor cannot be assigned to its own table".into());
            }
            if !endpoints.insert(endpoint)
                || (!device.mac.is_empty() && !macs.insert(device.mac.to_ascii_lowercase()))
            {
                return Err("Assign each device only once".into());
            }
            jobs.push(Job {
                row: index,
                id: id.clone(),
                ipid: row.key.ipid.clone(),
                master: master.clone(),
            });
        }
        if jobs.is_empty() {
            return Err("No assignments selected".into());
        }
        Ok(jobs)
    }

    pub fn validate_live(&self, devices: &[Device], jobs: &[Job]) -> Result<(), String> {
        let unchanged = |snapshot: &Device| {
            devices
                .iter()
                .find(|d| d.id == snapshot.id)
                .is_some_and(|d| {
                    d.host == snapshot.host
                        && d.port == snapshot.port
                        && d.kind == snapshot.kind
                        && d.model == snapshot.model
                        && d.mac == snapshot.mac
                        && d.discovered == snapshot.discovered
                })
        };
        if !unchanged(&self.processor) {
            return Err("Processor changed or was removed; reopen Assign IPIDs".into());
        }
        for job in jobs {
            let snapshot = self
                .candidates
                .iter()
                .find(|d| d.id == job.id)
                .ok_or_else(|| "Assignment target is missing".to_owned())?;
            if !unchanged(snapshot) {
                return Err("A target changed or was removed; reopen Assign IPIDs".into());
            }
        }
        Ok(())
    }

    pub fn show(&mut self, ctx: &egui::Context, externally_blocked: bool) -> Option<Action> {
        if !self.open {
            return None;
        }
        let mut action = None;
        let locked = self.busy() || externally_blocked;
        self.open = crate::popout::window(ctx, "Assign IPIDs", [1080.0, 800.0], |ui| {
            egui::Frame::new().inner_margin(14).show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 6.0);
                ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
                egui::ScrollArea::both()
                    .id_salt("ipid_window")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Assign IPIDs").size(26.0).strong());
                        ui.label("Choose a program, match its IPIDs to address book or discovered devices, then apply the assignments.");
                        ui.add_space(4.0);
                        self.configuration(ui, locked, &mut action);
                        ui.add_space(4.0);
                        self.assignments(ui, locked, &mut action);
                        ui.add_space(4.0);
                        self.apply_controls(ui, locked, &mut action);
                        ui.collapsing("Raw processor response", |ui| {
                            ui.monospace(&self.raw);
                        });
                    });
            });
        });
        if matches!(action, Some(Action::Reload)) {
            self.valid = false;
            self.rows.clear();
        }
        action
    }

    fn configuration(&mut self, ui: &mut egui::Ui, locked: bool, action: &mut Option<Action>) {
        card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            section_heading(ui, "01", "Processor & program");
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(self.processor.display_name())
                        .size(18.0)
                        .strong(),
                );
                ui.monospace(&self.processor.host);
                if !self.processor.model.is_empty() {
                    ui.weak(&self.processor.model);
                }
            });
            ui.separator();
            ui.add_enabled_ui(!locked, |ui| {
                ui.columns(2, |columns| {
                    let ui = &mut columns[0];
                    ui.strong("Program");
                    let previous = self.program;
                    ui.horizontal_wrapped(|ui| {
                        egui::ComboBox::from_id_salt("ipid_program")
                            .width(100.0)
                            .selected_text(self.program.to_string())
                            .show_ui(ui, |ui| {
                                for program in 1..=10 {
                                    ui.selectable_value(
                                        &mut self.program,
                                        program,
                                        program.to_string(),
                                    );
                                }
                            });
                        if ui.button("Reload table").clicked() {
                            *action = Some(Action::Reload);
                        }
                    });
                    ui.weak("Reloading clears the current device selections.");
                    if previous != self.program {
                        *action = Some(Action::Reload);
                    }

                    let ui = &mut columns[1];
                    ui.strong("Processor address format");
                    let previous_mode = self.mode;
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(&mut self.mode, AddressMode::Ip, "IP address");
                        ui.selectable_value(&mut self.mode, AddressMode::Hostname, "HOSTNAME");
                    });
                    match master_address(&self.processor, self.mode) {
                        Ok(address) => {
                            ui.horizontal_wrapped(|ui| {
                                ui.weak("Master to write:");
                                ui.monospace(address);
                            });
                        }
                        Err(error) => {
                            ui.colored_label(ui.visuals().error_fg_color, error);
                        }
                    }
                    if previous_mode != self.mode {
                        for row in &mut self.rows {
                            row.status = if row.selected.is_some() {
                                "Ready".into()
                            } else {
                                "Skip".into()
                            };
                        }
                    }
                });
            });
        });
    }

    fn assignments(&mut self, ui: &mut egui::Ui, locked: bool, action: &mut Option<Action>) {
        card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            section_heading(ui, "02", "Device assignments");
            let options: Vec<_> = self.candidates.iter().collect();
            let discovered = options.iter().filter(|d| d.discovered.is_some()).count();
            ui.horizontal_wrapped(|ui| {
                ui.weak(format!(
                    "{} IPIDs in program {}  ·  {} address book  ·  {discovered} discovered devices",
                    self.rows.len(),
                    self.program,
                    options.len() - discovered,
                ));
                if ui
                    .add_enabled(
                        !self.discovering && action.is_none(),
                        egui::Button::new("Discover devices"),
                    )
                    .on_hover_text("Scan the network for devices not in the address book")
                    .clicked()
                {
                    *action = Some(Action::Discover);
                }
            });
            ui.horizontal_wrapped(|ui| {
                if self.busy() {
                    ui.spinner();
                }
                if !self.message.is_empty() {
                    let color = if !self.valid && !self.busy() {
                        ui.visuals().error_fg_color
                    } else {
                        ui.visuals().text_color()
                    };
                    ui.colored_label(color, &self.message);
                }
            });
            if self.discovering {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Discovering devices… Choices update as devices are found.");
                });
            }
            ui.separator();
            if self.rows.is_empty() {
                ui.add_space(16.0);
                ui.strong(if self.valid {
                    "This program has no IPID entries"
                } else {
                    "Waiting for a program IP table"
                });
                ui.weak("Choose a program above or reload its table to see device assignments.");
                ui.add_space(16.0);
                return;
            }
            let device_width = (ui.available_width() - 486.0).max(260.0);
            egui::Grid::new("ipid_grid")
                .striped(true)
                .num_columns(4)
                .spacing([18.0, 8.0])
                .min_row_height(30.0)
                .show(ui, |ui| {
                    for (heading, width) in [
                        ("CIP_ID", 72.0),
                        ("Model Name", 160.0),
                        ("Device", device_width),
                        ("Result", 200.0),
                    ] {
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            ui.strong(heading);
                        });
                    }
                    ui.end_row();
                    for row in &mut self.rows {
                        badge(ui, &row.key.ipid, ui.visuals().hyperlink_color);
                        ui.vertical(|ui| {
                            ui.set_width(160.0);
                            ui.add(
                                egui::Label::new(egui::RichText::new(&row.key.model).strong())
                                    .wrap(),
                            );
                        });
                        let label = row
                            .selected
                            .as_ref()
                            .and_then(|id| options.iter().find(|d| &d.id == id))
                            .map(|d| device_label(d))
                            .unwrap_or_else(|| "Skip".into());
                        let previous = row.selected.clone();
                        ui.vertical(|ui| {
                            ui.set_width(device_width);
                            ui.add_enabled_ui(!locked, |ui| {
                                egui::ComboBox::from_id_salt((&row.key.ipid, &row.key.model))
                                    .width(device_width)
                                    .truncate()
                                    .selected_text(label)
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut row.selected, None, "Skip");
                                        for device in &options {
                                            ui.selectable_value(
                                                &mut row.selected,
                                                Some(device.id.clone()),
                                                device_label(device),
                                            );
                                        }
                                    });
                                if options.is_empty() {
                                    ui.small(
                                        "No devices available; add them to the address book or discover them",
                                    );
                                }
                            });
                            if let Some(device) = options
                                .iter()
                                .find(|device| row.selected.as_ref() == Some(&device.id))
                                && !model_matches(&row.key.model, device)
                            {
                                let model = Some(device_model(device))
                                    .filter(|model| !model.is_empty())
                                    .unwrap_or("unknown");
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    format!(
                                        "Warning: model mismatch (expected {}, selected {model})",
                                        row.key.model
                                    ),
                                );
                            }
                        });
                        if previous != row.selected {
                            row.status = if row.selected.is_some() {
                                "Ready".into()
                            } else {
                                "Skip".into()
                            };
                        }
                        ui.vertical(|ui| {
                            ui.set_width(200.0);
                            let color = if row.status.starts_with("Failed") {
                                ui.visuals().error_fg_color
                            } else if matches!(row.status.as_str(), "Verified" | "Already correct")
                            {
                                if ui.visuals().dark_mode {
                                    egui::Color32::from_rgb(100, 205, 145)
                                } else {
                                    egui::Color32::from_rgb(25, 115, 65)
                                }
                            } else if row.selected.is_some() {
                                ui.visuals().hyperlink_color
                            } else {
                                ui.visuals().weak_text_color()
                            };
                            badge(ui, &row.status, color);
                        });
                        ui.end_row();
                    }
                });
        });
    }

    fn apply_controls(&self, ui: &mut egui::Ui, locked: bool, action: &mut Option<Action>) {
        card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            section_heading(ui, "03", "Review & apply");
            let selected = self.rows.iter().filter(|row| row.selected.is_some()).count();
            let jobs = self.jobs();
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.strong(format!("{selected} selected  ·  {} skipped", self.rows.len() - selected));
                    ui.weak("Only selected devices will be updated.");
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(
                        !locked && action.is_none() && jobs.is_ok(),
                        egui::Button::new(egui::RichText::new("GO").strong().size(17.0))
                            .min_size(egui::vec2(120.0, 36.0))
                            .fill(ui.visuals().selection.bg_fill)
                            .stroke(ui.visuals().selection.stroke),
                    ).on_hover_text("Apply the selected IPID assignments").clicked() {
                        *action = Some(Action::Go);
                    }
                });
            });
            if let Err(error) = &jobs {
                ui.colored_label(if selected == 0 { ui.visuals().weak_text_color() } else { ui.visuals().warn_fg_color }, error);
            }
            ui.small("GO replaces only matching-IPID entries on selected devices. Other IPIDs are preserved.");
        });
    }
}

fn card(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(10)
        .inner_margin(12)
}

fn section_heading(ui: &mut egui::Ui, number: &str, title: &str) {
    ui.horizontal(|ui| {
        badge(ui, number, ui.visuals().hyperlink_color);
        ui.label(egui::RichText::new(title).size(17.0).strong());
    });
}

fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .corner_radius(5)
        .inner_margin(egui::Margin::symmetric(7, 3))
        .show(ui, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(text).strong().color(color)).wrap());
        });
}
