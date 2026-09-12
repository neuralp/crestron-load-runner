use super::*;

#[test]
#[ignore = "Reads the configured RMC3 over SSH; requires explicit permission"]
fn read_rmc3_resources() {
    let (preferences, error) = crate::storage::Preferences::load();
    assert!(error.is_none(), "Could not load application preferences");
    let (path, _) = crate::storage::startup_address_book(&preferences);
    let path = path
        .or(preferences.default_address_book.clone())
        .or_else(|| preferences.recent_address_books.first().cloned())
        .expect("No address book configured");
    let entries = crate::storage::load_address_book(&path).expect("Could not load address book");
    let candidates: Vec<_> = entries
        .iter()
        .filter(|e| e.name.eq_ignore_ascii_case("RMC3") || e.model.eq_ignore_ascii_case("RMC3"))
        .collect();
    assert!(
        candidates.len() == 1,
        "Expected exactly one RMC3 address-book entry"
    );
    let entry = candidates[0];
    let spec = ConnectionSpec {
        id: "resource-live-test".into(),
        host: entry.host.clone(),
        port: entry.port,
        credentials: crate::model::Credentials {
            username: preferences.default_username,
            password: preferences.default_password,
        },
        trusted_fingerprint: entry.ssh_host_key_fingerprint.clone(),
    };
    let (events, _receiver) = mpsc::channel();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let session = connect(&spec, &events).await.unwrap_or_else(|_| {
            panic!("SSH connection failed; check connectivity and saved host-key trust")
        });
        for command in ["free", "ramfree"] {
            let report = run_command(&spec, &session, command, SSH_TIMEOUT, &events)
                .await
                .expect("Resource query failed");
            println!("{command}:\n{report}");
            let capacity = crate::resources::parse(&report, command == "ramfree")
                .expect("Live report must parse");
            println!(
                "{} free / {} total",
                crate::archive::human_size(capacity.free),
                crate::archive::human_size(capacity.total)
            );
        }
        disconnect(session).await;
    });
}
