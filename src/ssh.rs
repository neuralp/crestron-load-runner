use std::{
    collections::HashMap,
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use russh::{
    ChannelMsg, Disconnect,
    client::{self, Handle},
    keys::{PublicKey, PublicKeyOrCertificate},
};
use russh_sftp::client::SftpSession;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    device_log::Direction,
    model::{Credentials, DeviceDetails},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
const SSH_TIMEOUT: Duration = Duration::from_secs(20);
const LOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// How a PUF update is applied and how its outcome is asked for.
const PUF_COMMAND: &str = "puf";
const PUF_RESULTS_COMMAND: &str = "puf -results";

/// How long the PUF sequence waits at each step. Named rather than inlined so
/// that a test can drive the same code without waiting out a real restart.
#[derive(Clone, Copy, Debug)]
struct Timings {
    /// Between attempts to reach the restarting device.
    poll: Duration,
    /// How long the device has altogether to come back.
    limit: Duration,
    /// Between attempts to get an answer out of a console that has just booted.
    settle: Duration,
    attempts: usize,
}

impl Timings {
    const DEVICE: Self = Self {
        poll: Duration::from_secs(10),
        limit: Duration::from_secs(900),
        settle: Duration::from_secs(5),
        attempts: 6,
    };
}
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ConnectionSpec {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub credentials: Credentials,
    pub trusted_fingerprint: Option<String>,
}

#[derive(Debug)]
pub enum WorkerCommand {
    Refresh(ConnectionSpec),
    RunScript {
        connection: ConnectionSpec,
        name: String,
        commands: Vec<String>,
    },
    UploadProgram {
        connection: ConnectionSpec,
        local_path: PathBuf,
        /// The program's `.sig` file, when one sits beside it. Uploaded with
        /// the program before the load command runs.
        signature: Option<PathBuf>,
        slot: u8,
    },
    UploadTouchpanel {
        connection: ConnectionSpec,
        local_path: PathBuf,
    },
    UploadConfig {
        connection: ConnectionSpec,
        local_path: PathBuf,
    },
    UploadFirmware {
        connection: ConnectionSpec,
        local_path: PathBuf,
        remote_name: String,
    },
    Stop,
}

#[derive(Debug)]
pub enum WorkerEvent {
    Connecting {
        id: String,
    },
    HostKeyUnknown {
        id: String,
        fingerprint: String,
    },
    Details {
        id: String,
        details: DeviceDetails,
    },
    Progress {
        id: String,
        sent: u64,
        total: u64,
    },
    /// Where a long operation has got to, when there are no bytes to count.
    Status {
        id: String,
        message: String,
        progress: Option<(u64, u64)>,
    },
    Complete {
        id: String,
        message: String,
    },
    Error {
        id: String,
        message: String,
    },
    Log {
        id: String,
        direction: Direction,
        text: String,
    },
    JobFinished {
        id: String,
    },
}

pub struct WorkerPool {
    senders: HashMap<String, Sender<WorkerCommand>>,
    pending: HashMap<String, usize>,
    event_sender: Sender<WorkerEvent>,
}

impl WorkerPool {
    pub fn new(event_sender: Sender<WorkerEvent>) -> Self {
        Self {
            senders: HashMap::new(),
            pending: HashMap::new(),
            event_sender,
        }
    }

    pub fn send(&mut self, id: &str, command: WorkerCommand) -> Result<(), String> {
        if matches!(command, WorkerCommand::Stop) {
            return self.retire(id);
        }
        if !self.senders.contains_key(id) {
            let (sender, receiver) = mpsc::channel();
            spawn_worker(id.to_owned(), receiver, self.event_sender.clone())?;
            self.senders.insert(id.to_owned(), sender);
        }
        self.senders[id]
            .send(command)
            .map_err(|_| "device worker stopped unexpectedly".to_owned())?;
        *self.pending.entry(id.to_owned()).or_default() += 1;
        Ok(())
    }

    pub fn is_busy(&self, id: &str) -> bool {
        self.pending.get(id).is_some_and(|count| *count > 0)
    }

    pub fn has_pending(&self) -> bool {
        self.pending.values().any(|count| *count > 0)
    }

    pub fn job_finished(&mut self, id: &str) {
        if let Some(count) = self.pending.get_mut(id) {
            *count = count.saturating_sub(1);
        }
    }

    pub fn retire(&mut self, id: &str) -> Result<(), String> {
        if self.is_busy(id) {
            return Err("Wait for this device's queued operations to finish".into());
        }
        if let Some(sender) = self.senders.remove(id) {
            let _ = sender.send(WorkerCommand::Stop);
        }
        self.pending.remove(id);
        Ok(())
    }

    pub fn retire_all(&mut self) -> Result<(), String> {
        if self.has_pending() {
            return Err("Wait for queued device operations to finish".into());
        }
        for id in self.senders.keys().cloned().collect::<Vec<_>>() {
            self.retire(&id)?;
        }
        Ok(())
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        for sender in self.senders.values() {
            let _ = sender.send(WorkerCommand::Stop);
        }
    }
}

/// Each device keeps its own thread, and each thread its own single-threaded
/// runtime: the command queue stays blocking and per-device, while the SSH
/// session that thread drives is asynchronous.
fn spawn_worker(
    id: String,
    receiver: Receiver<WorkerCommand>,
    events: Sender<WorkerEvent>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Could not start the SSH runtime: {error}"))?;
    thread::Builder::new()
        .name(format!("ssh-{id}"))
        .spawn(move || {
            while let Ok(command) = receiver.recv() {
                if matches!(command, WorkerCommand::Stop) {
                    break;
                }
                if let Err(error) = runtime.block_on(dispatch(command, &events)) {
                    match error {
                        ConnectError::UnknownHostKey { id, fingerprint } => {
                            log(&events, &id, Direction::Note, "Host key is not trusted");
                            let _ = events.send(WorkerEvent::HostKeyUnknown { id, fingerprint });
                        }
                        ConnectError::Message { id, message } => {
                            log(&events, &id, Direction::Note, &message);
                            let _ = events.send(WorkerEvent::Error { id, message });
                        }
                    }
                }
                let _ = events.send(WorkerEvent::JobFinished { id: id.clone() });
            }
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn dispatch(command: WorkerCommand, events: &Sender<WorkerEvent>) -> WorkerResult<()> {
    match command {
        WorkerCommand::Refresh(connection) => refresh(&connection, events).await,
        WorkerCommand::RunScript {
            connection,
            name,
            commands,
        } => run_script(&connection, &name, &commands, events).await,
        WorkerCommand::UploadProgram {
            connection,
            local_path,
            signature,
            slot,
        } => upload_program(&connection, &local_path, signature.as_deref(), slot, events).await,
        WorkerCommand::UploadTouchpanel {
            connection,
            local_path,
        } => upload_staged(&connection, &local_path, touchpanel_transfer, events).await,
        WorkerCommand::UploadConfig {
            connection,
            local_path,
        } => upload_staged(&connection, &local_path, config_transfer, events).await,
        WorkerCommand::UploadFirmware {
            connection,
            local_path,
            remote_name,
        } => upload_firmware(&connection, &local_path, &remote_name, events).await,
        WorkerCommand::Stop => Ok(()),
    }
}

#[derive(Debug)]
enum ConnectError {
    UnknownHostKey { id: String, fingerprint: String },
    Message { id: String, message: String },
}

type WorkerResult<T> = Result<T, ConnectError>;

fn log(events: &Sender<WorkerEvent>, id: &str, direction: Direction, text: &str) {
    let _ = events.send(WorkerEvent::Log {
        id: id.to_owned(),
        direction,
        text: text.to_owned(),
    });
}

fn message(spec: &ConnectionSpec, message: impl Into<String>) -> ConnectError {
    ConnectError::Message {
        id: spec.id.clone(),
        message: message.into(),
    }
}

/// The host key is judged inside the handshake, so the fingerprint has to come
/// back out to the caller that decides what to report about it.
#[derive(Clone, Default)]
struct SeenFingerprint(Arc<Mutex<Option<String>>>);

impl SeenFingerprint {
    fn set(&self, fingerprint: Option<String>) {
        if let Ok(mut seen) = self.0.lock() {
            *seen = fingerprint;
        }
    }

    fn get(&self) -> Option<String> {
        self.0.lock().ok().and_then(|seen| seen.clone())
    }
}

struct TrustOnFirstUse {
    trusted: Option<String>,
    seen: SeenFingerprint,
}

impl client::Handler for TrustOnFirstUse {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // Host certificates are not part of this app's trust model.
        let fingerprint = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => host_fingerprint(key),
            PublicKeyOrCertificate::Certificate(_) => None,
        };
        self.seen.set(fingerprint.clone());
        Ok(match (self.trusted.as_deref(), fingerprint.as_deref()) {
            (Some(trusted), Some(offered)) => trusted == offered,
            _ => false,
        })
    }
}

fn host_fingerprint(key: &PublicKey) -> Option<String> {
    key.to_bytes()
        .ok()
        .map(|blob| sha256_fingerprint(blob.as_ref()))
}

async fn connect(
    spec: &ConnectionSpec,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<Handle<TrustOnFirstUse>> {
    let _ = events.send(WorkerEvent::Connecting {
        id: spec.id.clone(),
    });
    open_connection(spec, events).await
}

async fn open_connection(
    spec: &ConnectionSpec,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<Handle<TrustOnFirstUse>> {
    if spec.credentials.username.trim().is_empty() {
        return Err(message(spec, "Enter an SSH username before connecting"));
    }
    if spec.credentials.password.is_empty() {
        return Err(message(spec, "Enter the SSH password for this session"));
    }

    log(
        events,
        &spec.id,
        Direction::Note,
        &format!("Connecting to {}:{}", spec.host, spec.port),
    );
    let stream = tcp_connect(spec)?;
    let seen = SeenFingerprint::default();
    let session = open_session(spec, stream, SSH_TIMEOUT, &seen).await?;
    authenticate(spec, session).await
}

/// Connected synchronously so an unreachable device fails in seconds rather
/// than waiting out the platform's own connect timeout.
fn tcp_connect(spec: &ConnectionSpec) -> WorkerResult<tokio::net::TcpStream> {
    let addresses = (spec.host.as_str(), spec.port)
        .to_socket_addrs()
        .map_err(|error| message(spec, format!("Could not resolve {}: {error}", spec.host)))?;
    let mut last_error = None;
    let mut stream = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(value) => {
                stream = Some(value);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let stream = stream.ok_or_else(|| {
        message(
            spec,
            format!(
                "Could not connect to {}:{}: {}",
                spec.host,
                spec.port,
                last_error
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "no address found".into())
            ),
        )
    })?;
    stream
        .set_nonblocking(true)
        .map_err(|error| message(spec, error.to_string()))?;
    tokio::net::TcpStream::from_std(stream).map_err(|error| message(spec, error.to_string()))
}

fn ssh_preferences() -> russh::Preferred {
    let mut preferred = russh::Preferred::default();
    // Russh supports NIST ECDH but omits it from its defaults. CrestronSSH
    // devices need it ahead of DH group exchange, which fails with russh
    // on the RMC3. Keep modern defaults first and do not enable SHA-1 KEX.
    let mut kex = preferred.kex.to_vec();
    let position = kex
        .iter()
        .position(|name| *name == russh::kex::DH_GEX_SHA256)
        .unwrap_or(0);
    kex.splice(
        position..position,
        [
            russh::kex::ECDH_SHA2_NISTP256,
            russh::kex::ECDH_SHA2_NISTP384,
            russh::kex::ECDH_SHA2_NISTP521,
        ],
    );
    preferred.kex = kex.into();
    preferred
}

async fn open_session(
    spec: &ConnectionSpec,
    stream: tokio::net::TcpStream,
    timeout: Duration,
    seen: &SeenFingerprint,
) -> WorkerResult<Handle<TrustOnFirstUse>> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(timeout),
        preferred: ssh_preferences(),
        ..Default::default()
    });
    let handler = TrustOnFirstUse {
        trusted: spec.trusted_fingerprint.clone(),
        seen: seen.clone(),
    };
    match tokio::time::timeout(timeout, client::connect_stream(config, stream, handler)).await {
        Ok(Ok(session)) => Ok(session),
        Ok(Err(error)) => Err(host_key_error(spec, seen, error)),
        Err(_) => Err(message(
            spec,
            format!("Timed out completing the SSH handshake with {}", spec.host),
        )),
    }
}

/// A refused host key surfaces as a generic handshake failure, so the
/// fingerprint recorded during the handshake is what separates a key we have
/// never seen from one that has changed.
fn host_key_error(
    spec: &ConnectionSpec,
    seen: &SeenFingerprint,
    error: russh::Error,
) -> ConnectError {
    match (spec.trusted_fingerprint.as_deref(), seen.get()) {
        (None, Some(fingerprint)) => ConnectError::UnknownHostKey {
            id: spec.id.clone(),
            fingerprint,
        },
        (Some(trusted), Some(offered)) if trusted != offered => message(
            spec,
            format!(
                "SSH host key changed. Expected {trusted}, received {offered}. Connection refused"
            ),
        ),
        _ => message(spec, format!("SSH handshake failed: {error}")),
    }
}

async fn authenticate(
    spec: &ConnectionSpec,
    mut session: Handle<TrustOnFirstUse>,
) -> WorkerResult<Handle<TrustOnFirstUse>> {
    let attempt = session.authenticate_password(
        spec.credentials.username.trim(),
        spec.credentials.password.clone(),
    );
    match tokio::time::timeout(SSH_TIMEOUT, attempt).await {
        Ok(Ok(result)) if result.success() => Ok(session),
        Ok(Ok(_)) => Err(message(spec, "SSH authentication was not accepted")),
        Ok(Err(error)) => Err(message(spec, format!("SSH authentication failed: {error}"))),
        Err(_) => Err(message(spec, "Timed out authenticating over SSH")),
    }
}

async fn disconnect(session: Handle<TrustOnFirstUse>) {
    let _ = session
        .disconnect(Disconnect::ByApplication, "", "English")
        .await;
}

async fn refresh(spec: &ConnectionSpec, events: &Sender<WorkerEvent>) -> WorkerResult<()> {
    let session = connect(spec, events).await?;
    let details = DeviceDetails {
        identity: format!(
            "{}\n\n{}",
            section_result(run_command(spec, &session, "hostname", SSH_TIMEOUT, events).await),
            section_result(run_command(spec, &session, "ver", SSH_TIMEOUT, events).await)
        ),
        network: section_result(run_command(spec, &session, "ipconfig", SSH_TIMEOUT, events).await),
        programs: section_result(
            run_command(spec, &session, "progcomments", SSH_TIMEOUT, events).await,
        ),
        ip_table: section_result(run_command(spec, &session, "ipt -t", SSH_TIMEOUT, events).await),
        cresnet: optional_section(
            run_command(spec, &session, "REPORTCRESNET", SSH_TIMEOUT, events).await,
        ),
    };
    disconnect(session).await;
    events
        .send(WorkerEvent::Details {
            id: spec.id.clone(),
            details,
        })
        .map_err(|error| message(spec, error.to_string()))?;
    Ok(())
}

async fn run_command(
    spec: &ConnectionSpec,
    session: &Handle<TrustOnFirstUse>,
    command: &str,
    timeout: Duration,
    events: &Sender<WorkerEvent>,
) -> Result<String, String> {
    log(events, &spec.id, Direction::Sent, command);
    let result = match tokio::time::timeout(timeout, exec(session, command)).await {
        Ok(result) => result,
        Err(_) => Err(format!("{command} did not finish within {timeout:?}")),
    };
    match &result {
        Ok(output) => log(events, &spec.id, Direction::Received, output),
        Err(error) => log(events, &spec.id, Direction::Received, error),
    }
    result
}

async fn run_script(
    spec: &ConnectionSpec,
    name: &str,
    commands: &[String],
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    if commands.is_empty()
        || commands.iter().any(|command| {
            command.trim().is_empty() || command.chars().any(|c| c.is_control() && c != '\t')
        })
    {
        return Err(message(
            spec,
            "A script must contain nonempty, single-line commands",
        ));
    }
    let session = connect(spec, events).await?;
    log(
        events,
        &spec.id,
        Direction::Note,
        &format!("Running script {name} ({} commands)", commands.len()),
    );
    let mut result = Ok(());
    for (index, command) in commands.iter().enumerate() {
        if let Err(error) = run_command(spec, &session, command, SSH_TIMEOUT, events).await {
            result = Err(message(
                spec,
                format!("Script {name} stopped at command {}: {error}", index + 1),
            ));
            break;
        }
    }
    disconnect(session).await;
    result?;
    let _ = events.send(WorkerEvent::Complete {
        id: spec.id.clone(),
        message: format!("Script {name} completed ({} commands)", commands.len()),
    });
    Ok(())
}

async fn exec(session: &Handle<TrustOnFirstUse>, command: &str) -> Result<String, String> {
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|error| error.to_string())?;
    channel
        .exec(true, command)
        .await
        .map_err(|error| error.to_string())?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            _ => {}
        }
    }

    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    // Crestron consoles do not always report an exit status; silence means success.
    let status = status.unwrap_or(0);
    if status != 0 {
        let detail = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        Err(format!(
            "{command} exited with status {status}: {}",
            detail.trim()
        ))
    } else if stderr.trim().is_empty() {
        Ok(stdout)
    } else {
        Ok(format!("{stdout}\n{stderr}"))
    }
}

