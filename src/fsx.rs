//! Filesystem helpers that keep amux's files private to the user.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

/// `mkdir -p` with mode 0700 for the directories it creates.
pub fn mkdir_private(p: &Path) -> std::io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    b.mode(0o700);
    b.create(p)
}

fn open_private(p: &Path, opts: &mut OpenOptions) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    opts.mode(0o600);
    opts.open(p)
}

/// Creates or truncates `p` with mode 0600 (on create) and writes `data`.
pub fn write_private(p: &Path, data: &[u8]) -> std::io::Result<()> {
    open_private(
        p,
        OpenOptions::new().write(true).create(true).truncate(true),
    )?
    .write_all(data)
}

/// Writes `data` to a fresh 0600 temp file and renames it over `p`, so readers never
/// see a partial file and old permissions or symlinks at `p` don't carry over.
pub fn write_atomic(p: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = p.with_file_name(name);
    let _ = fs::remove_file(&tmp);
    open_private(&tmp, OpenOptions::new().write(true).create_new(true))?.write_all(data)?;
    fs::rename(&tmp, p)
}

/// Opens `p` for appending, creating it with mode 0600.
pub fn open_append(p: &Path) -> std::io::Result<fs::File> {
    open_private(p, OpenOptions::new().append(true).create(true))
}

/// Appends a line to a 0600 file, tightening its mode if it already existed.
pub fn append_private(p: &Path, line: &str) -> std::io::Result<()> {
    open_append(p)?.write_all(line.as_bytes())?;
    #[cfg(unix)]
    fs::set_permissions(p, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(unix)]
pub fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}
