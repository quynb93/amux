//! Sandboxed runs (`amux spawn --sandbox`): the run host starts the CLI inside a
//! container instead of on the machine. The host still owns the terminal; what it starts
//! is `<engine> run --rm -it … <image> <cli> <args>`, with only what the CLI needs mounted
//! at the same paths: the run's checkout (and the repo's git dir for a worktree), the
//! account's overlay and the shared home it links into.
//!
//! Everything about the container is decided when the run is spawned and saved in
//! run.json (`sandbox`), so the host, `amux revive` and `amux stop` all use the same one.
//! Environment variables are passed by name (`-e NAME`), so a token never shows in `ps`.

use crate::launch::EnvPlan;
use crate::paths::{expand_home, tildify};
use crate::providers::ProviderSpec;
use crate::store::{overlay_home, Account, Auth};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// HOME inside the container. The image's own home may not be writable by your uid, and
/// your real home isn't mounted, so the CLI gets a scratch one.
pub const CONTAINER_HOME: &str = "/tmp";

/// A run's container, as saved in run.json.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Sandbox {
    /// `docker`, `podman`, or a path to either (or to something that takes their arguments).
    pub engine: String,
    pub image: String,
    /// False for `--network none`.
    pub network: bool,
    /// `uid:gid`, so files the CLI writes are yours.
    pub user: String,
    pub mounts: Vec<Mount>,
    /// `-e` entries: a bare name takes its value from the host's environment (the
    /// account's variables, AMUX_RUN, a token); `NAME=value` is set as given.
    pub env: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Mount {
    pub src: String,
    pub dst: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

impl Mount {
    fn same(p: &Path) -> Mount {
        let s = p.to_string_lossy().into_owned();
        Mount {
            src: s.clone(),
            dst: s,
            read_only: false,
        }
    }

    /// `path`, `path:ro`, `src:dst` or `src:dst:ro`, as in `docker run -v`; `~` is your home.
    pub fn parse(spec: &str) -> Result<Mount> {
        let mut parts: Vec<&str> = spec.split(':').collect();
        let read_only = match parts.last() {
            Some(&"ro") if parts.len() > 1 => {
                parts.pop();
                true
            }
            Some(&"rw") if parts.len() > 1 => {
                parts.pop();
                false
            }
            _ => false,
        };
        let (src, dst) = match parts.as_slice() {
            [p] if !p.is_empty() => (expand_home(p), expand_home(p)),
            [s, d] if !s.is_empty() && d.starts_with('/') => (expand_home(s), PathBuf::from(d)),
            _ => bail!("Can't read the sandbox mount \"{spec}\": use \"path\", \"path:ro\", \"src:/dst\" or \"src:/dst:ro\"."),
        };
        Ok(Mount {
            src: src.to_string_lossy().into_owned(),
            dst: dst.to_string_lossy().into_owned(),
            read_only,
        })
    }

    fn arg(&self) -> String {
        format!(
            "{}:{}{}",
            self.src,
            self.dst,
            if self.read_only { ":ro" } else { "" }
        )
    }
}

impl Sandbox {
    /// The container's name: `amux-<run>`.
    pub fn container(id: &str) -> String {
        format!("amux-{id}")
    }

    /// `docker` or `podman`, for display.
    pub fn engine_name(&self) -> String {
        Path::new(&self.engine)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.engine.clone())
    }

    fn podman(&self) -> bool {
        self.engine_name().contains("podman")
    }

    /// The whole command the run host starts: engine first, then its arguments.
    /// `bin` is the CLI as the provider names it; only its file name is used, because a
    /// path on this machine means nothing inside the image.
    pub fn command(&self, id: &str, cwd: &str, bin: &str, args: &[String]) -> Vec<String> {
        let mut c: Vec<String> = vec![
            self.engine.clone(),
            "run".into(),
            "--rm".into(),
            "-it".into(),
            "--init".into(),
            "--pull".into(),
            "never".into(),
            "--name".into(),
            Self::container(id),
            "--label".into(),
            format!("amux.run={id}"),
        ];
        if self.podman() {
            // Rootless podman maps your uid to root unless told to keep it.
            c.push("--userns=keep-id".into());
        }
        c.extend(["--user".into(), self.user.clone()]);
        if !self.network {
            c.extend(["--network".into(), "none".into()]);
        }
        c.extend(["-w".into(), cwd.to_string()]);
        for m in &self.mounts {
            c.extend(["-v".into(), m.arg()]);
        }
        for e in &self.env {
            c.extend(["-e".into(), e.clone()]);
        }
        c.push(self.image.clone());
        let name = Path::new(bin)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| bin.to_string());
        c.push(name);
        c.extend(args.iter().cloned());
        c
    }

    /// Removes the run's container if it's still there (`<engine> rm -f`). Quiet and
    /// best-effort: with `--rm` it is usually gone already.
    pub fn remove(&self, id: &str) {
        let _ = Command::new(&self.engine)
            .args(["rm", "-f", &Self::container(id)])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// `"sandbox"` in ~/.amux/config.json.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub engine: Option<String>,
    pub image: Option<String>,
    pub network: Option<bool>,
    pub mounts: Vec<String>,
    /// Every spawn is sandboxed unless it says `--no-sandbox`.
    pub always: bool,
}

