use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::model::{AddressEntry, endpoint_id};

/// What the application opens when it starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupBook {
    #[default]
    Empty,
    MostRecent,
    Specific,
}

/// The address book is a file the user names and owns; this is everything else.
/// `deny_unknown_fields` matches the rest of this module: an older build
/// discards a newer build's preferences rather than half-reading them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    #[serde(default)]
    pub default_username: String,
    #[serde(default)]
    pub default_password: String,
    #[serde(default)]
    pub startup: StartupBook,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_address_book: Option<PathBuf>,
    #[serde(default)]
    pub recent_address_books: Vec<PathBuf>,
}

impl Preferences {
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = preferences_path() else {
            return (
                Self::default(),
                Some("Could not locate the user configuration directory".into()),
            );
        };
        if !path.exists() {
            return (Self::default(), None);
        }
        match Self::load_from(&path) {
            Ok(preferences) => (preferences, None),
            Err(error) => (
                Self::default(),
                Some(format!("Could not read {}: {error}", path.display())),
            ),
        }
    }

    pub fn load_from(path: &Path) -> io::Result<Self> {
        serde_json::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)
    }

    /// Path-taking because the configuration directory is a process-global set
    /// once at startup, which leaves a test no way to redirect a save.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, data)?;
        fs::rename(temporary, path)
    }
}

pub const RECENT_ADDRESS_BOOKS: usize = 5;

/// Moves `path` to the front, dropping any entry that names the same file, and
/// keeps at most [`RECENT_ADDRESS_BOOKS`]. Reports whether the list changed, so
/// reopening the same book at every startup does not rewrite the preferences.
pub fn remember_recent(recent: &mut Vec<PathBuf>, path: &Path) -> bool {
    let updated: Vec<PathBuf> = std::iter::once(path.to_path_buf())
        .chain(
            recent
                .iter()
                .filter(|existing| !same_file(existing, path))
                .cloned(),
        )
        .take(RECENT_ADDRESS_BOOKS)
        .collect();
    let changed = updated != *recent;
    *recent = updated;
    changed
}

pub fn forget_recent(recent: &mut Vec<PathBuf>, path: &Path) -> bool {
    let before = recent.len();
    recent.retain(|existing| !same_file(existing, path));
    recent.len() != before
}

/// One file can be named by a relative path, an absolute one, or a symbolic
/// link. Recent entries are stored as the user chose them, so the comparison
/// resolves both sides rather than the stored value.
fn same_file(a: &Path, b: &Path) -> bool {
    match (resolved_path(a), resolved_path(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The address book to open at startup, and why nothing is opened when the
/// preference names a file that is gone.
pub fn startup_address_book(preferences: &Preferences) -> (Option<PathBuf>, Option<String>) {
    match preferences.startup {
        StartupBook::Empty => (None, None),
        StartupBook::MostRecent => (
            preferences
                .recent_address_books
                .iter()
                .find(|path| path.is_file())
                .cloned(),
            None,
        ),
        StartupBook::Specific => match &preferences.default_address_book {
            Some(path) if path.is_file() => (Some(path.clone()), None),
            // The preference is left set: the file may sit on a share that is
            // offline rather than having been deleted.
            Some(path) => (
                None,
                Some(format!(
                    "Default address book is missing: {}",
                    path.display()
                )),
            ),
            None => (None, Some("No default address book has been chosen".into())),
        },
    }
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

pub fn validate_portable_path(path: &Path) -> io::Result<()> {
    let preferences =
        preferences_path().ok_or_else(|| io::Error::other("no configuration directory"))?;
    ensure_separate_path(path, &preferences)?;
    ensure_separate_path(path, &preferences.with_file_name("scripts.json"))?;
    ensure_separate_path(path, &preferences.with_file_name("scripts.json.tmp"))?;
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
            "choose a JSON file separate from application-owned preferences and script files",
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

static CONFIG_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Redirects every stored file at startup, so a throwaway profile cannot touch
/// the real preferences. Later calls are refused: paths are read from here for
/// the rest of the process, and moving them mid-run would split the two halves
/// of a save across directories.
pub fn set_config_dir(dir: PathBuf) -> Result<(), PathBuf> {
    CONFIG_DIR_OVERRIDE.set(dir)
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "WorldDomination", "CrestronLoadRunner")
}

pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = CONFIG_DIR_OVERRIDE.get() {
        return Some(dir.clone());
    }
    project_dirs().map(|dirs| dirs.config_dir().to_path_buf())
}

pub fn preferences_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("preferences.json"))
}

/// Whole firmware images, which have no business in a roaming profile: they are
/// large, they are reproducible from the vendor, and copying them between
/// machines at every sign-in is a cost with no benefit. `--config-dir` still
/// gathers everything in one place, so a throwaway profile cannot reach the
/// real library.
pub fn firmware_dir() -> Option<PathBuf> {
    Some(choose_firmware_dir(
        CONFIG_DIR_OVERRIDE.get().cloned(),
        local_firmware_dir()?,
        roaming_firmware_dir()?,
        Path::exists,
    ))
}

fn choose_firmware_dir(
    override_dir: Option<PathBuf>,
    local: PathBuf,
    roaming: PathBuf,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.join("firmware");
    }
    // A library that an earlier build left in the roaming profile, and that
    // could not be moved, is read where it stands rather than abandoned.
    if !exists(&local) && exists(&roaming) {
        roaming
    } else {
        local
    }
}

