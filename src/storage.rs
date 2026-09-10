use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::model::{AddressEntry, endpoint_id};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub address_book: Vec<AddressEntry>,
    /// Read-only compatibility with settings written before fingerprints were
    /// stored on each address-book entry.
    #[serde(default, rename = "trusted_host_keys", skip_serializing)]
    pub(crate) legacy_trusted_host_keys: HashMap<String, String>,
    #[serde(default)]
    pub default_username: String,
    #[serde(default)]
    pub default_password: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddressBookDocument {
    #[serde(default = "address_book_version")]
    version: u32,
    devices: Vec<AddressEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AddressBookImport {
    Document(AddressBookDocument),
    Entries(Vec<AddressEntry>),
    LegacyConfig {
        address_book: Vec<AddressEntry>,
        #[serde(default)]
        trusted_host_keys: HashMap<String, String>,
    },
}

impl AppConfig {
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = config_path() else {
            return (
                Self::default(),
                Some("Could not locate the user configuration directory".into()),
            );
        };
        if !path.exists() {
            return (Self::default(), None);
        }
        match Self::load_from(&path) {
            Ok(config) => (config, None),
            Err(error) => (
                Self::default(),
                Some(format!("Could not read {}: {error}", path.display())),
            ),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let path = config_path().ok_or_else(|| io::Error::other("no configuration directory"))?;
        self.save_to(&path)
    }

    fn load_from(path: &Path) -> io::Result<Self> {
        let mut config: Self =
            serde_json::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)?;
        migrate_trusted_host_keys(
            &mut config.address_book,
            &mut config.legacy_trusted_host_keys,
        );
        validate_entries(&config.address_book)?;
        Ok(config)
    }

    fn save_to(&self, path: &Path) -> io::Result<()> {
        let mut config = self.clone();
        migrate_trusted_host_keys(
            &mut config.address_book,
            &mut config.legacy_trusted_host_keys,
        );
        validate_entries(&config.address_book)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(&config).map_err(io::Error::other)?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, data)?;
        fs::rename(temporary, path)
    }

    pub fn save_preferences(&self) -> io::Result<()> {
        let path = config_path().ok_or_else(|| io::Error::other("no configuration directory"))?;
        self.save_preferences_to(&path)
    }

    fn save_preferences_to(&self, path: &Path) -> io::Result<()> {
        let mut saved = match Self::load_from(path) {
            Ok(saved) => saved,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error),
        };
        saved.default_username = self.default_username.clone();
        saved.default_password = self.default_password.clone();
        saved.save_to(path)
    }
}

pub fn save_address_book(path: &Path, entries: &[AddressEntry]) -> io::Result<()> {
    validate_portable_path(path)?;
    validate_entries(entries)?;
    let document = AddressBookDocument {
        version: address_book_version(),
        devices: entries.to_vec(),
    };
    let data = serde_json::to_vec_pretty(&document).map_err(io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, data)?;
    fs::rename(temporary, path)
}

pub fn load_address_book(path: &Path) -> io::Result<Vec<AddressEntry>> {
    let data = fs::read_to_string(path)?;
    let imported: AddressBookImport = serde_json::from_str(&data).map_err(io::Error::other)?;
    let entries = match imported {
        AddressBookImport::Document(document) => {
            if document.version != address_book_version() {
                return Err(io::Error::other("unsupported address-book version"));
            }
            document.devices
        }
        AddressBookImport::Entries(entries) => entries,
        AddressBookImport::LegacyConfig {
            mut address_book,
            mut trusted_host_keys,
        } => {
            migrate_trusted_host_keys(&mut address_book, &mut trusted_host_keys);
            address_book
        }
    };
    validate_entries(&entries)?;
    Ok(entries)
}

