//! Read the uploaded signature and control signal-specific console debugging.
use super::*;
use std::collections::HashSet;

const MAX_SIGNATURE: usize = 16 * 1024 * 1024;

pub(super) struct Target {
    pub slot: u8,
    pub signals: DebugSignals,
}

pub(super) async fn read_signature(
    spec: &ConnectionSpec,
    slot: u8,
    assigned_name: Option<&str>,
    reply: Sender<Result<Vec<u8>, String>>,
    device: &SharedSession,
    events: &Sender<WorkerEvent>,
) {
    let result = async {
        if !(1..=10).contains(&slot) {
            return Err(message(spec, "Program slot must be 1 through 10"));
        }
        let (session, _) = device.acquire(spec, Announce::Quietly, events).await?;
        let sftp = match stage(spec, &session, None, events).await? {
            Staged::Ready(sftp) => sftp,
            Staged::UpToDate(_) => unreachable!("no firmware check"),
        };
        let directory = program_directory(slot);
        let result =
            tokio::time::timeout(SSH_TIMEOUT, async {
                let entries = sftp.read_dir(directory.clone()).await.map_err(|error| {
                    message(spec, format!("Could not list {directory}: {error}"))
                })?;
                let names: Vec<_> = entries.map(|entry| entry.file_name()).collect();
                let assigned = assigned_name.map(Path::new);
                let name = crate::debugger::choose_signature(slot, &names, assigned)
                    .map_err(|error| message(spec, error))?;
                let path = format!("{directory}/{name}");
                let mut file = sftp
                    .open(path.clone())
                    .await
                    .map_err(|error| message(spec, format!("Could not read {path}: {error}")))?;
                let mut bytes = Vec::new();
                let mut buffer = [0u8; CHUNK];
                loop {
                    let n = file.read(&mut buffer).await.map_err(|error| {
                        message(spec, format!("Could not read {path}: {error}"))
                    })?;
                    if n == 0 {
                        break;
                    }
                    if bytes.len() + n > MAX_SIGNATURE {
                        return Err(message(spec, "Signature exceeds 16 MiB"));
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let _ = file.close().await;
                Ok(bytes)
            })
            .await
            .unwrap_or_else(|_| Err(message(spec, "Timed out reading the remote signature")));
        let _ = sftp.close().await;
        result
    }
    .await;
    let result = result.map_err(|error| report(events, error, Announce::Quietly));
    let _ = reply.send(result);
}

fn command(slot: u8, verb: &str) -> String {
    format!("MDBGSIGNAL:{slot} -A:{verb}\r")
}

/// A processor accepted 120 signal numbers (740 characters) on one line and
/// rejected 160 (980 characters), so lines stay well inside both.
const MAX_SIGNAL_TERMS: usize = 100;
const MAX_SIGNAL_LINE: usize = 600;

/// Listing every flag of a large program takes the console several seconds.
const FLAG_TIMEOUT: Duration = Duration::from_secs(120);

/// `MDBGSIGNAL -S:<verb>` lines naming exactly these signals. Consecutive
/// numbers become `first:last` ranges, and long lists are split across lines.
fn signal_commands(slot: u8, verb: &str, signums: &[u32]) -> Vec<String> {
    let mut sorted = signums.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut terms = Vec::new();
    let mut rest = sorted.as_slice();
    while let [first, ..] = rest {
        let run = 1 + rest
            .windows(2)
            .take_while(|pair| pair[0].checked_add(1) == Some(pair[1]))
            .count();
        let last = rest[run - 1];
        terms.push(if run == 1 {
            format!("0x{first:X}")
        } else {
            format!("0x{first:X}:0x{last:X}")
        });
        rest = &rest[run..];
    }
    let prefix = format!("MDBGSIGNAL:{slot} -S:{verb}");
    let mut commands = Vec::new();
    let mut line = prefix.clone();
    let mut count = 0;
    for term in terms {
        if count == MAX_SIGNAL_TERMS || (count > 0 && line.len() + 1 + term.len() > MAX_SIGNAL_LINE)
        {
            commands.push(format!("{line}\r"));
            line = prefix.clone();
            count = 0;
        }
        line.push(' ');
        line.push_str(&term);
        count += 1;
    }
    if count > 0 {
        commands.push(format!("{line}\r"));
    }
    commands
}

/// Requests that arrive while the console is busy, applied once it is free.
#[derive(Default)]
struct Pending {
    sync: Vec<u32>,
    show_modules: Option<bool>,
    writes: VecDeque<SignalWrite>,
}

impl Pending {
    /// Keep a request for later; false for Stop or the window going away.
    fn keep(&mut self, command: Option<DebugCommand>) -> bool {
        match command {
            Some(DebugCommand::Sync(signums)) => self.sync.extend(signums),
            Some(DebugCommand::ShowModules(show)) => self.show_modules = Some(show),
            Some(DebugCommand::Write(write)) => self.writes.push_back(write),
            None | Some(DebugCommand::Stop) => return false,
        }
        true
    }
}

/// Wait for Stop or for the window to go away, keeping other requests that
/// arrive meanwhile.
async fn stopped(
    input: &mut tokio::sync::mpsc::UnboundedReceiver<DebugCommand>,
    pending: &mut Pending,
) {
    while pending.keep(input.recv().await) {}
}

async fn send(channel: &russh::Channel<client::Msg>, text: &str) -> Result<(), String> {
    channel
        .data(text.as_bytes())
        .await
        .map_err(|e| format!("Debugger console write failed: {e}"))
}

async fn send_all(channel: &russh::Channel<client::Msg>, lines: &[String]) -> Result<(), String> {
    for line in lines {
        send(channel, line).await?;
    }
    Ok(())
}

fn global_status(line: &str) -> Option<bool> {
    let line = line.trim();
    let line = if let Some((prompt, rest)) = line.split_once('>') {
        if prompt.chars().any(char::is_whitespace) {
            return None;
        }
        rest.trim()
    } else {
        line
    };
    match line {
        "GLOBAL:ON" => Some(true),
        "GLOBAL:OFF" => Some(false),
        _ => None,
    }
}

/// Read the console until the global flag's status arrives, handing every
/// other line outside a value report to `line`. What arrives is forwarded to
/// `output`, so value reports received meanwhile are kept.
async fn until_global(
    channel: &mut russh::Channel<client::Msg>,
    output: Option<&tokio::sync::mpsc::Sender<DebugEvent>>,
    timeout: Duration,
    mut line: impl FnMut(&str),
) -> Result<bool, String> {
    tokio::time::timeout(timeout, async {
        let mut text = Vec::new();
        let mut in_frame = false;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    if let Some(output) = output {
                        output
                            .send(DebugEvent::Output {
                                bytes: data.to_vec(),
                                received_at: std::time::SystemTime::now(),
                            })
                            .await
                            .map_err(|_| "Debugger output receiver disconnected".to_owned())?;
                    }
                    // A serial value containing GLOBAL:ON is not a status reply.
                    for &byte in data.iter() {
                        match byte {
                            1 => in_frame = true,
                            3 => in_frame = false,
                            _ if in_frame => {}
                            b'\r' | b'\n' => {
                                let current = String::from_utf8_lossy(&text);
                                if let Some(global) = global_status(&current) {
                                    return Ok(global);
                                }
                                line(&current);
                                text.clear();
                            }
                            // No console line is this long; never grow unbounded.
                            _ if text.len() < 4096 => text.push(byte),
                            _ => {}
                        }
                    }
                }
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => {
                    return Err("Debugger console closed".to_owned());
                }
                Some(_) => {}
            }
        }
    })
    .await
    .map_err(|_| "Timed out waiting for the processor's debug status".to_owned())?
}