impl Config {
    pub fn parse(v: Option<&Value>) -> Result<Config> {
        let Some(v) = v else {
            return Ok(Config::default());
        };
        let Value::Object(m) = v else {
            bail!("\"sandbox\" in config.json must be an object.");
        };
        let s = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_string);
        Ok(Config {
            engine: s("engine"),
            image: s("image"),
            network: match m.get("network") {
                None | Some(Value::Null) => None,
                Some(n) => Some(network_on(n.as_str().unwrap_or(&n.to_string()))?),
            },
            mounts: m
                .get("mounts")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            always: m.get("always").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    pub fn load() -> Result<Config> {
        let cfg = crate::store::load_config()?;
        Config::parse(cfg.extra.get("sandbox"))
    }
}

/// `on` or `off` (also true/false).
pub fn network_on(v: &str) -> Result<bool> {
    match v {
        "on" | "true" => Ok(true),
        "off" | "none" | "false" => Ok(false),
        _ => bail!("The sandbox network is \"on\" or \"off\", not \"{v}\"."),
    }
}

/// What `amux spawn` was asked for.
#[derive(Default, Debug, Clone)]
pub struct Request {
    /// `--sandbox` (None) or `--sandbox=<engine>` (Some).
    pub engine: Option<String>,
    pub image: Option<String>,
    pub network: Option<bool>,
}

/// Where `engine` is: a path that exists, or a name found on PATH.
pub fn find_engine(engine: &str) -> Option<PathBuf> {
    if engine.contains('/') {
        let p = expand_home(engine);
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(engine))
                .find(|p| p.is_file())
        })
        .unwrap_or(None)
}

/// Refuses an account whose login a container can't use: neither a stored token nor a
/// login file in its overlay (on macOS, Claude Code keeps its login in the Keychain).
pub fn check_login(spec: &ProviderSpec, acct: &Account, overlay: &Path) -> Result<()> {
    if acct.auth == Auth::Token || spec.login_files.is_empty() {
        return Ok(());
    }
    if spec.login_files.iter().any(|f| overlay.join(f).exists()) {
        return Ok(());
    }
    let id = acct.id();
    let keychain = if cfg!(target_os = "macos") && spec.id == "claude" {
        " On macOS, Claude Code keeps its login in the Keychain, which a container can't read."
    } else {
        ""
    };
    match &spec.token_env {
        Some(var) => {
            let how = spec
                .token_hint
                .as_deref()
                .map(|h| format!(" ({h})"))
                .unwrap_or_default();
            bail!(
                "{id} has no login a container can use: no stored token, and no {} in {}.{keychain} \
                 Give it a token{how}: `amux account token {id}` reads it from stdin, and the run gets it as {var}.",
                spec.login_files.join(" or "),
                tildify(overlay)
            )
        }
        None => bail!(
            "{id} has no login a container can use: no {} in {}.{keychain} Sign in with `amux {id}` first.",
            spec.login_files.join(" or "),
            tildify(overlay)
        ),
    }
}

