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

/// How the device's last finished operation ended. The card shows this
/// instead of the operation's own text, which goes to the device log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    FirmwareUpToDate,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceSource {
    Discovered,
    AddressBook,
}

/// Held in memory only; the password is never serialized. Debug output must
/// not carry it either, the way an API token's does not: it is reached through
/// `Device`, `ConnectionSpec` and every `WorkerCommand`.
#[derive(Clone, Default)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub struct DeviceDetails {
    pub disk_free: String,
    pub ram_free: String,
    pub identity: String,
    pub network: String,
    pub programs: String,
    pub ip_table: String,
    pub cresnet: Option<String>,
}

/// Observed this session, independent of an editable address-book display name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredIdentity {
    pub ip: String,
    pub hostname: String,
    pub model: String,
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
    pub discovered: Option<DiscoveredIdentity>,
    pub credentials: Credentials,
    pub ssh_host_key_fingerprint: Option<String>,
    pub https_certificate: Option<HttpsCertificateTrust>,
    pub vc4_api_token: Vc4ApiToken,
    pub program_slots: [Option<PathBuf>; 10],
    pub config_slots: [Option<PathBuf>; 10],
    pub touchpanel_project: Option<PathBuf>,
    pub selected: bool,
    pub connection: ConnectionState,
    pub progress: Option<(u64, u64)>,
    pub details: Option<DeviceDetails>,
    pub last_message: String,
    pub last_outcome: Option<Outcome>,
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
            discovered: None,
            credentials: Credentials {
                username: entry.username.clone(),
                password: String::new(),
            },
            ssh_host_key_fingerprint: entry.ssh_host_key_fingerprint.clone(),
            https_certificate: entry.https_certificate.clone(),
            vc4_api_token: entry.vc4_api_token.clone(),
            program_slots: entry.program_slots.clone(),
            config_slots: entry.config_slots.clone(),
            touchpanel_project: entry.touchpanel_project.clone(),
            selected: false,
            connection: ConnectionState::Disconnected,
            progress: None,
            details: None,
            last_message: String::new(),
            last_outcome: None,
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
            ssh_host_key_fingerprint: self.ssh_host_key_fingerprint.clone(),
            https_certificate: self.https_certificate.clone(),
            vc4_api_token: self.vc4_api_token.clone(),
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
pub struct HttpsCertificateTrust {
    pub endpoint: String,
    pub fingerprint: String,
}

/// Portable address books intentionally serialize this secret; debug output must not.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Vc4ApiToken(String);

impl Vc4ApiToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Vc4ApiToken {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl std::fmt::Debug for Vc4ApiToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host_key_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https_certificate: Option<HttpsCertificateTrust>,
    #[serde(default, skip_serializing_if = "Vc4ApiToken::is_empty")]
    pub vc4_api_token: Vc4ApiToken,
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
            ssh_host_key_fingerprint: None,
            https_certificate: None,
            vc4_api_token: Vc4ApiToken::default(),
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
        assert!(entry.ssh_host_key_fingerprint.is_none());
    }
}