fn validate_entries(entries: &[AddressEntry]) -> io::Result<()> {
    let mut endpoints = HashSet::new();
    for entry in entries {
        if entry.host.trim().is_empty() || entry.port == 0 {
            return Err(io::Error::other(
                "every device requires a host and nonzero SSH port",
            ));
        }
        let id = endpoint_id(&entry.host, entry.port);
        if !endpoints.insert(id.clone()) {
            return Err(io::Error::other(format!("duplicate device endpoint: {id}")));
        }
    }
    Ok(())
}

fn migrate_trusted_host_keys(
    entries: &mut [AddressEntry],
    trusted_host_keys: &mut HashMap<String, String>,
) {
    for entry in entries {
        let id = endpoint_id(&entry.host, entry.port);
        let legacy_fingerprint = trusted_host_keys.remove(&id);
        if entry.ssh_host_key_fingerprint.is_none() {
            entry.ssh_host_key_fingerprint = legacy_fingerprint;
        }
    }
}

pub fn validate_portable_path(path: &Path) -> io::Result<()> {
    let internal = config_path().ok_or_else(|| io::Error::other("no configuration directory"))?;
    ensure_separate_path(path, &internal)?;
    let firmware = firmware_dir().ok_or_else(|| io::Error::other("no configuration directory"))?;
    if resolved_path(path)?.starts_with(resolved_path(&firmware)?) {
        return Err(io::Error::other(
            "choose a JSON file outside the firmware storage directory",
        ));
    }
    Ok(())
}

fn ensure_separate_path(path: &Path, internal: &Path) -> io::Result<()> {
    if resolved_path(path)? == resolved_path(internal)? {
        return Err(io::Error::other(
            "choose a JSON file outside the internal application configuration file",
        ));
    }
    Ok(())
}

fn resolved_path(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    match fs::canonicalize(&path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(error)?;
            let name = path
                .file_name()
                .ok_or_else(|| io::Error::other("invalid file path"))?;
            Ok(resolved_path(parent)?.join(name))
        }
        Err(error) => Err(error),
    }
}

const fn address_book_version() -> u32 {
    1
}

pub fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("com", "WorldDomination", "CrestronLoadRunner")
        .map(|dirs| dirs.config_dir().join("address-book.json"))
}

