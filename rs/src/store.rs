use crate::fsx;
use crate::paths::{self, expand_home};
use crate::providers::{self, HomeEnvPoints, ProviderSpec};
use anyhow::{anyhow, bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// ~/.amux/config.json. Unknown keys are kept when amux rewrites the file.
#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    /// Extra providers, or partial overrides of built-ins matched by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<Value>>,
    /// Default account per provider id, e.g. { "claude": "work" }.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Config {
    pub fn default_for(&self, provider: &str) -> Option<&str> {
        self.defaults.as_ref()?.get(provider)?.as_str()
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    /// The CLI's own sign-in inside the overlay.
    Login,
    /// A stored secret injected as the provider's tokenEnv.
    Token,
}

impl Auth {
    pub fn as_str(self) -> &'static str {
        match self {
            Auth::Login => "login",
            Auth::Token => "token",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub provider: String,
    pub name: String,
    pub auth: Auth,
    /// Applies the provider's privacyEnv on every launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy: Option<bool>,
    /// Overrides the provider's API endpoint via its baseUrlEnv on every launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Fields written by other amux versions, kept as they are.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Account {
    pub fn id(&self) -> String {
        format!("{}@{}", self.provider, self.name)
    }
    pub fn privacy(&self) -> bool {
        self.privacy.unwrap_or(false)
    }
}

#[derive(Serialize, Deserialize, Default)]
struct AccountsFile {
    #[serde(default)]
    accounts: Vec<Account>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

fn read_json<T: DeserializeOwned + Default>(file: &Path) -> Result<T> {
    match std::fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| anyhow!("Cannot parse {}: {e}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(anyhow!("Cannot parse {}: {e}", file.display())),
    }
}

fn write_json<T: Serialize>(file: &Path, data: &T) -> Result<()> {
    if let Some(dir) = file.parent() {
        fsx::mkdir_private(dir)?;
    }
    let text = serde_json::to_string_pretty(data)? + "\n";
    fsx::write_atomic(file, text.as_bytes())
        .with_context(|| format!("Cannot write {}", file.display()))
}

pub fn load_config() -> Result<Config> {
    read_json(&paths::config_file())
}

pub fn save_config(cfg: &Config) -> Result<()> {
    write_json(&paths::config_file(), cfg)
}

fn falsy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        _ => false,
    }
}

/// Built-in providers merged with the ones in config.json, in definition order.
pub fn load_providers_from(cfg: &Config) -> Result<Vec<ProviderSpec>> {
    let mut raw: Vec<Map<String, Value>> = providers::builtin()
        .into_iter()
        .filter_map(|v| {
            if let Value::Object(m) = v {
                Some(m)
            } else {
                None
            }
        })
        .collect();
    for custom in cfg.providers.iter().flatten() {
        let Some(custom) = custom.as_object() else {
            bail!(
                "Each entry in \"providers\" in {} must be an object.",
                paths::config_file().display()
            )
        };
        let Some(id) = custom
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            bail!(
                "A provider in {} has no \"id\".",
                paths::config_file().display()
            )
        };
        let pos = raw
            .iter()
            .position(|m| m.get("id").and_then(Value::as_str) == Some(&id));
        let mut merged = pos.map(|i| raw[i].clone()).unwrap_or_default();
        for (k, v) in custom {
            merged.insert(k.clone(), v.clone());
        }
        let missing: Vec<&str> = ["bin", "sharedHome", "homeEnv"]
            .into_iter()
            .filter(|k| falsy(merged.get(*k)))
            .collect();
        if !missing.is_empty() {
            bail!(
                "Provider \"{id}\" in {} is missing: {}",
                paths::config_file().display(),
                missing.join(", ")
            );
        }
        for (k, default) in [
            ("label", Value::String(id.clone())),
            ("homeEnvPoints", "home".into()),
            ("privateEntries", Value::Array(vec![])),
            ("sharedDirs", Value::Array(vec![])),
        ] {
            if matches!(merged.get(k), None | Some(Value::Null)) {
                merged.insert(k.to_string(), default);
            }
        }
        match pos {
            Some(i) => raw[i] = merged,
            None => raw.push(merged),
        }
    }
    raw.into_iter()
        .map(|m| {
            let id = m
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let shared = m
                .get("sharedHome")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut spec: ProviderSpec = serde_json::from_value(Value::Object(m)).map_err(|e| {
                anyhow!(
                    "Provider \"{id}\" in {} is invalid: {e}",
                    paths::config_file().display()
                )
            })?;
            if let Some(s) = shared {
                spec.shared_home = expand_home(&s);
            }
            Ok(spec)
        })
        .collect()
}

pub fn load_providers() -> Result<Vec<ProviderSpec>> {
    load_providers_from(&load_config()?)
}

pub fn get_provider(id: &str) -> Result<ProviderSpec> {
    load_providers()?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| anyhow!("Unknown provider \"{id}\". Run `amux providers` to see the list."))
}

pub fn load_accounts() -> Result<Vec<Account>> {
    Ok(read_json::<AccountsFile>(&paths::accounts_file())?.accounts)
}

pub fn save_accounts(accounts: Vec<Account>) -> Result<()> {
    write_json(
        &paths::accounts_file(),
        &AccountsFile {
            accounts,
            extra: Map::new(),
        },
    )
}

/// Parses "claude@work", or "claude" (uses the provider's default account).
pub fn resolve_account(r: &str) -> Result<Account> {
    let accounts = load_accounts()?;
    let mut parts = r.split('@');
    let provider = parts.next().unwrap_or_default().to_string();
    let mut name = parts.next().unwrap_or_default().to_string();
    if name.is_empty() {
        name = load_config()?
            .default_for(&provider)
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            let own: Vec<&Account> = accounts.iter().filter(|a| a.provider == provider).collect();
            if own.len() == 1 {
                name = own[0].name.clone();
            } else {
                bail!("\"{r}\" is ambiguous. Use {provider}@<name> or set a default with `amux account default {provider}@<name>`.");
            }
        }
    }
    accounts
        .into_iter()
        .find(|a| a.provider == provider && a.name == name)
        .ok_or_else(|| anyhow!("No account \"{provider}@{name}\". Add it with `amux account add {provider} {name}`."))
}

/// amux's per-account dir: ~/.amux/accounts/<provider>/<name>
pub fn account_dir(a: &Account) -> PathBuf {
    paths::accounts_dir().join(&a.provider).join(&a.name)
}

/// The relocated home dir the CLI sees for this account, e.g. ~/.amux/accounts/claude/work/.claude
pub fn overlay_home(spec: &ProviderSpec, a: &Account) -> PathBuf {
    account_dir(a).join(spec.shared_home.file_name().unwrap_or_default())
}

/// Value to put in spec.home_env.
pub fn home_env_value(spec: &ProviderSpec, a: &Account) -> PathBuf {
    match spec.home_env_points {
        HomeEnvPoints::Parent => account_dir(a),
        HomeEnvPoints::Home => overlay_home(spec, a),
    }
}

pub fn ensure_amux_home() -> Result<()> {
    Ok(fsx::mkdir_private(&paths::AMUX_HOME)?)
}
