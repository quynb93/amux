//! `amux gc`: processes that finished runs left behind. A CLI that started a dev server,
//! a watcher or a test runner in the background can exit and leave it going, holding a
//! port, CPU and memory (and, if it was started through `amux slot`, a slot). A leftover
//! is a process still in the CLI's process group, or one that carries the run's AMUX_RUN
//! in its environment, which everything the CLI starts inherits, even when detached.

use crate::runs::{self, State};
use crate::util::table;
use anyhow::{bail, Result};
use std::process::{Command, Stdio};

/// Every process: (pid, process group, command line).
fn processes() -> Vec<(u32, u32, String)> {
    let Ok(out) = Command::new("ps")
        .args(["-A", "-o", "pid=,pgid=,command="])
        .stdin(Stdio::null())
        .output()
    else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let pgid = it.next()?.parse().ok()?;
            Some((pid, pgid, it.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

/// A process's environment, raw (NUL-separated `KEY=value`). Only for your own processes.
#[cfg(target_os = "linux")]
fn environ(pid: u32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ")).ok()
}

/// A process's environment, from the block the kernel keeps for it (`KERN_PROCARGS2`:
/// argc, the executable path, then the arguments and the environment).
#[cfg(target_os = "macos")]
fn environ(pid: u32) -> Option<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    let null = std::ptr::null_mut();
    unsafe {
        if libc::sysctl(mib.as_mut_ptr(), 3, null, &mut size, null, 0) != 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            null,
            0,
        ) != 0
        {
            return None;
        }
        buf.truncate(size);
        Some(buf)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn environ(_pid: u32) -> Option<Vec<u8>> {
    None
}

/// A process's working directory.
#[cfg(target_os = "linux")]
fn cwd_of(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
fn cwd_of(pid: u32) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // A C string spread over the array of arrays.
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(info.pvi_cdir.vip_path.as_ptr().cast(), 32 * 32) };
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
        &bytes[..end],
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cwd_of(_pid: u32) -> Option<std::path::PathBuf> {
    None
}

/// Whether the process was started under this run: it carries AMUX_RUN=<run> (readable
/// on Linux), or it works inside the run's own worktree, a folder amux made for the run
/// alone. Never by folder for a run without a worktree, where your own work lives.
fn belongs_to(pid: u32, run: &runs::Run) -> bool {
    if let Some(env) = environ(pid) {
        let want = format!("AMUX_RUN={}", run.id);
        if env.split(|b| *b == 0).any(|kv| kv == want.as_bytes()) {
            return true;
        }
    }
    let Some(wt) = &run.worktree else {
        return false;
    };
    let root = std::path::Path::new(&wt.path);
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    cwd_of(pid).is_some_and(|d| d.starts_with(&root))
}

pub fn gc_cmd(argv: &[String]) -> Result<i32> {
    let dry = argv.iter().any(|a| a == "--dry-run" || a == "-n");
    if let Some(a) = argv
        .iter()
        .find(|a| !matches!(a.as_str(), "--dry-run" | "-n"))
    {
        bail!("Unknown argument {a}. Usage: amux gc [--dry-run]");
    }
    let me = std::process::id();
    let procs = processes();
    let mut rows = vec![];
    let mut pids: Vec<u32> = vec![];
    for run in runs::list() {
        // Only runs that are over: a live run's processes are its own business.
        if matches!(
            run.state(),
            State::Running | State::Starting | State::Queued
        ) {
            continue;
        }
        let pgid = run.pid.unwrap_or(0);
        for (pid, g, cmd) in &procs {
            let ours = (pgid != 0 && *g == pgid) || belongs_to(*pid, &run);
            if *pid != me && ours && !pids.contains(pid) {
                rows.push(vec![
                    run.id.clone(),
                    pid.to_string(),
                    crate::util::one_line(cmd, 70),
                ]);
                pids.push(*pid);
            }
        }
    }
    if rows.is_empty() {
        println!("Nothing left behind by finished runs.");
        return Ok(0);
    }
    println!("{}", table(&["RUN", "PID", "COMMAND"], &rows));
    if dry {
        println!(
            "\n{} process(es). Run `amux gc` without --dry-run to end them.",
            rows.len()
        );
        return Ok(0);
    }
    for p in &pids {
        unsafe { libc::kill(*p as libc::pid_t, libc::SIGTERM) };
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let mut forced = false;
    for p in &pids {
        if runs::alive(*p) {
            forced = true;
            unsafe { libc::kill(*p as libc::pid_t, libc::SIGKILL) };
        }
    }
    println!(
        "\nEnded {} process(es){}.",
        rows.len(),
        if forced { " (some needed SIGKILL)" } else { "" }
    );
    Ok(0)
}