/// One file to stage: where it comes from, where it lands, and the name to
/// show for it.
#[derive(Clone, Debug)]
struct Transfer {
    local: PathBuf,
    remote: String,
    display: String,
}

impl Transfer {
    fn new(local: impl Into<PathBuf>, remote: String, display: String) -> Self {
        Self {
            local: local.into(),
            remote,
            display,
        }
    }
}

/// What an interactive session tells its window.
#[derive(Debug)]
pub enum TerminalEvent {
    Opened,
    Output(Vec<u8>),
    /// Why the session ended. Empty when the device simply closed it.
    Closed(String),
}

/// Opens an interactive shell on its own thread, outside the command queue: a
/// session a person is typing into lasts as long as they want it to, and must
/// not hold up the queued operations or stop the application closing.
///
/// Everything typed and everything received also reaches the device log, so a
/// terminal leaves the same record as any other operation.
pub fn open_terminal(
    spec: ConnectionSpec,
    input: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    output: Sender<TerminalEvent>,
    events: Sender<WorkerEvent>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Could not start the SSH runtime: {error}"))?;
    thread::Builder::new()
        .name(format!("terminal-{}", spec.id))
        .spawn(move || {
            let reason = match runtime.block_on(terminal_session(&spec, input, &output, &events)) {
                Ok(()) => String::new(),
                Err(ConnectError::UnknownHostKey { id, fingerprint }) => {
                    log(&events, &id, Direction::Note, "Host key is not trusted");
                    let _ = events.send(WorkerEvent::HostKeyUnknown { id, fingerprint });
                    "The host key is not trusted yet. Accept it, then connect again.".to_owned()
                }
                Err(ConnectError::Message { message, .. }) => message,
            };
            if !reason.is_empty() {
                log(&events, &spec.id, Direction::Note, &reason);
            }
            let _ = output.send(TerminalEvent::Closed(reason));
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn terminal_session(
    spec: &ConnectionSpec,
    mut input: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    output: &Sender<TerminalEvent>,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    // Without announcing it: the device card belongs to the queued operations,
    // and a terminal is not one of them.
    let session = open_connection(spec, events).await?;
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|error| message(spec, format!("Could not open an SSH channel: {error}")))?;
    // A console that expects a person expects a terminal, so ask for the same
    // one a terminal program would.
    channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await
        .map_err(|error| message(spec, format!("The device refused a terminal: {error}")))?;
    channel
        .request_shell(true)
        .await
        .map_err(|error| message(spec, format!("The device refused a shell: {error}")))?;
    log(events, &spec.id, Direction::Note, "Terminal opened");
    let _ = output.send(TerminalEvent::Opened);

    let mut received = Vec::new();
    loop {
        tokio::select! {
            typed = input.recv() => {
                // The window has gone; close the session with it.
                let Some(typed) = typed else { break };
                log(events, &spec.id, Direction::Sent, &String::from_utf8_lossy(&typed));
                channel
                    .data(typed.as_slice())
                    .await
                    .map_err(|error| message(spec, format!("Could not send: {error}")))?;
            }
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    log_lines(events, &spec.id, &mut received, &data);
                    let _ = output.send(TerminalEvent::Output(data.to_vec()));
                }
                // Either end closing the channel ends the session.
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => break,
                Some(_) => {}
            },
        }
    }
    // Whatever arrived without a newline behind it is still worth recording.
    if !received.is_empty() {
        log(
            events,
            &spec.id,
            Direction::Received,
            &String::from_utf8_lossy(&received),
        );
    }
    log(events, &spec.id, Direction::Note, "Terminal closed");
    let _ = channel.close().await;
    disconnect(session).await;
    Ok(())
}

