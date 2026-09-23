//! An interactive SSH session in its own window.
//!
//! This is a line console, not a terminal emulator: a line is sent when it is
//! entered, and what comes back is shown as text. Control sequences a real
//! terminal would act on are removed rather than obeyed, since a Crestron
//! console sends few of them and acting on half of them would be worse than
//! acting on none.

use eframe::egui;

use crate::ssh::TerminalEvent;

/// Scrollback kept per session. A device that talks continuously must not grow
/// the process without limit.
const SCROLLBACK: usize = 256 * 1024;

/// Marks where one session ends and the next begins in the kept scrollback.
const RECONNECTING: &str = "-- reconnecting --\n";

pub struct Terminal {
    pub open: bool,
    title: String,
    /// The ends of a session this window has asked for and not yet been given.
    /// Sessions are opened by whoever holds the device's connection, which is
    /// the application rather than a window, so asking is all a window does.
    pending_session: Option<(
        tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        std::sync::mpsc::Sender<TerminalEvent>,
    )>,
    input: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    output: std::sync::mpsc::Receiver<TerminalEvent>,
    received: String,
    entry: String,
    /// Entered lines, newest last, walked with the up and down arrows.
    history: Vec<String>,
    recalled: Option<usize>,
    status: String,
    connected: bool,
    /// Set when the session is ended from here, so that closing it deliberately
    /// does not read as the device having dropped it.
    disconnecting: bool,
    /// Set while a new session is opening, so the window is ready to be typed
    /// into rather than leaving the keyboard on whatever button was pressed.
    focus_entry: bool,
    /// Whether the view stays at the newest line. Held across frames rather
    /// than set when output arrives: the scroll area has to be told on every
    /// frame, since content that grows on one frame is measured on the next.
    follow: bool,
}

impl Terminal {
    /// Starts connecting at once: the window opening is the request.
    pub fn open(name: &str, host: &str, port: u16) -> Self {
        // Fixed when the window opens: the title is also what tells one
        // window from another, so a later rename must not move it.
        let title = format!("SSH — {name} ({host}:{port})");
        let (_, output) = std::sync::mpsc::channel();
        let mut terminal = Self {
            open: true,
            title,
            pending_session: None,
            input: None,
            output,
            received: String::new(),
            entry: String::new(),
            history: Vec::new(),
            recalled: None,
            status: String::new(),
            connected: false,
            disconnecting: false,
            focus_entry: false,
            follow: true,
        };
        terminal.connect();
        terminal
    }

    /// Gives an open window a session again when it has lost one, so that
    /// asking to connect to a device that already has a window does something.
    pub fn ensure_connected(&mut self) {
        if self.input.is_none() {
            self.connect();
        }
    }

    /// Starts a session and takes the window's end of it. Anything already
    /// received stays, with a line to say where the new session begins.
    fn connect(&mut self) {
        if !self.received.is_empty() {
            if !self.received.ends_with('\n') {
                self.received.push('\n');
            }
            self.received.push_str(RECONNECTING);
        }
        let (input, from_window) = tokio::sync::mpsc::unbounded_channel();
        let (to_window, output) = std::sync::mpsc::channel();
        self.output = output;
        self.input = Some(input);
        self.connected = false;
        self.disconnecting = false;
        self.status = "Connecting…".to_owned();
        self.focus_entry = true;
        self.follow = true;
        // Left for the application to pick up: the connection a session runs on
        // belongs to the device, and the settings it needs are looked up when
        // the session is started rather than when this window was opened.
        self.pending_session = Some((from_window, to_window));
    }

    /// Whether this window is still waiting for a session to be started for
    /// it. Asked before taking one, so that a window whose device has no
    /// credentials yet keeps waiting instead of being handed a connection
    /// that cannot be made.
    pub fn has_pending_session(&self) -> bool {
        self.pending_session.is_some()
    }

    /// Hands over the ends of a session this window is waiting for, to whoever
    /// can start one.
    pub fn take_pending_session(
        &mut self,
    ) -> Option<(
        tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        std::sync::mpsc::Sender<TerminalEvent>,
    )> {
        self.pending_session.take()
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        self.drain();
        let open = crate::popout::window(ctx, &self.title.clone(), [900.0, 600.0], |ui| {
            self.contents(ui);
        });
        self.open = open;
        if !self.open {
            self.disconnect();
        }
    }

    /// Dropping the sender is what ends the session; the thread notices and
    /// closes the channel behind it.
    fn disconnect(&mut self) {
        self.disconnecting = true;
        self.input = None;
    }

