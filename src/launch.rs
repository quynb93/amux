use crate::overlay::sync_overlay;
use crate::paths;
use crate::providers::ProviderSpec;
use crate::secrets::get_token;
use crate::store::{ensure_amux_home, get_provider, home_env_value, overlay_home, Account, Auth};
use crate::{fsx, time};
use anyhow::{anyhow, bail, Result};
use serde_json::json;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Inherited vars that launches drop, because they would point the CLI at another
/// account: the provider's home and token vars plus its `conflictingEnv`.
fn conflicting_env(spec: &ProviderSpec) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let all = std::iter::once(spec.home_env.as_str())
        .chain(spec.token_env.as_deref())
        .chain(spec.conflicting_env.iter().map(String::as_str));
    for k in all {
        if !out.iter().any(|o| o == k) {
            out.push(k.to_string());
        }
    }
    out
}

/// What a launch changes in the inherited environment.
pub struct EnvPlan {
    pub remove: Vec<String>,
    /// Variables to set, in the order they were decided.
    pub set: Vec<(String, String)>,
}

impl EnvPlan {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.set
            .iter()
            .rev()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    }
    pub fn apply(&self, cmd: &mut Command) {
        for k in &self.remove {
            cmd.env_remove(k);
        }
        for (k, v) in &self.set {
            cmd.env(k, v);
        }
    }
}

pub fn build_env(spec: &ProviderSpec, acct: &Account) -> Result<EnvPlan> {
    let mut set = vec![
        (
            spec.home_env.clone(),
            home_env_value(spec, acct).to_string_lossy().into_owned(),
        ),
        ("AMUX_ACCOUNT".to_string(), acct.id()),
    ];
    if acct.privacy() {
        set.extend(spec.privacy_vars());
    }
    let base_url = match (&acct.base_url, &spec.base_url_env) {
        (None, _) => None,
        (Some(url), Some(var)) => Some((var.clone(), url.clone())),
        (Some(_), None) => bail!("Provider \"{}\" has no base-URL env var configured; set \"baseUrlEnv\" in ~/.amux/config.json.", spec.id),
    };
    if acct.auth == Auth::Token {
        let Some(var) = &spec.token_env else {
            bail!(
                "Provider \"{}\" has no tokenEnv; use login auth instead.",
                spec.id
            )
        };
        let token = get_token(acct).ok_or_else(|| {
            anyhow!(
                "No stored token for {0}. Run `amux account token {0}`.",
                acct.id()
            )
        })?;
        set.push((var.clone(), token));
    }
    set.extend(base_url);
    Ok(EnvPlan {
        remove: conflicting_env(spec),
        set,
    })
}

/// Prepares the overlay and returns the env plan to run the provider CLI as `acct`.
pub fn prepare(acct: &Account) -> Result<(ProviderSpec, EnvPlan)> {
    let spec = get_provider(&acct.provider)?;
    ensure_amux_home()?;
    let report = sync_overlay(&spec, &overlay_home(&spec, acct))?;
    if !report.stray.is_empty() {
        eprintln!(
            "amux: {0} has unshared entries: {1} (run `amux sync {0} --adopt`)",
            acct.id(),
            report.stray.join(", ")
        );
    }
    let env = build_env(&spec, acct)?;
    Ok((spec, env))
}

fn log_launch(acct: &Account, args: &[String], cwd: &Path) {
    // Args can contain prompts, so the log is readable by the owner only. Best-effort.
    let line = json!({ "ts": time::iso(time::now_ms()), "account": acct.id(), "cwd": cwd.to_string_lossy(), "args": args });
    let _ = fsx::append_private(&paths::launch_log(), &format!("{line}\n"));
}

fn not_found(spec: &ProviderSpec, e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow!("\"{}\" not found on PATH.", spec.bin)
    } else {
        e.into()
    }
}

/// Runs the provider CLI in the foreground as `acct` and returns its exit code.
/// On Unix amux replaces itself with the CLI, which then owns the terminal and signals.
pub fn run(acct: &Account, args: &[String], cwd: Option<&Path>) -> Result<i32> {
    let (spec, env) = prepare(acct)?;
    let cwd = match cwd {
        Some(c) => c.to_path_buf(),
        None => std::env::current_dir()?,
    };
    log_launch(acct, args, &cwd);
    let mut cmd = Command::new(&spec.bin);
    cmd.args(args).current_dir(&cwd);
    env.apply(&mut cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(not_found(&spec, err))
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().map_err(|e| not_found(&spec, e))?;
        Ok(status.code().unwrap_or(1))
    }
}