/// The device log holds lines, while a terminal receives whatever arrives, so
/// output is held back until a line of it is complete.
fn log_lines(events: &Sender<WorkerEvent>, id: &str, held: &mut Vec<u8>, data: &[u8]) {
    held.extend_from_slice(data);
    while let Some(end) = held.iter().position(|byte| *byte == b'\n') {
        let line: Vec<u8> = held.drain(..=end).collect();
        log(
            events,
            id,
            Direction::Received,
            &String::from_utf8_lossy(&line),
        );
    }
    // A device that never sends a newline must not grow the buffer forever.
    if held.len() > 8 * 1024 {
        let line = std::mem::take(held);
        log(
            events,
            id,
            Direction::Received,
            &String::from_utf8_lossy(&line),
        );
    }
}

/// Loads are addressed by directory: a file left in the SFTP login directory is
/// not where the console command looks for it, so each kind names its own
/// destination and `transfer` pairs that directory with the command to run.
async fn upload_staged(
    spec: &ConnectionSpec,
    local_path: &Path,
    transfer: impl FnOnce(&str) -> (String, Option<String>),
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    let file_name = safe_remote_file_name(local_path)
        .ok_or_else(|| message(spec, "The selected file has an unsafe or missing file name"))?;
    let (remote_path, command) = transfer(&file_name);
    let transfer = Transfer::new(local_path, remote_path, file_name);
    upload_files(spec, vec![transfer], Apply::from(command), events).await
}

