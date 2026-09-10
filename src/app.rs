use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
};

use eframe::egui::{self, Color32, RichText};

use crate::{
    discovery::{self, DiscoveryEvent},
    model::{AddressEntry, ConnectionState, Device, DeviceKind, DeviceSource, endpoint_id},
    ssh::{ConnectionSpec, WorkerCommand, WorkerEvent, WorkerPool},
    storage::AppConfig,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceFilter {
    All,
    Processors,
    Touchpanels,
}

impl DeviceFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All devices",
            Self::Processors => "Processors",
            Self::Touchpanels => "Touchpanels",
        }
    }

    fn accepts(self, kind: DeviceKind) -> bool {
        match self {
            Self::All => true,
            Self::Processors => kind == DeviceKind::Processor,
            Self::Touchpanels => kind == DeviceKind::Touchpanel,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceView {
    All,
    Discovered,
    AddressBook,
}

/// Free-text device filter matched against the fields shown on a device card.
#[derive(Clone, Debug, Default)]
struct SearchQuery {
    /// Trimmed, lowercased query. An empty query matches every device.
    needle: String,
    /// Separator-stripped query, set only when it could be part of a MAC address.
    mac_needle: Option<String>,
}

impl SearchQuery {
    fn new(raw: &str) -> Self {
        let needle = raw.trim().to_ascii_lowercase();
        let stripped: String = needle
            .chars()
            .filter(|character| !matches!(character, ':' | '-' | '.') && !character.is_whitespace())
            .collect();
        let mac_needle = (stripped.len() >= 2
            && stripped
                .chars()
                .all(|character| character.is_ascii_hexdigit()))
        .then_some(stripped);
        Self { needle, mac_needle }
    }

    fn matches(&self, device: &Device) -> bool {
        if self.needle.is_empty() {
            return true;
        }
        let fields = [
            &device.model,
            &device.name,
            &device.host,
            &device.mac,
            &device.firmware,
        ];
        if fields
            .iter()
            .any(|field| field.to_ascii_lowercase().contains(&self.needle))
        {
            return true;
        }
        // A pasted MAC may omit the separators used by the stored form.
        self.mac_needle.as_ref().is_some_and(|mac_needle| {
            device
                .mac
                .chars()
                .filter(|character| *character != ':')
                .collect::<String>()
                .to_ascii_lowercase()
                .contains(mac_needle)
        })
    }
}

#[derive(Default)]
struct AddressDraft {
    name: String,
    host: String,
    port: u16,
    username: String,
    password: String,
    kind: DeviceKind,
}

#[derive(Clone, Default)]
struct PreferencesDraft {
    default_username: String,
    default_password: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingAction {
    Open(PathBuf),
    Quit,
}

pub struct LoadRunnerApp {
    config: AppConfig,
    devices: Vec<Device>,
    selected_id: Option<String>,
    filter: DeviceFilter,
    view: DeviceView,
    search: String,
    worker_pool: WorkerPool,
    worker_events: Receiver<WorkerEvent>,
    discovery_events: Receiver<DiscoveryEvent>,
    discovery_sender: std::sync::mpsc::Sender<DiscoveryEvent>,
    discovering: bool,
    discard_discovery_results: bool,
    pending_host_keys: HashMap<String, String>,
    add_device_open: bool,
    address_draft: AddressDraft,
    current_address_book: Option<PathBuf>,
    address_book_dirty: bool,
    pending_action: Option<PendingAction>,
    close_approved: bool,
    status_message: String,
    status_is_error: bool,
    about_open: bool,
    preferences_open: bool,
    preferences_draft: PreferencesDraft,
    firmware_editor: crate::firmware::FirmwareEditor,
    notice: Option<String>,
}

impl LoadRunnerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (config, load_error) = AppConfig::load();
        let mut app = Self::from_config(config, load_error);
        app.firmware_editor = crate::firmware::FirmwareEditor::load(crate::storage::firmware_dir());
        app
    }

    fn from_config(config: AppConfig, load_error: Option<String>) -> Self {
        let status_is_error = load_error.is_some();
        let status_message = load_error.unwrap_or_else(|| "Ready".into());
        let devices = config
            .address_book
            .iter()
            .map(Device::from_address)
            .collect();
        let (worker_sender, worker_events) = mpsc::channel();
        let (discovery_sender, discovery_events) = mpsc::channel();
        let preferences_draft = PreferencesDraft {
            default_username: config.default_username.clone(),
            default_password: config.default_password.clone(),
        };
        Self {
            config,
            devices,
            selected_id: None,
            filter: DeviceFilter::All,
            view: DeviceView::All,
            search: String::new(),
            worker_pool: WorkerPool::new(worker_sender),
            worker_events,
            discovery_events,
            discovery_sender,
            discovering: false,
            discard_discovery_results: false,
            pending_host_keys: HashMap::new(),
            add_device_open: false,
            address_draft: AddressDraft {
                port: 22,
                ..Default::default()
            },
            current_address_book: None,
            address_book_dirty: false,
            pending_action: None,
            close_approved: false,
            status_message,
            status_is_error,
            about_open: false,
            preferences_open: false,
            preferences_draft,
            firmware_editor: Default::default(),
            notice: None,
        }
    }

    fn process_events(&mut self) {
        while let Ok(event) = self.worker_events.try_recv() {
            match event {
                WorkerEvent::JobFinished { id } => self.worker_pool.job_finished(&id),
                WorkerEvent::Connecting { id } => {
                    if let Some(device) = self.device_mut(&id) {
                        device.connection = ConnectionState::Connecting;
                        device.last_message = "Opening SSH connection".into();
                    }
                }
                WorkerEvent::HostKeyUnknown { id, fingerprint } => {
                    if let Some(device) = self.device_mut(&id) {
                        device.connection = ConnectionState::Disconnected;
                        device.last_message = "Verify the SSH host key before reconnecting".into();
                    }
                    self.pending_host_keys.insert(id, fingerprint);
                }
                WorkerEvent::Details { id, details } => {
                    if let Some(device) = self.device_mut(&id) {
                        device.connection = ConnectionState::Connected;
                        device.details = Some(details);
                        device.last_message = "Device information refreshed".into();
                    }
                }
                WorkerEvent::Progress { id, sent, total } => {
                    if let Some(device) = self.device_mut(&id) {
                        device.connection = ConnectionState::Busy;
                        device.progress = Some((sent, total));
                        device.last_message = format!("Uploading {sent} of {total} bytes");
                    }
                }
                WorkerEvent::Complete { id, message } => {
                    if let Some(device) = self.device_mut(&id) {
                        device.connection = ConnectionState::Connected;
                        device.progress = None;
                        device.last_message = message;
                    }
                }
                WorkerEvent::Error { id, message } => {
                    if let Some(device) = self.device_mut(&id) {
                        device.connection = ConnectionState::Error;
                        device.progress = None;
                        device.last_message = message;
                    }
                }
            }
        }

        while let Ok(event) = self.discovery_events.try_recv() {
            match event {
                DiscoveryEvent::Found(device) => {
                    if !self.discard_discovery_results {
                        self.merge_discovered(*device);
                    }
                }
                DiscoveryEvent::Finished(result) => {
                    self.discovering = false;
                    self.discard_discovery_results = false;
                    if let Err(error) = result {
                        self.notice = Some(format!("Discovery failed: {error}"));
                    }
                }
            }
        }
    }

    fn merge_discovered(&mut self, mut discovered: Device) {
        self.firmware_editor.observe_model(&discovered.model);
        if discovered.ssh_host_key_fingerprint.is_none() {
            let endpoint = endpoint_id(&discovered.host, discovered.port);
            discovered.ssh_host_key_fingerprint = self
                .config
                .legacy_trusted_host_keys
                .get(&discovered.id)
                .or_else(|| self.config.legacy_trusted_host_keys.get(&endpoint))
                .cloned();
        }
        if let Some(index) = self.devices.iter().position(|device| {
            device.id == discovered.id
                || endpoint_id(&device.host, device.port)
                    == endpoint_id(&discovered.host, discovered.port)
        }) {
            if self.devices[index].source == DeviceSource::Discovered
                && self.devices[index].host != discovered.host
            {
                let id = self.devices[index].id.clone();
                if let Err(error) = self.worker_pool.retire(&id) {
                    self.status_message =
                        format!("IP changed; rescan after operations finish: {error}");
                    self.status_is_error = true;
                    return;
                }
                self.pending_host_keys.remove(&id);
                self.devices[index].host = discovered.host.clone();
                self.devices[index].connection = ConnectionState::Disconnected;
                self.devices[index].details = None;
            }
            let address_book_changed = {
                let existing = &mut self.devices[index];
                let previous = (existing.source == DeviceSource::AddressBook)
                    .then(|| existing.to_address_entry());
                if existing.source == DeviceSource::Discovered || existing.name.trim().is_empty() {
                    existing.name = discovered.name;
                }
                existing.model = discovered.model;
                existing.firmware = discovered.firmware;
                existing.mac = discovered.mac;
                existing.kind = discovered.kind;
                if existing.ssh_host_key_fingerprint.is_none() {
                    existing.ssh_host_key_fingerprint = discovered.ssh_host_key_fingerprint;
                }
                existing.last_message = "Rediscovered on the local network".into();
                previous.is_some_and(|previous| previous != existing.to_address_entry())
            };
            if address_book_changed {
                self.mark_address_book_dirty();
            }
            return;
        }
        discovered.selected = false;
        self.devices.push(discovered);
        self.devices
            .sort_by_key(|device| device.display_name().to_ascii_lowercase());
    }

    fn device_mut(&mut self, id: &str) -> Option<&mut Device> {
        self.devices.iter_mut().find(|device| device.id == id)
    }

    fn connection_spec(&self, id: &str) -> Option<ConnectionSpec> {
        let device = self.devices.iter().find(|device| device.id == id)?;
        let mut credentials = device.credentials.clone();
        if credentials.username.trim().is_empty() {
            credentials.username = self.config.default_username.clone();
        }
        if credentials.password.is_empty() {
            credentials.password = self.config.default_password.clone();
        }
        Some(ConnectionSpec {
            id: device.id.clone(),
            host: device.host.clone(),
            port: device.port,
            credentials,
            trusted_fingerprint: device.ssh_host_key_fingerprint.clone().or_else(|| {
                self.config
                    .legacy_trusted_host_keys
                    .get(&device.id)
                    .cloned()
            }),
        })
    }

    fn start_discovery(&mut self) {
        if !self.discovering {
            self.discovering = true;
            self.discard_discovery_results = false;
            discovery::spawn(self.discovery_sender.clone());
        }
    }

    fn clear_devices(&mut self) {
        let removed = self.devices.len();
        let address_book_changed = !self.config.address_book.is_empty()
            || self
                .devices
                .iter()
                .any(|device| device.source == DeviceSource::AddressBook);
        self.discard_discovery_results = self.discovering;
        self.devices.clear();
        self.selected_id = None;
        self.pending_host_keys.clear();
        self.config.legacy_trusted_host_keys.clear();
        self.sync_config_from_devices();
        self.address_book_dirty |= address_book_changed;
        self.status_message = format!("Cleared {removed} device(s)");
        self.status_is_error = false;
    }

    fn open_preferences(&mut self) {
        self.preferences_draft = PreferencesDraft {
            default_username: self.config.default_username.clone(),
            default_password: self.config.default_password.clone(),
        };
        self.preferences_open = true;
    }

    fn save_preferences(&mut self) {
        let default_username = self.preferences_draft.default_username.trim().to_owned();
        let default_password = self.preferences_draft.default_password.clone();
        let mut updated = self.config.clone();
        updated.default_username = default_username.clone();
        updated.default_password = default_password.clone();
        match updated.save_preferences() {
            Ok(()) => {
                self.config.default_username = default_username;
                self.config.default_password = default_password;
                self.preferences_open = false;
                self.status_message = "Preferences saved".into();
                self.status_is_error = false;
            }
            Err(error) => {
                self.notice = Some(format!("Could not save preferences: {error}"));
            }
        }
    }

    fn refresh_device(&mut self, id: &str) {
        let Some(connection) = self.connection_spec(id) else {
            return;
        };
        if let Err(error) = self
            .worker_pool
            .send(id, WorkerCommand::Refresh(connection))
        {
            self.notice = Some(error);
        }
    }

    fn trust_host_key(&mut self, id: &str, fingerprint: String) {
        let Some(index) = self.devices.iter().position(|device| device.id == id) else {
            return;
        };
        let endpoint = endpoint_id(&self.devices[index].host, self.devices[index].port);
        let is_address_book = self.devices[index].source == DeviceSource::AddressBook;
        self.devices[index].ssh_host_key_fingerprint = Some(fingerprint);
        self.pending_host_keys.remove(id);
        self.config.legacy_trusted_host_keys.remove(id);
        self.config.legacy_trusted_host_keys.remove(&endpoint);

        if is_address_book {
            self.mark_address_book_dirty();
            self.status_message =
                "SSH host-key fingerprint added to the address book; save to persist it".into();
        } else {
            self.status_message =
                "SSH host key trusted for this session; add the device to the address book to persist it"
                    .into();
            self.status_is_error = false;
        }
        self.refresh_device(id);
    }

    fn load_assigned_programs(&mut self) {
        let jobs: Vec<(String, PathBuf, u8)> = self
            .devices
            .iter()
            .filter(|device| {
                device.selected
                    && device.source == DeviceSource::AddressBook
                    && device.kind == DeviceKind::Processor
            })
            .flat_map(|device| {
                device
                    .program_slots
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, path)| {
                        path.clone()
                            .map(|path| (device.id.clone(), path, (slot + 1) as u8))
                    })
            })
            .collect();
        if jobs.is_empty() {
            self.notice = Some(
                "Select an address-book processor that has at least one assigned program".into(),
            );
            return;
        }
        if let Some((_, path, _)) = jobs
            .iter()
            .find(|(_, path, _)| !file_has_extension(path, "lpz"))
        {
            self.notice = Some(format!(
                "Assigned program is not an .lpz file: {}",
                path.display()
            ));
            return;
        }

        for (id, path, slot) in jobs {
            let Some(connection) = self.connection_spec(&id) else {
                continue;
            };
            if let Err(error) = self.worker_pool.send(
                &id,
                WorkerCommand::UploadProgram {
                    connection,
                    local_path: path,
                    slot,
                },
            ) {
                self.notice = Some(error);
            }
        }
    }

    fn load_assigned_touchpanels(&mut self) {
        let jobs: Vec<(String, PathBuf)> = self
            .devices
            .iter()
            .filter(|device| {
                device.selected
                    && device.source == DeviceSource::AddressBook
                    && device.kind == DeviceKind::Touchpanel
            })
            .filter_map(|device| {
                device
                    .touchpanel_project
                    .clone()
                    .map(|path| (device.id.clone(), path))
            })
            .collect();
        if jobs.is_empty() {
            self.notice =
                Some("Select an address-book touchpanel that has an assigned project".into());
            return;
        }
        if let Some((_, path)) = jobs
            .iter()
            .find(|(_, path)| !file_has_extension(path, "vtz"))
        {
            self.notice = Some(format!(
                "Assigned touchpanel project is not a .vtz file: {}",
                path.display()
            ));
            return;
        }

        for (id, path) in jobs {
            let Some(connection) = self.connection_spec(&id) else {
                continue;
            };
            if let Err(error) = self.worker_pool.send(
                &id,
                WorkerCommand::UploadTouchpanel {
                    connection,
                    local_path: path,
                },
            ) {
                self.notice = Some(error);
            }
        }
    }

    fn load_assigned_firmware(&mut self) {
        let jobs: Vec<(String, crate::firmware::FirmwareAssignment)> = self
            .devices
            .iter()
            .filter(|device| device.selected && device.source == DeviceSource::AddressBook)
            .filter_map(|device| {
                self.firmware_editor
                    .assignment_for_model(&device.model)
                    .map(|assignment| (device.id.clone(), assignment))
            })
            .collect();
        if jobs.is_empty() {
            self.notice =
                Some("Select an address-book device whose model has assigned firmware".into());
            return;
        }
        if let Some((_, assignment)) = jobs
            .iter()
            .find(|(_, assignment)| !assignment.local_path.is_file())
        {
            self.notice = Some(format!(
                "Stored firmware file is missing: {}",
                assignment.local_path.display()
            ));
            return;
        }

        for (id, assignment) in jobs {
            let Some(connection) = self.connection_spec(&id) else {
                continue;
            };
            if let Err(error) = self.worker_pool.send(
                &id,
                WorkerCommand::UploadFirmware {
                    connection,
                    local_path: assignment.local_path,
                    remote_name: assignment.original_name,
                },
            ) {
                self.notice = Some(error);
            }
        }
    }

    fn sync_config_from_devices(&mut self) {
        self.config.address_book = self
            .devices
            .iter()
            .filter(|device| device.source == DeviceSource::AddressBook)
            .map(Device::to_address_entry)
            .collect();
    }

    fn mark_address_book_dirty(&mut self) {
        self.sync_config_from_devices();
        self.address_book_dirty = true;
        self.status_message = "Address book modified".into();
        self.status_is_error = false;
    }

    fn save_config(&mut self) -> bool {
        self.sync_config_from_devices();
        match self.write_current_address_book() {
            Ok(()) => {
                self.address_book_dirty = false;
                self.status_message = match &self.current_address_book {
                    Some(path) => format!("Saved {}", path.display()),
                    None => "Saved local address book".into(),
                };
                self.status_is_error = false;
                true
            }
            Err(error) => {
                self.address_book_dirty = true;
                self.status_message = format!("Could not save address book: {error}");
                self.status_is_error = true;
                false
            }
        }
    }

    fn write_current_address_book(&self) -> std::io::Result<()> {
        if let Some(path) = &self.current_address_book {
            crate::storage::save_address_book(path, &self.config.address_book)?;
            let saved = crate::storage::load_address_book(path)?;
            if saved != self.config.address_book {
                return Err(std::io::Error::other(
                    "saved address book did not match the in-memory data",
                ));
            }
        }
        self.config.save()?;
        Ok(())
    }

    fn export_address_book(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name("crestron-address-book.json")
            .save_file()
        else {
            return;
        };
        self.sync_config_from_devices();
        let result =
            crate::storage::save_address_book(&path, &self.config.address_book).and_then(|_| {
                let saved = crate::storage::load_address_book(&path)?;
                if saved == self.config.address_book {
                    Ok(())
                } else {
                    Err(std::io::Error::other(
                        "saved address book did not match the in-memory data",
                    ))
                }
            });
        match result {
            Ok(()) => {
                self.current_address_book = Some(path.clone());
                self.address_book_dirty = false;
                self.status_message = format!("Saved {}", path.display());
                self.status_is_error = false;
                if let Err(error) = self.config.save() {
                    self.status_message = format!(
                        "Saved {}, but could not update the local address book: {error}",
                        path.display()
                    );
                    self.status_is_error = true;
                }
            }
            Err(error) => {
                self.address_book_dirty = true;
                self.status_message = format!("Could not save address book: {error}");
                self.status_is_error = true;
            }
        }
    }

    fn request_action(&mut self, action: PendingAction, ctx: &egui::Context) {
        if self.firmware_editor.is_busy() {
            self.status_message = "Wait for the firmware file import to finish".into();
            self.status_is_error = true;
            return;
        }
        if self.worker_pool.has_pending() {
            self.status_message = "Wait for queued device operations to finish before opening another address book or exiting".into();
            self.status_is_error = true;
            return;
        }
        if self.address_book_dirty {
            self.pending_action = Some(action);
        } else {
            self.execute_action(action, ctx);
        }
    }

    fn execute_action(&mut self, action: PendingAction, ctx: &egui::Context) {
        if let Err(error) = self.worker_pool.retire_all() {
            self.status_message = error;
            self.status_is_error = true;
            return;
        }
        match action {
            PendingAction::Open(path) => self.open_address_book(&path),
            PendingAction::Quit => {
                self.close_approved = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn confirm_pending_action(&mut self, save: bool, ctx: &egui::Context) {
        if self.firmware_editor.is_busy()
            || self.worker_pool.has_pending()
            || (save && !self.save_config())
        {
            return;
        }
        if let Some(action) = self.pending_action.take() {
            self.execute_action(action, ctx);
        }
    }

    fn import_address_book(&mut self, ctx: &egui::Context) {
        if self.worker_pool.has_pending() {
            self.status_message =
                "Wait for queued device operations before opening another address book".into();
            self.status_is_error = true;
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .pick_file()
        else {
            return;
        };
        self.request_action(PendingAction::Open(path), ctx);
    }

    fn open_address_book(&mut self, path: &Path) {
        if let Err(error) = self.worker_pool.retire_all() {
            self.status_message = error;
            self.status_is_error = true;
            return;
        }
        let result = crate::storage::validate_portable_path(path)
            .and_then(|()| crate::storage::load_address_book(path));
        match result {
            Ok(entries) => {
                let mut devices: Vec<Device> = entries.iter().map(Device::from_address).collect();
                devices.extend(
                    self.devices
                        .iter()
                        .filter(|device| {
                            device.source == DeviceSource::Discovered
                                && !entries.iter().any(|entry| {
                                    endpoint_id(&entry.host, entry.port)
                                        == endpoint_id(&device.host, device.port)
                                })
                        })
                        .cloned(),
                );
                self.config.address_book = entries;
                self.config.legacy_trusted_host_keys.clear();
                self.devices = devices;
                self.selected_id = None;
                self.pending_host_keys.clear();
                match self.config.save() {
                    Ok(()) => {
                        self.current_address_book = Some(path.to_owned());
                        self.address_book_dirty = false;
                        self.status_message = format!("Loaded {}", path.display());
                        self.status_is_error = false;
                    }
                    Err(error) => {
                        self.current_address_book = Some(path.to_owned());
                        self.address_book_dirty = false;
                        self.status_message = format!(
                            "Loaded {}, but could not update the local address book: {error}",
                            path.display()
                        );
                        self.status_is_error = true;
                    }
                }
            }
            Err(error) => {
                self.status_message = format!("Could not load address book: {error}");
                self.status_is_error = true;
            }
        }
    }

    fn add_address(&mut self) {
        let host = self.address_draft.host.trim().to_owned();
        if host.is_empty() || self.address_draft.port == 0 {
            self.notice = Some("Host and SSH port are required".into());
            return;
        }
        let entry = AddressEntry {
            name: self.address_draft.name.trim().to_owned(),
            host: host.clone(),
            port: self.address_draft.port,
            username: self.address_draft.username.trim().to_owned(),
            ssh_host_key_fingerprint: None,
            kind: self.address_draft.kind,
            model: String::new(),
            firmware: String::new(),
            mac: String::new(),
            program_slots: Default::default(),
            config_slots: Default::default(),
            touchpanel_project: None,
        };
        let id = endpoint_id(&host, entry.port);
        if let Some(index) = self
            .devices
            .iter()
            .position(|device| endpoint_id(&device.host, device.port) == id)
        {
            if self.devices[index].source == DeviceSource::AddressBook {
                self.selected_id = Some(self.devices[index].id.clone());
                self.notice = Some("That host and port are already in the address book".into());
                return;
            }
            let discovered_id = self.devices[index].id.clone();
            self.add_discovered_to_address_book(&discovered_id);
            let password = self.address_draft.password.clone();
            let Some(device) = self.device_mut(&id) else {
                return;
            };
            if !entry.name.is_empty() {
                device.name = entry.name;
            }
            if entry.kind != DeviceKind::Unknown {
                device.kind = entry.kind;
            }
            device.credentials.username = entry.username;
            device.credentials.password = password;
            self.add_device_open = false;
            self.address_draft = AddressDraft {
                port: 22,
                ..Default::default()
            };
            self.mark_address_book_dirty();
            return;
        }
        let mut device = Device::from_address(&entry);
        device.credentials.password = self.address_draft.password.clone();
        self.config.address_book.push(entry);
        self.devices.push(device);
        self.selected_id = Some(id);
        self.add_device_open = false;
        self.address_draft = AddressDraft {
            port: 22,
            ..Default::default()
        };
        self.mark_address_book_dirty();
    }

    fn add_discovered_to_address_book(&mut self, id: &str) {
        let Some(index) = self.devices.iter().position(|device| device.id == id) else {
            return;
        };
        if self.devices[index].source == DeviceSource::AddressBook {
            return;
        }

        let old_id = self.devices[index].id.clone();
        let new_id = endpoint_id(&self.devices[index].host, self.devices[index].port);
        if self.worker_pool.is_busy(&new_id) {
            self.status_message = "Wait for device operations before merging this address".into();
            self.status_is_error = true;
            return;
        }
        if let Err(error) = self.worker_pool.retire(&old_id) {
            self.status_message = error;
            self.status_is_error = true;
            return;
        }
        if let Some(existing) = self.devices.iter().enumerate().find_map(|(other, device)| {
            (other != index && endpoint_id(&device.host, device.port) == new_id)
                .then(|| device.id.clone())
        }) {
            let fingerprint = self.devices[index]
                .ssh_host_key_fingerprint
                .clone()
                .or_else(|| self.config.legacy_trusted_host_keys.remove(&old_id))
                .or_else(|| self.config.legacy_trusted_host_keys.remove(&new_id));
            self.devices.remove(index);
            self.pending_host_keys.remove(&old_id);
            self.selected_id = Some(existing.clone());
            if let Some(device) = self.device_mut(&existing) {
                device.selected = true;
                if device.ssh_host_key_fingerprint.is_none() {
                    device.ssh_host_key_fingerprint = fingerprint;
                }
            }
            self.mark_address_book_dirty();
            return;
        }
        let fingerprint = self
            .config
            .legacy_trusted_host_keys
            .remove(&old_id)
            .or_else(|| self.config.legacy_trusted_host_keys.remove(&new_id));
        if self.devices[index].ssh_host_key_fingerprint.is_none() {
            self.devices[index].ssh_host_key_fingerprint = fingerprint;
        }
        self.devices[index].id = new_id.clone();
        self.devices[index].source = DeviceSource::AddressBook;
        self.devices[index].selected = true;
        self.devices[index].last_message = "Added to address book".into();
        if let Some(fingerprint) = self.pending_host_keys.remove(&old_id) {
            self.pending_host_keys.insert(new_id.clone(), fingerprint);
        }
        self.selected_id = Some(new_id);
        self.mark_address_book_dirty();
    }

    fn remove_selected_address(&mut self) {
        let Some(id) = self.selected_id.clone() else {
            return;
        };
        let Some(device) = self.devices.iter().find(|device| device.id == id) else {
            return;
        };
        if device.source != DeviceSource::AddressBook {
            return;
        }
        let host = device.host.clone();
        let port = device.port;
        if let Err(error) = self.worker_pool.retire(&id) {
            self.status_message = error;
            self.status_is_error = true;
            return;
        }
        self.devices.retain(|device| device.id != id);
        self.config
            .address_book
            .retain(|entry| entry.host != host || entry.port != port);
        self.config.legacy_trusted_host_keys.remove(&id);
        self.pending_host_keys.remove(&id);
        self.selected_id = None;
        self.mark_address_book_dirty();
    }

    fn menu_bar(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("menu").show(root, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Add device…").clicked() {
                        self.add_device_open = true;
                        ui.close();
                    }
                    if ui.button("Open address book JSON…").clicked() {
                        self.import_address_book(ui.ctx());
                        ui.close();
                    }
                    if ui.button("Save address book as JSON…").clicked() {
                        self.export_address_book();
                        ui.close();
                    }
                    if ui.button("Save address book").clicked() {
                        self.save_config();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Preferences…").clicked() {
                        self.open_preferences();
                        ui.close();
                    }
                    if ui.button("Firmware Editor…").clicked() {
                        self.firmware_editor.open = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        self.request_action(PendingAction::Quit, ui.ctx());
                    }
                });
                ui.menu_button("Devices", |ui| {
                    if ui
                        .add_enabled(!self.discovering, egui::Button::new("Discover now"))
                        .clicked()
                    {
                        self.start_discovery();
                        ui.close();
                    }
                    if ui.button("Refresh selected device").clicked() {
                        if let Some(id) = self.selected_id.clone() {
                            self.refresh_device(&id);
                        }
                        ui.close();
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("About").clicked() {
                        self.about_open = true;
                        ui.close();
                    }
                });
                ui.separator();
                ui.label(RichText::new("CRESTRON LOAD RUNNER").strong());
                if self.discovering {
                    ui.spinner();
                    ui.label("Discovering…");
                }
            });
        });
    }

    fn action_bar(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("actions").show(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Add Device").clicked() {
                    self.add_device_open = true;
                }
                ui.separator();
                if ui.button("Load Assigned Program").clicked() {
                    self.load_assigned_programs();
                }
                if ui.button("Load Assigned Touchpanel").clicked() {
                    self.load_assigned_touchpanels();
                }
                if ui.button("Load Firmware").clicked() {
                    self.load_assigned_firmware();
                }
                ui.separator();
                let selected = self.devices.iter().filter(|device| device.selected).count();
                ui.label(format!("{selected} selected"));
            });
        });
    }

    fn status_bar(&self, root: &mut egui::Ui) {
        egui::Panel::bottom("status").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Address book:").strong());
                let path = self
                    .current_address_book
                    .clone()
                    .or_else(crate::storage::config_path);
                if let Some(path) = path {
                    ui.monospace(address_book_status_path(&path, self.address_book_dirty));
                } else {
                    ui.label(if self.address_book_dirty {
                        "Unavailable *"
                    } else {
                        "Unavailable"
                    });
                }
                ui.separator();
                ui.label(format!("{} device(s)", self.config.address_book.len()));
                ui.separator();
                let status = RichText::new(&self.status_message);
                ui.label(if self.status_is_error {
                    status.color(ui.visuals().error_fg_color)
                } else {
                    status
                });
            });
        });
    }

    fn devices_panel(&mut self, root: &mut egui::Ui) {
        let width = root.available_width() / 3.0;
        egui::Panel::left("devices")
            .resizable(true)
            .default_size(width)
            .min_size(300.0)
            .show(root, |ui| {
                ui.heading("Devices");
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.view, DeviceView::All, "All sources");
                    ui.selectable_value(&mut self.view, DeviceView::Discovered, "Discovered");
                    ui.selectable_value(&mut self.view, DeviceView::AddressBook, "Address book");
                    let search_box = ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("Search devices")
                            .desired_width(200.0),
                    );
                    if !self.search.is_empty() {
                        let clear_rect = egui::Rect::from_min_max(
                            egui::pos2(search_box.rect.right() - 20.0, search_box.rect.top()),
                            search_box.rect.right_bottom(),
                        )
                        .shrink(2.0);
                        if ui
                            .put(clear_rect, egui::Button::new("✕").small().frame(false))
                            .on_hover_text("Clear search")
                            .clicked()
                        {
                            self.search.clear();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("filter")
                        .selected_text(self.filter.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.filter,
                                DeviceFilter::Processors,
                                "Processors",
                            );
                            ui.selectable_value(
                                &mut self.filter,
                                DeviceFilter::Touchpanels,
                                "Touchpanels",
                            );
                            ui.selectable_value(&mut self.filter, DeviceFilter::All, "All devices");
                        });
                    if self.filter != DeviceFilter::All && ui.small_button("Clear filter").clicked()
                    {
                        self.filter = DeviceFilter::All;
                    }
                });
                ui.separator();

                let query = SearchQuery::new(&self.search);
                let visible_ids: Vec<String> = self
                    .devices
                    .iter()
                    .filter(|device| self.filter.accepts(device.kind))
                    .filter(|device| match self.view {
                        DeviceView::All => true,
                        DeviceView::Discovered => device.source == DeviceSource::Discovered,
                        DeviceView::AddressBook => device.source == DeviceSource::AddressBook,
                    })
                    .filter(|device| query.matches(device))
                    .map(|device| device.id.clone())
                    .collect();

                let selected_count = self.devices.iter().filter(|device| device.selected).count();
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{selected_count} selected"));
                    if ui
                        .add_enabled(
                            !visible_ids.is_empty(),
                            egui::Button::new("Select all visible"),
                        )
                        .clicked()
                    {
                        for device in &mut self.devices {
                            if device.source == DeviceSource::AddressBook
                                && visible_ids.contains(&device.id)
                            {
                                device.selected = true;
                            }
                        }
                    }
                    if ui
                        .add_enabled(selected_count > 0, egui::Button::new("Clear selection"))
                        .clicked()
                    {
                        for device in &mut self.devices {
                            device.selected = false;
                        }
                    }
                });
                ui.small("Address-book devices can be selected for concurrent loads.");
                ui.separator();

                egui::Panel::bottom("device_discovery_action")
                    .exact_size(48.0)
                    .show(ui, |ui| {
                        ui.horizontal_centered(|ui| {
                            let label = if self.discovering {
                                "Discovering…"
                            } else {
                                "Discover Devices"
                            };
                            if ui
                                .add_enabled(
                                    !self.discovering,
                                    egui::Button::new(label).min_size(egui::vec2(140.0, 32.0)),
                                )
                                .clicked()
                            {
                                self.start_discovery();
                            }
                            if ui
                                .add(
                                    egui::Button::new("Clear Devices")
                                        .min_size(egui::vec2(110.0, 32.0)),
                                )
                                .on_hover_text("Immediately clear all devices")
                                .clicked()
                            {
                                self.clear_devices();
                            }
                        });
                    });

                egui::ScrollArea::vertical().show(ui, |ui| {
                    if visible_ids.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(48.0);
                            ui.label(RichText::new("No matching devices").size(18.0));
                            ui.label("Run discovery, add an address, or clear the filters.");
                        });
                    }
                    for id in visible_ids {
                        self.device_card(ui, &id);
                        ui.add_space(6.0);
                    }
                });
            });
    }

    fn device_card(&mut self, ui: &mut egui::Ui, id: &str) {
        let is_current = self.selected_id.as_deref() == Some(id);
        let Some(index) = self.devices.iter().position(|device| device.id == id) else {
            return;
        };
        let frame = egui::Frame::group(ui.style()).fill(if is_current {
            ui.visuals().selection.bg_fill.gamma_multiply(0.45)
        } else {
            ui.visuals().faint_bg_color
        });
        let mut assignments_changed = false;
        let response = frame
            .show(ui, |ui| {
                ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
                    ui.set_min_width(ui.available_width());
                    let device = &mut self.devices[index];
                    let in_address_book = device.source == DeviceSource::AddressBook;
                    ui.horizontal(|ui| {
                        if in_address_book {
                            ui.checkbox(&mut device.selected, "Target")
                                .on_hover_text("Include this device in the next load");
                        }
                        ui.vertical(|ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(device.display_name()).strong().size(16.0));
                                device_type_badge(ui, device.kind);
                            });
                            if !device.model.is_empty() {
                                ui.label(
                                    RichText::new(&device.model)
                                        .strong()
                                        .size(19.0)
                                        .color(ui.visuals().strong_text_color()),
                                );
                            }
                            ui.horizontal_wrapped(|ui| {
                                ui.label(&device.host);
                                if !device.mac.trim().is_empty() {
                                    ui.separator();
                                    ui.label(format!("MAC {}", device.mac));
                                }
                            });
                            if in_address_book && !device.firmware.is_empty() {
                                ui.small(format!("Firmware {}", device.firmware));
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                            ui.vertical(|ui| {
                                if in_address_book {
                                    status_badge(ui, device.connection);
                                }
                                if in_address_book && device.kind == DeviceKind::Processor {
                                    assignments_changed |= slot_one_assignment(
                                        ui,
                                        "PROGRAM · SLOT 1",
                                        &mut device.program_slots[0],
                                        Some("lpz"),
                                    );
                                    assignments_changed |= slot_one_assignment(
                                        ui,
                                        "CONFIG · SLOT 1",
                                        &mut device.config_slots[0],
                                        None,
                                    );
                                }
                            });
                        });
                    });
                    if in_address_book {
                        match device.kind {
                            DeviceKind::Processor => {
                                for (slot, path) in device.program_slots.iter().enumerate().skip(1)
                                {
                                    if let Some(path) = path {
                                        assigned_file_row(ui, "Program", slot + 1, path);
                                    }
                                }
                                for (slot, path) in device.config_slots.iter().enumerate().skip(1) {
                                    if let Some(path) = path {
                                        assigned_file_row(ui, "Config", slot + 1, path);
                                    }
                                }
                            }
                            DeviceKind::Touchpanel => {
                                if let Some(path) = &device.touchpanel_project {
                                    assigned_project_row(ui, "Touchpanel", path);
                                }
                                assignments_changed |=
                                    project_assignment_button(ui, &mut device.touchpanel_project);
                            }
                            DeviceKind::Unknown => {}
                        }
                        if let Some((sent, total)) = device.progress {
                            let progress = if total == 0 {
                                0.0
                            } else {
                                sent as f32 / total as f32
                            };
                            ui.add(egui::ProgressBar::new(progress).show_percentage());
                        }
                        if !device.last_message.is_empty() {
                            ui.small(&device.last_message);
                        }
                    } else {
                        ui.small("Autodiscovered · right-click to add to the address book");
                    }
                })
                .response
            })
            .inner;
        let host = self.devices[index].host.clone();
        let mac = self.devices[index].mac.clone();
        let hostname = (self.devices[index].source == DeviceSource::Discovered
            && !self.devices[index].name.trim().is_empty()
            && self.devices[index].name != host)
            .then(|| self.devices[index].name.clone());
        let is_discovered = self.devices[index].source == DeviceSource::Discovered;
        let mut add_to_address_book = false;
        response.context_menu(|ui| {
            if is_discovered {
                if ui.button("Add to address book").clicked() {
                    add_to_address_book = true;
                    ui.close();
                }
                ui.separator();
            }
            let address_label = if host.parse::<std::net::IpAddr>().is_ok() {
                "Copy IP address"
            } else {
                "Copy hostname"
            };
            if ui.button(address_label).clicked() {
                ui.ctx().copy_text(host.clone());
                ui.close();
            }
            if let Some(hostname) = hostname
                .as_ref()
                .filter(|_| ui.button("Copy hostname").clicked())
            {
                ui.ctx().copy_text(hostname.clone());
                ui.close();
            }
            if !mac.trim().is_empty() && ui.button("Copy MAC address").clicked() {
                ui.ctx().copy_text(mac.clone());
                ui.close();
            }
        });
        if response.clicked() || response.secondary_clicked() {
            self.selected_id = Some(id.to_owned());
        }
        if assignments_changed {
            self.mark_address_book_dirty();
        }
        if add_to_address_book {
            self.add_discovered_to_address_book(id);
        }
    }

    fn details_panel(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default().show(root, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("device_details_scroll")
                .show(ui, |ui| {
                ui.heading("Device details");
                ui.separator();
                let Some(id) = self.selected_id.clone() else {
                    ui.add_space(40.0);
                    ui.label("Select a device to inspect it.");
                    return;
                };
                let Some(index) = self.devices.iter().position(|device| device.id == id) else {
                    return;
                };

                let mut refresh = false;
                let mut remove = false;
                let mut address_changed = false;
                {
                    let device = &mut self.devices[index];
                    ui.label(RichText::new(device.display_name()).strong().size(20.0));
                    if !device.model.is_empty() {
                        ui.label(RichText::new(&device.model).strong().size(18.0));
                    }
                    ui.label(format!("{}:{}", device.host, device.port));
                    ui.horizontal(|ui| {
                        status_badge(ui, device.connection);
                        if device.source == DeviceSource::AddressBook {
                            let previous_kind = device.kind;
                            egui::ComboBox::from_id_salt(("details_kind", &device.id))
                                .selected_text(device.kind.label())
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut device.kind,
                                        DeviceKind::Processor,
                                        "Processor",
                                    );
                                    ui.selectable_value(
                                        &mut device.kind,
                                        DeviceKind::Touchpanel,
                                        "Touchpanel",
                                    );
                                    ui.selectable_value(
                                        &mut device.kind,
                                        DeviceKind::Unknown,
                                        "Unknown",
                                    );
                                });
                            address_changed |= previous_kind != device.kind;
                        } else {
                            ui.label(device.kind.label());
                        }
                        ui.label(match device.source {
                            DeviceSource::Discovered => "Autodiscovery",
                            DeviceSource::AddressBook => "Address book",
                        });
                    });
                    if !device.mac.is_empty() {
                        ui.label(format!("MAC: {}", device.mac));
                    }
                    ui.add_space(10.0);
                    ui.collapsing("SSH credentials", |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Username");
                            address_changed |= ui
                                .text_edit_singleline(&mut device.credentials.username)
                                .changed();
                        });
                        ui.horizontal(|ui| {
                            ui.label("Password");
                            ui.add(
                                egui::TextEdit::singleline(&mut device.credentials.password)
                                    .password(true),
                            );
                        });
                        ui.small("Password is not saved to disk.");
                        if let Some(fingerprint) = &device.ssh_host_key_fingerprint {
                            ui.separator();
                            ui.label("SSH host-key fingerprint");
                            ui.monospace(fingerprint);
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Connect / refresh").clicked() {
                            refresh = true;
                        }
                        if device.source == DeviceSource::AddressBook
                            && ui.button("Remove").clicked()
                        {
                            remove = true;
                        }
                    });
                    if device.source == DeviceSource::AddressBook {
                        ui.add_space(8.0);
                        match device.kind {
                            DeviceKind::Processor => {
                                address_changed |= processor_assignments(ui, device);
                            }
                            DeviceKind::Touchpanel => {
                                address_changed |= touchpanel_assignment(ui, device);
                            }
                            DeviceKind::Unknown => {
                                ui.small("Set a device type before assigning load files.");
                            }
                        }
                    }
                }

                if address_changed {
                    self.mark_address_book_dirty();
                }

                if refresh {
                    self.refresh_device(&id);
                }
                if remove {
                    self.remove_selected_address();
                    return;
                }

                if let Some(fingerprint) = self.pending_host_keys.get(&id).cloned() {
                    ui.add_space(10.0);
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label(RichText::new("Untrusted SSH host key").strong());
                        ui.monospace(&fingerprint);
                        ui.label("Compare this fingerprint with a known-good value for the device.");
                        ui.horizontal(|ui| {
                            if ui.button("Trust this key").clicked() {
                                self.trust_host_key(&id, fingerprint.clone());
                            }
                            if ui.button("Reject").clicked() {
                                self.pending_host_keys.remove(&id);
                            }
                        });
                    });
                }

                ui.add_space(8.0);
                let details = self.devices[index].details.clone();
                if let Some(details) = details {
                    detail_section(ui, "Identity", &details.identity, true);
                    detail_section(ui, "Network", &details.network, true);
                    detail_section(ui, "Running programs", &details.programs, true);
                    detail_section(ui, "IP table", &details.ip_table, true);
                    if let Some(cresnet) = details.cresnet {
                        detail_section(ui, "Cresnet", &cresnet, false);
                    }
                } else {
                    ui.label("Connect to retrieve network, program, IP table, and Cresnet information.");
                }
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.pending_action.is_some() {
            egui::Modal::new(egui::Id::new("unsaved_address_book")).show(ctx, |ui| {
                ui.heading("Unsaved address-book changes");
                ui.label("Save your changes before continuing?");
                if self.status_is_error {
                    ui.colored_label(ui.visuals().error_fg_color, &self.status_message);
                }
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.confirm_pending_action(true, ctx);
                    }
                    if ui.button("Discard").clicked() {
                        self.confirm_pending_action(false, ctx);
                    }
                    if ui.button("Cancel").clicked() {
                        self.pending_action = None;
                    }
                });
            });
            return;
        }
        if self.add_device_open {
            let mut open = self.add_device_open;
            egui::Window::new("Add address-book device")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    egui::Grid::new("address_form")
                        .num_columns(2)
                        .spacing([12.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Name");
                            ui.text_edit_singleline(&mut self.address_draft.name);
                            ui.end_row();
                            ui.label("IP / hostname");
                            ui.text_edit_singleline(&mut self.address_draft.host);
                            ui.end_row();
                            ui.label("SSH port");
                            ui.add(
                                egui::DragValue::new(&mut self.address_draft.port).range(1..=65535),
                            );
                            ui.end_row();
                            ui.label("Device type");
                            egui::ComboBox::from_id_salt("address_kind")
                                .selected_text(self.address_draft.kind.label())
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.address_draft.kind,
                                        DeviceKind::Processor,
                                        "Processor",
                                    );
                                    ui.selectable_value(
                                        &mut self.address_draft.kind,
                                        DeviceKind::Touchpanel,
                                        "Touchpanel",
                                    );
                                    ui.selectable_value(
                                        &mut self.address_draft.kind,
                                        DeviceKind::Unknown,
                                        "Unknown",
                                    );
                                });
                            ui.end_row();
                            ui.label("Username");
                            ui.text_edit_singleline(&mut self.address_draft.username);
                            ui.end_row();
                            ui.label("Password");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.address_draft.password)
                                    .password(true),
                            );
                            ui.end_row();
                        });
                    ui.small("The address and username are saved. The password is session-only.");
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Add device").clicked() {
                            self.add_address();
                        }
                        if ui.button("Cancel").clicked() {
                            self.add_device_open = false;
                        }
                    });
                });
            self.add_device_open &= open;
        }

        if self.preferences_open {
            let mut open = self.preferences_open;
            egui::Window::new("Preferences")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("Default SSH credentials");
                    ui.small("Used when a device does not have its own username or password.");
                    ui.add_space(8.0);
                    egui::Grid::new("preferences_form")
                        .num_columns(2)
                        .spacing([12.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Username");
                            ui.text_edit_singleline(&mut self.preferences_draft.default_username);
                            ui.end_row();
                            ui.label("Password");
                            ui.add(
                                egui::TextEdit::singleline(
                                    &mut self.preferences_draft.default_password,
                                )
                                .password(true),
                            );
                            ui.end_row();
                        });
                    ui.small("These credentials are saved in the local application settings.");
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            self.save_preferences();
                        }
                        if ui.button("Cancel").clicked() {
                            self.preferences_open = false;
                        }
                    });
                });
            self.preferences_open &= open;
        }

        if self.about_open {
            egui::Window::new("About Crestron Load Runner")
                .open(&mut self.about_open)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.heading("Crestron Load Runner");
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.label("Rust + egui device deployment utility");
                });
        }

        self.firmware_editor.show(ctx);

        if let Some(message) = self.notice.clone() {
            let mut open = true;
            egui::Window::new("Notice")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(message);
                    if ui.button("OK").clicked() {
                        self.notice = None;
                    }
                });
            if !open {
                self.notice = None;
            }
        }
    }
}

