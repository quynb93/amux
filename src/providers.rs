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
    /// would point the CLI at another account or tie it to another session.
    #[serde(default)]
    pub conflicting_env: Vec<String>,
    /// Text the CLI shows when it waits for you, e.g. a permission prompt. Background
    /// runs match it (case-insensitively) to show "waiting" and send a notification.
    #[serde(default)]
    pub waiting_patterns: Vec<String>,
    /// Text the CLI shows when the account has hit its usage limit. A run that shows it is
    /// marked waiting, and its account limited until the reset time the text names.
    #[serde(default)]
    pub limit_patterns: Vec<String>,
    /// Env var that points the CLI at another API endpoint (a gateway or relay). It only
    /// redirects where requests go; it never changes organization-managed policy.
    pub base_url_env: Option<String>,
    /// The CLI's own documented opt-outs, set when an account has privacy mode on.
    pub privacy_env: Option<Map<String, Value>>,
    /// Session file format: "claude", "codex" or "gemini".
    pub sessions: Option<String>,
    /// Files in the overlay that hold a login, for `amux spawn --sandbox` to check that a
    /// container can sign in (a Keychain login can't be read from one).
    #[serde(default)]
    pub login_files: Vec<String>,
    /// The permission prompt layout auto-approve may read and answer: "claude-code" is
    /// the only one known. Without it, auto-approve leaves this CLI's prompts to you.
    #[serde(default)]
    pub auto_approve: Option<String>,
    /// Entries of the shared home a sandboxed run may read but not change: the settings,
    /// instructions and code the CLI loads on this machine too (`settings.json`, hooks,
    /// skills, plugins, …). The rest of the shared home is writable in the container.
    /// Left out, `DEFAULT_SANDBOX_READ_ONLY` applies.
    #[serde(default)]
    pub sandbox_read_only: Option<Vec<String>>,
}

/// What a sandboxed run can't change in a shared home whose provider doesn't say
/// (`sandboxReadOnly`): every settings file, instruction file and code folder the
/// built-in CLIs load.
pub const DEFAULT_SANDBOX_READ_ONLY: &[&str] = &[
    "settings.json",
    "settings.local.json",
    "CLAUDE.md",
    "AGENTS.md",
    "AGENTS.override.md",
    "GEMINI.md",
    "QWEN.md",
    "config.toml",
    "keybindings.json",
    "hooks",
    "skills",
    "plugins",
    "agents",
    "commands",
    "output-styles",
    "extensions",
    "prompts",
    "rules",
];

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
            "conflictingEnv": [
                "CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN",
                // Set by a running Claude Code session for its own child processes. A CLI
                // amux starts is a separate session, not a child of the one it ran from.
                "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_EXECPATH", "CLAUDE_PID",
                "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ATTENDED",
                "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN"
            ],
            // Permission and confirmation dialogs.
            "waitingPatterns": ["Do you want to", "Enter to confirm"],
            // "You've hit your limit · resets 7pm (Asia/Saigon)", and while it waits:
            // "Usage limit reached · continuing automatically at 12:10pm".
            "limitPatterns": ["You've hit your limit", "Usage limit reached", "limit reached · limit resets"],
            "baseUrlEnv": "ANTHROPIC_BASE_URL",
            // Documented user opt-outs. CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC also turns
            // off auto-update and /bug. Managed (enterprise) settings still take precedence.
            "privacyEnv": {
                "DISABLE_TELEMETRY": "1",
                "DISABLE_ERROR_REPORTING": "1",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"
            },
            "sessions": "claude",
            // On Linux; on macOS the login is in the Keychain instead.
            "loginFiles": [".credentials.json"],
            "autoApprove": "claude-code",
            "sandboxReadOnly": [
                "settings.json", "settings.local.json", "CLAUDE.md", "keybindings.json",
                "hooks", "skills", "plugins", "agents", "commands", "output-styles", "rules"
            ]
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
            "waitingPatterns": ["Would you like to run", "Would you like to make the following edits", "Yes, proceed"],
            "limitPatterns": ["You've hit your usage limit", "usage limit has been reached"],
            "baseUrlEnv": "OPENAI_BASE_URL",
            "sessions": "codex",
            "loginFiles": ["auth.json"],
            "sandboxReadOnly": [
                "config.toml", "AGENTS.md", "AGENTS.override.md", "hooks", "skills", "prompts",
                "plugins", "rules", "agents"
            ]
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
            "waitingPatterns": ["Allow execution", "Apply this change?", "Waiting for user confirmation"],
            "limitPatterns": ["exhausted your daily quota", "Quota exceeded for quota metric"],
            "sessions": "gemini",
            "loginFiles": ["oauth_creds.json"],
            "sandboxReadOnly": [
                "settings.json", "GEMINI.md", "extensions", "commands", "skills", "agents", "hooks"
            ]
        }),
        // Qwen Code (QwenLM/qwen-code), checked against its source:
        // packages/core/src/config/storage.ts reads QWEN_HOME as the whole ~/.qwen dir
        // (getGlobalQwenDir) and keeps the login in oauth_creds.json and
        // google_accounts.json, and MCP logins in mcp-oauth-tokens.json. Sessions go to
        // projects/ under the same dir. packages/cli/src/config/top-level-options.ts has
        // --resume <id> and -i <prompt>; the prompts are in cli/src/i18n/locales/en.js.
        // It signs in with /auth inside a session, so there is no login command.
        json!({
            "id": "qwen",
            "label": "Qwen Code",
            "bin": "qwen",
            "sharedHome": "~/.qwen",
            "homeEnv": "QWEN_HOME",
            "homeEnvPoints": "home",
            "privateEntries": ["oauth_creds.json", "google_accounts.json", "mcp-oauth-tokens.json"],
            "sharedDirs": ["projects", "tmp", "commands", "extensions", "skills"],
            "resumeArgs": ["--resume", "{id}"],
            "promptArgs": ["-i", "{prompt}"],
            "conflictingEnv": ["QWEN_HOME"],
            "waitingPatterns": ["Allow execution", "Apply this change?", "Waiting for user confirmation"],
            "sandboxReadOnly": [
                "settings.json", "QWEN.md", "extensions", "commands", "skills", "agents", "hooks"
            ]
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
