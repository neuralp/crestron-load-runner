//! What the uninstaller shows before the program is removed: an offer to
//! remove, one kind at a time, what the program saved outside its install
//! folder. Everything is kept unless it is ticked, so a later install picks up
//! where this one left off.
//!
//! It is the program rather than the installer that asks, because only the
//! program knows where it keeps things, and an uninstall started from
//! Settings shows no installer dialogs of its own.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use eframe::egui;

/// Where each kind of saved data lives.
pub struct DataLocations {
    pub preferences: Option<PathBuf>,
    pub scripts: Option<PathBuf>,
    pub firmware: Option<PathBuf>,
    /// The name the vault keeps this profile's secrets under.
    pub vault_service: String,
    /// Folders that are removed afterwards if, and only if, they are empty.
    /// Innermost first, so a parent emptied by removing its child goes too.
    pub prune: Vec<PathBuf>,
}

impl DataLocations {
    pub fn from_storage() -> Self {
        let config = crate::storage::config_dir();
        let firmware = crate::storage::firmware_dir();
        // The folder itself, the application's, and the organisation's. A
        // `--config-dir` profile is a folder the user chose, so nothing above
        // it is touched, empty or not.
        let levels = if crate::storage::config_dir_overridden() { 1 } else { 3 };
        let mut prune: Vec<PathBuf> = [config.as_deref(), firmware.as_deref().and_then(Path::parent)]
            .into_iter()
            .flatten()
            .flat_map(|dir| dir.ancestors().take(levels).map(Path::to_path_buf).collect::<Vec<_>>())
            .collect();
        prune.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
        prune.dedup();
        Self {
            preferences: crate::storage::preferences_path(),
            scripts: config.map(|dir| dir.join("scripts.json")),
            firmware,
            vault_service: crate::storage::vault_service(),
            prune,
        }
    }
}

/// What is there to remove, looked at once when the window opens.
pub struct Inventory {
    pub preferences: bool,
    pub scripts: bool,
    /// The library's size, when there is one.
    pub firmware: Option<u64>,
    /// How many passwords and tokens are saved, or why that is not known.
    pub vault: Result<usize, String>,
}

impl Inventory {
    pub fn of(locations: &DataLocations, vault: Result<usize, String>) -> Self {
        let is_file = |path: &Option<PathBuf>| path.as_deref().is_some_and(Path::is_file);
        Self {
            preferences: is_file(&locations.preferences),
            scripts: is_file(&locations.scripts),
            firmware: locations
                .firmware
                .as_deref()
                .filter(|dir| dir.is_dir())
                .map(directory_size),
            vault,
        }
    }
}

fn directory_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => directory_size(&entry.path()),
            Ok(_) => entry.metadata().map_or(0, |metadata| metadata.len()),
            Err(_) => 0,
        })
        .sum()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Choices {
    pub preferences: bool,
    pub firmware: bool,
    pub scripts: bool,
    pub vault: bool,
}

impl Choices {
    fn any(self) -> bool {
        self.preferences || self.firmware || self.scripts || self.vault
    }
}

/// Removes what was ticked, and says what could not be. `remove_vault` is
/// what clears the passwords, so tests can stand in for the real store.
pub fn remove(
    locations: &DataLocations,
    choices: Choices,
    remove_vault: impl FnOnce(&str) -> Result<usize, String>,
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut note = |what: &str, path: &Path, result: io::Result<()>| match result {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            failures.push(format!("{what} ({}): {error}", path.display()));
        }
        _ => {}
    };
    // Each file goes with the temporary copy an interrupted save leaves.
    for (chosen, path, what) in [
        (choices.preferences, &locations.preferences, "Preferences"),
        (choices.scripts, &locations.scripts, "Scripts"),
    ] {
        if let (true, Some(path)) = (chosen, path) {
            note(what, path, fs::remove_file(path));
            let temporary = path.with_extension("json.tmp");
            note(what, &temporary, fs::remove_file(&temporary));
        }
    }
    if let (true, Some(dir)) = (choices.firmware, &locations.firmware) {
        note("Firmware library", dir, fs::remove_dir_all(dir));
    }
    if choices.vault
        && let Err(error) = remove_vault(&locations.vault_service)
    {
        failures.push(format!("Saved passwords and API tokens: {error}"));
    }
    for dir in &locations.prune {
        // Refuses a folder that is not empty, which is the point.
        let _ = fs::remove_dir(dir);
    }
    failures
}

