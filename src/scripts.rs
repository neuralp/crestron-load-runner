use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::PathBuf,
};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::model::Device;

const DEVICE_VARIABLES: [&str; 7] = [
    "device.name",
    "device.host",
    "device.port",
    "device.model",
    "device.mac",
    "device.firmware",
    "device.kind",
];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub name: String,
    pub body: String,
}

/// Templates are substitutions, not a programming language. Blank lines and
/// full-line # comments are ignored; each remaining line is one SSH exec command.
fn command_lines(body: &str) -> impl Iterator<Item = &str> {
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

fn substitute(
    line: &str,
    mut lookup: impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut remaining = line;
    let mut output = String::new();
    while let Some(start) = remaining.find("{{") {
        let literal = &remaining[..start];
        if literal.contains("}}") {
            return Err("Unexpected }} in template".into());
        }
        output.push_str(literal);
        remaining = &remaining[start + 2..];
        let end = remaining
            .find("}}")
            .ok_or("Unclosed {{ variable in template")?;
        let key = remaining[..end].trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
            || key.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(format!("Invalid template variable: {key:?}"));
        }
        if key.starts_with("device.") && !DEVICE_VARIABLES.contains(&key) {
            return Err(format!("Unknown device variable: {key}"));
        }
        let value = lookup(key)?;
        if value.chars().any(char::is_control) {
            return Err(format!("Variable {key} contains a control character"));
        }
        output.push_str(&value);
        remaining = &remaining[end + 2..];
    }
    if remaining.contains("}}") {
        return Err("Unexpected }} in template".into());
    }
    output.push_str(remaining);
    if output.chars().any(|c| c.is_control() && c != '\t') {
        return Err("Commands cannot contain control characters".into());
    }
    Ok(output)
}

impl Script {
    pub fn variables(&self) -> Result<BTreeSet<String>, String> {
        let mut variables = BTreeSet::new();
        let mut count = 0;
        for line in command_lines(&self.body) {
            count += 1;
            substitute(line, |key| {
                if !DEVICE_VARIABLES.contains(&key) {
                    variables.insert(key.to_owned());
                }
                Ok(String::new())
            })?;
        }
        if count == 0 {
            return Err("A script must contain at least one command".into());
        }
        Ok(variables)
    }

    pub fn render(
        &self,
        target: &ScriptTarget,
        variables: &BTreeMap<String, String>,
    ) -> Result<Vec<String>, String> {
        self.variables()?;
        command_lines(&self.body)
            .map(|line| {
                let command = substitute(line, |key| {
                    let value = if DEVICE_VARIABLES.contains(&key) {
                        target.variables.get(key)
                    } else {
                        variables.get(key)
                    }
                    .ok_or_else(|| format!("Missing variable: {key}"))?;
                    if value.trim().is_empty() {
                        return Err(format!("Variable {key} is empty"));
                    }
                    Ok(value.clone())
                })?;
                if command.trim().is_empty() {
                    return Err("A rendered command is empty".into());
                }
                Ok(command)
            })
            .collect()
    }
}

#[derive(Clone)]
pub struct ScriptTarget {
    pub id: String,
    pub name: String,
    variables: BTreeMap<String, String>,
}

impl From<&Device> for ScriptTarget {
    fn from(device: &Device) -> Self {
        Self {
            id: device.id.clone(),
            name: format!(
                "{} ({}:{})",
                device.display_name(),
                device.host,
                device.port
            ),
            variables: [
                ("device.name", device.display_name().to_owned()),
                ("device.host", device.host.clone()),
                ("device.port", device.port.to_string()),
                ("device.model", device.model.clone()),
                ("device.mac", device.mac.clone()),
                ("device.firmware", device.firmware.clone()),
                ("device.kind", device.kind.label().to_owned()),
            ]
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
        }
    }
}

#[derive(Default)]
pub struct ScriptEditor {
    pub open: bool,
    pub scripts: Vec<Script>,
    draft: Vec<Script>,
    selected: Option<usize>,
    path: Option<PathBuf>,
    load_failed: bool,
    message: String,
}