/// A program is staged with its signature: the processor reads the signature
/// from the same directory, under the program's name with a `.zig` extension.
async fn upload_program(
    spec: &ConnectionSpec,
    local_path: &Path,
    signature: Option<&Path>,
    slot: u8,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    let file_name = safe_remote_file_name(local_path)
        .ok_or_else(|| message(spec, "The selected file has an unsafe or missing file name"))?;
    let directory = program_directory(slot);
    let mut transfers = vec![Transfer::new(
        local_path,
        format!("{directory}/{file_name}"),
        file_name.clone(),
    )];
    if let Some(signature) = signature {
        let remote_name = signature_remote_name(&file_name).ok_or_else(|| {
            message(
                spec,
                "The program's signature has an unsafe or missing file name",
            )
        })?;
        transfers.push(Transfer::new(
            signature,
            format!("{directory}/{remote_name}"),
            remote_name,
        ));
    }
    upload_files(
        spec,
        transfers,
        Apply::Command(format!("progload -p:{slot}")),
        events,
    )
    .await
}

/// Slot N runs the program staged in its own directory. Note that the SFTP
/// namespace is not the one the SSH console shows: the console's
/// `\SIMPL\app01` is reached over SFTP as `/program01`.
fn program_directory(slot: u8) -> String {
    format!("/program{slot:02}")
}

/// The signature is uploaded under the program's stem with a `.zig` extension,
/// whatever the local `.sig` file happens to be called.
fn signature_remote_name(program_name: &str) -> Option<String> {
    let stem = Path::new(program_name).file_stem()?.to_str()?;
    let name = format!("{stem}.zig");
    is_safe_remote_file_name(&name).then_some(name)
}

/// `projectload` installs whichever project is sitting in the display directory.
fn touchpanel_transfer(remote_name: &str) -> (String, Option<String>) {
    (
        format!("/display/{remote_name}"),
        Some("projectload".to_owned()),
    )
}

/// Configuration files are read from the user directory by the running program;
/// staging one is the whole operation, so there is no load command to issue.
fn config_transfer(remote_name: &str) -> (String, Option<String>) {
    (format!("/user/{remote_name}"), None)
}

async fn upload_firmware(
    spec: &ConnectionSpec,
    local_path: &Path,
    remote_name: &str,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    let Some((remote_path, apply)) = firmware_transfer(remote_name) else {
        return Err(message(
            spec,
            "The assigned firmware has an unsafe or missing file name",
        ));
    };
    let transfer = Transfer::new(local_path, remote_path, remote_name.to_owned());
    upload_files(spec, vec![transfer], apply, events).await
}

/// What to do once the firmware is staged. Only the file name decides: a zip
/// update is pushed in place, and everything else is a PUF, which the device
/// applies to itself and then restarts.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Apply {
    Nothing,
    Command(String),
    Puf,
}

impl From<Option<String>> for Apply {
    fn from(command: Option<String>) -> Self {
        command.map_or(Self::Nothing, Self::Command)
    }
}

/// The name never reaches a console command, but it does become a remote path,
/// so it still has to be a plain file name.
fn firmware_transfer(remote_name: &str) -> Option<(String, Apply)> {
    if !is_safe_command_file_name(remote_name) {
        return None;
    }
    let remote_path = format!("/firmware/{remote_name}");
    let apply = if Path::new(remote_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        Apply::Command("pushupdate full".to_owned())
    } else {
        Apply::Puf
    };
    Some((remote_path, apply))
}

/// Applies a staged PUF: the device works through the update, reporting as it
/// goes, then restarts without closing the session politely. It is unreachable
/// for minutes, and its port answers again before its console does, so the
/// wait is a repeated login and the results query is retried after it.
async fn apply_puf(
    spec: &ConnectionSpec,
    session: Handle<TrustOnFirstUse>,
    timings: Timings,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<String> {
    log(events, &spec.id, Direction::Sent, PUF_COMMAND);
    let report =
        match tokio::time::timeout(LOAD_TIMEOUT, exec_until_closed(&session, PUF_COMMAND)).await {
            Ok(Ok(report)) => report,
            // Neither a dropped connection nor a silent one proves the update
            // failed; the device is asked directly once it is back.
            Ok(Err(error)) => format!("(the connection ended: {error})"),
            Err(_) => format!("({PUF_COMMAND} was still running after {LOAD_TIMEOUT:?})"),
        };
    log(events, &spec.id, Direction::Received, &report);
    disconnect(session).await;

    let session = await_restart(spec, timings, events).await?;
    let results = read_puf_results(spec, &session, timings, events).await;
    disconnect(session).await;
    let Some(results) = crate::puf::parse(&results) else {
        return Ok("the device did not report component results".to_owned());
    };
    log(events, &spec.id, Direction::Note, &results.as_text());
    Ok(results.summary())
}

/// The device is gone while it restarts, so every attempt is expected to fail
/// until it is not. An untrusted host key is not a restart and stops the wait.
async fn await_restart(
    spec: &ConnectionSpec,
    timings: Timings,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<Handle<TrustOnFirstUse>> {
    log(
        events,
        &spec.id,
        Direction::Note,
        &format!(
            "Waiting for the device to restart, checking {}:{} every {} seconds for up to {}",
            spec.host,
            spec.port,
            timings.poll.as_secs(),
            clock(timings.limit)
        ),
    );
    let started = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(timings.poll).await;
        let waited = started.elapsed();
        match connect(spec, events).await {
            Ok(session) => {
                log(
                    events,
                    &spec.id,
                    Direction::Note,
                    &format!("The device answered after {}", clock(waited)),
                );
                return Ok(session);
            }
            Err(error @ ConnectError::UnknownHostKey { .. }) => return Err(error),
            Err(ConnectError::Message {
                message: reason, ..
            }) => {
                log(
                    events,
                    &spec.id,
                    Direction::Note,
                    &format!("Not back after {}: {reason}", clock(waited)),
                );
                if waited >= timings.limit {
                    return Err(message(
                        spec,
                        format!(
                            "The device did not come back within {}: {reason}",
                            clock(timings.limit)
                        ),
                    ));
                }
                let _ = events.send(WorkerEvent::Status {
                    id: spec.id.clone(),
                    message: format!(
                        "Waiting for the device to restart ({} of {})",
                        clock(waited),
                        clock(timings.limit)
                    ),
                    progress: Some((waited.as_secs(), timings.limit.as_secs())),
                });
            }
        }
    }
}

/// A device that has just booted accepts a login before its console will
/// answer, so the query is repeated until it produces the report.
async fn read_puf_results(
    spec: &ConnectionSpec,
    session: &Handle<TrustOnFirstUse>,
    timings: Timings,
    events: &Sender<WorkerEvent>,
) -> String {
    let mut last = String::new();
    for attempt in 0..timings.attempts {
        if attempt > 0 {
            tokio::time::sleep(timings.settle).await;
        }
        match run_command(spec, session, PUF_RESULTS_COMMAND, SSH_TIMEOUT, events).await {
            Ok(output) if crate::puf::parse(&output).is_some() => return output,
            Ok(output) => last = output,
            Err(error) => last = error,
        }
    }
    last
}

/// Everything the device says before it stops saying anything. A restart ends
/// the channel partway through, which is the expected ending here, so neither
/// the exit status nor the missing close is treated as a failure.
async fn exec_until_closed(
    session: &Handle<TrustOnFirstUse>,
    command: &str,
) -> Result<String, String> {
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|error| error.to_string())?;
    channel
        .exec(true, command)
        .await
        .map_err(|error| error.to_string())?;
    let mut output = Vec::new();
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                output.extend_from_slice(&data);
            }
            _ => {}
        }
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

