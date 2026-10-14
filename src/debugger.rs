//! Processor signal debugger: signature records, bounded traces, and a popout.
use crate::ssh::{DebugCommand, DebugEvent};
use eframe::egui;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::Path,
    sync::mpsc::{self, Receiver},
    time::SystemTime,
};

const HEADER: &[u8] = b"[LOGOSSIG001.000]";
const MAX_EVENTS: usize = 20_000;
/// Large programs' signatures run to a few MiB; this only stops a hostile
/// archive from expanding without bound.
const MAX_EXPANDED_SIGNATURE: u64 = 256 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Signal {
    pub name: String,
    pub metadata: [u8; 6],
}
impl Signal {
    pub fn signum(&self) -> u32 {
        u32::from_le_bytes(self.metadata[..4].try_into().expect("four bytes"))
    }
}

/// A raw `.sig`, or the zip archive holding one that SIMPL and Toolbox leave
/// on the processor as the `.zig`.
pub(crate) fn parse_signals(bytes: &[u8]) -> Result<Vec<Signal>, String> {
    if bytes.starts_with(b"PK\x03\x04") {
        let signature = crate::archive::sole_member(bytes, "sig", MAX_EXPANDED_SIGNATURE)
            .map_err(|error| format!("Could not read the zipped signature: {error}"))?;
        return parse_records(&signature);
    }
    parse_records(bytes)
}

fn parse_records(bytes: &[u8]) -> Result<Vec<Signal>, String> {
    if !bytes.starts_with(HEADER) {
        return Err("Unknown signature format".into());
    }
    let mut offset = HEADER.len();
    let mut signals = Vec::new();
    while offset < bytes.len() {
        let length = bytes
            .get(offset..offset + 2)
            .and_then(|part| part.try_into().ok())
            .map(u16::from_le_bytes)
            .ok_or("Truncated signature length")? as usize;
        if length < 8 || !(length - 8).is_multiple_of(2) {
            return Err(format!("Invalid signature record at byte {offset}"));
        }
        let end = offset
            .checked_add(length)
            .ok_or("Signature length overflow")?;
        let record = bytes.get(offset..end).ok_or("Truncated signature record")?;
        let units = record[2..length - 6]
            .chunks_exact(2)
            .map(|part| u16::from_le_bytes([part[0], part[1]]));
        let name = String::from_utf16(&units.collect::<Vec<_>>())
            .map_err(|_| format!("Invalid UTF-16 at byte {offset}"))?;
        let metadata = record[length - 6..].try_into().expect("six bytes");
        signals.push(Signal { name, metadata });
        offset = end;
    }
    if signals.is_empty() {
        return Err("Signature has no signals".into());
    }
    Ok(signals)
}

pub(crate) fn debug_signums(signals: &[Signal]) -> Vec<u32> {
    let mut seen = HashSet::with_capacity(signals.len());
    signals
        .iter()
        .map(Signal::signum)
        .filter(|number| seen.insert(*number))
        .collect()
}

pub(crate) fn choose_signature(
    slot: u8,
    entries: &[String],
    assigned: Option<&Path>,
) -> Result<String, String> {
    if !(1..=10).contains(&slot) {
        return Err("Program slot must be 1 through 10".into());
    }
    let candidates: Vec<_> = entries
        .iter()
        .filter(|name| {
            crate::ssh::is_safe_remote_file_name(name)
                && Path::new(name)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("zig"))
        })
        .collect();
    if let Some(stem) = assigned
        .and_then(crate::ssh::safe_remote_file_name)
        .as_deref()
        .and_then(|name| Path::new(name).file_stem())
        .and_then(|s| s.to_str())
    {
        let expected = format!("{stem}.zig");
        if let Some(found) = candidates.iter().find(|name| name.as_str() == expected) {
            return Ok((*found).to_string());
        }
    }
    match candidates.as_slice() {
        [one] => Ok((*one).to_string()),
        [] => Err(format!("No .zig in /program{slot:02}")),
        _ => Err(format!(
            "Multiple .zig files in /program{slot:02}; assign the matching program first"
        )),
    }
}

const MAX_SIGNALS_PER_LEAF: usize = 50;
const MAX_BRANCH_CHILDREN: usize = 10;

/// Alphabetically ordered name ranges. Equal names keep distinct record indices.
enum SignalTree {
    Branch { label: String, children: Vec<Self> },
    Leaf { label: String, indices: Vec<usize> },
}

fn range_label(signals: &[Signal], first: usize, last: usize, count: usize) -> String {
    fn preview(name: &str) -> String {
        let mut chars = name.chars();
        let start: String = chars.by_ref().take(22).collect();
        if chars.next().is_some() {
            format!("{start}…")
        } else {
            start
        }
    }
    let from = preview(&signals[first].name);
    let to = preview(&signals[last].name);
    if signals[first].name == signals[last].name {
        format!("{from} · {count} signals (#{first}–#{last})")
    } else {
        format!("{from} – {to} · {count}")
    }
}

