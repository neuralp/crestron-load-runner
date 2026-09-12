//! Read-only Crestron Virtual Control REST API and device-details tables.
//! Reference: https://docs.crestron.com/en-us/8314/Content/Topics/API-Reference/API-Reference.htm

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::Duration,
};

use crate::model::HttpsCertificateTrust;
use eframe::egui;
use reqwest::{
    Url,
    blocking::Client,
    header::{ACCEPT, AUTHORIZATION, HeaderValue},
};
use serde_json::{Map, Value};

#[path = "vc4_tls.rs"]
mod tls;

const BODY_LIMIT: u64 = 16 * 1024 * 1024;
const PAGE_SIZE: usize = 50;

pub fn is_vc4(model: &str) -> bool {
    model.trim().eq_ignore_ascii_case("VC-4")
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Resource {
    Library,
    Instances,
    IpTable(String),
}

impl Resource {
    fn endpoint(&self) -> &'static str {
        match self {
            Self::Library => "ProgramLibrary",
            Self::Instances => "ProgramInstance",
            Self::IpTable(_) => "IpTableByPID",
        }
    }

    fn collection(&self) -> &'static str {
        match self {
            Self::Instances => "ProgramInstanceLibrary",
            _ => self.endpoint(),
        }
    }
}

#[derive(Clone, Debug)]
struct Record {
    id: String,
    fields: Map<String, Value>,
}

impl Record {
    fn is_stopped(&self) -> bool {
        self.fields
            .get("Status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.trim().eq_ignore_ascii_case("stopped"))
    }

    fn cell(&self, field: &str) -> String {
        match self.fields.get(field) {
            Some(Value::String(value)) => value.clone(),
            Some(Value::Null) | None => String::new(),
            Some(value) => value.to_string(),
        }
    }

    fn matches(&self, query: &str) -> bool {
        self.id.to_lowercase().contains(query)
            || self
                .fields
                .values()
                .any(|v| v.to_string().to_lowercase().contains(query))
    }
}

/// The GET collections are objects keyed by server IDs, not arrays. In
/// particular a ProgramInstanceLibrary key is not necessarily its instance ID.
fn parse_records(resource: &Resource, value: &Value) -> Result<Vec<Record>, String> {
    // HTTP 200 can contain INVALID ID. Never echo arbitrary server status
    // text, which could contain credentials, into the UI.
    fn invalid_id(value: &Value) -> bool {
        match value {
            Value::Object(fields) => {
                fields
                    .get("StatusInfo")
                    .and_then(Value::as_str)
                    .is_some_and(|s| {
                        s.split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                            .eq_ignore_ascii_case("INVALID ID")
                    })
                    || fields.values().any(invalid_id)
            }
            Value::Array(values) => values.iter().any(invalid_id),
            _ => false,
        }
    }
    if invalid_id(value) {
        return Err(if matches!(resource, Resource::IpTable(_)) {
            "VC-4 returned INVALID ID for this ProgramInstanceId. Refresh ProgramInstances and retry this instance's IP table."
        } else { "VC-4 returned INVALID ID; refresh the server data and retry." }.into());
    }
    let collection = value.get("Device")
        .and_then(|v| v.get("Programs"))
        .and_then(|v| v.get(resource.collection()))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            let location = if value.get("Device").is_none() { "Device is missing" }
                else if value.pointer("/Device/Programs").is_none() { "Device.Programs is missing" }
                else if value["Device"]["Programs"].get(resource.collection()).is_none() { "the requested collection is missing" }
                else { "the requested collection is not an object" };
            format!("Expected Device.Programs.{} object; {location}. This may be an API error or a different firmware response format. Share a redacted response JSON to diagnose it; do not include the API token.", resource.collection())
        })?;
    let mut ids = BTreeSet::new();
    let mut rows = Vec::with_capacity(collection.len());
    for (key, value) in collection {
        let fields = value.as_object().ok_or("Invalid record in API response")?;
        let id = match resource {
            Resource::Instances => fields
                .get("ProgramInstanceId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or("ProgramInstanceId is missing or invalid")?,
            Resource::Library => fields
                .get("ProgramId")
                .and_then(Value::as_str)
                .unwrap_or(key),
            Resource::IpTable(pid) => {
                if fields
                    .get("ProgramInstanceId")
                    .is_some_and(|id| id.as_str() != Some(pid))
                {
                    return Err("IP table belongs to a different program instance".into());
                }
                key
            }
        };
        if !ids.insert(id.to_owned()) {
            return Err("Duplicate ID in API response".into());
        }
        rows.push(Record {
            id: id.to_owned(),
            fields: fields.clone(),
        });
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rows)
}

