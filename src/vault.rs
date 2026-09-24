//! Secrets kept in the operating system's per-user store: Windows Credential
//! Manager, or the Secret Service (GNOME Keyring, KWallet) on Linux. Both are
//! unlocked by signing in, so there is no master password to ask for.
//!
//! Where there is no store — a Linux session without a Secret Service — the
//! vault keeps what it is given for the rest of the process and nothing more.
//! A secret is never written anywhere else instead.

use std::{cell::RefCell, collections::HashMap};

use crate::model::endpoint_id;

/// The default SSH password from Preferences.
pub const DEFAULT_PASSWORD: &str = "default-password";

/// Where one device's secret is kept.
///
/// By MAC address when the device has one, because that survives a new IP
/// address from DHCP and reaching the same device by hostname instead; by host
/// and port otherwise. Either way it is not tied to an address book, so every
/// book that names the device finds it.
///
/// `legacy` is the host-and-port key for a device that has a MAC. It is where
/// the secret was kept before the MAC was known, or by a build that did not
/// key by MAC, and it is read once to move what it holds across.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceKey {
    key: String,
    legacy: Option<String>,
}

impl DeviceKey {
    /// A device's SSH password.
    pub fn ssh_password(mac: &str, host: &str, port: u16) -> Self {
        Self::new("ssh", mac, host, port)
    }

    /// A VC-4 server's API token.
    pub fn vc4_token(mac: &str, host: &str, port: u16) -> Self {
        Self::new("vc4", mac, host, port)
    }

    fn new(kind: &str, mac: &str, host: &str, port: u16) -> Self {
        let endpoint = format!("{kind}:{}", endpoint_id(host, port));
        match normalized_mac(mac) {
            Some(mac) => Self {
                key: format!("{kind}:mac:{mac}"),
                legacy: Some(endpoint),
            },
            None => Self {
                key: endpoint,
                legacy: None,
            },
        }
    }

    #[cfg(test)]
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// Twelve lowercase hex digits whatever the separators, or `None` for
/// anything that is not a usable MAC address: blank, malformed, or all zeros.
fn normalized_mac(mac: &str) -> Option<String> {
    let digits: String = mac
        .chars()
        .filter(|character| !matches!(character, ':' | '-' | '.') && !character.is_whitespace())
        .map(|character| character.to_ascii_lowercase())
        .collect();
    (digits.len() == 12
        && digits.chars().all(|character| character.is_ascii_hexdigit())
        && digits.chars().any(|character| character != '0'))
    .then_some(digits)
}

pub trait SecretStore {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError>;
    fn set(&self, key: &str, secret: &str) -> Result<(), StoreError>;
    /// Deleting what is not there succeeds.
    fn delete(&self, key: &str) -> Result<(), StoreError>;
}

#[derive(Debug)]
pub enum StoreError {
    /// There is no store to reach, or it refused access.
    Unavailable(String),
    Other(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) | Self::Other(message) => f.write_str(message),
        }
    }
}

/// The platform store, through `keyring`. Every key is an entry under
/// `service`.
pub struct OsStore {
    service: String,
}

impl OsStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(&self.service, key).map_err(store_error)
    }
}

fn store_error(error: keyring::Error) -> StoreError {
    match error {
        keyring::Error::NoStorageAccess(_) | keyring::Error::PlatformFailure(_) => {
            StoreError::Unavailable(error.to_string())
        }
        other => StoreError::Other(other.to_string()),
    }
}

impl SecretStore for OsStore {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        match self.entry(key)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }

    fn set(&self, key: &str, secret: &str) -> Result<(), StoreError> {
        self.entry(key)?.set_password(secret).map_err(store_error)
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(store_error(error)),
        }
    }
}

