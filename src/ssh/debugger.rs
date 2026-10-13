//! Read the uploaded signature and control signal-specific console debugging.
use super::*;

const MAX_SIGNATURE: usize = 16 * 1024 * 1024;

pub(super) struct Target {
    pub slot: u8,
    pub signums: Vec<u32>,
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

async fn send(channel: &russh::Channel<client::Msg>, text: &str) -> Result<(), String> {
    channel
        .data(text.as_bytes())
        .await
        .map_err(|e| format!("Debugger console write failed: {e}"))
}

/// Read the slot's global flag without changing per-signal flags.
async fn show(
    channel: &mut russh::Channel<client::Msg>,
    slot: u8,
    output: Option<&tokio::sync::mpsc::Sender<DebugEvent>>,
) -> Result<bool, String> {
    send(channel, &command(slot, "SHOW")).await?;
    let prefix = "GLOBAL:";
    let answer = tokio::time::timeout(SSH_TIMEOUT, async {
        let mut received = Vec::new();
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
                            _ if !in_frame => received.push(byte),
                            _ => {}
                        }
                    }
                    if received.len() > 16 * 1024 {
                        // Reports can arrive while asking for status. Keep the
                        // newest bytes (including a reply split across reads),
                        // without accumulating an unbounded high-rate stream.
                        received.drain(..received.len() - 16 * 1024);
                    }
                    let text = String::from_utf8_lossy(&received);
                    if let Some(position) = text.find(prefix) {
                        let value = &text[position + prefix.len()..];
                        if value.starts_with("ON") {
                            return Ok(true);
                        }
                        if value.starts_with("OFF") {
                            return Ok(false);
                        }
                    }
                }
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => {
                    return Err::<bool, String>("Debugger console closed".into());
                }
                Some(_) => {}
            }
        }
    })
    .await
    .map_err(|_| "Timed out asking for signal status".to_owned())??;
    Ok(answer)
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
    let Target { slot, signums } = target;
    let mut owned = false;
    let total = signums.len();
    let result = async {
        channel.request_pty(true, "xterm", 80, 24, 0, 0, &[]).await.map_err(|e| e.to_string())?;
        channel.request_shell(true).await.map_err(|e| e.to_string())?;
        let _ = output.try_send(DebugEvent::Opened);
        if *cancelled.borrow() || input.try_recv().is_ok() || input.is_closed() {
            return Ok(());
        }
        let already_on = show(&mut channel, slot, Some(&output)).await?;
        if !already_on {
            owned = true;
            send(&channel, &command(slot, "ON")).await?;
            if !show(&mut channel, slot, Some(&output)).await? {
                return Err("Processor did not enable global debugging".into());
            }
        }
        // A command response after SYNC fences the snapshot. Signature entries
        // (e.g. module/internal signals) need not all produce value reports.
        send(&channel, &command(slot, "SYNC")).await?;
        output.try_send(DebugEvent::Progress { processed: total, total, already_on: if already_on { total } else { 0 } }).map_err(|_| "Debugger output queue full or disconnected".to_owned())?;
        tokio::select! {
            _ = cancelled.changed() => return Ok(()),
            _ = input.recv() => return Ok(()),
            result = async {
                if !show(&mut channel, slot, Some(&output)).await? {
                    return Err("Global debugging stopped during initial synchronization".to_owned());
                }
                output.send(DebugEvent::SyncComplete).await.map_err(|_| "Debugger output receiver disconnected".to_owned())
            } => result?,
        }
        loop {
            tokio::select! {
                _ = cancelled.changed() => break,
                action = input.recv() => match action {
                    None | Some(DebugCommand::Stop) => break,
                },
                message = channel.wait() => match message {
                    Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                        let event = DebugEvent::Output {
                            bytes: data.to_vec(),
                            received_at: std::time::SystemTime::now(),
                        };
                        tokio::select! {
                            _ = cancelled.changed() => break,
                            _ = input.recv() => break,
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
    if owned {
        let cleanup = async {
            send(&channel, &command(slot, "OFF")).await?;
            if show(&mut channel, slot, None).await? {
                return Err("Global debugging remains ON".to_owned());
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = cleanup {
            failure = Some(format!(
                "{}; global debug cleanup was not confirmed: {error}",
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

    #[test]
    fn debugger_commands_are_scoped_to_slot_and_cr_terminated() {
        assert_eq!(command(1, "SHOW"), "MDBGSIGNAL:1 -A:SHOW\r");
        assert_eq!(command(3, "ON"), "MDBGSIGNAL:3 -A:ON\r");
        assert_eq!(command(3, "OFF"), "MDBGSIGNAL:3 -A:OFF\r");
        assert_eq!(command(3, "SYNC"), "MDBGSIGNAL:3 -A:SYNC\r");
    }

    #[test]
    fn global_startup_and_cleanup_are_constant_command_count() {
        global_session(false, 5975);
    }

    #[test]
    fn preexisting_global_debug_is_not_disabled() {
        global_session(true, 5975);
    }

    #[test]
    fn initial_sync_completes_when_signature_entries_do_not_report_values() {
        global_session(false, 6000);
    }

    fn global_session(initially_on: bool, signature_count: u32) {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Console {
            commands: Arc<Mutex<Vec<String>>>,
            enabled: bool,
        }
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
                self.commands.lock().unwrap().push(command.clone());
                let verb = command
                    .split("-A:")
                    .nth(1)
                    .unwrap()
                    .split(' ')
                    .next()
                    .unwrap()
                    .trim();
                match verb {
                    "ON" => {
                        self.enabled = true;
                        session.data(id, "\x01\r\n00001011:1=42\r\n\x03".to_owned())?;
                    }
                    "OFF" => {
                        self.enabled = false;
                    }
                    "SYNC" => {
                        session.data(id, "\x0100000000:1=GLOBAL:OFF\r\n\x03".to_owned())?;
                        for signum in 0..5975 {
                            session.data(id, format!("\x01\r\n{signum:08X}:1={signum}\r\n\x03"))?;
                        }
                    }
                    "SHOW" => {
                        let state = if self.enabled { "ON" } else { "OFF" };
                        session.data(id, format!("GLOBAL:{state}\r\n"))?;
                    }
                    _ => panic!("unexpected command {command:?}"),
                }
                Ok(())
            }
        }

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
                let commands = Arc::new(Mutex::new(Vec::new()));
                let server_commands = commands.clone();
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let console = Console {
                        commands: server_commands,
                        enabled: initially_on,
                    };
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
                let (output, mut receiver) = tokio::sync::mpsc::channel(256);
                let (_cancel, cancelled) = tokio::sync::watch::channel(false);
                let task = tokio::spawn(session(
                    spec,
                    Target {
                        slot: 1,
                        signums: (0..signature_count).collect(),
                    },
                    input,
                    output,
                    SharedSession::default(),
                    events,
                    cancelled,
                ));
                let mut frames = Vec::new();
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match receiver.try_recv() {
                            Ok(DebugEvent::Progress { .. }) => {}
                            Ok(DebugEvent::Output { bytes, .. }) => {
                                frames.extend(bytes);
                            }
                            Ok(DebugEvent::SyncComplete) => break,
                            Ok(DebugEvent::Closed(error)) => {
                                panic!("debugger closed early: {error}")
                            }
                            _ => tokio::time::sleep(Duration::from_millis(5)).await,
                        }
                    }
                })
                .await
                .unwrap();
                assert!(
                    initially_on
                        || frames
                            .windows(b"00001011:1=42".len())
                            .any(|slice| slice == b"00001011:1=42")
                );
                sender.send(DebugCommand::Stop).unwrap();
                let snapshot = String::from_utf8_lossy(&frames);
                for signum in 0..5975 {
                    assert!(snapshot.contains(&format!("{signum:08X}:1={signum}\r\n")));
                }
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match receiver.try_recv() {
                            Ok(DebugEvent::Closed(error)) => {
                                assert!(error.is_empty(), "{error}");
                                break;
                            }
                            _ => tokio::time::sleep(Duration::from_millis(5)).await,
                        }
                    }
                })
                .await
                .unwrap();
                task.await.unwrap();
                let commands = commands.lock().unwrap();
                let expected: Vec<_> = if initially_on {
                    vec!["SHOW", "SYNC", "SHOW"]
                } else {
                    vec!["SHOW", "ON", "SHOW", "SYNC", "SHOW", "OFF", "SHOW"]
                }
                .into_iter()
                .map(|verb| command(1, verb))
                .collect();
                assert_eq!(*commands, expected);
                drop(commands);
                server.abort();
            });
    }
}
