use super::*;

type Step = (String, String, u32);

struct TranscriptServer {
    steps: Arc<Mutex<VecDeque<Step>>>,
    received: Arc<Mutex<Vec<String>>>,
    authenticate: bool,
}

impl russh::server::Handler for TranscriptServer {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        _user: &str,
        _password: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        Ok(if self.authenticate {
            russh::server::Auth::Accept
        } else {
            russh::server::Auth::reject()
        })
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
        let (expected, response, status) = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected console command");
        assert_eq!(command, expected);
        session.channel_success(channel)?;
        if !response.is_empty() {
            session.data(channel, response)?;
        }
        session.exit_status_request(channel, status)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

fn step(command: &str, output: &str, status: u32) -> Step {
    (command.into(), output.into(), status)
}
fn table(rows: &str) -> String {
    format!("CIP_ID|IP Address/SiteName\n{rows}")
}

fn run_case(
    steps: Vec<Step>,
    assignment: bool,
    master: &str,
    trusted: bool,
    authenticate: bool,
) -> (Result<String, String>, Vec<String>, Vec<WorkerEvent>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let key = russh::keys::PrivateKey::from(
                russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[53; 32]),
            );
            let fingerprint = host_fingerprint(key.public_key()).unwrap();
            let config = Arc::new(russh::server::Config {
                keys: vec![key],
                auth_rejection_time: Duration::ZERO,
                ..Default::default()
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let steps: Arc<Mutex<VecDeque<Step>>> = Arc::new(Mutex::new(steps.into()));
            let received = Arc::new(Mutex::new(Vec::new()));
            let handler = TranscriptServer {
                steps: steps.clone(),
                received: received.clone(),
                authenticate,
            };
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                if let Ok(session) = russh::server::run_stream(config, stream, handler).await {
                    let _ = session.await;
                }
            });
            let connection = ConnectionSpec {
                id: "loopback-ipid".into(),
                host: "127.0.0.1".into(),
                port,
                credentials: Credentials {
                    username: "test".into(),
                    password: "test".into(),
                },
                trusted_fingerprint: trusted.then_some(fingerprint),
            };
            let (reply, receiver) = mpsc::channel();
            let command = if assignment {
                WorkerCommand::AssignIpid {
                    connection,
                    ipid: "11".into(),
                    master: master.into(),
                    reply,
                }
            } else {
                WorkerCommand::ReadIpids {
                    connection,
                    program: 10,
                    reply,
                }
            };
            let (events, event_receiver) = mpsc::channel();
            let device = SharedSession::default();
            tokio::time::timeout(Duration::from_secs(5), dispatch(command, &device, &events))
                .await
                .expect("IPID dispatch timed out")
                .unwrap();
            let result = receiver
                .try_recv()
                .expect("every outcome must answer the feature channel");
            let commands = received.lock().unwrap().clone();
            assert!(
                steps.lock().unwrap().is_empty(),
                "missing expected commands"
            );
            let events = event_receiver.try_iter().collect();
            device.close().await;
            server.abort();
            (result, commands, events)
        })
}

#[test]
fn query_sends_exact_program_and_logs_response() {
    let raw = "TableStart:[ Program 10 ]\nCIP_ID|Model Name\n11|TS-770";
    let (result, commands, events) = run_case(
        vec![step("ipt -t -P:10", raw, 0)],
        false,
        "unused",
        true,
        true,
    );
    assert_eq!(result.unwrap(), raw);
    assert_eq!(commands, ["ipt -t -P:10"]);
    assert!(events.iter().any(|event| matches!(event, WorkerEvent::Log {
        direction: Direction::Sent, text, .. } if text == "ipt -t -P:10")));
    assert!(events.iter().any(|event| matches!(event, WorkerEvent::Log {
        direction: Direction::Received, text, .. } if text == raw)));
}

