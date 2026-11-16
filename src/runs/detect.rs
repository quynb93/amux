//! Works out from a run's output when the CLI is waiting for you: a permission or
//! confirmation prompt (matched by the provider's `waitingPatterns`), a terminal bell, or
//! a notification escape sequence (OSC 9, OSC 777 or OSC 99).
//!
//! When the CLI reports its own state with OSC 7501 (see program.rs), that wins: from the
//! first report on, only the reports decide whether it is waiting.
//!
//! Escape sequences are stripped as bytes arrive, across chunk boundaries, and the text
//! that remains is matched case-insensitively against the last few KB of output.

/// Text kept for matching. Prompts are short and drawn in one go.
const WINDOW: usize = 4096;
/// Output after the last signal that, with no new signal, means the CLI moved on.
const CLEAR_AFTER: usize = 4096;
/// OSC 7501 allows up to 4096 bytes; longer sequences are discarded whole.
const MAX_OSC: usize = 4096;

use super::program::{self, Osc, Report};

#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    /// The CLI started waiting; the reason is the prompt text or notification message.
    Waiting(String),
    Cleared,
}

impl Default for Detector {
    fn default() -> Self {
        Detector::new(&[])
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Ground,
    Esc,
    Csi,
    Osc,
    /// ESC seen inside an OSC: `ESC \` ends it.
    OscEsc,
}

pub struct Detector {
    /// (lowercased for matching, as written for messages)
    patterns: Vec<(String, String)>,
    text: Vec<u8>,
    osc: Vec<u8>,
    mode: Mode,
    waiting: bool,
    /// The OSC being read grew past MAX_OSC.
    osc_overflow: bool,
    /// The CLI has sent an OSC 7501 report, so its reports decide the state.
    reports: bool,
    /// Its current root record.
    record: Option<Report>,
    record_changed: bool,
    probed: bool,
    since_signal: usize,
}

impl Detector {
    pub fn new(patterns: &[String]) -> Self {
        Detector {
            patterns: patterns
                .iter()
                .filter(|p| !p.is_empty())
                .map(|p| (p.to_lowercase(), p.clone()))
                .collect(),
            text: Vec::with_capacity(WINDOW * 2),
            osc: vec![],
            mode: Mode::Ground,
            waiting: false,
            osc_overflow: false,
            reports: false,
            record: None,
            record_changed: false,
            probed: false,
            since_signal: 0,
        }
    }

    #[cfg(test)]
    pub fn waiting(&self) -> bool {
        self.waiting
    }

    /// The user typed something: whatever the CLI was waiting for has been answered.
    pub fn answered(&mut self) -> Option<Change> {
        // The CLI says itself when it stops being blocked.
        if self.reports {
            return None;
        }
        self.text.clear();
        self.since_signal = 0;
        std::mem::take(&mut self.waiting).then_some(Change::Cleared)
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Option<Change> {
        let mut signal: Option<String> = None;
        for &b in chunk {
            match self.mode {
                Mode::Ground => match b {
                    0x1b => self.mode = Mode::Esc,
                    0x07 => signal = Some("terminal bell".into()),
                    b'\r' | b'\n' | b'\t' => self.push(b' '),
                    0x00..=0x1f | 0x7f => {}
                    _ => self.push(b),
                },
                Mode::Esc => {
                    self.mode = match b {
                        b'[' => Mode::Csi,
                        b']' => {
                            self.osc.clear();
                            self.osc_overflow = false;
                            Mode::Osc
                        }
                        _ => Mode::Ground,
                    }
                }
                Mode::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        // Cursor moves separate words that are drawn apart.
                        self.push(b' ');
                        self.mode = Mode::Ground;
                    }
                }
                Mode::Osc => match b {
                    0x07 => {
                        signal = self.osc_end().or(signal);
                        self.mode = Mode::Ground;
                    }
                    0x1b => self.mode = Mode::OscEsc,
                    _ if self.osc.len() < MAX_OSC => self.osc.push(b),
                    _ => self.osc_overflow = true,
                },
                Mode::OscEsc => {
                    if b == b'\\' {
                        signal = self.osc_end().or(signal);
                    }
                    self.mode = Mode::Ground;
                }
            }
        }
        if self.reports {
            let blocked = self.record.as_ref().filter(|r| r.state == "blocked");
            return match (blocked, self.waiting) {
                (Some(r), false) => {
                    self.waiting = true;
                    Some(Change::Waiting(r.reason()))
                }
                (None, true) => {
                    self.waiting = false;
                    Some(Change::Cleared)
                }
                _ => None,
            };
        }
        let signal = signal.or_else(|| self.matched());
        if let Some(reason) = signal {
            self.since_signal = 0;
            if !self.waiting {
                self.waiting = true;
                return Some(Change::Waiting(reason));
            }
            return None;
        }
        self.since_signal += chunk.len();
        if self.waiting && self.since_signal >= CLEAR_AFTER {
            self.waiting = false;
            self.text.clear();
            return Some(Change::Cleared);
        }
        None
    }