fn server_url(host: &str, port: u16) -> Result<Url, String> {
    let host = host.trim();
    if host.is_empty()
        || port == 0
        || host
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '\\' | '@' | '?' | '#' | '%'))
    {
        return Err("Use a server hostname or IP address and a valid HTTPS port, not a URL".into());
    }
    let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Url::parse(&format!("https://{host}:{port}/")).map_err(|_| "Invalid VC-4 server address".into())
}

fn resource_url(base: &Url, resource: &Resource) -> Result<Url, String> {
    let mut url = base.clone();
    let mut path = url
        .path_segments_mut()
        .map_err(|_| "Invalid API base URL")?;
    path.clear()
        .extend(["VirtualControl", "config", "api", resource.endpoint()]);
    if let Resource::IpTable(pid) = resource {
        if pid.is_empty() || matches!(pid.as_str(), "." | "..") || pid.chars().any(char::is_control)
        {
            return Err("Invalid ProgramInstanceId".into());
        }
        path.push(pid);
    }
    drop(path);
    Ok(url)
}

fn authorization(token: &str) -> Result<HeaderValue, String> {
    if token.trim().is_empty() || token.chars().any(char::is_control) {
        return Err("Enter a valid VC-4 API token from Settings → Tokens".into());
    }
    let mut header = HeaderValue::from_str(token).map_err(|_| "Invalid API token header")?;
    header.set_sensitive(true);
    Ok(header)
}

fn client(trusted: Option<String>, untrusted: tls::UntrustedCertificate) -> Result<Client, String> {
    Client::builder()
        .use_preconfigured_tls(tls::config(trusted, untrusted)?)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| "Could not initialize HTTPS certificate verification".into())
}

fn fetch(
    client: &Client,
    base: &Url,
    token: &HeaderValue,
    resource: &Resource,
) -> Result<Vec<Record>, String> {
    let url = resource_url(base, resource)?;
    let response = client.get(url).header(AUTHORIZATION, token.clone()).header(ACCEPT, "application/json")
        .send().map_err(|error| {
            if error.is_timeout() { "VC-4 request timed out".to_owned() }
            else { "Could not connect to VC-4 over HTTPS. Check the hostname, port and certificate approval.".to_owned() }
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 | 403 => "VC-4 rejected the API token or its permissions (HTTP 401/403)".into(),
            300..=399 => "VC-4 redirected the API request; redirects are not followed. Check the server address".into(),
            _ => format!("VC-4 returned HTTP {} for {}", status.as_u16(), resource.endpoint()),
        });
    }
    if response
        .content_length()
        .is_some_and(|size| size > BODY_LIMIT)
    {
        return Err("VC-4 response exceeds 16 MiB".into());
    }
    let mut bytes = Vec::new();
    response
        .take(BODY_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read VC-4 response (connection interrupted or timed out)")?;
    if bytes.len() as u64 > BODY_LIMIT {
        return Err("VC-4 response exceeds 16 MiB".into());
    }
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| "VC-4 returned invalid JSON (check the API address and authentication)")?;
    parse_records(resource, &value)
}

type Reply = (Resource, Result<Vec<Record>, String>);

struct Worker {
    requests: Sender<Resource>,
    replies: Receiver<Reply>,
    cancelled: Arc<AtomicBool>,
    untrusted: tls::UntrustedCertificate,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Worker {
    fn start(base: Url, token: HeaderValue, trusted: Option<String>) -> Result<Self, String> {
        let (requests, incoming) = mpsc::channel();
        let (outgoing, replies) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let untrusted = tls::UntrustedCertificate::default();
        let observed = untrusted.clone();
        std::thread::Builder::new()
            .name("vc4-api".into())
            .spawn(move || {
                // Client construction and every
                // network operation happen off the UI thread. One request at a time.
                let client = client(trusted, observed.clone());
                while let Ok(resource) = incoming.recv() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let result = match &client {
                        Ok(client) => fetch(client, &base, &token, &resource),
                        Err(error) => Err(error.clone()),
                    };
                    if outgoing.send((resource, result)).is_err()
                        || observed.lock().map_or(true, |seen| seen.is_some())
                    {
                        break;
                    }
                }
            })
            .map_err(|_| "Could not start VC-4 worker")?;
        Ok(Self {
            requests,
            replies,
            cancelled,
            untrusted,
        })
    }
}

