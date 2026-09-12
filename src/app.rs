use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
};

use eframe::egui::{self, Color32, RichText};

use crate::{
    discovery::{self, DiscoveryEvent},
    model::{
        AddressEntry, ConnectionState, Device, DeviceKind, DeviceSource, Outcome, endpoint_id,
    },
    ssh::{ConnectionSpec, WorkerCommand, WorkerEvent, WorkerPool},
    storage::{Preferences, StartupBook},
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
    startup: StartupBook,
    default_address_book: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingAction {
    New,
    Open(PathBuf),
    Quit,
}

pub struct LoadRunnerApp {
    preferences: Preferences,
    /// Where the preferences are written. The configuration directory is a
    /// process-global set once at startup, so this is what lets a test keep its
    /// preference writes out of the real one.
    preferences_path: Option<PathBuf>,
    address_book: Vec<AddressEntry>,
    devices: Vec<Device>,
    selected_id: Option<String>,
    filter: DeviceFilter,
    view: DeviceView,
    search: String,
    worker_pool: WorkerPool,
    worker_events: Receiver<WorkerEvent>,
    /// Kept so an interactive session can log to the same device log the
    /// queued operations write to.
    worker_sender: std::sync::mpsc::Sender<WorkerEvent>,
    terminals: BTreeMap<String, crate::terminal::Terminal>,
    vc4: HashMap<String, crate::vc4::Panel>,
    discovery_events: Receiver<DiscoveryEvent>,
    discovery_sender: std::sync::mpsc::Sender<DiscoveryEvent>,
    discovering: bool,
    discard_discovery_results: bool,
    pending_host_keys: HashMap<String, String>,
    device_log: crate::device_log::DeviceLog,
    log_view_open: bool,
    log_only_selected: bool,
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
    backdrop: crate::backdrop::Backdrop,
    firmware_editor: crate::firmware::FirmwareEditor,
    script_editor: crate::scripts::ScriptEditor,
    script_run: Option<crate::scripts::RunDialog>,
    notice: Option<String>,
}

/// The application mark, drawn rather than loaded so it is sharp at any size
/// and in any theme. The same description rasters the executable's icon.
fn mark(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let gold = egui::Color32::from_rgb(
        crate::logo::GOLD[0],
        crate::logo::GOLD[1],
        crate::logo::GOLD[2],
    );
    for triangle in crate::logo::TRIANGLES {
        let corners = triangle
            .iter()
            .map(|(x, y)| rect.min + egui::vec2(x * size, y * size))
            .collect();
        ui.painter().add(egui::Shape::convex_polygon(
            corners,
            gold,
            egui::Stroke::NONE,
        ));
    }
}

/// A dialog that blocks the main window. The backdrop is painted separately,
/// once per frame, so that stacked dialogs do not darken it twice.
fn modal(id: &str) -> egui::Modal {
    egui::Modal::new(egui::Id::new(id)).backdrop_color(egui::Color32::TRANSPARENT)
}

impl LoadRunnerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        // Before the editor reads the catalog, since where it reads depends on
        // whether a library from an earlier build could be moved.
        let moved = crate::storage::migrate_firmware_dir();
        let (preferences, load_error) = Preferences::load();
        let (startup_book, startup_error) = crate::storage::startup_address_book(&preferences);
        let mut app = Self::from_parts(
            preferences,
            crate::storage::preferences_path(),
            Vec::new(),
            moved.or(load_error).or(startup_error),
        );
        app.firmware_editor = crate::firmware::FirmwareEditor::load(crate::storage::firmware_dir());
        if let Some(path) = startup_book {
            app.open_address_book(&path);
        }
        app
    }

    fn from_parts(
        preferences: Preferences,
        preferences_path: Option<PathBuf>,
        address_book: Vec<AddressEntry>,
        load_error: Option<String>,
    ) -> Self {
        let status_is_error = load_error.is_some();
        let status_message = load_error.unwrap_or_else(|| "Ready".into());
        let devices = address_book.iter().map(Device::from_address).collect();
        let (worker_sender, worker_events) = mpsc::channel();
        let (discovery_sender, discovery_events) = mpsc::channel();
        let preferences_draft = PreferencesDraft {
            default_username: preferences.default_username.clone(),
            default_password: preferences.default_password.clone(),
            startup: preferences.startup,
            default_address_book: preferences.default_address_book.clone(),
        };
        Self {
            preferences,
            backdrop: crate::backdrop::Backdrop::default(),
            script_editor: crate::scripts::ScriptEditor::load(
                preferences_path
                    .as_ref()
                    .map(|path| path.with_file_name("scripts.json")),
            ),
            script_run: None,
            preferences_path,
            address_book,
            devices,
            selected_id: None,
            filter: DeviceFilter::All,
            view: DeviceView::All,
            search: String::new(),
            worker_pool: WorkerPool::new(worker_sender.clone()),
            worker_events,
            worker_sender,
            terminals: BTreeMap::new(),
            vc4: HashMap::new(),
            discovery_events,
            discovery_sender,
            discovering: false,
            discard_discovery_results: false,
            pending_host_keys: HashMap::new(),
            device_log: crate::device_log::DeviceLog::default(),
            log_view_open: false,
            log_only_selected: true,
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

    fn apply_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::JobFinished { id } => self.worker_pool.job_finished(&id),
            WorkerEvent::Log {
                id,
                direction,
                text,
            } => self.device_log.push(id, direction, &text),
            WorkerEvent::Connecting { id } => {
                if let Some(device) = self.device_mut(&id) {
                    device.connection = ConnectionState::Connecting;
                    device.last_message = "Opening SSH connection".into();
                    device.last_outcome = None;
                }
            }
            WorkerEvent::HostKeyUnknown { id, fingerprint } => {
                if let Some(device) = self.device_mut(&id) {
                    device.connection = ConnectionState::Disconnected;
                    device.last_message = "Verify the SSH host key before reconnecting".into();
                    device.last_outcome = Some(Outcome::Failed);
                }
                self.pending_host_keys.insert(id, fingerprint);
            }
            WorkerEvent::Details { id, details } => {
                if let Some(device) = self.device_mut(&id) {
                    device.connection = ConnectionState::Connected;
                    device.details = Some(details);
                    device.last_message = "Device information refreshed".into();
                    device.last_outcome = Some(Outcome::Succeeded);
                }
            }
            WorkerEvent::Progress { id, sent, total } => {
                if let Some(device) = self.device_mut(&id) {
                    device.connection = ConnectionState::Busy;
                    device.progress = Some((sent, total));
                    device.last_message = format!("Uploading {sent} of {total} bytes");
                    device.last_outcome = None;
                }
            }
            WorkerEvent::Status {
                id,
                message,
                progress,
            } => {
                if let Some(device) = self.device_mut(&id) {
                    device.connection = ConnectionState::Busy;
                    device.progress = progress;
                    device.last_message = message;
                    device.last_outcome = None;
                }
            }
            WorkerEvent::FirmwareUpToDate { id, message } => {
                if let Some(device) = self.device_mut(&id) {
                    device.connection = ConnectionState::Connected;
                    device.progress = None;
                    device.last_message = message;
                    device.last_outcome = Some(Outcome::FirmwareUpToDate);
                }
            }
            WorkerEvent::Complete { id, message } => {
                if let Some(device) = self.device_mut(&id) {
                    device.connection = ConnectionState::Connected;
                    device.progress = None;
                    device.last_message = message;
                    device.last_outcome = Some(Outcome::Succeeded);
                }
            }
            WorkerEvent::Error { id, message } => {
                if let Some(device) = self.device_mut(&id) {
                    device.connection = ConnectionState::Error;
                    device.progress = None;
                    device.last_message = message;
                    device.last_outcome = Some(Outcome::Failed);
                }
            }
        }
    }

    fn process_events(&mut self) {
        self.vc4.retain(|id, _| {
            self.devices
                .iter()
                .any(|d| d.id == *id && crate::vc4::is_vc4(&d.model))
        });
        while let Ok(event) = self.worker_events.try_recv() {
            self.apply_worker_event(event);
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
                self.vc4.remove(&id);
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
            credentials.username = self.preferences.default_username.clone();
        }
        if credentials.password.is_empty() {
            credentials.password = self.preferences.default_password.clone();
        }
        Some(ConnectionSpec {
            id: device.id.clone(),
            host: device.host.clone(),
            port: device.port,
            credentials,
            trusted_fingerprint: device.ssh_host_key_fingerprint.clone(),
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
        let address_book_changed = !self.address_book.is_empty()
            || self
                .devices
                .iter()
                .any(|device| device.source == DeviceSource::AddressBook);
        self.discard_discovery_results = self.discovering;
        self.devices.clear();
        self.vc4.clear();
        self.selected_id = None;
        self.pending_host_keys.clear();
        self.sync_address_book_from_devices();
        self.address_book_dirty |= address_book_changed;
        self.status_message = format!("Cleared {removed} device(s)");
        self.status_is_error = false;
    }

    fn open_preferences(&mut self) {
        self.preferences_draft = PreferencesDraft {
            default_username: self.preferences.default_username.clone(),
            default_password: self.preferences.default_password.clone(),
            startup: self.preferences.startup,
            default_address_book: self.preferences.default_address_book.clone(),
        };
        self.preferences_open = true;
    }

    fn save_preferences(&mut self) {
        let draft = self.preferences_draft.clone();
        if draft.startup == StartupBook::Specific {
            let Some(path) = draft.default_address_book.as_deref() else {
                self.notice = Some("Choose the address book to open at startup".into());
                return;
            };
            if let Err(error) = crate::storage::validate_portable_path(path) {
                self.notice = Some(format!("Cannot open that file at startup: {error}"));
                return;
            }
            if !path.is_file() {
                self.notice = Some(format!("No such address book: {}", path.display()));
                return;
            }
        }
        // Assigned field by field: the recent list lives on the same struct and
        // is not part of the draft.
        self.preferences.default_username = draft.default_username.trim().to_owned();
        self.preferences.default_password = draft.default_password;
        self.preferences.startup = draft.startup;
        self.preferences.default_address_book = draft.default_address_book;
        match self.store_preferences() {
            Ok(()) => {
                self.preferences_open = false;
                self.status_message = "Preferences saved".into();
                self.status_is_error = false;
            }
            Err(error) => {
                self.notice = Some(format!("Could not save preferences: {error}"));
            }
        }
    }

    fn store_preferences(&self) -> std::io::Result<()> {
        let path = self
            .preferences_path
            .as_deref()
            .ok_or_else(|| std::io::Error::other("no configuration directory"))?;
        self.preferences.save_to(path)
    }

    /// Records the file in the recent list. Best effort: a document that was
    /// written or read successfully is not undone by a preferences failure, so
    /// the trouble is appended to the status message instead.
    fn remember_recent(&mut self, path: &Path) {
        if !crate::storage::remember_recent(&mut self.preferences.recent_address_books, path) {
            return;
        }
        if let Err(error) = self.store_preferences() {
            self.status_message = format!(
                "{}, but could not update preferences: {error}",
                self.status_message
            );
            self.status_is_error = true;
        }
    }

    fn forget_recent(&mut self, path: &Path) {
        if crate::storage::forget_recent(&mut self.preferences.recent_address_books, path) {
            let _ = self.store_preferences();
        }
    }

    fn refresh_device(&mut self, id: &str) {
        if let Some(device) = self
            .devices
            .iter()
            .find(|d| d.id == id && crate::vc4::is_vc4(&d.model))
        {
            self.vc4
                .entry(id.to_owned())
                .or_insert_with(|| {
                    crate::vc4::Panel::new(device.https_certificate.clone())
                        .with_token(&device.vc4_api_token)
                })
                .refresh(&device.host);
            return;
        }
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

    fn vc4_address_book_target(&mut self, id: &str) -> Option<String> {
        let device = self.devices.iter().find(|d| d.id == id)?;
        let mut target = id.to_owned();
        if device.source == DeviceSource::Discovered {
            target = endpoint_id(&device.host, device.port);
            self.add_discovered_to_address_book(id);
            if !self
                .devices
                .iter()
                .any(|d| d.id == target && d.source == DeviceSource::AddressBook)
                || self
                    .devices
                    .iter()
                    .any(|d| d.id == id && d.source == DeviceSource::Discovered)
            {
                self.notice = Some(
                    "Could not add the server to the address book; VC-4 connection settings were not saved"
                        .into(),
                );
                return None;
            }
            if target != id
                && let Some(panel) = self.vc4.remove(id)
            {
                self.vc4.insert(target.clone(), panel);
            }
        }
        Some(target)
    }

    fn sync_vc4_token(&mut self, id: &str) {
        let Some(token) = self.vc4.get(id).map(crate::vc4::Panel::token) else {
            return;
        };
        let Some(device) = self.device_mut(id) else {
            return;
        };
        if device.vc4_api_token != token {
            device.vc4_api_token = token;
            if device.source == DeviceSource::AddressBook {
                self.mark_address_book_dirty();
            }
        }
    }

    fn save_vc4_token(&mut self, id: &str) {
        let Some(target) = self.vc4_address_book_target(id) else {
            return;
        };
        self.sync_vc4_token(&target);
        self.mark_address_book_dirty();
        if !self.save_address_book() {
            self.notice = Some(format!(
                "API token change has not been saved. {}",
                self.status_message
            ));
        }
    }

    fn apply_https_trust(&mut self, id: &str, action: crate::vc4::TrustAction) {
        let Some(target) = self.vc4_address_book_target(id) else {
            return;
        };
        self.sync_vc4_token(&target);
        let accepted = matches!(action, crate::vc4::TrustAction::Accept(_));
        let trust = match action {
            crate::vc4::TrustAction::Accept(trust) => Some(trust),
            crate::vc4::TrustAction::Forget => None,
        };
        self.device_mut(&target)
            .expect("server still present")
            .https_certificate = trust.clone();
        if let Some(panel) = self.vc4.get_mut(&target) {
            panel.set_trust(trust);
        }
        self.mark_address_book_dirty();
        // Uses the normal write/read-back path. Untitled books open Save As;
        // cancellation or an I/O failure remains visibly unsaved.
        if !self.save_address_book() {
            self.notice = Some(format!(
                "HTTPS certificate change is only in memory and has not been saved. {}",
                self.status_message
            ));
            return;
        }
        if accepted {
            self.refresh_device(&target);
        }
    }

    /// Opens an interactive session on one device, or brings back the window of
    /// a session that has ended so it can be started again.
    fn open_terminal(&mut self, id: &str) {
        if let Some(terminal) = self.terminals.get_mut(id).filter(|terminal| terminal.open) {
            terminal.ensure_connected();
            return;
        }
        let Some(connection) = self.connection_spec(id) else {
            return;
        };
        let name = self.device_name(id);
        self.terminals.insert(
            id.to_owned(),
            crate::terminal::Terminal::open(&name, connection, self.worker_sender.clone()),
        );
    }

    fn open_script_run(&mut self, target: Option<&str>) {
        let targets = self
            .devices
            .iter()
            .filter(|device| {
                target.map_or(
                    device.selected && device.source == DeviceSource::AddressBook,
                    |id| device.id == id,
                )
            })
            .map(crate::scripts::ScriptTarget::from)
            .collect::<Vec<_>>();
        if targets.is_empty() {
            self.notice =
                Some("Select at least one address-book target device to run a script".into());
            return;
        }
        self.script_run = Some(crate::scripts::RunDialog::new(
            self.script_editor.scripts.clone(),
            targets,
        ));
    }

    fn queue_script(&mut self, request: crate::scripts::RunRequest) {
        // Validate the complete target set before queueing anything. The dialog
        // snapshots targets, so a later selection change cannot broaden a run.
        let jobs = request
            .jobs
            .into_iter()
            .map(|(id, commands)| {
                self.connection_spec(&id)
                    .map(|connection| (id, connection, commands))
            })
            .collect::<Option<Vec<_>>>();
        let Some(jobs) = jobs else {
            self.notice = Some(
                "A script target was removed. Open Run Script again to review the targets".into(),
            );
            return;
        };
        for (id, connection, commands) in jobs {
            if let Err(error) = self.worker_pool.send(
                &id,
                WorkerCommand::RunScript {
                    connection,
                    name: request.name.clone(),
                    commands,
                },
            ) {
                self.notice = Some(error);
            }
        }
    }

    /// Drops a stored fingerprint so the next connection asks again. A device
    /// can offer several host keys, and which one is negotiated depends on the
    /// algorithms the client supports, so a stored key can stop matching
    /// without the device having changed.
    fn forget_host_key(&mut self, id: &str) {
        let Some(index) = self.devices.iter().position(|device| device.id == id) else {
            return;
        };
        let had_key = self.devices[index]
            .ssh_host_key_fingerprint
            .take()
            .is_some();
        let is_address_book = self.devices[index].source == DeviceSource::AddressBook;
        self.pending_host_keys.remove(id);
        if !had_key {
            self.status_message = "This device has no trusted SSH host key".into();
            self.status_is_error = false;
            return;
        }
        if is_address_book {
            self.mark_address_book_dirty();
            self.status_message =
                "SSH host key forgotten; the next connection will ask you to trust it again. Save to persist"
                    .into();
        } else {
            self.status_message =
                "SSH host key forgotten; the next connection will ask you to trust it again".into();
        }
        self.status_is_error = false;
    }

    fn trust_host_key(&mut self, id: &str, fingerprint: String) {
        let Some(index) = self.devices.iter().position(|device| device.id == id) else {
            return;
        };
        let is_address_book = self.devices[index].source == DeviceSource::AddressBook;
        self.devices[index].ssh_host_key_fingerprint = Some(fingerprint);
        self.pending_host_keys.remove(id);

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
        self.load_assigned_programs_for(None);
    }

    fn load_assigned_programs_for(&mut self, target: Option<&str>) {
        let jobs: Vec<(String, PathBuf, u8)> = self
            .devices
            .iter()
            .filter(|device| !crate::vc4::is_vc4(&device.model))
            .filter(|device| {
                target.map_or(device.selected, |id| device.id == id)
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
            let signature = signature_beside(&path);
            if signature.is_none() {
                self.device_log.push(
                    id.clone(),
                    crate::device_log::Direction::Note,
                    &format!(
                        "No signature file beside {}; uploading the program only",
                        file_display_name(&path)
                    ),
                );
            }
            if let Err(error) = self.worker_pool.send(
                &id,
                WorkerCommand::UploadProgram {
                    connection,
                    local_path: path,
                    signature,
                    slot,
                },
            ) {
                self.notice = Some(error);
            }
        }
    }

    fn load_assigned_configs(&mut self) {
        self.load_assigned_configs_for(None);
    }

    fn load_assigned_configs_for(&mut self, target: Option<&str>) {
        let jobs: Vec<(String, PathBuf)> = self
            .devices
            .iter()
            .filter(|device| !crate::vc4::is_vc4(&device.model))
            .filter(|device| {
                target.map_or(device.selected, |id| device.id == id)
                    && device.source == DeviceSource::AddressBook
                    && device.kind == DeviceKind::Processor
            })
            .flat_map(|device| {
                device
                    .config_slots
                    .iter()
                    .filter_map(|path| path.clone().map(|path| (device.id.clone(), path)))
            })
            .collect();
        if jobs.is_empty() {
            self.notice = Some(
                "Select an address-book processor that has at least one assigned configuration file"
                    .into(),
            );
            return;
        }
        // Configuration slots accept any file type, so a missing file is the
        // only thing that can be checked before connecting.
        if let Some((_, path)) = jobs.iter().find(|(_, path)| !path.is_file()) {
            self.notice = Some(format!(
                "Assigned configuration file is missing: {}",
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
                WorkerCommand::UploadConfig {
                    connection,
                    local_path: path,
                },
            ) {
                self.notice = Some(error);
            }
        }
    }

    fn load_assigned_touchpanels(&mut self) {
        self.load_assigned_touchpanels_for(None);
    }

    fn load_assigned_touchpanels_for(&mut self, target: Option<&str>) {
        let jobs: Vec<(String, PathBuf)> = self
            .devices
            .iter()
            .filter(|device| !crate::vc4::is_vc4(&device.model))
            .filter(|device| {
                target.map_or(device.selected, |id| device.id == id)
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
            .filter(|device| !crate::vc4::is_vc4(&device.model))
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

    fn sync_address_book_from_devices(&mut self) {
        self.address_book = self
            .devices
            .iter()
            .filter(|device| device.source == DeviceSource::AddressBook)
            .map(Device::to_address_entry)
            .collect();
    }

    fn mark_address_book_dirty(&mut self) {
        self.sync_address_book_from_devices();
        self.address_book_dirty = true;
        self.status_message = "Address book modified".into();
        self.status_is_error = false;
    }

    fn save_address_book(&mut self) -> bool {
        match self.current_address_book.clone() {
            Some(path) => self.write_address_book_to(&path),
            None => self.save_address_book_as(),
        }
    }

    fn save_address_book_as(&mut self) -> bool {
        let name = self
            .current_address_book
            .as_deref()
            .map_or_else(|| "address-book.json".into(), file_display_name);
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name(name)
            .save_file()
        else {
            // The unsaved-changes prompt only shows the status line when it is
            // an error, so a cancelled dialog has to say something there.
            self.status_message = "Choose a file to save the address book".into();
            self.status_is_error = true;
            return false;
        };
        self.write_address_book_to(&path)
    }

    /// The one place the address book is written. The file is read back and
    /// compared because it is now the only copy of the data.
    fn write_address_book_to(&mut self, path: &Path) -> bool {
        self.sync_address_book_from_devices();
        let result = crate::storage::save_address_book(path, &self.address_book).and_then(|()| {
            let saved = crate::storage::load_address_book(path)?;
            if saved == self.address_book {
                Ok(())
            } else {
                Err(std::io::Error::other(
                    "saved address book did not match the in-memory data",
                ))
            }
        });
        match result {
            Ok(()) => {
                self.current_address_book = Some(path.to_owned());
                self.address_book_dirty = false;
                self.status_message = format!("Saved {}", path.display());
                self.status_is_error = false;
                self.remember_recent(path);
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

    /// Discovered devices describe the network rather than the document, so
    /// they survive a new address book just as they survive opening one.
    fn new_address_book(&mut self) {
        if let Err(error) = self.worker_pool.retire_all() {
            self.status_message = error;
            self.status_is_error = true;
            return;
        }
        self.devices
            .retain(|device| device.source == DeviceSource::Discovered);
        self.vc4.clear();
        self.address_book.clear();
        self.pending_host_keys.clear();
        self.selected_id = None;
        self.current_address_book = None;
        self.address_book_dirty = false;
        self.status_message = "New address book".into();
        self.status_is_error = false;
    }

    /// What the pending action would throw away. Quitting abandons the script
    /// drafts too, so it has to ask about them; switching address books leaves
    /// the separate script library alone.
    fn unsaved(&self, action: &PendingAction) -> Vec<&'static str> {
        let mut unsaved = Vec::new();
        if self.address_book_dirty {
            unsaved.push("address book");
        }
        if *action == PendingAction::Quit && self.script_editor.is_dirty() {
            unsaved.push("script library");
        }
        unsaved
    }

    fn request_action(&mut self, action: PendingAction, ctx: &egui::Context) {
        if self.firmware_editor.is_busy() {
            self.status_message = "Wait for the firmware file import to finish".into();
            self.status_is_error = true;
            return;
        }
        if self.worker_pool.has_pending() {
            self.status_message = "Wait for queued device operations to finish before switching address books or exiting".into();
            self.status_is_error = true;
            return;
        }
        if self.unsaved(&action).is_empty() {
            self.execute_action(action, ctx);
        } else {
            self.pending_action = Some(action);
        }
    }

    fn execute_action(&mut self, action: PendingAction, ctx: &egui::Context) {
        if let Err(error) = self.worker_pool.retire_all() {
            self.status_message = error;
            self.status_is_error = true;
            return;
        }
        match action {
            PendingAction::New => self.new_address_book(),
            PendingAction::Open(path) => self.open_address_book(&path),
            PendingAction::Quit => {
                self.close_approved = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// `save` writes everything the confirmation listed; otherwise the listed
    /// changes are abandoned. Either way a failure leaves the confirmation up.
    fn confirm_pending_action(&mut self, save: bool, ctx: &egui::Context) {
        if self.firmware_editor.is_busy() || self.worker_pool.has_pending() {
            return;
        }
        if save {
            if self.address_book_dirty && !self.save_address_book() {
                return;
            }
            if self
                .pending_action
                .as_ref()
                .is_some_and(|action| self.unsaved(action).contains(&"script library"))
                && let Err(error) = self.script_editor.save()
            {
                self.status_message = error;
                self.status_is_error = true;
                return;
            }
        }
        if let Some(action) = self.pending_action.take() {
            self.execute_action(action, ctx);
        }
    }

    fn open_address_book_dialog(&mut self, ctx: &egui::Context) {
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
                self.address_book = entries;
                self.vc4.clear();
                self.devices = devices;
                self.selected_id = None;
                self.pending_host_keys.clear();
                self.current_address_book = Some(path.to_owned());
                self.address_book_dirty = false;
                self.status_message = format!("Loaded {}", path.display());
                self.status_is_error = false;
                self.remember_recent(path);
            }
            Err(error) => {
                let missing = error.kind() == std::io::ErrorKind::NotFound;
                self.status_message = format!("Could not load address book: {error}");
                self.status_is_error = true;
                // A file that is merely malformed stays listed so it can be
                // fixed; one that is gone will never open again.
                if missing {
                    self.forget_recent(path);
                }
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
            https_certificate: None,
            vc4_api_token: Default::default(),
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
        self.address_book.push(entry);
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
            let fingerprint = self.devices[index].ssh_host_key_fingerprint.clone();
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
        self.vc4.remove(&id);
        self.address_book
            .retain(|entry| entry.host != host || entry.port != port);
        self.pending_host_keys.remove(&id);
        self.selected_id = None;
        self.mark_address_book_dirty();
    }

    fn menu_bar(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("menu").show(root, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New address book").clicked() {
                        self.request_action(PendingAction::New, ui.ctx());
                        ui.close();
                    }
                    if ui.button("Open address book…").clicked() {
                        self.open_address_book_dialog(ui.ctx());
                        ui.close();
                    }
                    ui.menu_button("Open Recent", |ui| self.recent_menu(ui));
                    if ui.button("Save address book").clicked() {
                        self.save_address_book();
                        ui.close();
                    }
                    if ui.button("Save address book as…").clicked() {
                        self.save_address_book_as();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Add device…").clicked() {
                        self.add_device_open = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Preferences…").clicked() {
                        self.open_preferences();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        self.request_action(PendingAction::Quit, ui.ctx());
                    }
                });
                ui.menu_button("Devices", |ui| {
                    if ui.button("Firmware Editor…").clicked() {
                        self.firmware_editor.open = true;
                        ui.close();
                    }
                    if ui.button("Script Editor…").clicked() {
                        self.script_editor.open = true;
                        ui.close();
                    }
                    ui.separator();
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

    fn recent_menu(&mut self, ui: &mut egui::Ui) {
        // Cloned because acting on an entry borrows self mutably.
        let recent = self.preferences.recent_address_books.clone();
        if recent.is_empty() {
            ui.add_enabled(false, egui::Button::new("No recent address books"));
            return;
        }
        for path in &recent {
            // Five absolute paths would be unreadable, so the full one is hover
            // text and disambiguates two files sharing a name.
            if ui
                .button(file_display_name(path))
                .on_hover_text(path.display().to_string())
                .clicked()
            {
                self.request_action(PendingAction::Open(path.clone()), ui.ctx());
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Clear recent list").clicked() {
            self.preferences.recent_address_books.clear();
            if let Err(error) = self.store_preferences() {
                self.notice = Some(format!("Could not save preferences: {error}"));
            }
            ui.close();
        }
    }

    fn action_bar(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("actions").show(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Load Assigned Program").clicked() {
                    self.load_assigned_programs();
                }
                if ui.button("Load Assigned Config").clicked() {
                    self.load_assigned_configs();
                }
                if ui.button("Load Assigned Touchpanel").clicked() {
                    self.load_assigned_touchpanels();
                }
                if ui.button("Load Firmware").clicked() {
                    self.load_assigned_firmware();
                }
                if ui.button("Run Script").clicked() {
                    self.open_script_run(None);
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
                ui.monospace(address_book_status_path(
                    self.current_address_book.as_deref(),
                    self.address_book_dirty,
                ));
                ui.separator();
                ui.label(format!("{} device(s)", self.address_book.len()));
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
                        self.device_actions(ui);
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

    fn device_actions(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            let gaps = 2.0 * ui.spacing().item_spacing.x;
            let available = ui.available_width();
            // Center the group; shrink and wrap labels in a narrow sidebar.
            let scale = ((available - gaps) / 350.0).clamp(0.0, 1.0);
            ui.add_space(((available - (350.0 * scale + gaps)) / 2.0).max(0.0));
            let label = if self.discovering {
                "Discovering…"
            } else {
                "Discover Devices"
            };
            if ui
                .add_enabled_ui(!self.discovering, |ui| {
                    ui.add_sized([140.0 * scale, 32.0], egui::Button::new(label).wrap())
                })
                .inner
                .clicked()
            {
                self.start_discovery();
            }
            if ui
                .add_sized(
                    [100.0 * scale, 32.0],
                    egui::Button::new("Add Device").wrap(),
                )
                .clicked()
            {
                self.add_device_open = true;
            }
            if ui
                .add_sized(
                    [110.0 * scale, 32.0],
                    egui::Button::new("Clear Devices").wrap(),
                )
                .on_hover_text("Immediately clear all devices")
                .clicked()
            {
                self.clear_devices();
            }
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
                    // Card text participates in card clicks; selectable output
                    // remains available in Device Details.
                    ui.style_mut().interaction.selectable_labels = false;
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
                                if in_address_book && !crate::vc4::is_vc4(&device.model) {
                                    status_badge(ui, device.connection);
                                }
                                if in_address_book && !crate::vc4::is_vc4(&device.model) {
                                    match device.kind {
                                        DeviceKind::Processor => {
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
                                        DeviceKind::Touchpanel => {
                                            assignments_changed |= slot_one_assignment(
                                                ui,
                                                "PROJECT",
                                                &mut device.touchpanel_project,
                                                Some("vtz"),
                                            );
                                        }
                                        DeviceKind::Unknown => {}
                                    }
                                }
                            });
                        });
                    });
                    if in_address_book {
                        // Slot 1 of each kind is shown in the card's slot column;
                        // the rest are listed here.
                        if device.kind == DeviceKind::Processor
                            && !crate::vc4::is_vc4(&device.model)
                        {
                            for (slot, path) in device.program_slots.iter().enumerate().skip(1) {
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
                        if let Some((sent, total)) = device.progress {
                            let progress = if total == 0 {
                                0.0
                            } else {
                                sent as f32 / total as f32
                            };
                            ui.add(egui::ProgressBar::new(progress).show_percentage());
                        }
                        if let Some(outcome) = device.last_outcome {
                            outcome_indicator(ui, outcome, &device.last_message);
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
        let has_host_key = self.devices[index].ssh_host_key_fingerprint.is_some();
        let mut add_to_address_book = false;
        let mut forget_host_key = false;
        response.context_menu(|ui| {
            if ui.button("Connect SSH…").clicked() {
                self.open_terminal(id);
                ui.close();
            }
            if ui.button("Run Script…").clicked() {
                self.open_script_run(Some(id));
                ui.close();
            }
            ui.separator();
            if !is_discovered && !crate::vc4::is_vc4(&self.devices[index].model) {
                let device = &self.devices[index];
                let kind = device.kind;
                let has_program = device.program_slots.iter().any(Option::is_some);
                let has_config = device.config_slots.iter().any(Option::is_some);
                let has_project = device.touchpanel_project.is_some();
                match kind {
                    DeviceKind::Processor => {
                        if ui
                            .add_enabled(has_program, egui::Button::new("Load Assigned Program"))
                            .clicked()
                        {
                            self.load_assigned_programs_for(Some(id));
                            ui.close();
                        }
                        if ui
                            .add_enabled(has_config, egui::Button::new("Load Assigned Config"))
                            .clicked()
                        {
                            self.load_assigned_configs_for(Some(id));
                            ui.close();
                        }
                        ui.separator();
                    }
                    DeviceKind::Touchpanel => {
                        if ui
                            .add_enabled(has_project, egui::Button::new("Load Assigned Touchpanel"))
                            .clicked()
                        {
                            self.load_assigned_touchpanels_for(Some(id));
                            ui.close();
                        }
                        ui.separator();
                    }
                    DeviceKind::Unknown => {}
                }
            }
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
            ui.separator();
            if ui
                .add_enabled(has_host_key, egui::Button::new("Forget SSH host key"))
                .on_hover_text("Ask again the next time this device is contacted")
                .clicked()
            {
                forget_host_key = true;
                ui.close();
            }
        });
        if response.clicked() || response.secondary_clicked() {
            self.selected_id = Some(id.to_owned());
        }
        if response.double_clicked() {
            self.refresh_device(id);
        }
        if assignments_changed {
            self.mark_address_book_dirty();
        }
        if add_to_address_book {
            self.add_discovered_to_address_book(id);
        }
        if forget_host_key {
            self.forget_host_key(id);
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
                let mut terminal = false;
                let mut remove = false;
                let mut open_log = false;
                let mut address_changed = false;
                let mut model_changed = false;
                {
                    let device = &mut self.devices[index];
                    ui.label(RichText::new(device.display_name()).strong().size(20.0));
                    if device.source == DeviceSource::AddressBook {
                        ui.horizontal(|ui| {
                            ui.label("Device model");
                            model_changed = ui.add(egui::TextEdit::singleline(&mut device.model).hint_text("e.g. VC-4")).changed();
                        });
                        if model_changed {
                            address_changed = true;
                            let kind = crate::model::classify_model(&device.model);
                            if kind != DeviceKind::Unknown { device.kind = kind; }
                        }
                    } else if !device.model.is_empty() {
                        ui.label(RichText::new(&device.model).strong().size(18.0));
                    }
                    ui.label(format!("{}:{}", device.host, device.port));
                    ui.horizontal(|ui| {
                        if crate::vc4::is_vc4(&device.model) {
                            ui.label("HTTPS API");
                        } else {
                            status_badge(ui, device.connection);
                        }
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
                        if ui
                            .button("Connect SSH…")
                            .on_hover_text("An interactive console in its own window")
                            .clicked()
                        {
                            terminal = true;
                        }
                        if ui
                            .button("Device log…")
                            .on_hover_text("Every line sent to and received from devices")
                            .clicked()
                        {
                            open_log = true;
                        }
                        if device.source == DeviceSource::AddressBook
                            && ui.button("Remove").clicked()
                        {
                            remove = true;
                        }
                    });
                    if device.source == DeviceSource::AddressBook && !crate::vc4::is_vc4(&device.model) {
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
                if model_changed { self.vc4.remove(&id); }

                if open_log {
                    self.log_view_open = true;
                }
                if refresh {
                    self.refresh_device(&id);
                }
                if terminal {
                    self.open_terminal(&id);
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
                if crate::vc4::is_vc4(&self.devices[index].model) {
                    let trust = self.devices[index].https_certificate.clone();
                    let token = self.devices[index].vc4_api_token.clone();
                    let panel = self.vc4.entry(id.clone()).or_insert_with(|| crate::vc4::Panel::new(trust).with_token(&token));
                    panel.show(ui, &id);
                    let save_token = panel.take_token_save();
                    let trust_action = panel.take_trust_action();
                    self.sync_vc4_token(&id);
                    if let Some(action) = trust_action { self.apply_https_trust(&id, action); }
                    else if save_token { self.save_vc4_token(&id); }
                    return;
                }
                let details = self.devices[index].details.clone();
                if let Some(details) = details {
                    crate::resources::show(ui, &details.disk_free, &details.ram_free);
                    detail_section(ui, "Identity", &details.identity, true);
                    detail_section(ui, "Network", &details.network, true);
                    if self.devices[index].kind == DeviceKind::Processor {
                        program_info_section(ui, &id, &details.programs);
                    }
                    crate::ip_table::show(ui, &id, &details.ip_table);
                    if let Some(cresnet) = details.cresnet
                        && self.devices[index].kind == DeviceKind::Processor
                    {
                        crate::cresnet::show(ui, &id, &cresnet);
                    }
                } else {
                    ui.label("Connect to retrieve network, program, IP table, and Cresnet information.");
                }
            });
        });
    }

    fn log_window(&mut self, ctx: &egui::Context) {
        if !self.log_view_open {
            return;
        }
        let selected = self.selected_id.clone();
        let mut open = true;
        egui::Window::new("Device log")
            .open(&mut open)
            .default_size([820.0, 460.0])
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.add_enabled(
                        selected.is_some(),
                        egui::Checkbox::new(
                            &mut self.log_only_selected,
                            "Only the selected device",
                        ),
                    );
                    if ui.button("Copy all").clicked() {
                        let transcript = self.device_log.to_transcript(|id| self.device_name(id));
                        ui.ctx().copy_text(transcript);
                    }
                    if ui.button("Clear").clicked() {
                        self.device_log.clear();
                    }
                });
                let filter = self
                    .log_only_selected
                    .then_some(selected.as_deref())
                    .flatten();
                ui.small(match self.device_log.dropped() {
                    0 => format!("{} entries", self.device_log.len()),
                    dropped => format!(
                        "{} entries · {dropped} older entries dropped (limit {})",
                        self.device_log.len(),
                        crate::device_log::DeviceLog::CAPACITY
                    ),
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("device_log_scroll")
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut shown = 0;
                        for entry in self.device_log.entries() {
                            if filter.is_some_and(|id| id != entry.device) {
                                continue;
                            }
                            shown += 1;
                            let color = match entry.direction {
                                crate::device_log::Direction::Sent => {
                                    Color32::from_rgb(120, 170, 255)
                                }
                                crate::device_log::Direction::Received => {
                                    ui.visuals().strong_text_color()
                                }
                                crate::device_log::Direction::Note => {
                                    ui.visuals().weak_text_color()
                                }
                            };
                            ui.horizontal_top(|ui| {
                                ui.monospace(
                                    RichText::new(format!(
                                        "{}  {:<18} {}",
                                        entry.clock(),
                                        self.device_name(&entry.device),
                                        entry.direction.arrow()
                                    ))
                                    .weak(),
                                );
                                ui.monospace(RichText::new(&entry.text).color(color));
                            });
                        }
                        if shown == 0 {
                            ui.label(if self.device_log.is_empty() {
                                "Nothing has been sent to a device yet."
                            } else {
                                "No entries for the selected device."
                            });
                        }
                    });
            });
        self.log_view_open = open;
    }

    /// Log entries outlive the devices they name, so an unknown endpoint falls
    /// back to the id it was recorded against.
    fn device_name(&self, id: &str) -> String {
        self.devices
            .iter()
            .find(|device| device.id == id)
            .map_or_else(|| id.to_owned(), |device| device.display_name().to_owned())
    }

    /// Whether a dialog is blocking the main window, and so whether the
    /// frosted backdrop belongs behind it.
    fn modal_open(&self) -> bool {
        self.pending_action.is_some()
            || self.add_device_open
            || self.preferences_open
            || self.about_open
            || self.script_run.is_some()
            || self.notice.is_some()
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        // The editors are separate operating-system windows, so they keep
        // drawing even while a modal owns the main window.
        self.firmware_editor.show(ctx);
        self.script_editor.show(ctx);
        for terminal in self.terminals.values_mut() {
            terminal.show(ctx);
        }
        // A closed window keeps nothing: its session has already ended.
        self.terminals.retain(|_, terminal| terminal.open);
        if self.modal_open() {
            self.backdrop.paint(ctx);
        }
        if let Some(action) = self.pending_action.clone() {
            let quitting = action == PendingAction::Quit;
            let unsaved = self.unsaved(&action).join(" and the ");
            modal("unsaved_address_book").show(ctx, |ui| {
                ui.heading("Unsaved changes");
                ui.label(format!("There are unsaved changes in the {unsaved}."));
                if self.status_is_error {
                    ui.colored_label(ui.visuals().error_fg_color, &self.status_message);
                }
                ui.horizontal(|ui| {
                    if ui
                        .button(if quitting { "Save and quit" } else { "Save" })
                        .clicked()
                    {
                        self.confirm_pending_action(true, ctx);
                    }
                    if ui
                        .button(if quitting {
                            "Discard all changes and quit"
                        } else {
                            "Discard"
                        })
                        .clicked()
                    {
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
            modal("add_device").show(ctx, |ui| {
                ui.heading("Add address-book device");
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
                        ui.add(egui::DragValue::new(&mut self.address_draft.port).range(1..=65535));
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
        }

        if self.preferences_open {
            modal("preferences").show(ctx, |ui| {
                ui.heading("Preferences");
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
                ui.add_space(12.0);
                ui.separator();
                ui.label("At startup");
                ui.radio_value(
                    &mut self.preferences_draft.startup,
                    StartupBook::Empty,
                    "Start with an empty address book",
                );
                ui.radio_value(
                    &mut self.preferences_draft.startup,
                    StartupBook::MostRecent,
                    "Reopen the most recent address book",
                );
                ui.radio_value(
                    &mut self.preferences_draft.startup,
                    StartupBook::Specific,
                    "Always open a specific address book",
                );
                let specific = self.preferences_draft.startup == StartupBook::Specific;
                let current = self.current_address_book.clone();
                ui.add_enabled_ui(specific, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let chosen = &mut self.preferences_draft.default_address_book;
                        let label = chosen
                            .as_deref()
                            .map_or_else(|| "None".to_owned(), file_display_name);
                        let path = ui.monospace(label);
                        if let Some(chosen) = chosen.as_deref() {
                            path.on_hover_text(chosen.display().to_string());
                        }
                        if ui.small_button("Choose…").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("JSON", &["json"])
                                .pick_file()
                        {
                            *chosen = Some(path);
                        }
                        if ui
                            .add_enabled(
                                current.is_some(),
                                egui::Button::new("Use current").small(),
                            )
                            .clicked()
                        {
                            *chosen = current;
                        }
                        if chosen.is_some() && ui.small_button("Clear").clicked() {
                            *chosen = None;
                        }
                    });
                });
                ui.small("Opened at startup while the file still exists.");
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
        }

        if self.about_open {
            modal("about").show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(4.0);
                    mark(ui, 96.0);
                    ui.add_space(8.0);
                    ui.heading("Crestron Load Runner");
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.label("Rust + egui device deployment utility");
                    ui.add_space(4.0);
                    ui.separator();
                    if ui.button("Close").clicked() {
                        self.about_open = false;
                    }
                });
            });
        }

        if let Some(mut dialog) = self.script_run.take() {
            if let Some(request) = dialog.show(ctx) {
                self.queue_script(request);
            }
            if dialog.open {
                self.script_run = Some(dialog);
            }
        }

        if let Some(message) = self.notice.clone() {
            modal("notice").show(ctx, |ui| {
                ui.heading("Notice");
                ui.label(message);
                ui.separator();
                if ui.button("OK").clicked() {
                    self.notice = None;
                }
            });
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
        self.log_window(&ctx);
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

/// A program is loaded with its signature: the `.sig` file sitting beside it
/// under the same name. Absent is not an error — an unsigned program still
/// loads — so a missing signature is recorded in the device log instead.
fn signature_beside(program: &Path) -> Option<PathBuf> {
    let signature = program.with_extension("sig");
    signature.is_file().then_some(signature)
}

fn file_display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn address_book_status_path(path: Option<&Path>, dirty: bool) -> String {
    let name = path.map_or_else(|| "Untitled".to_owned(), |path| path.display().to_string());
    format!("{name}{}", if dirty { " *" } else { "" })
}

fn assigned_file_row(ui: &mut egui::Ui, kind: &str, slot: usize, path: &Path) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{kind} {slot}")).strong());
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

fn processor_assignments(ui: &mut egui::Ui, device: &mut Device) -> bool {
    let mut changed = false;
    let programs = egui::CollapsingHeader::new("Program slot assignments")
        .id_salt(("program_assignments", &device.id))
        .default_open(false)
        .show(ui, |ui| {
            for slot in 0..10 {
                changed |=
                    assignment_slot(ui, slot + 1, &mut device.program_slots[slot], Some("lpz"));
            }
        });
    if programs.body_response.is_none() {
        slot_one_summary(ui, device.program_slots[0].as_deref());
    }
    let configs = egui::CollapsingHeader::new("Configuration slot assignments")
        .id_salt(("config_assignments", &device.id))
        .default_open(false)
        .show(ui, |ui| {
            for slot in 0..10 {
                changed |= assignment_slot(ui, slot + 1, &mut device.config_slots[slot], None);
            }
        });
    if configs.body_response.is_none() {
        slot_one_summary(ui, device.config_slots[0].as_deref());
    }
    changed
}

fn slot_one_summary(ui: &mut egui::Ui, assignment: Option<&Path>) {
    let name = assignment
        .map(file_display_name)
        .unwrap_or_else(|| "Unassigned".into());
    let response = ui.add(egui::Label::new(format!("Slot 1: {name}")).selectable(true));
    if let Some(path) = assignment {
        response.on_hover_text(path.display().to_string());
    }
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

/// The result of the last operation, standing in for its output text. The
/// text itself is in the device log; the message is kept as hover text so the
/// detail is one gesture away.
fn outcome_indicator(ui: &mut egui::Ui, outcome: Outcome, message: &str) {
    let (glyph, label, color) = match outcome {
        Outcome::Succeeded => ("\u{2714}", "Succeeded", Color32::from_rgb(68, 180, 110)),
        Outcome::FirmwareUpToDate => (
            "\u{2714}",
            "Firmware already up to date.",
            Color32::from_rgb(68, 180, 110),
        ),
        Outcome::Failed => ("\u{2716}", "Failed", Color32::from_rgb(226, 96, 96)),
    };
    let response = ui.label(
        RichText::new(format!("{glyph} {label}"))
            .strong()
            .color(color),
    );
    if !message.is_empty() {
        response.on_hover_text(message);
    }
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

/// What is shown out of a `progcomments` answer. It names about fifteen
/// fields; the rest stay behind the raw response.
const PROGRAM_INFO_FIELDS: [&str; 4] = ["Source File", "Program File", "Compiled On", "Programmer"];

fn parse_program_info(contents: &str) -> [Option<&str>; 4] {
    let mut values = [None; 4];
    for line in contents.lines() {
        // Split only once: timestamps and archive paths can contain colons.
        if let Some((key, value)) = line.split_once(':')
            && let Some(index) = PROGRAM_INFO_FIELDS
                .iter()
                .position(|field| *field == key.trim())
        {
            values[index] = Some(value.trim());
        }
    }
    values
}

fn program_info_section(ui: &mut egui::Ui, device_id: &str, contents: &str) {
    ui.push_id(("program_info", device_id), |ui| {
        egui::CollapsingHeader::new("Running programs")
            .default_open(true)
            .show(ui, |ui| {
                let values = parse_program_info(contents);
                if values.iter().any(Option::is_some) {
                    egui::Frame::new()
                        .inner_margin(4)
                        .fill(ui.visuals().text_edit_bg_color())
                        .stroke(egui::Stroke::new(1.0, egui::Color32::BLACK))
                        .show(ui, |ui| {
                            egui::Grid::new("summary").num_columns(2).show(ui, |ui| {
                                for (field, value) in PROGRAM_INFO_FIELDS.iter().zip(values) {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!("{field}:")).strong(),
                                        )
                                        .selectable(true),
                                    );
                                    ui.add(
                                        egui::Label::new(value.unwrap_or("Not reported"))
                                            .selectable(true),
                                    );
                                    ui.end_row();
                                }
                            });
                        });
                } else {
                    ui.label("No program information fields were reported.");
                }
                detail_section(ui, "Raw response", contents, false);
            });
    });
}

fn detail_section(ui: &mut egui::Ui, title: &str, contents: &str, open: bool) {
    egui::CollapsingHeader::new(title)
        .default_open(open)
        .show(ui, |ui| {
            // An immutable text buffer allows selection/copy without editing.
            let mut text = contents.trim();
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .frame(
                        egui::Frame::new()
                            .inner_margin(4)
                            .fill(ui.visuals().text_edit_bg_color())
                            .stroke(egui::Stroke::new(1.0, egui::Color32::BLACK)),
                    )
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
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
        assert_eq!(
            address_book_status_path(Some(path), false),
            "address-book.json"
        );
        assert_eq!(
            address_book_status_path(Some(path), true),
            "address-book.json *"
        );
        assert_eq!(address_book_status_path(None, false), "Untitled");
        assert_eq!(address_book_status_path(None, true), "Untitled *");
    }
}
