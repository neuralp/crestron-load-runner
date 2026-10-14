//! Loopback SSH/SFTP tests for the order of processor program loads.
use super::*;
use crate::test_support::TestDir;
use russh_sftp::protocol::{FileAttributes, Handle as SftpHandle, OpenFlags, Status, StatusCode};

type Steps = Arc<Mutex<Vec<String>>>;

struct ProgramServer {
    channels: std::collections::HashMap<russh::ChannelId, russh::Channel<russh::server::Msg>>,
    steps: Steps,
    fail_load: bool,
    fail_signature: bool,
}

impl russh::server::Handler for ProgramServer {
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
        channel: russh::Channel<russh::server::Msg>,
        reply: russh::server::ChannelOpenHandle,
        _: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: russh::ChannelId,
        name: &str,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        assert_eq!(name, "sftp");
        session.channel_success(id)?;
        let channel = self.channels.remove(&id).unwrap();
        russh_sftp::server::run(
            channel.into_stream(),
            SignatureServer {
                steps: self.steps.clone(),
                fail_signature: self.fail_signature,
            },
        )
        .await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        id: russh::ChannelId,
        data: &[u8],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        self.steps.lock().unwrap().push(format!("exec:{command}"));
        session.channel_success(id)?;
        session.data(id, "Program load complete".to_owned())?;
        session.exit_status_request(id, u32::from(self.fail_load))?;
        session.eof(id)?;
        session.close(id)?;
        Ok(())
    }
}

struct SignatureServer {
    steps: Steps,
    fail_signature: bool,
}

impl russh_sftp::server::Handler for SignatureServer {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        _: OpenFlags,
        _: FileAttributes,
    ) -> Result<SftpHandle, Self::Error> {
        self.steps.lock().unwrap().push(format!("open:{filename}"));
        if self.fail_signature && filename.ends_with(".zig") {
            return Err(StatusCode::Failure);
        }
        Ok(SftpHandle {
            id,
            handle: filename,
        })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        _: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        // A signature arrives zipped, as SIMPL leaves it; record what it holds.
        let data = if handle.ends_with(".zig") {
            let member = crate::archive::sole_member(&data, "sig", 1 << 20).unwrap();
            let name_length = usize::from(u16::from_le_bytes([data[26], data[27]]));
            let name = String::from_utf8_lossy(&data[30..30 + name_length]);
            format!("zip[{name}]:{}", String::from_utf8_lossy(&member))
        } else {
            String::from_utf8_lossy(&data).into_owned()
        };
        self.steps
            .lock()
            .unwrap()
            .push(format!("write:{handle}:{data}"));
        Ok(ok_status(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.steps.lock().unwrap().push(format!("close:{handle}"));
        Ok(ok_status(id))
    }
}

fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: "en-US".into(),
    }
}

#[test]
fn program_upload_orders_signature_after_load_and_reports_failures() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        for (signature_present, fail_load, fail_signature) in [
            (true, false, false),
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let dir = TestDir::new();
            let program = dir.path().join("room.lpz");
            let signature = dir.path().join("room.sig");
            std::fs::write(&program, b"program").unwrap();
            if signature_present {
                std::fs::write(&signature, b"signature").unwrap();
            }

            let key = russh::keys::PrivateKey::from(
                russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[82; 32]),
            );
            let fingerprint = host_fingerprint(key.public_key()).unwrap();
            let config = Arc::new(russh::server::Config {
                keys: vec![key],
                auth_rejection_time: Duration::ZERO,
                ..Default::default()
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let steps: Steps = Arc::new(Mutex::new(Vec::new()));
            let server_steps = steps.clone();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let handler = ProgramServer {
                    channels: Default::default(),
                    steps: server_steps,
                    fail_load,
                    fail_signature,
                };
                if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                    let _ = session.await;
                }
            });
            let spec = ConnectionSpec {
                id: "test".into(),
                host: "127.0.0.1".into(),
                port,
                credentials: Credentials {
                    username: "admin".into(),
                    password: "test".into(),
                },
                trusted_fingerprint: Some(fingerprint),
            };
            let (events, receiver) = mpsc::channel();
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                upload_program(
                    &spec,
                    &program,
                    signature_present.then_some(signature.as_path()),
                    3,
                    &SharedSession::default(),
                    &events,
                ),
            )
            .await
            .unwrap();
            assert_eq!(result.is_ok(), !fail_load && !fail_signature, "{result:?}");
            let steps = steps.lock().unwrap().clone();
            let expected: &[&str] = if fail_load {
                &[
                    "open:/program03/room.lpz",
                    "write:/program03/room.lpz:program",
                    "close:/program03/room.lpz",
                    "exec:progload -p:3",
                ]
            } else if fail_signature {
                &[
                    "open:/program03/room.lpz",
                    "write:/program03/room.lpz:program",
                    "close:/program03/room.lpz",
                    "exec:progload -p:3",
                    "open:/program03/room.zig",
                ]
            } else if signature_present {
                &[
                    "open:/program03/room.lpz",
                    "write:/program03/room.lpz:program",
                    "close:/program03/room.lpz",
                    "exec:progload -p:3",
                    "open:/program03/room.zig",
                    "write:/program03/room.zig:zip[room.sig]:signature",
                    "close:/program03/room.zig",
                ]
            } else {
                &[
                    "open:/program03/room.lpz",
                    "write:/program03/room.lpz:program",
                    "close:/program03/room.lpz",
                    "exec:progload -p:3",
                ]
            };
            assert_eq!(steps, expected);
            let completed = receiver
                .try_iter()
                .filter(|event| matches!(event, WorkerEvent::Complete { .. }))
                .count();
            assert_eq!(completed, usize::from(!fail_load && !fail_signature));
            server.abort();
        }
    });
}