#[derive(Default)]
enum Load {
    #[default]
    Idle,
    Loading,
    Ready(Vec<Record>),
    Error(String),
}

impl Load {
    fn rows(&self) -> &[Record] {
        if let Self::Ready(rows) = self {
            rows
        } else {
            &[]
        }
    }

    fn message(&self, ui: &mut egui::Ui) {
        match self {
            Self::Idle => {
                ui.weak("Not fetched. Connect / refresh to retrieve this table.");
            }
            Self::Loading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading…");
                });
            }
            Self::Error(error) => {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            Self::Ready(rows) if rows.is_empty() => {
                ui.weak("No entries.");
            }
            Self::Ready(_) => (),
        }
    }
}

/// Transient state only: neither tokens nor API data enter an address book or
/// preferences file. Dropping a panel disconnects its reply channel so results
/// cannot leak into a newly opened address book, even at the same endpoint.
pub struct Panel {
    token: String,
    port: u16,
    trusted: Option<HttpsCertificateTrust>,
    endpoint: Option<String>,
    pending_certificate: Option<String>,
    trust_action: Option<TrustAction>,
    save_token: bool,
    worker: Option<Worker>,
    library: Load,
    instances: Load,
    tables: BTreeMap<String, Load>,
    expanded: BTreeSet<String>,
    library_view: TableView,
    instances_view: TableView,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            token: String::new(),
            port: 443,
            trusted: None,
            endpoint: None,
            pending_certificate: None,
            trust_action: None,
            save_token: false,
            worker: None,
            library: Load::Idle,
            instances: Load::Idle,
            tables: BTreeMap::new(),
            expanded: BTreeSet::new(),
            library_view: TableView::default(),
            instances_view: TableView::default(),
        }
    }
}

pub enum TrustAction {
    Accept(HttpsCertificateTrust),
    Forget,
}

impl Panel {
    pub fn with_token(mut self, token: &crate::model::Vc4ApiToken) -> Self {
        self.token = token.as_str().to_owned();
        self
    }

    pub fn token(&self) -> crate::model::Vc4ApiToken {
        self.token.clone().into()
    }

    pub fn take_token_save(&mut self) -> bool {
        std::mem::take(&mut self.save_token)
    }

    pub fn new(trusted: Option<HttpsCertificateTrust>) -> Self {
        let port = trusted
            .as_ref()
            .and_then(|t| Url::parse(&t.endpoint).ok())
            .and_then(|url| url.port_or_known_default())
            .unwrap_or(443);
        Self {
            port,
            trusted,
            ..Default::default()
        }
    }

    pub fn take_trust_action(&mut self) -> Option<TrustAction> {
        self.trust_action.take()
    }

    pub fn set_trust(&mut self, trusted: Option<HttpsCertificateTrust>) {
        self.reset();
        self.trusted = trusted;
    }

    fn trusted_for(&self, url: &Url) -> Option<String> {
        self.trusted
            .as_ref()
            .filter(|t| t.endpoint == url.as_str())
            .map(|t| t.fingerprint.clone())
    }

    fn reset(&mut self) {
        self.pending_certificate = None;
        self.trust_action = None;
        self.worker = None;
        self.library = Load::Idle;
        self.instances = Load::Idle;
        self.tables.clear();
        self.expanded.clear();
    }

    pub fn refresh(&mut self, host: &str) {
        self.reset();
        let started = server_url(host, self.port).and_then(|url| {
            self.endpoint = Some(url.to_string());
            let trusted = self.trusted_for(&url);
            authorization(&self.token).and_then(|token| Worker::start(url, token, trusted))
        });
        match started {
            Ok(worker) => {
                self.worker = Some(worker);
                self.request(Resource::Library);
                self.request(Resource::Instances);
            }
            Err(error) => {
                self.library = Load::Error(error.clone());
                self.instances = Load::Error(error);
            }
        }
    }