/// The window. Runs until it is closed, and never fails the uninstall that
/// opened it.
pub fn run(icon: egui::IconData) -> eframe::Result {
    let locations = DataLocations::from_storage();
    let vault = crate::vault::saved_keys(&locations.vault_service).map(|keys| keys.len());
    let inventory = Inventory::of(&locations, vault);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Remove Crestron Load Runner data?")
            .with_inner_size([560.0, 420.0])
            .with_resizable(false)
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "Remove Crestron Load Runner data",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(RemoveData {
                locations,
                inventory,
                choices: Choices::default(),
                failures: None,
            }))
        }),
    )
}

struct RemoveData {
    locations: DataLocations,
    inventory: Inventory,
    choices: Choices,
    /// Set once removal has run and something could not be removed.
    failures: Option<Vec<String>>,
}

impl eframe::App for RemoveData {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            if self.show(ui) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

impl RemoveData {
    /// Draws the window, and answers whether it should close.
    fn show(&mut self, ui: &mut egui::Ui) -> bool {
        ui.heading("Remove saved data too?");
        ui.label(
            "Crestron Load Runner is being uninstalled. Tick anything you also want removed. \
             Whatever is left unticked is kept, so installing again later picks up where you left off.",
        );
        ui.add_space(10.0);

        if let Some(failures) = &self.failures {
            ui.label(egui::RichText::new("Some data could not be removed:").strong());
            for failure in failures {
                ui.label(egui::RichText::new(failure).color(ui.visuals().error_fg_color));
            }
            ui.add_space(10.0);
            return ui.button("Close").clicked();
        }

        let path = |path: &Option<PathBuf>| {
            path.as_deref()
                .map_or_else(String::new, |path| path.display().to_string())
        };
        choice(
            ui,
            &mut self.choices.preferences,
            self.inventory.preferences,
            "Preferences",
            "Default username, startup address book and recent list",
            &path(&self.locations.preferences),
        );
        choice(
            ui,
            &mut self.choices.firmware,
            self.inventory.firmware.is_some(),
            "Firmware library",
            &match self.inventory.firmware {
                Some(size) => format!("Imported firmware files, {}", crate::archive::human_size(size)),
                None => "Imported firmware files".to_owned(),
            },
            &path(&self.locations.firmware),
        );
        choice(
            ui,
            &mut self.choices.scripts,
            self.inventory.scripts,
            "Scripts",
            "The script library",
            &path(&self.locations.scripts),
        );
        let (saved, detail) = match &self.inventory.vault {
            Ok(0) => (false, "Nothing saved".to_owned()),
            Ok(1) => (true, "1 entry in Windows Credential Manager".to_owned()),
            Ok(count) => (true, format!("{count} entries in Windows Credential Manager")),
            Err(error) => (false, error.clone()),
        };
        choice(
            ui,
            &mut self.choices.vault,
            saved,
            "Saved passwords and API tokens",
            "Default and device passwords, and VC-4 tokens",
            &detail,
        );

        ui.add_space(12.0);
        ui.separator();
        let mut close = false;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.choices.any(), egui::Button::new("Remove selected"))
                .clicked()
            {
                let failures = remove(&self.locations, self.choices, crate::vault::remove_all);
                if failures.is_empty() {
                    close = true;
                } else {
                    self.failures = Some(failures);
                }
            }
            if ui.button("Keep everything").clicked() {
                close = true;
            }
        });
        close
    }
}

