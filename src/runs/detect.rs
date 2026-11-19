//! Works out from a run's output when the CLI is waiting for you: a permission or
//! confirmation prompt (matched by the provider's `waitingPatterns`), a terminal bell, or
//! a notification escape sequence (OSC 9, OSC 777 or OSC 99).
//!
//! When the CLI reports its own state with OSC 7501 (see program.rs), that wins: from the
//! first report on, only the reports decide whether it is waiting. Reports from state hooks
//! (`amux state-hooks`, see state_hooks.rs) count the same way until the first OSC 7501
//! report, which then takes over: OSC 7501 > hooks > patterns and timing.
//!
//! Escape sequences are stripped as bytes arrive, across chunk boundaries, and the text
//! that remains is matched case-insensitively against the last few KB of output.

/// Text kept for matching. Prompts are short and drawn in one go.
const WINDOW: usize = 4096;
/// Output after the last signal that, with no new signal, means the CLI moved on.
const CLEAR_AFTER: usize = 4096;
/// OSC 7501 allows up to 4096 bytes; longer sequences are discarded whole.
const MAX_OSC: usize = 4096;
/// How long after you (or auto-approve) answered a hook-reported block a repeat of it
/// (Claude Code's permission notification, see `HookReport::echo`) is old news.
const ECHO_AFTER: Duration = Duration::from_secs(10);

use super::program::{self, HookReport, Osc, Report};
use std::time::{Duration, Instant};

