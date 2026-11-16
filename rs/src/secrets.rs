//! Token storage: the macOS Keychain, or a 0600 file in the account dir elsewhere.

use crate::store::{account_dir, Account};
use anyhow::{bail, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const SERVICE: &str = "amux";

fn use_keychain() -> bool {
    cfg!(target_os = "macos")
}

fn fallback_file(a: &Account) -> PathBuf {
    account_dir(a).join("token")
}

pub fn set_token(a: &Account, token: &str) -> Result<()> {
    if token.is_empty() {
        bail!("Empty token for {}; nothing stored.", a.id());
    }
    if use_keychain() {
        // Pass the command on stdin (`security -i`) rather than argv, so the token never
        // shows up in `ps`. -U updates an existing item. Interactive mode splits on
        // whitespace and quotes, so only accept tokens without them.
        if token
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\'))
        {
            bail!("Tokens can't contain spaces, quotes or backslashes.");
        }
        let mut child = Command::new("security")
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            writeln!(
                stdin,
                "add-generic-password -U -s \"{SERVICE}\" -a \"{}\" -w \"{token}\"",
                a.id()
            )?;
        }
        let out = child.wait_with_output()?;
        // `security -i` can exit 0 even when the inner command fails, so read it back.
        if !out.status.success() || get_token(a).as_deref() != Some(token) {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!(
                "Keychain write failed: {}",
                if msg.is_empty() {
                    "token not stored".into()
                } else {
                    msg
                }
            );
        }
        return Ok(());
    }
    crate::fsx::mkdir_private(&account_dir(a))?;
    crate::fsx::write_atomic(&fallback_file(a), token.as_bytes())?;
    Ok(())
}

pub fn get_token(a: &Account) -> Option<String> {
    if use_keychain() {
        let out = Command::new("security")
            .args(["find-generic-password", "-s", SERVICE, "-a", &a.id(), "-w"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        return out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string());
    }
    std::fs::read_to_string(fallback_file(a))
        .ok()
        .map(|s| s.trim_end().to_string())
}

pub fn delete_token(a: &Account) {
    if use_keychain() {
        let _ = Command::new("security")
            .args(["delete-generic-password", "-s", SERVICE, "-a", &a.id()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        return;
    }
    let _ = std::fs::remove_file(fallback_file(a));
}