fn local_firmware_dir() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.data_local_dir().join("firmware"))
}

/// Where earlier builds kept the firmware library, beside the preferences.
fn roaming_firmware_dir() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.config_dir().join("firmware"))
}

/// Moves a firmware library that an earlier build left in the roaming profile.
/// Called once at startup; returns what to tell the user when it could not.
pub fn migrate_firmware_dir() -> Option<String> {
    if CONFIG_DIR_OVERRIDE.get().is_some() {
        return None;
    }
    migrate_firmware(&roaming_firmware_dir()?, &local_firmware_dir()?)
}

/// Renaming is instant within a volume, which is where both directories sit
/// unless the profile is redirected. Where it is, the move is left to the user
/// rather than copying gigabytes during startup, and the old directory goes on
/// being used until they do it.
fn migrate_firmware(roaming: &Path, local: &Path) -> Option<String> {
    if !roaming.is_dir() || local.exists() {
        return None;
    }
    let renamed = local
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::rename(roaming, local));
    renamed.err().map(|error| {
        format!(
            "The firmware library is still in {} and could not be moved to {}: {error}.              Move that directory yourself to finish; until then it is used where it is.",
            roaming.display(),
            local.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeviceKind;
    use crate::test_support::TestDir;

    #[test]
    fn portable_books_cannot_overwrite_script_storage() {
        let preferences = preferences_path().unwrap();
        assert!(validate_portable_path(&preferences.with_file_name("scripts.json")).is_err());
        assert!(validate_portable_path(&preferences.with_file_name("scripts.json.tmp")).is_err());
    }

    #[test]
    fn preferences_and_the_script_library_stay_together() {
        // --config-dir has to move the stored files, not just the preferences.
        let dir = config_dir().expect("a configuration directory");
        assert_eq!(preferences_path().unwrap(), dir.join("preferences.json"));
    }

    #[test]
    fn the_firmware_library_sits_outside_the_roaming_profile() {
        let local = PathBuf::from("/local/data/firmware");
        let roaming = PathBuf::from("/roaming/config/firmware");
        let chosen = |present: &[&Path]| {
            choose_firmware_dir(None, local.clone(), roaming.clone(), |path| {
                present.contains(&path)
            })
        };
        // A new profile, and one already moved, both use the local directory.
        assert_eq!(chosen(&[]), local);
        assert_eq!(chosen(&[&local]), local);
        // A library an earlier build left behind is read where it stands.
        assert_eq!(chosen(&[&roaming]), roaming);
        // With both there the moved one is the library and the leftover is not.
        assert_eq!(chosen(&[&local, &roaming]), local);
        // A throwaway profile still gathers everything in one directory.
        assert_eq!(
            choose_firmware_dir(Some("/profile".into()), local, roaming, |_| true),
            Path::new("/profile/firmware")
        );
    }

    #[test]
    fn migrating_the_firmware_library_moves_it_once_and_overwrites_nothing() {
        let dir = TestDir::new();
        let roaming = dir.path().join("roaming/config/firmware");
        let local = dir.path().join("local/data/firmware");

        // Nothing to move, and nothing created for the sake of it.
        assert!(migrate_firmware(&roaming, &local).is_none());
        assert!(!local.exists());

        fs::create_dir_all(&roaming).unwrap();
        fs::write(roaming.join("catalog.json"), b"moved").unwrap();
        assert!(migrate_firmware(&roaming, &local).is_none());
        assert!(!roaming.exists(), "the old directory is left behind");
        assert_eq!(fs::read(local.join("catalog.json")).unwrap(), b"moved");

        // A library that reappears in the old place never overwrites the one
        // already moved.
        fs::create_dir_all(&roaming).unwrap();
        fs::write(roaming.join("catalog.json"), b"older").unwrap();
        assert!(migrate_firmware(&roaming, &local).is_none());
        assert!(roaming.is_dir());
        assert_eq!(fs::read(local.join("catalog.json")).unwrap(), b"moved");

        // A move that cannot happen is reported, and loses nothing.
        let blocked = dir.path().join("blocked");
        fs::write(&blocked, b"a file where a directory would go").unwrap();
        let reported = migrate_firmware(&roaming, &blocked.join("data/firmware")).unwrap();
        assert!(reported.contains("could not be moved"), "{reported}");
        assert_eq!(fs::read(roaming.join("catalog.json")).unwrap(), b"older");
    }

    #[test]
    fn rejects_a_portable_document_as_preferences() {
        let dir = TestDir::new();
        let path = dir.path().join("preferences.json");
        fs::write(&path, r#"{"version":1,"devices":[]}"#).unwrap();
        assert!(Preferences::load_from(&path).is_err());
    }

    #[test]
    fn preferences_written_by_an_older_build_load_with_the_new_fields_empty() {
        let dir = TestDir::new();
        let path = dir.path().join("preferences.json");
        fs::write(&path, r#"{"default_username":"admin"}"#).unwrap();

        let preferences = Preferences::load_from(&path).unwrap();

        assert_eq!(preferences.default_username, "admin");
        assert!(preferences.default_password.is_empty());
        assert_eq!(preferences.startup, StartupBook::Empty);
        assert!(preferences.default_address_book.is_none());
        assert!(preferences.recent_address_books.is_empty());
    }

    #[test]
    fn preferences_round_trip_the_startup_choice_and_the_recent_list() {
        let dir = TestDir::new();
        let path = dir.path().join("preferences.json");
        let preferences = Preferences {
            default_username: "admin".into(),
            default_password: "secret".into(),
            startup: StartupBook::Specific,
            default_address_book: Some(dir.path().join("book.json")),
            recent_address_books: vec![dir.path().join("book.json")],
        };

        preferences.save_to(&path).unwrap();

        assert_eq!(Preferences::load_from(&path).unwrap(), preferences);
        // The address book is a file of its own now.
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.get("address_book").is_none());
    }

    #[test]
    fn the_most_recent_five_address_books_are_kept_most_recent_first() {
        let dir = TestDir::new();
        let mut recent = Vec::new();
        for index in 0..7 {
            let path = dir.path().join(format!("book{index}.json"));
            assert!(remember_recent(&mut recent, &path));
        }

        assert_eq!(recent.len(), RECENT_ADDRESS_BOOKS);
        assert_eq!(recent[0], dir.path().join("book6.json"));
        assert_eq!(recent[4], dir.path().join("book2.json"));
    }

    #[test]
    fn reopening_an_address_book_moves_it_to_the_front_of_the_recent_list() {
        let dir = TestDir::new();
        let first = dir.path().join("first.json");
        let second = dir.path().join("second.json");
        let mut recent = Vec::new();
        remember_recent(&mut recent, &first);
        remember_recent(&mut recent, &second);
        assert_eq!(recent, vec![second.clone(), first.clone()]);

        assert!(remember_recent(&mut recent, &first));
        assert_eq!(recent, vec![first.clone(), second]);
        // Already at the front, so there is nothing to write.
        assert!(!remember_recent(&mut recent, &first));

        assert!(forget_recent(&mut recent, &first));
        assert!(!forget_recent(&mut recent, &first));
    }

    #[test]
    fn recent_entries_naming_the_same_file_by_a_different_path_are_one_entry() {
        let dir = TestDir::new();
        let path = dir.path().join("book.json");
        fs::write(&path, r#"{"version":1,"devices":[]}"#).unwrap();
        let mut recent = vec![path];

        remember_recent(&mut recent, &dir.path().join(".").join("book.json"));

        assert_eq!(recent.len(), 1);
    }

    #[test]
    fn startup_opens_the_default_address_book_only_while_it_exists() {
        let dir = TestDir::new();
        let path = dir.path().join("book.json");
        fs::write(&path, r#"{"version":1,"devices":[]}"#).unwrap();
        let mut preferences = Preferences {
            startup: StartupBook::Specific,
            default_address_book: Some(path.clone()),
            ..Default::default()
        };
        assert_eq!(
            startup_address_book(&preferences),
            (Some(path.clone()), None)
        );

        fs::remove_file(&path).unwrap();
        let (opened, message) = startup_address_book(&preferences);
        assert!(opened.is_none());
        assert!(message.unwrap().contains("missing"));
        assert_eq!(preferences.default_address_book, Some(path));

        preferences.startup = StartupBook::Empty;
        assert_eq!(startup_address_book(&preferences), (None, None));
    }

    #[test]
    fn startup_skips_recent_address_books_that_are_no_longer_there() {
        let dir = TestDir::new();
        let present = dir.path().join("present.json");
        fs::write(&present, r#"{"version":1,"devices":[]}"#).unwrap();
        let preferences = Preferences {
            startup: StartupBook::MostRecent,
            recent_address_books: vec![dir.path().join("missing.json"), present.clone()],
            ..Default::default()
        };

        assert_eq!(startup_address_book(&preferences), (Some(present), None));
    }

    #[test]
    fn prevents_preferences_path_collision_including_aliases() {
        let dir = TestDir::new();
        let path = dir.path().join("preferences.json");
        assert!(ensure_separate_path(&path, &path).is_err());
        Preferences::default().save_to(&path).unwrap();
        let original = fs::read(&path).unwrap();
        assert!(ensure_separate_path(&dir.path().join("./preferences.json"), &path).is_err());
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