impl ScriptEditor {
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut editor = Self {
            path,
            ..Default::default()
        };
        if let Some(path) = &editor.path {
            let loaded = match fs::read(path) {
                Ok(data) => serde_json::from_slice::<Vec<Script>>(&data).map_err(|e| e.to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(e) => Err(e.to_string()),
            }
            .and_then(|scripts| {
                validate_catalog(&scripts)?;
                Ok(scripts)
            });
            match loaded {
                Ok(scripts) => {
                    editor.draft = scripts.clone();
                    editor.scripts = scripts;
                }
                Err(error) => {
                    editor.load_failed = true;
                    editor.message = format!(
                        "Cannot read {}: {error}. Saving is disabled to protect the file; repair it and restart.",
                        path.display()
                    );
                }
            }
        }
        editor
    }

    pub fn is_dirty(&self) -> bool {
        self.draft != self.scripts
    }

    fn save(&mut self) -> Result<(), String> {
        if self.load_failed {
            return Err("Cannot overwrite an unreadable script library".into());
        }
        validate_catalog(&self.draft)?;
        let path = self
            .path
            .as_ref()
            .ok_or("No script library path is available")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let temporary = path.with_extension("json.tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| format!("Could not create {}: {e}", temporary.display()))?;
        let result = (|| {
            file.write_all(&serde_json::to_vec_pretty(&self.draft)?)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path)?;
            let written: Vec<Script> = serde_json::from_slice(&fs::read(path)?)?;
            if written != self.draft {
                return Err(std::io::Error::other(
                    "Script library read-back verification failed",
                ));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|e: std::io::Error| format!("Could not save scripts: {e}"))?;
        self.scripts = self.draft.clone();
        Ok(())
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Script Editor")
            .open(&mut open)
            .default_width(700.0)
            .show(ctx, |ui| {
            ui.label("Named Crestron console scripts · one command per line · # comments");
            ui.label("Use {{device.host}}, {{device.name}}, {{device.port}}, {{device.model}}, {{device.mac}}, {{device.firmware}}, {{device.kind}}.");
            ui.label("Other placeholders, e.g. {{room}}, prompt for a value before each run. Values are substituted literally, without quoting. Do not store passwords in scripts.");
            ui.horizontal(|ui| {
                if ui.button("New script").clicked() {
                    self.draft.push(Script::default());
                    self.selected = Some(self.draft.len() - 1);
                }
                if ui.add_enabled(self.selected.is_some(), egui::Button::new("Delete script")).clicked()
                    && let Some(index) = self.selected.take()
                {
                    self.draft.remove(index);
                }
                if ui.add_enabled(!self.load_failed && self.is_dirty(), egui::Button::new("Save scripts")).clicked() {
                    self.message = match self.save() { Ok(()) => "Scripts saved".into(), Err(e) => e };
                }
                if ui.add_enabled(self.is_dirty(), egui::Button::new("Discard changes")).clicked() {
                    self.draft = self.scripts.clone();
                    self.selected = None;
                    self.message.clear();
                }
            });
            if self.is_dirty() { ui.label("Unsaved changes — only saved scripts can run. Closing this window retains the draft."); }
            egui::ComboBox::from_id_salt("script_to_edit")
                .selected_text(self.selected.and_then(|i| self.draft.get(i)).map_or("Select a script", |s| if s.name.is_empty() { "Untitled" } else { &s.name }))
                .show_ui(ui, |ui| {
                    for (index, script) in self.draft.iter().enumerate() {
                        ui.selectable_value(&mut self.selected, Some(index), if script.name.is_empty() { "Untitled" } else { &script.name });
                    }
                });
            if let Some(script) = self.selected.and_then(|i| self.draft.get_mut(i)) {
                ui.horizontal(|ui| { ui.label("Name"); ui.text_edit_singleline(&mut script.name); });
                egui::ScrollArea::vertical().max_height(350.0).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut script.body).font(egui::TextStyle::Monospace).desired_rows(12).desired_width(f32::INFINITY));
                });
                if let Err(error) = script.variables() { ui.colored_label(ui.visuals().error_fg_color, error); }
            }
            if let Some(path) = &self.path { ui.small(format!("Library: {}", path.display())); }
            if !self.message.is_empty() { ui.label(&self.message); }
        });
        self.open = open;
    }
}

