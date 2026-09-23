//! Loopback SSH tests: no real device or firmware installation is used.
use super::*;
use crate::test_support::{TestDir, stamp, zip};

struct Server {
    report: &'static str,
    exit_status: u32,
    seen: Arc<Mutex<Vec<String>>>,
}

impl russh::server::Handler for Server {
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
    async fn exec_request(
        &mut self,
        channel: russh::ChannelId,
        data: &[u8],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        self.seen
            .lock()
            .unwrap()
            .push(String::from_utf8(data.to_vec()).unwrap());
        session.channel_success(channel)?;
        session.data(channel, self.report)?;
        session.exit_status_request(channel, self.exit_status)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        channel: russh::ChannelId,
        name: &str,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        self.seen.lock().unwrap().push(name.to_owned());
        // A newer version may reach SFTP. Deliberately refuse it here so this
        // test can never transfer firmware or issue an apply command.
        session.channel_failure(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

#[test]
fn firmware_preflight_blocks_before_sftp_unless_strictly_newer() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (version, report, exit_status, expected) in [
        (Some("1.8001.0297"), "PUF: 1.8001.0298\r\n", 0, "older than"),
        (
            Some("1.8001.0298"),
            "PUF: 1.8001.0298\r\n",
            0,
            "same version",
        ),
        (Some("1.8001.0299"), "Cab: 1.8001.0298\r\n", 0, "no PUF:"),
        (Some("1.8001.0299"), "PUF: invalid\r\n", 0, "invalid device"),
        (
            Some("1.8001.0299"),
            "PUF: 1.8001.0298\r\n",
            1,
            "could not query",
        ),
        (
            Some("1.8001.0299"),
            "PUF: 1.8001.0298\r\n",
            0,
            "Could not start SFTP",
        ),
        (None, "PUF: 1.8001.0298\r\n", 0, "no [Package] Version"),
        (Some("bad"), "PUF: 1.8001.0298\r\n", 0, "invalid package"),
    ] {
        runtime.block_on(async {
            let dir = TestDir::new();
            let ini = version.map_or_else(|| "[Package]\nName=RMC3\n".into(), |v| format!("[Package]\nVersion={v}\n"));
            let path = dir.path().join("firmware.puf");
            std::fs::write(&path, zip(&[("~.package.ini", ini.as_bytes(), true, stamp(2025, 12, 15, 12, 9))])).unwrap();
            let key = russh::keys::PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[42; 32]));
            let fingerprint = host_fingerprint(key.public_key()).unwrap();
            let config = Arc::new(russh::server::Config { keys: vec![key], auth_rejection_time: Duration::ZERO, ..Default::default() });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let handler = Server { report, exit_status, seen: seen.clone() };
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                if let Ok(session) = russh::server::run_stream(config, stream, handler).await { let _ = session.await; }
            });
            let spec = ConnectionSpec { id: "test".into(), host: "127.0.0.1".into(), port,
                credentials: crate::model::Credentials { username: "test".into(), password: "synthetic".into() }, trusted_fingerprint: Some(fingerprint) };
            let (events, receiver) = mpsc::channel();
            let result = tokio::time::timeout(Duration::from_secs(5), upload_firmware(&spec, &path, "firmware.puf", &SharedSession::default(), &events)).await.unwrap();
            let not_needed = matches!(expected, "older than" | "same version");
            if not_needed {
                assert!(result.is_ok(), "older/equal firmware should complete without uploading");
            } else {
                assert!(matches!(result, Err(ConnectError::Message { message, .. }) if message.contains(expected)), "expected {expected}");
            }
            let reached_sftp = expected == "Could not start SFTP";
            let local_failure = version.is_none() || version == Some("bad");
            let expected_commands: Vec<&str> = if local_failure { vec![] } else if reached_sftp { vec!["ver -v", "sftp"] } else { vec!["ver -v"] };
            assert_eq!(*seen.lock().unwrap(), expected_commands);
            let events: Vec<_> = receiver.try_iter().collect();
            assert!(!events.iter().any(|e| matches!(e, WorkerEvent::Progress { .. })));
            assert_eq!(events.iter().filter(|e| matches!(e, WorkerEvent::FirmwareUpToDate { id, message } if id == "test" && message.starts_with("Firmware upgrade not needed"))).count(), usize::from(not_needed));
            server.abort();
        });
    }
}
