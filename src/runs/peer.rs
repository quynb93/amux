//! Who is on the other end of a run's socket, and what it may do there.
//!
//! Any process of yours can connect to a run's socket. A process an agent started (a
//! descendant of some run's CLI) is held to its team: it may type into (INPUT, RESIZE)
//! only the runs whose lead is its own run, so a lead drives its workers with `amux send`
//! but no agent drives its own prompt or another team's. State reports (STATE) are taken
//! only from the run's own CLI and what it started. Everything else (you, `amux ui`, the
//! scheduler, `amux __pipe` for another machine) is unrestricted.
//!
//! The host checks this on its socket (`allows`); `amux send` checks it first too
//! (`check_typing`), so the agent gets a clear error instead of a closed connection.

use super::{list, Run, State};
use anyhow::{bail, Result};
use std::os::unix::net::UnixStream;

/// Where a connecting process comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum From {
    /// Not started by any live run's CLI.
    User,
    /// A descendant of this run's CLI (the nearest one up its ancestry).
    Run(String),
    /// Its process couldn't be identified: allowed nothing that is restricted.
    Unknown,
}

/// What a connection wants to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// INPUT or RESIZE: typing into the run.
    Type,
    /// STATE: reporting the run's state (state hooks).
    Report,
}

/// Whether `from` may `act` on the run `id` whose lead is `lead`.
pub fn allows(from: &From, act: Act, id: &str, lead: Option<&str>) -> bool {
    match (from, act) {
        (From::Unknown, _) => false,
        (From::User, Act::Type) => true,
        (From::User, Act::Report) => false,
        (From::Run(r), Act::Type) => lead == Some(r.as_str()),
        (From::Run(r), Act::Report) => r == id,
    }
}

/// The process at the other end of a Unix socket.
#[cfg(target_os = "macos")]
pub fn peer_pid(s: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    };
    (r == 0 && pid > 0).then_some(pid as u32)
}

