use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

use eframe::egui;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Paths in the catalog are relative to its directory, never to the source file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FirmwareFile {
    original_name: String,
    stored_file: String,
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirmwareAssignment {
    pub local_path: PathBuf,
    pub original_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    version: u32,
    models: BTreeMap<String, Option<FirmwareFile>>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            version: 1,
            models: BTreeMap::new(),
        }
    }
}

impl Catalog {
    fn load(root: &Path) -> io::Result<Self> {
        let data = match fs::read(root.join("catalog.json")) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let catalog: Self = serde_json::from_slice(&data).map_err(io::Error::other)?;
        if catalog.version != 1 {
            return Err(io::Error::other("unsupported firmware catalog version"));
        }
        for (model, assignment) in &catalog.models {
            if model.trim().is_empty() {
                return Err(io::Error::other("empty device model in firmware catalog"));
            }
            if let Some(file) = assignment
                && (file.sha256.len() != 64
                    || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || file.stored_file != format!("{}.firmware", file.sha256))
            {
                return Err(io::Error::other("invalid stored firmware path or checksum"));
            }
        }
        Ok(catalog)
    }

    fn save(&self, root: &Path) -> io::Result<()> {
        fs::create_dir_all(root)?;
        let (temporary, mut file) = temporary_file(root)?;
        let result = (|| {
            file.write_all(&serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, root.join("catalog.json"))?;
            if Self::load(root)? != *self {
                return Err(io::Error::other("firmware catalog verification failed"));
            }
            Ok(())
        })();
        let _ = fs::remove_file(temporary);
        result
    }
}