impl eframe::App for LoadRunnerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

impl LoadRunnerApp {
    fn tick(&mut self, ctx: &egui::Context) {
        self.process_events();
        self.firmware_editor.poll(ctx);
        if ctx.input(|input| input.viewport().close_requested()) && !self.close_approved {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.request_action(PendingAction::Quit, ctx);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        }
        if self.discovering || self.worker_pool.has_pending() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if self.pending_action.is_some() {
            ui.disable();
        }
        self.menu_bar(ui);
        self.action_bar(ui);
        self.status_bar(ui);
        self.devices_panel(ui);
        self.details_panel(ui);
        self.dialogs(&ctx);
        if self.discovering
            || self.worker_pool.has_pending()
            || self.devices.iter().any(|device| {
                matches!(
                    device.connection,
                    ConnectionState::Connecting | ConnectionState::Busy
                )
            })
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

fn file_has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn file_display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn address_book_status_path(path: &Path, dirty: bool) -> String {
    format!("{}{}", path.display(), if dirty { " *" } else { "" })
}

fn assigned_file_row(ui: &mut egui::Ui, kind: &str, slot: usize, path: &Path) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{kind} {slot}")).strong());
        ui.monospace(file_display_name(path));
    });
}

fn assigned_project_row(ui: &mut egui::Ui, kind: &str, path: &Path) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(kind).strong());
        ui.monospace(file_display_name(path));
    });
}

