use super::*;
use crate::debugger::Panel;

impl LoadRunnerApp {
    pub(super) fn open_debugger(&mut self, target: Option<&str>) {
        let (id, name) = match self.ipid_processor(target) {
            Ok(device) => (device.id.clone(), device.display_name().to_owned()),
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        if let Some(panel) = self.debuggers.get_mut(&id) {
            panel.open = true;
        } else {
            self.debuggers.insert(id.clone(), Panel::new(id, &name));
        }
    }

    pub(super) fn show_debuggers(&mut self, ctx: &egui::Context) {
        let missing: Vec<_> = self
            .debuggers
            .keys()
            .filter(|id| !self.devices.iter().any(|device| &device.id == *id))
            .cloned()
            .collect();
        for id in missing {
            if let Some(mut panel) = self.debuggers.remove(&id) {
                panel.stop();
            }
        }
        for panel in self.debuggers.values_mut() {
            if panel.open {
                panel.show(ctx);
            } else {
                panel.poll();
            }
            if !panel.open
                && let Some(error) = panel.cleanup_error.take()
            {
                self.notice = Some(format!("Debugger cleanup could not be confirmed: {error}"));
            }
        }
        let starters: Vec<_> = self
            .debuggers
            .iter()
            .filter(|(_, panel)| {
                panel.open
                    && panel.awaiting_session
                    && panel.fetch.is_none()
                    && panel.pending_session.is_none()
                    && panel.input.is_none()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in starters {
            self.start_debugger(&id);
        }
        let waiting: Vec<_> = self
            .debuggers
            .iter_mut()
            .filter_map(|(id, panel)| {
                panel.pending_session.take().map(|session| {
                    (
                        id.clone(),
                        panel.slot,
                        crate::debugger::debug_signums(&panel.signals),
                        session,
                    )
                })
            })
            .collect();
        for (id, slot, signums, (input, output)) in waiting {
            if let Some(panel) = self.debuggers.get_mut(&id) {
                panel.session_sent = true;
            }
            if let Some(connection) = self.connection_spec(&id) {
                if let Err(error) = self
                    .worker_pool
                    .open_debugger(&id, connection, slot, signums, input, output)
                {
                    self.notice = Some(error);
                }
            } else {
                let _ = output.try_send(crate::ssh::DebugEvent::Closed(
                    "Processor no longer listed".into(),
                ));
            }
        }
        self.debuggers
            .retain(|_, panel| panel.open || panel.stopping);
        if self.pending_debug_action.is_some()
            && !self
                .debuggers
                .values()
                .any(|panel| panel.session_sent || panel.stopping)
        {
            let action = self.pending_debug_action.take().unwrap();
            if self.notice.is_none() {
                self.execute_action(action, ctx);
            }
        }
    }

    pub(super) fn start_debugger(&mut self, id: &str) {
        let slot = match self.debuggers.get(id) {
            Some(panel) if panel.open => panel.slot,
            _ => return,
        };
        if self.connection_needs_credentials(id) {
            if self.credential_prompt.is_none() {
                let target = self.device_name(id);
                self.open_credential_prompt(
                    target,
                    Some(CredentialGate::StartDebug(id.to_owned())),
                    None,
                );
            }
            return;
        }
        let Some(connection) = self.connection_spec(id) else {
            return;
        };
        let assigned_name = self
            .devices
            .iter()
            .find(|device| device.id == id)
            .and_then(|device| device.program_slots.get(usize::from(slot - 1)))
            .and_then(|program| program.as_ref())
            .map(|program| program.to_string_lossy().to_string());
        let (reply, fetch) = mpsc::channel();
        if let Some(panel) = self.debuggers.get_mut(id) {
            panel.awaiting_session = false;
            panel.fetch = Some(fetch);
            panel.reset_signals();
        }
        if let Err(error) = self.worker_pool.send(
            id,
            WorkerCommand::ReadDebugSignature {
                connection,
                slot,
                assigned_name,
                reply,
            },
        ) && let Some(panel) = self.debuggers.get_mut(id)
        {
            panel.fetch = None;
            panel.status = error;
        }
    }
}
