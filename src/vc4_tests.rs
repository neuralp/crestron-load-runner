use super::*;

use serde_json::json;
use std::{io::Write as _, net::TcpListener, thread, time::Instant};

// Synthetic fixtures following the vendor's documented GET envelopes. These
// are deliberately not presented as captures from a real VC-4 server.
fn instances() -> Value {
    json!({"Device":{"Programs":{"ProgramInstanceLibrary":{
        "map-key-not-the-pid":{"ProgramInstanceId":"room/a","ProgramLibraryId":"shared","Name":"North room","Status":"Running"},
        "other":{"ProgramInstanceId":"second","ProgramLibraryId":"shared","Name":"South room","Status":"Stopped"}
    }}}})
}

struct Server {
    base: Url,
    fingerprint: String,
    requests: Receiver<String>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new(responses: Vec<(u16, String)>) -> Self {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
                .unwrap();
        let fingerprint = tls::fingerprint(cert.der().as_ref());
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key.into())
        .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = Url::parse(&format!("https://{}/", listener.local_addr().unwrap())).unwrap();
        let (sent, requests) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let handle = thread::spawn(move || {
            let mut responses = responses.into_iter();
            while !stopping.load(Ordering::Relaxed) {
                let (socket, _) = match listener.accept() {
                    Ok(socket) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let session = rustls::ServerConnection::new(config.clone()).unwrap();
                let mut stream = rustls::StreamOwned::new(session, socket);
                let mut request = Vec::new();
                let mut byte = [0u8];
                while request.len() < 16384 && !request.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    request.push(byte[0]);
                }
                // An untrusted-certificate test aborts the handshake before HTTP.
                if !request.ends_with(b"\r\n\r\n") {
                    continue;
                }
                let _ = sent.send(String::from_utf8(request).unwrap());
                let Some((status, body)) = responses.next() else {
                    break;
                };
                let reply = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        Self {
            base,
            fingerprint,
            requests,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap();
    }
}

#[test]
fn https_worker_uses_raw_authorization_token_and_only_documented_get_paths() {
    let library = json!({"Device":{"Programs":{"ProgramLibrary":{"shared":{"ProgramId":"shared","FriendlyName":"Common code"}}}}});
    let ip = json!({"Device":{"Programs":{"IpTableByPID":{"entry":{"ProgramInstanceId":"room/a","ProgramIpId":"12","Status":"Online"}}}}});
    let server = Server::new(vec![
        (200, library.to_string()),
        (200, instances().to_string()),
        (200, ip.to_string()),
    ]);
    let worker = Worker::start(
        server.base.clone(),
        authorization("synthetic-read-only-token").unwrap(),
        Some(server.fingerprint.clone()),
    )
    .unwrap();
    for (resource, path, expected) in [
        (Resource::Library, "ProgramLibrary", 1),
        (Resource::Instances, "ProgramInstance", 2),
        (
            Resource::IpTable("room/a".into()),
            "IpTableByPID/room%2Fa",
            1,
        ),
    ] {
        worker.requests.send(resource.clone()).unwrap();
        let (returned_resource, result) =
            worker.replies.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(returned_resource, resource);
        assert_eq!(result.unwrap().len(), expected);
        let request = server
            .requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(request.starts_with(&format!(
            "GET /VirtualControl/config/api/{path} HTTP/1.1\r\n"
        )));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: synthetic-read-only-token\r\n")
        );
        assert!(
            request
                .to_lowercase()
                .contains("accept: application/json\r\n")
        );
    }
}

#[test]
fn expanding_instance_sends_its_program_instance_id_over_https() {
    // All four identifiers differ: the collection key, database ID, library
    // ID and public ProgramInstanceId must never be used interchangeably.
    let instance = json!({"Device":{"Programs":{"ProgramInstanceLibrary":{
        "collection-key": {"id":917, "ProgramLibraryId":"library-id",
            "ProgramInstanceId":"Room A%2F", "Name":"Test room", "Status":"Running"}
    }}}});
    let server = Server::new(vec![
        (
            200,
            json!({"Device":{"Programs":{"ProgramLibrary":{}}}}).to_string(),
        ),
        (200, instance.to_string()),
        (
            200,
            json!({"Actions":[{"Results":[{"StatusInfo":"INVALID ID"}]}]}).to_string(),
        ),
    ]);
    let mut panel = Panel::new(Some(HttpsCertificateTrust {
        endpoint: server.base.to_string(),
        fingerprint: server.fingerprint.clone(),
    }))
    .with_token(&"synthetic-token".to_owned().into());
    panel.refresh("127.0.0.1");
    let until = Instant::now() + Duration::from_secs(5);
    while matches!(panel.library, Load::Loading) || matches!(panel.instances, Load::Loading) {
        panel.poll();
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(panel.instances.rows()[0].id, "Room A%2F");
    assert_eq!(server.requests.try_iter().count(), 2);
    let ctx = egui::Context::default();
    for _ in 0..3 {
        draw(&ctx, &mut panel).drop_without_applying_deltas();
    }
    click(&ctx, &mut panel, "+");
    let request = server
        .requests
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let path = "/VirtualControl/config/api/IpTableByPID/Room%20A%252F";
    assert_eq!(
        request.lines().next(),
        Some(format!("GET {path} HTTP/1.1").as_str())
    );
    let until = Instant::now() + Duration::from_secs(5);
    while matches!(panel.tables.get("Room A%2F"), Some(Load::Loading)) {
        panel.poll();
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        matches!(panel.tables.get("Room A%2F"), Some(Load::Error(e)) if e.contains("INVALID ID"))
    );
    let output = draw(&ctx, &mut panel);
    let path_visible = output.shapes.iter().any(
        |s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.text() == format!("GET {path}")),
    );
    output.drop_without_applying_deltas();
    assert!(path_visible);
}

#[test]
fn rejects_untrusted_tls_and_http_errors_without_echoing_secrets() {
    let server = Server::new(vec![
        (401, "synthetic-secret-echo".into()),
        (503, "synthetic-secret-echo".into()),
        (200, "<html>login</html>".into()),
        (200, "{\"Actions\":[]}".into()),
        (302, "synthetic-secret-echo".into()),
    ]);
    let token = authorization("synthetic-secret-echo").unwrap();
    let observed = tls::UntrustedCertificate::default();
    let error = fetch(
        &client(None, observed.clone()).unwrap(),
        &server.base,
        &token,
        &Resource::Library,
    )
    .unwrap_err();
    assert!(error.contains("certificate approval"));
    assert_eq!(*observed.lock().unwrap(), Some(server.fingerprint.clone()));
    assert!(server.requests.try_recv().is_err());
    let trusted = client(Some(server.fingerprint.clone()), Default::default()).unwrap();
    for message in [
        "token",
        "503",
        "invalid JSON",
        "Expected Device.Programs",
        "redirected",
    ] {
        let error = fetch(&trusted, &server.base, &token, &Resource::Library).unwrap_err();
        assert!(error.contains(message), "{error}");
        assert!(!error.contains("synthetic-secret-echo"));
    }
    assert!(
        fetch(
            &trusted,
            &Url::parse("http://127.0.0.1:1").unwrap(),
            &token,
            &Resource::Library
        )
        .is_err()
    );
}

#[test]
fn certificate_approval_is_explicit_and_reject_sends_no_token() {
    let server = Server::new(vec![
        (
            200,
            json!({"Device":{"Programs":{"ProgramLibrary":{}}}}).to_string(),
        ),
        (200, instances().to_string()),
    ]);
    let mut panel = Panel {
        token: "synthetic-token".into(),
        port: server.base.port().unwrap(),
        ..Default::default()
    };
    let ctx = egui::Context::default();
    for reject in [true, false] {
        panel.refresh("127.0.0.1");
        let until = Instant::now() + Duration::from_secs(5);
        while panel.pending_certificate.is_none() {
            panel.poll();
            assert!(Instant::now() < until, "certificate prompt did not appear");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            panel.pending_certificate.as_deref(),
            Some(server.fingerprint.as_str())
        );
        assert!(panel.worker.is_none());
        assert!(server.requests.try_recv().is_err());
        for _ in 0..3 {
            draw(&ctx, &mut panel).drop_without_applying_deltas();
        }
        let output = draw(&ctx, &mut panel);
        assert!(output.shapes.iter().any(
            |s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.text() == server.fingerprint)
        ));
        output.drop_without_applying_deltas();
        if reject {
            click(&ctx, &mut panel, "Reject certificate");
            assert!(panel.take_trust_action().is_none());
            assert!(panel.trusted.is_none());
            assert!(panel.pending_certificate.is_none());
        } else {
            click(&ctx, &mut panel, "Accept certificate and save");
            let Some(TrustAction::Accept(trust)) = panel.take_trust_action() else {
                panic!("accept action missing");
            };
            assert_eq!(trust.endpoint, server.base.as_str());
            assert_eq!(trust.fingerprint, server.fingerprint);
            // The application persists this action before reconnecting.
            assert!(server.requests.try_recv().is_err());
            panel.set_trust(Some(trust));
            panel.refresh("127.0.0.1");
            let until = Instant::now() + Duration::from_secs(5);
            while matches!(panel.library, Load::Loading) || matches!(panel.instances, Load::Loading)
            {
                panel.poll();
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(5));
            }
            assert!(matches!(panel.library, Load::Ready(_)));
            assert_eq!(server.requests.try_iter().count(), 2);
        }
    }
}

