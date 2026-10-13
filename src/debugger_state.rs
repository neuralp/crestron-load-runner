//! Per-processor/slot debugger preferences. Never persists values or credentials.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;

#[cfg(test)]
#[path = "debugger_state_tests.rs"]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalRef {
    signum: u32,
    name: String,
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    disabled_signals: Vec<SignalRef>,
    watch_groups: BTreeMap<String, Vec<SignalRef>>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    processors: BTreeMap<String, BTreeMap<u8, Settings>>,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            version: 1,
            processors: BTreeMap::new(),
        }
    }
}
impl Document {
    fn load(path: &Path) -> io::Result<Self> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(io::Error::other("Debugger settings exceed 16 MiB"));
        }
        let document: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if document.version != 1
            || document
                .processors
                .values()
                .any(|slots| slots.keys().any(|slot| !(1..=10).contains(slot)))
        {
            return Err(io::Error::other(
                "Unsupported debugger settings version or program slot",
            ));
        }
        Ok(document)
    }
    fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(io::Error::other("Debugger settings exceed 16 MiB"));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(&temporary, path)?;
        if Self::load(path)? != *self {
            return Err(io::Error::other(
                "Debugger settings read-back verification failed",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Selection {
    disabled: HashSet<u32>,
    groups: BTreeMap<String, Vec<u32>>,
}

pub(super) struct Persistence {
    processor: String,
    path: Option<PathBuf>,
    slot: Option<u8>,
    last_attempt: Option<Selection>,
    pub pending_disabled: HashSet<u32>,
    pub notice: String,
    pub details: String,
    pub error: Option<String>,
}
impl Persistence {
    pub fn new(processor: &str) -> Self {
        Self {
            processor: processor.into(),
            // Unit tests inject an isolated path rather than touching user config.
            path: if cfg!(test) {
                None
            } else {
                crate::storage::config_dir().map(|dir| dir.join("debugger-state.json"))
            },
            slot: None,
            last_attempt: None,
            pending_disabled: HashSet::new(),
            notice: String::new(),
            details: String::new(),
            error: None,
        }
    }
}

impl Panel {
    pub(super) fn suspend_settings(&mut self) {
        self.save_settings();
        self.persistence.slot = None;
        self.persistence.pending_disabled.clear();
    }
    pub(super) fn restore_settings(&mut self) {
        self.persistence.slot = None;
        self.persistence.last_attempt = None;
        self.persistence.pending_disabled.clear();
        self.persistence.notice.clear();
        self.persistence.details.clear();
        self.persistence.error = None;
        let Some(path) = &self.persistence.path else {
            if !cfg!(test) {
                self.persistence.error =
                    Some("Could not locate the user configuration directory".into());
            }
            return;
        };
        let document = match Document::load(path) {
            Ok(document) => document,
            Err(error) => {
                self.persistence.error = Some(format!(
                    "Could not load {}: {error}. File left unchanged; restart debugging after fixing it.",
                    path.display()
                ));
                return;
            }
        };
        let settings = document
            .processors
            .get(&self.persistence.processor)
            .and_then(|slots| slots.get(&self.slot))
            .cloned()
            .unwrap_or_default();
        // Match ID + name first; allow recompilation to renumber a uniquely
        // named signal, but never apply settings to a recycled ID alone.
        let mut names: HashMap<&str, HashSet<u32>> = HashMap::new();
        for signal in &self.signals {
            names
                .entry(&signal.name)
                .or_default()
                .insert(signal.signum());
        }
        let mut resolved = BTreeMap::new();
        let mut remapped = 0;
        let mut missing = 0;
        let mut ambiguous = 0;
        let mut details = Vec::new();
        for saved in settings
            .disabled_signals
            .iter()
            .chain(settings.watch_groups.values().flatten())
        {
            if resolved.contains_key(saved) {
                continue;
            }
            let target = match names.get(saved.name.as_str()) {
                Some(ids) if ids.contains(&saved.signum) => Some(saved.signum),
                Some(ids) if ids.len() == 1 => {
                    remapped += 1;
                    ids.iter().next().copied()
                }
                Some(_) => {
                    ambiguous += 1;
                    details.push(format!(
                        "Ambiguous: {} (0x{:08X})",
                        saved.name, saved.signum
                    ));
                    None
                }
                None => {
                    missing += 1;
                    details.push(format!("Missing: {} (0x{:08X})", saved.name, saved.signum));
                    None
                }
            };
            resolved.insert(saved.clone(), target);
        }
        self.disabled_signals.clear();
        self.persistence.pending_disabled = settings
            .disabled_signals
            .iter()
            .filter_map(|signal| resolved[signal])
            .collect();
        self.groups = settings
            .watch_groups
            .into_iter()
            .map(|(group, members)| {
                let mut seen = HashSet::new();
                let members = members
                    .iter()
                    .filter_map(|signal| resolved[signal])
                    .filter(|id| seen.insert(*id))
                    .collect();
                (group, members)
            })
            .collect();
        self.groups.entry("Watched".into()).or_default();
        self.timeline_group = None;
        self.persistence.notice = format!(
            "Settings inventory: {} checked; {remapped} remapped; {missing} missing; {ambiguous} ambiguous",
            resolved.len()
        );
        self.persistence.details = details.join("\n");
        self.persistence.slot = Some(self.slot);
        // Persist the validated inventory, preserving other processors/slots.
        self.save_settings();
    }

    pub(super) fn restore_exclusions_after_sync(&mut self) {
        self.disabled_signals
            .extend(self.persistence.pending_disabled.drain());
    }

    pub(super) fn save_settings(&mut self) {
        let (Some(_), Some(slot)) = (&self.persistence.path, self.persistence.slot) else {
            return;
        };
        let selection = Selection {
            disabled: self
                .disabled_signals
                .union(&self.persistence.pending_disabled)
                .copied()
                .collect(),
            groups: self.groups.clone(),
        };
        if self.persistence.last_attempt.as_ref() == Some(&selection) {
            return;
        }
        self.ensure_signal_lookup();
        let path = self.persistence.path.as_ref().unwrap().clone();
        let reference = |id: &u32| {
            self.signal_lookup
                .as_ref()
                .unwrap()
                .get(id)
                .map(|&index| SignalRef {
                    signum: *id,
                    name: self.signals[index].name.clone(),
                })
        };
        let mut disabled_signals: Vec<_> =
            selection.disabled.iter().filter_map(reference).collect();
        disabled_signals.sort();
        let settings = Settings {
            disabled_signals,
            watch_groups: selection
                .groups
                .iter()
                .map(|(name, ids)| (name.clone(), ids.iter().filter_map(reference).collect()))
                .collect(),
        };
        let result = (|| {
            let mut document = Document::load(&path)?;
            document
                .processors
                .entry(self.persistence.processor.clone())
                .or_default()
                .insert(slot, settings);
            document.save(&path)
        })();
        self.persistence.error = result
            .err()
            .map(|error: io::Error| format!("Could not save {}: {error}", path.display()));
        self.persistence.last_attempt = Some(selection);
    }

    pub(super) fn settings_ui(&mut self, ui: &mut egui::Ui) {
        if !self.persistence.notice.is_empty() {
            ui.weak(&self.persistence.notice)
                .on_hover_text(&self.persistence.details);
        }
        if !self.persistence.pending_disabled.is_empty() {
            ui.weak(format!("Restoring {} exclusions after initial sync ({}/{} values received)", self.persistence.pending_disabled.len(), self.values.len(), self.signal_lookup.as_ref().map_or(0, HashMap::len)))
                .on_hover_text("Saved exclusions are applied after the console acknowledges the command following SYNC. Signature entries that never report values do not block restoration.");
        }
        if let Some(error) = self.persistence.error.clone() {
            ui.colored_label(ui.visuals().warn_fg_color, error);
            if self.persistence.slot.is_some()
                && ui.button("Retry saving debugger settings").clicked()
            {
                self.persistence.last_attempt = None;
                self.save_settings();
            }
        }
    }
}