fn temporary_file(root: &Path) -> io::Result<(PathBuf, File)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let path = root.join(format!(
            ".import-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn stage_for_deletion(root: &Path, stored: &Path) -> io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let directory = root.join(format!(
            ".delete-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&directory) {
            Ok(()) => {
                let staged = directory.join("firmware");
                if let Err(error) = fs::rename(stored, &staged) {
                    let _ = fs::remove_dir(directory);
                    return Err(error);
                }
                return Ok(staged);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn checksum(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Copy and verify before publishing. Content-based names avoid collisions between
/// models, source basenames, and replacements. Existing files are never overwritten.
fn copy_firmware(root: &Path, source: &Path) -> io::Result<FirmwareFile> {
    let mut input = File::open(source)?;
    if !input.metadata()?.is_file() {
        return Err(io::Error::other("select a regular firmware file"));
    }
    let original_name = source
        .file_name()
        .ok_or_else(|| io::Error::other("firmware file has no name"))?
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(root)?;
    let (temporary, mut output) = temporary_file(root)?;
    let result = (|| {
        let mut hash = Sha256::new();
        let mut bytes = 0;
        let mut buffer = [0; 64 * 1024];
        loop {
            let length = input.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            output.write_all(&buffer[..length])?;
            hash.update(&buffer[..length]);
            bytes += length as u64;
        }
        if bytes == 0 {
            return Err(io::Error::other("firmware file is empty"));
        }
        output.sync_all()?;
        drop(output);
        let sha256: String = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if checksum(&temporary)? != sha256 {
            return Err(io::Error::other("firmware copy verification failed"));
        }
        let stored_file = format!("{sha256}.firmware");
        let destination = root.join(&stored_file);
        if destination.exists() {
            if checksum(&destination)? != sha256 {
                return Err(io::Error::other("existing firmware copy is corrupt"));
            }
        } else {
            fs::rename(&temporary, &destination)?;
        }
        Ok(FirmwareFile {
            original_name,
            stored_file,
            bytes,
            sha256,
        })
    })();
    let _ = fs::remove_file(temporary);
    result
}

type ImportResult = Result<(String, FirmwareFile), String>;

/// Starting width of the catalog tree; the divider is draggable from there.
const TREE_WIDTH: f32 = 240.0;

#[derive(Default)]
pub struct FirmwareEditor {
    pub open: bool,
    root: Option<PathBuf>,
    catalog: Catalog,
    observed_models: BTreeSet<String>,
    selected_model: String,
    manual_model: String,
    insight: Option<Insight>,
    pending: Option<Receiver<ImportResult>>,
    error: Option<String>,
    load_failed: bool,
    dirty: bool,
    message: String,
}

impl FirmwareEditor {
    pub fn load(root: Option<PathBuf>) -> Self {
        let mut editor = Self {
            root,
            ..Default::default()
        };
        editor.reload();
        editor
    }

    fn reload(&mut self) {
        let result = self
            .root
            .as_deref()
            .ok_or_else(|| io::Error::other("no application configuration directory"))
            .and_then(Catalog::load);
        match result {
            Ok(catalog) => {
                self.catalog = catalog;
                self.load_failed = false;
                self.error = None;
                for model in self.observed_models.clone() {
                    self.observe_model(&model);
                }
            }
            Err(error) => {
                self.load_failed = true;
                self.error = Some(format!("Could not load firmware catalog: {error}"));
            }
        }
    }

    pub fn observe_model(&mut self, model: &str) {
        let model = model.trim();
        if model.is_empty() {
            return;
        }
        if let Some(existing) = self
            .catalog
            .models
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(model))
            .cloned()
        {
            self.observed_models.insert(existing);
        } else {
            self.observed_models.insert(model.to_owned());
            self.catalog.models.insert(model.to_owned(), None);
            self.dirty = true;
        }
    }

    pub fn assignment_for_model(&self, model: &str) -> Option<FirmwareAssignment> {
        let model = model.trim();
        let (_, assignment) = self
            .catalog
            .models
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(model))?;
        let file = assignment.as_ref()?;
        Some(FirmwareAssignment {
            local_path: self.root.as_ref()?.join(&file.stored_file),
            original_name: file.original_name.clone(),
        })
    }

    pub fn is_busy(&self) -> bool {
        self.pending.is_some()
    }

    fn save_catalog(&mut self, catalog: Catalog) -> io::Result<()> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| io::Error::other("no application configuration directory"))?;
        catalog.save(root)?;
        self.catalog = catalog;
        self.dirty = false;
        Ok(())
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        if self.dirty
            && !self.load_failed
            && self.error.is_none()
            && self.root.is_some()
            && let Err(error) = self.save_catalog(self.catalog.clone())
        {
            self.error = Some(format!("Could not save discovered models: {error}"));
        }
        if let Some(receiver) = &self.pending {
            let result = match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Firmware import worker stopped unexpectedly".into()))
                }
            };
            if let Some(result) = result {
                self.pending = None;
                let result = result.and_then(|(model, file)| {
                    let mut catalog = self.catalog.clone();
                    catalog.models.insert(model.clone(), Some(file));
                    self.save_catalog(catalog)
                        .map_err(|error| error.to_string())?;
                    self.message = format!("Firmware copied and saved for {model}");
                    Ok(())
                });
                if let Err(error) = result {
                    self.error = Some(format!("Could not assign firmware: {error}"));
                }
            }
        }
        if self.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn start_import(&mut self, source: PathBuf) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if self.is_busy()
            || self.load_failed
            || !self.catalog.models.contains_key(&self.selected_model)
        {
            return;
        }
        let model = self.selected_model.clone();
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("firmware-import".into())
            .spawn(move || {
                let result = copy_firmware(&root, &source)
                    .map(|file| (model, file))
                    .map_err(|error| error.to_string());
                let _ = sender.send(result);
            }) {
            Ok(_) => {
                self.pending = Some(receiver);
                self.error = None;
                self.message = "Copying and verifying firmware…".into();
            }
            Err(error) => self.error = Some(format!("Could not start firmware import: {error}")),
        }
    }

    fn add_manual_model(&mut self) {
        let model = self.manual_model.trim();
        if model.is_empty() {
            self.error = Some("Enter a device model before adding it".into());
            return;
        }
        if let Some(existing) = self
            .catalog
            .models
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(model))
            .cloned()
        {
            self.selected_model = existing.clone();
            self.manual_model.clear();
            self.error = None;
            self.message = format!("{existing} is already in the firmware catalog");
            return;
        }

        let model = model.to_owned();
        let mut catalog = self.catalog.clone();
        catalog.models.insert(model.clone(), None);
        match self.save_catalog(catalog) {
            Ok(()) => {
                self.selected_model = model.clone();
                self.manual_model.clear();
                self.error = None;
                self.message = format!("Added device model {model}");
            }
            Err(error) => self.error = Some(format!("Could not add device model: {error}")),
        }
    }

    fn remove_assignment(&mut self, model: &str) -> Result<&'static str, String> {
        let key = self
            .catalog
            .models
            .keys()
            .find(|candidate| candidate.eq_ignore_ascii_case(model.trim()))
            .cloned()
            .ok_or_else(|| "Device model is not in the firmware catalog".to_owned())?;
        let file = self
            .catalog
            .models
            .get(&key)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(|| "No firmware is assigned to this model".to_owned())?;
        let root = self
            .root
            .clone()
            .ok_or_else(|| "No application configuration directory is available".to_owned())?;

        let mut catalog = self.catalog.clone();
        catalog.models.insert(key, None);
        let shared = catalog
            .models
            .values()
            .flatten()
            .any(|assignment| assignment.stored_file == file.stored_file);
        let stored = root.join(&file.stored_file);
        let staged = if shared || !stored.exists() {
            None
        } else {
            Some(stage_for_deletion(&root, &stored).map_err(|error| error.to_string())?)
        };

        if let Err(error) = self.save_catalog(catalog) {
            if let Some(staged) = staged {
                let staging_directory = staged.parent().map(Path::to_path_buf);
                fs::rename(&staged, &stored).map_err(|restore_error| {
                    format!(
                        "Could not save catalog ({error}) and could not restore firmware file ({restore_error})"
                    )
                })?;
                if let Some(directory) = staging_directory {
                    let _ = fs::remove_dir(directory);
                }
            }
            return Err(error.to_string());
        }

        if let Some(staged) = staged {
            let staging_directory = staged.parent().map(Path::to_path_buf);
            fs::remove_file(&staged).map_err(|error| {
                format!(
                    "Assignment was removed, but the firmware file could not be deleted: {error}"
                )
            })?;
            if let Some(directory) = staging_directory {
                let _ = fs::remove_dir(directory);
            }
            Ok("Assignment and stored firmware file removed")
        } else if shared {
            Ok("Assignment removed; stored file retained because another model uses it")
        } else {
            Ok("Assignment removed; stored firmware file was already missing")
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let open = crate::popout::window(ctx, "Firmware Editor", [860.0, 620.0], |ui| {
            self.contents(ui)
        });
        self.open = open;
    }

    /// Tools across the top, the catalog tree on the left, and everything known
    /// about the selected model's firmware on the right.
    fn contents(&mut self, ui: &mut egui::Ui) {
        let busy = self.is_busy();
        egui::Panel::top("firmware_tools").show(ui, |ui| {
            ui.add_space(4.0);
            ui.add_enabled_ui(!busy && !self.load_failed, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Model");
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.manual_model)
                            .hint_text("e.g. RMC4")
                            .desired_width(200.0),
                    );
                    let entered = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    let added = ui
                        .add_enabled(
                            !self.manual_model.trim().is_empty(),
                            egui::Button::new("Add model"),
                        )
                        .clicked();
                    if entered || added {
                        self.add_manual_model();
                    }
                    if busy {
                        ui.spinner();
                    }
                });
            });
            if let Some(error) = self.error.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                    if ui.button("Retry catalog save / load").clicked() {
                        if self.load_failed {
                            self.reload();
                        } else {
                            self.error = None;
                        }
                    }
                });
            }
            ui.add_space(4.0);
        });
        egui::Panel::bottom("firmware_status").show(ui, |ui| {
            ui.add_space(4.0);
            ui.label("Assign one firmware file per discovered or manually entered device model.");
            ui.small(
                "Files are copied into local storage. Use Load Firmware in the main window to install them.",
            );
            if let Some(root) = &self.root {
                ui.small(format!("Storage: {}", root.display()));
            }
            if !self.message.is_empty() {
                ui.label(&self.message);
            }
            ui.add_space(4.0);
        });
        egui::Panel::left("firmware_tree")
            .default_size(TREE_WIDTH)
            .show(ui, |ui| self.tree(ui));
        egui::CentralPanel::default().show(ui, |ui| self.detail(ui));
    }

    /// The catalog as a tree: a branch per firmware file, holding every model
    /// it is assigned to, with unassigned models left at the root.
    fn tree(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("firmware_tree_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.catalog.models.is_empty() {
                    ui.weak("No device models yet — discover devices or add one above.");
                    return;
                }
                let mut branches: BTreeMap<String, Vec<String>> = BTreeMap::new();
                let mut roots = Vec::new();
                for (model, assignment) in &self.catalog.models {
                    match assignment {
                        Some(file) => branches
                            .entry(file.original_name.clone())
                            .or_default()
                            .push(model.clone()),
                        None => roots.push(model.clone()),
                    }
                }
                for (file, models) in &branches {
                    egui::CollapsingHeader::new(file)
                        .id_salt(("firmware_file", file))
                        .default_open(true)
                        .show(ui, |ui| {
                            for model in models {
                                ui.selectable_value(&mut self.selected_model, model.clone(), model);
                            }
                        });
                }
                for model in &roots {
                    ui.selectable_value(&mut self.selected_model, model.clone(), model);
                }
            });
    }

    fn detail(&mut self, ui: &mut egui::Ui) {
        let Some(assignment) = self.catalog.models.get(&self.selected_model).cloned() else {
            self.insight = None;
            ui.weak("Select a model in the tree, or add one above.");
            return;
        };
        ui.heading(&self.selected_model);
        ui.add_enabled_ui(!self.is_busy() && !self.load_failed, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Choose firmware file…").clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_file()
                {
                    self.start_import(path);
                }
                if ui
                    .add_enabled(assignment.is_some(), egui::Button::new("Remove assignment"))
                    .clicked()
                {
                    let model = self.selected_model.clone();
                    match self.remove_assignment(&model) {
                        Ok(message) => {
                            self.error = None;
                            self.message = message.into();
                        }
                        Err(error) => {
                            self.error = Some(format!("Could not remove assignment: {error}"));
                        }
                    }
                }
            });
        });
        ui.separator();
        let Some(file) = assignment else {
            self.insight = None;
            ui.label("No firmware assigned.");
            return;
        };
        let stored = self.root.as_ref().map(|root| root.join(&file.stored_file));
        if self
            .insight
            .as_ref()
            .is_none_or(|held| held.of != file.stored_file)
        {
            self.insight = stored.as_deref().map(Insight::read);
        }
        egui::ScrollArea::vertical()
            .id_salt("firmware_detail")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if stored.is_some_and(|stored| !stored.is_file()) {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        "Stored file is missing. Choose the firmware file again.",
                    );
                }
                let insight = self.insight.as_ref();
                table(
                    ui,
                    "firmware_file_facts",
                    [
                        ("File", file.original_name.clone()),
                        ("Size", crate::archive::human_size(file.bytes)),
                        (
                            "Imported",
                            insight
                                .and_then(|insight| insight.imported.clone())
                                .unwrap_or_else(|| "Unknown".into()),
                        ),
                        ("SHA-256", file.sha256.clone()),
                    ],
                );
                let Some(insight) = insight else {
                    return;
                };
                ui.add_space(8.0);
                match &insight.archive {
                    Err(error) => {
                        ui.strong("Contents");
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    Ok(archive) if archive.package.is_empty() => {
                        // A zip update describes nothing inside, so its own
                        // files are all there is to report.
                        ui.strong("Archive");
                        table(
                            ui,
                            "firmware_archive_facts",
                            [
                                ("Files", archive.entries.to_string()),
                                (
                                    "Newest file",
                                    archive.newest.clone().unwrap_or_else(|| "Unknown".into()),
                                ),
                            ],
                        );
                    }
                    Ok(archive) => {
                        ui.strong("Package");
                        table(
                            ui,
                            "firmware_package",
                            archive
                                .package
                                .iter()
                                .map(|(key, value)| (key.as_str(), value.clone())),
                        );
                    }
                }
            });
    }
}

