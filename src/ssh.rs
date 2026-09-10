use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use sha2::{Digest, Sha256};
use ssh2::Session;

use crate::model::{Credentials, DeviceDetails};

const SSH_TIMEOUT_MS: u32 = 20_000;
const LOAD_TIMEOUT_MS: u32 = 300_000;

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
    UploadProgram {
        connection: ConnectionSpec,
        local_path: PathBuf,
        slot: u8,
    },
    UploadTouchpanel {
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
    Connecting { id: String },
    HostKeyUnknown { id: String, fingerprint: String },
    Details { id: String, details: DeviceDetails },
    Progress { id: String, sent: u64, total: u64 },
    Complete { id: String, message: String },
    Error { id: String, message: String },
    JobFinished { id: String },
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

fn spawn_worker(
    id: String,
    receiver: Receiver<WorkerCommand>,
    events: Sender<WorkerEvent>,
) -> Result<(), String> {
    thread::Builder::new()
        .name(format!("ssh-{id}"))
        .spawn(move || {
            while let Ok(command) = receiver.recv() {
                if matches!(command, WorkerCommand::Stop) {
                    break;
                }
                let result = match command {
                    WorkerCommand::Refresh(connection) => refresh(&connection, &events),
                    WorkerCommand::UploadProgram {
                        connection,
                        local_path,
                        slot,
                    } => upload(
                        &connection,
                        &local_path,
                        format!("progload -p:{slot} {{file}}"),
                        &events,
                    ),
                    WorkerCommand::UploadTouchpanel {
                        connection,
                        local_path,
                    } => upload(
                        &connection,
                        &local_path,
                        "projectload {file}".into(),
                        &events,
                    ),
                    WorkerCommand::UploadFirmware {
                        connection,
                        local_path,
                        remote_name,
                    } => upload_firmware(&connection, &local_path, &remote_name, &events),
                    WorkerCommand::Stop => break,
                };
                if let Err(error) = result {
                    match error {
                        ConnectError::UnknownHostKey { id, fingerprint } => {
                            let _ = events.send(WorkerEvent::HostKeyUnknown { id, fingerprint });
                        }
                        ConnectError::Message { id, message } => {
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

#[derive(Debug)]
enum ConnectError {
    UnknownHostKey { id: String, fingerprint: String },
    Message { id: String, message: String },
}

type WorkerResult<T> = Result<T, ConnectError>;

fn message(spec: &ConnectionSpec, message: impl Into<String>) -> ConnectError {
    ConnectError::Message {
        id: spec.id.clone(),
        message: message.into(),
    }
}

fn connect(spec: &ConnectionSpec, events: &Sender<WorkerEvent>) -> WorkerResult<Session> {
    let _ = events.send(WorkerEvent::Connecting {
        id: spec.id.clone(),
    });
    if spec.credentials.username.trim().is_empty() {
        return Err(message(spec, "Enter an SSH username before connecting"));
    }
    if spec.credentials.password.is_empty() {
        return Err(message(spec, "Enter the SSH password for this session"));
    }

    let addresses = (spec.host.as_str(), spec.port)
        .to_socket_addrs()
        .map_err(|error| message(spec, format!("Could not resolve {}: {error}", spec.host)))?;
    let mut last_error = None;
    let mut stream = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, Duration::from_secs(6)) {
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
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|error| message(spec, error.to_string()))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(20)))
        .map_err(|error| message(spec, error.to_string()))?;

    let mut session =
        timed_session(stream, SSH_TIMEOUT_MS).map_err(|error| message(spec, error.to_string()))?;
    session
        .handshake()
        .map_err(|error| message(spec, format!("SSH handshake failed: {error}")))?;
    let host_key = session
        .host_key()
        .ok_or_else(|| message(spec, "The device did not provide an SSH host key"))?
        .0;
    let fingerprint = sha256_fingerprint(host_key);
    match spec.trusted_fingerprint.as_deref() {
        None => {
            return Err(ConnectError::UnknownHostKey {
                id: spec.id.clone(),
                fingerprint,
            });
        }
        Some(expected) if expected != fingerprint => {
            return Err(message(
                spec,
                format!(
                    "SSH host key changed. Expected {expected}, received {fingerprint}. Connection refused"
                ),
            ));
        }
        Some(_) => {}
    }

    session
        .userauth_password(spec.credentials.username.trim(), &spec.credentials.password)
        .map_err(|error| message(spec, format!("SSH authentication failed: {error}")))?;
    if !session.authenticated() {
        return Err(message(spec, "SSH authentication was not accepted"));
    }
    Ok(session)
}

fn timed_session(stream: TcpStream, timeout_ms: u32) -> Result<Session, ssh2::Error> {
    let mut session = Session::new()?;
    session.set_timeout(timeout_ms);
    session.set_tcp_stream(stream);
    Ok(session)
}

fn refresh(spec: &ConnectionSpec, events: &Sender<WorkerEvent>) -> WorkerResult<()> {
    let session = connect(spec, events)?;
    let hostname = run_command(&session, "hostname");
    let version = run_command(&session, "ver");
    let details = DeviceDetails {
        identity: format!(
            "{}\n\n{}",
            section_result(hostname),
            section_result(version)
        ),
        network: section_result(run_command(&session, "ipconfig")),
        programs: section_result(run_command(&session, "proginf")),
        ip_table: section_result(run_command(&session, "ipt -t")),
        cresnet: optional_section(run_command(&session, "REPORTCRESNET")),
    };
    events
        .send(WorkerEvent::Details {
            id: spec.id.clone(),
            details,
        })
        .map_err(|error| message(spec, error.to_string()))?;
    Ok(())
}

fn upload(
    spec: &ConnectionSpec,
    local_path: &Path,
    command_template: String,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    let file_name = safe_remote_file_name(local_path)
        .ok_or_else(|| message(spec, "The selected file has an unsafe or missing file name"))?;
    let command = command_template.replace("{file}", &file_name);
    upload_to(
        spec,
        local_path,
        Path::new(&file_name),
        &file_name,
        command,
        events,
    )
}

fn upload_firmware(
    spec: &ConnectionSpec,
    local_path: &Path,
    remote_name: &str,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    let Some((remote_path, command)) = firmware_transfer(remote_name) else {
        return Err(message(
            spec,
            "The assigned firmware has an unsafe or missing file name",
        ));
    };
    upload_to(spec, local_path, &remote_path, remote_name, command, events)
}

fn firmware_transfer(remote_name: &str) -> Option<(PathBuf, String)> {
    if !is_safe_remote_file_name(remote_name) {
        return None;
    }
    let remote_path = PathBuf::from("/firmware").join(remote_name);
    let command = if Path::new(remote_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        "pushupdate full".to_owned()
    } else {
        format!(r"puf \romdisk\user\system\{remote_name}")
    };
    Some((remote_path, command))
}

fn upload_to(
    spec: &ConnectionSpec,
    local_path: &Path,
    remote_path: &Path,
    display_name: &str,
    command: String,
    events: &Sender<WorkerEvent>,
) -> WorkerResult<()> {
    let mut local = File::open(local_path).map_err(|error| {
        message(
            spec,
            format!("Could not open {}: {error}", local_path.display()),
        )
    })?;
    let total = local
        .metadata()
        .map_err(|error| message(spec, error.to_string()))?
        .len();
    let session = connect(spec, events)?;
    let sftp = session
        .sftp()
        .map_err(|error| message(spec, format!("Could not start SFTP: {error}")))?;
    let mut remote = sftp.create(remote_path).map_err(|error| {
        message(
            spec,
            format!(
                "Could not create remote file {}: {error}",
                remote_path.display()
            ),
        )
    })?;

    let mut buffer = [0_u8; 64 * 1024];
    let mut sent = 0_u64;
    loop {
        let length = local
            .read(&mut buffer)
            .map_err(|error| message(spec, format!("Could not read local file: {error}")))?;
        if length == 0 {
            break;
        }
        remote
            .write_all(&buffer[..length])
            .map_err(|error| message(spec, format!("SFTP upload failed: {error}")))?;
        sent += length as u64;
        let _ = events.send(WorkerEvent::Progress {
            id: spec.id.clone(),
            sent,
            total,
        });
    }
    remote
        .close()
        .map_err(|error| message(spec, format!("Could not close remote file: {error}")))?;

    session.set_timeout(LOAD_TIMEOUT_MS);
    let output = run_command(&session, &command)
        .map_err(|error| message(spec, format!("Load command failed: {error}")))?;
    events
        .send(WorkerEvent::Complete {
            id: spec.id.clone(),
            message: if output.trim().is_empty() {
                format!("Uploaded {display_name}; command completed")
            } else {
                format!("Uploaded {display_name}: {}", output.trim())
            },
        })
        .map_err(|error| message(spec, error.to_string()))?;
    Ok(())
}

fn run_command(session: &Session, command: &str) -> Result<String, String> {
    let mut channel = session
        .channel_session()
        .map_err(|error| error.to_string())?;
    channel.exec(command).map_err(|error| error.to_string())?;
    let mut stdout = String::new();
    channel
        .read_to_string(&mut stdout)
        .map_err(|error| error.to_string())?;
    let mut stderr = String::new();
    channel
        .stderr()
        .read_to_string(&mut stderr)
        .map_err(|error| error.to_string())?;
    channel.wait_close().map_err(|error| error.to_string())?;
    let status = channel.exit_status().map_err(|error| error.to_string())?;
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

fn is_safe_remote_file_name(name: &str) -> bool {
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

    #[test]
    fn stalled_ssh_handshake_times_out() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_silent_peer, _) = listener.accept().unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut session = timed_session(stream, 100).unwrap();
            assert_eq!(session.timeout(), 100);
            sender.send(session.handshake().is_err()).unwrap();
        });
        assert!(receiver.recv_timeout(Duration::from_secs(3)).unwrap());
        worker.join().unwrap();
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
                receiver.recv_timeout(Duration::from_secs(2)).unwrap()
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
    fn validates_remote_file_names() {
        assert_eq!(
            safe_remote_file_name(Path::new("/tmp/room-program_1.lpz")).as_deref(),
            Some("room-program_1.lpz")
        );
        assert!(safe_remote_file_name(Path::new("/tmp/room program.lpz")).is_none());
        assert!(!is_safe_remote_file_name("."));
        assert!(!is_safe_remote_file_name(".."));
        assert!(!is_safe_remote_file_name("folder/device.puf"));
    }

    #[test]
    fn builds_crestron_firmware_transfer_paths_and_commands() {
        assert_eq!(
            firmware_transfer("rmc4_2.8000.00001.puf"),
            Some((
                PathBuf::from("/firmware/rmc4_2.8000.00001.puf"),
                r"puf \romdisk\user\system\rmc4_2.8000.00001.puf".into(),
            ))
        );
        assert_eq!(
            firmware_transfer("update.ZIP"),
            Some((
                PathBuf::from("/firmware/update.ZIP"),
                "pushupdate full".into(),
            ))
        );
        assert!(firmware_transfer("unsafe firmware.puf").is_none());
    }

    #[test]
    fn encodes_sha256_fingerprint_without_padding() {
        assert_eq!(
            sha256_fingerprint(b"test"),
            "SHA256:n4bQgYhMfWWaL+qgxVrQFaO/TxsrC4Is0V1sFbDwCgg"
        );
    }
}
