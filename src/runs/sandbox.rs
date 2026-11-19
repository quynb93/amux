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
    /// Which amux home started the container: a short hash of the canonical AMUX_HOME and
    /// your uid. It is in the container's name and its `amux.home` label, so runs of
    /// another home or user on the same engine never share a name, and amux only ever
    /// removes its own containers. Empty for runs saved before it was.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub home: String,
    /// The container's name, `amux-<home>-<run>`. Empty for runs saved before it was,
    /// whose containers are named `amux-<run>`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
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
    /// The container's name: the saved one, or `amux-<run>` for a run saved before names
    /// were.
    pub fn container(&self, id: &str) -> String {
        if self.name.is_empty() {
            format!("amux-{id}")
        } else {
            self.name.clone()
        }
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
        let mut c = self.run_args(&self.container(id), &format!("amux.run={id}"), true, cwd);
        c.extend(self.env.iter().flat_map(|e| ["-e".to_string(), e.clone()]));
        c.push(self.image.clone());
        let name = Path::new(bin)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| bin.to_string());
        c.push(name);
        c.extend(args.iter().cloned());
        c
    }

    /// The command that runs a run's tests in its image, with the same mounts, user and
    /// network as the run: `sh -c <cmd>` in `cwd`, with no terminal. Its container is
    /// `<the run's>-test-<pid>`, labelled `amux.test=<run>` (so stopping the run leaves it
    /// be). The pid keeps two test runs of the same run (`verify` and `land`, say) apart.
    pub fn test_command(&self, id: &str, cwd: &str, cmd: &str) -> Vec<String> {
        let name = format!("{}-test-{}", self.container(id), std::process::id());
        let mut c = self.run_args(&name, &format!("amux.test={id}"), false, cwd);
        for e in ["AMUX_RUN", "CI=1", "PYTHONDONTWRITEBYTECODE=1"] {
            c.extend(["-e".into(), e.into()]);
        }
        // The run's own variables are its account's: the tests don't get them, except
        // HOME and git's identity.
        for e in &self.env {
            if e.starts_with("HOME=") || e.starts_with("GIT_") {
                c.extend(["-e".into(), e.clone()]);
            }
        }
        c.extend([self.image.clone(), "sh".into(), "-c".into(), cmd.into()]);
        c
    }

    /// `<engine> run` up to the image: name, label, user, network, folder and mounts.
    fn run_args(&self, name: &str, label: &str, tty: bool, cwd: &str) -> Vec<String> {
        let mut c: Vec<String> = vec![
            self.engine.clone(),
            "run".into(),
            "--rm".into(),
            if tty { "-it" } else { "-i" }.into(),
            "--init".into(),
            "--pull".into(),
            "never".into(),
            "--name".into(),
            name.into(),
            "--label".into(),
            label.into(),
        ];
        if !self.home.is_empty() {
            c.extend(["--label".into(), format!("amux.home={}", self.home)]);
        }
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
        c
    }

    /// `<engine> ps` filters that match only this run's container: its run and home
    /// labels, or for a run saved before homes were, its run label and name.
    pub fn filters(&self, id: &str) -> Vec<String> {
        let mut f = vec![format!("label=amux.run={id}")];
        if self.home.is_empty() {
            f.push(format!("name={}", self.container(id)));
        } else {
            f.push(format!("label=amux.home={}", self.home));
        }
        f
    }

    /// Removes the run's container if it's still there. Quiet and best-effort: with `--rm`
    /// it is usually gone already. Only a container whose labels say it is this run's is
    /// removed (`<engine> ps -a -q --filter …`, then `rm -f` by id), never another home's
    /// or another user's.
    pub fn remove(&self, id: &str) {
        let mut ps = Command::new(&self.engine);
        ps.args(["ps", "-a", "-q"]);
        for f in self.filters(id) {
            ps.args(["--filter", &f]);
        }
        let Ok(out) = ps.stdin(Stdio::null()).stderr(Stdio::null()).output() else {
            return;
        };
        if !out.status.success() {
            return;
        }
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return;
        }
        let _ = Command::new(&self.engine)
            .args(["rm", "-f"])
            .args(&ids)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// An amux home's id in container names and labels: a short hash of the canonical