#[test]
fn changed_certificate_requires_reapproval_before_any_http_request() {
    let server = Server::new(vec![]);
    let old = tls::fingerprint(b"old synthetic certificate");
    let trust = HttpsCertificateTrust {
        endpoint: server.base.to_string(),
        fingerprint: old.clone(),
    };
    let mut panel = Panel::new(Some(trust));
    panel.token = "synthetic-token".into();
    panel.refresh("127.0.0.1");
    let until = Instant::now() + Duration::from_secs(5);
    while panel.pending_certificate.is_none() {
        panel.poll();
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(server.requests.try_recv().is_err());
    assert_eq!(panel.trusted.as_ref().unwrap().fingerprint, old);
    let ctx = egui::Context::default();
    for _ in 0..3 {
        draw(&ctx, &mut panel).drop_without_applying_deltas();
    }
    let output = draw(&ctx, &mut panel);
    for label in [
        "HTTPS certificate changed",
        old.as_str(),
        server.fingerprint.as_str(),
    ] {
        assert!(
            output
                .shapes
                .iter()
                .any(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.text() == label)),
            "missing {label}"
        );
    }
    output.drop_without_applying_deltas();
    click(&ctx, &mut panel, "Accept replacement and save");
    assert!(
        matches!(panel.take_trust_action(), Some(TrustAction::Accept(t)) if t.fingerprint == server.fingerprint)
    );
    assert!(server.requests.try_recv().is_err());
}