fn slot_one_assignment(
    ui: &mut egui::Ui,
    label: &str,
    assignment: &mut Option<PathBuf>,
    extension: Option<&str>,
) -> bool {
    let mut changed = false;
    let file_name = assignment
        .as_deref()
        .map(file_display_name)
        .unwrap_or_else(|| "Unassigned".into());
    let hover_text = assignment.as_deref().map_or_else(
        || "Click to choose a file".into(),
        |path| {
            format!(
                "{}\n\nClick to choose a different file. Right-click to clear.",
                path.display()
            )
        },
    );
    let slot = egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_min_size(egui::vec2(120.0, 34.0));
            ui.small(RichText::new(label).strong());
            ui.small(RichText::new(&file_name).monospace());
        });
    let response = slot
        .response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(hover_text);
    if response.clicked() {
        let selected = match extension {
            Some(extension) => rfd::FileDialog::new()
                .add_filter(extension.to_ascii_uppercase(), &[extension])
                .pick_file(),
            None => rfd::FileDialog::new().pick_file(),
        };
        if let Some(path) = selected {
            *assignment = Some(path);
            changed = true;
        }
    }
    response.context_menu(|ui| {
        if ui
            .add_enabled(assignment.is_some(), egui::Button::new("Clear assignment"))
            .clicked()
        {
            *assignment = None;
            changed = true;
            ui.close();
        }
    });
    changed
}

