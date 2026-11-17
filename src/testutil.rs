//! Helpers for unit tests: throwaway directories and small fixtures.

use crate::providers::{HomeEnvPoints, ProviderSpec};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A fresh, empty temp dir (resolved, so symlinked /tmp paths compare equal), removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "amux-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir.canonicalize().unwrap())
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn write(file: &Path, content: &str) {
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, content).unwrap();
}

pub fn jsonl(records: &[serde_json::Value]) -> String {
    records.iter().map(|r| r.to_string() + "\n").collect()
}

pub fn spec(id: &str, shared_home: &Path) -> ProviderSpec {
    ProviderSpec {
        id: id.into(),
        label: id.into(),
        bin: id.into(),
        shared_home: shared_home.to_path_buf(),
        home_env: "X_HOME".into(),
        home_env_points: HomeEnvPoints::Home,
        private_entries: vec![],
        shared_dirs: vec![],
        login_args: None,
        status_args: None,
        resume_args: None,
        prompt_args: None,
        token_env: None,
        token_hint: None,
        conflicting_env: vec![],
        waiting_patterns: vec![],
        limit_patterns: vec![],
        base_url_env: None,
        privacy_env: None,
        sessions: None,
    }
}

/// A built-in provider, as loaded with an empty config.
pub fn builtin_spec(id: &str) -> ProviderSpec {
    crate::store::load_providers_from(&Default::default())
        .unwrap()
        .into_iter()
        .find(|p| p.id == id)
        .unwrap()
}