#[test]
fn saved_certificate_is_bound_to_https_host_and_port() {
    let trust = HttpsCertificateTrust {
        endpoint: "https://vc4.example.test:8443/".into(),
        fingerprint: tls::fingerprint(b"synthetic certificate"),
    };
    let panel = Panel::new(Some(trust.clone()));
    assert_eq!(panel.port, 8443);
    assert_eq!(
        panel.trusted_for(&server_url("vc4.example.test", 8443).unwrap()),
        Some(trust.fingerprint)
    );
    assert!(
        panel
            .trusted_for(&server_url("vc4.example.test", 443).unwrap())
            .is_none()
    );
    assert!(
        panel
            .trusted_for(&server_url("other.example.test", 8443).unwrap())
            .is_none()
    );
}

#[test]
fn token_buttons_request_persistence_and_never_render_the_secret() {
    let ctx = egui::Context::default();
    let mut panel = Panel::default();
    for _ in 0..3 {
        draw(&ctx, &mut panel).drop_without_applying_deltas();
    }
    panel.token = "synthetic-ui-secret".into();
    let output = draw(&ctx, &mut panel);
    assert!(!output.shapes.iter().any(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.text().contains("synthetic-ui-secret"))));
    output.drop_without_applying_deltas();
    click(&ctx, &mut panel, "Save token");
    assert!(panel.take_token_save());
    assert!(!panel.take_token_save());
    click(&ctx, &mut panel, "Forget token");
    assert!(panel.take_token_save());
    assert!(panel.token().is_empty());
    assert!(panel.worker.is_none());
}