fn validate_catalog(scripts: &[Script]) -> Result<(), String> {
    let mut names = BTreeSet::new();
    for script in scripts {
        if script.name.trim().is_empty() || script.name.chars().any(char::is_control) {
            return Err("Every script needs a nonempty name without control characters".into());
        }
        if !names.insert(script.name.trim().to_lowercase()) {
            return Err(format!("Duplicate script name: {}", script.name));
        }
        script
            .variables()
            .map_err(|e| format!("{}: {e}", script.name))?;
    }
    Ok(())
}

pub struct RunRequest {
    pub name: String,
    pub jobs: Vec<(String, Vec<String>)>,
}

pub struct RunDialog {
    pub open: bool,
    scripts: Vec<Script>,
    targets: Vec<ScriptTarget>,
    selected: usize,
    variables: BTreeMap<String, String>,
}

impl RunDialog {
    pub fn new(scripts: Vec<Script>, targets: Vec<ScriptTarget>) -> Self {
        Self {
            open: true,
            scripts,
            targets,
            selected: 0,
            variables: BTreeMap::new(),
        }
    }

    fn prepare(&self) -> Result<RunRequest, String> {
        let script = self
            .scripts
            .get(self.selected)
            .ok_or("Save a script in Devices → Script Editor first")?;
        if self.targets.is_empty() {
            return Err("Select at least one target device".into());
        }
        let jobs = self
            .targets
            .iter()
            .map(|target| {
                script
                    .render(target, &self.variables)
                    .map(|commands| (target.id.clone(), commands))
                    .map_err(|e| format!("{}: {e}", target.name))
            })
            .collect::<Result<_, _>>()?;
        Ok(RunRequest {
            name: script.name.clone(),
            jobs,
        })
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<RunRequest> {
        let mut open = self.open;
        let mut request = None;
        egui::Window::new("Run Script")
            .open(&mut open)
            .default_width(650.0)
            .show(ctx, |ui| {
            let previous = self.selected;
            egui::ComboBox::from_id_salt("script_to_run")
                .selected_text(self.scripts.get(self.selected).map_or("No saved scripts", |s| s.name.as_str()))
                .show_ui(ui, |ui| {
                    for (index, script) in self.scripts.iter().enumerate() {
                        ui.selectable_value(&mut self.selected, index, &script.name);
                    }
                });
            if previous != self.selected { self.variables.clear(); }
            if let Some(script) = self.scripts.get(self.selected)
                && let Ok(keys) = script.variables()
            {
                for key in keys {
                    ui.horizontal(|ui| { ui.label(&key); ui.text_edit_singleline(self.variables.entry(key).or_default()); });
                }
            }
            ui.label(format!("{} target device(s). Commands run in order on each device; different devices run concurrently.", self.targets.len()));
            ui.label("Review every command. Substitutions are literal, not escaped. Scripts may change configuration or reboot devices. Commands and responses are recorded in the device log.");
            let prepared = self.prepare();
            egui::ScrollArea::both().max_height(350.0).show(ui, |ui| {
                for (index, target) in self.targets.iter().enumerate() {
                    ui.strong(&target.name);
                    if let Ok(prepared) = &prepared {
                        let text = prepared.jobs[index].1.join("\n");
                        let mut text = text.as_str();
                        ui.add(egui::TextEdit::multiline(&mut text).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY));
                    }
                }
            });
            if let Err(error) = &prepared { ui.colored_label(ui.visuals().error_fg_color, error); }
            ui.horizontal(|ui| {
                if ui.add_enabled(prepared.is_ok(), egui::Button::new("Run on these devices")).clicked() {
                    request = prepared.ok();
                }
                if ui.button("Cancel").clicked() { self.open = false; }
            });
        });
        self.open &= open && request.is_none();
        request
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::AddressEntry, test_support::TestDir};

