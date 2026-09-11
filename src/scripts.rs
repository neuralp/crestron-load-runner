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
    /// Model this script applies to, as a wildcard pattern. Optional, and only
    /// used to preselect a script in Run Script; it never restricts what runs.
    #[serde(default)]
    pub model: String,
    pub body: String,
}

/// Case-insensitive wildcard match against a device model: `*` matches any run
/// of characters and `?` exactly one. An empty pattern or an unknown model
/// matches nothing, so scripts without a model never claim a device.
fn model_matches(pattern: &str, model: &str) -> bool {
    let pattern: Vec<char> = pattern
        .trim()
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let model: Vec<char> = model
        .trim()
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if pattern.is_empty() || model.is_empty() {
        return false;
    }
    // Greedy scan with one backtrack point: the last `*` and the position after
    // the character it most recently consumed.
    let (mut p, mut m) = (0, 0);
    let (mut star, mut consumed) = (None, 0);
    while m < model.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some(p);
                p += 1;
                consumed = m;
            }
            Some('?') => {
                p += 1;
                m += 1;
            }
            Some(&c) if c == model[m] => {
                p += 1;
                m += 1;
            }
            _ => match star {
                Some(index) => {
                    p = index + 1;
                    consumed += 1;
                    m = consumed;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
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
    /// How the script reads in the editor and run-dialog pickers.
    fn label(&self) -> String {
        let name = self.name.trim();
        let name = if name.is_empty() { "Untitled" } else { name };
        match self.model.trim() {
            "" => name.to_owned(),
            model => format!("{name} ({model})"),
        }
    }

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
    /// Discovered model, used to preselect a matching script.
    model: String,
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
            model: device.model.clone(),
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

/// Starting width of the script tree; the divider is draggable from there.
const TREE_WIDTH: f32 = 220.0;

fn leaf(ui: &mut egui::Ui, selected: &mut Option<usize>, index: usize, script: &Script) {
    let name = script.name.trim();
    let name = if name.is_empty() { "Untitled" } else { name };
    ui.selectable_value(selected, Some(index), name);
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

    pub fn save(&mut self) -> Result<(), String> {
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
        let open =
            crate::popout::window(ctx, "Script Editor", [940.0, 620.0], |ui| self.panels(ui));
        self.open = open;
    }

    /// Tools across the top, the library tree on the left, the selected script
    /// on the right, and the template reference along the bottom.
    fn panels(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("script_tools").show(ui, |ui| {
            ui.add_space(4.0);
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
            ui.add_space(4.0);
        });
        egui::Panel::bottom("script_reference").show(ui, |ui| {
            ui.add_space(4.0);
            ui.label("Named Crestron console scripts · one command per line · # comments");
            ui.label("Use {{device.host}}, {{device.name}}, {{device.port}}, {{device.model}}, {{device.mac}}, {{device.firmware}}, {{device.kind}}.");
            ui.label("Other placeholders, e.g. {{room}}, prompt for a value before each run. Values are substituted literally, without quoting. Do not store passwords in scripts.");
            ui.label("Model is optional and matches the discovered model to preselect this script in Run Script: * matches any characters and ? exactly one, e.g. TSW-*.");
            if let Some(path) = &self.path { ui.small(format!("Library: {}", path.display())); }
            if !self.message.is_empty() { ui.label(&self.message); }
            ui.add_space(4.0);
        });
        egui::Panel::left("script_tree")
            .default_size(TREE_WIDTH)
            .show(ui, |ui| self.tree(ui));
        egui::CentralPanel::default().show(ui, |ui| self.detail(ui));
    }

    /// The library as a tree: a branch per model, with scripts that have no
    /// model left at the root so the tree only branches where a model gives it
    /// something to branch on.
    fn tree(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("script_tree")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut branches: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                let mut roots = Vec::new();
                for (index, script) in self.draft.iter().enumerate() {
                    match script.model.trim() {
                        "" => roots.push(index),
                        model => branches.entry(model.to_owned()).or_default().push(index),
                    }
                }
                if branches.is_empty() && roots.is_empty() {
                    ui.weak("No scripts yet — choose New script.");
                    return;
                }
                for (model, indices) in &branches {
                    egui::CollapsingHeader::new(model)
                        .id_salt(("script_model", model))
                        .default_open(true)
                        .show(ui, |ui| {
                            for &index in indices {
                                leaf(ui, &mut self.selected, index, &self.draft[index]);
                            }
                        });
                }
                for &index in &roots {
                    leaf(ui, &mut self.selected, index, &self.draft[index]);
                }
            });
    }

    /// The selected script's fields, or a hint when the tree has no selection.
    fn detail(&mut self, ui: &mut egui::Ui) {
        let Some(script) = self.selected.and_then(|index| self.draft.get_mut(index)) else {
            ui.weak("Select a script in the tree, or choose New script.");
            return;
        };
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut script.name).desired_width(220.0));
            ui.label("Model");
            ui.add(egui::TextEdit::singleline(&mut script.model).desired_width(160.0));
        });
        // Keep the template error in view by reserving its line before the body
        // takes the rest of the pane.
        let issue = script.variables().err();
        let body = (ui.available_height() - if issue.is_some() { 24.0 } else { 0.0 }).max(80.0);
        egui::ScrollArea::vertical()
            .id_salt("script_body")
            .max_height(body)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut script.body)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(12)
                        .desired_width(f32::INFINITY),
                );
            });
        if let Some(error) = issue {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }
}