fn input() -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(2000.0, 1600.0),
        )),
        ..Default::default()
    }
}

fn draw(ctx: &egui::Context, panel: &mut Panel) -> egui::FullOutput {
    ctx.run_ui(input(), |ui| panel.show(ui, "vc4-test"))
}

fn click(ctx: &egui::Context, panel: &mut Panel, label: &str) {
    let output = draw(ctx, panel);
    let pos = output
        .shapes
        .iter()
        .find_map(|shape| {
            if let egui::Shape::Text(text) = &shape.shape
                && text.galley.text() == label
            {
                Some(text.pos + text.galley.size() * 0.5)
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("missing button {label}"));
    output.drop_without_applying_deltas();
    for pressed in [true, false] {
        let mut raw = input();
        raw.events = vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        ctx.run_ui(raw, |ui| panel.show(ui, "vc4-test"))
            .drop_without_applying_deltas();
    }
}

#[test]
fn stopped_instances_never_request_ip_tables() {
    for status in ["Stopped", "stopped", " STOPPED "] {
        let (requests, incoming) = mpsc::channel();
        let (_outgoing, replies) = mpsc::channel();
        let mut panel = Panel {
            instances: Load::Ready(
                parse_records(
                    &Resource::Instances,
                    &json!({"Device":{"Programs":{"ProgramInstanceLibrary":{
                        "entry":{"ProgramInstanceId":"stopped-room","Status":status}
                    }}}}),
                )
                .unwrap(),
            ),
            worker: Some(Worker {
                requests,
                replies,
                cancelled: Arc::new(AtomicBool::new(false)),
                untrusted: Default::default(),
            }),
            ..Default::default()
        };
        // The request boundary must also reject requests, not only the UI.
        panel.request(Resource::IpTable("stopped-room".into()));
        assert!(incoming.try_recv().is_err());
        let ctx = egui::Context::default();
        for _ in 0..3 {
            draw(&ctx, &mut panel).drop_without_applying_deltas();
        }
        click(&ctx, &mut panel, "+");
        let output = draw(&ctx, &mut panel);
        let stopped_message = output.shapes.iter().any(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.text() == "Program is stopped; IP table is not fetched."));
        output.drop_without_applying_deltas();
        assert!(stopped_message);
        click(&ctx, &mut panel, "Refresh IP table");
        assert!(incoming.try_recv().is_err());
        assert!(!panel.tables.contains_key("stopped-room"));
        click(&ctx, &mut panel, "-");
        assert!(!panel.expanded.contains("stopped-room"));
    }
}

#[test]
fn expansion_fetches_only_its_pid_once_and_refresh_retries_after_errors() {
    let (requests, incoming) = mpsc::channel();
    let (outgoing, replies) = mpsc::channel();
    let mut panel = Panel {
        token: "synthetic-ui-token".into(),
        library: Load::Ready(Vec::new()),
        instances: Load::Ready(parse_records(&Resource::Instances, &instances()).unwrap()),
        worker: Some(Worker {
            requests,
            replies,
            cancelled: Arc::new(AtomicBool::new(false)),
            untrusted: Default::default(),
        }),
        ..Default::default()
    };
    let ctx = egui::Context::default();
    for _ in 0..3 {
        draw(&ctx, &mut panel).drop_without_applying_deltas();
    }
    assert!(
        incoming.try_recv().is_err(),
        "collapsed instances must not fetch IP tables"
    );
    click(&ctx, &mut panel, "+");
    assert_eq!(
        incoming.recv_timeout(Duration::from_secs(1)).unwrap(),
        Resource::IpTable("room/a".into())
    );
    for _ in 0..3 {
        draw(&ctx, &mut panel).drop_without_applying_deltas();
    }
    assert!(
        incoming.try_recv().is_err(),
        "loading must not enqueue duplicates"
    );
    outgoing
        .send((
            Resource::IpTable("room/a".into()),
            Err("Test API error".into()),
        ))
        .unwrap();
    draw(&ctx, &mut panel).drop_without_applying_deltas();
    assert!(matches!(panel.tables["room/a"], Load::Error(_)));
    click(&ctx, &mut panel, "Refresh IP table");
    assert_eq!(
        incoming.try_recv().unwrap(),
        Resource::IpTable("room/a".into())
    );
    let rows = parse_records(&Resource::IpTable("room/a".into()), &json!({"Device":{"Programs":{"IpTableByPID":{"entry":{"ProgramInstanceId":"room/a","remote_ip":"192.0.2.80","Status":"Online"}}}}})).unwrap();
    outgoing
        .send((Resource::IpTable("room/a".into()), Ok(rows)))
        .unwrap();
    let output = draw(&ctx, &mut panel);
    assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "192.0.2.80")));
    output.drop_without_applying_deltas();
    click(&ctx, &mut panel, "-");
    click(&ctx, &mut panel, "+");
    assert!(
        incoming.try_recv().is_err(),
        "reopening uses cached data until refresh"
    );
    assert!(!panel.tables.contains_key("second"));
}