    fn slot(&mut self, resource: &Resource) -> &mut Load {
        match resource {
            Resource::Library => &mut self.library,
            Resource::Instances => &mut self.instances,
            Resource::IpTable(pid) => self.tables.entry(pid.clone()).or_default(),
        }
    }

    fn request(&mut self, resource: Resource) {
        if let Resource::IpTable(pid) = &resource
            && self
                .instances
                .rows()
                .iter()
                .any(|row| row.id == *pid && row.is_stopped())
        {
            self.tables.remove(pid);
            return;
        }
        if matches!(self.slot(&resource), Load::Loading) {
            return;
        }
        let result = self
            .worker
            .as_ref()
            .ok_or("Connect / refresh before fetching an IP table")
            .and_then(|w| {
                w.requests
                    .send(resource.clone())
                    .map_err(|_| "VC-4 worker stopped; connect / refresh to retry")
            });
        *self.slot(&resource) = match result {
            Ok(()) => Load::Loading,
            Err(e) => Load::Error(e.into()),
        };
    }

    fn poll(&mut self) {
        loop {
            let pending = self
                .worker
                .as_ref()
                .and_then(|w| w.untrusted.lock().ok()?.clone());
            if let Some(fingerprint) = pending {
                self.reset();
                self.pending_certificate = Some(fingerprint);
                self.library = Load::Error(
                    "HTTPS certificate approval required; API token was not sent".into(),
                );
                self.instances = Load::Error("HTTPS certificate approval required".into());
                break;
            }
            let reply = self.worker.as_ref().map(|w| w.replies.try_recv());
            match reply {
                Some(Ok((resource, result))) => {
                    *self.slot(&resource) = match result {
                        Ok(rows) => Load::Ready(rows),
                        Err(e) => Load::Error(e),
                    };
                }
                Some(Err(mpsc::TryRecvError::Disconnected)) => {
                    for load in [&mut self.library, &mut self.instances]
                        .into_iter()
                        .chain(self.tables.values_mut())
                    {
                        if matches!(load, Load::Loading) {
                            *load = Load::Error(
                                "VC-4 worker stopped; connect / refresh to retry".into(),
                            );
                        }
                    }
                    self.worker = None;
                    break;
                }
                _ => break,
            }
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, device_id: &str) {
        self.poll();
        ui.push_id(("vc4", device_id), |ui| {
            ui.weak("VC-4 virtual server · read-only HTTPS API");
            egui::CollapsingHeader::new("VC-4 API connection").default_open(self.token.is_empty()).show(ui, |ui| {
                let mut changed = false;
                ui.horizontal(|ui| {
                    ui.label("API token");
                    changed |= ui.add(egui::TextEdit::singleline(&mut self.token).password(true)).changed();
                    if ui.button("Save token").clicked() { self.save_token = true; }
                    if ui.button("Forget token").clicked() { self.token.clear(); changed = true; self.save_token = true; }
                });
                ui.horizontal(|ui| {
                    ui.label("HTTPS port");
                    changed |= ui.add(egui::DragValue::new(&mut self.port).range(1..=65535)).changed();
                });
                if let Some(trusted) = &self.trusted {
                    ui.label(format!("Accepted HTTPS certificate for {}", trusted.endpoint));
                    ui.monospace(&trusted.fingerprint);
                    if ui.button("Forget HTTPS certificate").clicked() { self.trust_action = Some(TrustAction::Forget); }
                }
                ui.small("Create a read-only token in VC-4 web settings → Tokens. Paste the token itself, without a Bearer prefix. Tokens are saved unencrypted in the address-book JSON. Protect that file and do not share it. Save token / Forget token saves the address book, including other pending edits. SSH credentials are not used. Approve the HTTPS certificate fingerprint on first connection.");
                if changed { self.reset(); }
            });
            self.certificate_prompt(ui);
            ui.separator();
            ui.heading("ProgramLibrary");
            self.library.message(ui);
            let rows = self.library.rows();
            let indices = self.library_view.controls(ui, rows);
            plain_table(ui, "library", rows, &indices, LIBRARY_COLUMNS);
            ui.separator();
            ui.heading("ProgramInstances");
            self.instances.message(ui);
            let rows = self.instances.rows();
            let indices = self.instances_view.controls(ui, rows);
            let mut requests = Vec::new();
            egui::ScrollArea::horizontal().id_salt("instances_columns").show(ui, |ui| {
                table_row(ui, None, INSTANCE_COLUMNS, true, false);
                for index in indices {
                    let record = &rows[index];
                    let mut open = self.expanded.contains(&record.id);
                    ui.push_id(&record.id, |ui| {
                        if table_row(ui, Some(record), INSTANCE_COLUMNS, false, open) {
                            open = !open;
                            if open { self.expanded.insert(record.id.clone()); }
                            else { self.expanded.remove(&record.id); }
                        }
                        if open {
                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    ui.strong(format!("IPTableByPID · {}", record.id));
                                    let load = self.tables.get(&record.id);
                                    if ui.add_enabled(!record.is_stopped() && !matches!(load, Some(Load::Loading)), egui::Button::new("Refresh IP table")).clicked() {
                                        requests.push(Resource::IpTable(record.id.clone()));
                                    }
                                });
                                if record.is_stopped() {
                                    ui.label("Program is stopped; IP table is not fetched.");
                                } else if let Some(load) = self.tables.get(&record.id) {
                                    // Use the same URL builder as the worker so the
                                    // encoded ProgramInstanceId can be checked without
                                    // exposing the Authorization header or token.
                                    if let Some(base) = self.endpoint.as_deref().and_then(|s| Url::parse(s).ok())
                                        && let Ok(url) = resource_url(&base, &Resource::IpTable(record.id.clone())) {
                                        ui.add(egui::Label::new(format!("GET {}", url.path())).selectable(true));
                                    }
                                    load.message(ui);
                                    // Bounded viewport; all IP entries remain reachable.
                                    egui::ScrollArea::vertical().id_salt("ip_rows").max_height(280.0).show(ui, |ui| {
                                        let ip_rows = load.rows();
                                        plain_table(ui, "ip_table", ip_rows, &(0..ip_rows.len()).collect::<Vec<_>>(), IP_COLUMNS);
                                    });
                                } else {
                                    ui.label("Loading…");
                                    requests.push(Resource::IpTable(record.id.clone()));
                                }
                                ui.collapsing("Instance properties", |ui| properties(ui, record));
                            });
                        }
                    });
                }
            });
            for resource in requests { self.request(resource); }
            if matches!(self.library, Load::Loading) || matches!(self.instances, Load::Loading)
                || self.tables.values().any(|v| matches!(v, Load::Loading)) {
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
        });
    }

    fn certificate_prompt(&mut self, ui: &mut egui::Ui) {
        let Some(fingerprint) = self.pending_certificate.clone() else {
            return;
        };
        let Some(endpoint) = self.endpoint.clone() else {
            return;
        };
        let previous = self.trusted.as_ref().filter(|t| t.endpoint == endpoint);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong(if previous.is_some() { "HTTPS certificate changed" } else { "Untrusted HTTPS certificate" });
            ui.label(&endpoint);
            if let Some(previous) = previous {
                ui.label("Previously accepted SHA-256 fingerprint:");
                ui.monospace(&previous.fingerprint);
            }
            ui.label("Presented SHA-256 certificate fingerprint:");
            ui.monospace(&fingerprint);
            ui.label("Compare this fingerprint with a known-good value from the server administrator. Certificate changes can indicate replacement or interception. No API token is sent until you accept.");
            ui.small("Accepting saves the address book, including any other pending address-book edits. A discovered server will be added to it.");
            ui.horizontal(|ui| {
                let label = if previous.is_some() { "Accept replacement and save" } else { "Accept certificate and save" };
                if ui.button(label).clicked() {
                    self.trust_action = Some(TrustAction::Accept(HttpsCertificateTrust { endpoint, fingerprint }));
                }
                if ui.button("Reject certificate").clicked() {
                    self.pending_certificate = None;
                    self.library = Load::Error("HTTPS certificate rejected; no API token was sent".into());
                }
            });
        });
    }
}