/// AMUX_HOME and your uid.
pub fn home_id(home: &Path, uid: u32) -> String {
    use sha2::{Digest, Sha256};
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let h = Sha256::digest(format!("{}\0{uid}", home.to_string_lossy()).as_bytes());
    format!("{h:x}")[..8].to_string()
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
            // Wrongly typed, these would quietly mount less or sandbox nothing: refuse them.
            mounts: match m.get("mounts") {
                None | Some(Value::Null) => vec![],
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|x| {
                        x.as_str().map(str::to_string).ok_or_else(|| {
                            anyhow!("\"sandbox\".\"mounts\" in config.json is a list of strings.")
                        })
                    })
                    .collect::<Result<_>>()?,
                Some(_) => bail!("\"sandbox\".\"mounts\" in config.json is a list of strings."),
            },
            always: match m.get("always") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => bail!("\"sandbox\".\"always\" in config.json is true or false."),
            },
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

/// Where `engine` is: an executable at that path (`~` is your home), or one on PATH.
pub fn find_engine(engine: &str) -> Option<PathBuf> {
    use crate::commands::maintenance::which;
    if engine.contains('/') {
        which(&expand_home(engine).to_string_lossy())
    } else {
        which(engine)
    }
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
    // git prints the top with symlinks resolved, but the container starts in `cwd` as
    // spelled (`-w`): mount the checkout there too, or the CLI starts in an empty folder.
    if !cwd.starts_with(&top) {
        let logical = cwd.canonicalize().ok().and_then(|c| {
            let depth = c.strip_prefix(&top).ok()?.components().count();
            let mut l = cwd.to_path_buf();
            for _ in 0..depth {
                l.pop();
            }
            Some(l)
        });
        out.extend(logical);
    }
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

/// What keeps a sandboxed agent from planting code that git on this machine would run.
/// A sandboxed run in a git repository works in its own linked worktree: in a plain
/// checkout `.git` is the agent's to write, and a `commondir` or `config.worktree` it
/// creates there points git at a config of its own (`core.fsmonitor`, `core.sshCommand`,
/// `diff.external`), which runs at your next git command. So the repository's git dir is
/// mounted read-only (by `plan`), and only what git writes for commits, branches and tags
/// is writable over it: `objects/`, `refs/`, `logs/`, `packed-refs` and this worktree's own
/// `worktrees/<name>/`, with that folder's `commondir`, `gitdir` and `config.worktree`
/// read-only again, as is the worktree's `.git` file. Missing ones are created first, so
/// they can be mounted. `tops` are the paths the checkout is mounted at (its top, and the
/// path the run starts from when that differs). Empty outside git; an error for a plain
/// checkout.
fn git_guard_mounts(cwd: &Path, tops: &[PathBuf]) -> Result<Vec<Mount>> {
    use crate::git::git;
    let abs = |what: &str| {
        git(cwd, &["rev-parse", "--path-format=absolute", what])
            .ok()
            .map(PathBuf::from)
    };
    let (Some(top), Some(git_dir), Some(common)) = (
        abs("--show-toplevel"),
        abs("--git-dir"),
        abs("--git-common-dir"),
    ) else {
        return Ok(vec![]);
    };
    if git_dir == common {
        bail!(
            "A sandboxed run in a git repository works in its own worktree, and {} is a plain checkout: \
             its .git folder would be the agent's to write, and git on your machine would run what it plants there. \
             Spawn it with --worktree (amux spawn --sandbox does that by itself).",
            top.display()
        );
    }
    // Made first, so each can be mounted: git creates them when it first needs them.
    std::fs::create_dir_all(common.join("logs"))?;
    for f in [common.join("packed-refs"), git_dir.join("config.worktree")] {
        let made = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&f);
        match made {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut paths: Vec<(PathBuf, bool)> = vec![(top.join(".git"), true)];
    for d in ["objects", "refs", "logs", "packed-refs"] {
        paths.push((common.join(d), false));
    }
    paths.push((git_dir.clone(), false));
    for f in ["commondir", "gitdir", "config.worktree"] {
        paths.push((git_dir.join(f), true));
    }
    let mut out = vec![];
    for (p, ro) in paths {
        if !p.exists() {
            continue;
        }
        // At each path the checkout is mounted at, or as it is for the repo's git dir.
        let dsts: Vec<PathBuf> = match p.strip_prefix(&top) {
            Ok(rel) => tops.iter().map(|t| t.join(rel)).collect(),
            Err(_) => vec![p.clone()],
        };
        for d in dsts {
            out.push(Mount {
                src: p.to_string_lossy().into_owned(),
                dst: d.to_string_lossy().into_owned(),
                read_only: ro,
            });
        }
    }
    Ok(out)
}

/// What a missing read-only entry of the shared home is created as, so the agent can't
/// create it in the container: a folder when the name has no extension, else a file that
/// reads as "nothing set" to the CLI (`{}` for settings, `{"bindings": []}` for Claude
/// Code's keybindings, empty for instructions and TOML), the same as no file at all.
fn placeholder(name: &str) -> Option<&'static str> {
    if !name.contains('.') {
        return None;
    }
    Some(match name {
        "keybindings.json" => "{\"bindings\": []}\n",
        n if n.ends_with(".json") => "{}\n",
        _ => "",
    })
}

/// The shared home, as a container gets it: writable (the CLI keeps sessions, history,
/// shell snapshots, plans, todos, caches and more there), but with the entries that hold
/// settings, instructions or code the CLI loads on this machine too (the provider's
/// `sandboxReadOnly`) mounted read-only over it, each created first if missing. A mount
/// point can't be removed or renamed, so the agent can't swap one for its own. The login
/// files of the shared home's own account (the provider's `privateEntries` and
/// `loginFiles`) are hidden behind /dev/null or `empty`.
fn home_mounts(spec: &ProviderSpec, empty: &Path) -> Result<Vec<Mount>> {
    let home = &spec.shared_home;
    let s = |p: &Path| p.to_string_lossy().into_owned();
    crate::fsx::mkdir_private(home)?;
    let mut out = vec![Mount::same(home)];
    let defaults: Vec<String> = crate::providers::DEFAULT_SANDBOX_READ_ONLY
        .iter()
        .map(|n| n.to_string())
        .collect();
    let read_only = spec.sandbox_read_only.as_ref().unwrap_or(&defaults);
    for n in read_only {
        if n.is_empty() || n.contains('/') || n == "." || n == ".." {
            bail!("\"sandboxReadOnly\" of provider {} names \"{n}\": use names of entries in its home, without a /.", spec.id);
        }
        let p = home.join(n);
        match std::fs::symlink_metadata(&p) {
            // A link's target would be mounted, not the link: the agent could replace the
            // link itself.
            Ok(m) if m.file_type().is_symlink() => bail!(
                "{} is a symbolic link, and a sandboxed run can't be kept from replacing it. \
                 Move the file it points to there instead, or leave {n} out of the provider's \"sandboxReadOnly\".",
                p.display()
            ),
            Ok(_) => {}
            Err(_) => match placeholder(n) {
                None => crate::fsx::mkdir_private(&p)?,
                Some(text) => {
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    let f = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&p);
                    match f {
                        Ok(mut f) => f.write_all(text.as_bytes())?,
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            },
        }
        out.push(Mount {
            src: s(&p),
            dst: s(&p),
            read_only: true,
        });
    }
    let names: Vec<String> = std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    for n in &names {
        let p = home.join(n);
        if crate::overlay::is_private(spec, n) || spec.login_files.contains(n) {
            out.push(Mount {
                src: if p.is_dir() {
                    s(empty)
                } else {
                    "/dev/null".into()
                },
                dst: s(&p),
                read_only: true,
            });
        }
    }
    Ok(out)
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
    id: &str,
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
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| anyhow!("Could not run {engine}: {e}"))?;
    if !inspect.status.success() {
        // A daemon that isn't running (or a socket you can't reach) also fails here: say
        // that rather than suggest pulling an image that may well be there.
        let err = String::from_utf8_lossy(&inspect.stderr);
        let err = err.trim();
        let lower = err.to_lowercase();
        if !err.is_empty() && !lower.contains("no such") && !lower.contains("not known") {
            bail!("{engine} couldn't check for the image {image}: {err}");
        }
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
    let checkout = checkout_mounts(cwd);
    let is_git_dir = |p: &Path| {
        p.join("HEAD").is_file() && p.join("objects").is_dir() && p.join("refs").is_dir()
    };
    for p in &checkout {
        // The repository's git dir, for a worktree: read-only, but for what git writes
        // (see `git_guard_mounts`).
        let git_dir = is_git_dir(p);
        push_mount(
            &mut mounts,
            Mount {
                read_only: git_dir,
                ..Mount::same(p)
            },
        );
    }
    // The overlay links into the shared home, so both must be there at the same paths.
    push_mount(&mut mounts, Mount::same(&overlay));
    let empty = crate::runs::run_dir(id).join("sandbox-empty");
    std::fs::create_dir_all(&empty)?;
    let mut home = home_mounts(spec, &empty)?.into_iter();
    if let Some(top) = home.next() {
        push_mount(&mut mounts, top);
    }
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
    // Inside the mounts above, so after them: the engines mount parents first.
    let tops: Vec<PathBuf> = checkout
        .iter()
        .filter(|p| !is_git_dir(p))
        .cloned()
        .collect();
    mounts.extend(git_guard_mounts(cwd, &tops)?);
    mounts.extend(home);

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
    // Unless the provider moves the CLI's home with HOME itself, which then must win.
    if !vars.iter().any(|v| v == "HOME") {
        vars.push(format!("HOME={CONTAINER_HOME}"));
    }
    vars.extend(git_identity(cwd));

    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let home = home_id(&crate::paths::AMUX_HOME, uid);
    Ok(Sandbox {
        name: format!("amux-{home}-{id}"),
        home,
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
            home: "0123abcd".into(),
            name: "amux-0123abcd-claude-1".into(),
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
            "docker run --rm -it --init --pull never --name amux-0123abcd-claude-1 \
             --label amux.run=claude-1 --label amux.home=0123abcd \
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
        // A run saved before containers had a home: the old name, and no home label.
        let mut s = sb();
        s.home.clear();
        s.name.clear();
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
        assert_eq!(s.filters("x"), args(&["label=amux.run=x", "name=amux-x"]));
    }

    #[test]
    fn containers_are_named_and_labelled_per_amux_home() {
        let s = sb();
        assert_eq!(s.container("claude-1"), "amux-0123abcd-claude-1");
        assert_eq!(
            s.filters("claude-1"),
            args(&["label=amux.run=claude-1", "label=amux.home=0123abcd"])
        );
        // The id depends on the home and the uid, and is stable for both.
        let (a, b) = (TempDir::new(), TempDir::new());
        let id = home_id(a.path(), 501);
        assert_eq!(id.len(), 8);
        assert_eq!(id, home_id(a.path(), 501));
        assert_ne!(id, home_id(b.path(), 501));
        assert_ne!(id, home_id(a.path(), 502));
        // A run.json from before keeps working: no home or name, the old container name.
        let old: Sandbox = serde_json::from_value(json!({
            "engine": "docker", "image": "i", "network": true, "user": "1:1",
            "mounts": [], "env": []
        }))
        .unwrap();
        assert_eq!(old.container("r"), "amux-r");
        let saved = serde_json::to_value(&s).unwrap();
        assert_eq!(saved["name"], "amux-0123abcd-claude-1");
        assert_eq!(saved["home"], "0123abcd");
    }

    #[test]
    fn removes_only_its_own_container() {
        // A fake engine: `ps` lists a container only for this home's filters; `rm` is logged.
        let d = TempDir::new();
        let log = d.path().join("log");
        let engine = d.path().join("engine");
        write(
            &engine,
            &format!(
                "#!/bin/sh\necho \"$*\" >> {}\n\
                 [ \"$1\" = ps ] && case \"$*\" in *amux.home=0123abcd*) echo c0ffee;; esac\nexit 0\n",
                log.display()
            ),
        );
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut s = sb();
        s.engine = engine.to_string_lossy().into_owned();
        s.remove("claude-1");
        let mut other = s.clone();
        other.home = "ffffffff".into();
        other.name = "amux-ffffffff-claude-1".into();
        other.remove("claude-1");
        let log = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            [
                "ps -a -q --filter label=amux.run=claude-1 --filter label=amux.home=0123abcd",
                "rm -f c0ffee",
                "ps -a -q --filter label=amux.run=claude-1 --filter label=amux.home=ffffffff",
            ]
        );
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
        // Reached through a symlink: also at the path the run starts from.
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        assert_eq!(checkout_mounts(&link.join("sub")), [repo.clone(), link]);
        // A plain checkout: its top only, from any subdirectory.
        assert_eq!(checkout_mounts(&repo.join("sub")), [repo]);
        // Not a repository: the folder itself.
        let plain = t.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(checkout_mounts(&plain), [plain]);
    }

    fn git_in(dir: &Path, a: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(a)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {a:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repo with one commit, and a linked worktree of it on branch x.
    fn repo_and_worktree(t: &TempDir) -> (PathBuf, PathBuf) {
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q"]);
        write(&repo.join("a.txt"), "a");
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-qm", "init"]);
        let wt = t.path().join("wt");
        git_in(
            &repo,
            &["worktree", "add", "-q", "-b", "x", wt.to_str().unwrap()],
        );
        (repo, wt)
    }

    /// In a plain checkout, .git is the agent's to write: a `commondir` or
    /// `config.worktree` it creates there points git at a config of its own, which runs on
    /// this machine at the next git command. A sandboxed run gets a worktree instead, whose
    /// repository git dir is read-only but for what git writes.
    #[test]
    fn a_sandboxed_checkout_is_a_worktree_with_its_git_dir_read_only() {
        let t = TempDir::new();
        let (repo, wt) = repo_and_worktree(&t);
        let args = |ms: &[Mount]| ms.iter().map(Mount::arg).collect::<Vec<_>>();
        // A plain checkout: refused.
        let err = git_guard_mounts(&repo, std::slice::from_ref(&repo)).unwrap_err();
        assert!(err.to_string().contains("--worktree"), "{err}");
        let (r, w) = (repo.display(), wt.display());
        let g = format!("{r}/.git");
        let wg = format!("{g}/worktrees/wt");
        assert_eq!(
            args(&git_guard_mounts(&wt, std::slice::from_ref(&wt)).unwrap()),
            [
                format!("{w}/.git:{w}/.git:ro"),
                format!("{g}/objects:{g}/objects"),
                format!("{g}/refs:{g}/refs"),
                format!("{g}/logs:{g}/logs"),
                format!("{g}/packed-refs:{g}/packed-refs"),
                format!("{wg}:{wg}"),
                format!("{wg}/commondir:{wg}/commondir:ro"),
                format!("{wg}/gitdir:{wg}/gitdir:ro"),
                format!("{wg}/config.worktree:{wg}/config.worktree:ro"),
            ]
        );
        // Created so they could be mounted, empty: git reads them as nothing set.
        assert_eq!(std::fs::read(repo.join(".git/packed-refs")).unwrap(), b"");
        assert_eq!(
            std::fs::read(repo.join(".git/worktrees/wt/config.worktree")).unwrap(),
            b""
        );
        // The repository's git dir itself is read-only in the container.
        assert_eq!(checkout_mounts(&wt), [wt.clone(), repo.join(".git")]);
        // Not a repository: nothing.
        assert!(git_guard_mounts(t.path(), &[t.path().to_path_buf()])
            .unwrap()
            .is_empty());
    }

    /// What git writes in the repository's git dir for an agent's everyday work in its
    /// worktree (commits, branches, tags, merges, stashes, resets, rebases, a local fetch)
    /// lands only where the container may write: nothing is refused that git needs.
    #[test]
    fn git_in_a_worktree_writes_only_where_the_sandbox_lets_it() {
        let t = TempDir::new();
        let (repo, wt) = repo_and_worktree(&t);
        git_in(&repo, &["tag", "v0"]);
        git_in(&repo, &["branch", "other"]);
        git_in(&repo, &["pack-refs", "--all"]);
        let mounts = git_guard_mounts(&wt, std::slice::from_ref(&wt)).unwrap();
        let common = repo.join(".git");
        // Every file under the git dir, with its content.
        fn files(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    files(&p, out);
                } else {
                    out.push((p.clone(), std::fs::read(&p).unwrap_or_default()));
                }
            }
        }
        let snap = || {
            let mut v = vec![];
            files(&common, &mut v);
            v.sort();
            v
        };
        let before = snap();
        write(&wt.join("b.txt"), "b");
        for a in [
            &["add", "."][..],
            &["commit", "-qm", "one"],
            &["branch", "b2"],
            &["tag", "t1"],
            &["tag", "-a", "t2", "-m", "annotated"],
            &["checkout", "-q", "-b", "y"],
            &["commit", "-q", "--allow-empty", "-m", "two"],
            &["checkout", "-q", "x"],
            &["merge", "-q", "--no-edit", "y"],
            &["stash", "-q", "--include-untracked"],
            &["reset", "-q", "--hard", "HEAD~1"],
            &["rebase", "-q", "other", "y"],
            &["checkout", "-q", "x"],
            &["branch", "-D", "b2"],
            &["tag", "-d", "t1"],
            &["fetch", "-q", "../repo", "x"],
        ] {
            git_in(&wt, a);
        }
        let after = snap();
        let writable = |p: &Path| {
            let ro = mounts
                .iter()
                .filter(|m| Path::new(&m.dst).starts_with(&common))
                .filter(|m| p.starts_with(&m.dst))
                .max_by_key(|m| m.dst.len());
            ro.is_some_and(|m| !m.read_only)
        };
        let mut changed: Vec<&PathBuf> = after
            .iter()
            .filter(|f| !before.contains(f))
            .map(|(p, _)| p)
            .collect();
        changed.extend(
            before
                .iter()
                .filter(|(p, _)| !after.iter().any(|(q, _)| q == p))
                .map(|(p, _)| p),
        );
        assert!(changed.len() > 5, "{changed:?}");
        let refused: Vec<_> = changed.iter().filter(|p| !writable(p)).collect();
        assert!(
            refused.is_empty(),
            "written outside the writable mounts: {refused:?}"
        );
    }

    /// The shared home is writable (Claude Code writes shell-snapshots/, plans/, debug/,
    /// session-env/, … there) but its settings, instructions and code are read-only, created
    /// first when missing, and its own login is hidden.
    #[test]
    fn the_shared_home_is_writable_but_its_settings_and_code_are_not() {
        let t = TempDir::new();
        let home = t.path().join("home");
        let mut s = spec("claude", &home);
        s.sandbox_read_only = Some(vec![
            "settings.json".into(),
            "keybindings.json".into(),
            "CLAUDE.md".into(),
            "hooks".into(),
            "skills".into(),
        ]);
        s.private_entries = vec![".credentials.json".into(), "statsig".into()];
        s.login_files = vec!["auth.json".into()];
        for d in ["projects", "hooks", "statsig"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        for f in [".credentials.json", "auth.json", "history.jsonl"] {
            write(&home.join(f), "{}");
        }
        write(&home.join("CLAUDE.md"), "be nice");
        let empty = t.path().join("empty");
        let mut got: Vec<String> = home_mounts(&s, &empty)
            .unwrap()
            .iter()
            .map(Mount::arg)
            .collect();
        got.sort();
        let h = home.display();
        let e = empty.display();
        let mut want = vec![
            format!("{h}:{h}"),
            format!("{h}/settings.json:{h}/settings.json:ro"),
            format!("{h}/keybindings.json:{h}/keybindings.json:ro"),
            format!("{h}/CLAUDE.md:{h}/CLAUDE.md:ro"),
            format!("{h}/hooks:{h}/hooks:ro"),
            format!("{h}/skills:{h}/skills:ro"),
            format!("/dev/null:{h}/.credentials.json:ro"),
            format!("/dev/null:{h}/auth.json:ro"),
            format!("{e}:{h}/statsig:ro"),
        ];
        want.sort();
        assert_eq!(got, want);
        // Missing ones were made, reading as "nothing set"; existing ones kept.
        let read = |n: &str| std::fs::read_to_string(home.join(n)).unwrap();
        assert_eq!(read("settings.json"), "{}\n");
        assert_eq!(read("keybindings.json"), "{\"bindings\": []}\n");
        assert_eq!(read("CLAUDE.md"), "be nice");
        assert!(home.join("skills").is_dir());
        // A link can't be kept read-only: the agent could replace the link.
        std::fs::remove_file(home.join("CLAUDE.md")).unwrap();
        std::os::unix::fs::symlink(t.path().join("elsewhere.md"), home.join("CLAUDE.md")).unwrap();
        assert!(home_mounts(&s, &empty).is_err());
        // Names only.
        s.sandbox_read_only = Some(vec!["../x".into()]);
        assert!(home_mounts(&s, &empty).is_err());
        // A provider that doesn't say gets the defaults.
        let home2 = t.path().join("home2");
        let s2 = spec("mycli", &home2);
        let got = home_mounts(&s2, &empty).unwrap();
        for n in [
            "settings.json",
            "AGENTS.md",
            "config.toml",
            "plugins",
            "extensions",
        ] {
            let p = home2.join(n).to_string_lossy().into_owned();
            assert!(got.iter().any(|m| m.dst == p && m.read_only), "{n}");
        }
    }

    /// The built-in providers keep their CLIs' settings and code read-only.
    #[test]
    fn built_in_providers_name_what_a_sandbox_cant_change() {
        for (id, must) in [
            (
                "claude",
                &["settings.json", "CLAUDE.md", "hooks", "skills", "plugins"][..],
            ),
            (
                "codex",
                &["config.toml", "AGENTS.md", "skills", "rules"][..],
            ),
            (
                "gemini",
                &["settings.json", "GEMINI.md", "extensions", "commands"][..],
            ),
        ] {
            let ro = crate::testutil::builtin_spec(id).sandbox_read_only.unwrap();
            for n in must {
                assert!(ro.iter().any(|r| r == n), "{id}: {n}");
            }
        }
    }
}
