use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Processor,
    Touchpanel,
    #[default]
    Unknown,
}

impl DeviceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Processor => "Processor",
            Self::Touchpanel => "Touchpanel",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceSource {
    Discovered,
    AddressBook,
}

#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceDetails {
    pub identity: String,
    pub network: String,
    pub programs: String,
    pub ip_table: String,
    pub cresnet: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Device {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub name: String,
    pub model: String,
    pub firmware: String,
    pub mac: String,
    pub kind: DeviceKind,
    pub source: DeviceSource,
    pub credentials: Credentials,
    pub program_slots: [Option<PathBuf>; 10],
    pub config_slots: [Option<PathBuf>; 10],
    pub touchpanel_project: Option<PathBuf>,
    pub selected: bool,
    pub connection: ConnectionState,
    pub progress: Option<(u64, u64)>,
    pub details: Option<DeviceDetails>,
    pub last_message: String,
}

impl Device {
    pub fn from_address(entry: &AddressEntry) -> Self {
        let id = endpoint_id(&entry.host, entry.port);
        Self {
            id,
            host: entry.host.clone(),
            port: entry.port,
            name: entry.name.clone(),
            model: entry.model.clone(),
            firmware: entry.firmware.clone(),
            mac: entry.mac.clone(),
            kind: entry.kind,
            source: DeviceSource::AddressBook,
            credentials: Credentials {
                username: entry.username.clone(),
                password: String::new(),
            },
            program_slots: entry.program_slots.clone(),
            config_slots: entry.config_slots.clone(),
            touchpanel_project: entry.touchpanel_project.clone(),
            selected: false,
            connection: ConnectionState::Disconnected,
            progress: None,
            details: None,
            last_message: "Password is kept only for this session".into(),
        }
    }

    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.host
        } else {
            &self.name
        }
    }

    pub fn to_address_entry(&self) -> AddressEntry {
        AddressEntry {
            name: self.name.clone(),
            host: self.host.clone(),
            port: self.port,
            username: self.credentials.username.clone(),
            kind: self.kind,
            model: self.model.clone(),
            firmware: self.firmware.clone(),
            mac: self.mac.clone(),
            program_slots: self.program_slots.clone(),
            config_slots: self.config_slots.clone(),
            touchpanel_project: self.touchpanel_project.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Busy,
    Error,
}

impl ConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "Offline",
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Busy => "Working",
            Self::Error => "Error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressEntry {
    pub name: String,
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub kind: DeviceKind,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub firmware: String,
    #[serde(default)]
    pub mac: String,
    #[serde(default)]
    pub program_slots: [Option<PathBuf>; 10],
    #[serde(default)]
    pub config_slots: [Option<PathBuf>; 10],
    #[serde(default)]
    pub touchpanel_project: Option<PathBuf>,
}

impl Default for AddressEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: default_ssh_port(),
            username: String::new(),
            kind: DeviceKind::Unknown,
            model: String::new(),
            firmware: String::new(),
            mac: String::new(),
            program_slots: Default::default(),
            config_slots: Default::default(),
            touchpanel_project: None,
        }
    }
}

pub const fn default_ssh_port() -> u16 {
    22
}

pub fn endpoint_id(host: &str, port: u16) -> String {
    format!("{}:{port}", host.trim().to_ascii_lowercase())
}

pub fn classify_model(model: &str) -> DeviceKind {
    let model = model.trim().to_ascii_uppercase();
    const PANELS: &[&str] = &["TSW-", "TSS-", "TS-", "TPMC-", "DGE-", "TST-"];
    const PROCESSORS: &[&str] = &[
        "CP3", "CP4", "MC3", "MC4", "RMC3", "RMC4", "PRO3", "AV3", "DIN-AP", "DMPS", "VC-4",
    ];

    if PANELS.iter().any(|prefix| model.starts_with(prefix)) {
        DeviceKind::Touchpanel
    } else if PROCESSORS.iter().any(|prefix| model.starts_with(prefix)) {
        DeviceKind::Processor
    } else {
        DeviceKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_models() {
        assert_eq!(classify_model("RMC4"), DeviceKind::Processor);
        assert_eq!(classify_model("VC-4"), DeviceKind::Processor);
        assert_eq!(classify_model("TSW-1070"), DeviceKind::Touchpanel);
        assert_eq!(classify_model("NVX-360"), DeviceKind::Unknown);
    }

    #[test]
    fn older_address_entries_receive_empty_file_assignments() {
        let entry: AddressEntry = serde_json::from_str(
            r#"{"name":"Room","host":"192.0.2.10","port":22,"username":"admin","kind":"Processor"}"#,
        )
        .unwrap();
        assert!(entry.program_slots.iter().all(Option::is_none));
        assert!(entry.config_slots.iter().all(Option::is_none));
        assert!(entry.touchpanel_project.is_none());
    }
}
