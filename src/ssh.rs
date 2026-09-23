use std::{
    collections::{HashMap, VecDeque},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc::Sender},
    thread,
    time::Duration,
};

/// The tests build their own event channels; the worker's own are handed to it.
#[cfg(test)]
use std::sync::mpsc;

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

/// Long enough for a processor on a slow link, short enough that an
/// unreachable device fails in seconds rather than waiting out the platform's
/// own. The test build cuts it right down: its devices are deliberately
/// unroutable, and the suite should not spend seconds waiting for that.
#[cfg(not(test))]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
#[cfg(test)]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(50);
const SSH_TIMEOUT: Duration = Duration::from_secs(20);
const LOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a held connection may sit unused before the device is allowed to
/// have it back. A device permits only a few SSH sessions at once, shared with
/// every other tool on site, so one this application is not using is one
/// nobody else can have either.
const IDLE_LIMIT: Duration = Duration::from_secs(20 * 60);
/// How long the thread waits, as it ends, for a goodbye to reach the device.
/// Best effort: the process may go first.
const FAREWELL: Duration = Duration::from_millis(250);
/// Transport-level liveness for a held connection. This is the operating
/// system's own probing, not an SSH message, so an idle session stays silent as
/// far as the device's console and its own idle timer are concerned, while a
/// peer that has gone away without saying so is still noticed.
const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
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
    /// A console window asking to be put on this device's connection. Taken off
    /// the queue as it arrives rather than waited for in turn, so that a window
    /// opens while a load is running instead of after it.
    OpenTerminal {
        connection: ConnectionSpec,
        input: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        output: Sender<TerminalEvent>,
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
    /// The device refused the username and password, or there was none to
    /// send. Announced so the application can ask for them again.
    CredentialsRejected {
        id: String,
        message: String,
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
    FirmwareUpToDate {
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
    senders: HashMap<String, tokio::sync::mpsc::UnboundedSender<WorkerCommand>>,
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

    /// The device's worker, started if this is the first thing to want it.
    fn worker(
        &mut self,
        id: &str,
    ) -> Result<&tokio::sync::mpsc::UnboundedSender<WorkerCommand>, String> {
        if !self.senders.contains_key(id) {
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            spawn_worker(id.to_owned(), receiver, self.event_sender.clone())?;
            self.senders.insert(id.to_owned(), sender);
        }
        Ok(&self.senders[id])
    }

    pub fn send(&mut self, id: &str, command: WorkerCommand) -> Result<(), String> {
        if matches!(command, WorkerCommand::Stop) {
            return self.retire(id);
        }
        self.worker(id)?
            .send(command)
            .map_err(|_| "device worker stopped unexpectedly".to_owned())?;
        *self.pending.entry(id.to_owned()).or_default() += 1;
        Ok(())
    }

    /// Puts a console window on this device's connection.
    ///
    /// Deliberately not `send`: a console is not one of the queued operations,
    /// and counting it as one would mean the application could never be closed,
    /// nor its address book changed, while a window was open.
    pub fn open_terminal(
        &mut self,
        id: &str,
        connection: ConnectionSpec,
        input: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        output: Sender<TerminalEvent>,
    ) -> Result<(), String> {
        // Kept back so the window is told whichever way this fails.
        let reply = output.clone();
        let result = self.worker(id).and_then(|sender| {
            sender
                .send(WorkerCommand::OpenTerminal {
                    connection,
                    input,
                    output,
                })
                .map_err(|_| "device worker stopped unexpectedly".to_owned())
        });
        if let Err(error) = &result {
            let _ = reply.send(TerminalEvent::Closed(error.clone()));
        }
        result
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

/// Each device keeps its own thread, its own single-threaded runtime, and the
/// one SSH connection that runtime holds open.
///
/// The runtime has to keep running between commands rather than be entered for
/// each one: russh drives a session on a task of its own, and a task on a
/// current-thread runtime only makes progress while that runtime does. A
/// connection left behind by a finished `block_on` would simply stop reading.
fn spawn_worker(
    id: String,
    receiver: tokio::sync::mpsc::UnboundedReceiver<WorkerCommand>,
    events: Sender<WorkerEvent>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Could not start the SSH runtime: {error}"))?;
    thread::Builder::new()
        .name(format!("ssh-{id}"))
        .spawn(move || runtime.block_on(worker_loop(id, receiver, events)))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// A console window's end of a session, kept so that the window can be told
/// when the connection underneath it goes.
struct Console {
    task: tokio::task::JoinHandle<()>,
    reply: Sender<TerminalEvent>,
    /// Which of the device's connections it was put on.
    generation: u64,
}

/// Runs one device: its queue, its console window, and the connection they
/// share.
///
/// Queued operations are done one at a time and in order, as they always have
/// been, but each is run as a task rather than awaited here, so that a console
/// window can still be opened while a firmware load is running. A console that
/// had to wait its turn would be useless at exactly the moment it is wanted.
async fn worker_loop(
    id: String,
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<WorkerCommand>,
    events: Sender<WorkerEvent>,
) {
    let device = SharedSession::default();
    let mut queue: VecDeque<WorkerCommand> = VecDeque::new();
    let mut running: Option<tokio::task::JoinHandle<()>> = None;
    let mut console: Option<Console> = None;
    // Set once this worker has been told to stop. What is still queued is
    // abandoned, but whatever is already running is waited for: a firmware
    // transfer cut off partway through would leave the device half written.
    let mut stopping = false;
    loop {
        if stopping && running.is_none() {
            break;
        }
        tokio::select! {
            command = receiver.recv(), if !stopping => match command {
                None | Some(WorkerCommand::Stop) => {
                    stopping = true;
                    queue.clear();
                }
                Some(WorkerCommand::OpenTerminal { connection, input, output }) => {
                    // One window per device: the application only asks for a
                    // second once the first has gone.
                    end_console(&mut console, None).await;
                    open_console(&device, &mut console, connection, input, output, &events).await;
                }
                Some(command) => queue.push_back(command),
            },
            Some(()) = finished(&mut running) => {
                running = None;
                let _ = events.send(WorkerEvent::JobFinished { id: id.clone() });
                // A connection replaced while that ran is not the one the window
                // is on, and two connections to one device is what this is all
                // meant to avoid.
                if let Some(open) = &console
                    && open.generation != device.generation().await
                {
                    end_console(
                        &mut console,
                        Some("The connection was reopened. Connect again."),
                    )
                    .await;
                }
            }
        }
        if !stopping
            && running.is_none()
            && let Some(command) = queue.pop_front()
        {
            let _ = events.send(WorkerEvent::Status {
                id: id.clone(),
                message: starting(&command),
                progress: None,
            });
            let (queued, reporter) = (device.clone(), events.clone());
            running = Some(tokio::spawn(async move {
                if let Err(error) = dispatch(command, &queued, &reporter).await {
                    report(&reporter, error, Announce::OnTheCard);
                }
            }));
        }
    }
    // All of this has to happen while the runtime is still running: closing a
    // channel and saying goodbye are both work, and there is nowhere left to do
    // it once `block_on` has returned.
    end_console(&mut console, Some("The device session was closed")).await;
    device.close().await;
}

/// What the device card says as an operation starts.
///
/// The card used to learn this from a connection being opened for every
/// operation. Now that one connection serves them all, there is usually nothing
/// to open, so the operation says what it is instead, which is more use anyway.
fn starting(command: &WorkerCommand) -> String {
    match command {
        WorkerCommand::Refresh(_) => "Reading device information".to_owned(),
        WorkerCommand::RunScript { name, .. } => format!("Running script {name}"),
        WorkerCommand::UploadProgram { slot, .. } => format!("Loading program slot {slot}"),
        WorkerCommand::UploadTouchpanel { .. } => "Loading touchpanel project".to_owned(),
        WorkerCommand::UploadConfig { .. } => "Loading configuration file".to_owned(),
        WorkerCommand::UploadFirmware { .. } => "Loading firmware".to_owned(),
        // Neither is ever queued.
        WorkerCommand::OpenTerminal { .. } | WorkerCommand::Stop => String::new(),
    }
}

/// Waits for the running job, or forever when there is none, so that the loop
/// can select on it either way.
async fn finished(running: &mut Option<tokio::task::JoinHandle<()>>) -> Option<()> {
    match running {
        Some(task) => {
            let _ = task.await;
            Some(())
        }
        None => std::future::pending().await,
    }
}

/// Ends a console session and, unless it is only being replaced, says why.
async fn end_console(console: &mut Option<Console>, reason: Option<&str>) {
    let Some(console) = console.take() else {
        return;
    };
    console.task.abort();
    // Waited for, so that the connection it was holding is actually let go
    // before anything tries to close that connection.
    let _ = console.task.await;
    if let Some(reason) = reason {
        let _ = console.reply.send(TerminalEvent::Closed(reason.to_owned()));
    }
}

/// Puts a window on the device's connection, opening one if it has none.
async fn open_console(
    device: &SharedSession,
    console: &mut Option<Console>,
    connection: ConnectionSpec,
    input: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    output: Sender<TerminalEvent>,
    events: &Sender<WorkerEvent>,
) {
    // Quietly: the device card belongs to the queued operations, and a console
    // window is not one of them.
    let (session, _) = match device.acquire(&connection, Announce::Quietly, events).await {
        Ok(ready) => ready,
        Err(error) => {
            let reason = report(events, error, Announce::Quietly);
            let _ = output.send(TerminalEvent::Closed(reason));
            return;
        }
    };
    let generation = device.generation().await;
    let reply = output.clone();
    let events = events.clone();
    let task = tokio::spawn(async move {
        let reason = match terminal_session(&connection, session, input, &output, &events).await {
            Ok(()) => String::new(),
            Err(error) => report(&events, error, Announce::Quietly),
        };
        let _ = output.send(TerminalEvent::Closed(reason));
    });
    *console = Some(Console {
        task,
        reply,
        generation,
    });
}

/// Says what went wrong, and answers with what the window or the card should
/// show.
///
/// An untrusted host key is always announced: offering to accept it is the only
/// way past it. A plain failure from a console window stays in the device log
/// and in that window, rather than restating the state of a device whose queued
/// operations have nothing to do with it.
fn report(events: &Sender<WorkerEvent>, error: ConnectError, announce: Announce) -> String {
    match error {
        ConnectError::UnknownHostKey { id, fingerprint } => {
            log(events, &id, Direction::Note, "Host key is not trusted");
            let _ = events.send(WorkerEvent::HostKeyUnknown { id, fingerprint });
            "The host key is not trusted yet. Accept it, then connect again.".to_owned()
        }
        ConnectError::CredentialsRejected { id, message } => {
            log(events, &id, Direction::Note, &message);
            // The card is told only when it is the card's business, exactly as
            // for a plain failure. The prompt, though, is raised whichever way
            // the connection was asked for: a console window's refusal is as
            // good a reason to ask for a password as a queued operation's.
            if announce == Announce::OnTheCard {
                let _ = events.send(WorkerEvent::Error {
                    id: id.clone(),
                    message: message.clone(),
                });
            }
            let _ = events.send(WorkerEvent::CredentialsRejected {
                id,
                message: message.clone(),
            });
            message
        }
        ConnectError::Message { id, message } => {
            log(events, &id, Direction::Note, &message);
            if announce == Announce::OnTheCard {
                let _ = events.send(WorkerEvent::Error {
                    id,
                    message: message.clone(),
                });
            }
            message
        }
    }
}

async fn dispatch(
    command: WorkerCommand,
    device: &SharedSession,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    match command {
        WorkerCommand::Refresh(connection) => refresh(&connection, device, events).await,
        WorkerCommand::RunScript {
            connection,
            name,
            commands,
        } => run_script(&connection, &name, &commands, device, events).await,
        WorkerCommand::UploadProgram {
            connection,
            local_path,
            signature,
            slot,
        } => {
            upload_program(
                &connection,
                &local_path,
                signature.as_deref(),
                slot,
                device,
                events,
            )
            .await
        }
        WorkerCommand::UploadTouchpanel {
            connection,
            local_path,
        } => {
            upload_staged(
                &connection,
                &local_path,
                touchpanel_transfer,
                device,
                events,
            )
            .await
        }
        WorkerCommand::UploadConfig {
            connection,
            local_path,
        } => upload_staged(&connection, &local_path, config_transfer, device, events).await,
        WorkerCommand::UploadFirmware {
            connection,
            local_path,
            remote_name,
        } => upload_firmware(&connection, &local_path, &remote_name, device, events).await,
        // Neither reaches here: the loop takes both before anything is queued.
        WorkerCommand::Stop | WorkerCommand::OpenTerminal { .. } => Ok(()),
    }
}

#[derive(Debug)]
enum ConnectError {
    UnknownHostKey {
        id: String,
        fingerprint: String,
    },
    /// The device would not take the username and password it was sent, or
    /// there was none to send. Reported apart from a plain failure so that the
    /// application can ask for them again, the way it asks about a host key.
    CredentialsRejected {
        id: String,
        message: String,
    },
    Message {
        id: String,
        message: String,
    },
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

fn rejected(spec: &ConnectionSpec, message: impl Into<String>) -> ConnectError {
    ConnectError::CredentialsRejected {
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

/// Whether opening a connection is the device card's business. A queued
/// operation announces itself there; a console window is not one of the queue's
/// operations and says nothing about the device's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Announce {
    OnTheCard,
    Quietly,
}

async fn connect(
    spec: &ConnectionSpec,
    announce: Announce,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<Handle<TrustOnFirstUse>> {
    if announce == Announce::OnTheCard {
        let _ = events.send(WorkerEvent::Connecting {
            id: spec.id.clone(),
        });
    }
    if spec.credentials.username.trim().is_empty() {
        return Err(rejected(spec, "No SSH username for this device"));
    }
    if spec.credentials.password.is_empty() {
        return Err(rejected(spec, "No SSH password for this device"));
    }

    log(
        events,
        &spec.id,
        Direction::Note,
        &format!("Connecting to {}:{}", spec.host, spec.port),
    );
    let stream = tcp_connect(spec).await?;
    let seen = SeenFingerprint::default();
    let session = open_session(spec, stream, SSH_TIMEOUT, &seen).await?;
    authenticate(spec, session).await
}

/// Opens the socket a session will be held on.
///
/// Resolving and connecting are both blocking, and the runtime they would block
/// now carries the device's held session and any console window on it, so they
/// are done away from it. Connecting with an explicit timeout is still what
/// makes an unreachable device fail in seconds rather than waiting out the
/// platform's own.
async fn tcp_connect(spec: &ConnectionSpec) -> WorkerResult<tokio::net::TcpStream> {
    let (host, port) = (spec.host.clone(), spec.port);
    let stream = tokio::task::spawn_blocking(move || resolve_and_connect(&host, port))
        .await
        .map_err(|error| message(spec, format!("Could not connect to {}: {error}", spec.host)))?
        .map_err(|reason| message(spec, reason))?;
    // A held connection is silent for as long as nothing is asked of it, so the
    // operating system is left to notice a peer that has gone away without
    // saying so. This is not an SSH message: the device's console sees nothing.
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_IDLE)
        .with_interval(TCP_KEEPALIVE_INTERVAL);
    socket2::SockRef::from(&stream)
        .set_tcp_keepalive(&keepalive)
        .map_err(|error| message(spec, error.to_string()))?;
    // Russh only applies its own `nodelay` when it opens the socket itself, and
    // this one is handed to it already open. It matters here because a console
    // somebody is typing into shares the socket with bulk file transfers.
    stream
        .set_nodelay(true)
        .map_err(|error| message(spec, error.to_string()))?;
    stream
        .set_nonblocking(true)
        .map_err(|error| message(spec, error.to_string()))?;
    tokio::net::TcpStream::from_std(stream).map_err(|error| message(spec, error.to_string()))
}

/// Tries every address the name resolves to, and reports the last failure if
/// none of them answer.
fn resolve_and_connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("Could not resolve {host}: {error}"))?;
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(format!(
        "Could not connect to {host}:{port}: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no address found".into())
    ))
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

/// How every connection to a device is configured.
///
/// A connection is held open between operations and shared with the console
/// window, so it spends most of its life saying nothing. Russh must not read
/// that silence as a fault, and no keepalive is sent to break it: the device is
/// left free to close an idle session on its own schedule.
///
/// `inactivity_timeout` is what bounds that silence. It is deliberately not
/// `None`: russh selects the socket flush against this same timer, so without
/// it a device that stopped reading would wedge the session with no timeout
/// anywhere. As a long idle limit it does both jobs at once.
fn client_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        inactivity_timeout: Some(IDLE_LIMIT),
        keepalive_interval: None,
        preferred: ssh_preferences(),
        ..Default::default()
    })
}

async fn open_session(
    spec: &ConnectionSpec,
    stream: tokio::net::TcpStream,
    timeout: Duration,
    seen: &SeenFingerprint,
) -> WorkerResult<Handle<TrustOnFirstUse>> {
    let config = client_config();
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
        // Only an answered refusal is the password's fault. A transport
        // failure during the exchange, or a timeout, says nothing about it and
        // must not send the application back to ask for it again.
        Ok(Ok(_)) => Err(rejected(spec, "SSH authentication was not accepted")),
        Ok(Err(error)) => Err(message(spec, format!("SSH authentication failed: {error}"))),
        Err(_) => Err(message(spec, "Timed out authenticating over SSH")),
    }
}

async fn disconnect(session: &Handle<TrustOnFirstUse>) {
    let _ = session
        .disconnect(Disconnect::ByApplication, "", "English")
        .await;
}

/// Whether a connection was already open when it was asked for. Only an
/// operation working on one that was can start again on a new one: a
/// connection just made cannot have been dropped before it was used, so a
/// failure on one is the operation's own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reuse {
    Held,
    Fresh,
}