fn clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Stages every file over one connection and one SFTP session, then runs the
/// load command once, so a multi-file load is a single conversation with the
/// device and reports a single progress bar.
async fn upload_files(
    spec: &ConnectionSpec,
    transfers: Vec<Transfer>,
    apply: Apply,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    // Opened before connecting so a missing file fails without touching the device.
    let mut sources = Vec::with_capacity(transfers.len());
    let mut total = 0_u64;
    for transfer in transfers {
        let file = tokio::fs::File::open(&transfer.local)
            .await
            .map_err(|error| {
                message(
                    spec,
                    format!("Could not open {}: {error}", transfer.local.display()),
                )
            })?;
        let length = file
            .metadata()
            .await
            .map_err(|error| message(spec, error.to_string()))?
            .len();
        total += length;
        sources.push((transfer, file, length));
    }
    let display_name = sources
        .iter()
        .map(|(transfer, _, _)| transfer.display.as_str())
        .collect::<Vec<_>>()
        .join(" + ");

    let session = connect(spec, events).await?;
    let channel = session
        .channel_open_session()
        .await
        .map_err(|error| message(spec, format!("Could not open an SSH channel: {error}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|error| message(spec, format!("Could not start SFTP: {error}")))?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|error| message(spec, format!("Could not start SFTP: {error}")))?;

    let mut buffer = vec![0_u8; CHUNK];
    let mut sent = 0_u64;
    for (transfer, mut local, length) in sources {
        log(
            events,
            &spec.id,
            Direction::Note,
            &format!("SFTP create {} ({length} bytes)", transfer.remote),
        );
        let mut remote = sftp.create(&transfer.remote).await.map_err(|error| {
            message(
                spec,
                format!("Could not create remote file {}: {error}", transfer.remote),
            )
        })?;
        loop {
            let read = local
                .read(&mut buffer)
                .await
                .map_err(|error| message(spec, format!("Could not read local file: {error}")))?;
            if read == 0 {
                break;
            }
            remote
                .write_all(&buffer[..read])
                .await
                .map_err(|error| message(spec, format!("SFTP upload failed: {error}")))?;
            sent += read as u64;
            let _ = events.send(WorkerEvent::Progress {
                id: spec.id.clone(),
                sent,
                total,
            });
        }
        remote
            .close()
            .await
            .map_err(|error| message(spec, format!("Could not close remote file: {error}")))?;
        log(
            events,
            &spec.id,
            Direction::Note,
            &format!("Uploaded {} to {}", transfer.display, transfer.remote),
        );
    }
    let _ = sftp.close().await;

    let completion = match apply {
        Apply::Nothing => {
            disconnect(session).await;
            format!("Uploaded {display_name}")
        }
        Apply::Command(command) => {
            let output = run_command(spec, &session, &command, LOAD_TIMEOUT, events)
                .await
                .map_err(|error| message(spec, format!("Load command failed: {error}")))?;
            disconnect(session).await;
            if output.trim().is_empty() {
                format!("Uploaded {display_name}; command completed")
            } else {
                format!("Uploaded {display_name}: {}", output.trim())
            }
        }
        // Takes over the session: the device restarts partway through and is
        // reconnected to before it will say how the update went.
        Apply::Puf => {
            let summary = apply_puf(spec, session, Timings::DEVICE, events).await?;
            format!("Updated firmware from {display_name}: {summary}")
        }
    };
    events
        .send(WorkerEvent::Complete {
            id: spec.id.clone(),
            message: completion,
        })
        .map_err(|error| message(spec, error.to_string()))?;
    Ok(())
}

fn section_result(result: Result<String, String>) -> String {
    result.unwrap_or_else(|error| format!("Unavailable: {error}"))
}

fn optional_section(result: Result<String, String>) -> Option<String> {
    match result {
        Ok(output)
            if !output.trim().is_empty()
                && !output.to_ascii_lowercase().contains("invalid command")
                && !output.to_ascii_lowercase().contains("unknown command") =>
        {
            Some(output)
        }
        _ => None,
    }
}

fn safe_remote_file_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    is_safe_remote_file_name(name).then(|| name.to_owned())
}

/// Staged names reach the device only as the last component of an SFTP path, so
/// the requirement is that they cannot escape the destination directory.
/// Ordinary filename characters, spaces included, are fine.
fn is_safe_remote_file_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control)
}

/// A name interpolated into a console command has to survive that command's
/// argument splitting, so it keeps the far narrower allowlist: a space would
/// split the argument and a metacharacter could append another command. Any
/// new command that names its file must validate with this, not the above.
fn is_safe_command_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
}

fn sha256_fingerprint(host_key: &[u8]) -> String {
    let digest = Sha256::digest(host_key);
    let encoded = base64_without_dependency(&digest);
    format!("SHA256:{encoded}")
}