#[test]
fn refreshing_discards_old_replies_and_clears_all_instance_caches() {
    let (requests, _incoming) = mpsc::channel();
    let (outgoing, replies) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let mut panel = Panel {
        worker: Some(Worker {
            requests,
            replies,
            cancelled: stop.clone(),
            untrusted: Default::default(),
        }),
        ..Default::default()
    };
    panel.tables.insert("old".into(), Load::Ready(Vec::new()));
    panel.expanded.insert("old".into());
    outgoing
        .send((
            Resource::Instances,
            Ok(parse_records(&Resource::Instances, &instances()).unwrap()),
        ))
        .unwrap();
    panel.refresh("vc4.example.test"); // No token: local validation, no network.
    assert!(stop.load(Ordering::Relaxed));
    panel.poll();
    assert!(matches!(panel.instances, Load::Error(_)));
    assert!(panel.tables.is_empty());
    assert!(panel.expanded.is_empty());
    assert!(
        outgoing
            .send((Resource::Instances, Ok(Vec::new())))
            .is_err()
    );
}

#[test]
fn large_collections_are_paged_and_search_does_not_lose_later_entries() {
    let rows: Vec<_> = (0..1001)
        .map(|i| Record {
            id: i.to_string(),
            fields: Map::from_iter([("Name".into(), json!(format!("Room {i}")))]),
        })
        .collect();
    let mut view = TableView::default();
    let ctx = egui::Context::default();
    let mut shown = Vec::new();
    ctx.run_ui(input(), |ui| {
        shown = view.controls(ui, &rows);
    })
    .drop_without_applying_deltas();
    assert_eq!(shown, (0..50).collect::<Vec<_>>());
    view.page = 20;
    ctx.run_ui(input(), |ui| {
        shown = view.controls(ui, &rows);
    })
    .drop_without_applying_deltas();
    assert_eq!(shown, [1000]);
    view.query = "ROOM 1000".into();
    ctx.run_ui(input(), |ui| {
        shown = view.controls(ui, &rows);
    })
    .drop_without_applying_deltas();
    assert_eq!(shown, [1000]);
    assert_eq!(view.page, 0);
}

#[test]
fn panel_refresh_receives_both_tables_without_fetching_ip_tables() {
    let server = Server::new(vec![
        (
            200,
            json!({"Device":{"Programs":{"ProgramLibrary":{}}}}).to_string(),
        ),
        (200, instances().to_string()),
    ]);
    let mut panel = Panel {
        token: "synthetic-token".into(),
        port: server.base.port().unwrap(),
        trusted: Some(HttpsCertificateTrust {
            endpoint: server.base.to_string(),
            fingerprint: server.fingerprint.clone(),
        }),
        ..Default::default()
    };
    panel.refresh("127.0.0.1");
    let until = Instant::now() + Duration::from_secs(5);
    while matches!(panel.library, Load::Loading) || matches!(panel.instances, Load::Loading) {
        panel.poll();
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(panel.library, Load::Ready(_)));
    assert_eq!(panel.instances.rows().len(), 2);
    assert!(panel.tables.is_empty());
    assert_eq!(server.requests.try_iter().count(), 2);
}
