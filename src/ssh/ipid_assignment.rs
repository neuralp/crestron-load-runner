use super::*;

/// Reply independently of general card events: another queued job must never
/// complete an assignment row. A closed popout does not cancel queued writes.
fn finish(
    spec: &ConnectionSpec,
    result: WorkerResult<String>,
    reply: Sender<Result<String, String>>,
    events: &Sender<WorkerEvent>,
    assignment: bool,
) {
    let result = match result {
        Ok(value) => {
            let _ = events.send(WorkerEvent::Complete {
                id: spec.id.clone(),
                message: if assignment {
                    format!("IPID assignment: {value}")
                } else {
                    "Program IP table received".into()
                },
            });
            Ok(value)
        }
        Err(error) => Err(report(events, error, Announce::OnTheCard)),
    };
    let _ = reply.send(result);
}

pub(super) async fn read(
    spec: &ConnectionSpec,
    program: u8,
    reply: Sender<Result<String, String>>,
    device: &SharedSession,
    events: &Sender<WorkerEvent>,
) {
    let result = async {
        if !(1..=10).contains(&program) {
            return Err(message(spec, "Program must be 1 through 10"));
        }
        let (session, _) = device.acquire(spec, Announce::OnTheCard, events).await?;
        // Preserve even unfamiliar output so the window can display it.
        run_command(
            spec,
            &session,
            &format!("ipt -t -P:{program}"),
            SSH_TIMEOUT,
            events,
        )
        .await
        .map_err(|error| message(spec, error))
    }
    .await;
    finish(spec, result, reply, events, false);
}

pub(super) async fn assign(
    spec: &ConnectionSpec,
    ipid: &str,
    master: &str,
    reply: Sender<Result<String, String>>,
    device: &SharedSession,
    events: &Sender<WorkerEvent>,
) {
    let result = async {
        crate::ipid_assignment::ipid_key(ipid).map_err(|e| message(spec, e))?;
        crate::ipid_assignment::address_key(master).map_err(|e| message(spec, e))?;
        let (session, _) = device.acquire(spec, Announce::OnTheCard, events).await?;
        let session = &session;
        crate::ipid_assignment::replace(ipid, master, |command| async move {
            run_command(spec, session, &command, SSH_TIMEOUT, events).await
        }).await.map_err(|e| message(spec, format!(
            "IPID {ipid}: {e}. The previous mapping may be absent; inspect the table before retrying."
        )))
    }.await;
    finish(spec, result, reply, events, true);
}