fn project_assignment_button(ui: &mut egui::Ui, assignment: &mut Option<PathBuf>) -> bool {
    let label = if assignment.is_some() {
        "Replace touchpanel project…"
    } else {
        "Assign touchpanel project…"
    };
    if ui.button(label).clicked()
        && let Some(path) = rfd::FileDialog::new()
            .add_filter("VTZ", &["vtz"])
            .pick_file()
    {
        *assignment = Some(path);
        return true;
    }
    false
}

fn processor_assignments(ui: &mut egui::Ui, device: &mut Device) -> bool {
    let mut changed = false;
    egui::CollapsingHeader::new("Program slot assignments")
        .default_open(true)
        .show(ui, |ui| {
            for slot in 0..10 {
                changed |=
                    assignment_slot(ui, slot + 1, &mut device.program_slots[slot], Some("lpz"));
            }
        });
    egui::CollapsingHeader::new("Configuration slot assignments")
        .default_open(false)
        .show(ui, |ui| {
            for slot in 0..10 {
                changed |= assignment_slot(ui, slot + 1, &mut device.config_slots[slot], None);
            }
        });
    changed
}

fn touchpanel_assignment(ui: &mut egui::Ui, device: &mut Device) -> bool {
    let mut changed = false;
    egui::CollapsingHeader::new("Touchpanel project assignment")
        .default_open(true)
        .show(ui, |ui| {
            changed |= project_assignment_row(ui, &mut device.touchpanel_project);
        });
    changed
}

