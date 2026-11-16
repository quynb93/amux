//! OSC 7501, the Program Status Protocol (Rex, Rev 0.4): a CLI reports its own state in
//! its output, as `ESC ] 7501 ; state=blocked:kind=permission:msg=<base64> ST`.
//! Codex sends it on any terminal. Claude Code (2.1.295 and later) sends it once the
//! terminal answers the probe `ESC ] 7501 ; ? ST`, which the run host does.
//!
//! Only the root record (no `id`) is tracked: it is the CLI itself. Child records
//! (subagents) are ignored.

/// The run host's answer to the probe: the states and kinds it understands.
pub const PROBE_REPLY: &[u8] =
    b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

/// What a CLI last said about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// idle, working, blocked, done or error.
    pub state: String,
    /// For blocked: permission, question or auth.
    pub kind: Option<String>,
    /// What the CLI is doing or asking, decoded from `msg`.
    pub message: Option<String>,
}

impl Report {
    /// Why the CLI is blocked, for `Run.waiting` and notifications.
    pub fn reason(&self) -> String {
        if let Some(m) = self.message.as_deref().filter(|m| !m.is_empty()) {
            return m.to_string();
        }
        match self.kind.as_deref() {
            Some("permission") => "permission request".into(),
            Some("question") => "a question".into(),
            Some("auth") => "sign-in".into(),
            _ => "blocked".into(),
        }
    }

    /// Records that end when the CLI does (spec: working and blocked are dropped; done and
    /// error survive).
    pub fn ends_with_process(&self) -> bool {
        !matches!(self.state.as_str(), "done" | "error")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Osc {
    /// `7501;?`: the CLI asks whether the terminal speaks the protocol.
    Probe,
    /// A new root record.
    Report(Report),
    /// `state=clear` for the root (or everything): forget the record.
    Clear,
    /// `133;A`, a new shell prompt: working and blocked records end.
    Prompt,
    /// A 7501 record we ignore (a child record, or malformed and discarded whole).
    Ignored,
}

const STATES: [&str; 6] = ["idle", "working", "done", "blocked", "error", "clear"];
const MAX_MSG: usize = 2048;

/// Interprets an OSC body (between `ESC ]` and the terminator). None if it isn't one of
/// the sequences this protocol cares about.
pub fn parse(osc: &[u8]) -> Option<Osc> {
    if osc.starts_with(b"133;A") {
        return Some(Osc::Prompt);
    }
    let body = osc.strip_prefix(b"7501;")?;
    if body == b"?" {
        return Some(Osc::Probe);
    }
    let body = std::str::from_utf8(body).ok()?;
    let (mut state, mut kind, mut msg, mut id) = (None, None, None, None);
    for pair in body.split(':') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        // Values are restricted to this alphabet; anything else discards the report.
        let ok = v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b));
        if !ok {
            return Some(Osc::Ignored);
        }
        // The last duplicate wins; unknown keys are ignored.
        match k {
            "state" => state = Some(v),
            "kind" => kind = Some(v),
            "msg" => msg = Some(v),
            "id" => id = Some(v),
            _ => {}
        }
    }
    let Some(state) = state.filter(|s| STATES.contains(s)) else {
        return Some(Osc::Ignored);
    };
    // A child record (a subagent): only the CLI itself, the root, is tracked.
    if id.is_some_and(|i| !i.is_empty()) {
        return Some(Osc::Ignored);
    }
    if state == "clear" {
        return Some(Osc::Clear);
    }
    let message = match msg {
        Some(m) => match decode_text(m) {
            Some(t) => Some(t),
            None => return Some(Osc::Ignored),
        },
        None => None,
    };
    Some(Osc::Report(Report {
        state: state.to_string(),
        kind: kind
            .filter(|_| state == "blocked")
            .filter(|k| matches!(*k, "permission" | "question" | "auth"))
            .map(str::to_string),
        message,
    }))
}

/// Base64 to UTF-8 text without control characters, as the spec requires of `msg`.
fn decode_text(b64: &str) -> Option<String> {
    let bytes = base64_decode(b64)?;
    if bytes.len() > MAX_MSG {
        return None;
    }
    let s = String::from_utf8(bytes).ok()?;
    if s.chars().any(char::is_control) {
        return None;
    }
    Some(s.trim().to_string())
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(state: &str, kind: Option<&str>, msg: Option<&str>) -> Osc {
        Osc::Report(Report {
            state: state.into(),
            kind: kind.map(Into::into),
            message: msg.map(Into::into),
        })
    }

    #[test]
    fn decodes_base64() {
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode("aGk").unwrap(), b"hi");
        assert_eq!(base64_decode("YWJj").unwrap(), b"abc");
        assert_eq!(base64_decode("").unwrap(), b"");
        assert!(base64_decode("a").is_none());
        assert!(base64_decode("a*bc").is_none());
    }

    #[test]
    fn parses_reports() {
        assert_eq!(parse(b"7501;?"), Some(Osc::Probe));
        assert_eq!(
            parse(b"7501;state=working:app=codex"),
            Some(report("working", None, None))
        );
        // "approve Write: a.rs"
        assert_eq!(
            parse(b"7501;state=blocked:kind=permission:msg=YXBwcm92ZSBXcml0ZTogYS5ycw=="),
            Some(report(
                "blocked",
                Some("permission"),
                Some("approve Write: a.rs")
            ))
        );
        // kind only counts with blocked; the last duplicate wins; unknown keys are fine.
        assert_eq!(
            parse(b"7501;state=idle:kind=permission:state=done:future=1"),
            Some(report("done", None, None))
        );
        assert_eq!(parse(b"7501;state=clear"), Some(Osc::Clear));
        assert_eq!(parse(b"133;A"), Some(Osc::Prompt));
    }

    #[test]
    fn ignores_children_and_malformed_reports() {
        assert_eq!(parse(b"7501;state=working:id=sub/1"), Some(Osc::Ignored));
        assert_eq!(parse(b"7501;state=sleeping"), Some(Osc::Ignored));
        assert_eq!(parse(b"7501;kind=permission"), Some(Osc::Ignored));
        // A value outside the alphabet, or a msg with control characters, discards it.
        assert_eq!(parse(b"7501;state=working:msg=a b"), Some(Osc::Ignored));
        assert_eq!(parse(b"7501;state=working:msg=Gw=="), Some(Osc::Ignored));
        // Other OSCs aren't ours.
        assert_eq!(parse(b"0;title"), None);
        assert_eq!(parse(b"9;hello"), None);
    }

    #[test]
    fn explains_why_it_is_blocked() {
        let r = |kind: Option<&str>, msg: Option<&str>| Report {
            state: "blocked".into(),
            kind: kind.map(Into::into),
            message: msg.map(Into::into),
        };
        assert_eq!(
            r(Some("permission"), Some("approve Bash")).reason(),
            "approve Bash"
        );
        assert_eq!(r(Some("permission"), None).reason(), "permission request");
        assert_eq!(r(Some("question"), Some("")).reason(), "a question");
        assert_eq!(r(None, None).reason(), "blocked");
        assert!(r(None, None).ends_with_process());
    }
}
