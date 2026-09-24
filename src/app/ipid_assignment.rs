use super::*;
use crate::ipid_assignment::{self as ipid, Action, Panel};

impl LoadRunnerApp {
    pub(super) fn ipid_processor(&self, target: Option<&str>) -> Result<&Device, String> {
        let mut targets = self
            .devices
            .iter()
            .filter(|device| target.map_or(device.selected, |id| device.id == id));
        let device = targets
            .next()
            .ok_or("Select exactly one processor target")?;
        if targets.next().is_some() {
            return Err("Select exactly one processor target".into());
        }
        if !ipid::is_processor(device) {
            return Err("Requires a console processor; VC-4 uses REST".into());
        }
        Ok(device)
    }

    fn ipid_candidates(&self, processor_id: &str) -> Vec<Device> {
        self.devices
            .iter()
            .filter(|device| {
                device.id != processor_id
                    && (device.discovered.is_some() || device.source == DeviceSource::AddressBook)
                    && !crate::vc4::is_vc4(&device.model)
            })
            .cloned()
            .collect()
    }

    pub(super) fn ipid_busy(&self) -> bool {
        self.ipid_assignment.as_ref().is_some_and(Panel::busy)
    }

    pub(super) fn sync_ipid_discovery(&mut self) {
        let Some(panel) = &self.ipid_assignment else {
            return;
        };
        let candidates = self.ipid_candidates(&panel.processor.id);
        let processor = self
            .devices
            .iter()
            .find(|d| d.id == panel.processor.id)
            .cloned();
        let panel = self.ipid_assignment.as_mut().unwrap();
        panel.discovering = self.discovering;
        let selected: std::collections::HashSet<_> = panel
            .rows
            .iter()
            .filter_map(|row| row.selected.as_ref())
            .collect();
        // Keep reviewed target snapshots: rediscovery must not silently move a
        // pending assignment to a new endpoint. GO still validates those snapshots.
        for candidate in candidates {
            if let Some(previous) = panel.candidates.iter_mut().find(|d| d.id == candidate.id) {
                if !selected.contains(&candidate.id) {
                    *previous = candidate;
                }
            } else {
                panel.candidates.push(candidate);
            }
        }
        // Enrich a manual processor with its discovered hostname/model before
        // assignments are reviewed, without changing the SSH endpoint queried.
        if selected.is_empty()
            && let Some(processor) = processor
            && ipid::is_processor(&processor)
            && processor.host == panel.processor.host
            && processor.port == panel.processor.port
        {
            panel.processor = processor;
        }
    }

