use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

pub static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"))
});

/// Root of amux's own state. Override with AMUX_HOME.
pub static AMUX_HOME: LazyLock<PathBuf> = LazyLock::new(|| match std::env::var_os("AMUX_HOME") {
    Some(p) if !p.is_empty() => resolve(Path::new(&p)),
    _ => HOME.join(".amux"),
});

pub fn config_file() -> PathBuf {
    AMUX_HOME.join("config.json")
}
pub fn accounts_file() -> PathBuf {
    AMUX_HOME.join("accounts.json")
}
pub fn accounts_dir() -> PathBuf {
    AMUX_HOME.join("accounts")
}
pub fn handoff_dir() -> PathBuf {
    AMUX_HOME.join("handoffs")
}
pub fn launch_log() -> PathBuf {
    AMUX_HOME.join("launches.jsonl")
}
pub fn runs_dir() -> PathBuf {
    AMUX_HOME.join("runs")
}
pub fn worktrees_dir() -> PathBuf {
    AMUX_HOME.join("worktrees")
}

/// Makes `p` absolute against the current directory and removes `.` and `..` lexically.
pub fn resolve(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

pub fn expand_home(p: &str) -> PathBuf {
    if p == "~" {
        return HOME.clone();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return HOME.join(rest);
    }
    resolve(Path::new(p))
}

pub fn tildify(p: &Path) -> String {
    match p.strip_prefix(&*HOME) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}