/// The folder a run works in, as the container must see it: the top of its git checkout
/// (so git works from any subdirectory), else the folder itself. And for a linked
/// worktree, the repo's git dir its `.git` file points to, which git needs as well.
fn checkout_mounts(cwd: &Path) -> Vec<PathBuf> {
    use crate::git::git;
    let Ok(top) = git(cwd, &["rev-parse", "--show-toplevel"]) else {
        return vec![cwd.to_path_buf()];
    };
    let top = PathBuf::from(top);
    let mut out = vec![top.clone()];
    // --path-format needs git 2.31; older ones print it relative to `cwd`.
    let common = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .or_else(|_| git(cwd, &["rev-parse", "--git-common-dir"]))
    .ok()
    .map(|c| cwd.join(c));
    if let Some(c) = common {
        let c = c.canonicalize().unwrap_or(c);
        let top_c = top.canonicalize().unwrap_or(top);
        if !c.starts_with(&top_c) {
            out.push(c);
        }
    }
    out
}

/// The author and committer git would use here, so commits made in the container (whose
/// HOME has no .gitconfig) are still yours.
fn git_identity(cwd: &Path) -> Vec<String> {
    use crate::git::git;
    let mut out = vec![];
    for (key, who) in [("user.name", "NAME"), ("user.email", "EMAIL")] {
        if let Ok(v) = git(cwd, &["config", key]) {
            if !v.is_empty() {
                out.push(format!("GIT_AUTHOR_{who}={v}"));
                out.push(format!("GIT_COMMITTER_{who}={v}"));
            }
        }
    }
    out
}

/// Adds `m` unless the same folder is mounted already, or it is mounted at its own path
/// inside a folder that is.
fn push_mount(out: &mut Vec<Mount>, m: Mount) {
    let identity = |x: &Mount| x.src == x.dst;
    let covered = out.iter().any(|o| {
        o.dst == m.dst || (identity(o) && identity(&m) && Path::new(&m.dst).starts_with(&o.dst))
    });
    if !covered {
        out.push(m);
    }
}