/// Runs the provider CLI with a 20-second limit and captures its output (for status checks).
pub fn capture(acct: &Account, args: &[String]) -> Result<(i32, String)> {
    let (spec, env) = prepare(acct)?;
    let mut cmd = Command::new(&spec.bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    env.apply(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| not_found(&spec, e))?;
    let (tx, rx) = std::sync::mpsc::channel::<(usize, String)>();
    let read_all = |i: usize, mut r: Box<dyn Read + Send>| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = r.read_to_end(&mut s);
            let _ = tx.send((i, String::from_utf8_lossy(&s).into_owned()));
        });
    };
    read_all(0, Box::new(child.stdout.take().expect("piped")));
    read_all(1, Box::new(child.stderr.take().expect("piped")));
    drop(tx);
    let deadline = Instant::now() + Duration::from_secs(20);
    let code = loop {
        if let Some(st) = child.try_wait()? {
            break st.code().unwrap_or(1);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break 1;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // A helper the CLI started can keep the pipes open after it exits or is killed, so
    // wait only briefly for the rest of the output instead of joining the readers.
    let mut parts = [String::new(), String::new()];
    let grace = Instant::now() + Duration::from_secs(2);
    while let Ok((i, s)) = rx.recv_timeout(grace.saturating_duration_since(Instant::now())) {
        parts[i] = s;
    }
    Ok((code, parts.concat().trim().to_string()))
}

/// Substitutes `{name}` placeholders; unknown names become empty.
pub fn fill_args(template: &[String], vars: &[(&str, &str)]) -> Vec<String> {
    template
        .iter()
        .map(|a| {
            let mut out = String::new();
            let mut rest = a.as_str();
            while let Some(start) = rest.find('{') {
                out.push_str(&rest[..start]);
                let after = &rest[start + 1..];
                match after.find('}') {
                    Some(end)
                        if end > 0
                            && after[..end]
                                .chars()
                                .all(|c| c.is_alphanumeric() || c == '_') =>
                    {
                        let key = &after[..end];
                        out.push_str(
                            vars.iter()
                                .find(|(k, _)| *k == key)
                                .map(|(_, v)| *v)
                                .unwrap_or(""),
                        );
                        rest = &after[end + 1..];
                    }
                    _ => {
                        out.push('{');
                        rest = after;
                    }
                }
            }
            out.push_str(rest);
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_placeholders() {
        let t = vec![
            "--resume".to_string(),
            "{id}".to_string(),
            "x{nope}y".to_string(),
            "{not a var}".to_string(),
        ];
        assert_eq!(
            fill_args(&t, &[("id", "abc")]),
            vec!["--resume", "abc", "xy", "{not a var}"]
        );
    }
}

#[cfg(test)]
mod env_tests {
    use super::*;

    fn acct(provider: &str) -> Account {
        serde_json::from_value(serde_json::json!({"provider": provider, "name": "work", "auth": "login", "createdAt": "x"})).unwrap()
    }

    fn spec(id: &str) -> ProviderSpec {
        crate::testutil::builtin_spec(id)
    }

    #[test]
    fn points_at_the_overlay_and_drops_inherited_account_vars() {
        let env = build_env(&spec("claude"), &acct("claude")).unwrap();
        assert!(env
            .get("CLAUDE_CONFIG_DIR")
            .unwrap()
            .ends_with("/accounts/claude/work/.claude"));
        assert_eq!(env.get("AMUX_ACCOUNT"), Some("claude@work"));
        assert_eq!(
            env.get("DISABLE_TELEMETRY"),
            None,
            "privacy is off by default"
        );
        assert!(env.remove.contains(&"CLAUDE_CODE_OAUTH_TOKEN".to_string()));
        let codex = build_env(&spec("codex"), &acct("codex")).unwrap();
        assert!(
            codex.remove.contains(&"OPENAI_API_KEY".to_string()),
            "login accounts don't inherit the shell's key"
        );
    }

    #[test]
    fn adds_privacy_and_base_url() {
        let mut a = acct("claude");
        a.privacy = Some(true);
        a.base_url = Some("https://gw.example.com".into());
        let env = build_env(&spec("claude"), &a).unwrap();
        assert_eq!(env.get("DISABLE_TELEMETRY"), Some("1"));
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL"),
            Some("https://gw.example.com")
        );
    }

    #[test]
    fn gemini_points_at_the_parent_and_has_no_base_url() {
        let env = build_env(&spec("gemini"), &acct("gemini")).unwrap();
        assert!(env
            .get("GEMINI_CLI_HOME")
            .unwrap()
            .ends_with("/accounts/gemini/work"));
        let mut a = acct("gemini");
        a.base_url = Some("https://x.example.com".into());
        assert!(build_env(&spec("gemini"), &a)
            .err()
            .unwrap()
            .to_string()
            .contains("no base-URL env var"));
    }
}