#[cfg(test)]
impl ScriptEditor {
    /// Lets tests outside this module put the editor into a dirty state.
    pub fn draft_script(&mut self, script: Script) {
        self.draft.push(script);
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
        if script.model.chars().any(char::is_control) {
            return Err(format!(
                "{}: a model cannot contain control characters",
                script.name
            ));
        }
        script
            .variables()
            .map_err(|e| format!("{}: {e}", script.name))?;
    }
    Ok(())
}

/// The first script whose model pattern covers every target, so a mixed
/// selection preselects nothing rather than something wrong. Falls back to the
/// first script when no pattern matches.
fn preselect(scripts: &[Script], targets: &[ScriptTarget]) -> usize {
    if targets.is_empty() {
        return 0;
    }
    scripts
        .iter()
        .position(|script| {
            targets
                .iter()
                .all(|target| model_matches(&script.model, &target.model))
        })
        .unwrap_or(0)
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
        let selected = preselect(&scripts, &targets);
        Self {
            open: true,
            scripts,
            targets,
            selected,
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
        let mut request = None;
        egui::Modal::new(egui::Id::new("run_script"))
            .backdrop_color(egui::Color32::TRANSPARENT)
            .show(ctx, |ui| {
            ui.set_width(650.0);
            ui.heading("Run Script");
            let previous = self.selected;
            egui::ComboBox::from_id_salt("script_to_run")
                .selected_text(self.scripts.get(self.selected).map_or_else(|| "No saved scripts".to_owned(), Script::label))
                .show_ui(ui, |ui| {
                    for (index, script) in self.scripts.iter().enumerate() {
                        ui.selectable_value(&mut self.selected, index, script.label());
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
        self.open &= request.is_none();
        request
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::AddressEntry, test_support::TestDir};

    fn target(host: &str) -> ScriptTarget {
        modeled_target(host, "")
    }

    /// Renders one editor frame and returns every drawn string with the centre
    /// of its box, which is both what the window shows and where to click.
    fn editor_frame(
        ctx: &egui::Context,
        editor: &mut ScriptEditor,
        events: Vec<egui::Event>,
    ) -> Vec<(String, egui::Rect)> {
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1360.0, 900.0),
                )),
                events,
                ..Default::default()
            },
            |ui| editor.show(ui.ctx()),
        );
        let drawn = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_owned(),
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                )),
                _ => None,
            })
            .collect();
        output.drop_without_applying_deltas();
        drawn
    }

    /// Settles the layout, then returns what the editor draws.
    fn editor_text(ctx: &egui::Context, editor: &mut ScriptEditor) -> Vec<(String, egui::Rect)> {
        for _ in 0..2 {
            editor_frame(ctx, editor, Vec::new());
        }
        editor_frame(ctx, editor, Vec::new())
    }

    fn at(drawn: &[(String, egui::Rect)], label: &str) -> egui::Rect {
        drawn
            .iter()
            .find_map(|(text, rect)| (text == label).then_some(*rect))
            .unwrap_or_else(|| panic!("{label} is not on screen: {drawn:?}"))
    }

    /// Clicks the first widget drawing exactly `label`.
    fn click(ctx: &egui::Context, editor: &mut ScriptEditor, label: &str) {
        let pos = at(&editor_text(ctx, editor), label).center();
        for pressed in [true, false] {
            editor_frame(
                ctx,
                editor,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
    }

    fn modeled_target(host: &str, model: &str) -> ScriptTarget {
        ScriptTarget::from(&Device::from_address(&AddressEntry {
            host: host.into(),
            model: model.into(),
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
        click(&ctx, &mut editor, "New script");
        assert_eq!(editor.selected, Some(0));
        assert!(editor.is_dirty());
        editor.draft[0] = Script {
            name: "Room info".into(),
            model: "RMC?".into(),
            body: "hostname\nver".into(),
        };
        // The model sits beside the name, and branches the tree.
        let drawn = editor_text(&ctx, &mut editor);
        for expected in ["Name", "Model", "RMC?", "Room info", "hostname\nver"] {
            at(&drawn, expected);
        }
        click(&ctx, &mut editor, "Save scripts");
        assert!(!editor.is_dirty(), "{}", editor.message);
        let saved = ScriptEditor::load(Some(path.clone()));
        assert_eq!(saved.scripts[0].name, "Room info");
        assert_eq!(saved.scripts[0].model, "RMC?");
        click(&ctx, &mut editor, "Delete script");
        assert!(editor.draft.is_empty());
        assert_eq!(editor.scripts.len(), 1);
        click(&ctx, &mut editor, "Discard changes");
        assert_eq!(editor.draft.len(), 1);
        editor.selected = Some(0);
        click(&ctx, &mut editor, "Delete script");
        click(&ctx, &mut editor, "Save scripts");
        assert!(ScriptEditor::load(Some(path)).scripts.is_empty());
    }

    #[test]
    fn the_tree_branches_on_model_and_opens_the_script_that_is_clicked() {
        let ctx = egui::Context::default();
        let mut editor = ScriptEditor::load(None);
        editor.open = true;
        let empty = editor_text(&ctx, &mut editor);
        at(&empty, "No scripts yet — choose New script.");
        at(&empty, "Select a script in the tree, or choose New script.");

        let script = |name: &str, model: &str| Script {
            name: name.into(),
            model: model.into(),
            body: "ver".into(),
        };
        editor.draft = vec![
            script("Panel info", "TSW-*"),
            script("Anything", ""),
            script("Processor info", "RMC4"),
        ];
        let drawn = editor_text(&ctx, &mut editor);
        // Branches are sorted by model and hold their scripts; a script with no
        // model stays at the root, below the branches and outdented from them.
        assert!(at(&drawn, "RMC4").top() < at(&drawn, "TSW-*").top());
        assert!(at(&drawn, "Processor info").left() > at(&drawn, "RMC4").left());
        assert!(at(&drawn, "Panel info").left() > at(&drawn, "TSW-*").left());
        assert!(at(&drawn, "Anything").top() > at(&drawn, "Panel info").top());
        assert!(at(&drawn, "Anything").left() < at(&drawn, "Panel info").left());
        // The tree is the selection: clicking a leaf opens it on the right.
        click(&ctx, &mut editor, "Processor info");
        assert_eq!(editor.selected, Some(2));
        let drawn = editor_text(&ctx, &mut editor);
        assert!(
            !drawn
                .iter()
                .any(|(text, _)| text.starts_with("Select a script"))
        );
        assert!(at(&drawn, "ver").left() > at(&drawn, "Processor info").left());
        click(&ctx, &mut editor, "Anything");
        assert_eq!(editor.selected, Some(1));
    }

    #[test]
    fn templates_are_literal_and_require_all_variables() {
        let script = Script { name: "Room setup".into(), model: String::new(), body: "# comment {{ignored}}\r\nhostname {{ room }}\r\n# another comment\r\nroute {{device.host}} {{device.port}}".into() };
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
                    model: String::new(),
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
            model: "RMC4".into(),
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
            model: String::new(),
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
                model: String::new(),
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
    fn model_wildcards_match_case_insensitively_and_never_match_blanks() {
        for (pattern, model) in [
            ("TSW-1070", "tsw-1070"),
            ("TSW-*", "TSW-1070"),
            ("tsw-*", "TSW-770"),
            ("*-1070", "TSW-1070"),
            ("RMC?", "RMC3"),
            ("*", "CP4N"),
            ("CP4*N*", "CP4-R-N"),
            (" TSW-* ", " TSW-60 "),
        ] {
            assert!(
                model_matches(pattern, model),
                "{pattern} should match {model}"
            );
        }
        for (pattern, model) in [
            ("TSW-*", "TS-1070"),
            ("RMC4", "RMC40"),
            ("RMC?", "RMC"),
            ("RMC?", "RMC4X"),
            ("", "RMC4"),
            ("*", ""),
            ("TSW-*", ""),
        ] {
            assert!(
                !model_matches(pattern, model),
                "{pattern} should not match {model}"
            );
        }
    }

    #[test]
    fn run_dialog_preselects_the_first_script_matching_every_target_model() {
        let script = |name: &str, model: &str| Script {
            name: name.into(),
            model: model.into(),
            body: "ver".into(),
        };
        let scripts = vec![
            script("Anything", ""),
            script("Panels", "TSW-*"),
            script("Ten-inch panels", "TSW-10??"),
            script("Processors", "rmc4"),
        ];
        let selected =
            |targets: Vec<ScriptTarget>| RunDialog::new(scripts.clone(), targets).selected;
        // An exact model wins regardless of case, and the first matching
        // wildcard wins over a later, narrower one.
        assert_eq!(selected(vec![modeled_target("192.0.2.1", "RMC4")]), 3);
        assert_eq!(selected(vec![modeled_target("192.0.2.1", "TSW-1070")]), 1);
        // Every target must match, so a mixed selection preselects nothing.
        assert_eq!(
            selected(vec![
                modeled_target("192.0.2.1", "TSW-770"),
                modeled_target("192.0.2.2", "TSW-1070"),
            ]),
            1
        );
        assert_eq!(
            selected(vec![
                modeled_target("192.0.2.1", "TSW-770"),
                modeled_target("192.0.2.2", "RMC4"),
            ]),
            0
        );
        // An undiscovered or unrecognized model falls back to the first script.
        assert_eq!(selected(vec![modeled_target("192.0.2.1", "")]), 0);
        assert_eq!(selected(vec![modeled_target("192.0.2.1", "NVX-360")]), 0);
        assert_eq!(selected(Vec::new()), 0);
    }

    #[test]
    fn libraries_saved_before_models_load_and_bad_models_are_rejected() {
        let dir = TestDir::new();
        let path = dir.path().join("scripts.json");
        fs::write(&path, br#"[{"name":"Info","body":"ver"}]"#).unwrap();
        let mut editor = ScriptEditor::load(Some(path));
        assert_eq!(editor.scripts[0].model, "");
        editor.draft[0].model = "TSW-\n*".into();
        assert!(editor.save().is_err());
        assert_eq!(editor.scripts[0].model, "");
    }

    #[test]
    fn run_preflight_checks_all_targets_before_producing_jobs() {
        let script = Script {
            name: "Test".into(),
            model: String::new(),
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