/// Everything about a run's container, decided at spawn: checks that the engine and the
/// image are here and that the account can sign in from inside, then lists the mounts and
/// the variables to pass. `cwd` is the folder the run starts in.
pub fn plan(
    req: &Request,
    cfg: &Config,
    spec: &ProviderSpec,
    acct: &Account,
    env: &EnvPlan,
    cwd: &Path,
) -> Result<Sandbox> {
    let engine = req
        .engine
        .clone()
        .or_else(|| cfg.engine.clone())
        .unwrap_or_else(|| "docker".into());
    let found = find_engine(&engine).ok_or_else(|| {
        anyhow!(
            "--sandbox runs the CLI with {engine}, and {engine} isn't installed (not on PATH). \
             Install Docker or Podman, or name another engine: --sandbox=podman, or \"sandbox\": {{ \"engine\": \"podman\" }} in ~/.amux/config.json."
        )
    })?;
    let engine = if engine.contains('/') {
        found.to_string_lossy().into_owned()
    } else {
        engine
    };
    let image = req.image.clone().or_else(|| cfg.image.clone()).ok_or_else(|| {
        anyhow!(
            "--sandbox needs an image with the CLI in it, and amux never picks or pulls one for you. \
             Build one (docs/sandbox.md has a Dockerfile), then pass --image <name> or set \"sandbox\": {{ \"image\": \"<name>\" }} in ~/.amux/config.json."
        )
    })?;
    let inspect = Command::new(&engine)
        .args(["image", "inspect", &image])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !inspect.is_ok_and(|s| s.success()) {
        bail!(
            "The image {image} isn't on this machine, and amux doesn't pull images by itself. \
             Build it (see docs/sandbox.md) or `{} pull {image}`, then spawn again.",
            Path::new(&engine)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
    }
    let overlay = overlay_home(spec, acct);
    check_login(spec, acct, &overlay)?;

    let mut mounts = vec![];
    for p in checkout_mounts(cwd) {
        push_mount(&mut mounts, Mount::same(&p));
    }
    // The overlay links into the shared home, so both must be there at the same paths.
    push_mount(&mut mounts, Mount::same(&overlay));
    push_mount(&mut mounts, Mount::same(&spec.shared_home));
    for m in &cfg.mounts {
        let m = Mount::parse(m)?;
        if !Path::new(&m.src).exists() {
            bail!(
                "The sandbox mount {} doesn't exist (from \"mounts\" in ~/.amux/config.json).",
                m.src
            );
        }
        push_mount(&mut mounts, m);
    }

    let mut vars: Vec<String> = vec![];
    for (k, _) in &env.set {
        if !vars.contains(k) {
            vars.push(k.clone());
        }
    }
    for k in ["AMUX_RUN", "TERM", "COLORTERM", "LANG"] {
        if !vars.iter().any(|v| v == k) {
            vars.push(k.into());
        }
    }
    vars.push(format!("HOME={CONTAINER_HOME}"));
    vars.extend(git_identity(cwd));

    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    Ok(Sandbox {
        engine,
        image,
        network: req.network.or(cfg.network).unwrap_or(true),
        user: format!("{uid}:{gid}"),
        mounts,
        env: vars,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{spec, write, TempDir};
    use serde_json::json;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn sb() -> Sandbox {
        Sandbox {
            engine: "docker".into(),
            image: "amux-agents:1".into(),
            network: true,
            user: "501:20".into(),
            mounts: vec![
                Mount::same(Path::new("/w/app-claude-1")),
                Mount::same(Path::new("/src/app/.git")),
                Mount::same(Path::new("/h/.amux/accounts/claude/work/.claude")),
                Mount::same(Path::new("/h/.claude")),
            ],
            env: args(&["CLAUDE_CONFIG_DIR", "AMUX_ACCOUNT", "AMUX_RUN", "HOME=/tmp"]),
        }
    }

    #[test]
    fn builds_the_exact_container_command() {
        let c = sb().command(
            "claude-1",
            "/w/app-claude-1/src",
            "/opt/homebrew/bin/claude",
            &args(&["--model", "opus"]),
        );
        assert_eq!(
            c.join(" "),
            "docker run --rm -it --init --pull never --name amux-claude-1 --label amux.run=claude-1 \
             --user 501:20 -w /w/app-claude-1/src \
             -v /w/app-claude-1:/w/app-claude-1 -v /src/app/.git:/src/app/.git \
             -v /h/.amux/accounts/claude/work/.claude:/h/.amux/accounts/claude/work/.claude \
             -v /h/.claude:/h/.claude \
             -e CLAUDE_CONFIG_DIR -e AMUX_ACCOUNT -e AMUX_RUN -e HOME=/tmp \
             amux-agents:1 claude --model opus"
        );
    }

    #[test]
    fn no_network_and_podman_keep_your_uid() {
        let mut s = sb();
        s.network = false;
        s.engine = "/usr/local/bin/podman".into();
        s.mounts = vec![Mount::parse("/data:/data:ro").unwrap()];
        s.env = vec![];
        let c = s.command("x", "/w", "codex", &[]);
        assert_eq!(
            c.join(" "),
            "/usr/local/bin/podman run --rm -it --init --pull never --name amux-x --label amux.run=x \
             --userns=keep-id --user 501:20 --network none -w /w -v /data:/data:ro amux-agents:1 codex"
        );
        assert_eq!(s.engine_name(), "podman");
    }

    #[test]
    fn reads_mounts_and_config() {
        let m = Mount::parse("/a:ro").unwrap();
        assert_eq!(
            (m.src.as_str(), m.dst.as_str(), m.read_only),
            ("/a", "/a", true)
        );
        let m = Mount::parse("/a:/b").unwrap();
        assert_eq!(
            (m.src.as_str(), m.dst.as_str(), m.read_only),
            ("/a", "/b", false)
        );
        assert!(Mount::parse("/a:b:ro").is_err());
        assert!(Mount::parse("").is_err());

        let c = Config::parse(Some(&json!({
            "engine": "podman", "image": "img", "network": "off", "mounts": ["/x"], "always": true
        })))
        .unwrap();
        assert_eq!(c.engine.as_deref(), Some("podman"));
        assert_eq!(c.network, Some(false));
        assert_eq!(c.mounts, ["/x"]);
        assert!(c.always);
        assert!(Config::parse(Some(&json!({ "network": "maybe" }))).is_err());
        assert_eq!(Config::parse(None).unwrap(), Config::default());
    }

    #[test]
    fn skips_mounts_already_covered() {
        let mut out = vec![];
        push_mount(&mut out, Mount::same(Path::new("/r")));
        push_mount(&mut out, Mount::same(Path::new("/r/.git")));
        push_mount(&mut out, Mount::same(Path::new("/r")));
        push_mount(&mut out, Mount::same(Path::new("/s")));
        let dsts: Vec<&str> = out.iter().map(|m| m.dst.as_str()).collect();
        assert_eq!(dsts, ["/r", "/s"]);
    }

    fn acct(auth: &str) -> Account {
        serde_json::from_value(
            json!({"provider": "claude", "name": "work", "auth": auth, "createdAt": "x"}),
        )
        .unwrap()
    }

    #[test]
    fn refuses_a_login_the_container_cant_read() {
        let t = TempDir::new();
        let mut s = spec("claude", &t.path().join("shared"));
        s.login_files = vec![".credentials.json".into()];
        s.token_env = Some("CLAUDE_CODE_OAUTH_TOKEN".into());
        let overlay = t.path().join("overlay");
        let e = check_login(&s, &acct("login"), &overlay)
            .unwrap_err()
            .to_string();
        assert!(e.contains("amux account token claude@work"), "{e}");
        assert!(e.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{e}");
        // A token account is fine, and so is a login kept in a file.
        assert!(check_login(&s, &acct("token"), &overlay).is_ok());
        write(&overlay.join(".credentials.json"), "{}");
        assert!(check_login(&s, &acct("login"), &overlay).is_ok());
        // A provider that names no login files can't be checked, so it isn't refused.
        s.login_files = vec![];
        assert!(check_login(&s, &acct("login"), &t.path().join("none")).is_ok());
    }

    #[test]
    fn mounts_a_worktree_and_the_repo_git_dir_it_points_to() {
        let t = TempDir::new();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let g = |dir: &Path, a: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(a)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {a:?}");
        };
        g(&repo, &["init", "-q"]);
        write(&repo.join("sub/a.txt"), "a");
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-qm", "init"]);
        let wt = t.path().join("wt");
        g(
            &repo,
            &["worktree", "add", "-q", "-b", "x", wt.to_str().unwrap()],
        );
        assert_eq!(
            checkout_mounts(&wt.join("sub")),
            [wt.clone(), repo.join(".git")]
        );
        // A plain checkout: its top only, from any subdirectory.
        assert_eq!(checkout_mounts(&repo.join("sub")), [repo]);
        // Not a repository: the folder itself.
        let plain = t.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(checkout_mounts(&plain), [plain]);
    }
}