/// Read the slot's global flag without changing any flags.
async fn show(
    channel: &mut russh::Channel<client::Msg>,
    slot: u8,
    output: Option<&tokio::sync::mpsc::Sender<DebugEvent>>,
) -> Result<bool, String> {
    send(channel, &command(slot, "SHOW")).await?;
    until_global(channel, output, SSH_TIMEOUT, |_| {}).await
}

/// Which of `signums` have their signal-specific debug flag on. A global
/// status request after the listing marks its end.
async fn flags_on(
    channel: &mut russh::Channel<client::Msg>,
    slot: u8,
    signums: &[u32],
    output: Option<&tokio::sync::mpsc::Sender<DebugEvent>>,
) -> Result<HashSet<u32>, String> {
    let mut on = HashSet::new();
    if signums.is_empty() {
        return Ok(on);
    }
    send_all(channel, &signal_commands(slot, "SHOW", signums)).await?;
    send(channel, &command(slot, "SHOW")).await?;
    // Lines look like `00000020:DEBUG=ON:IGNORE=OFF`, the first after a prompt.
    until_global(channel, output, FLAG_TIMEOUT, |line| {
        if let Some(end) = line.find(":DEBUG=ON")
            && let Some(number) = end.checked_sub(8).and_then(|start| line.get(start..end))
            && let Ok(signum) = u32::from_str_radix(number, 16)
        {
            on.insert(signum);
        }
    })
    .await?;
    Ok(on)
}

/// SETSIGNAL executes asynchronously in the program. Read back after a short
/// delay, preserving received reports rather than inventing a local value.
async fn write_signal(
    channel: &mut russh::Channel<client::Msg>,
    slot: u8,
    write: &SignalWrite,
    output: Option<&tokio::sync::mpsc::Sender<DebugEvent>>,
) -> Result<(), String> {
    let text = write.command(slot)?;
    tokio::time::timeout(SSH_TIMEOUT, async {
        send(channel, &text).await?;
        tokio::time::sleep(Duration::from_millis(75)).await;
        send_all(channel, &signal_commands(slot, "SYNC", &[write.signum])).await?;
        send(channel, &command(slot, "SHOW")).await?;
        let mut error = None;
        until_global(channel, output, SSH_TIMEOUT, |line| {
            let lower = line.to_ascii_lowercase();
            // With console echo enabled the serial argument can contain
            // words such as "error"; that is not a processor rejection.
            if lower.contains("setsignal:") || lower.contains("mdbgsignal:") {
                return;
            }
            if lower.contains("error")
                || lower.contains("invalid")
                || lower.contains("bad or incomplete")
                || lower.contains("out of range")
            {
                error = Some(line.trim().to_owned());
            }
        })
        .await?;
        error.map_or(Ok(()), Err)
    })
    .await
    .map_err(|_| "Timed out sending signal value; its state is uncertain".to_owned())?
}