/// Every key saved under `service`, found by asking the platform store what it
/// holds rather than by knowing the keys: a device's is only known while the
/// device is listed. keyring cannot list, so this goes to the store itself.
///
/// keyring names a Windows entry `{key}.{service}`, and Credential Manager can
/// only filter by prefix, so every generic entry is read and matched on that
/// suffix here. The match is exact, which is what leaves a `--config-dir`
/// profile's `CrestronLoadRunner (<dir>)` entries alone.
#[cfg(windows)]
pub fn saved_keys(service: &str) -> Result<Vec<String>, String> {
    use windows_sys::Win32::{
        Foundation::{ERROR_NOT_FOUND, GetLastError},
        Security::Credentials::{CRED_TYPE_GENERIC, CREDENTIALW, CredEnumerateW, CredFree},
    };

    let suffix = format!(".{service}");
    let mut count = 0u32;
    let mut credentials: *mut *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: a null filter lists every credential; the out-pointers are valid
    // locals, and the list is freed below whatever is read from it.
    if unsafe { CredEnumerateW(std::ptr::null(), 0, &mut count, &mut credentials) } == 0 {
        let error = unsafe { GetLastError() };
        return if error == ERROR_NOT_FOUND {
            Ok(Vec::new())
        } else {
            Err(format!("Could not list Windows Credential Manager entries (error {error})"))
        };
    }
    let mut keys = Vec::new();
    for index in 0..count as usize {
        // SAFETY: CredEnumerateW returned `count` valid credential pointers,
        // each with a null-terminated target name, alive until CredFree.
        let credential = unsafe { &**credentials.add(index) };
        if credential.Type != CRED_TYPE_GENERIC || credential.TargetName.is_null() {
            continue;
        }
        let name = unsafe {
            let length = (0..).take_while(|&i| *credential.TargetName.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(credential.TargetName, length))
        };
        if let Some(key) = name.strip_suffix(&suffix).filter(|key| !key.is_empty()) {
            keys.push(key.to_owned());
        }
    }
    // SAFETY: the list came from CredEnumerateW and is not used after this.
    unsafe { CredFree(credentials.cast()) };
    keys.sort();
    Ok(keys)
}

#[cfg(not(windows))]
pub fn saved_keys(_service: &str) -> Result<Vec<String>, String> {
    Err("Listing saved passwords is not supported on this platform yet".into())
}

/// Deletes everything [`saved_keys`] finds, and says how many went. Carries on
/// past a failure so that one stuck entry does not keep the rest.
pub fn remove_all(service: &str) -> Result<usize, String> {
    let store = OsStore::new(service);
    let mut removed = 0;
    let mut failed = None;
    for key in saved_keys(service)? {
        match store.delete(&key) {
            Ok(()) => removed += 1,
            Err(error) => failed = Some(format!("Could not remove {key}: {error}")),
        }
    }
    failed.map_or(Ok(removed), Err)
}

/// Lives as long as the process. What tests use, and what stands in for a
/// platform store that is not there.
#[derive(Default)]
pub struct MemoryStore {
    secrets: RefCell<HashMap<String, String>>,
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.secrets.borrow().get(key).cloned())
    }

    fn set(&self, key: &str, secret: &str) -> Result<(), StoreError> {
        self.secrets
            .borrow_mut()
            .insert(key.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.secrets.borrow_mut().remove(key);
        Ok(())
    }
}

/// The store, and what has been read from it this run.
///
/// Every answer is cached, misses included, so the store is asked about a key
/// at most once per run. That matters because the answer is wanted every
/// frame the details panel is up, and the first question can block while a
/// keyring asks to be unlocked.
///
/// No `Debug`: the cache holds secrets.
pub struct Vault {
    store: Box<dyn SecretStore>,
    persistent: bool,
    cache: RefCell<HashMap<String, Option<String>>>,
}

impl Vault {
    /// The platform store, or a session-only vault when there is none. The
    /// first read doubles as the probe, and its answer is kept.
    pub fn open(service: impl Into<String>) -> Self {
        let store = OsStore::new(service);
        match store.get(DEFAULT_PASSWORD) {
            Ok(default_password) => {
                let vault = Self::with_store(Box::new(store), true);
                vault
                    .cache
                    .borrow_mut()
                    .insert(DEFAULT_PASSWORD.to_owned(), default_password);
                vault
            }
            Err(_) => Self::session_only(),
        }
    }

    /// Kept for this run and forgotten at exit, because there is nowhere safe
    /// to keep it longer.
    pub fn session_only() -> Self {
        Self::with_store(Box::<MemoryStore>::default(), false)
    }

    pub fn with_store(store: Box<dyn SecretStore>, persistent: bool) -> Self {
        Self {
            store,
            persistent,
            cache: RefCell::default(),
        }
    }

    /// Whether what is saved here outlasts the process.
    pub fn is_persistent(&self) -> bool {
        self.persistent
    }