type Column = (&'static str, f32);
const LIBRARY_COLUMNS: &[Column] = &[
    ("ProgramId", 100.0),
    ("FriendlyName", 200.0),
    ("AppFile", 220.0),
    ("ProgramType", 130.0),
    ("CompileDateTime", 180.0),
    ("Notes", 220.0),
    ("Tags", 150.0),
];
const INSTANCE_COLUMNS: &[Column] = &[
    ("ProgramInstanceId", 150.0),
    ("Name", 220.0),
    ("Status", 110.0),
    ("ProgramLibraryId", 140.0),
    ("Location", 150.0),
    ("LastStarted", 180.0),
    ("Runtime", 100.0),
    ("RestartRequired", 130.0),
];
const IP_COLUMNS: &[Column] = &[
    ("ProgramIpId", 100.0),
    ("Status", 100.0),
    ("remote_ip", 150.0),
    ("Hostname", 160.0),
    ("Model", 140.0),
    ("Description", 220.0),
    ("MacAddress", 170.0),
    ("DeviceId", 90.0),
    ("device_type", 90.0),
];

#[derive(Default)]
struct TableView {
    query: String,
    page: usize,
}

impl TableView {
    fn controls(&mut self, ui: &mut egui::Ui, rows: &[Record]) -> Vec<usize> {
        let mut indices = Vec::new();
        ui.horizontal(|ui| {
            ui.label("Search");
            if ui.text_edit_singleline(&mut self.query).changed() {
                self.page = 0;
            }
            let query = self.query.trim().to_lowercase();
            indices = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.matches(&query))
                .map(|(i, _)| i)
                .collect();
            let pages = indices.len().div_ceil(PAGE_SIZE).max(1);
            self.page = self.page.min(pages - 1);
            if ui
                .add_enabled(self.page > 0, egui::Button::new("Previous"))
                .clicked()
            {
                self.page -= 1;
            }
            ui.label(format!(
                "Page {} / {} · {} entries",
                self.page + 1,
                pages,
                indices.len()
            ));
            if ui
                .add_enabled(self.page + 1 < pages, egui::Button::new("Next"))
                .clicked()
            {
                self.page += 1;
            }
        });
        indices
            .into_iter()
            .skip(self.page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .collect()
    }
}

/// Fixed cell widths keep every row and heading aligned, while the detail
/// beneath an expanded row spans the table instead of widening its first cell.
fn table_row(
    ui: &mut egui::Ui,
    record: Option<&Record>,
    columns: &[Column],
    heading: bool,
    open: bool,
) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        if columns == INSTANCE_COLUMNS {
            if heading {
                ui.add_sized([26.0, 22.0], egui::Label::new(""));
            } else {
                clicked = ui
                    .add_sized(
                        [26.0, 22.0],
                        egui::Button::new(if open { "-" } else { "+" }),
                    )
                    .on_hover_text(if open {
                        "Collapse program instance"
                    } else {
                        "Expand program instance"
                    })
                    .clicked();
            }
        }
        for (field, width) in columns {
            let value = if heading {
                (*field).to_owned()
            } else {
                let record = record.expect("data rows have a record");
                if matches!(*field, "ProgramId" | "ProgramInstanceId") {
                    record.id.clone()
                } else {
                    record.cell(field)
                }
            };
            let text = if heading {
                egui::RichText::new(&value).strong()
            } else {
                egui::RichText::new(&value)
            };
            ui.add_sized([*width, 22.0], egui::Label::new(text).truncate())
                .on_hover_text(value);
        }
    });
    if heading {
        ui.separator();
    }
    clicked
}