    /// Whether the CLI sent the OSC 7501 probe since the last call: the host answers it.
    pub fn take_probe(&mut self) -> bool {
        std::mem::take(&mut self.probed)
    }

    /// The CLI's new root record, if it changed since the last call (None: cleared).
    pub fn take_record(&mut self) -> Option<Option<Report>> {
        std::mem::take(&mut self.record_changed).then(|| self.record.clone())
    }

    /// The process ended: records that live only as long as it does are dropped.
    pub fn exited(&mut self) -> Option<Option<Report>> {
        if self.record.as_ref().is_some_and(Report::ends_with_process) {
            self.set_record(None);
        }
        self.take_record()
    }

    fn set_record(&mut self, r: Option<Report>) {
        if self.record != r {
            self.record = r;
            self.record_changed = true;
        }
    }

    /// An OSC ended. Returns a notification message (OSC 9, 777, 99) unless the CLI
    /// reports with OSC 7501, whose records are handled here.
    fn osc_end(&mut self) -> Option<String> {
        if std::mem::take(&mut self.osc_overflow) {
            return None;
        }
        match program::parse(&self.osc) {
            Some(Osc::Probe) => self.probed = true,
            Some(Osc::Report(r)) => {
                self.reports = true;
                self.set_record(Some(r));
            }
            Some(Osc::Clear) => self.set_record(None),
            Some(Osc::Prompt) => {
                if self
                    .record
                    .as_ref()
                    .is_some_and(|r| r.state == "working" || r.state == "blocked")
                {
                    self.set_record(None);
                }
            }
            Some(Osc::Ignored) => {}
            None if !self.reports => return osc_message(&self.osc),
            None => {}
        }
        None
    }

    fn push(&mut self, b: u8) {
        // Collapse runs of spaces, so layout padding doesn't push prompts out of the window.
        if b == b' ' && self.text.last() == Some(&b' ') {
            return;
        }
        self.text.push(b);
        if self.text.len() > WINDOW * 2 {
            self.text.drain(..self.text.len() - WINDOW);
        }
    }

    fn matched(&self) -> Option<String> {
        if self.patterns.is_empty() {
            return None;
        }
        let start = self.text.len().saturating_sub(WINDOW);
        let text = String::from_utf8_lossy(&self.text[start..]).to_lowercase();
        self.patterns
            .iter()
            .find(|(lower, _)| text.contains(lower.as_str()))
            .map(|(_, orig)| orig.clone())
    }
}