#[cfg(target_os = "linux")]
pub fn peer_pid(s: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn peer_pid(_s: &UnixStream) -> Option<u32> {
    None
}

/// A connecting process's ancestry, read when it connects (it may be gone by the time
/// what it sent is read).
#[derive(Debug, Clone)]
pub struct Peer {
    /// The process, its parent, and so on up to (not including) init. None: unreadable.
    chain: Option<Vec<u32>>,
    session: Option<u32>,
}

impl Peer {
    pub fn of(s: &UnixStream) -> Peer {
        match peer_pid(s) {
            Some(pid) => Peer::of_pid(pid),
            // No way to ask on this system: as before, everyone is you.
            None if cfg!(not(any(target_os = "linux", target_os = "macos"))) => Peer {
                chain: Some(vec![]),
                session: None,
            },
            None => Peer {
                chain: None,
                session: None,
            },
        }
    }

    pub fn of_pid(pid: u32) -> Peer {
        Peer {
            chain: ancestry(pid, parent_of),
            session: session_of(pid),
        }
    }

    /// Where it comes from, among the runs live now.
    pub fn from(&self) -> From {
        let clis: Vec<(String, u32)> = list()
            .into_iter()
            .filter(|r| r.state() == State::Running)
            .filter_map(|r| Some((r.id, r.pid?)))
            .collect();
        self.nearest_run(&clis)
    }

    /// The nearest run whose CLI is the process or one of its ancestors. A process that
    /// left the tree (its parent ended, or it was reparented) still has the CLI's session,
    /// since the CLI is a session leader: that is the fallback.
    fn nearest_run(&self, clis: &[(String, u32)]) -> From {
        let run_of = |p: u32| clis.iter().find(|(_, c)| *c == p).map(|(id, _)| id.clone());
        let Some(chain) = &self.chain else {
            return From::Unknown;
        };
        if let Some(id) = chain.iter().find_map(|p| run_of(*p)) {
            return From::Run(id);
        }
        match self.session.and_then(run_of) {
            Some(id) => From::Run(id),
            None => From::User,
        }
    }
}

/// A connection's peer and, once something restricted comes, where it comes from.
pub struct Gate {
    peer: Peer,
    from: std::sync::OnceLock<From>,
}

impl Gate {
    pub fn new(s: &UnixStream) -> Gate {
        Gate {
            peer: Peer::of(s),
            from: std::sync::OnceLock::new(),
        }
    }

    pub fn allows(&self, act: Act, id: &str, lead: Option<&str>) -> bool {
        allows(self.from.get_or_init(|| self.peer.from()), act, id, lead)
    }
}

/// `pid` and its ancestors below init. None when its parent can't be read (the process
/// is gone, or the system can't say). Further up, a process that can't be read (another
/// user's, such as a login shell's parent) ends the walk: a run's CLI is yours.
fn ancestry(pid: u32, parent: impl Fn(u32) -> Option<u32>) -> Option<Vec<u32>> {
    let mut chain = vec![pid];
    let mut p = pid;
    for _ in 0..64 {
        let up = match parent(p) {
            Some(pp) => pp,
            None if p == pid => return None,
            None => break,
        };
        match up {
            pp if pp > 1 && !chain.contains(&pp) => {
                chain.push(pp);
                p = pp;
            }
            _ => break,
        }
    }
    Some(chain)
}

/// `amux send` and friends, before connecting: refuses what the run's host would refuse,
/// with a message that says why. Remote runs are checked by the host over there.
pub fn check_typing(run: &Run) -> Result<()> {
    if run.id.contains(':') {
        return Ok(());
    }
    let from = Peer::of_pid(std::process::id()).from();
    if allows(&from, Act::Type, &run.id, run.lead.as_deref()) {
        return Ok(());
    }
    match from {
        From::Run(r) if r == run.id => {
            bail!("An agent can't type into its own run ({r}). Only you, or its team's lead, can.")
        }
        From::Run(r) => bail!(
            "{r} can't type into {}: from inside a run, amux sends only to that run's own workers.",
            run.id
        ),
        _ => bail!(
            "Could not tell which process this is, so {} refuses it.",
            run.id
        ),
    }
}

/// A process's parent. None: it couldn't be read.
#[cfg(target_os = "linux")]
pub fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid ...": comm may hold spaces and parentheses.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "macos")]
pub fn parent_of(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then_some(info.pbi_ppid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn parent_of(_pid: u32) -> Option<u32> {
    None
}

fn session_of(pid: u32) -> Option<u32> {
    let s = unsafe { libc::getsid(pid as libc::pid_t) };
    (s > 0).then_some(s as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_type_only_into_their_own_workers() {
        let lead = From::Run("lead".into());
        assert!(allows(&lead, Act::Type, "w1", Some("lead")));
        assert!(!allows(&lead, Act::Type, "lead", None), "not its own run");
        assert!(
            !allows(&lead, Act::Type, "other", Some("x")),
            "not another team"
        );
        assert!(!allows(&lead, Act::Type, "solo", None));
        assert!(allows(&From::User, Act::Type, "solo", None));
        assert!(!allows(&From::Unknown, Act::Type, "solo", None));
    }

    #[test]
    fn state_reports_only_from_the_runs_own_cli() {
        assert!(allows(&From::Run("a".into()), Act::Report, "a", None));
        assert!(!allows(&From::Run("b".into()), Act::Report, "a", None));
        assert!(!allows(
            &From::Run("lead".into()),
            Act::Report,
            "w",
            Some("lead")
        ));
        assert!(!allows(&From::User, Act::Report, "a", None));
        assert!(!allows(&From::Unknown, Act::Report, "a", None));
    }

    #[test]
    fn finds_the_nearest_run_up_the_process_tree() {
        // 1 ← 10 (lead's CLI) ← 11 (shell) ← 12 (worker's CLI) ← 13 (hook)
        //   ← 20 (your shell) ← 21 (amux send)
        let parent = |p: u32| match p {
            10 | 20 => Some(1),
            11 => Some(10),
            12 => Some(11),
            13 => Some(12),
            21 => Some(20),
            30 => Some(1),
            _ => None,
        };
        let session = |p: u32| Some(if p == 30 { 10 } else { p });
        let clis = [("lead".to_string(), 10), ("w".to_string(), 12)];
        let near = |pid| {
            Peer {
                chain: ancestry(pid, parent),
                session: session(pid),
            }
            .nearest_run(&clis)
        };
        assert_eq!(near(11), From::Run("lead".into()));
        assert_eq!(near(13), From::Run("w".into()));
        assert_eq!(near(12), From::Run("w".into()));
        assert_eq!(near(21), From::User);
        // Detached from the lead's tree, but still in its CLI's session.
        assert_eq!(near(30), From::Run("lead".into()));
        // Its parent can't be read (it's gone): unknown, not you.
        assert_eq!(near(99), From::Unknown);
        let unreadable = Peer {
            chain: None,
            session: None,
        };
        assert_eq!(unreadable.nearest_run(&clis), From::Unknown);
    }

    #[test]
    fn reads_the_peer_of_a_socket() {
        let (a, _b) = UnixStream::pair().unwrap();
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert_eq!(peer_pid(&a), Some(std::process::id()));
            assert_eq!(
                parent_of(std::process::id()),
                Some(std::os::unix::process::parent_id())
            );
        }
    }
}