fn plain_table(
    ui: &mut egui::Ui,
    id: &str,
    rows: &[Record],
    indices: &[usize],
    columns: &[Column],
) {
    egui::ScrollArea::horizontal().id_salt(id).show(ui, |ui| {
        table_row(ui, None, columns, true, false);
        for &index in indices {
            table_row(ui, Some(&rows[index]), columns, false, false);
        }
    });
}

fn properties(ui: &mut egui::Ui, record: &Record) {
    egui::Grid::new("properties").striped(true).show(ui, |ui| {
        for field in record.fields.keys() {
            ui.strong(field);
            ui.label(record.cell(field));
            ui.end_row();
        }
    });
}

#[cfg(test)]
#[path = "vc4_tests.rs"]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vc4_models_are_exact_case_insensitive_matches() {
        assert!(is_vc4(" VC-4 "));
        assert!(is_vc4("vc-4"));
        assert!(!is_vc4("VC-400"));
        assert!(!is_vc4("CP4"));
    }

    #[test]
    fn documented_envelopes_use_instance_ids_not_map_keys_or_library_ids() {
        let records = parse_records(
            &Resource::Instances,
            &json!({"Device":{"Programs":{
                "ProgramInstanceLibrary": {"ProgramInstanceId1": {
                    "id":2,"ProgramInstanceId":"21","ProgramLibraryId":"33",
                    "Name":"Room 21","Status":"Running","RestartRequired":false
                }}
            }}}),
        )
        .unwrap();
        assert_eq!(records[0].id, "21");
        assert_eq!(records[0].cell("Status"), "Running");
        assert_eq!(records[0].cell("RestartRequired"), "false");
        let library = parse_records(&Resource::Library, &json!({"Device":{"Programs":{
            "ProgramLibrary":{"33":{"ProgramId":"33","FriendlyName":"Shared program","AppFile":"room.cpz"}}
        }}})).unwrap();
        assert_eq!(library[0].id, "33");
        let ip = parse_records(&Resource::IpTable("21".into()), &json!({"Device":{"Programs":{
            "IpTableByPID":{"entry":{"ProgramInstanceId":"21","ProgramIpId":"12","remote_ip":"192.0.2.4","Status":"Online"}}
        }}})).unwrap();
        assert_eq!(ip[0].cell("remote_ip"), "192.0.2.4");
    }

    #[test]
    fn ip_table_invalid_id_is_reported_as_an_api_error_not_an_empty_table() {
        let resource = Resource::IpTable("P01".into());
        for value in [
            json!({"StatusInfo":"INVALID ID"}),
            json!({"Actions":[{"Results":[{"StatusInfo":"INVALID ID"}]}]}),
        ] {
            let error = parse_records(&resource, &value).unwrap_err();
            assert!(error.contains("INVALID ID"));
            assert!(error.contains("ProgramInstanceId"));
        }
        let error = parse_records(
            &resource,
            &json!({"Actions":[{"Results":[{"StatusInfo":"synthetic-secret"}]}]}),
        )
        .unwrap_err();
        assert!(!error.contains("synthetic-secret"));
    }

    #[test]
    fn empty_collections_are_distinct_from_errors_and_malformed_responses() {
        assert!(
            parse_records(
                &Resource::Library,
                &json!({"Device":{"Programs":{"ProgramLibrary":{}}}})
            )
            .unwrap()
            .is_empty()
        );
        for invalid in [
            json!({}),
            json!({"Actions":[{"Results":[{"StatusInfo":"INVALID ID"}]}]}),
            json!({"Device":{"Programs":{"ProgramLibrary":[]}}}),
            json!({"Device":{"Programs":{"ProgramLibrary":{"bad":null}}}}),
        ] {
            assert!(parse_records(&Resource::Library, &invalid).is_err());
        }
        assert!(
            parse_records(
                &Resource::Instances,
                &json!({"Device":{"Programs":{
                    "ProgramInstanceLibrary":{"missing-id":{"Name":"Room"}}
                }}})
            )
            .is_err()
        );
    }

    #[test]
    fn urls_require_https_and_encode_each_instance_as_one_path_segment() {
        let base = server_url("vc4.example.test", 8443).unwrap();
        assert_eq!(
            resource_url(&base, &Resource::IpTable("room/a ?#%".into()))
                .unwrap()
                .as_str(),
            "https://vc4.example.test:8443/VirtualControl/config/api/IpTableByPID/room%2Fa%20%3F%23%25"
        );
        assert!(server_url("user@host", 443).is_err());
        assert!(server_url("host/path", 443).is_err());
        assert!(server_url("http://host", 443).is_err());
        assert!(server_url("host", 0).is_err());
        assert!(server_url("2001:db8::1", 443).is_ok());
        assert!(resource_url(&base, &Resource::IpTable("..".into())).is_err());
        assert!(authorization("secret\r\nInjected: yes").is_err());
        assert!(authorization("").is_err());
        assert_eq!(authorization("test-token").unwrap(), "test-token");
    }
}