fn signal_tree(signals: &[Signal], query: &str) -> Vec<SignalTree> {
    let needle = query.to_lowercase();
    let names: Vec<_> = signals
        .iter()
        .map(|signal| signal.name.to_lowercase())
        .collect();
    let mut indices: Vec<_> = (0..signals.len())
        .filter(|&index| !signals[index].name.starts_with("::") && names[index].contains(&needle))
        .collect();
    indices.sort_by(|&a, &b| {
        names[a]
            .cmp(&names[b])
            .then_with(|| signals[a].name.cmp(&signals[b].name))
            .then_with(|| signals[a].signum().cmp(&signals[b].signum()))
            .then_with(|| a.cmp(&b))
    });
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for index in indices {
        let label = match names[index].chars().next() {
            Some('0'..='9') => "0–9".to_owned(),
            Some(letter @ 'a'..='z') => letter.to_ascii_uppercase().to_string(),
            _ => "Other".to_owned(),
        };
        groups.entry(label).or_default().push(index);
    }
    let other = groups.remove("Other");
    let mut groups: Vec<_> = groups.into_iter().collect();
    if let Some(indices) = other {
        groups.push(("Other".into(), indices));
    }
    groups
        .into_iter()
        .map(|(label, indices)| {
            let count = indices.len();
            let label = format!(
                "{label} · {count} {}",
                if count == 1 { "signal" } else { "signals" }
            );
            if indices.len() <= MAX_SIGNALS_PER_LEAF {
                SignalTree::Leaf { label, indices }
            } else {
                SignalTree::Branch {
                    label,
                    children: signal_ranges(signals, &indices),
                }
            }
        })
        .collect()
}

fn signal_ranges(signals: &[Signal], indices: &[usize]) -> Vec<SignalTree> {
    let mut nodes: Vec<_> = indices
        .chunks(MAX_SIGNALS_PER_LEAF)
        .map(|chunk| SignalTree::Leaf {
            label: range_label(signals, chunk[0], *chunk.last().unwrap(), chunk.len()),
            indices: chunk.to_vec(),
        })
        .collect();
    while nodes.len() > MAX_BRANCH_CHILDREN {
        let mut remaining = nodes.into_iter();
        let mut branches = Vec::new();
        loop {
            let children: Vec<_> = remaining.by_ref().take(MAX_BRANCH_CHILDREN).collect();
            if children.is_empty() {
                break;
            }
            let first = children.first().unwrap().first();
            let last = children.last().unwrap().last();
            let count = children.iter().map(SignalTree::count).sum();
            branches.push(SignalTree::Branch {
                label: range_label(signals, first, last, count),
                children,
            });
        }
        nodes = branches;
    }
    nodes
}

impl SignalTree {
    fn ordered_signums(&self, signals: &[Signal], order: &mut Vec<u32>) {
        match self {
            Self::Leaf { indices, .. } => {
                order.extend(indices.iter().map(|&i| signals[i].signum()))
            }
            Self::Branch { children, .. } => {
                for child in children {
                    child.ordered_signums(signals, order);
                }
            }
        }
    }
    fn signums(&self, signals: &[Signal]) -> HashSet<u32> {
        match self {
            Self::Leaf { indices, .. } => indices.iter().map(|&i| signals[i].signum()).collect(),
            Self::Branch { children, .. } => children
                .iter()
                .flat_map(|child| child.signums(signals))
                .collect(),
        }
    }
    fn first(&self) -> usize {
        match self {
            Self::Branch { children, .. } => children.first().unwrap().first(),
            Self::Leaf { indices, .. } => indices[0],
        }
    }

    fn last(&self) -> usize {
        match self {
            Self::Branch { children, .. } => children.last().unwrap().last(),
            Self::Leaf { indices, .. } => *indices.last().unwrap(),
        }
    }