#[test]
fn replacement_reports_success_only_after_final_readback() {
    let (result, commands, events) = run_case(
        vec![
            step("ipt -t", &table("11|old-master\n22|untouched"), 0),
            step("remmaster 11 old-master", "", 0),
            step("ipt -t", &table("22|untouched"), 0),
            step("addmaster 11 192.0.2.10", "", 0),
            step("ipt -t", &table("11|192.0.2.10\n22|untouched"), 0),
        ],
        true,
        "192.0.2.10",
        true,
        true,
    );
    assert_eq!(result.unwrap(), "Verified");
    assert_eq!(
        commands,
        [
            "ipt -t",
            "remmaster 11 old-master",
            "ipt -t",
            "addmaster 11 192.0.2.10",
            "ipt -t"
        ]
    );
    let complete = events
        .iter()
        .position(|e| matches!(e, WorkerEvent::Complete { .. }))
        .unwrap();
    let last_read = events
        .iter()
        .rposition(|e| {
            matches!(
                e,
                WorkerEvent::Log {
                    direction: Direction::Received,
                    ..
                }
            )
        })
        .unwrap();
    assert!(complete > last_read);
}

#[test]
fn hostname_is_the_master_operand_not_the_ssh_endpoint() {
    let (result, commands, _) = run_case(
        vec![
            step("ipt -t", &table(""), 0),
            step("addmaster 11 rack-cp4.example.test", "", 0),
            step("ipt -t", &table("11|rack-cp4.example.test"), 0),
        ],
        true,
        "rack-cp4.example.test",
        true,
        true,
    );
    assert_eq!(result.unwrap(), "Verified");
    assert_eq!(commands[1], "addmaster 11 rack-cp4.example.test");
}

#[test]
fn failures_stop_without_replaying_commands_or_claiming_success() {
    let cases = vec![
        vec![step("ipt -t", "read failed", 1)],
        vec![step("ipt -t", "unknown output", 0)],
        vec![
            step("ipt -t", &table("11|old-master"), 0),
            step("remmaster 11 old-master", "failed", 1),
        ],
        vec![
            step("ipt -t", &table("11|old-master"), 0),
            step("remmaster 11 old-master", "Error: denied", 0),
        ],
        vec![
            step("ipt -t", &table("11|old-master"), 0),
            step("remmaster 11 old-master", "", 0),
            step("ipt -t", &table("11|old-master"), 0),
        ],
        vec![
            step("ipt -t", &table("11|old-master"), 0),
            step("remmaster 11 old-master", "", 0),
            step("ipt -t", "malformed readback", 0),
        ],
        vec![
            step("ipt -t", &table(""), 0),
            step("addmaster 11 192.0.2.10", "denied", 1),
        ],
        vec![
            step("ipt -t", &table(""), 0),
            step("addmaster 11 192.0.2.10", "", 0),
            step("ipt -t", &table(""), 0),
        ],
        vec![
            step("ipt -t", &table("22|untouched"), 0),
            step("addmaster 11 192.0.2.10", "", 0),
            step("ipt -t", &table("11|192.0.2.10\n22|changed"), 0),
        ],
    ];
    for steps in cases {
        let expected: Vec<_> = steps.iter().map(|step| step.0.clone()).collect();
        let (result, commands, events) = run_case(steps, true, "192.0.2.10", true, true);
        assert!(result.is_err());
        assert_eq!(commands, expected);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WorkerEvent::Complete { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkerEvent::Error { .. }))
        );
    }
}

#[test]
fn trust_and_credentials_failures_reply_without_issuing_commands() {
    let (result, commands, events) = run_case(Vec::new(), true, "192.0.2.10", false, true);
    assert!(result.is_err());
    assert!(commands.is_empty());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, WorkerEvent::HostKeyUnknown { .. }))
    );
    let (result, commands, events) = run_case(Vec::new(), true, "192.0.2.10", true, false);
    assert!(result.is_err());
    assert!(commands.is_empty());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, WorkerEvent::CredentialsRejected { .. }))
    );
}
