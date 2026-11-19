//! A run's processes: its CLI and what the CLI started, and the files they hold open.
//! Used to tell which session file belongs to a run when several runs share a session
//! folder (Codex keeps its rollout file open while the session lives). Reading only:
//! nothing here signals or changes a process.

use std::path::{Path, PathBuf};

/// How far below the CLI to look: a wrapper script, then `node`, then the native binary.
const DEPTH: usize = 4;

/// The files under `root` that `pid` or one of its descendants holds open and `pick`
/// takes (with what `pick` made of them), from the nearest level that holds any: the CLI's
/// own files come before those of a CLI the agent started inside the run. A file `pick`
/// leaves out doesn't stop the search at its level. Empty when there are none, or the
/// system can't say.
pub fn held_under<T>(
    pid: u32,
    root: &Path,
    pick: impl Fn(&Path) -> Option<T>,
) -> Vec<(PathBuf, T)> {
    let Ok(root) = root.canonicalize() else {
        return vec![];
    };
    let mut level = vec![pid];
    let mut seen = level.clone();
    for _ in 0..DEPTH {
        let mut files: Vec<PathBuf> = level
            .iter()
            .flat_map(|p| open_files(*p))
            .filter(|f| f.starts_with(&root))
            .collect();
        files.sort();
        files.dedup();
        let found: Vec<(PathBuf, T)> = files
            .into_iter()
            .filter_map(|f| pick(&f).map(|t| (f, t)))
            .collect();
        if !found.is_empty() {
            return found;
        }
        level = children(&level)
            .into_iter()
            .filter(|c| !seen.contains(c))
            .collect();
        if level.is_empty() {
            break;
        }
        seen.extend(&level);
    }
    vec![]
}

#[cfg(target_os = "linux")]
fn open_files(pid: u32) -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return vec![];
    };
    dir.flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .filter(|p| p.is_absolute())
        .collect()
}

/// The children of any of `parents`, from one pass over /proc.
#[cfg(target_os = "linux")]
fn children(parents: &[u32]) -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|p| {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{p}/stat")) else {
                return false;
            };
            // "pid (comm) state ppid ...": comm may hold spaces and parentheses.
            stat.rfind(')')
                .and_then(|i| stat[i + 1..].split_whitespace().nth(1)?.parse::<u32>().ok())
                .is_some_and(|ppid| parents.contains(&ppid))
        })
        .collect()
}

/// `struct proc_fileinfo` and `struct vnode_fdinfowithpath` from <sys/proc_info.h>,
/// which libc doesn't have.
#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: i64,
    fi_type: i32,
    fi_guardflags: u32,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: libc::vnode_info_path,
}

#[cfg(target_os = "macos")]
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
#[cfg(target_os = "macos")]
const PROC_PPID_ONLY: u32 = 6;

#[cfg(target_os = "macos")]
fn open_files(pid: u32) -> Vec<PathBuf> {
    use std::mem::size_of;
    use std::os::unix::ffi::OsStrExt;
    let Ok(pid) = libc::c_int::try_from(pid) else {
        return vec![];
    };
    let want =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if want <= 0 {
        return vec![];
    }
    // Room for a few more opened meanwhile.
    let n = want as usize / size_of::<libc::proc_fdinfo>() + 16;
    let mut fds: Vec<libc::proc_fdinfo> = (0..n)
        .map(|_| libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        })
        .collect();
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (n * size_of::<libc::proc_fdinfo>()) as libc::c_int,
        )
    };
    if got <= 0 {
        return vec![];
    }
    fds.truncate(got as usize / size_of::<libc::proc_fdinfo>());
    let mut out = Vec::new();
    for fd in fds
        .iter()
        .filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
    {
        // SAFETY: plain C data; all zeroes is a valid value.
        let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
        let size = size_of::<VnodeFdInfoWithPath>() as libc::c_int;
        let r = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd.proc_fd,
                PROC_PIDFDVNODEPATHINFO,
                (&mut info as *mut VnodeFdInfoWithPath).cast(),
                size,
            )
        };
        if r != size {
            continue;
        }
        let bytes: Vec<u8> = info
            .pvip
            .vip_path
            .iter()
            .flatten()
            .map(|c| *c as u8)
            .take_while(|c| *c != 0)
            .collect();
        if bytes.first() == Some(&b'/') {
            out.push(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)));
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn children(parents: &[u32]) -> Vec<u32> {
    parents.iter().flat_map(|p| children_of(*p)).collect()
}

#[cfg(target_os = "macos")]
fn children_of(pid: u32) -> Vec<u32> {
    let mut buf = vec![0i32; 4096];
    let n = unsafe {
        libc::proc_listpids(
            PROC_PPID_ONLY,
            pid,
            buf.as_mut_ptr().cast(),
            (buf.len() * 4) as libc::c_int,
        )
    };
    if n <= 0 {
        return vec![];
    }
    buf.truncate(n as usize / 4);
    buf.into_iter()
        .filter(|p| *p > 0)
        .map(|p| p as u32)
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_files(_pid: u32) -> Vec<PathBuf> {
    vec![]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn children(_parents: &[u32]) -> Vec<u32> {
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_files_a_process_and_its_children_hold_open() {
        let t = crate::testutil::TempDir::new();
        let held = t.path().join("sessions/held.jsonl");
        crate::testutil::write(&held, "x\n");
        crate::testutil::write(&t.path().join("sessions/closed.jsonl"), "x\n");
        let _f = std::fs::File::open(&held).unwrap();
        let me = std::process::id();
        let all = |pid, dir: &Path| -> Vec<PathBuf> {
            held_under(pid, dir, |_| Some(()))
                .into_iter()
                .map(|(p, ())| p)
                .collect()
        };
        let found = all(me, &t.path().join("sessions"));
        assert_eq!(found, vec![held.canonicalize().unwrap()]);
        // Nothing open under another folder.
        crate::testutil::write(&t.path().join("other/x"), "");
        assert!(all(me, &t.path().join("other")).is_empty());
        // A child holding a file: found one level down. `sh` keeps its input open.
        let kid_file = t.path().join("kid/script.sh");
        crate::testutil::write(&kid_file, "sleep 5\n");
        // Ours, but not one `pick` takes: it doesn't hide the child's.
        let mine = t.path().join("kid/mine.txt");
        crate::testutil::write(&mine, "");
        let _mine = std::fs::File::open(&mine).unwrap();
        let script = |p: &Path| p.extension().is_some_and(|e| e == "sh").then_some(());
        let mut kid = std::process::Command::new("sh")
            .stdin(std::fs::File::open(&kid_file).unwrap())
            .spawn()
            .unwrap();
        let mut found = vec![];
        for _ in 0..100 {
            found = held_under(me, &t.path().join("kid"), script)
                .into_iter()
                .map(|(p, ())| p)
                .collect();
            if !found.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Our own process holds nothing there that `pick` takes, so the child's is found.
        assert_eq!(found, vec![kid_file.canonicalize().unwrap()]);
        let _ = kid.kill();
        let _ = kid.wait();
    }
}