pub(super) async fn session(
    spec: ConnectionSpec,
    target: Target,
    mut input: tokio::sync::mpsc::UnboundedReceiver<DebugCommand>,
    output: tokio::sync::mpsc::Sender<DebugEvent>,
    device: SharedSession,
    events: Sender<WorkerEvent>,
    mut cancelled: tokio::sync::watch::Receiver<bool>,
) {
    let (session, _) = match device.acquire(&spec, Announce::Quietly, &events).await {
        Ok(ready) => ready,
        Err(error) => {
            let _ = output.try_send(DebugEvent::Closed(report(
                &events,
                error,
                Announce::Quietly,
            )));
            return;
        }
    };
    let mut channel = match session.channel_open_session().await {
        Ok(channel) => channel,
        Err(error) => {
            let _ = output.try_send(DebugEvent::Closed(error.to_string()));
            return;
        }
    };
    let Target {
        slot,
        signals:
            DebugSignals {
                signals,
                modules,
                mut show_modules,
            },
    } = target;
    // With global debugging already on every signal reports and nothing is
    // changed. Otherwise only the wanted signals' own flags are turned on,
    // which leaves module signals unreported until they are asked for.
    let mut global = false;
    // Flags found on, which belong to someone else and are left alone.
    let mut preexisting = HashSet::new();
    // Flags this session turned on, and so turns off again.
    let mut enabled = HashSet::new();
    let mut pending = Pending::default();
    let mut held = HashSet::new();
    let result = async {
        channel.request_pty(true, "xterm", 80, 24, 0, 0, &[]).await.map_err(|e| e.to_string())?;
        channel.request_shell(true).await.map_err(|e| e.to_string())?;
        let _ = output.try_send(DebugEvent::Opened);
        if *cancelled.borrow() {
            return Ok(());
        }
        loop {
            match input.try_recv() {
                Ok(command) => {
                    if !pending.keep(Some(command)) {
                        return Ok(());
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return Ok(()),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            }
        }
        if let Some(show) = pending.show_modules.take() {
            show_modules = show;
        }
        global = show(&mut channel, slot, Some(&output)).await?;
        if !global {
            let all: Vec<u32> = signals.iter().chain(&modules).copied().collect();
            preexisting = flags_on(&mut channel, slot, &all, Some(&output)).await?;
            let wanted: Vec<u32> = signals
                .iter()
                .chain(modules.iter().filter(|_| show_modules))
                .filter(|signum| !preexisting.contains(signum))
                .copied()
                .collect();
            send_all(&channel, &signal_commands(slot, "ON", &wanted)).await?;
            enabled.extend(wanted);
        }
        let reporting = |show_modules: bool, preexisting: &HashSet<u32>| {
            if global {
                signals.len() + modules.len()
            } else {
                signals.len()
                    + modules
                        .iter()
                        .filter(|signum| show_modules || preexisting.contains(signum))
                        .count()
            }
        };
        // A command response after SYNC fences the snapshot. Signature entries
        // (e.g. module/internal signals) need not all produce value reports.
        send(&channel, &command(slot, "SYNC")).await?;
        output
            .try_send(DebugEvent::Monitoring { global, reporting: reporting(show_modules, &preexisting) })
            .map_err(|_| "Debugger output queue full or disconnected".to_owned())?;
        tokio::select! {
            _ = cancelled.changed() => return Ok(()),
            _ = stopped(&mut input, &mut pending) => return Ok(()),
            result = async {
                show(&mut channel, slot, Some(&output)).await?;
                output.send(DebugEvent::SyncComplete).await.map_err(|_| "Debugger output receiver disconnected".to_owned())
            } => result?,
        }
        loop {
            if let Some(show) = pending.show_modules.take()
                && show != show_modules
            {
                show_modules = show;
                if !global {
                    if show {
                        let on: Vec<u32> = modules
                            .iter()
                            .filter(|signum| !preexisting.contains(signum) && !enabled.contains(signum))
                            .copied()
                            .collect();
                        send_all(&channel, &signal_commands(slot, "ON", &on)).await?;
                        enabled.extend(on);
                    } else {
                        let off: Vec<u32> = modules.iter().filter(|signum| enabled.contains(signum)).copied().collect();
                        send_all(&channel, &signal_commands(slot, "OFF", &off)).await?;
                        for signum in off {
                            enabled.remove(&signum);
                        }
                    }
                }
                let _ = output.try_send(DebugEvent::Monitoring { global, reporting: reporting(show_modules, &preexisting) });
            }
            // Replies arrive as ordinary value reports on this channel.
            send_all(&channel, &signal_commands(slot, "SYNC", &std::mem::take(&mut pending.sync))).await?;
            if let Some(write) = pending.writes.pop_front() {
                if !signals.contains(&write.signum) && !modules.contains(&write.signum) {
                    let _ = output.try_send(DebugEvent::WriteResult { signum: write.signum, result: Err("Signal is not in this session's signature".into()) });
                    continue;
                }
                if matches!(write.value, SignalValue::Hold) {
                    // Track before sending: cancellation can occur after the
                    // processor received High but before its reply arrived.
                    held.insert(write.signum);
                }
                let result = tokio::select! {
                    _ = cancelled.changed() => break,
                    _ = stopped(&mut input, &mut pending) => break,
                    result = write_signal(&mut channel, slot, &write, Some(&output)) => result,
                };
                if result.is_ok() && matches!(write.value, SignalValue::Release | SignalValue::Digital(_) | SignalValue::Pulse(_)) {
                    held.remove(&write.signum);
                }
                let failure = result.as_ref().err().cloned();
                tokio::select! {
                    _ = cancelled.changed() => break,
                    _ = stopped(&mut input, &mut pending) => break,
                    sent = output.send(DebugEvent::WriteResult { signum: write.signum, result }) => if sent.is_err() { break; },
                }
                if let Some(error) = failure {
                    // A failed fence leaves command ordering uncertain. Stop
                    // rather than send more user writes on that console.
                    return Err(format!("Signal write failed: {error}"));
                }
                continue;
            }
            tokio::select! {
                _ = cancelled.changed() => break,
                action = input.recv() => if !pending.keep(action) { break },
                message = channel.wait() => match message {
                    Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                        let event = DebugEvent::Output {
                            bytes: data.to_vec(),
                            received_at: std::time::SystemTime::now(),
                        };
                        tokio::select! {
                            _ = cancelled.changed() => break,
                            _ = stopped(&mut input, &mut pending) => break,
                            result = output.send(event) => if result.is_err() { break; },
                        }
                    }
                    Some(ChannelMsg::Close | ChannelMsg::Eof) | None => return Err("Debugger console disconnected".into()),
                    Some(_) => {}
                },

            }
        }
        Ok(())
    }.await;
    let mut failure = result.err();
    for signum in held {
        let release = SignalWrite {
            signum,
            value: SignalValue::Release,
        };
        if let Err(error) = write_signal(&mut channel, slot, &release, None).await {
            failure = Some(format!(
                "{}; held signal 0x{signum:08X} release could not be confirmed: {error}",
                failure.unwrap_or_else(|| "Debugger stopped".into())
            ));
        }
    }
    if !enabled.is_empty() {
        let cleanup = async {
            let enabled: Vec<u32> = enabled.iter().copied().collect();
            send_all(&channel, &signal_commands(slot, "OFF", &enabled)).await?;
            let remaining = flags_on(&mut channel, slot, &enabled, None).await?;
            if !remaining.is_empty() {
                return Err(format!("{} signals remain in debug mode", remaining.len()));
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = cleanup {
            failure = Some(format!(
                "{}; signal debug cleanup was not confirmed: {error}",
                failure.unwrap_or_else(|| "Debugger stopped".into())
            ));
        }
    }
    let _ = channel.close().await;
    let _ = tokio::time::timeout(
        SSH_TIMEOUT,
        output.send(DebugEvent::Closed(failure.unwrap_or_default())),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn debugger_commands_are_scoped_to_slot_and_cr_terminated() {
        assert_eq!(command(1, "SHOW"), "MDBGSIGNAL:1 -A:SHOW\r");
        assert_eq!(command(3, "ON"), "MDBGSIGNAL:3 -A:ON\r");
        assert_eq!(command(3, "OFF"), "MDBGSIGNAL:3 -A:OFF\r");
        assert_eq!(command(3, "SYNC"), "MDBGSIGNAL:3 -A:SYNC\r");
    }

    #[test]
    fn echoed_serial_text_cannot_be_mistaken_for_a_command_fence() {
        assert_eq!(global_status("GLOBAL:ON"), Some(true));
        assert_eq!(global_status(" RMC3>GLOBAL:OFF "), Some(false));
        for text in [
            "SETSIGNAL:1 0x1 \"GLOBAL:ON\"",
            "RMC3>SETSIGNAL:1 0x1 \"GLOBAL:OFF\"",
            "SETSIGNAL:1 0x1 \">GLOBAL:ON\"",
        ] {
            assert_eq!(global_status(text), None);
        }
    }

    #[test]
    fn signal_commands_name_only_the_requested_signals_in_bounded_lines() {
        assert_eq!(
            signal_commands(2, "SYNC", &[0x14, 0x10, 0x11, 0x12, 0x12, 0x20]),
            ["MDBGSIGNAL:2 -S:SYNC 0x10:0x12 0x14 0x20\r"]
        );
        assert_eq!(signal_commands(1, "ON", &[7]), ["MDBGSIGNAL:1 -S:ON 0x7\r"]);
        assert!(signal_commands(1, "OFF", &[]).is_empty());
        assert_eq!(
            signal_commands(1, "SHOW", &[u32::MAX - 1, u32::MAX]),
            ["MDBGSIGNAL:1 -S:SHOW 0xFFFFFFFE:0xFFFFFFFF\r"]
        );

        // Every other number, so nothing merges into a range.
        let scattered: Vec<u32> = (0..1_000).map(|n| 0x1000 + n * 2).collect();
        let lines = signal_commands(3, "SYNC", &scattered);
        let mut named = Vec::new();
        for line in &lines {
            assert!(
                line.len() <= MAX_SIGNAL_LINE + 1,
                "{} characters",
                line.len()
            );
            let terms: Vec<_> = line
                .strip_prefix("MDBGSIGNAL:3 -S:SYNC ")
                .and_then(|rest| rest.strip_suffix('\r'))
                .unwrap()
                .split(' ')
                .collect();
            assert!(terms.len() <= MAX_SIGNAL_TERMS);
            named.extend(
                terms
                    .iter()
                    .map(|term| u32::from_str_radix(term.trim_start_matches("0x"), 16).unwrap()),
            );
        }
        assert_eq!(named, scattered);
        // A whole contiguous block is one short line however many it covers.
        let block: Vec<u32> = (0x20..0x7000).collect();
        assert_eq!(
            signal_commands(1, "SYNC", &block),
            ["MDBGSIGNAL:1 -S:SYNC 0x20:0x6FFF\r"]
        );
    }

    const SIGNALS: u32 = 5975;
    const MODULE: u32 = 0x9000;

    fn signals(show_modules: bool) -> DebugSignals {
        DebugSignals {
            signals: (0..SIGNALS).collect(),
            modules: (MODULE..MODULE + 0x100).collect(),
            show_modules,
        }
    }

    fn set(signums: impl IntoIterator<Item = u32>) -> HashSet<u32> {
        signums.into_iter().collect()
    }

    /// What the simulated console holds.
    #[derive(Default)]
    struct State {
        global: bool,
        flags: HashSet<u32>,
        commands: Vec<String>,
        values: std::collections::HashMap<u32, String>,
        /// Ignore `-S:OFF`, as a console that lost the request would.
        stuck: bool,
        reject_release: bool,
    }

    struct Scenario {
        global: bool,
        preexisting: HashSet<u32>,
        signals: DebugSignals,
        /// Ask for signals before the session opens, which must wait for the
        /// initial synchronization rather than be taken as Stop, and again
        /// while monitoring.
        sync: bool,
        /// Ticks and unticks of Show module signals while monitoring.
        toggles: Vec<bool>,
        stuck: bool,
        writes: Vec<SignalWrite>,
        shutdown: u8,
        reject_release: bool,
    }

    impl Default for Scenario {
        fn default() -> Self {
            Self {
                global: false,
                preexisting: HashSet::new(),
                signals: signals(false),
                sync: false,
                toggles: Vec::new(),
                stuck: false,
                writes: Vec::new(),
                shutdown: 0,
                reject_release: false,
            }
        }
    }

    struct Outcome {
        /// Flags on once the initial synchronization completed.
        monitoring: HashSet<u32>,
        /// Flags on after each toggle.
        toggled: Vec<HashSet<u32>>,
        reports: Vec<(bool, usize)>,
        closed: String,
        state: State,
    }

    fn label(command: &str) -> String {
        command.trim_end_matches('\r').to_owned()
    }

    #[test]
    fn failed_held_release_is_reported_while_debug_flags_are_still_cleaned_up() {
        let outcome = run(Scenario {
            writes: vec![SignalWrite {
                signum: 3,
                value: SignalValue::Hold,
            }],
            reject_release: true,
            ..Default::default()
        });
        assert!(outcome.closed.contains("release could not be confirmed"));
        assert!(outcome.closed.contains("release rejected"));
        assert_eq!(outcome.state.values[&3], "1");
        assert!(outcome.state.flags.is_empty());
    }

    #[test]
    fn typed_writes_reach_console_and_held_signals_release_on_every_shutdown_path() {
        for shutdown in 0..3 {
            let values = [
                SignalValue::Digital(true),
                SignalValue::Digital(false),
                SignalValue::Pulse(100),
                SignalValue::Analog(65535),
                SignalValue::Serial("a\"\r\nb".into()),
                SignalValue::Hold,
                SignalValue::Release,
                SignalValue::Hold,
            ];
            let writes: Vec<_> = values
                .into_iter()
                .map(|value| SignalWrite { signum: 3, value })
                .collect();
            let mut expected: Vec<_> = writes.iter().map(|w| w.command(1).unwrap()).collect();
            expected.push(
                SignalWrite {
                    signum: 3,
                    value: SignalValue::Release,
                }
                .command(1)
                .unwrap(),
            );
            let outcome = run(Scenario {
                writes,
                shutdown,
                ..Default::default()
            });
            assert_eq!(outcome.closed, "");
            assert_eq!(outcome.state.values[&3], "0");
            assert_eq!(
                outcome
                    .state
                    .commands
                    .iter()
                    .filter(|c| c.starts_with("SETSIGNAL"))
                    .cloned()
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(outcome.state.flags.is_empty());
        }
    }

    #[test]
    fn only_non_module_signals_are_turned_on_and_exactly_those_turned_off() {
        let outcome = run(Scenario {
            preexisting: set([5, MODULE + 5]),
            ..Default::default()
        });
        assert_eq!(outcome.closed, "");
        assert_eq!(outcome.monitoring, set((0..SIGNALS).chain([MODULE + 5])));
        // Flags someone else set stay; global debugging is never touched.
        assert_eq!(outcome.state.flags, set([5, MODULE + 5]));
        assert!(!outcome.state.global);
        assert_eq!(outcome.reports, [(false, SIGNALS as usize + 1)]);
        let commands: Vec<_> = outcome.state.commands.iter().map(|c| label(c)).collect();
        assert_eq!(
            commands,
            [
                "MDBGSIGNAL:1 -A:SHOW",
                "MDBGSIGNAL:1 -S:SHOW 0x0:0x1756 0x9000:0x90FF",
                "MDBGSIGNAL:1 -A:SHOW",
                "MDBGSIGNAL:1 -S:ON 0x0:0x4 0x6:0x1756",
                "MDBGSIGNAL:1 -A:SYNC",
                "MDBGSIGNAL:1 -A:SHOW",
                "MDBGSIGNAL:1 -S:OFF 0x0:0x4 0x6:0x1756",
                "MDBGSIGNAL:1 -S:SHOW 0x0:0x4 0x6:0x1756",
                "MDBGSIGNAL:1 -A:SHOW",
            ]
        );
    }

    #[test]
    fn module_signals_are_reported_only_while_shown() {
        let outcome = run(Scenario {
            preexisting: set([MODULE + 5]),
            toggles: vec![true, false],
            ..Default::default()
        });
        assert_eq!(outcome.closed, "");
        let plain = set((0..SIGNALS).chain([MODULE + 5]));
        assert_eq!(outcome.monitoring, plain);
        assert_eq!(
            outcome.toggled[0],
            set((0..SIGNALS).chain(MODULE..MODULE + 0x100))
        );
        // Unticking turns off only what ticking turned on.
        assert_eq!(outcome.toggled[1], plain);
        assert_eq!(outcome.state.flags, set([MODULE + 5]));
        assert_eq!(
            outcome.reports,
            [
                (false, SIGNALS as usize + 1),
                (false, SIGNALS as usize + 0x100),
                (false, SIGNALS as usize + 1)
            ]
        );
    }

    #[test]
    fn module_signals_shown_from_the_start_are_turned_on_and_off() {
        let outcome = run(Scenario {
            signals: signals(true),
            ..Default::default()
        });
        assert_eq!(outcome.closed, "");
        assert_eq!(
            outcome.monitoring,
            set((0..SIGNALS).chain(MODULE..MODULE + 0x100))
        );
        assert!(outcome.state.flags.is_empty());
    }

    #[test]
    fn preexisting_global_debug_changes_no_flags() {
        let outcome = run(Scenario {
            global: true,
            preexisting: set([7]),
            toggles: vec![true, false],
            ..Default::default()
        });
        assert_eq!(outcome.closed, "");
        assert!(outcome.state.global);
        assert_eq!(outcome.state.flags, set([7]));
        let total = SIGNALS as usize + 0x100;
        assert_eq!(
            outcome.reports,
            [(true, total), (true, total), (true, total)]
        );
        let commands: Vec<_> = outcome.state.commands.iter().map(|c| label(c)).collect();
        assert_eq!(
            commands,
            [
                "MDBGSIGNAL:1 -A:SHOW",
                "MDBGSIGNAL:1 -A:SYNC",
                "MDBGSIGNAL:1 -A:SHOW"
            ]
        );
    }

    #[test]
    fn requested_signals_are_synchronized_without_stopping_the_session() {
        for global in [false, true] {
            let outcome = run(Scenario {
                global,
                sync: true,
                ..Default::default()
            });
            assert_eq!(outcome.closed, "");
            let commands: Vec<_> = outcome.state.commands.iter().map(|c| label(c)).collect();
            let syncs: Vec<_> = commands
                .iter()
                .filter(|command| command.contains("-S:SYNC"))
                .collect();
            assert_eq!(
                syncs,
                [
                    "MDBGSIGNAL:1 -S:SYNC 0x9000:0x9001",
                    "MDBGSIGNAL:1 -S:SYNC 0x9002 0x9010"
                ]
            );
            // Both after the initial synchronization's closing status request.
            let fence = commands
                .iter()
                .position(|command| command.ends_with("-A:SYNC"))
                .unwrap()
                + 1;
            assert!(commands[fence].ends_with("-A:SHOW"));
            assert!(commands[fence + 1].contains("-S:SYNC"));
        }
    }

    #[test]
    fn initial_sync_completes_when_signature_entries_do_not_report_values() {
        let outcome = run(Scenario {
            signals: DebugSignals {
                signals: (0..6000).collect(),
                ..signals(false)
            },
            ..Default::default()
        });
        assert_eq!(outcome.closed, "");
        assert!(outcome.state.flags.is_empty());
    }

    #[test]
    fn flags_left_on_are_reported_as_unconfirmed_cleanup() {
        let outcome = run(Scenario {
            stuck: true,
            ..Default::default()
        });
        assert!(
            outcome
                .closed
                .contains(&format!("{SIGNALS} signals remain in debug mode")),
            "{}",
            outcome.closed
        );
    }

    fn signums(terms: &str) -> Vec<u32> {
        let number = |text: &str| u32::from_str_radix(text.trim_start_matches("0x"), 16).unwrap();
        terms
            .split(' ')
            .flat_map(|term| {
                let (first, last) = term.split_once(':').unwrap_or((term, term));
                number(first)..=number(last)
            })
            .collect()
    }

    #[derive(Clone)]
    struct Console(Arc<Mutex<State>>);

    impl russh::server::Handler for Console {
        type Error = russh::Error;

        async fn auth_password(
            &mut self,
            _: &str,
            _: &str,
        ) -> Result<russh::server::Auth, Self::Error> {
            Ok(russh::server::Auth::Accept)
        }
        async fn channel_open_session(
            &mut self,
            _: russh::Channel<russh::server::Msg>,
            reply: russh::server::ChannelOpenHandle,
            _: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }
        async fn pty_request(
            &mut self,
            id: russh::ChannelId,
            _: &str,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            _: &[(russh::Pty, u32)],
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(id)?;
            Ok(())
        }
        async fn shell_request(
            &mut self,
            id: russh::ChannelId,
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(id)?;
            Ok(())
        }
        async fn data(
            &mut self,
            id: russh::ChannelId,
            data: &[u8],
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            let command = String::from_utf8_lossy(data).into_owned();
            let mut state = self.0.lock().unwrap();
            state.commands.push(command.clone());
            let line = command.trim_end_matches('\r');
            if let Some(rest) = line.strip_prefix("SETSIGNAL:1 ") {
                let (number, value) = rest.split_once(' ').unwrap();
                let signum = u32::from_str_radix(number.trim_start_matches("0x"), 16).unwrap();
                if state.reject_release && value == "0" {
                    session.data(id, "\r\nERROR: release rejected\r\n".to_owned())?;
                    return Ok(());
                }
                state.values.insert(signum, value.to_owned());
                session.data(id, format!("\x01\r\n{signum:08X}:1={value}\r\n\x03"))?;
                return Ok(());
            }
            if let Some(rest) = line.strip_prefix("MDBGSIGNAL:1 -S:") {
                let (verb, terms) = rest.split_once(' ').unwrap();
                let signums = signums(terms);
                match verb {
                    "ON" => state.flags.extend(signums),
                    "OFF" if state.stuck => {}
                    "OFF" => {
                        for signum in signums {
                            state.flags.remove(&signum);
                        }
                    }
                    "SHOW" => {
                        // The console runs the first line on from its prompt,
                        // and a value report can arrive in the middle.
                        let mut reply = String::from("\r\n\r\nCP3>");
                        for (index, signum) in signums.iter().enumerate() {
                            let on = if state.flags.contains(signum) {
                                "ON"
                            } else {
                                "OFF"
                            };
                            reply.push_str(&format!("{signum:08X}:DEBUG={on}:IGNORE=OFF\r\n"));
                            if index == 1 {
                                reply.push_str("\x01\r\n00000002:1=GLOBAL:ON\r\n\x03");
                            }
                        }
                        session.data(id, reply)?;
                    }
                    "SYNC" => {
                        for signum in signums {
                            session.data(id, format!("\x01\r\n{signum:08X}:1=synced\r\n\x03"))?;
                        }
                    }
                    _ => panic!("unexpected command {command:?}"),
                }
                return Ok(());
            }
            match line.strip_prefix("MDBGSIGNAL:1 -A:") {
                Some("ON") => {
                    state.global = true;
                    session.data(id, "\x01\r\n00001011:1=42\r\n\x03".to_owned())?;
                }
                Some("OFF") => state.global = false,
                Some("SYNC") => {
                    session.data(id, "\x0100000000:1=GLOBAL:OFF\r\n\x03".to_owned())?;
                    for signum in 0..SIGNALS {
                        session.data(id, format!("\x01\r\n{signum:08X}:1={signum}\r\n\x03"))?;
                    }
                }
                Some("SHOW") => {
                    let global = if state.global { "ON" } else { "OFF" };
                    session.data(id, format!("\r\nCP3>\r\nGLOBAL:{global}\r\n"))?;
                }
                _ => panic!("unexpected command {command:?}"),
            }
            Ok(())
        }
    }

    fn run(scenario: Scenario) -> Outcome {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let key = russh::keys::PrivateKey::from(
                    russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[17; 32]),
                );
                let fingerprint = host_fingerprint(key.public_key()).unwrap();
                let config = Arc::new(russh::server::Config {
                    keys: vec![key],
                    auth_rejection_time: Duration::ZERO,
                    ..Default::default()
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = listener.local_addr().unwrap().port();
                let state = Arc::new(Mutex::new(State {
                    global: scenario.global,
                    flags: scenario.preexisting.clone(),
                    stuck: scenario.stuck,
                    reject_release: scenario.reject_release,
                    ..Default::default()
                }));
                let console = Console(state.clone());
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let running = russh::server::run_stream(config, stream, console)
                        .await
                        .unwrap();
                    let _ = running.await;
                });
                let spec = ConnectionSpec {
                    id: "test:debug".into(),
                    host: "127.0.0.1".into(),
                    port,
                    credentials: Credentials {
                        username: "test".into(),
                        password: "test".into(),
                    },
                    trusted_fingerprint: Some(fingerprint),
                };
                let (events, _event_rx) = mpsc::channel();
                let (sender, input) = tokio::sync::mpsc::unbounded_channel();
                if scenario.sync {
                    sender
                        .send(DebugCommand::Sync(vec![0x9001, 0x9000]))
                        .unwrap();
                }
                let (output, mut receiver) = tokio::sync::mpsc::channel(256);
                let (cancel, cancelled) = tokio::sync::watch::channel(false);
                let task = tokio::spawn(session(
                    spec,
                    Target {
                        slot: 1,
                        signals: scenario.signals,
                    },
                    input,
                    output,
                    SharedSession::default(),
                    events,
                    cancelled,
                ));
                let mut frames = Vec::new();
                let mut reports = Vec::new();
                // Until the event `done` accepts, keeping value reports.
                async fn until(
                    receiver: &mut tokio::sync::mpsc::Receiver<DebugEvent>,
                    frames: &mut Vec<u8>,
                    reports: &mut Vec<(bool, usize)>,
                    mut done: impl FnMut(&DebugEvent, &[u8]) -> bool,
                ) -> DebugEvent {
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let event = receiver.recv().await.expect("debugger events");
                            match &event {
                                DebugEvent::Output { bytes, .. } => frames.extend(bytes),
                                DebugEvent::Monitoring { global, reporting } => {
                                    reports.push((*global, *reporting))
                                }
                                _ => {}
                            }
                            if done(&event, frames) {
                                return event;
                            }
                            if let DebugEvent::Closed(error) = event {
                                panic!("debugger closed early: {error}");
                            }
                        }
                    })
                    .await
                    .expect("debugger event in time")
                }
                until(&mut receiver, &mut frames, &mut reports, |event, _| {
                    matches!(event, DebugEvent::SyncComplete)
                })
                .await;
                let snapshot = String::from_utf8_lossy(&frames).into_owned();
                for signum in 0..SIGNALS {
                    assert!(snapshot.contains(&format!("{signum:08X}:1={signum}\r\n")));
                }
                let monitoring = state.lock().unwrap().flags.clone();
                let mut toggled = Vec::new();
                for show in scenario.toggles {
                    let before = reports.len();
                    sender.send(DebugCommand::ShowModules(show)).unwrap();
                    until(&mut receiver, &mut frames, &mut reports, |event, _| {
                        matches!(event, DebugEvent::Monitoring { .. })
                    })
                    .await;
                    assert_eq!(reports.len(), before + 1);
                    // The commands were written before the report; give the
                    // console a moment to read them.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    toggled.push(state.lock().unwrap().flags.clone());
                }
                if scenario.sync {
                    sender
                        .send(DebugCommand::Sync(vec![0x9010, 0x9002]))
                        .unwrap();
                    let wanted = [0x9000, 0x9001, 0x9002, 0x9010]
                        .map(|signum| format!("{signum:08X}:1=synced\r\n"));
                    until(&mut receiver, &mut frames, &mut reports, |_, frames| {
                        let text = String::from_utf8_lossy(frames);
                        wanted.iter().all(|reply| text.contains(reply))
                    })
                    .await;
                }
                for write in scenario.writes {
                    sender.send(DebugCommand::Write(write)).unwrap();
                    let result = until(&mut receiver, &mut frames, &mut reports, |event, _| {
                        matches!(event, DebugEvent::WriteResult { .. })
                    })
                    .await;
                    assert!(
                        matches!(result, DebugEvent::WriteResult { result: Ok(()), .. }),
                        "{result:?}"
                    );
                }
                match scenario.shutdown {
                    1 => drop(sender),
                    2 => {
                        cancel.send(true).unwrap();
                    }
                    _ => sender.send(DebugCommand::Stop).unwrap(),
                }
                let closed = match until(&mut receiver, &mut frames, &mut reports, |event, _| {
                    matches!(event, DebugEvent::Closed(_))
                })
                .await
                {
                    DebugEvent::Closed(error) => error,
                    _ => unreachable!(),
                };
                task.await.unwrap();
                server.abort();
                let state = std::mem::take(&mut *state.lock().unwrap());
                Outcome {
                    monitoring,
                    toggled,
                    reports,
                    closed,
                    state,
                }
            })
    }
}