    fn drain(&mut self) {
        loop {
            let event = match self.output.try_recv() {
                Ok(event) => event,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // The session ended without saying so, because whatever was
                // holding the other end went with it. A window that still
                // believes it has one has to be told, or it sits reading
                // "Connected" at nothing.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if self.input.is_some() {
                        self.connected = false;
                        self.input = None;
                        self.status = if self.disconnecting {
                            "Disconnected".to_owned()
                        } else {
                            "The session has ended".to_owned()
                        };
                    }
                    break;
                }
            };
            match event {
                TerminalEvent::Opened => {
                    self.connected = true;
                    self.status = "Connected".to_owned();
                }
                TerminalEvent::Output(data) => self.received.push_str(&readable(&data)),
                TerminalEvent::Closed(reason) => {
                    self.connected = false;
                    self.input = None;
                    self.status = match () {
                        () if !reason.is_empty() => reason,
                        () if self.disconnecting => "Disconnected".to_owned(),
                        () => "The device closed the session".to_owned(),
                    };
                }
            }
            let overflow = self.received.len().saturating_sub(SCROLLBACK);
            if overflow > 0 {
                // On a line boundary, so the top of the view is not half a line.
                let cut = self.received[overflow..]
                    .find('\n')
                    .map_or(self.received.len(), |end| overflow + end + 1);
                self.received.drain(..cut);
            }
        }
    }

    fn contents(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("terminal_status").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if self.connected {
                    ui.label("Connected");
                } else {
                    ui.colored_label(ui.visuals().warn_fg_color, &self.status);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // A session that has ended leaves the window in place, so
                    // the same button is what opens another.
                    if self.input.is_none() {
                        if ui.button("Reconnect").clicked() {
                            self.connect();
                        }
                    } else if ui
                        .add_enabled(self.connected, egui::Button::new("Disconnect"))
                        .clicked()
                    {
                        self.disconnect();
                    }
                    ui.checkbox(&mut self.follow, "Auto-scroll")
                        .on_hover_text("Keep the newest line in view");
                });
            });
            ui.add_space(4.0);
        });
        egui::Panel::bottom("terminal_entry").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let entry = ui.add_enabled(
                    self.connected,
                    egui::TextEdit::singleline(&mut self.entry)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("Type a console command and press Enter")
                        .desired_width(f32::INFINITY),
                );
                // Once the session is up, the field is where typing goes.
                if self.connected && std::mem::take(&mut self.focus_entry) {
                    entry.request_focus();
                }
                if entry.has_focus() {
                    self.recall(ui);
                }
                if entry.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    self.send();
                    entry.request_focus();
                }
            });
            ui.small(
                "Lines are sent as they are entered, and everything both ways reaches the device log.",
            );
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("terminal_scrollback")
                .auto_shrink([false, false])
                .stick_to_bottom(self.follow)
                .show(ui, |ui| {
                    let mut text = self.received.as_str();
                    ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
                    );
                });
        });
    }

    /// The up and down arrows walk back through what has been entered, the way
    /// a shell does.
    fn recall(&mut self, ui: &egui::Ui) {
        let (up, down) = ui.input(|input| {
            (
                input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::ArrowDown),
            )
        });
        if !up && !down || self.history.is_empty() {
            return;
        }
        self.recalled = match (self.recalled, up) {
            (None, true) => Some(self.history.len() - 1),
            (Some(0), true) => Some(0),
            (Some(index), true) => Some(index - 1),
            (Some(index), false) if index + 1 < self.history.len() => Some(index + 1),
            (Some(_), false) => None,
            (None, false) => None,
        };
        self.entry = self
            .recalled
            .map(|index| self.history[index].clone())
            .unwrap_or_default();
    }

    fn send(&mut self) {
        let Some(input) = &self.input else {
            return;
        };
        let line = std::mem::take(&mut self.entry);
        self.recalled = None;
        if !line.trim().is_empty() && self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        // The console ends a line the way a terminal does.
        if input.send(format!("{line}\r").into_bytes()).is_err() {
            self.connected = false;
            self.status = "The session has ended".to_owned();
        }
    }
}