fn base64_without_dependency(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[((value >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            output.push(TABLE[(value & 63) as usize] as char);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn spec() -> ConnectionSpec {
        ConnectionSpec {
            id: "test:22".into(),
            host: "127.0.0.1".into(),
            port: 22,
            credentials: Credentials {
                username: "admin".into(),
                password: "secret".into(),
            },
            trusted_fingerprint: None,
        }
    }

    #[test]
    fn scripts_execute_over_ssh_in_order_stop_on_failure_and_require_trust() {
        struct ScriptServer {
            received: Arc<Mutex<Vec<String>>>,
        }
        impl russh::server::Handler for ScriptServer {
            type Error = russh::Error;

            async fn auth_password(
                &mut self,
                user: &str,
                password: &str,
            ) -> Result<russh::server::Auth, Self::Error> {
                assert_eq!((user, password), ("admin", "secret"));
                Ok(russh::server::Auth::Accept)
            }

            async fn channel_open_session(
                &mut self,
                _channel: russh::Channel<russh::server::Msg>,
                reply: russh::server::ChannelOpenHandle,
                _session: &mut russh::server::Session,
            ) -> Result<(), Self::Error> {
                reply.accept().await;
                Ok(())
            }

            async fn exec_request(
                &mut self,
                channel: russh::ChannelId,
                data: &[u8],
                session: &mut russh::server::Session,
            ) -> Result<(), Self::Error> {
                let command = String::from_utf8(data.to_vec()).unwrap();
                self.received.lock().unwrap().push(command.clone());
                session.channel_success(channel)?;
                session.data(channel, format!("output for {command}"))?;
                session.exit_status_request(channel, if command == "fail" { 1 } else { 0 })?;
                session.eof(channel)?;
                session.close(channel)?;
                Ok(())
            }
        }

        for (trusted, fail) in [(true, false), (true, true), (false, false)] {
            runtime().block_on(async {
                // A deterministic key for this loopback-only test server.
                let key = russh::keys::PrivateKey::from(
                    russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[42; 32]),
                );
                let fingerprint = host_fingerprint(key.public_key()).unwrap();
                let config = Arc::new(russh::server::Config {
                    keys: vec![key],
                    auth_rejection_time: Duration::ZERO,
                    ..Default::default()
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = listener.local_addr().unwrap().port();
                let received = Arc::new(Mutex::new(Vec::new()));
                let handler = ScriptServer { received: received.clone() };
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                        let _ = session.await;
                    }
                });
                let mut spec = spec();
                spec.port = port;
                spec.trusted_fingerprint = trusted.then_some(fingerprint);
                let commands = vec!["first".into(), if fail { "fail".into() } else { "second".into() }, "third".into()];
                let (events, receiver) = mpsc::channel();
                let result = tokio::time::timeout(Duration::from_secs(5), run_script(&spec, "Test", &commands, &events)).await.unwrap();
                if !trusted {
                    assert!(matches!(result, Err(ConnectError::UnknownHostKey { .. })));
                    assert!(received.lock().unwrap().is_empty());
                } else if fail {
                    assert!(matches!(result, Err(ConnectError::Message { message, .. }) if message.contains("command 2")));
                    assert_eq!(*received.lock().unwrap(), ["first", "fail"]);
                } else {
                    result.unwrap();
                    assert_eq!(*received.lock().unwrap(), commands);
                }
                let events = receiver.try_iter().collect::<Vec<_>>();
                assert_eq!(events.iter().any(|event| matches!(event, WorkerEvent::Complete { .. })), trusted && !fail);
                if trusted {
                    assert!(events.iter().any(|event| matches!(event, WorkerEvent::Log { text, direction: Direction::Received, .. } if text == "output for first")));
                }
                server.abort();
            });
        }
    }

    #[test]
    fn ecdh_precedes_group_exchange_without_enabling_sha1() {
        let preferred = ssh_preferences();
        let position = |name| preferred.kex.iter().position(|n| *n == name).unwrap();
        assert!(position(russh::kex::CURVE25519) < position(russh::kex::ECDH_SHA2_NISTP256));
        for name in [
            russh::kex::ECDH_SHA2_NISTP256,
            russh::kex::ECDH_SHA2_NISTP384,
            russh::kex::ECDH_SHA2_NISTP521,
        ] {
            assert!(position(name) < position(russh::kex::DH_GEX_SHA256));
        }
        assert!(!preferred.kex.contains(&russh::kex::DH_G1_SHA1));
        assert!(!preferred.kex.contains(&russh::kex::DH_G14_SHA1));
        assert!(!preferred.kex.contains(&russh::kex::DH_GEX_SHA1));
        for name in russh::Preferred::default().kex.iter() {
            assert!(preferred.kex.contains(name));
        }
    }

    #[test]
    #[ignore = "requires CRESTRON_SSH_PROBE_HOST; performs a handshake only, no authentication"]
    fn live_ssh_handshake() {
        let mut spec = spec();
        spec.host = std::env::var("CRESTRON_SSH_PROBE_HOST").expect("set probe host");
        spec.credentials = Credentials::default();
        spec.trusted_fingerprint = std::env::var("CRESTRON_SSH_PROBE_FINGERPRINT").ok();
        runtime().block_on(async {
            let seen = SeenFingerprint::default();
            let stream = tcp_connect(&spec).unwrap();
            match open_session(&spec, stream, SSH_TIMEOUT, &seen).await {
                Ok(session) => {
                    println!("Handshake completed; fingerprint: {:?}", seen.get());
                    disconnect(session).await;
                }
                Err(ConnectError::UnknownHostKey { fingerprint, .. })
                    if spec.trusted_fingerprint.is_none() =>
                {
                    println!("Reached host-key verification: {fingerprint}");
                }
                Err(error) => panic!("Handshake failed: {error:?}"),
            }
        });
    }

    #[test]
    fn stalled_ssh_handshake_times_out() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_silent_peer, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        let runtime = runtime();

        let error = runtime.block_on(async {
            let stream = tokio::net::TcpStream::from_std(stream).unwrap();
            match open_session(
                &spec(),
                stream,
                Duration::from_millis(300),
                &SeenFingerprint::default(),
            )
            .await
            {
                Ok(_) => panic!("a silent peer cannot complete a handshake"),
                Err(error) => error,
            }
        });
        let ConnectError::Message { message, .. } = error else {
            panic!("expected a plain error, not an unknown host key");
        };
        assert!(message.contains("Timed out"), "{message}");
    }

    #[test]
    fn missing_credentials_are_reported_before_connecting() {
        let (events, _receiver) = mpsc::channel();
        let mut blank = spec();
        blank.credentials.password.clear();
        let error = runtime().block_on(async {
            match connect(&blank, &events).await {
                Ok(_) => panic!("a blank password cannot connect"),
                Err(error) => error,
            }
        });
        let ConnectError::Message { message, .. } = error else {
            panic!("expected a plain error");
        };
        assert!(message.contains("password"), "{message}");
    }

    #[test]
    fn queue_accounting_blocks_retirement_until_every_job_is_observed() {
        let (events, receiver) = mpsc::channel();
        let mut pool = WorkerPool::new(events);
        let id = "test:22";
        let spec = ConnectionSpec {
            id: id.into(),
            host: "192.0.2.1".into(),
            port: 22,
            credentials: Credentials::default(),
            trusted_fingerprint: None,
        };
        pool.send(id, WorkerCommand::Refresh(spec.clone())).unwrap();
        pool.send(id, WorkerCommand::Refresh(spec)).unwrap();
        assert!(pool.is_busy(id));
        assert!(pool.retire(id).is_err());
        assert!(pool.retire_all().is_err());
        let mut finished = 0;
        while finished < 2 {
            if let WorkerEvent::JobFinished { id } =
                receiver.recv_timeout(Duration::from_secs(5)).unwrap()
            {
                pool.job_finished(&id);
                finished += 1;
                assert_eq!(pool.has_pending(), finished < 2);
            }
        }
        pool.retire(id).unwrap();
        assert!(!pool.senders.contains_key(id));
        assert!(!pool.has_pending());
    }

    #[test]
    fn staged_names_allow_spaces_but_never_escape_the_directory() {
        assert_eq!(
            safe_remote_file_name(Path::new("/tmp/room-program_1.lpz")).as_deref(),
            Some("room-program_1.lpz")
        );
        // Ordinary Windows project names reach the device unchanged.
        assert_eq!(
            safe_remote_file_name(Path::new(r"C:\Working Directory\W classrooms.lpz")).as_deref(),
            Some("W classrooms.lpz")
        );
        assert!(!is_safe_remote_file_name(""));
        assert!(!is_safe_remote_file_name("   "));
        assert!(!is_safe_remote_file_name("."));
        assert!(!is_safe_remote_file_name(".."));
        assert!(!is_safe_remote_file_name("folder/device.puf"));
        assert!(!is_safe_remote_file_name(r"folder\device.puf"));
        assert!(!is_safe_remote_file_name("stream:name"));
        assert!(!is_safe_remote_file_name("bell\u{7}.lpz"));
    }

    #[test]
    fn command_interpolated_names_keep_the_narrow_allowlist() {
        assert!(is_safe_command_file_name("rmc4_2.8000.00001.puf"));
        assert!(!is_safe_command_file_name("unsafe firmware.puf"));
        assert!(!is_safe_command_file_name("device.puf; reboot"));
        assert!(!is_safe_command_file_name("."));
        assert!(!is_safe_command_file_name(".."));
        assert!(!is_safe_command_file_name("folder/device.puf"));
    }

    #[test]
    fn remote_paths_stay_posix_regardless_of_the_local_platform() {
        // Built as strings on purpose: joining with PathBuf emits a backslash
        // on Windows, which SFTP reads as part of the file name.
        assert_eq!(program_directory(1), "/program01");
        assert_eq!(program_directory(10), "/program10");
        assert_eq!(
            touchpanel_transfer("lobby-panel.vtz"),
            (
                "/display/lobby-panel.vtz".to_owned(),
                Some("projectload".to_owned()),
            )
        );
        assert_eq!(
            config_transfer("control-room.json"),
            ("/user/control-room.json".to_owned(), None)
        );
    }

    #[test]
    fn signatures_take_the_program_stem_with_a_zig_extension() {
        assert_eq!(
            signature_remote_name("W classrooms.lpz").as_deref(),
            Some("W classrooms.zig")
        );
        assert_eq!(
            signature_remote_name("room-program_1.lpz").as_deref(),
            Some("room-program_1.zig")
        );
        // Extensions other than .lpz keep their stem too.
        assert_eq!(
            signature_remote_name("archive.tar.lpz").as_deref(),
            Some("archive.tar.zig")
        );
        assert!(signature_remote_name("..").is_none());
    }

    /// A session a person types into: it stays open, carries what is typed
    /// both ways, and leaves the same record in the device log as anything else.
    #[test]
    fn a_terminal_carries_a_console_both_ways_and_into_the_device_log() {
        struct ConsoleServer {
            received: Arc<Mutex<Vec<String>>>,
            greeted: bool,
        }
        impl russh::server::Handler for ConsoleServer {
            type Error = russh::Error;

            async fn auth_password(
                &mut self,
                _user: &str,
                _password: &str,
            ) -> Result<russh::server::Auth, Self::Error> {
                Ok(russh::server::Auth::Accept)
            }

            async fn channel_open_session(
                &mut self,
                _channel: russh::Channel<russh::server::Msg>,
                reply: russh::server::ChannelOpenHandle,
                _session: &mut russh::server::Session,
            ) -> Result<(), Self::Error> {
                reply.accept().await;
                Ok(())
            }

            async fn pty_request(
                &mut self,
                channel: russh::ChannelId,
                term: &str,
                _col_width: u32,
                _row_height: u32,
                _pix_width: u32,
                _pix_height: u32,
                _modes: &[(russh::Pty, u32)],
                session: &mut russh::server::Session,
            ) -> Result<(), Self::Error> {
                self.received.lock().unwrap().push(format!("pty:{term}"));
                session.channel_success(channel)?;
                Ok(())
            }

            async fn shell_request(
                &mut self,
                channel: russh::ChannelId,
                session: &mut russh::server::Session,
            ) -> Result<(), Self::Error> {
                self.received.lock().unwrap().push("shell".to_owned());
                session.channel_success(channel)?;
                // A console greets whoever opened it.
                session.data(channel, "RMC4 Console\r\n>".to_owned())?;
                self.greeted = true;
                Ok(())
            }

            async fn data(
                &mut self,
                channel: russh::ChannelId,
                data: &[u8],
                session: &mut russh::server::Session,
            ) -> Result<(), Self::Error> {
                let typed = String::from_utf8_lossy(data).into_owned();
                self.received.lock().unwrap().push(typed.clone());
                // Echoed the way a console with a terminal does, then answered.
                session.data(channel, format!("{typed}\nRMC4\r\n>"))?;
                Ok(())
            }
        }

        // The window side of a terminal is blocking, so the test is too, and
        // the server needs a thread of its own to be answering meanwhile.
        let key = russh::keys::PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[11; 32]),
        );
        let fingerprint = host_fingerprint(key.public_key()).unwrap();
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::ZERO,
            ..Default::default()
        });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let handler = ConsoleServer {
            received: received.clone(),
            greeted: false,
        };
        thread::spawn(move || {
            runtime().block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (stream, _) = listener.accept().await.unwrap();
                if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                    let _ = session.await;
                }
            });
        });

        {
            let mut spec = spec();
            spec.port = port;
            spec.trusted_fingerprint = Some(fingerprint);
            let (events, worker_events) = mpsc::channel();
            let (input, from_window) = tokio::sync::mpsc::unbounded_channel();
            let (to_window, output) = mpsc::channel();
            open_terminal(spec, from_window, to_window, events).unwrap();

            // The session opens on its own; the window does not ask it to.
            let next = |output: &mpsc::Receiver<TerminalEvent>| {
                output.recv_timeout(Duration::from_secs(10)).unwrap()
            };
            assert!(matches!(next(&output), TerminalEvent::Opened));
            let mut shown = String::new();
            while !shown.contains('>') {
                match next(&output) {
                    TerminalEvent::Output(data) => shown.push_str(&String::from_utf8_lossy(&data)),
                    other => panic!("{other:?}"),
                }
            }
            assert!(shown.contains("RMC4 Console"), "{shown:?}");

            input.send(b"hostname\r".to_vec()).unwrap();
            shown.clear();
            while !shown.contains("RMC4") {
                match next(&output) {
                    TerminalEvent::Output(data) => shown.push_str(&String::from_utf8_lossy(&data)),
                    other => panic!("{other:?}"),
                }
            }

            // Closing the window ends the session.
            drop(input);
            let closed = loop {
                match next(&output) {
                    TerminalEvent::Closed(reason) => break reason,
                    TerminalEvent::Output(_) => continue,
                    other => panic!("{other:?}"),
                }
            };
            assert_eq!(closed, "", "a session closed from here reports no fault");

            assert_eq!(
                *received.lock().unwrap(),
                ["pty:xterm", "shell", "hostname\r"],
                "a terminal is asked for, and what is typed reaches the device"
            );
            let logged = worker_events
                .try_iter()
                .filter_map(|event| match event {
                    WorkerEvent::Log {
                        direction, text, ..
                    } => Some((direction, text)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let said = |direction: Direction, wanted: &str| {
                logged
                    .iter()
                    .any(|(seen, text)| *seen == direction && text.contains(wanted))
            };
            assert!(said(Direction::Note, "Terminal opened"), "{logged:?}");
            assert!(said(Direction::Sent, "hostname"), "{logged:?}");
            assert!(said(Direction::Received, "RMC4 Console"), "{logged:?}");
            assert!(said(Direction::Note, "Terminal closed"), "{logged:?}");
        }
    }

    /// Drives the whole PUF sequence against a device that answers `puf`, drops
    /// the connection as it restarts, comes back, and only then has results.
    #[test]
    fn puf_update_survives_the_restart_and_reports_the_component_table() {
        const TABLE: &str = "The PUF update result by components:\n\
             ----Name----+----Result----\n\
             Bootloader  | Success\n\
             OS          | Success\n";

        #[derive(Clone)]
        struct PufServer {
            received: Arc<Mutex<Vec<String>>>,
        }
        impl russh::server::Handler for PufServer {
            type Error = russh::Error;

            async fn auth_password(
                &mut self,
                _user: &str,
                _password: &str,
            ) -> Result<russh::server::Auth, Self::Error> {
                Ok(russh::server::Auth::Accept)
            }

            async fn channel_open_session(
                &mut self,
                _channel: russh::Channel<russh::server::Msg>,
                reply: russh::server::ChannelOpenHandle,
                _session: &mut russh::server::Session,
            ) -> Result<(), Self::Error> {
                reply.accept().await;
                Ok(())
            }

            async fn exec_request(
                &mut self,
                channel: russh::ChannelId,
                data: &[u8],
                session: &mut russh::server::Session,
            ) -> Result<(), Self::Error> {
                let command = String::from_utf8(data.to_vec()).unwrap();
                let mut received = self.received.lock().unwrap();
                received.push(command.clone());
                let asked = received.iter().filter(|seen| **seen == command).count();
                drop(received);
                session.channel_success(channel)?;
                if command == PUF_COMMAND {
                    // Report, then vanish mid-command the way a restart does.
                    session.data(channel, "Updating component 1 of 2\n".to_owned())?;
                    session.disconnect(Disconnect::ByApplication, "rebooting", "")?;
                    return Ok(());
                }
                // A console that has only just booted answers without results.
                let answer = if asked > 1 {
                    TABLE.to_owned()
                } else {
                    "Results are not available yet\n".to_owned()
                };
                session.data(channel, answer)?;
                session.exit_status_request(channel, 0)?;
                session.eof(channel)?;
                session.close(channel)?;
                Ok(())
            }
        }

        runtime().block_on(async {
            let key = russh::keys::PrivateKey::from(
                russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[7; 32]),
            );
            let fingerprint = host_fingerprint(key.public_key()).unwrap();
            let config = Arc::new(russh::server::Config {
                keys: vec![key],
                auth_rejection_time: Duration::ZERO,
                ..Default::default()
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let received = Arc::new(Mutex::new(Vec::new()));
            let handler = PufServer {
                received: received.clone(),
            };
            // The device is reachable again straight away; the restart wait is
            // what makes the second login a new connection.
            let server = tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (config, handler) = (config.clone(), handler.clone());
                    tokio::spawn(async move {
                        if let Ok(session) =
                            russh::server::run_stream(config, stream, handler).await
                        {
                            let _ = session.await;
                        }
                    });
                }
            });

            let mut spec = spec();
            spec.port = port;
            spec.trusted_fingerprint = Some(fingerprint);
            let timings = Timings {
                poll: Duration::from_millis(20),
                limit: Duration::from_secs(5),
                settle: Duration::from_millis(20),
                attempts: 4,
            };
            let (events, receiver) = mpsc::channel();
            let session = connect(&spec, &events).await.unwrap();
            let summary = tokio::time::timeout(
                Duration::from_secs(20),
                apply_puf(&spec, session, timings, &events),
            )
            .await
            .unwrap()
            .unwrap();

            assert_eq!(summary, "2 component(s): Success ×2");
            assert_eq!(
                *received.lock().unwrap(),
                [PUF_COMMAND, PUF_RESULTS_COMMAND, PUF_RESULTS_COMMAND],
                "the results query has to be repeated until the console answers"
            );
            let logged = receiver
                .try_iter()
                .filter_map(|event| match event {
                    WorkerEvent::Log { text, .. } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            // What the device said before it went, and the table it gave after.
            assert!(logged.contains("Updating component 1 of 2"), "{logged}");
            assert!(logged.contains("Bootloader  Success"), "{logged}");
            server.abort();
        });
    }

    #[test]
    fn a_device_that_never_comes_back_gives_up_instead_of_waiting_forever() {
        runtime().block_on(async {
            // Bound and dropped, so the port is closed and refuses at once.
            let port = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let mut spec = spec();
            spec.port = port;
            let timings = Timings {
                poll: Duration::from_millis(20),
                limit: Duration::from_millis(60),
                settle: Duration::from_millis(20),
                attempts: 2,
            };
            let (events, receiver) = mpsc::channel();
            let result = tokio::time::timeout(
                Duration::from_secs(20),
                await_restart(&spec, timings, &events),
            )
            .await
            .unwrap();
            assert!(
                matches!(result, Err(ConnectError::Message { message, .. })
                    if message.contains("did not come back within 0:00")),
                "the wait has to end with the limit it was given"
            );
            // The wait is reported while it runs, not only when it ends.
            assert!(receiver.try_iter().any(|event| matches!(
                event,
                WorkerEvent::Status { message, progress: Some((_, 0)), .. }
                    if message.starts_with("Waiting for the device to restart")
            )));
        });
    }

    #[test]
    fn builds_crestron_firmware_transfer_paths_and_commands() {
        // A PUF is applied by a command with no name in it: the device finds
        // the file it was handed in the firmware directory.
        assert_eq!(
            firmware_transfer("rmc4_2.8000.00001.puf"),
            Some(("/firmware/rmc4_2.8000.00001.puf".to_owned(), Apply::Puf))
        );
        assert_eq!(
            firmware_transfer("no_extension"),
            Some(("/firmware/no_extension".to_owned(), Apply::Puf))
        );
        assert_eq!(
            firmware_transfer("update.ZIP"),
            Some((
                "/firmware/update.ZIP".to_owned(),
                Apply::Command("pushupdate full".to_owned()),
            ))
        );
        assert!(firmware_transfer("unsafe firmware.puf").is_none());
        assert!(firmware_transfer("../escape.puf").is_none());
    }

    #[test]
    fn elapsed_time_reads_as_a_clock() {
        assert_eq!(clock(Duration::from_secs(0)), "0:00");
        assert_eq!(clock(Duration::from_secs(9)), "0:09");
        assert_eq!(clock(Duration::from_secs(630)), "10:30");
        assert_eq!(clock(Timings::DEVICE.limit), "15:00");
    }

    #[test]
    fn encodes_sha256_fingerprint_without_padding() {
        assert_eq!(
            sha256_fingerprint(b"test"),
            "SHA256:n4bQgYhMfWWaL+qgxVrQFaO/TxsrC4Is0V1sFbDwCgg"
        );
    }
}