    fn target(host: &str) -> ScriptTarget {
        ScriptTarget::from(&Device::from_address(&AddressEntry {
            host: host.into(),
            ..Default::default()
        }))
    }

    #[test]
    fn editor_buttons_create_save_and_delete_named_scripts() {
        let dir = TestDir::new();
        let path = dir.path().join("scripts.json");
        let mut editor = ScriptEditor::load(Some(path.clone()));
        editor.open = true;
        let ctx = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1360.0, 900.0),
            )),
            ..Default::default()
        };
        let click = |editor: &mut ScriptEditor, label| {
            for _ in 0..2 {
                ctx.run_ui(input(), |ui| editor.show(ui.ctx()))
                    .drop_without_applying_deltas();
            }
            let output = ctx.run_ui(input(), |ui| editor.show(ui.ctx()));
            let pos = output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.text() == label
                    {
                        Some(text.pos + text.galley.size() * 0.5)
                    } else {
                        None
                    }
                })
                .expect(label);
            output.drop_without_applying_deltas();
            for pressed in [true, false] {
                let mut raw = input();
                raw.events.push(egui::Event::PointerMoved(pos));
                raw.events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
                ctx.run_ui(raw, |ui| editor.show(ui.ctx()))
                    .drop_without_applying_deltas();
            }
        };
        click(&mut editor, "New script");
        assert_eq!(editor.selected, Some(0));
        assert!(editor.is_dirty());
        editor.draft[0] = Script {
            name: "Room info".into(),
            body: "hostname\nver".into(),
        };
        click(&mut editor, "Save scripts");
        assert!(!editor.is_dirty(), "{}", editor.message);
        assert_eq!(
            ScriptEditor::load(Some(path.clone())).scripts[0].name,
            "Room info"
        );
        click(&mut editor, "Delete script");
        assert!(editor.draft.is_empty());
        assert_eq!(editor.scripts.len(), 1);
        click(&mut editor, "Discard changes");
        assert_eq!(editor.draft.len(), 1);
        editor.selected = Some(0);
        click(&mut editor, "Delete script");
        click(&mut editor, "Save scripts");
        assert!(ScriptEditor::load(Some(path)).scripts.is_empty());
    }

    #[test]
    fn templates_are_literal_and_require_all_variables() {
        let script = Script { name: "Room setup".into(), body: "# comment {{ignored}}\r\nhostname {{ room }}\r\n# another comment\r\nroute {{device.host}} {{device.port}}".into() };
        assert_eq!(script.variables().unwrap(), BTreeSet::from(["room".into()]));
        assert!(
            script
                .render(&target("192.0.2.1"), &BTreeMap::new())
                .is_err()
        );
        let vars = BTreeMap::from([("room".into(), "{{not_recursive}}".into())]);
        assert_eq!(
            script.render(&target("192.0.2.1"), &vars).unwrap(),
            ["hostname {{not_recursive}}", "route 192.0.2.1 22"]
        );
        for value in ["", "room\nreboot", "room\rreboot", "bad\0value"] {
            assert!(
                script
                    .render(
                        &target("192.0.2.1"),
                        &BTreeMap::from([("room".into(), value.into())])
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn malformed_templates_and_unknown_device_fields_are_rejected() {
        for body in [
            "# only comment",
            "",
            "hostname {{room",
            "hostname }}",
            "{{}}",
            "{{a-b}}",
            "{{device.password}}",
            "{{device.typo}}",
            "{{1name}}",
        ] {
            assert!(
                Script {
                    name: "test".into(),
                    body: body.into()
                }
                .variables()
                .is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn library_round_trips_edits_and_deletions_without_losing_valid_data() {
        let dir = TestDir::new();
        let path = dir.path().join("scripts.json");
        let mut editor = ScriptEditor::load(Some(path.clone()));
        editor.draft.push(Script {
            name: "Info".into(),
            body: "hostname\nver".into(),
        });
        editor.save().unwrap();
        assert_eq!(
            ScriptEditor::load(Some(path.clone())).scripts,
            editor.scripts
        );
        editor.draft[0].name = "Updated".into();
        editor.save().unwrap();
        assert_eq!(
            ScriptEditor::load(Some(path.clone())).scripts[0].name,
            "Updated"
        );
        editor.draft.push(editor.draft[0].clone());
        assert!(editor.save().is_err());
        assert_eq!(ScriptEditor::load(Some(path.clone())).scripts.len(), 1);
        editor.draft.clear();
        editor.save().unwrap();
        assert!(ScriptEditor::load(Some(path)).scripts.is_empty());
    }

    #[test]
    fn corrupt_library_and_failed_saves_preserve_disk_and_saved_scripts() {
        let dir = TestDir::new();
        let path = dir.path().join("scripts.json");
        fs::write(&path, b"not json").unwrap();
        let mut editor = ScriptEditor::load(Some(path.clone()));
        assert!(editor.save().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
        let mut editor = ScriptEditor::load(Some(dir.path().join("new.json")));
        editor.draft.push(Script {
            name: "Info".into(),
            body: "ver".into(),
        });
        fs::write(dir.path().join("new.json.tmp"), b"occupied").unwrap();
        assert!(editor.save().is_err());
        assert!(editor.scripts.is_empty());
        assert!(editor.is_dirty());
    }

    #[test]
    fn run_dialog_prompts_for_custom_values_and_requires_a_valid_preview() {
        let mut dialog = RunDialog::new(
            vec![Script {
                name: "Room setup".into(),
                body: "hostname {{room}}".into(),
            }],
            vec![target("192.0.2.1")],
        );
        let ctx = egui::Context::default();
        let render = |dialog: &mut RunDialog, events| {
            let mut request = None;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1100.0, 800.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    ui.style_mut().animation_time = 0.0;
                    request = dialog.show(ui.ctx());
                },
            );
            (output, request)
        };
        render(&mut dialog, Vec::new())
            .0
            .drop_without_applying_deltas();
        let (output, _) = render(&mut dialog, Vec::new());
        let labels = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(labels.contains("room"));
        assert!(labels.contains("Variable room is empty"));
        output.drop_without_applying_deltas();
        assert!(dialog.prepare().is_err());
        // Fill the prompted value, then exercise the actual confirmation button.
        dialog.variables.insert("room".into(), "LivingRoom".into());
        let (output, _) = render(&mut dialog, Vec::new());
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text().contains("hostname LivingRoom")
        )));
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape
                    && text.galley.text() == "Run on these devices"
                {
                    Some(text.pos + text.galley.size() * 0.5)
                } else {
                    None
                }
            })
            .unwrap();
        output.drop_without_applying_deltas();
        let events = |pressed| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        render(&mut dialog, events(true))
            .0
            .drop_without_applying_deltas();
        let (output, request) = render(&mut dialog, events(false));
        output.drop_without_applying_deltas();
        assert_eq!(request.unwrap().jobs[0].1, ["hostname LivingRoom"]);
        assert!(!dialog.open);
    }

    #[test]
    fn run_preflight_checks_all_targets_before_producing_jobs() {
        let script = Script {
            name: "Test".into(),
            body: "hostname {{device.host}}\nver".into(),
        };
        let mut dialog =
            RunDialog::new(vec![script], vec![target("192.0.2.1"), target("192.0.2.2")]);
        let request = dialog.prepare().unwrap();
        assert_eq!(request.jobs.len(), 2);
        assert_eq!(request.jobs[1].1, ["hostname 192.0.2.2", "ver"]);
        dialog.targets[1]
            .variables
            .insert("device.host".into(), "bad\nreboot".into());
        assert!(dialog.prepare().is_err());
    }
}