    fn count(&self) -> usize {
        match self {
            Self::Branch { children, .. } => children.iter().map(Self::count).sum(),
            Self::Leaf { indices, .. } => indices.len(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Sample {
    pub id: u64,
    pub signum: u32,
    pub value: String,
    pub at: SystemTime,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ValueDisplay {
    #[default]
    Ascii,
    Hex,
    Mixed,
}

// Decode explicit byte notation, never guess that a bare analog number is hex.
fn serial_bytes(value: &str) -> Vec<u8> {
    let hex = |pair: &str| {
        if pair.len() == 2 && pair.bytes().all(|b| b.is_ascii_hexdigit()) {
            u8::from_str_radix(pair, 16).ok()
        } else {
            None
        }
    };
    let tokens: Vec<_> = value.split_ascii_whitespace().collect();
    // Crestron serial reports also encode bytes as adjacent [HH] groups.
    // Require the entire value to match so malformed or literal brackets survive.
    if !value.is_empty() && value.len().is_multiple_of(4) {
        let bytes: Option<Vec<_>> = value
            .as_bytes()
            .chunks_exact(4)
            .map(|group| {
                if group[0] == b'[' && group[3] == b']' {
                    std::str::from_utf8(&group[1..3]).ok().and_then(hex)
                } else {
                    None
                }
            })
            .collect();
        if let Some(bytes) = bytes {
            return bytes;
        }
    }
    if !tokens.is_empty() && (tokens.len() > 1 || tokens[0].starts_with("0x")) {
        let bytes: Option<Vec<_>> = tokens
            .iter()
            .map(|token| hex(token.strip_prefix("0x").unwrap_or(token)))
            .collect();
        if let Some(bytes) = bytes {
            return bytes;
        }
    }
    let mut bytes = Vec::with_capacity(value.len());
    let mut rest = value.as_bytes();
    while !rest.is_empty() {
        if rest.starts_with(b"\\x")
            && rest.len() >= 4
            && let Ok(pair) = std::str::from_utf8(&rest[2..4])
            && let Some(byte) = hex(pair)
        {
            bytes.push(byte);
            rest = &rest[4..];
        } else {
            bytes.push(rest[0]);
            rest = &rest[1..];
        }
    }
    bytes
}

fn display_value(value: &str, mode: ValueDisplay) -> String {
    let bytes = serial_bytes(value);
    match mode {
        ValueDisplay::Hex => bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" "),
        ValueDisplay::Ascii => bytes
            .iter()
            .map(|&byte| {
                if (0x20..=0x7e).contains(&byte) {
                    char::from(byte)
                } else {
                    '.'
                }
            })
            .collect(),
        ValueDisplay::Mixed => bytes
            .iter()
            .map(|&byte| {
                if (0x20..=0x7e).contains(&byte) {
                    char::from(byte).to_string()
                } else {
                    format!("\\x{byte:02X}")
                }
            })
            .collect(),
    }
}

#[derive(Default)]
struct SignalSelection {
    selected: HashSet<u32>,
    anchor: Option<u32>,
}

fn update_selection(
    selected: &mut HashSet<u32>,
    anchor: &mut Option<u32>,
    order: &[u32],
    target: u32,
    additive: bool,
    shift: bool,
    context: bool,
) {
    if context && selected.contains(&target) {
        return;
    }
    if shift
        && !context
        && let Some(start) = anchor.and_then(|a| order.iter().position(|&n| n == a))
        && let Some(end) = order.iter().position(|&n| n == target)
    {
        if !additive {
            selected.clear();
        }
        selected.extend(order[start.min(end)..=start.max(end)].iter().copied());
        return;
    }
    if !additive || context {
        selected.clear();
    }
    if !selected.insert(target) && additive {
        selected.remove(&target);
    }
    *anchor = Some(target);
}

pub(crate) struct Panel {
    pub open: bool,

    title: String,
    persistence: persistence::Persistence,
    pub slot: u8,
    pub fetch: Option<Receiver<Result<Vec<u8>, String>>>,
    pub awaiting_session: bool,
    pub pending_session: Option<(
        tokio::sync::mpsc::UnboundedReceiver<DebugCommand>,
        tokio::sync::mpsc::Sender<DebugEvent>,
    )>,
    pub input: Option<tokio::sync::mpsc::UnboundedSender<DebugCommand>>,
    output: tokio::sync::mpsc::Receiver<DebugEvent>,
    pub signals: Vec<Signal>,
    signal_lookup: Option<HashMap<u32, usize>>,
    pub search: String,
    tree: Option<Vec<SignalTree>>,
    tree_query: String,
    pub status: String,
    pub groups: BTreeMap<String, Vec<u32>>,
    new_group: String,
    timeline_group: Option<String>,
    samples: VecDeque<Sample>,
    paused: bool,
    pending_samples: VecDeque<Sample>,
    pending_dropped: usize,
    next_sample_id: u64,
    selected_sample: Option<u64>,
    detail_signal: Option<u32>,
    disabled_signals: HashSet<u32>,
    tree_selection: HashSet<u32>,
    tree_anchor: Option<u32>,
    watch_selections: HashMap<String, SignalSelection>,
    display_types: HashMap<u32, ValueDisplay>,
    values: HashMap<u32, String>,
    frames: Vec<u8>,
    pub running: bool,
    pub stopping: bool,
    pub session_sent: bool,
    pub cleanup_error: Option<String>,
}
impl Panel {
    pub fn reset_signals(&mut self) {
        self.suspend_settings();
        self.signals.clear();
        self.signal_lookup = None;
        self.tree = None;
        self.samples.clear();
        self.values.clear();
        self.paused = false;
        self.pending_samples.clear();
        self.pending_dropped = 0;
        self.selected_sample = None;
        self.disabled_signals.clear();
        self.detail_signal = None;
        self.frames.clear();
        self.display_types.clear();
        self.tree_selection.clear();
        self.tree_anchor = None;
        self.watch_selections.clear();
        for members in self.groups.values_mut() {
            members.clear();
        }
    }
    pub fn new(id: String, name: &str) -> Self {
        let (_, output) = tokio::sync::mpsc::channel(256);
        Self {
            open: true,
            title: format!("Signal Debugger — {name} ({id})"),
            persistence: persistence::Persistence::new(name),
            slot: 1,
            fetch: None,
            awaiting_session: false,
            pending_session: None,
            input: None,
            output,
            signals: Vec::new(),
            signal_lookup: None,
            search: String::new(),
            tree: None,
            tree_query: String::new(),
            status: "Choose a program slot and press Start".into(),
            groups: BTreeMap::from([("Watched".into(), Vec::new())]),
            new_group: String::new(),
            timeline_group: None,
            samples: VecDeque::new(),
            paused: false,
            pending_samples: VecDeque::new(),
            pending_dropped: 0,
            next_sample_id: 0,
            selected_sample: None,
            detail_signal: None,
            disabled_signals: HashSet::new(),
            tree_selection: HashSet::new(),
            tree_anchor: None,
            watch_selections: HashMap::new(),
            display_types: HashMap::new(),
            values: HashMap::new(),
            frames: Vec::new(),
            running: false,
            stopping: false,
            session_sent: false,
            cleanup_error: None,
        }
    }
    pub fn stop(&mut self) {
        self.save_settings();
        if let Some(input) = self.input.take() {
            let _ = input.send(DebugCommand::Stop);
            if self.session_sent {
                self.stopping = true;
                self.status = "Turning off debug signals…".into();
            }
        }
        self.pending_session = None;
        self.awaiting_session = false;
        self.fetch = None;
        self.running = false;
    }
    pub fn poll(&mut self) {
        let fetch_result = self
            .fetch
            .as_ref()
            .and_then(|reply| match reply.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Signature download ended unexpectedly".into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some(result) = fetch_result {
            self.fetch = None;
            match result.and_then(|bytes| parse_signals(&bytes)) {
                Ok(signals) => {
                    self.signals = signals;
                    self.signal_lookup = None;
                    self.tree = None;
                    self.status = format!("Loaded {} signals", self.signals.len());
                    self.restore_settings();
                    self.connect();
                }
                Err(error) => self.status = error,
            }
        }
        let started = std::time::Instant::now();
        for _ in 0..64 {
            if started.elapsed() >= std::time::Duration::from_millis(4) {
                break;
            }
            let event = match self.output.try_recv() {
                Ok(event) => event,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    if self.session_sent {
                        self.running = false;
                        self.stopping = false;
                        self.session_sent = false;
                        self.input = None;
                        let error =
                            "Debugger worker disconnected; signal cleanup was not confirmed";
                        self.cleanup_error = Some(error.into());
                        self.status = error.into();
                    }
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            };
            match event {
                DebugEvent::Opened => {
                    self.running = true;
                    self.status = "Connected — enabling all signals…".into();
                }
                DebugEvent::SyncComplete => {
                    self.restore_exclusions_after_sync();
                    self.status = format!(
                        "Initial sync complete — monitoring; {} exclusions restored",
                        self.disabled_signals.len()
                    );
                }
                DebugEvent::Progress {
                    processed,
                    total,
                    already_on,
                } => {
                    self.status = if processed == total {
                        format!("Monitoring {total} signals ({already_on} already in debug mode)")
                    } else {
                        format!(
                            "Enabling signals: {processed}/{total} ({already_on} already active)"
                        )
                    };
                }
                DebugEvent::Output { bytes, received_at } => self.ingest(&bytes, received_at),
                DebugEvent::Closed(error) => {
                    self.running = false;
                    self.input = None;
                    self.stopping = false;
                    self.session_sent = false;
                    if !error.is_empty() {
                        self.cleanup_error = Some(error.clone());
                    }
                    self.status = if error.is_empty() {
                        "Debugger stopped".into()
                    } else {
                        error
                    };
                }
            }
        }
    }
    fn connect(&mut self) {
        let (input, commands) = tokio::sync::mpsc::unbounded_channel();
        let (reply, output) = tokio::sync::mpsc::channel(256);
        self.input = Some(input);
        self.output = output;
        self.pending_session = Some((commands, reply));
    }
    fn ensure_signal_lookup(&mut self) {
        if self.signal_lookup.is_none() {
            let mut lookup = HashMap::with_capacity(self.signals.len());
            for (index, signal) in self.signals.iter().enumerate() {
                lookup.entry(signal.signum()).or_insert(index);
            }
            self.signal_lookup = Some(lookup);
        }
    }
    fn signal_name(&self, signum: u32) -> &str {
        self.signal_lookup
            .as_ref()
            .and_then(|lookup| lookup.get(&signum))
            .map_or("Unknown signal", |&index| self.signals[index].name.as_str())
    }
    fn timeline_indices(&self) -> Vec<usize> {
        let members: Option<HashSet<u32>> = self.timeline_group.as_ref().map(|group| {
            self.groups
                .get(group)
                .into_iter()
                .flatten()
                .copied()
                .collect()
        });
        (0..self.samples.len())
            .rev()
            .filter(|&index| {
                members
                    .as_ref()
                    .is_none_or(|members| members.contains(&self.samples[index].signum))
            })
            .collect()
    }

    fn clear_history(&mut self) {
        self.samples.clear();
        self.pending_samples.clear();
        self.pending_dropped = 0;
        self.selected_sample = None;
    }
    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if !self.paused {
            self.samples.append(&mut self.pending_samples);
            while self.samples.len() > MAX_EVENTS {
                self.samples.pop_front();
            }
            if !self
                .samples
                .iter()
                .any(|sample| Some(sample.id) == self.selected_sample)
            {
                self.selected_sample = None;
            }
        } else {
            self.pending_dropped = 0;
        }
    }
    fn ingest(&mut self, bytes: &[u8], received_at: SystemTime) {
        self.ensure_signal_lookup();
        for &byte in bytes {
            if byte == 1 {
                self.frames.clear();
                self.frames.push(byte);
                continue;
            }
            if self.frames.is_empty() {
                continue;
            }
            if self.frames.len() >= 64 * 1024 {
                self.frames.clear();
                self.status = "Debug frame too large; skipped malformed frame".into();
                continue;
            }
            self.frames.push(byte);
            // Global SYNC can return many records inside one SOH/ETX frame.
            // Flush complete lines in bounded batches rather than rejecting
            // the entire snapshot at the malformed-record size limit.
            if byte != 3 && !(byte == b'\n' && self.frames.len() >= 32 * 1024) {
                continue;
            }
            let frame = &self.frames;
            let end = if byte == 3 {
                frame.len() - 1
            } else {
                frame.len()
            };
            for line in String::from_utf8_lossy(&frame[1..end]).lines() {
                let line = line.trim_start();
                if let Some((number, value)) = line.split_once('=')
                    && let Some((number, _format)) = number.split_once(':')
                    && let Ok(signum) = u32::from_str_radix(number, 16)
                    && self.signal_lookup.as_ref().unwrap().contains_key(&signum)
                {
                    let value = value.to_owned();
                    self.values.insert(signum, value.clone());
                    // Local exclusions suppress timeline events, not the
                    // current-state cache used by the signal tree.
                    if self.disabled_signals.contains(&signum) {
                        continue;
                    }
                    let destination = if self.paused {
                        &mut self.pending_samples
                    } else {
                        &mut self.samples
                    };
                    destination.push_back(Sample {
                        id: self.next_sample_id,
                        signum,
                        value,
                        at: received_at,
                    });
                    self.next_sample_id += 1;
                    if self.pending_samples.len() > MAX_EVENTS {
                        self.pending_samples.pop_front();
                        self.pending_dropped += 1;
                    }
                    while self.samples.len() > MAX_EVENTS {
                        if self
                            .samples
                            .pop_front()
                            .is_some_and(|sample| Some(sample.id) == self.selected_sample)
                        {
                            self.selected_sample = None;
                        }
                    }
                }
            }
            self.frames.clear();
            if byte != 3 {
                self.frames.push(1);
            }
        }
    }
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        self.poll();
        if !self.output.is_empty() {
            ctx.request_repaint();
        }
        let was_open = self.open;
        let title = self.title.clone();
        self.open = crate::popout::window(ctx, &title, [1200.0, 760.0], |ui| self.contents(ui));
        if was_open && !self.open {
            self.stop();
        }
        self.open
    }

    fn show_signal_nodes(&mut self, ui: &mut egui::Ui, nodes: &[SignalTree]) {
        for node in nodes {
            let (label, children) = match node {
                SignalTree::Branch { label, children } => (label, Some(children.as_slice())),
                SignalTree::Leaf { label, .. } => (label, None),
            };
            let response = egui::CollapsingHeader::new(label)
                .id_salt(("signal_range", node.first(), node.count()))
                .default_open(!self.signal_filter().is_empty())
                .show(ui, |ui| {
                    if let Some(children) = children {
                        self.show_signal_nodes(ui, children);
                    } else if let SignalTree::Leaf { indices, .. } = node {
                        for &index in indices {
                            self.show_signal_row(ui, index);
                        }
                    }
                });
            if response.header_response.secondary_clicked() {
                self.tree_selection = node.signums(&self.signals);
            }
            response
                .header_response
                .context_menu(|ui| self.tree_context_menu(ui));
        }
    }
    fn select_tree_signal(&mut self, signum: u32, additive: bool, context: bool) {
        update_selection(
            &mut self.tree_selection,
            &mut self.tree_anchor,
            &[],
            signum,
            additive,
            false,
            context,
        );
    }

    fn add_tree_selection_to_group(&mut self, group: &str) {
        let mut selected: Vec<_> = self.tree_selection.iter().copied().collect();
        selected.sort_unstable();
        if let Some(members) = self.groups.get_mut(group) {
            for signum in selected {
                if !members.contains(&signum) {
                    members.push(signum);
                }
            }
        }
    }

    fn show_signal_row(&mut self, ui: &mut egui::Ui, index: usize) {
        // Preserve full scroll geometry, but do no text layout/decoding for
        // off-screen rows. All branches share the one parent scroll area.
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::hover());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let signum = self.signals[index].signum();
        let response = ui.interact(
            rect,
            ui.id().with(("signal_record", index)),
            egui::Sense::click(),
        );
        if response.clicked() || response.secondary_clicked() {
            let modifiers = ui.input(|input| input.modifiers);
            let additive = modifiers.command || modifiers.ctrl;
            if modifiers.shift && !response.secondary_clicked() {
                let mut order = Vec::new();
                for node in signal_tree(&self.signals, self.signal_filter()) {
                    node.ordered_signums(&self.signals, &mut order);
                }
                update_selection(
                    &mut self.tree_selection,
                    &mut self.tree_anchor,
                    &order,
                    signum,
                    additive,
                    true,
                    false,
                );
            } else {
                self.select_tree_signal(signum, additive, response.secondary_clicked());
            }
        }
        let selected = self.tree_selection.contains(&signum);
        if response.double_clicked() {
            self.detail_signal = Some(signum);
        }
        let fill = if selected {
            ui.visuals().selection.bg_fill
        } else if response.hovered() {
            ui.visuals().widgets.hovered.weak_bg_fill
        } else if index.is_multiple_of(2) {
            ui.visuals().faint_bg_color
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, 2.0, fill);
        let name = &self.signals[index].name;
        let value = self
            .values
            .get(&signum)
            .map(|value| {
                let text = display_value(
                    value,
                    self.display_types.get(&signum).copied().unwrap_or_default(),
                );
                if text.is_empty() { "\"\"".into() } else { text }
            })
            .unwrap_or_else(|| "Not received".into());
        let split = (rect.right() - 110.0).max(rect.left() + 30.0);
        let name_rect = egui::Rect::from_min_max(
            rect.min + egui::vec2(5.0, 0.0),
            egui::pos2(split - 6.0, rect.bottom()),
        );
        let value_rect = egui::Rect::from_min_max(
            egui::pos2(split + 6.0, rect.top()),
            rect.max - egui::vec2(4.0, 0.0),
        );
        let mut text = egui::RichText::new(name);
        if self.disabled_signals.contains(&signum) {
            text = text.weak().strikethrough();
        }
        ui.put(
            name_rect,
            egui::Label::new(text)
                .halign(egui::Align::Min)
                .selectable(false)
                .truncate(),
        );
        ui.painter().line_segment(
            [
                egui::pos2(split, rect.top() + 4.0),
                egui::pos2(split, rect.bottom() - 4.0),
            ],
            ui.visuals().widgets.noninteractive.bg_stroke,
        );
        let mut text = egui::RichText::new(&value).monospace();
        if !self.values.contains_key(&signum) {
            text = text.weak();
        }
        ui.put(
            value_rect,
            egui::Label::new(text)
                .halign(egui::Align::Min)
                .selectable(false)
                .truncate(),
        );
        response
            .on_hover_text(format!("{name}\n{value}"))
            .context_menu(|ui| self.tree_context_menu(ui));
    }

    fn signal_filter(&self) -> &str {
        if self.search.chars().take(3).count() >= 3 {
            &self.search
        } else {
            ""
        }
    }

    fn signal_search(&mut self, ui: &mut egui::Ui) {
        let response = ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .desired_width(ui.available_width())
                .margin(egui::Margin {
                    right: 24,
                    ..egui::Margin::symmetric(4, 2)
                })
                .hint_text("Search signals (3+ characters)"),
        );
        let rect = response.rect;
        let clear_rect = egui::Rect::from_min_max(
            egui::pos2(rect.right() - rect.height(), rect.top()),
            rect.max,
        );
        ui.add_enabled_ui(!self.search.is_empty(), |ui| {
            if ui
                .put(clear_rect, egui::Button::new("×").frame(false))
                .on_hover_text("Clear search")
                .clicked()
            {
                self.search.clear();
                response.request_focus();
            }
        });
    }
    fn tree_context_menu(&mut self, ui: &mut egui::Ui) {
        ui.weak(format!("{} selected signals", self.tree_selection.len()));
        if ui.button("Disable debugging locally").clicked() {
            self.disabled_signals
                .extend(self.tree_selection.iter().copied());
            ui.close();
        }
        if ui.button("Enable debugging locally").clicked() {
            self.disabled_signals
                .retain(|signal| !self.tree_selection.contains(signal));
            self.persistence
                .pending_disabled
                .retain(|signal| !self.tree_selection.contains(signal));
            ui.close();
        }
        ui.menu_button("Add to watch group", |ui| {
            let groups: Vec<_> = self.groups.keys().cloned().collect();
            for group in groups {
                if ui.button(&group).clicked() {
                    self.add_tree_selection_to_group(&group);
                    ui.close();
                }
            }
        });
    }
    fn contents(&mut self, ui: &mut egui::Ui) {
        self.ensure_signal_lookup();
        egui::Panel::top("debug_header").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Signal Debugger");
                ui.add_enabled_ui(
                    !self.running
                        && !self.stopping
                        && self.fetch.is_none()
                        && !self.awaiting_session,
                    |ui| {
                        egui::ComboBox::from_id_salt("debug_slot")
                            .selected_text(format!("Slot {}", self.slot))
                            .show_ui(ui, |ui| {
                                for slot in 1..=10 {
                                    ui.selectable_value(
                                        &mut self.slot,
                                        slot,
                                        format!("Slot {slot}"),
                                    );
                                }
                            });
                    },
                );
                if self.running || self.fetch.is_some() || self.awaiting_session {
                    if ui.button("Stop").clicked() {
                        self.stop();
                    }
                } else if self.stopping {
                    ui.spinner();
                } else if ui.button("Start").clicked() {
                    self.awaiting_session = true;
                    self.status = "Reading remote signature…".into();
                }
                ui.label(&self.status);
            });
            self.settings_ui(ui);
        });
        egui::Panel::left("debug_signals")
            .resizable(true)
            .default_size(350.0)
            .show(ui, |ui| {
                egui::Panel::bottom("debug_signal_selection_footer")
                    .resizable(false)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.weak(format!("{} selected", self.tree_selection.len()));
                            if ui
                                .add_enabled(
                                    !self.tree_selection.is_empty(),
                                    egui::Button::new("Clear selection"),
                                )
                                .clicked()
                            {
                                self.tree_selection.clear();
                                self.tree_anchor = None;
                            }
                        });
                    });
                ui.heading("Signals");
                ui.weak(format!(
                    "Values received: {} / {}",
                    self.values.len(),
                    self.signal_lookup.as_ref().map_or(0, HashMap::len)
                ));
                self.signal_search(ui);
                ui.horizontal(|ui| {
                    ui.add_sized([(ui.available_width() - 110.0).max(40.0), 20.0], egui::Label::new(egui::RichText::new("Signal name").strong()));
                    ui.strong("Current value");
                }).response.on_hover_text("Ctrl/Cmd-click to toggle selection · Shift-click for range · Right-click for actions");
                ui.separator();
                if self.tree.is_none() || self.tree_query != self.signal_filter() {
                    self.tree = Some(signal_tree(&self.signals, self.signal_filter()));
                    self.tree_query = self.signal_filter().to_owned();
                }
                let tree = self.tree.take().unwrap_or_default();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if tree.is_empty() && !self.signals.is_empty() {
                        ui.weak("No matching signals");
                    }
                    self.show_signal_nodes(ui, &tree);
                });
                self.tree = Some(tree);
            });
        egui::Panel::right("debug_watches")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                ui.heading("Watch groups");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.new_group).hint_text("New group"));
                    if ui.button("Add").clicked() && !self.new_group.trim().is_empty() {
                        self.groups
                            .entry(self.new_group.trim().to_owned())
                            .or_default();
                        self.timeline_group = Some(self.new_group.trim().to_owned());
                        self.new_group.clear();
                    }
                });
                ui.weak("Select a group to filter the timeline");
                ui.weak("Ctrl/Cmd-click signals to toggle · Shift-click for range");
                ui.selectable_value(&mut self.timeline_group, None, "All signals");
                let mut remove = Vec::new();
                for (group, members) in &self.groups {
                    let response = ui.selectable_value(
                        &mut self.timeline_group,
                        Some(group.clone()),
                        format!("{group} · {} signals", members.len()),
                    );
                    response.context_menu(|ui| {
                        if ui.button("Remove all signals").clicked() {
                            remove.extend(members.iter().map(|&n| (group.clone(), n)));
                            ui.close();
                        }
                    });
                }
                egui::ScrollArea::vertical()
                    .id_salt("watch_members")
                    .show(ui, |ui| {
                        for (group, members) in &self.groups.clone() {
                            if self
                                .timeline_group
                                .as_ref()
                                .is_some_and(|selected| selected != group)
                            {
                                continue;
                            }
                            ui.separator();
                            let header = egui::CollapsingHeader::new(group)
                                .id_salt(("watch_group_members", group))
                                .default_open(true)
                                .show(ui, |ui| {
                                    for &signum in members {
                                        ui.horizontal(|ui| {
                                            if ui.small_button("✕").clicked() {
                                                remove.push((group.clone(), signum));
                                            }
                                            let name = self.signal_name(signum).to_owned();
                                            let selection = self
                                                .watch_selections
                                                .entry(group.clone())
                                                .or_default();
                                            let response = ui
                                                .push_id((group, signum), |ui| {
                                                    ui.selectable_label(
                                                        selection.selected.contains(&signum),
                                                        name,
                                                    )
                                                })
                                                .inner;
                                            if response.double_clicked() {
                                                self.detail_signal = Some(signum);
                                            }
                                            if response.clicked() || response.secondary_clicked() {
                                                let modifiers = ui.input(|input| input.modifiers);
                                                update_selection(
                                                    &mut selection.selected,
                                                    &mut selection.anchor,
                                                    members,
                                                    signum,
                                                    modifiers.ctrl || modifiers.command,
                                                    modifiers.shift,
                                                    response.secondary_clicked(),
                                                );
                                            }
                                            response.context_menu(|ui| {
                                                if ui.button("Remove selected signals").clicked() {
                                                    remove.extend(
                                                        selection
                                                            .selected
                                                            .iter()
                                                            .map(|&n| (group.clone(), n)),
                                                    );
                                                    ui.close();
                                                }
                                            });
                                            if let Some(value) = self.values.get(&signum) {
                                                ui.monospace(display_value(
                                                    value,
                                                    self.display_types
                                                        .get(&signum)
                                                        .copied()
                                                        .unwrap_or_default(),
                                                ));
                                            }
                                        });
                                    }
                                });
                            header.header_response.context_menu(|ui| {
                                if ui.button("Remove all signals").clicked() {
                                    remove.extend(members.iter().map(|&n| (group.clone(), n)));
                                    ui.close();
                                }
                            });
                        }
                    });
                for (group, signum) in remove {
                    if let Some(members) = self.groups.get_mut(&group) {
                        members.retain(|&n| n != signum);
                        if let Some(selection) = self.watch_selections.get_mut(&group) {
                            selection.selected.remove(&signum);
                            if selection.anchor == Some(signum) {
                                selection.anchor = None;
                            }
                        }
                    }
                }
            });
        egui::CentralPanel::default().show(ui, |ui| {
            let mut visible = self.timeline_indices();
            ui.horizontal(|ui| {
                ui.heading("Signal timeline");
                if ui.button(if self.paused { "Resume" } else { "Pause" }).clicked() {
                    self.toggle_pause();
                    visible = self.timeline_indices();
                }
                if self.paused { ui.weak(format!("{} buffered", self.pending_samples.len())); }
                if self.pending_dropped > 0 {
                    ui.colored_label(ui.visuals().warn_fg_color, format!("{} older buffered changes discarded (20,000 limit)", self.pending_dropped));
                }
                if ui
                    .add_enabled(!self.samples.is_empty() || !self.pending_samples.is_empty(), egui::Button::new("Clear signals"))
                    .clicked()
                {
                    self.clear_history();
                    visible.clear();
                }
                ui.weak(format!("{} / {} changes", visible.len(), self.samples.len()));
                if let Some(group) = &self.timeline_group { ui.label(format!("Group: {group}")); }
                if !self.disabled_signals.is_empty() || !self.persistence.pending_disabled.is_empty() {
                    ui.label(format!("{} disabled locally", self.disabled_signals.len()));
                    if ui.small_button("Enable all locally").clicked() {
                        self.disabled_signals.clear();
                        self.persistence.pending_disabled.clear();
                    }
                }
            });
            ui.weak("Newest first · Click to select · Right-click for signal actions");
            let width = ui.available_width().max(540.0);
            let name_width = ((width - 244.0) * 0.55).max(150.0);
            let value_width = (width - name_width - 244.0).max(120.0);
            egui::ScrollArea::horizontal().id_salt("signal_columns").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_min_width(width);
            let (header, _) = ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::hover());
            ui.painter().rect_filled(header, 4.0, ui.visuals().widgets.inactive.bg_fill);
            for (offset, label) in [(6.0, "Received (local)"), (234.0, "Signal name"), (242.0 + name_width, "Value received")] {
                ui.painter().text(header.left_center() + egui::vec2(offset, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(13.0), ui.visuals().strong_text_color());
            }
            ui.separator();
            if visible.is_empty() {
                ui.add_space(24.0);
                ui.strong(if self.timeline_group.is_some() { "No changes for this watch group" } else { "Waiting for signal changes" });
                ui.weak("Received values appear here automatically while monitoring.");
            }
            let count = visible.len();
            egui::ScrollArea::vertical()
                .id_salt(("signal_changes", &self.timeline_group))
                .auto_shrink([false, false])
                .show_rows(ui, 30.0, count, |ui, rows| {
                    for row in rows {
                        let sample = &self.samples[visible[row]];
                        let signum = sample.signum;
                        let disabled = self.disabled_signals.contains(&signum);
                        let name = self.signal_name(signum).to_owned();
                        let mode = self.display_types.get(&signum).copied().unwrap_or_default();
                        let displayed = display_value(&sample.value, mode);
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 30.0), egui::Sense::hover());
                        let response = ui.interact(rect, ui.id().with(("signal_sample", sample.id)), egui::Sense::click());
                        let selected = self.selected_sample == Some(sample.id);
                        let fill = if selected { ui.visuals().selection.bg_fill }
                            else if response.hovered() { ui.visuals().widgets.hovered.bg_fill }
                            else if row % 2 == 0 { ui.visuals().faint_bg_color }
                            else { egui::Color32::TRANSPARENT };
                        ui.painter().rect_filled(rect, 4.0, fill);
                        let color = if selected { ui.visuals().selection.stroke.color } else if disabled { ui.visuals().weak_text_color() } else { ui.visuals().text_color() };
                        let cells = [
                            (0.0, 220.0, format_received(sample.at), false),
                            (228.0, name_width, name.clone(), false),
                            (236.0 + name_width, value_width, displayed.clone(), true),
                        ];
                        for (offset, cell_width, text, mono) in cells {
                            let cell = egui::Rect::from_min_size(rect.min + egui::vec2(offset + 6.0, 0.0), egui::vec2(cell_width - 12.0, 30.0));
                            ui.painter().with_clip_rect(cell.intersect(ui.clip_rect())).text(
                                cell.left_center(), egui::Align2::LEFT_CENTER, text,
                                if mono { egui::FontId::monospace(13.0) } else { egui::FontId::proportional(13.0) }, color,
                            );
                        }
                        let response = response.on_hover_text(format!("{name}\n0x{signum:08X}\n{}{}", displayed, if disabled { "\nDisabled locally; historical value" } else { "" }));
                        if response.clicked() || response.secondary_clicked() { self.selected_sample = Some(sample.id); }
                        if response.double_clicked() { self.detail_signal = Some(signum); }
                        response.context_menu(|ui| {
                            ui.strong("Signal actions");
                            ui.weak(format!("0x{signum:08X}"));
                            ui.separator();
                            ui.menu_button("Add to watch group", |ui| {
                                for (group, members) in &mut self.groups {
                                    if ui.button(group).clicked() {
                                        if !members.contains(&signum) { members.push(signum); }
                                        ui.close();
                                    }
                                }
                            });
                            ui.menu_button("Display type", |ui| {
                                for (kind, label) in [(ValueDisplay::Ascii, "ASCII"), (ValueDisplay::Hex, "Hex"), (ValueDisplay::Mixed, "Mixed")] {
                                    if ui.selectable_label(mode == kind, label).clicked() {
                                        self.display_types.insert(signum, kind);
                                        ui.ctx().request_repaint();
                                        ui.close();
                                    }
                                }
                            });
                            ui.separator();
                            let action = if disabled { "Enable debugging locally" } else { "Disable debugging locally" };
                            if ui.button(action).on_hover_text("Stops timeline recording in this window. Current values keep updating; processor flags and existing history are unchanged.").clicked() {
                                if disabled { self.disabled_signals.remove(&signum); self.persistence.pending_disabled.remove(&signum); } else { self.disabled_signals.insert(signum); }
                                ui.close();
                            }
                            if ui.button("Copy value").clicked() {
                                ui.ctx().copy_text(displayed.clone());
                                ui.close();
                            }
                        });
                    }
                });
            });
        });
        self.save_settings();
        self.show_signal_details(ui.ctx());
    }
}

#[path = "debugger_details.rs"]
mod details;

#[path = "debugger_state.rs"]
mod persistence;

fn format_received(at: SystemTime) -> String {
    let local: chrono::DateTime<chrono::Local> = at.into();
    local.format("%Y-%m-%d %H:%M:%S%.3f %:z").to_string()
}

#[cfg(test)]
#[path = "debugger_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "debugger_selection_tests.rs"]
mod selection_tests;