/// Text as a log can hold it: the escape sequences a terminal would act on
/// removed, line endings made uniform, and nothing else left that would move
/// the cursor about.
fn readable(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(data);
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => match characters.next() {
                // A control sequence runs to its first byte in @ through ~.
                Some('[') => {
                    while characters
                        .next()
                        .is_some_and(|byte| !matches!(byte, '\u{40}'..='\u{7e}'))
                    {
                    }
                }
                // An operating-system command runs to a bell or an escape.
                Some(']') => {
                    while characters
                        .next()
                        .is_some_and(|byte| !matches!(byte, '\u{7}' | '\u{1b}'))
                    {
                    }
                }
                // An intermediate byte runs on to a final one in 0 through ~.
                Some('\u{20}'..='\u{2f}') => {
                    while characters
                        .next()
                        .is_some_and(|byte| !matches!(byte, '\u{30}'..='\u{7e}'))
                    {
                    }
                }
                // Anything else is a two-character sequence already consumed.
                _ => {}
            },
            '\r' => {
                // Carriage returns end a line here, and never on their own
                // reopen one that has already ended.
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
            }
            '\n' | '\t' => out.push(character),
            character if character.is_control() => {}
            character => out.push(character),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_shown_as_text_with_the_terminal_control_removed() {
        assert_eq!(readable(b"hostname\r\nRMC4\r\n"), "hostname\nRMC4\n");
        // A bare carriage return ends the line, and does not double one.
        assert_eq!(readable(b"one\rtwo\r\n"), "one\ntwo\n");
        assert_eq!(readable(b"done\n\r"), "done\n");
        // Colour and cursor moves are dropped, and the text between them kept.
        assert_eq!(
            readable(b"\x1b[32mgreen\x1b[0m and \x1b[1;31mred\x1b[m"),
            "green and red"
        );
        assert_eq!(readable(b"\x1b]0;a title\x07shell>"), "shell>");
        assert_eq!(readable(b"\x1b(Bplain"), "plain");
        // Tabs survive; the rest of the control characters do not.
        assert_eq!(readable(b"a\tb\x07\x00c"), "a\tbc");
        // An unfinished sequence takes nothing with it that came before.
        assert_eq!(readable(b"kept\x1b["), "kept");
    }

    /// A window with no session running, which is the state a terminal is left
    /// in when the device closes it. Built directly so that no test needs a
    /// device to talk to.
    fn idle() -> Terminal {
        let (_, output) = std::sync::mpsc::channel();
        Terminal {
            open: true,
            title: "SSH — test".to_owned(),
            pending_session: None,
            input: None,
            output,
            received: String::new(),
            entry: String::new(),
            history: Vec::new(),
            recalled: None,
            status: "The device closed the session".to_owned(),
            connected: false,
            disconnecting: false,
            focus_entry: false,
            follow: true,
        }
    }

    fn input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 700.0),
            )),
            ..Default::default()
        }
    }

    /// Every string the window draws, and where it draws it.
    fn drawn(ctx: &egui::Context, terminal: &mut Terminal) -> Vec<(String, egui::Rect)> {
        for _ in 0..2 {
            ctx.run_ui(input(), |ui| terminal.show(ui.ctx()))
                .drop_without_applying_deltas();
        }
        let output = ctx.run_ui(input(), |ui| terminal.show(ui.ctx()));
        let found = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_owned(),
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                )),
                _ => None,
            })
            .collect();
        output.drop_without_applying_deltas();
        found
    }

    fn click(ctx: &egui::Context, terminal: &mut Terminal, label: &str) {
        let drawn = drawn(ctx, terminal);
        let pos = drawn
            .iter()
            .find_map(|(text, rect)| (text == label).then(|| rect.center()))
            .unwrap_or_else(|| panic!("{label} is not on screen: {drawn:?}"));
        for pressed in [true, false] {
            let mut raw = input();
            raw.events.push(egui::Event::PointerMoved(pos));
            raw.events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            ctx.run_ui(raw, |ui| terminal.show(ui.ctx()))
                .drop_without_applying_deltas();
        }
    }

    #[test]
    fn a_new_session_is_ready_to_be_typed_into() {
        let ctx = egui::Context::default();
        let mut terminal = idle();
        // As the window stands the moment a session opens.
        terminal.connected = true;
        terminal.status = "Connected".to_owned();
        terminal.focus_entry = true;
        ctx.run_ui(input(), |ui| terminal.show(ui.ctx()))
            .drop_without_applying_deltas();
        let mut raw = input();
        raw.events.push(egui::Event::Text("hostname".to_owned()));
        ctx.run_ui(raw, |ui| terminal.show(ui.ctx()))
            .drop_without_applying_deltas();
        assert_eq!(
            terminal.entry, "hostname",
            "typing went somewhere other than the entry field"
        );
    }

    #[test]
    fn a_window_whose_session_ended_offers_to_open_another() {
        let ctx = egui::Context::default();
        let mut terminal = idle();
        terminal.received.push_str("RMC4 Console\nRMC4>\n");
        let labels = drawn(&ctx, &mut terminal);
        let says = |labels: &[(String, egui::Rect)], wanted: &str| {
            labels.iter().any(|(text, _)| text == wanted)
        };
        assert!(says(&labels, "Reconnect"), "{labels:?}");
        assert!(!says(&labels, "Disconnect"), "one button, not both");
        assert!(says(&labels, "The device closed the session"));

        click(&ctx, &mut terminal, "Reconnect");
        // Asking for a session is what the button does; whether this unreachable
        // address gives one is the session's business, not the button's.
        assert!(
            terminal.received.contains(RECONNECTING),
            "the scrollback does not say where the new session begins"
        );
        assert!(
            terminal.received.starts_with("RMC4 Console"),
            "what the last session said is still there"
        );
        assert_ne!(terminal.status, "The device closed the session");
        assert!(!terminal.disconnecting);
    }

    #[test]
    fn a_first_session_is_not_marked_as_a_reconnection() {
        let mut terminal = idle();
        terminal.connect();
        assert!(!terminal.received.contains(RECONNECTING));
        assert!(terminal.received.is_empty());
    }

    #[test]
    fn asking_for_a_session_leaves_its_ends_for_somebody_to_pick_up() {
        let mut terminal = idle();
        assert!(terminal.take_pending_session().is_none());
        terminal.connect();
        assert!(
            terminal.take_pending_session().is_some(),
            "nothing was left for the application to start a session with"
        );
        assert!(
            terminal.take_pending_session().is_none(),
            "the same session was handed out twice"
        );
    }

    /// A session can end without saying so, when whatever was holding the other
    /// end went with it. The window has to notice, or it sits reading
    /// "Connected" at nothing and swallows what is typed into it.
    #[test]
    fn a_window_whose_session_vanished_stops_saying_it_is_connected() {
        let mut terminal = idle();
        terminal.connect();
        // Taken and dropped, the way a worker that has stopped would.
        drop(terminal.take_pending_session());
        terminal.connected = true;
        terminal.drain();
        assert!(!terminal.connected);
        assert!(
            terminal.input.is_none(),
            "the window still offers Disconnect"
        );
        assert_eq!(terminal.status, "The session has ended");
    }

    /// A terminal with more output than fits, so there is something to scroll.
    fn filled(follow: bool) -> Terminal {
        let mut terminal = idle();
        terminal.status = "Connected".to_owned();
        terminal.connected = true;
        terminal.follow = follow;
        for line in 0..200 {
            terminal.received.push_str(&format!("line {line}\n"));
        }
        terminal
    }

    /// Where the scrollback galley sits, and the rectangle it is clipped to.
    fn scrollback(ctx: &egui::Context, terminal: &mut Terminal) -> (egui::Rect, egui::Rect) {
        // The scroll area measures its content before it can sit at the end of it.
        for _ in 0..3 {
            ctx.run_ui(input(), |ui| terminal.show(ui.ctx()))
                .drop_without_applying_deltas();
        }
        let output = ctx.run_ui(input(), |ui| terminal.show(ui.ctx()));
        let found = output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text().starts_with("line 0\n") => Some((
                    egui::Rect::from_min_size(text.pos, text.galley.size()),
                    clipped.clip_rect,
                )),
                _ => None,
            })
            .expect("the scrollback was not drawn");
        output.drop_without_applying_deltas();
        found
    }

    #[test]
    fn the_newest_line_stays_in_view_until_auto_scroll_is_turned_off() {
        let ctx = egui::Context::default();
        let (galley, clip) = scrollback(&ctx, &mut filled(true));
        assert!(
            galley.height() > clip.height(),
            "the fixture has to be taller than the view to say anything"
        );
        // Following: the end of the output is what the view is showing.
        assert!(
            (galley.bottom() - clip.bottom()).abs() < 4.0,
            "the newest line is not in view: galley {galley:?} in {clip:?}"
        );

        let ctx = egui::Context::default();
        let (galley, clip) = scrollback(&ctx, &mut filled(false));
        // Not following: the view stays where it was, which is the beginning.
        assert!(
            (galley.top() - clip.top()).abs() < 4.0,
            "the view moved without being asked: galley {galley:?} in {clip:?}"
        );
    }

    #[test]
    fn auto_scroll_can_be_turned_off_from_the_window() {
        let ctx = egui::Context::default();
        let mut terminal = filled(true);
        click(&ctx, &mut terminal, "Auto-scroll");
        assert!(!terminal.follow, "the checkbox did not turn following off");
    }

    #[test]
    fn scrollback_is_trimmed_on_a_line_boundary() {
        let mut terminal = idle();
        let (sender, receiver) = std::sync::mpsc::channel();
        terminal.output = receiver;
        let line = "0123456789abcdef\n";
        for _ in 0..(SCROLLBACK / line.len() + 100) {
            sender
                .send(TerminalEvent::Output(line.as_bytes().to_vec()))
                .unwrap();
        }
        terminal.drain();
        assert!(terminal.received.len() <= SCROLLBACK);
        assert!(
            terminal.received.starts_with(line),
            "the view starts at a line, not part of one"
        );
        assert!(terminal.received.ends_with(line));
    }
}