fn project_assignment_row(ui: &mut egui::Ui, assignment: &mut Option<PathBuf>) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label("Project");
        ui.monospace(
            assignment
                .as_deref()
                .map(file_display_name)
                .unwrap_or_else(|| "Unassigned".into()),
        );
        if ui.small_button("Choose…").clicked()
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("VTZ", &["vtz"])
                .pick_file()
        {
            *assignment = Some(path);
            changed = true;
        }
        if assignment.is_some() && ui.small_button("Clear").clicked() {
            *assignment = None;
            changed = true;
        }
    });
    changed
}

fn assignment_slot(
    ui: &mut egui::Ui,
    slot: usize,
    assignment: &mut Option<PathBuf>,
    extension: Option<&str>,
) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("Slot {slot}"));
        ui.monospace(
            assignment
                .as_deref()
                .map(file_display_name)
                .unwrap_or_else(|| "Unassigned".into()),
        );
        if ui.small_button("Choose…").clicked() {
            let selected = match extension {
                Some(extension) => rfd::FileDialog::new()
                    .add_filter(extension.to_ascii_uppercase(), &[extension])
                    .pick_file(),
                None => rfd::FileDialog::new().pick_file(),
            };
            if let Some(path) = selected {
                *assignment = Some(path);
                changed = true;
            }
        }
        if assignment.is_some() && ui.small_button("Clear").clicked() {
            *assignment = None;
            changed = true;
        }
    });
    changed
}