pub fn firmware_dir() -> Option<PathBuf> {
    config_path().and_then(|path| path.parent().map(|parent| parent.join("firmware")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeviceKind;
    use crate::test_support::TestDir;

    #[test]
    fn rejects_portable_document_as_local_config() {
        let dir = TestDir::new();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"version":1,"devices":[]}"#).unwrap();
        assert!(AppConfig::load_from(&path).is_err());
    }

    #[test]
    fn loads_settings_created_before_default_credentials_were_added() {
        let dir = TestDir::new();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"address_book":[],"trusted_host_keys":{}}"#).unwrap();

        let config = AppConfig::load_from(&path).unwrap();

        assert!(config.default_username.is_empty());
        assert!(config.default_password.is_empty());
    }

    #[test]
    fn prevents_config_path_collision_including_aliases() {
        let dir = TestDir::new();
        let path = dir.path().join("config.json");
        assert!(ensure_separate_path(&path, &path).is_err());
        AppConfig::default().save_to(&path).unwrap();
        let original = fs::read(&path).unwrap();
        assert!(ensure_separate_path(&dir.path().join("./config.json"), &path).is_err());
        #[cfg(unix)]
        {
            let alias = dir.path().join("alias.json");
            std::os::unix::fs::symlink(&path, &alias).unwrap();
            assert!(ensure_separate_path(&alias, &path).is_err());
        }
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(ensure_separate_path(&dir.path().join("export.json"), &path).is_ok());
    }

    #[test]
    fn migrates_legacy_host_keys_into_address_book_entries() {
        let dir = TestDir::new();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{
                "address_book": [{"name": "Room", "host": "192.0.2.1"}],
                "trusted_host_keys": {"192.0.2.1:22": "SHA256:legacy"}
            }"#,
        )
        .unwrap();

        let config = AppConfig::load_from(&path).unwrap();
        assert_eq!(
            config.address_book[0].ssh_host_key_fingerprint.as_deref(),
            Some("SHA256:legacy")
        );
        assert!(config.legacy_trusted_host_keys.is_empty());

        config.save_to(&path).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.get("trusted_host_keys").is_none());
        assert_eq!(
            saved["address_book"][0]["ssh_host_key_fingerprint"],
            "SHA256:legacy"
        );
    }

    #[test]
    fn saving_preferences_preserves_the_saved_address_book() {
        let dir = TestDir::new();
        let path = dir.path().join("config.json");
        let mut config = AppConfig {
            address_book: vec![AddressEntry {
                host: "192.0.2.1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        config.save_to(&path).unwrap();
        let saved_book = config.address_book.clone();
        config.address_book[0].name = "Unsaved edit".into();
        config.default_username = "admin".into();
        config.default_password = "secret".into();

        config.save_preferences_to(&path).unwrap();

        let reloaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(reloaded.address_book, saved_book);
        assert_eq!(reloaded.default_username, "admin");
        assert_eq!(reloaded.default_password, "secret");
    }

    #[test]
    fn rejects_duplicate_endpoints_in_all_import_formats() {
        let dir = TestDir::new();
        let path = dir.path().join("duplicate.json");
        let entries = vec![
            AddressEntry {
                host: "Room".into(),
                ..Default::default()
            },
            AddressEntry {
                host: " room ".into(),
                ..Default::default()
            },
        ];
        for data in [
            serde_json::json!({"version":1, "devices":entries}),
            serde_json::json!({"address_book":entries}),
            serde_json::json!(entries),
        ] {
            fs::write(&path, data.to_string()).unwrap();
            assert!(
                load_address_book(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("duplicate")
            );
        }
        assert!(save_address_book(&path, &entries).is_err());
    }

    #[test]
    fn imports_legacy_address_book_shape() {
        let dir = TestDir::new();
        let path = dir.path().join("legacy.json");
        let data = r#"{
            "address_book":[{"name":"Room","host":"192.0.2.1","username":"admin","kind":"Processor"}],
            "trusted_host_keys":{"192.0.2.1:22":"SHA256:legacy-portable"}
        }"#;
        fs::write(&path, data).unwrap();

        let address_book = load_address_book(&path).unwrap();
        assert_eq!(address_book.len(), 1);
        assert_eq!(address_book[0].host, "192.0.2.1");
        assert_eq!(
            address_book[0].ssh_host_key_fingerprint.as_deref(),
            Some("SHA256:legacy-portable")
        );
    }

    #[test]
    fn json_file_round_trip_preserves_assignments_and_host_key() {
        let mut entry = AddressEntry {
            name: "Control Room".into(),
            host: "192.0.2.2".into(),
            username: "admin".into(),
            ssh_host_key_fingerprint: Some("SHA256:known-host-key".into()),
            kind: DeviceKind::Processor,
            ..Default::default()
        };
        entry.program_slots[2] = Some(PathBuf::from("programs/control-room.lpz"));
        entry.config_slots[4] = Some(PathBuf::from("config/control-room.json"));
        let path = std::env::temp_dir().join(format!(
            "crestron-load-runner-address-book-{}.json",
            std::process::id()
        ));

        save_address_book(&path, &[entry]).unwrap();
        let loaded = load_address_book(&path).unwrap();
        let _ = fs::remove_file(path);

        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].program_slots[2].as_deref(),
            Some(Path::new("programs/control-room.lpz"))
        );
        assert_eq!(
            loaded[0].config_slots[4].as_deref(),
            Some(Path::new("config/control-room.json"))
        );
        assert_eq!(
            loaded[0].ssh_host_key_fingerprint.as_deref(),
            Some("SHA256:known-host-key")
        );
    }
}