    pub(super) fn open_ipid_assignment(&mut self, target: Option<&str>) {
        if let Some(panel) = &mut self.ipid_assignment
            && panel.busy()
        {
            panel.open = true;
            self.sync_ipid_discovery();
            return;
        }
        if self.credential_prompt.is_some() || self.pending_action.is_some() {
            return;
        }
        let processor = match self.ipid_processor(target) {
            Ok(device) => device.clone(),
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        // Reopening the same closed window retains its last results.
        self.sync_ipid_discovery();
        if let Some(panel) = &mut self.ipid_assignment
            && !panel.open
            && panel.processor.id == processor.id
            && panel.validate_live(&self.devices, &[]).is_ok()
        {
            panel.open = true;
            return;
        }
        let candidates = self.ipid_candidates(&processor.id);
        self.next_ipid_token = self
            .next_ipid_token
            .checked_add(1)
            .expect("IPID window token exhausted");
        let token = self.next_ipid_token;
        self.ipid_assignment = Some(Panel::new(token, processor, candidates));
        self.sync_ipid_discovery();
        self.queue_ipid_table(token);
    }

    fn ipid_error(&mut self, token: u64, error: String) {
        if let Some(panel) = &mut self.ipid_assignment
            && panel.token == token
        {
            panel.awaiting_credentials = false;
            panel.message = error;
        }
    }

    pub(super) fn queue_ipid_table(&mut self, token: u64) {
        if self.credential_prompt.is_some() || self.pending_action.is_some() {
            return;
        }
        let Some(panel) = &mut self.ipid_assignment else {
            return;
        };
        if panel.token != token {
            return;
        }
        panel.awaiting_credentials = false;
        if !panel.open || panel.busy() {
            return;
        }
        if let Err(error) = panel.validate_live(&self.devices, &[]) {
            panel.message = error;
            panel.valid = false;
            return;
        }
        panel.valid = false;
        panel.rows.clear();
        panel.raw.clear();
        let (id, program) = (panel.processor.id.clone(), panel.program);
        if self
            .needs_session_credentials(std::slice::from_ref(&id), CredentialGate::ReadIpids(token))
        {
            if let Some(panel) = &mut self.ipid_assignment {
                panel.awaiting_credentials = true;
                panel.message = "Waiting for credentials in the main window".into();
            }
            return;
        }
        let Some(connection) = self.connection_spec(&id) else {
            self.ipid_error(token, "Processor removed; reopen Assign IPIDs".into());
            return;
        };
        let candidates = self.ipid_candidates(&id);
        if let Some(panel) = &mut self.ipid_assignment {
            panel.candidates = candidates;
        }
        let (reply, receiver) = mpsc::channel();
        match self.worker_pool.send(
            &id,
            WorkerCommand::ReadIpids {
                connection,
                program,
                reply,
            },
        ) {
            Err(error) => self.ipid_error(token, error),
            Ok(()) => {
                if let Some(panel) = &mut self.ipid_assignment {
                    panel.table_reply = Some(receiver);
                    panel.message = format!("Reading program {program} IP table");
                }
            }
        }
    }

    pub(super) fn queue_ipid_assignments(&mut self, token: u64) {
        if self.credential_prompt.is_some() || self.pending_action.is_some() {
            return;
        }
        let Some(panel) = &mut self.ipid_assignment else {
            return;
        };
        if panel.token != token {
            return;
        }
        panel.awaiting_credentials = false;
        if !panel.open || panel.busy() {
            return;
        }
        let jobs = match panel.jobs().and_then(|jobs| {
            panel.validate_live(&self.devices, &jobs)?;
            Ok(jobs)
        }) {
            Ok(jobs) => jobs,
            Err(error) => {
                panel.message = error;
                return;
            }
        };
        let ids: Vec<_> = jobs.iter().map(|job| job.id.clone()).collect();
        if self.needs_session_credentials(&ids, CredentialGate::AssignIpids(token)) {
            if let Some(panel) = &mut self.ipid_assignment {
                panel.awaiting_credentials = true;
                panel.message = "Waiting for credentials in the main window".into();
            }
            return;
        }
        // Build the entire batch before sending anything. Trust and credentials
        // are resolved now; endpoint/model changes require a new review.
        let connections = ids
            .iter()
            .map(|id| self.connection_spec(id))
            .collect::<Option<Vec<_>>>();
        let Some(connections) = connections else {
            self.ipid_error(
                token,
                "A target was removed; no assignments were queued".into(),
            );
            return;
        };
        for (job, connection) in jobs.into_iter().zip(connections) {
            let (reply, receiver) = mpsc::channel();
            let result = self.worker_pool.send(
                &job.id,
                WorkerCommand::AssignIpid {
                    connection,
                    ipid: job.ipid,
                    master: job.master,
                    reply,
                },
            );
            if let Some(panel) = &mut self.ipid_assignment {
                let row = &mut panel.rows[job.row];
                match result {
                    Ok(()) => {
                        row.reply = Some(receiver);
                        row.status = "Queued / working".into();
                    }
                    Err(error) => row.status = format!("Failed to queue: {error}"),
                }
                panel.message =
                    "Results are shown per row; closing does not cancel queued writes".into();
            }
        }
    }

    pub(super) fn show_ipid_assignment(&mut self, ctx: &egui::Context) {
        let blocked = self.pending_action.is_some() || self.credential_prompt.is_some();
        let action = self
            .ipid_assignment
            .as_mut()
            .and_then(|panel| panel.show(ctx, blocked).map(|action| (panel.token, action)));
        match action {
            Some((token, Action::Reload)) => self.queue_ipid_table(token),
            Some((token, Action::Go)) => self.queue_ipid_assignments(token),
            Some((_, Action::Discover)) => self.start_discovery(),
            None => {}
        }
    }
}