/// Facts as aligned rows, which is how the package description reads best.
fn table<'a>(ui: &mut egui::Ui, id: &str, rows: impl IntoIterator<Item = (&'a str, String)>) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([16.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            for (name, value) in rows {
                ui.label(name);
                ui.add(egui::Label::new(egui::RichText::new(value).monospace()).wrap());
                ui.end_row();
            }
        });
}

/// What the stored copy of a firmware file says about itself, read once per
/// selection rather than every frame.
struct Insight {
    of: String,
    imported: Option<String>,
    archive: Result<crate::archive::Archive, String>,
}

impl Insight {
    fn read(stored: &Path) -> Self {
        Self {
            of: stored
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            imported: fs::metadata(stored)
                .and_then(|data| data.modified())
                .ok()
                .and_then(crate::archive::utc),
            archive: crate::archive::read(stored),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn imported_file_survives_source_removal_and_catalog_reload() {
        let dir = TestDir::new();
        let root = dir.path().join("firmware");
        let source = dir.path().join("device.puf");
        fs::write(&source, b"test firmware bytes").unwrap();
        let file = copy_firmware(&root, &source).unwrap();
        let mut catalog = Catalog::default();
        catalog.models.insert("RMC4".into(), Some(file.clone()));
        catalog.save(&root).unwrap();
        fs::remove_file(source).unwrap();
        assert_eq!(Catalog::load(&root).unwrap(), catalog);
        assert_eq!(
            fs::read(root.join(file.stored_file)).unwrap(),
            b"test firmware bytes"
        );
    }

    #[test]
    fn same_names_and_unsafe_models_do_not_collide_or_escape_storage() {
        let dir = TestDir::new();
        let root = dir.path().join("firmware");
        let source = dir.path().join("device.puf");
        fs::write(&source, b"first").unwrap();
        let first = copy_firmware(&root, &source).unwrap();
        fs::write(&source, b"second").unwrap();
        let second = copy_firmware(&root, &source).unwrap();
        assert_ne!(first.stored_file, second.stored_file);
        assert_eq!(copy_firmware(&root, &source).unwrap(), second);
        let mut catalog = Catalog::default();
        catalog
            .models
            .insert("../../model".into(), Some(first.clone()));
        catalog
            .models
            .insert("TSW-1070".into(), Some(second.clone()));
        catalog.save(&root).unwrap();
        assert_eq!(Catalog::load(&root).unwrap(), catalog);
        assert_eq!(fs::read(root.join(first.stored_file)).unwrap(), b"first");
        assert_eq!(fs::read(root.join(second.stored_file)).unwrap(), b"second");
    }

    #[test]
    fn invalid_catalog_is_not_overwritten_and_empty_files_are_rejected() {
        let dir = TestDir::new();
        fs::write(dir.path().join("catalog.json"), "invalid JSON").unwrap();
        let mut editor = FirmwareEditor::load(Some(dir.path().into()));
        editor.observe_model("RMC4");
        editor.poll(&egui::Context::default());
        assert!(editor.load_failed);
        assert_eq!(
            fs::read_to_string(dir.path().join("catalog.json")).unwrap(),
            "invalid JSON"
        );
        let empty = dir.path().join("empty.puf");
        fs::write(&empty, []).unwrap();
        assert!(copy_firmware(dir.path(), &empty).is_err());
        assert!(copy_firmware(dir.path(), &dir.path().join("missing.puf")).is_err());
    }

    #[test]
    fn discovery_models_and_background_assignment_persist() {
        let dir = TestDir::new();
        let root = dir.path().join("firmware");
        let source = dir.path().join("device.puf");
        fs::write(&source, b"test firmware").unwrap();
        let mut editor = FirmwareEditor::load(Some(root.clone()));
        editor.observe_model("");
        editor.observe_model("RMC4");
        editor.observe_model("RMC4");
        editor.selected_model = "RMC4".into();
        editor.start_import(source);
        editor.observe_model("TSW-1070");
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while editor.is_busy() {
            editor.poll(&ctx);
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(editor.error.is_none());
        let assignment = editor.assignment_for_model("rmc4").unwrap();
        assert_eq!(assignment.original_name, "device.puf");
        assert!(assignment.local_path.is_file());
        let loaded = Catalog::load(&root).unwrap();
        assert_eq!(loaded.models.len(), 2);
        assert!(loaded.models["RMC4"].is_some());
        assert!(loaded.models["TSW-1070"].is_none());
    }

    #[test]
    fn manually_entered_models_persist_and_match_case_insensitively() {
        let dir = TestDir::new();
        let root = dir.path().join("firmware");
        let mut editor = FirmwareEditor::load(Some(root.clone()));
        editor.manual_model = "  CP4N  ".into();
        editor.add_manual_model();
        assert_eq!(editor.selected_model, "CP4N");
        assert!(editor.manual_model.is_empty());
        assert!(editor.error.is_none());

        editor.manual_model = "cp4n".into();
        editor.add_manual_model();
        editor.observe_model("cp4n");
        editor.poll(&egui::Context::default());
        let loaded = Catalog::load(&root).unwrap();
        assert_eq!(loaded.models.len(), 1);
        assert!(loaded.models.contains_key("CP4N"));
    }

    #[test]
    fn removing_assignments_deletes_only_unreferenced_firmware_files() {
        let dir = TestDir::new();
        let root = dir.path().join("firmware");
        let source = dir.path().join("device.puf");
        fs::write(&source, b"shared firmware").unwrap();
        let file = copy_firmware(&root, &source).unwrap();
        let stored = root.join(&file.stored_file);
        let mut editor = FirmwareEditor::load(Some(root.clone()));
        editor
            .catalog
            .models
            .insert("RMC4".into(), Some(file.clone()));
        editor
            .catalog
            .models
            .insert("CP4N".into(), Some(file.clone()));
        editor.save_catalog(editor.catalog.clone()).unwrap();

        assert!(
            editor
                .remove_assignment("rmc4")
                .unwrap()
                .contains("retained")
        );
        assert!(stored.is_file());
        assert!(
            editor
                .remove_assignment("CP4N")
                .unwrap()
                .contains("removed")
        );
        assert!(!stored.exists());
        let loaded = Catalog::load(&root).unwrap();
        assert!(loaded.models["RMC4"].is_none());
        assert!(loaded.models["CP4N"].is_none());
    }

    #[test]
    fn failed_replacement_keeps_previous_assignment_and_bytes() {
        let dir = TestDir::new();
        let root = dir.path().join("firmware");
        let source = dir.path().join("device.puf");
        fs::write(&source, b"original firmware").unwrap();
        let original = copy_firmware(&root, &source).unwrap();
        let mut editor = FirmwareEditor::load(Some(root.clone()));
        editor.observe_model("RMC4");
        editor
            .catalog
            .models
            .insert("RMC4".into(), Some(original.clone()));
        editor.save_catalog(editor.catalog.clone()).unwrap();
        editor.selected_model = "RMC4".into();
        editor.start_import(dir.path().join("missing.puf"));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while editor.is_busy() {
            editor.poll(&egui::Context::default());
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(editor.error.is_some());
        assert_eq!(
            Catalog::load(&root).unwrap().models["RMC4"],
            Some(original.clone())
        );
        assert_eq!(
            fs::read(root.join(original.stored_file)).unwrap(),
            b"original firmware"
        );

        let previous = editor.catalog.clone();
        // A deterministic disk failure: a regular file cannot be a directory.
        editor.root = Some(source);
        let mut replacement = previous.clone();
        replacement.models.insert("RMC4".into(), None);
        assert!(editor.save_catalog(replacement).is_err());
        assert_eq!(editor.catalog, previous);
    }

    #[test]
    fn editor_shows_the_package_of_a_puf_and_removes_the_assignment() {
        let dir = TestDir::new();
        let root = dir.path().join("firmware");
        let source = dir.path().join("device.puf");
        fs::write(
            &source,
            crate::test_support::zip(&[
                (
                    "~.package.ini",
                    b"[Package]\nName=RMC4 Firmware\nVersion=2.8001.00049\n",
                    true,
                    crate::test_support::stamp(2024, 3, 15, 14, 22),
                ),
                (
                    "firmware.bin",
                    b"payload",
                    false,
                    crate::test_support::stamp(2024, 3, 15, 14, 22),
                ),
            ]),
        )
        .unwrap();
        let file = copy_firmware(&root, &source).unwrap();
        let mut editor = FirmwareEditor::load(Some(root.clone()));
        for model in ["RMC4", "CP4", "TSW-1070"] {
            editor.observe_model(model);
        }
        // Two models share the file, so the tree has to gather them under it.
        for model in ["RMC4", "CP4"] {
            editor
                .catalog
                .models
                .insert(model.into(), Some(file.clone()));
        }
        editor.save_catalog(editor.catalog.clone()).unwrap();
        editor.selected_model = "RMC4".into();
        editor.open = true;
        let ctx = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 900.0),
            )),
            ..Default::default()
        };
        ctx.run_ui(input(), |ui| editor.show(ui.ctx()))
            .drop_without_applying_deltas();
        let output = ctx.run_ui(input(), |ui| editor.show(ui.ctx()));
        let placed = |label: &str| {
            output.shapes.iter().find_map(|clipped| {
                if let egui::Shape::Text(text) = &clipped.shape
                    && text.galley.text() == label
                {
                    Some(egui::Rect::from_min_size(text.pos, text.galley.size()))
                } else {
                    None
                }
            })
        };
        for expected in [
            "Firmware Editor",
            "Add model",
            // The tree: a branch per file, its models under it, unassigned at
            // the root.
            "device.puf",
            "RMC4",
            "CP4",
            "TSW-1070",
            // The detail pane.
            "Choose firmware file…",
            "File",
            "Size",
            "Imported",
            "SHA-256",
            // The package description, read out of the compressed entry.
            "Package",
            "Name",
            "RMC4 Firmware",
            "Version",
            "2.8001.00049",
        ] {
            assert!(placed(expected).is_some(), "{expected} was not rendered");
        }
        assert!(placed(&crate::archive::human_size(file.bytes)).is_some());
        assert!(
            placed("CP4").unwrap().left() > placed("device.puf").unwrap().left(),
            "assigned models are not indented under their file"
        );
        assert!(
            placed("TSW-1070").unwrap().left() < placed("CP4").unwrap().left(),
            "an unassigned model is not at the root"
        );

        let remove = placed("Remove assignment").unwrap().center();
        output.drop_without_applying_deltas();
        for pressed in [true, false] {
            let mut raw = input();
            raw.events.push(egui::Event::PointerMoved(remove));
            raw.events.push(egui::Event::PointerButton {
                pos: remove,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            ctx.run_ui(raw, |ui| editor.show(ui.ctx()))
                .drop_without_applying_deltas();
        }
        assert!(Catalog::load(&root).unwrap().models["RMC4"].is_none());
        // The other model still holds the file, so the copy has to survive.
        assert!(root.join(&file.stored_file).exists());
    }

    #[test]
    fn editor_reports_a_zip_update_and_a_file_it_cannot_read() {
        let dir = TestDir::new();
        let root = dir.path().join("firmware");
        let update = dir.path().join("update.zip");
        fs::write(
            &update,
            crate::test_support::zip(&[
                (
                    "a.bin",
                    b"a",
                    false,
                    crate::test_support::stamp(2021, 6, 1, 9, 30),
                ),
                (
                    "b.bin",
                    b"bb",
                    false,
                    crate::test_support::stamp(2023, 12, 25, 18, 5),
                ),
            ]),
        )
        .unwrap();
        let broken = dir.path().join("broken.puf");
        fs::write(&broken, b"not an archive").unwrap();
        let mut editor = FirmwareEditor::load(Some(root.clone()));
        let ctx = egui::Context::default();
        let drawn = |editor: &mut FirmwareEditor| {
            let input = || egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 900.0),
                )),
                ..Default::default()
            };
            ctx.run_ui(input(), |ui| editor.show(ui.ctx()))
                .drop_without_applying_deltas();
            let output = ctx.run_ui(input(), |ui| editor.show(ui.ctx()));
            let text = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            output.drop_without_applying_deltas();
            text
        };
        editor.open = true;
        editor.observe_model("RMC4");

        // No assignment yet.
        editor.selected_model = "RMC4".into();
        assert!(
            drawn(&mut editor)
                .iter()
                .any(|text| text == "No firmware assigned.")
        );

        // A zip update reports its own files, since it describes nothing.
        let file = copy_firmware(&root, &update).unwrap();
        editor.catalog.models.insert("RMC4".into(), Some(file));
        let text = drawn(&mut editor);
        for expected in ["Archive", "Files", "2", "Newest file", "2023-12-25 18:05"] {
            assert!(
                text.iter().any(|drawn| drawn == expected),
                "{expected}: {text:?}"
            );
        }
        assert!(!text.iter().any(|drawn| drawn == "Package"));

        // Something that is not an archive at all is reported, not hidden.
        let file = copy_firmware(&root, &broken).unwrap();
        editor.catalog.models.insert("RMC4".into(), Some(file));
        let text = drawn(&mut editor);
        assert!(text.iter().any(|drawn| drawn == "Contents"), "{text:?}");
        assert!(
            text.iter().any(|drawn| drawn.contains("Not a zip archive")),
            "{text:?}"
        );
    }
}