    /// Where saved secrets go, as the interface names it.
    pub fn store_name(&self) -> &'static str {
        if !self.persistent {
            "memory for this session"
        } else if cfg!(windows) {
            "Windows Credential Manager"
        } else {
            "the system keyring"
        }
    }

    /// A store that fails to answer is taken to have nothing, rather than
    /// being asked again every frame.
    pub fn get(&self, key: &str) -> Option<String> {
        if let Some(cached) = self.cache.borrow().get(key) {
            return cached.clone();
        }
        let secret = self.store.get(key).ok().flatten();
        self.cache
            .borrow_mut()
            .insert(key.to_owned(), secret.clone());
        secret
    }

    /// Kept for this run whether or not the store took it, so a failed save
    /// still lets the next connection use what was typed.
    pub fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        self.cache
            .borrow_mut()
            .insert(key.to_owned(), Some(secret.to_owned()));
        self.store
            .set(key, secret)
            .map_err(|error| format!("Could not save to {}: {error}", self.store_name()))
    }

    pub fn delete(&self, key: &str) -> Result<(), String> {
        self.cache.borrow_mut().insert(key.to_owned(), None);
        self.store
            .delete(key)
            .map_err(|error| format!("Could not remove from {}: {error}", self.store_name()))
    }

    /// Stops using a saved secret for the rest of this run without deleting
    /// it: a password the device has just refused.
    pub fn suppress(&self, key: &str) {
        self.cache.borrow_mut().insert(key.to_owned(), None);
    }

    /// A device's secret. One found only under its host-and-port key is moved
    /// to its MAC key on the way, once; if the move cannot be saved it is
    /// still returned, and left where it was.
    pub fn get_device(&self, key: &DeviceKey) -> Option<String> {
        if let Some(secret) = self.get(&key.key) {
            return Some(secret);
        }
        let legacy = key.legacy.as_deref()?;
        let secret = self.get(legacy)?;
        if self.set(&key.key, &secret).is_ok() {
            let _ = self.delete(legacy);
        }
        Some(secret)
    }

    pub fn contains_device(&self, key: &DeviceKey) -> bool {
        self.get_device(key).is_some()
    }

    /// Saves under the MAC key, and clears the host-and-port key so that an
    /// older secret cannot come back from there.
    pub fn set_device(&self, key: &DeviceKey, secret: &str) -> Result<(), String> {
        self.set(&key.key, secret)?;
        match &key.legacy {
            Some(legacy) => self.delete(legacy),
            None => Ok(()),
        }
    }

    /// Forgets the secret under both keys.
    pub fn delete_device(&self, key: &DeviceKey) -> Result<(), String> {
        let deleted = self.delete(&key.key);
        match &key.legacy {
            Some(legacy) => deleted.and(self.delete(legacy)),
            None => deleted,
        }
    }

    pub fn suppress_device(&self, key: &DeviceKey) {
        self.suppress(&key.key);
        if let Some(legacy) = &key.legacy {
            self.suppress(legacy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts reads so the cache can be seen to work, and can be told to
    /// fail every write.
    #[derive(Default)]
    struct CountingStore {
        inner: MemoryStore,
        reads: std::rc::Rc<std::cell::Cell<usize>>,
        refuse_writes: bool,
    }

    impl SecretStore for CountingStore {
        fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
            self.reads.set(self.reads.get() + 1);
            self.inner.get(key)
        }
        fn set(&self, key: &str, secret: &str) -> Result<(), StoreError> {
            if self.refuse_writes {
                return Err(StoreError::Unavailable("locked".into()));
            }
            self.inner.set(key, secret)
        }
        fn delete(&self, key: &str) -> Result<(), StoreError> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn keys_name_the_mac_whatever_its_spelling_and_the_endpoint_without_one() {
        for mac in ["00:10:7F:11:22:33", "00-10-7f-11-22-33", "00107f112233"] {
            let key = DeviceKey::ssh_password(mac, "192.0.2.1", 22);
            assert_eq!(key.key(), "ssh:mac:00107f112233");
            assert_eq!(key.legacy.as_deref(), Some("ssh:192.0.2.1:22"));
        }
        assert_eq!(
            DeviceKey::vc4_token("00:10:7f:11:22:33", "VC4.example", 443).key(),
            "vc4:mac:00107f112233"
        );
        for mac in ["", "00:00:00:00:00:00", "not a mac", "00:10:7f:11:22"] {
            let key = DeviceKey::ssh_password(mac, " Room.Example ", 22);
            assert_eq!(key.key(), "ssh:room.example:22", "{mac}");
            assert_eq!(key.legacy, None);
        }
    }

    /// A new address changes nothing for a device known by its MAC.
    #[test]
    fn a_device_keeps_its_secret_when_its_address_changes() {
        let vault = Vault::with_store(Box::<MemoryStore>::default(), true);
        let before = DeviceKey::ssh_password("00:10:7f:11:22:33", "192.0.2.1", 22);
        vault.set_device(&before, "secret").unwrap();

        let after = DeviceKey::ssh_password("00:10:7f:11:22:33", "192.0.2.99", 22);
        assert_eq!(vault.get_device(&after).as_deref(), Some("secret"));
        let by_name = DeviceKey::ssh_password("00:10:7f:11:22:33", "rmc4.local", 22);
        assert_eq!(vault.get_device(&by_name).as_deref(), Some("secret"));
    }

    /// Saved before the MAC was known, or by an earlier build: found under
    /// the endpoint once, and moved to the MAC for good.
    #[test]
    fn a_secret_saved_by_address_moves_to_the_mac_key() {
        let store = MemoryStore::default();
        store.set("ssh:192.0.2.1:22", "secret").unwrap();
        let vault = Vault::with_store(Box::new(store), true);
        let key = DeviceKey::ssh_password("00:10:7f:11:22:33", "192.0.2.1", 22);

        assert_eq!(vault.get_device(&key).as_deref(), Some("secret"));
        assert_eq!(
            vault.store.get("ssh:mac:00107f112233").unwrap().as_deref(),
            Some("secret")
        );
        assert_eq!(vault.store.get("ssh:192.0.2.1:22").unwrap(), None);
    }

    #[test]
    fn forgetting_clears_both_keys_and_saving_clears_the_old_one() {
        let store = MemoryStore::default();
        store.set("ssh:192.0.2.1:22", "old").unwrap();
        let vault = Vault::with_store(Box::new(store), true);
        let key = DeviceKey::ssh_password("00:10:7f:11:22:33", "192.0.2.1", 22);

        vault.set_device(&key, "new").unwrap();
        assert_eq!(vault.store.get("ssh:192.0.2.1:22").unwrap(), None);

        vault.store.set("ssh:192.0.2.1:22", "old").unwrap();
        vault.delete_device(&key).unwrap();
        assert!(!vault.contains_device(&key));
        assert_eq!(vault.store.get("ssh:192.0.2.1:22").unwrap(), None);
    }

    #[test]
    fn the_store_is_asked_about_a_key_once_including_a_miss() {
        let store = CountingStore::default();
        let reads = store.reads.clone();
        let vault = Vault::with_store(Box::new(store), true);

        assert_eq!(vault.get("ssh:a:22"), None);
        assert_eq!(vault.get("ssh:a:22"), None);
        assert_eq!(reads.get(), 1);

        vault.set("ssh:a:22", "secret").unwrap();
        assert_eq!(vault.get("ssh:a:22").as_deref(), Some("secret"));
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn a_refused_write_is_reported_but_kept_for_the_session() {
        let store = CountingStore {
            refuse_writes: true,
            ..Default::default()
        };
        let vault = Vault::with_store(Box::new(store), true);

        let error = vault.set(DEFAULT_PASSWORD, "secret").unwrap_err();
        assert!(!error.contains("secret"), "{error}");
        assert_eq!(vault.get(DEFAULT_PASSWORD).as_deref(), Some("secret"));
    }

    #[test]
    fn suppressing_hides_a_secret_for_the_run_without_deleting_it() {
        let store = MemoryStore::default();
        store.set("ssh:a:22", "stale").unwrap();
        let vault = Vault::with_store(Box::new(store), true);
        assert!(vault.get("ssh:a:22").is_some());

        vault.suppress("ssh:a:22");
        assert!(vault.get("ssh:a:22").is_none());
        assert_eq!(
            vault.store.get("ssh:a:22").unwrap().as_deref(),
            Some("stale")
        );
    }

    #[test]
    fn a_session_only_vault_says_so() {
        let vault = Vault::session_only();
        assert!(!vault.is_persistent());
        vault.set("ssh:a:22", "secret").unwrap();
        assert_eq!(vault.get("ssh:a:22").as_deref(), Some("secret"));
    }

    /// Writes to, reads from and deletes from the real platform store.
    #[test]
    #[ignore = "Touches the real Windows Credential Manager or Secret Service"]
    fn the_platform_store_round_trips_a_secret() {
        let store = OsStore::new("CrestronLoadRunner live test");
        let key = "live-test";
        store.set(key, "synthetic-secret").unwrap();
        assert_eq!(store.get(key).unwrap().as_deref(), Some("synthetic-secret"));
        store.delete(key).unwrap();
        assert_eq!(store.get(key).unwrap(), None);
        store.delete(key).unwrap();
    }

    /// Lists exactly this service's entries, not a similarly named one's, and
    /// removes them all.
    #[test]
    #[ignore = "Touches the real Windows Credential Manager"]
    fn the_platform_store_lists_and_removes_only_its_own_entries() {
        let service = "CrestronLoadRunner list test";
        let neighbour = format!("{service} (profile)");
        let store = OsStore::new(service);
        let other = OsStore::new(neighbour.as_str());
        store.set("ssh:mac:00107f112233", "one").unwrap();
        store.set("vc4:192.0.2.1:22", "two").unwrap();
        other.set("ssh:mac:00107f112233", "kept").unwrap();

        assert_eq!(
            saved_keys(service).unwrap(),
            ["ssh:mac:00107f112233", "vc4:192.0.2.1:22"]
        );
        assert_eq!(remove_all(service).unwrap(), 2);
        assert!(saved_keys(service).unwrap().is_empty());
        assert_eq!(saved_keys(&neighbour).unwrap(), ["ssh:mac:00107f112233"]);
        other.delete("ssh:mac:00107f112233").unwrap();
    }
}