/// The one SSH connection a device gets.
///
/// Every queued operation and the console window share it: it is opened the
/// first time something needs it and held afterwards, so that a refresh, a
/// script and ten program slots are one conversation with the device rather
/// than twelve. It is replaced only when the device has dropped it or the
/// settings it was opened with have stopped matching.
#[derive(Default)]
struct DeviceSession {
    open: Option<(ConnectionSpec, Arc<Handle<TrustOnFirstUse>>)>,
    /// Counts the connections this device has had. Anything still holding an
    /// older number is holding a connection that has since been replaced.
    generation: u64,
}

impl DeviceSession {
    async fn acquire(
        &mut self,
        spec: &ConnectionSpec,
        announce: Announce,
        events: &Sender<WorkerEvent>,
    ) -> WorkerResult<(Arc<Handle<TrustOnFirstUse>>, Reuse)> {
        if let Some((open_for, session)) = &self.open {
            if same_connection(open_for, spec) && !session.is_closed() {
                return Ok((session.clone(), Reuse::Held));
            }
            self.close().await;
        }
        let session = Arc::new(connect(spec, announce, events).await?);
        self.remember(spec, session.clone());
        Ok((session, Reuse::Fresh))
    }

    fn remember(&mut self, spec: &ConnectionSpec, session: Arc<Handle<TrustOnFirstUse>>) {
        self.generation = self.generation.wrapping_add(1);
        self.open = Some((spec.clone(), session));
    }