/// One tickable kind of data. Disabled, and marked so, when there is none.
fn choice(ui: &mut egui::Ui, chosen: &mut bool, present: bool, title: &str, what: &str, detail: &str) {
    if !present {
        *chosen = false;
    }
    let label = if present {
        title.to_owned()
    } else {
        format!("{title} (nothing saved)")
    };
    ui.add_enabled(present, egui::Checkbox::new(chosen, label));
    ui.indent(title, |ui| {
        ui.small(what);
        if !detail.is_empty() {
            ui.weak(detail);
        }
    });
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    /// A profile laid out the way `storage` lays one out, in a throwaway
    /// directory: roaming config and local data under one organisation.
    fn profile(dir: &TestDir) -> DataLocations {
        let organisation = dir.path().join("WorldDomination");
        let config = organisation.join("CrestronLoadRunner").join("config");
        let data = organisation.join("CrestronLoadRunner").join("data");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(data.join("firmware").join("RMC4")).unwrap();
        fs::write(config.join("preferences.json"), b"{}").unwrap();
        fs::write(config.join("preferences.json.tmp"), b"{}").unwrap();
        fs::write(config.join("scripts.json"), b"[]").unwrap();
        fs::write(data.join("firmware").join("RMC4").join("rmc4.puf"), [0u8; 2048]).unwrap();
        DataLocations {
            preferences: Some(config.join("preferences.json")),
            scripts: Some(config.join("scripts.json")),
            firmware: Some(data.join("firmware")),
            vault_service: "test".into(),
            prune: vec![
                config.clone(),
                data.clone(),
                organisation.join("CrestronLoadRunner"),
                organisation.clone(),
            ],
        }
    }

    #[test]
    fn the_inventory_says_what_is_there() {
        let dir = TestDir::new();
        let locations = profile(&dir);
        let inventory = Inventory::of(&locations, Ok(3));
        assert!(inventory.preferences);
        assert!(inventory.scripts);
        assert_eq!(inventory.firmware, Some(2048));

        fs::remove_file(locations.scripts.as_ref().unwrap()).unwrap();
        assert!(!Inventory::of(&locations, Ok(0)).scripts);
    }

    #[test]
    fn only_what_is_ticked_is_removed_and_occupied_folders_stay() {
        let dir = TestDir::new();
        let locations = profile(&dir);
        let mut vault_cleared = false;

        let failures = remove(
            &locations,
            Choices {
                scripts: true,
                ..Default::default()
            },
            |_| {
                vault_cleared = true;
                Ok(0)
            },
        );

        assert!(failures.is_empty(), "{failures:?}");
        assert!(!locations.scripts.as_ref().unwrap().exists());
        assert!(locations.preferences.as_ref().unwrap().exists());
        assert!(locations.firmware.as_ref().unwrap().exists());
        assert!(!vault_cleared, "passwords were not ticked");
        assert!(locations.prune[3].exists(), "a folder still in use was removed");
    }

    #[test]
    fn removing_everything_leaves_no_empty_folders_behind() {
        let dir = TestDir::new();
        let locations = profile(&dir);
        let mut service = None;

        let failures = remove(
            &locations,
            Choices {
                preferences: true,
                firmware: true,
                scripts: true,
                vault: true,
            },
            |name| {
                service = Some(name.to_owned());
                Ok(2)
            },
        );

        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(service.as_deref(), Some("test"));
        assert!(!dir.path().join("WorldDomination").exists());
        assert!(dir.path().exists(), "pruning went above the organisation");
    }

    /// Something else kept in the organisation's folder keeps the folder.
    #[test]
    fn a_folder_holding_something_else_is_not_removed() {
        let dir = TestDir::new();
        let locations = profile(&dir);
        let unrelated = dir.path().join("WorldDomination").join("OtherApp");
        fs::create_dir_all(&unrelated).unwrap();

        let all = Choices {
            preferences: true,
            firmware: true,
            scripts: true,
            vault: false,
        };
        assert!(remove(&locations, all, |_| Ok(0)).is_empty());

        assert!(unrelated.exists());
        assert!(!dir.path().join("WorldDomination").join("CrestronLoadRunner").exists());
    }

    #[test]
    fn a_vault_failure_is_reported_and_the_rest_still_goes() {
        let dir = TestDir::new();
        let locations = profile(&dir);
        let failures = remove(
            &locations,
            Choices {
                scripts: true,
                vault: true,
                ..Default::default()
            },
            |_| Err("locked".into()),
        );
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("locked"));
        assert!(!locations.scripts.as_ref().unwrap().exists());
    }

    #[test]
    fn the_window_starts_with_nothing_ticked_and_disables_what_is_missing() {
        let dir = TestDir::new();
        let locations = profile(&dir);
        fs::remove_file(locations.scripts.as_ref().unwrap()).unwrap();
        let inventory = Inventory::of(&locations, Ok(0));
        let mut window = RemoveData {
            locations,
            inventory,
            choices: Choices::default(),
            failures: None,
        };
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            window.show(ui);
        });
        let texts: Vec<String> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect();
        output.drop_without_applying_deltas();

        assert_eq!(window.choices, Choices::default());
        for expected in [
            "Preferences",
            "Firmware library",
            "Scripts (nothing saved)",
            "Saved passwords and API tokens (nothing saved)",
            "Keep everything",
        ] {
            assert!(texts.iter().any(|text| text == expected), "missing {expected}");
        }
    }
}