fn status_badge(ui: &mut egui::Ui, state: ConnectionState) {
    let color = match state {
        ConnectionState::Connected => Color32::from_rgb(68, 180, 110),
        ConnectionState::Connecting | ConnectionState::Busy => Color32::from_rgb(235, 177, 72),
        ConnectionState::Error => Color32::from_rgb(225, 82, 82),
        ConnectionState::Disconnected => ui.visuals().weak_text_color(),
    };
    ui.label(RichText::new(format!("● {}", state.label())).color(color));
}

fn device_type_badge(ui: &mut egui::Ui, kind: DeviceKind) {
    let color = match kind {
        DeviceKind::Processor => Color32::from_rgb(76, 156, 255),
        DeviceKind::Touchpanel => Color32::from_rgb(180, 108, 255),
        DeviceKind::Unknown => Color32::from_rgb(230, 167, 66),
    };
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(
                RichText::new(kind.label().to_ascii_uppercase())
                    .strong()
                    .color(color),
            );
        });
}

fn detail_section(ui: &mut egui::Ui, title: &str, contents: &str, open: bool) {
    egui::CollapsingHeader::new(title)
        .default_open(open)
        .show(ui, |ui| {
            let mut text = contents.trim().to_owned();
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .interactive(false),
            );
        });
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_extensions_case_insensitively() {
        assert!(file_has_extension(Path::new("program.LPZ"), "lpz"));
        assert!(!file_has_extension(Path::new("program.vtz"), "lpz"));
    }

    #[test]
    fn marks_unsaved_address_book_paths() {
        let path = Path::new("address-book.json");
        assert_eq!(address_book_status_path(path, false), "address-book.json");
        assert_eq!(address_book_status_path(path, true), "address-book.json *");
    }
}