/// What showed that the CLI is waiting, for `amux explain`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The CLI's own OSC 7501 report.
    Report,
    /// A report from a state hook the CLI ran (`amux __hook`).
    Hook,
    /// Text matching the provider's `waitingPatterns`.
    Pattern,
    Bell,
    /// An OSC 9, 777 or 99 notification.
    Notification,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Report => "report",
            Source::Hook => "hook",
            Source::Pattern => "pattern",
            Source::Bell => "bell",
            Source::Notification => "notification",
        }
    }
}

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
    /// Usage-limit messages, lowercased (see `take_limit`).
    limits: Vec<String>,
    text: Vec<u8>,
    osc: Vec<u8>,
    mode: Mode,
    waiting: bool,
    /// What showed the current wait.
    source: Source,
    /// The OSC being read grew past MAX_OSC.
    osc_overflow: bool,
    /// The CLI has sent an OSC 7501 report, so its reports decide the state.
    reports: bool,
    /// A state hook has reported (and no OSC 7501 yet): hook reports decide the state.
    hooked: bool,
    /// The tool call a hook-reported block is about (see `HookReport::tool`).
    blocked_tool: Option<String>,
    /// When a hook-reported block was last answered.
    answered_at: Option<Instant>,
    /// Its current root record.
    record: Option<Report>,
    /// `waiting` came from `waitingPatterns` while the CLI reported idle or done (a
    /// dialog of its own it doesn't report, like a confirmation), not from a report.
    pattern_wait: bool,
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
            limits: vec![],
            text: Vec::with_capacity(WINDOW * 2),
            osc: vec![],
            mode: Mode::Ground,
            waiting: false,
            source: Source::Pattern,
            osc_overflow: false,
            reports: false,
            hooked: false,
            blocked_tool: None,
            answered_at: None,
            record: None,
            pattern_wait: false,
            record_changed: false,
            probed: false,
            since_signal: 0,
        }
    }

    /// Also watches for these usage-limit messages.
    pub fn with_limits(mut self, limits: &[String]) -> Self {
        self.limits = limits
            .iter()
            .filter(|p| !p.is_empty())
            .map(|p| p.to_lowercase())
            .collect();
        self
    }

    /// The usage-limit message the CLI is showing, from where it starts (with the reset
    /// time that usually follows), once: the text it was found in is dropped, so the
    /// same message on screen isn't reported again.
    pub fn take_limit(&mut self) -> Option<String> {
        if self.limits.is_empty() {
            return None;
        }
        let start = self.text.len().saturating_sub(WINDOW);
        let text = String::from_utf8_lossy(&self.text[start..]).into_owned();
        let lower = text.to_lowercase();
        let at = self
            .limits
            .iter()
            .filter_map(|p| lower.find(p.as_str()))
            .min()?;
        // Byte offsets from the lowercased copy only line up for ASCII; fall back to the
        // start of the window otherwise.
        let from = if lower.len() == text.len() && text.is_char_boundary(at) {
            at
        } else {
            0
        };
        let msg: String = text[from..].chars().take(140).collect();
        self.text.clear();
        Some(msg.split("  ").next().unwrap_or(&msg).trim().to_string())
    }

    #[cfg(test)]
    pub fn waiting(&self) -> bool {
        self.waiting
    }

    /// You answered (see `Keys`), or auto-approve did: whatever the CLI was waiting for
    /// has been answered.
    pub fn answered(&mut self) -> Option<Change> {
        // A hook-reported block: no hook says when it ends (an answered permission prompt
        // reports nothing until the tool has run), so your answer does, and the CLI goes on.
        if !self.reports && self.hooked && self.waiting && !self.pattern_wait {
            self.waiting = false;
            self.blocked_tool = None;
            self.answered_at = Some(Instant::now());
            self.set_record(Some(Report {
                state: "working".into(),
                kind: None,
                message: None,
            }));
            return Some(Change::Cleared);
        }
        // The CLI says itself when it stops being blocked.
        if self.reports || self.hooked {
            // A reported block ends when the CLI says so; a matched dialog when you answer.
            if !(self.waiting && self.pattern_wait) {
                return None;
            }
            self.pattern_wait = false;
        }
        self.text.clear();
        self.since_signal = 0;
        std::mem::take(&mut self.waiting).then_some(Change::Cleared)
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Option<Change> {
        let mut signal: Option<String> = None;
        let mut src = Source::Pattern;
        for &b in chunk {
            match self.mode {
                Mode::Ground => match b {
                    0x1b => self.mode = Mode::Esc,
                    0x07 => {
                        signal = Some("terminal bell".into());
                        src = Source::Bell;
                    }
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
                        if let Some(m) = self.osc_end() {
                            (signal, src) = (Some(m), Source::Notification);
                        }
                        self.mode = Mode::Ground;
                    }
                    0x1b => self.mode = Mode::OscEsc,
                    _ if self.osc.len() < MAX_OSC => self.osc.push(b),
                    _ => self.osc_overflow = true,
                },
                Mode::OscEsc => {
                    if b == b'\\' {
                        if let Some(m) = self.osc_end() {
                            (signal, src) = (Some(m), Source::Notification);
                        }
                    }
                    self.mode = Mode::Ground;
                }
            }
        }
        if self.reports || self.hooked {
            return self.reported(chunk.len());
        }
        let signal = match signal {
            Some(s) => Some((s, src)),
            None => self.matched().map(|p| (p, Source::Pattern)),
        };
        if let Some((reason, src)) = signal {
            self.since_signal = 0;
            if !self.waiting {
                self.waiting = true;
                self.source = src;
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

    /// What showed the current (or last) wait.
    pub fn source(&self) -> Source {
        self.source
    }

    /// Whether the CLI sent the OSC 7501 probe since the last call: the host answers it.
    pub fn take_probe(&mut self) -> bool {
        std::mem::take(&mut self.probed)
    }

    /// The CLI's new root record, if it changed since the last call (None: cleared).
    pub fn take_record(&mut self) -> Option<Option<Report>> {
        std::mem::take(&mut self.record_changed).then(|| self.record.clone())
    }

    /// A state hook reported the CLI's state. Ignored once the CLI reports with OSC 7501,
    /// which is exact. While blocked, a second block keeps the first message: Claude Code's
    /// permission notification follows its PermissionRequest hook, which says more.
    ///
    /// A block stays until a report not about another tool call: a parallel tool call
    /// starting or finishing while the CLI asks about one doesn't end it. A repeat of a
    /// block that was just answered (`echo`) is ignored.
    pub fn hook(&mut self, h: HookReport) -> Option<Change> {
        if self.reports {
            return None;
        }
        self.hooked = true;
        let HookReport {
            report: mut r,
            tool,
            echo,
        } = h;
        let blocked = self.record.as_ref().is_some_and(|o| o.state == "blocked");
        if blocked {
            if r.state == "working" && tool.is_some() && tool != self.blocked_tool {
                return None;
            }
        } else if r.state == "blocked"
            && echo
            && self.answered_at.is_some_and(|t| t.elapsed() < ECHO_AFTER)
        {
            return None;
        }
        if r.state == "blocked" {
            let old = self.record.as_ref().filter(|_| blocked);
            if let Some(old) = old.filter(|o| o.message.is_some()) {
                r.message.clone_from(&old.message);
                r.kind.clone_from(&old.kind);
            }
            if !blocked || self.blocked_tool.is_none() {
                self.blocked_tool = tool;
            }
        } else {
            self.blocked_tool = None;
        }
        self.set_record(Some(r));
        self.reported(0)
    }

    /// The kind of the block the CLI reported, if it is blocked.
    pub fn blocked_kind(&self) -> Option<&str> {
        self.record
            .as_ref()
            .filter(|r| r.state == "blocked")
            .and_then(|r| r.kind.as_deref())
    }

    /// Whether the state comes from state hooks only (no OSC 7501 report yet).
    pub fn hooks_only(&self) -> bool {
        self.hooked && !self.reports
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
            let state = |r: &Option<Report>| r.as_ref().map(|r| r.state.clone());
            if state(&self.record) != state(&r) {
                // Prompt text from before this report is old news.
                self.text.clear();
            }
            self.record = r;
            self.record_changed = true;
        }
    }

    /// Whether the CLI is waiting, once it reports its state. A reported block decides.
    /// While it reports idle or done, `waitingPatterns` still catch dialogs it doesn't
    /// report (Claude Code's confirmations after a turn); the next working or blocked
    /// report, or your answer, ends those. Bells and OSC 9 no longer count.
    fn reported(&mut self, len: usize) -> Option<Change> {
        let state = self.record.as_ref().map(|r| r.state.as_str());
        if state == Some("blocked") {
            let reason = self.record.as_ref().map(Report::reason).unwrap_or_default();
            self.pattern_wait = false;
            self.source = if self.reports {
                Source::Report
            } else {
                Source::Hook
            };
            return (!std::mem::replace(&mut self.waiting, true))
                .then_some(Change::Waiting(reason));
        }
        let quiet = matches!(state, None | Some("idle" | "done"));
        if self.waiting && (!self.pattern_wait || !quiet) {
            // A reported block ended, or the CLI went back to work past a dialog.
            self.waiting = false;
            self.pattern_wait = false;
            return Some(Change::Cleared);
        }
        if !quiet {
            return None;
        }
        match self.matched() {
            Some(p) => {
                self.since_signal = 0;
                self.pattern_wait = true;
                self.source = Source::Pattern;
                (!std::mem::replace(&mut self.waiting, true)).then_some(Change::Waiting(p))
            }
            None if self.waiting => {
                // The dialog has scrolled out of view without a report: as without reports.
                self.since_signal += len;
                (self.since_signal >= CLEAR_AFTER).then(|| {
                    self.waiting = false;
                    self.pattern_wait = false;
                    self.text.clear();
                    Change::Cleared
                })
            }
            None => None,
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

/// Reads what you type into a run, to tell an answer from keys that only look around.
/// A waiting run stays waiting while you attach, move through a menu or switch windows;
/// Enter, a choice (a digit or letter), Esc or Ctrl-C answer it. Escape sequences (arrow
/// keys, focus reports, mouse reports) aren't answers, and neither is pasted text until
/// you press Enter. Remembers a paste that goes on into the next read.
#[derive(Default)]
pub struct Keys {
    paste: bool,
}

impl Keys {
    pub fn answers(&mut self, input: &[u8]) -> bool {
        let mut answer = false;
        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            if b == 0x1b {
                match input.get(i + 1) {
                    Some(b'[') => {
                        // CSI: parameters, then a final byte.
                        let start = i + 2;
                        let mut j = start;
                        while j < input.len() && !(0x40..=0x7e).contains(&input[j]) {
                            j += 1;
                        }
                        if input.get(j) == Some(&b'~') {
                            match &input[start..j] {
                                b"200" => self.paste = true,
                                b"201" => self.paste = false,
                                _ => {}
                            }
                        }
                        i = j + 1;
                    }
                    // SS3: arrow and function keys in application mode.
                    Some(b'O') => i += 3,
                    // Esc on its own (or twice) cancels; Esc and a key is Alt and that key.
                    _ => {
                        answer |= !self.paste;
                        i += 2;
                    }
                }
                continue;
            }
            if !self.paste {
                answer |= match b {
                    b'\r' | b'\n' | 0x03 | 0x04 => true,
                    0x00..=0x1f | 0x7f => false,
                    _ => true,
                };
            }
            i += 1;
        }
        answer
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
    fn records_what_showed_the_wait() {
        let mut d = det();
        d.feed(b"Do you want to go?");
        assert_eq!(d.source(), Source::Pattern);
        d.answered();
        d.feed(b"\x07");
        assert_eq!(d.source(), Source::Bell);
        d.answered();
        d.feed(b"\x1b]9;hey\x07");
        assert_eq!(d.source(), Source::Notification);
        d.answered();
        d.feed(b"\x1b]7501;state=blocked\x07");
        assert_eq!(d.source(), Source::Report);
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
    fn catches_unreported_dialogs_while_the_cli_reports_idle_or_done() {
        let mut d = det();
        d.feed(b"\x1b]7501;state=working\x07");
        // While working, prompt text doesn't count, and it is forgotten once the turn ends.
        assert_eq!(d.feed(b"Enter to confirm"), None);
        assert_eq!(d.feed(b"\x1b]7501;state=done\x07"), None);
        // A dialog after the turn, which Claude Code doesn't report: waiting.
        assert_eq!(
            d.feed(b"Teach auto mode? \x1b[1mEnter to confirm\x1b[0m"),
            Some(Change::Waiting("Enter to confirm".into()))
        );
        assert_eq!(d.feed(b" (still on screen)"), None);
        // Answering it clears it, though the CLI stays done.
        assert_eq!(d.answered(), Some(Change::Cleared));
        assert_eq!(d.feed(b"redraw"), None);
        // Shown again, then the CLI goes back to work: that clears it too.
        assert_eq!(
            d.feed(b"Enter to confirm"),
            Some(Change::Waiting("Enter to confirm".into()))
        );
        assert_eq!(
            d.feed(b"\x1b]7501;state=working\x07"),
            Some(Change::Cleared)
        );
        // Bells and OSC 9 stay off once the CLI reports, even while idle.
        d.feed(b"\x1b]7501;state=idle\x07");
        assert_eq!(d.feed(b"\x07\x1b]9;hey\x07"), None);
        // A reported block ends with the next report, whatever the screen says.
        assert_eq!(
            d.feed(b"\x1b]7501;state=blocked:kind=permission\x07"),
            Some(Change::Waiting("permission request".into()))
        );
        assert_eq!(d.answered(), None);
        assert_eq!(d.feed(b"\x1b]7501;state=idle\x07"), Some(Change::Cleared));
    }

    fn rep(state: &str, msg: Option<&str>) -> HookReport {
        HookReport {
            report: Report {
                state: state.into(),
                kind: (state == "blocked").then(|| "permission".into()),
                message: msg.map(Into::into),
            },
            tool: None,
            echo: false,
        }
    }

    fn tool(mut h: HookReport, t: &str) -> HookReport {
        h.tool = Some(t.into());
        h
    }

    #[test]
    fn other_tool_calls_dont_end_a_hook_block() {
        let mut d = det();
        d.hook(tool(rep("working", None), "Bash:1"));
        assert_eq!(
            d.hook(tool(rep("blocked", Some("approve Bash: rm")), "Bash:2")),
            Some(Change::Waiting("approve Bash: rm".into()))
        );
        assert_eq!(d.blocked_kind(), Some("permission"));
        // A parallel call starting and finishing while the dialog is open.
        assert_eq!(d.hook(tool(rep("working", None), "Read:3")), None);
        assert_eq!(d.hook(tool(rep("working", None), "Bash:1")), None);
        assert!(d.waiting());
        // The notification that follows keeps the block and its tool.
        assert_eq!(d.hook(rep("blocked", Some("Claude needs you"))), None);
        // The asked-about call finishing ends it (answered elsewhere).
        assert_eq!(
            d.hook(tool(rep("working", None), "Bash:2")),
            Some(Change::Cleared)
        );
        // A report about no tool call (a new prompt, the turn's end) ends one too.
        d.hook(tool(rep("blocked", None), "Bash:4"));
        assert_eq!(d.hook(rep("idle", None)), Some(Change::Cleared));
        assert_eq!(d.blocked_kind(), None);
    }

    #[test]
    fn ignores_a_repeat_of_a_block_just_answered() {
        let mut d = det();
        d.hook(tool(rep("blocked", Some("approve Bash: ls")), "Bash:1"));
        assert_eq!(d.answered(), Some(Change::Cleared));
        // Claude Code's permission notification, after the prompt was answered.
        let mut echo = rep("blocked", Some("Claude needs your permission"));
        echo.echo = true;
        assert_eq!(d.hook(echo.clone()), None);
        assert_eq!(d.take_record().unwrap().unwrap().state, "working");
        assert!(!d.waiting());
        // A new block is not a repeat.
        assert_eq!(
            d.hook(rep("blocked", Some("approve Edit"))),
            Some(Change::Waiting("approve Edit".into()))
        );
        // With nothing answered lately, a repeat counts (its hook may have been the only one).
        let mut d = det();
        assert_eq!(
            d.hook(echo),
            Some(Change::Waiting("Claude needs your permission".into()))
        );
    }

    #[test]
    fn only_answers_end_a_wait() {
        let mut k = Keys::default();
        // Focus in and out, arrows (normal and application mode), mouse, Tab, Backspace.
        for keys in [
            &b"\x1b[I"[..],
            b"\x1b[O",
            b"\x1b[A\x1b[B",
            b"\x1bOA",
            b"\x1b[<0;10;5M",
            b"\t\x7f",
            b"",
        ] {
            assert!(!k.answers(keys), "{keys:?}");
        }
        // Enter, a choice, Esc, Ctrl-C.
        for keys in [
            &b"\r"[..],
            b"1",
            b"y",
            b"\x1b",
            b"\x1b\x1b",
            b"\x03",
            b"\x1b[B\r",
        ] {
            assert!(k.answers(keys), "{keys:?}");
        }
        // Pasted text isn't an answer, even across reads; Enter after it is.
        assert!(!k.answers(b"\x1b[200~some"));
        assert!(!k.answers(b" text\r\x1b[201~"));
        assert!(k.answers(b"\r"));
    }

    #[test]
    fn hook_reports_decide_until_the_cli_reports_itself() {
        let mut d = det();
        assert_eq!(d.hook(rep("working", None)), None);
        assert!(d.hooks_only());
        assert_eq!(d.take_record().unwrap().unwrap().state, "working");
        // Like OSC 7501: prompt text and bells don't count while it reports working.
        assert_eq!(d.feed(b"Do you want to\x07"), None);
        assert_eq!(
            d.hook(rep("blocked", Some("approve Bash: cargo test"))),
            Some(Change::Waiting("approve Bash: cargo test".into()))
        );
        assert_eq!(d.source(), Source::Hook);
        // A second block (the notification) keeps the better message.
        assert_eq!(
            d.hook(rep("blocked", Some("Claude needs your permission"))),
            None
        );
        d.take_record();
        assert_eq!(
            d.record.as_ref().unwrap().message.as_deref(),
            Some("approve Bash: cargo test")
        );
        // Output doesn't end it; your answer does, and the CLI goes back to work.
        assert_eq!(d.feed(&[b'x'; CLEAR_AFTER * 2]), None);
        assert_eq!(d.answered(), Some(Change::Cleared));
        assert_eq!(d.take_record().unwrap().unwrap().state, "working");
        assert_eq!(d.feed(b"more"), None);
        // Idle: patterns count again, as with OSC 7501.
        assert_eq!(d.hook(rep("idle", None)), None);
        assert_eq!(
            d.feed(b"Enter to confirm"),
            Some(Change::Waiting("Enter to confirm".into()))
        );
        assert_eq!(d.source(), Source::Pattern);
        d.answered();
        // The first OSC 7501 report takes over, and hooks are ignored from then on.
        d.feed(b"\x1b]7501;state=working\x07");
        assert!(!d.hooks_only());
        assert_eq!(d.hook(rep("blocked", Some("x"))), None);
        assert_eq!(d.take_record().unwrap().unwrap().state, "working");
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
