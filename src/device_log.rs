use std::{
    collections::VecDeque,
    time::{SystemTime, UNIX_EPOCH},
};

/// Whether a line went to the device, came back from it, or is the app's own
/// annotation of something that is not console text (a connection attempt, an
/// SFTP transfer, a failure).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Sent,
    Received,
    Note,
}

impl Direction {
    pub fn arrow(self) -> &'static str {
        match self {
            Self::Sent => "→",
            Self::Received => "←",
            Self::Note => "·",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub at: SystemTime,
    /// Endpoint id, so an entry stays attributable after a device is renamed
    /// or removed.
    pub device: String,
    pub direction: Direction,
    pub text: String,
}

impl LogEntry {
    /// UTC, because the standard library cannot resolve a local time zone
    /// without pulling in a dependency.
    pub fn clock(&self) -> String {
        let seconds = self
            .at
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default();
        let day = seconds % 86_400;
        format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
    }
}

/// A bounded in-memory transcript of every device conversation this session.
#[derive(Debug, Default)]
pub struct DeviceLog {
    entries: VecDeque<LogEntry>,
    dropped: usize,
}

impl DeviceLog {
    /// Enough to cover a session's worth of loads without letting a chatty
    /// device grow the process without limit.
    pub const CAPACITY: usize = 2_000;
    const MAX_TEXT: usize = 8_192;

    pub fn push(&mut self, device: String, direction: Direction, text: &str) {
        let text = text.trim_end();
        if text.is_empty() {
            return;
        }
        let text = match text.char_indices().nth(Self::MAX_TEXT) {
            Some((cut, _)) => format!("{}… (truncated)", &text[..cut]),
            None => text.to_owned(),
        };
        if self.entries.len() == Self::CAPACITY {
            self.entries.pop_front();
            self.dropped += 1;
        }
        self.entries.push_back(LogEntry {
            at: SystemTime::now(),
            device,
            direction,
            text,
        });
    }

    pub fn entries(&self) -> impl Iterator<Item = &LogEntry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many entries have been discarded to stay within [`Self::CAPACITY`].
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.dropped = 0;
    }

    /// The whole transcript as text, for copying out of the log window.
    pub fn to_transcript(&self, name: impl Fn(&str) -> String) -> String {
        self.entries
            .iter()
            .map(|entry| {
                format!(
                    "{} {} {} {}",
                    entry.clock(),
                    name(&entry.device),
                    entry.direction.arrow(),
                    entry.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_lines_are_not_recorded() {
        let mut log = DeviceLog::default();
        log.push("a:22".into(), Direction::Received, "   \n\n");
        log.push("a:22".into(), Direction::Received, "");
        assert!(log.is_empty());
    }

    #[test]
    fn oldest_entries_are_dropped_and_counted() {
        let mut log = DeviceLog::default();
        for index in 0..DeviceLog::CAPACITY + 5 {
            log.push("a:22".into(), Direction::Sent, &format!("line {index}"));
        }
        assert_eq!(log.len(), DeviceLog::CAPACITY);
        assert_eq!(log.dropped(), 5);
        assert_eq!(log.entries().next().unwrap().text, "line 5");
    }

    #[test]
    fn oversized_output_is_truncated_on_a_character_boundary() {
        let mut log = DeviceLog::default();
        log.push("a:22".into(), Direction::Received, &"é".repeat(9_000));
        let text = &log.entries().next().unwrap().text;
        assert!(text.ends_with("… (truncated)"));
        assert_eq!(text.chars().filter(|c| *c == 'é').count(), 8_192);
    }

    #[test]
    fn transcript_resolves_display_names() {
        let mut log = DeviceLog::default();
        log.push("192.0.2.1:22".into(), Direction::Sent, "hostname");
        log.push("192.0.2.1:22".into(), Direction::Received, "ROOM");
        let transcript = log.to_transcript(|_| "W223-CP".to_owned());
        let mut lines = transcript.lines();
        assert!(lines.next().unwrap().ends_with("W223-CP → hostname"));
        assert!(lines.next().unwrap().ends_with("W223-CP ← ROOM"));
    }

    #[test]
    fn clearing_resets_the_dropped_count() {
        let mut log = DeviceLog::default();
        log.push("a:22".into(), Direction::Note, "connecting");
        log.clear();
        assert!(log.is_empty());
        assert_eq!(log.dropped(), 0);
    }
}
