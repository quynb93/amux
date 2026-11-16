//! Background runs: a provider CLI started in its own pseudo-terminal and held by a small
//! `amux __run-host` process, so you can detach from it and attach again later.
//!
//! Each run lives in `~/.amux/runs/<id>/`:
//!   run.json     what was started, where, and how it ended
//!   output.log   everything the CLI printed (rotated to output.log.1 at 4 MB)
//!   sock         the host's Unix socket, present while the run is alive

pub mod attach;
pub mod detect;
pub mod host;
pub mod notify;
pub mod program;
pub mod screen;

use crate::store::{read_json, write_json};
use crate::{fsx, paths, time};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    /// `provider@name`.
    pub account: String,
    pub bin: String,
    pub args: Vec<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<Worktree>,
    pub started_at: i64,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cols: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_pid: Option<u32>,
    /// The CLI's pid, which is also its process group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why the CLI could not be started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Prompt text that means the CLI is waiting for you (from the provider spec).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_patterns: Vec<String>,
    /// Set by the host while the CLI is waiting for you.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Waiting>,
    /// What the CLI last reported about itself with OSC 7501, if it reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProgramStatus>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProgramStatus {
    /// idle, working, blocked, done or error.
    pub state: String,
    /// For blocked: permission, question or auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// What the CLI is doing or asking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// When it entered this state.
    pub since: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Waiting {
    /// The prompt text or notification message that showed it.
    pub reason: String,
    pub since: i64,
    /// What showed it: report (OSC 7501), pattern, bell or notification. Absent from
    /// runs started by amux 0.4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Worktree {
    /// The main checkout the worktree was added to.
    pub repo: String,
    pub path: String,
    pub branch: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Running,
    Exited(i32),
    Failed,
    /// The host is gone without recording an exit (killed, or the machine restarted).
    Lost,
}

impl Run {
    pub fn state(&self) -> State {
        if self.error.is_some() {
            return State::Failed;
        }
        if let Some(code) = self.exit_code {
            return State::Exited(code);
        }
        match self.host_pid {
            Some(pid) if alive(pid) => State::Running,
            None if time::now_ms() - self.started_at < 10_000 => State::Starting,
            _ => State::Lost,
        }
    }

    pub fn dir(&self) -> PathBuf {
        run_dir(&self.id)
    }

    pub fn log_files(&self) -> [PathBuf; 2] {
        let dir = self.dir();
        [dir.join("output.log.1"), dir.join("output.log")]
    }
}

pub fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // Signal 0 checks for existence; EPERM means it exists but belongs to someone else.
    unsafe {
        libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

pub fn run_dir(id: &str) -> PathBuf {
    paths::runs_dir().join(id)
}

pub fn load(id: &str) -> Result<Run> {
    read_json::<Option<Run>>(&run_dir(id).join("run.json"))?
        .ok_or_else(|| anyhow!("No run named \"{id}\"."))
}

pub fn save(run: &Run) -> Result<()> {
    write_json(&run.dir().join("run.json"), run)
}

/// All runs, oldest first. Folders without a readable run.json are skipped.
pub fn list() -> Vec<Run> {
    let Ok(rd) = std::fs::read_dir(paths::runs_dir()) else {
        return vec![];
    };
    let mut out: Vec<Run> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| load(&e.file_name().to_string_lossy()).ok())
        .collect();
    out.sort_by_key(|r| (r.started_at, r.id.clone()));
    out
}

/// A run by exact id or unique prefix.
pub fn find(prefix: &str) -> Result<Run> {
    let all = list();
    if let Some(r) = all.iter().find(|r| r.id == prefix) {
        return Ok(r.clone());
    }
    let mut m: Vec<Run> = all
        .into_iter()
        .filter(|r| r.id.starts_with(prefix))
        .collect();
    match m.len() {
        0 => bail!("No run matches \"{prefix}\". See `amux ps`."),
        1 => Ok(m.remove(0)),
        n => bail!("\"{prefix}\" matches {n} runs; use more characters."),
    }
}

/// Claims the run folder for `name`, or for `<base>-<n>` with the next free number.
/// Creating the folder is the claim, so two spawns at once never get the same id.
pub fn claim_id(name: Option<&str>, base: &str) -> Result<String> {
    let root = paths::runs_dir();
    fsx::mkdir_private(&root)?;
    let mkdir = |id: &str| -> io::Result<()> {
        let mut b = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(root.join(id))
    };
    if let Some(name) = name {
        return match mkdir(name) {
            Ok(()) => Ok(name.to_string()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                bail!("A run named \"{name}\" already exists. Pick another --name or `amux rm {name}`.")
            }
            Err(e) => Err(e.into()),
        };
    }
    let used = list()
        .iter()
        .filter_map(|r| r.id.strip_prefix(&format!("{base}-"))?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    for n in used + 1.. {
        let id = format!("{base}-{n}");
        match mkdir(&id) {
            Ok(()) => return Ok(id),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!()
}

// Client → host messages: a kind byte, a big-endian u32 length, then the payload.
// Host → client is the raw terminal output.
pub const INPUT: u8 = 0;
pub const RESIZE: u8 = 1;
/// Payload `[1]`: this client only watches (a preview), so the host still notifies you.
/// `[0]`: a person is attached again. Hosts before 0.5 ignore it.
pub const WATCH: u8 = 2;
const MAX_FRAME: usize = 1 << 20;

pub fn write_frame(w: &mut impl Write, kind: u8, data: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + data.len());
    buf.push(kind);
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf)
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut data = vec![0; len];
    r.read_exact(&mut data)?;
    Ok((head[0], data))
}

pub fn size_payload(rows: u16, cols: u16) -> [u8; 4] {
    let (r, c) = (rows.to_be_bytes(), cols.to_be_bytes());
    [r[0], r[1], c[0], c[1]]
}

pub fn parse_size(d: &[u8]) -> Option<(u16, u16)> {
    match d {
        [a, b, c, e] => Some((u16::from_be_bytes([*a, *b]), u16::from_be_bytes([*c, *e]))),
        _ => None,
    }
}

/// The terminal size of stdout (or stdin), if either is a terminal.
pub fn term_size() -> Option<(u16, u16)> {
    for fd in [1, 0] {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) } == 0
            && ws.ws_row > 0
            && ws.ws_col > 0
        {
            return Some((ws.ws_row, ws.ws_col));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = vec![];
        write_frame(&mut buf, INPUT, b"hello").unwrap();
        write_frame(&mut buf, RESIZE, &size_payload(40, 120)).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_frame(&mut r).unwrap(), (INPUT, b"hello".to_vec()));
        let (kind, d) = read_frame(&mut r).unwrap();
        assert_eq!((kind, parse_size(&d)), (RESIZE, Some((40, 120))));
        assert!(read_frame(&mut r).is_err());
    }

    #[test]
    fn rejects_oversized_frames() {
        let mut r: &[u8] = &[INPUT, 0xff, 0xff, 0xff, 0xff];
        assert!(read_frame(&mut r).is_err());
    }

    #[test]
    fn derives_state() {
        let mut r = Run {
            started_at: time::now_ms(),
            ..Default::default()
        };
        assert_eq!(r.state(), State::Starting);
        r.host_pid = Some(std::process::id());
        assert_eq!(r.state(), State::Running);
        r.exit_code = Some(3);
        assert_eq!(r.state(), State::Exited(3));
        r.error = Some("not found".into());
        assert_eq!(r.state(), State::Failed);
        let lost = Run {
            host_pid: Some(u32::MAX - 1),
            ..Default::default()
        };
        assert_eq!(lost.state(), State::Lost);
    }
}
