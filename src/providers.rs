//! Built-in provider definitions. Each describes how a CLI keeps its home directory,
//! so amux can give every account its own login while sharing everything else.
//! Extra providers, or overrides of these, can be declared as plain JSON in
//! ~/.amux/config.json under "providers".

use crate::fsx;
use crate::paths::HOME;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum HomeEnvPoints {
    /// homeEnv points at the home dir itself (CLAUDE_CONFIG_DIR, CODEX_HOME).
    Home,
    /// homeEnv points at a dir that contains the home dir (GEMINI_CLI_HOME → <dir>/.gemini).
    Parent,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSpec {
    pub id: String,
    pub label: String,
    /// Executable name or absolute path.
    pub bin: String,
    /// The real, shared home directory, e.g. ~/.claude (expanded after loading).
    pub shared_home: PathBuf,
    /// Env var that relocates the CLI's home, e.g. CLAUDE_CONFIG_DIR.
    pub home_env: String,
    pub home_env_points: HomeEnvPoints,
    /// Entries in the home dir that belong to one account. Exact names, or a trailing "*" for a prefix.
    pub private_entries: Vec<String>,
    /// Dirs created in the shared home up front so the CLI never makes a private copy.
    pub shared_dirs: Vec<String>,
    pub login_args: Option<Vec<String>>,
    pub status_args: Option<Vec<String>>,
    /// Args to resume a session by id; "{id}" is substituted.
    pub resume_args: Option<Vec<String>>,
    /// Args to start an interactive session with a prompt; "{prompt}" is substituted.
    pub prompt_args: Option<Vec<String>>,
    /// Env var used when an account authenticates with a stored token.
    pub token_env: Option<String>,
    /// Shown when asking for a token, e.g. how to create one.
    #[serde(default)]
    pub token_hint: Option<String>,
    /// Inherited env vars dropped at launch, besides homeEnv and tokenEnv, because they
    /// would point the CLI at another account.
    #[serde(default)]
    pub conflicting_env: Vec<String>,
    /// Env var that points the CLI at another API endpoint (a gateway or relay). It only
    /// redirects where requests go; it never changes organization-managed policy.
    pub base_url_env: Option<String>,
    /// The CLI's own documented opt-outs, set when an account has privacy mode on.
    pub privacy_env: Option<Map<String, Value>>,
    /// Session file format: "claude", "codex" or "gemini".
    pub sessions: Option<String>,
}

impl ProviderSpec {
    pub fn privacy_vars(&self) -> Vec<(String, String)> {
        self.privacy_env
            .iter()
            .flatten()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string()),
                )
            })
            .collect()
    }
}

pub fn builtin() -> Vec<Value> {
    vec![
        json!({
            "id": "claude",
            "label": "Claude Code",
            "bin": "claude",
            "sharedHome": "~/.claude",
            "homeEnv": "CLAUDE_CONFIG_DIR",
            "homeEnvPoints": "home",
            // On macOS the OAuth credentials live in the Keychain under a service name
            // derived from CLAUDE_CONFIG_DIR, so each overlay gets its own login.
            // .claude.json holds the signed-in account and per-account state.
            // remote-settings.json* includes its .signature/.synced-at sidecars, which
            // must match this account's copy.
            "privateEntries": [
                ".claude.json", ".claude.json.backup*", ".credentials.json", ".last-update-result.json",
                ".last-cleanup", "policy-limits.json*", "remote-settings.json*", "stats-cache.json",
                "statsig", "telemetry", "daemon", "backups"
            ],
            "sharedDirs": ["projects", "skills", "plugins", "agents", "commands", "hooks", "todos", "file-history", "sessions"],
            "loginArgs": ["auth", "login"],
            "statusArgs": ["auth", "status"],
            "resumeArgs": ["--resume", "{id}"],
            "promptArgs": ["{prompt}"],
            "tokenEnv": "CLAUDE_CODE_OAUTH_TOKEN",
            "tokenHint": "create one with `claude setup-token`",
            "conflictingEnv": ["CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN"],
            "baseUrlEnv": "ANTHROPIC_BASE_URL",
            // Documented user opt-outs. CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC also turns
            // off auto-update and /bug. Managed (enterprise) settings still take precedence.
            "privacyEnv": {
                "DISABLE_TELEMETRY": "1",
                "DISABLE_ERROR_REPORTING": "1",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"
            },
            "sessions": "claude"
        }),
        json!({
            "id": "codex",
            "label": "Codex CLI",
            "bin": "codex",
            "sharedHome": "~/.codex",
            "homeEnv": "CODEX_HOME",
            "homeEnvPoints": "home",
            "privateEntries": ["auth.json"],
            "sharedDirs": ["sessions", "skills", "prompts", "plugins", "memories", "rules"],
            "loginArgs": ["login"],
            "statusArgs": ["login", "status"],
            "resumeArgs": ["resume", "{id}"],
            "promptArgs": ["{prompt}"],
            "tokenEnv": "OPENAI_API_KEY",
            "conflictingEnv": ["CODEX_HOME"],
            "baseUrlEnv": "OPENAI_BASE_URL",
            "sessions": "codex"
        }),
        json!({
            "id": "gemini",
            "label": "Gemini CLI",
            "bin": "gemini",
            "sharedHome": "~/.gemini",
            "homeEnv": "GEMINI_CLI_HOME",
            "homeEnvPoints": "parent",
            "privateEntries": ["oauth_creds.json", "google_accounts.json"],
            "sharedDirs": ["tmp", "history", "extensions", "commands"],
            // Gemini has no separate login command; it asks on first interactive run.
            "resumeArgs": ["--resume", "{id}"],
            "promptArgs": ["-i", "{prompt}"],
            "tokenEnv": "GEMINI_API_KEY",
            "conflictingEnv": ["GEMINI_CLI_HOME"],
            "sessions": "gemini"
        }),
    ]
}

/// Called once when an account overlay is created, before first launch.
/// Seeds per-account files that would otherwise force a fresh onboarding.
pub fn seed_overlay(spec: &ProviderSpec, overlay_home: &Path) {
    if spec.id != "claude" {
        return;
    }
    // Copy settings like onboarding state, project trust and user-scope MCP servers,
    // but drop the signed-in account so the new one logs in cleanly.
    let candidates = [
        spec.shared_home.join(".claude.json"),
        HOME.join(".claude.json"),
    ];
    let dest = overlay_home.join(".claude.json");
    let Some(src) = candidates.iter().find(|p| p.exists()) else {
        return;
    };
    if dest.exists() {
        return;
    }
    let Ok(Value::Object(mut data)) = std::fs::read(src)
        .map_err(drop)
        .and_then(|b| serde_json::from_slice(&b).map_err(drop))
    else {
        // Unreadable source: let Claude Code create its own.
        return;
    };
    for key in [
        "oauthAccount",
        "primaryApiKey",
        "customApiKeyResponses",
        "userID",
        "s1mAccessCache",
    ] {
        data.remove(key);
    }
    if let Ok(text) = serde_json::to_string_pretty(&Value::Object(data)) {
        let _ = fsx::write_private(&dest, text.as_bytes());
    }
}