    /// Lets go of the connection without saying goodbye on it: either the
    /// device is restarting, or it has already taken the connection away.
    fn forget(&mut self) {
        if self.open.take().is_some() {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Says goodbye, so that the device frees the session for whoever wants it
    /// next rather than waiting out its own timeout.
    async fn close(&mut self) {
        let Some((_, session)) = self.open.take() else {
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        disconnect(&session).await;
        // The goodbye is only written while this runtime is running, and
        // closing is often the last thing it does. Waiting for the session to
        // end is what gets it onto the wire. A console window still holding the
        // connection means there is nothing to wait for here.
        if let Some(session) = Arc::into_inner(session) {
            let _ = tokio::time::timeout(FAREWELL, session).await;
        }
    }
}

/// The device's one connection, as its queue and its console window share it.
/// The lock is held while a connection is made or let go, never while an
/// operation runs, so that opening a console does not wait out a firmware load.
#[derive(Clone, Default)]
struct SharedSession(Arc<tokio::sync::Mutex<DeviceSession>>);

impl SharedSession {
    async fn acquire(
        &self,
        spec: &ConnectionSpec,
        announce: Announce,
        events: &Sender<WorkerEvent>,
    ) -> WorkerResult<(Arc<Handle<TrustOnFirstUse>>, Reuse)> {
        self.0.lock().await.acquire(spec, announce, events).await
    }

    async fn forget(&self) {
        self.0.lock().await.forget();
    }

    async fn remember(&self, spec: &ConnectionSpec, session: Arc<Handle<TrustOnFirstUse>>) {
        self.0.lock().await.remember(spec, session);
    }

    async fn close(&self) {
        self.0.lock().await.close().await;
    }

    async fn generation(&self) -> u64 {
        self.0.lock().await.generation
    }
}

/// What has to still be true for an open connection to be the one an operation
/// wants. The device's id is deliberately not compared: a discovered device
/// keeps its id when its address changes, and the address is what decides the
/// connection. The trusted fingerprint is compared because forgetting a host
/// key promises that the next connection asks about it again, and a held
/// connection would otherwise sail straight past the question.
fn same_connection(open_for: &ConnectionSpec, wanted: &ConnectionSpec) -> bool {
    open_for.host == wanted.host
        && open_for.port == wanted.port
        && open_for.credentials.username == wanted.credentials.username
        && open_for.credentials.password == wanted.credentials.password
        && open_for.trusted_fingerprint == wanted.trusted_fingerprint
}

async fn refresh(
    spec: &ConnectionSpec,
    device: &SharedSession,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    let (mut session, reuse) = device.acquire(spec, Announce::OnTheCard, events).await?;
    // A connection the device dropped while nothing was being asked of it looks
    // open until something is. The first query is where that shows, and every
    // query here is a question rather than a change, so a held connection that
    // turns out to have gone is replaced and the report simply asked for again.
    let mut disk_free = run_command(spec, &session, "free", SSH_TIMEOUT, events).await;
    if disk_free.is_err() && reuse == Reuse::Held {
        log(
            events,
            &spec.id,
            Direction::Note,
            "The open connection had gone; opening another",
        );
        device.forget().await;
        (session, _) = device.acquire(spec, Announce::OnTheCard, events).await?;
        disk_free = run_command(spec, &session, "free", SSH_TIMEOUT, events).await;
    }
    let details = DeviceDetails {
        disk_free: section_result(disk_free),
        ram_free: section_result(run_command(spec, &session, "ramfree", SSH_TIMEOUT, events).await),
        identity: format!(
            "{}\n\n{}",
            section_result(run_command(spec, &session, "hostname", SSH_TIMEOUT, events).await),
            section_result(run_command(spec, &session, "ver -v", SSH_TIMEOUT, events).await)
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
    device: &SharedSession,
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
    // Deliberately not started again on a new connection if it fails: a script
    // is whatever somebody wrote, and running part of one twice is worse than
    // reporting that it stopped.
    let (session, _) = device.acquire(spec, Announce::OnTheCard, events).await?;
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

/// Carries an interactive shell between the device and its window.
///
/// This runs as a task beside the device's queue rather than in it: a session
/// somebody is typing into lasts as long as they want it to, and must not hold
/// up the queued operations or stop the application closing. It is a channel on
/// the device's one connection, not a connection of its own.
///
/// Everything typed and everything received also reaches the device log, so a
/// terminal leaves the same record as any other operation.
async fn terminal_session(
    spec: &ConnectionSpec,
    session: Arc<Handle<TrustOnFirstUse>>,
    mut input: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    output: &Sender<TerminalEvent>,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
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
    // Only the shell channel is given back. The connection under it belongs to
    // the device, and whatever else is using it goes on doing so.
    let _ = channel.close().await;
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
    device: &SharedSession,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    let file_name = safe_remote_file_name(local_path)
        .ok_or_else(|| message(spec, "The selected file has an unsafe or missing file name"))?;
    let (remote_path, command) = transfer(&file_name);
    let transfer = Transfer::new(local_path, remote_path, file_name);
    upload_files(
        spec,
        vec![transfer],
        Apply::from(command),
        None,
        device,
        events,
    )
    .await
}

/// A program is staged with its signature: the processor reads the signature
/// from the same directory, under the program's name with a `.zig` extension.
async fn upload_program(
    spec: &ConnectionSpec,
    local_path: &Path,
    signature: Option<&Path>,
    slot: u8,
    device: &SharedSession,
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
        None,
        device,
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
    device: &SharedSession,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    let Some((remote_path, apply)) = firmware_transfer(remote_name) else {
        return Err(message(
            spec,
            "The assigned firmware has an unsafe or missing file name",
        ));
    };
    let transfer = Transfer::new(local_path, remote_path, remote_name.to_owned());
    // Reading and unpacking the file blocks, and the runtime it would block now
    // carries the device's held connection and any console window on it.
    let package = local_path.to_owned();
    let archive = tokio::task::spawn_blocking(move || crate::archive::read(&package))
        .await
        .map_err(|error| message(spec, format!("Firmware blocked: {error}")))?
        .map_err(|error| {
            message(
                spec,
                format!("Firmware blocked: cannot read package metadata: {error}"),
            )
        })?;
    let version = crate::firmware_version::Version::from_package(&archive.package)
        .map_err(|error| message(spec, error))?;
    upload_files(spec, vec![transfer], apply, Some(version), device, events).await
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
    session: Arc<Handle<TrustOnFirstUse>>,
    device: &SharedSession,
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
    // Let go of it before saying goodbye, so that nothing waiting on this
    // device is handed a connection to a processor that is about to restart.
    device.forget().await;
    disconnect(&session).await;
    drop(session);

    // Asked for directly rather than through the device's held connection:
    // there is deliberately nothing held at this point, and every attempt is
    // expected to fail until the device answers.
    let restored = Arc::new(await_restart(spec, timings, events).await?);
    // The connection it came back on is the one it keeps, so a refresh after a
    // firmware load does not have to log in again.
    device.remember(spec, restored.clone()).await;
    let results = read_puf_results(spec, &restored, timings, events).await;
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
        match connect(spec, Announce::OnTheCard, events).await {
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
            // A device part-way through its restart can answer before it will
            // take a password, so a refusal here is waited out like any other
            // failure rather than given up on. The credentials themselves were
            // good enough to start this load.
            Err(
                ConnectError::Message {
                    message: reason, ..
                }
                | ConnectError::CredentialsRejected {
                    message: reason, ..
                },
            ) => {
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

/// How far the preparation for a transfer got. A firmware package the device
/// already has calls the whole load off before anything is sent.
enum Staged {
    Ready(SftpSession),
    UpToDate(String),
}

/// Everything a transfer needs before its first byte: the firmware check, when
/// there is one, and the SFTP session to write through. Nothing here changes
/// anything on the device, which is what makes it safe to do twice.
async fn stage(
    spec: &ConnectionSpec,
    session: &Handle<TrustOnFirstUse>,
    firmware_version: Option<&crate::firmware_version::Version>,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<Staged> {
    // Query on this upload's authenticated session, not cached discovery or
    // Device Details. No SFTP channel or remote file exists until this passes.
    if let Some(version) = firmware_version {
        let check = run_command(spec, session, "ver -v", SSH_TIMEOUT, events)
            .await
            .map_err(|error| {
                format!("Firmware blocked: could not query device PUF version: {error}")
            })
            .and_then(|report| version.check_upgrade(&report));
        match check {
            Ok(crate::firmware_version::UpgradeCheck::Needed(summary)) => {
                log(events, &spec.id, Direction::Note, &summary);
            }
            Ok(crate::firmware_version::UpgradeCheck::NotNeeded(summary)) => {
                log(events, &spec.id, Direction::Note, &summary);
                return Ok(Staged::UpToDate(summary));
            }
            Err(error) => return Err(message(spec, error)),
        }
    }
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
    Ok(Staged::Ready(sftp))
}

/// Stages every file over one SFTP session and then runs the load command once,
/// so a multi-file load is a single conversation with the device and reports a
/// single progress bar.
async fn upload_files(
    spec: &ConnectionSpec,
    transfers: Vec<Transfer>,
    apply: Apply,
    firmware_version: Option<crate::firmware_version::Version>,
    device: &SharedSession,
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

    let (mut session, reuse) = device.acquire(spec, Announce::OnTheCard, events).await?;
    // Everything up to the first byte written can be done again: the firmware
    // check only asks a question, and no remote file exists until the transfer
    // starts. So a held connection that turns out to have gone costs another
    // login here rather than a failed load.
    let mut staged = stage(spec, &session, firmware_version.as_ref(), events).await;
    if staged.is_err() && reuse == Reuse::Held {
        log(
            events,
            &spec.id,
            Direction::Note,
            "The open connection had gone; opening another",
        );
        device.forget().await;
        (session, _) = device.acquire(spec, Announce::OnTheCard, events).await?;
        staged = stage(spec, &session, firmware_version.as_ref(), events).await;
    }
    let sftp = match staged? {
        Staged::Ready(sftp) => sftp,
        Staged::UpToDate(summary) => {
            let _ = events.send(WorkerEvent::FirmwareUpToDate {
                id: spec.id.clone(),
                message: summary,
            });
            return Ok(());
        }
    };

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
        Apply::Nothing => format!("Uploaded {display_name}"),
        Apply::Command(command) => {
            let output = run_command(spec, &session, &command, LOAD_TIMEOUT, events)
                .await
                .map_err(|error| message(spec, format!("Load command failed: {error}")))?;
            if output.trim().is_empty() {
                format!("Uploaded {display_name}; command completed")
            } else {
                format!("Uploaded {display_name}: {}", output.trim())
            }
        }
        // Takes over the connection: the device restarts partway through and is
        // reconnected to before it will say how the update went.
        Apply::Puf => {
            let summary = apply_puf(spec, session, device, Timings::DEVICE, events).await?;
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
#[path = "firmware_preflight_tests.rs"]
mod firmware_preflight_tests;

#[cfg(test)]
#[path = "resource_live_tests.rs"]
mod resource_live_tests;

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

    /// A loopback console that answers commands and counts the connections it
    /// is given, so that a test can tell one login from several.
    #[derive(Clone)]
    struct CountingServer {
        received: Arc<Mutex<Vec<String>>>,
        /// Whether to drop the connection once a command has been answered,
        /// standing for a device that lets an idle session go.
        close_when_answered: bool,
    }

    impl russh::server::Handler for CountingServer {
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
            _term: &str,
            _col_width: u32,
            _row_height: u32,
            _pix_width: u32,
            _pix_height: u32,
            _modes: &[(russh::Pty, u32)],
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            Ok(())
        }

        async fn shell_request(
            &mut self,
            channel: russh::ChannelId,
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            session.data(channel, "console>".to_owned())?;
            Ok(())
        }

        async fn data(
            &mut self,
            channel: russh::ChannelId,
            data: &[u8],
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            let typed = String::from_utf8_lossy(data).into_owned();
            self.received.lock().unwrap().push(format!("typed:{typed}"));
            session.data(channel, format!("{typed}\nconsole>"))?;
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: russh::ChannelId,
            data: &[u8],
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            let command = String::from_utf8_lossy(data).into_owned();
            self.received.lock().unwrap().push(command.clone());
            session.channel_success(channel)?;
            session.data(channel, format!("output for {command}"))?;
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
            if self.close_when_answered {
                session.disconnect(Disconnect::ByApplication, "idle", "")?;
            }
            Ok(())
        }
    }

    /// A running loopback server and what it has seen.
    struct Loopback {
        port: u16,
        fingerprint: String,
        connections: Arc<std::sync::atomic::AtomicUsize>,
        received: Arc<Mutex<Vec<String>>>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Loopback {
        /// Accepts in a loop and counts, so that a second connection is
        /// something a test can see rather than merely fail to observe.
        async fn start(seed: u8, close_when_answered: bool) -> Self {
            let key = russh::keys::PrivateKey::from(
                russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]),
            );
            let fingerprint = host_fingerprint(key.public_key()).unwrap();
            let config = Arc::new(russh::server::Config {
                keys: vec![key],
                auth_rejection_time: Duration::ZERO,
                ..Default::default()
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let received = Arc::new(Mutex::new(Vec::new()));
            let server = tokio::spawn({
                let (connections, received) = (connections.clone(), received.clone());
                async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let config = config.clone();
                        let handler = CountingServer {
                            received: received.clone(),
                            close_when_answered,
                        };
                        tokio::spawn(async move {
                            if let Ok(session) =
                                russh::server::run_stream(config, stream, handler).await
                            {
                                let _ = session.await;
                            }
                        });
                    }
                }
            });
            Self {
                port,
                fingerprint,
                connections,
                received,
                server,
            }
        }

        fn spec(&self) -> ConnectionSpec {
            let mut spec = spec();
            spec.port = self.port;
            spec.trusted_fingerprint = Some(self.fingerprint.clone());
            spec
        }

        fn connections(&self) -> usize {
            self.connections.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn received(&self) -> Vec<String> {
            self.received.lock().unwrap().clone()
        }
    }

    /// The point of the whole arrangement: a device is logged in to once, and
    /// everything afterwards goes over that one connection.
    #[test]
    fn operations_after_the_first_reuse_the_one_connection() {
        runtime().block_on(async {
            let device_server = Loopback::start(21, false).await;
            let spec = device_server.spec();
            let (events, _receiver) = mpsc::channel();
            let device = SharedSession::default();

            for _ in 0..3 {
                run_script(&spec, "Test", &["hostname".into()], &device, &events)
                    .await
                    .unwrap();
            }
            assert_eq!(
                device_server.connections(),
                1,
                "each operation logged in again instead of reusing the connection"
            );

            refresh(&spec, &device, &events).await.unwrap();
            assert_eq!(
                device_server.connections(),
                1,
                "a refresh after a script opened a second connection"
            );
            assert_eq!(
                device_server
                    .received()
                    .iter()
                    .filter(|command| *command == "hostname")
                    .count(),
                4,
                "the script ran three times and the refresh asks for the hostname once"
            );
            device_server.server.abort();
        });
    }

    /// Settings that decide the connection are what decide whether it can be
    /// kept. Forgetting a host key promises that the next connection asks about
    /// it again, and a held one would otherwise sail straight past the question.
    #[test]
    fn a_connection_is_not_reused_once_its_settings_change() {
        let base = ConnectionSpec {
            id: "kept".into(),
            host: "192.0.2.1".into(),
            port: 22,
            credentials: Credentials {
                username: "admin".into(),
                password: "secret".into(),
            },
            trusted_fingerprint: Some("SHA256:aaa".into()),
        };
        assert!(same_connection(&base, &base.clone()));
        // The id is deliberately not compared: a discovered device keeps its id
        // when its address changes, and the address is what decides this.
        let mut renamed = base.clone();
        renamed.id = "something else".into();
        assert!(same_connection(&base, &renamed));
        for changed in [
            ConnectionSpec {
                host: "192.0.2.2".into(),
                ..base.clone()
            },
            ConnectionSpec {
                port: 2222,
                ..base.clone()
            },
            ConnectionSpec {
                credentials: Credentials {
                    username: "other".into(),
                    password: "secret".into(),
                },
                ..base.clone()
            },
            ConnectionSpec {
                credentials: Credentials {
                    username: "admin".into(),
                    password: "changed".into(),
                },
                ..base.clone()
            },
            ConnectionSpec {
                trusted_fingerprint: None,
                ..base.clone()
            },
        ] {
            assert!(
                !same_connection(&base, &changed),
                "{changed:?} should have needed a new connection"
            );
        }
    }

    /// And over the wire: an edited password is a different connection.
    #[test]
    fn an_edited_password_opens_another_connection() {
        runtime().block_on(async {
            let device_server = Loopback::start(22, false).await;
            let mut spec = device_server.spec();
            let (events, _receiver) = mpsc::channel();
            let device = SharedSession::default();

            run_script(&spec, "Test", &["hostname".into()], &device, &events)
                .await
                .unwrap();
            spec.credentials.password.push('!');
            run_script(&spec, "Test", &["hostname".into()], &device, &events)
                .await
                .unwrap();
            assert_eq!(
                device_server.connections(),
                2,
                "the connection was kept even though the password had changed"
            );
            device_server.server.abort();
        });
    }

    /// A device that lets an idle session go is reconnected to rather than
    /// reported as a failure.
    #[test]
    fn a_connection_the_device_dropped_is_replaced() {
        runtime().block_on(async {
            let device_server = Loopback::start(23, true).await;
            let spec = device_server.spec();
            let (events, _receiver) = mpsc::channel();
            let device = SharedSession::default();

            refresh(&spec, &device, &events).await.unwrap();
            // The goodbye has to be read before the next operation asks for the
            // connection, which is what makes it visibly closed rather than
            // merely dead.
            tokio::time::sleep(Duration::from_millis(200)).await;

            refresh(&spec, &device, &events).await.unwrap();
            assert_eq!(
                device_server.connections(),
                2,
                "a dropped connection has to be replaced, not reused"
            );
            device_server.server.abort();
        });
    }

    /// A console window and the queued operations share the device's one
    /// connection, and the window never counts as a queued operation.
    #[test]
    fn a_console_window_shares_the_connection_and_never_holds_up_quitting() {
        // The window side is blocking, so the server needs a thread of its own.
        let (ready, started) = mpsc::channel();
        let (finish, finished) = mpsc::channel::<()>();
        thread::spawn(move || {
            runtime().block_on(async move {
                let device_server = Loopback::start(24, false).await;
                ready
                    .send((
                        device_server.port,
                        device_server.fingerprint.clone(),
                        device_server.connections.clone(),
                        device_server.received.clone(),
                    ))
                    .unwrap();
                // Kept answering until the test says it is done.
                let _ = tokio::task::spawn_blocking(move || finished.recv()).await;
                device_server.server.abort();
            });
        });
        let (port, fingerprint, connections, received) = started.recv().unwrap();

        let mut spec = spec();
        spec.port = port;
        spec.trusted_fingerprint = Some(fingerprint);
        let (events, worker_events) = mpsc::channel();
        let (input, from_window) = tokio::sync::mpsc::unbounded_channel();
        let (to_window, output) = mpsc::channel();
        let mut pool = WorkerPool::new(events);
        let id = spec.id.clone();

        pool.open_terminal(&id, spec.clone(), from_window, to_window)
            .unwrap();
        assert!(matches!(
            output.recv_timeout(Duration::from_secs(10)).unwrap(),
            TerminalEvent::Opened
        ));
        assert!(
            !pool.has_pending(),
            "a console window must not hold up quitting"
        );

        // A queued operation, while that window is open.
        pool.send(
            &id,
            WorkerCommand::RunScript {
                connection: spec,
                name: "Test".into(),
                commands: vec!["hostname".into()],
            },
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match worker_events.recv_timeout(Duration::from_secs(10)).unwrap() {
                WorkerEvent::JobFinished { .. } => break,
                _ if std::time::Instant::now() > deadline => panic!("the script never finished"),
                _ => continue,
            }
        }
        assert_eq!(
            connections.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the console and the script have to be on one connection"
        );

        // And the window still works afterwards.
        input.send(b"hostname\r".to_vec()).unwrap();
        let mut shown = String::new();
        while !shown.contains("hostname") {
            match output.recv_timeout(Duration::from_secs(10)).unwrap() {
                TerminalEvent::Output(data) => shown.push_str(&String::from_utf8_lossy(&data)),
                other => panic!("{other:?}"),
            }
        }
        assert!(
            received
                .lock()
                .unwrap()
                .iter()
                .any(|seen| seen == "hostname"),
            "the queued script did not reach the device"
        );
        let _ = finish.send(());
    }

    /// Retiring a device closes its connection and does not leave the window
    /// believing it still has one.
    #[test]
    fn retiring_a_device_tells_its_console_window() {
        let (ready, started) = mpsc::channel();
        let (finish, finished) = mpsc::channel::<()>();
        thread::spawn(move || {
            runtime().block_on(async move {
                let device_server = Loopback::start(25, false).await;
                ready
                    .send((device_server.port, device_server.fingerprint.clone()))
                    .unwrap();
                let _ = tokio::task::spawn_blocking(move || finished.recv()).await;
                device_server.server.abort();
            });
        });
        let (port, fingerprint) = started.recv().unwrap();

        let mut spec = spec();
        spec.port = port;
        spec.trusted_fingerprint = Some(fingerprint);
        let (events, _worker_events) = mpsc::channel();
        let (_input, from_window) = tokio::sync::mpsc::unbounded_channel();
        let (to_window, output) = mpsc::channel();
        let mut pool = WorkerPool::new(events);
        let id = spec.id.clone();

        pool.open_terminal(&id, spec, from_window, to_window)
            .unwrap();
        assert!(matches!(
            output.recv_timeout(Duration::from_secs(10)).unwrap(),
            TerminalEvent::Opened
        ));
        pool.retire(&id).unwrap();

        let closed = loop {
            match output.recv_timeout(Duration::from_secs(10)).unwrap() {
                TerminalEvent::Closed(reason) => break reason,
                TerminalEvent::Output(_) | TerminalEvent::Opened => continue,
            }
        };
        assert!(
            !closed.is_empty(),
            "a window whose device was retired has to be told why"
        );
        let _ = finish.send(());
    }

    /// The one setting the whole arrangement depends on. A connection that is
    /// held open is silent, and russh must not read that silence as a fault,
    /// but the timer cannot simply be removed: it is also the only bound on a
    /// write to a device that has stopped reading.
    #[test]
    fn a_held_connection_is_silent_but_not_kept_forever() {
        let config = client_config();
        assert_eq!(config.inactivity_timeout, Some(IDLE_LIMIT));
        assert!(
            config.keepalive_interval.is_none(),
            "a held connection sends nothing while it is idle"
        );
        assert!(
            IDLE_LIMIT > SSH_TIMEOUT,
            "an idle limit is not an operation timeout"
        );
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
                let result = tokio::time::timeout(Duration::from_secs(5), run_script(&spec, "Test", &commands, &SharedSession::default(), &events)).await.unwrap();
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
            let stream = tcp_connect(&spec).await.unwrap();
            match open_session(&spec, stream, SSH_TIMEOUT, &seen).await {
                Ok(session) => {
                    println!("Handshake completed; fingerprint: {:?}", seen.get());
                    disconnect(&session).await;
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
            match connect(&blank, Announce::OnTheCard, &events).await {
                Ok(_) => panic!("a blank password cannot connect"),
                Err(error) => error,
            }
        });
        let ConnectError::CredentialsRejected { message, .. } = error else {
            panic!("expected a rejection the application can ask about");
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
            // Opened through the pool, on the connection the device's queued
            // operations would use, rather than on one of its own.
            let mut pool = WorkerPool::new(events);
            let id = spec.id.clone();
            pool.open_terminal(&id, spec, from_window, to_window)
                .unwrap();
            assert!(
                !pool.has_pending(),
                "a console window must not count as a queued operation"
            );

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
            let device = SharedSession::default();
            let (session, _) = device
                .acquire(&spec, Announce::OnTheCard, &events)
                .await
                .unwrap();
            let summary = tokio::time::timeout(
                Duration::from_secs(20),
                apply_puf(&spec, session, &device, timings, &events),
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