/// The message of a notification OSC, or None for other OSCs (titles, links, progress).
fn osc_message(osc: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(osc);
    let mut parts = s.splitn(2, ';');
    let (code, rest) = (parts.next()?, parts.next().unwrap_or(""));
    let msg = match code {
        // iTerm2 style; `9;4;...` is a progress report in other terminals.
        "9" if !rest.starts_with("4;") => rest.to_string(),
        // `777;notify;title;body`
        "777" => {
            let mut f = rest.splitn(3, ';');
            if f.next() != Some("notify") {
                return None;
            }
            let (title, body) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
            if body.is_empty() {
                title.to_string()
            } else {
                format!("{title}: {body}")
            }
        }
        // kitty: `99;metadata;payload`
        "99" => rest
            .split_once(';')
            .map(|(_, p)| p)
            .unwrap_or("")
            .to_string(),
        _ => return None,
    };
    let msg = msg.trim().to_string();
    Some(if msg.is_empty() {
        "notification".into()
    } else {
        msg
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det() -> Detector {
        Detector::new(&["Do you want to".into(), "Enter to confirm".into()])
    }

    #[test]
    fn matches_prompts_through_escape_sequences_split_across_chunks() {
        let mut d = det();
        assert_eq!(d.feed(b"\x1b[1mDo you \x1b[3"), None);
        assert_eq!(
            d.feed(b"8;5;2mwant to\x1b[0m proceed?"),
            Some(Change::Waiting("Do you want to".into()))
        );
        // Still on screen: no second notice.
        assert_eq!(d.feed(b" 1. Yes"), None);
        assert_eq!(d.answered(), Some(Change::Cleared));
        assert_eq!(d.feed(b"working"), None);
    }

    #[test]
    fn treats_bells_and_notification_oscs_as_waiting() {
        let mut d = det();
        assert_eq!(
            d.feed(b"done\x07"),
            Some(Change::Waiting("terminal bell".into()))
        );
        d.answered();
        assert_eq!(
            d.feed(b"\x1b]9;Claude needs your permission\x07"),
            Some(Change::Waiting("Claude needs your permission".into()))
        );
        d.answered();
        assert_eq!(
            d.feed(b"\x1b]777;notify;Codex;Approve command?\x1b\\"),
            Some(Change::Waiting("Codex: Approve command?".into()))
        );
    }

    #[test]
    fn ignores_titles_links_and_progress() {
        let mut d = det();
        assert_eq!(
            d.feed(b"\x1b]0;my title\x07\x1b]8;;https://x\x07link\x1b]8;;\x07"),
            None
        );
        assert_eq!(d.feed(b"\x1b]9;4;1;50\x07"), None);
        assert!(!d.waiting());
    }

    #[test]
    fn osc_7501_reports_decide_once_they_arrive() {
        let mut d = det();
        // The probe, split across reads, is noticed once.
        assert_eq!(d.feed(b"\x1b]75"), None);
        assert_eq!(d.feed(b"01;?\x1b\\"), None);
        assert!(d.take_probe());
        assert!(!d.take_probe());
        assert_eq!(d.feed(b"\x1b]7501;state=working:app=claude\x07"), None);
        assert_eq!(d.take_record().unwrap().unwrap().state, "working");
        assert_eq!(d.take_record(), None);
        // From now on, prompt text, bells and OSC 9 don't count.
        assert_eq!(d.feed(b"Do you want to\x07\x1b]9;hey\x07"), None);
        assert_eq!(
            d.feed(b"\x1b]7501;state=blocked:kind=question\x07"),
            Some(Change::Waiting("a question".into()))
        );
        // Typing, or a lot of output, doesn't end it; the CLI's next report does.
        assert_eq!(d.answered(), None);
        assert_eq!(d.feed(&[b'x'; CLEAR_AFTER * 2]), None);
        assert_eq!(
            d.feed(b"\x1b]7501;state=working\x07"),
            Some(Change::Cleared)
        );
        // A new shell prompt ends a working record; so does the process ending.
        d.take_record();
        d.feed(b"\x1b]133;A\x07");
        assert_eq!(d.take_record(), Some(None));
        d.feed(b"\x1b]7501;state=working\x07");
        assert_eq!(d.exited(), Some(None));
        // done survives the process.
        d.feed(b"\x1b]7501;state=done\x07");
        d.take_record();
        assert_eq!(d.exited(), None);
    }

    #[test]
    fn discards_oversized_osc_7501() {
        let mut d = det();
        let mut seq = b"\x1b]7501;state=blocked:msg=".to_vec();
        seq.extend(std::iter::repeat_n(b'A', MAX_OSC));
        seq.push(0x07);
        assert_eq!(d.feed(&seq), None);
        assert_eq!(d.take_record(), None);
        // The next one is read normally.
        d.feed(b"\x1b]7501;state=idle\x07");
        assert_eq!(d.take_record().unwrap().unwrap().state, "idle");
    }

    #[test]
    fn clears_once_the_cli_has_moved_on() {
        let mut d = det();
        d.feed(b"\x07");
        assert_eq!(d.feed(&[b'x'; 1000]), None);
        assert_eq!(d.feed(&[b'y'; CLEAR_AFTER]), Some(Change::Cleared));
        assert!(!d.waiting());
    }
}
