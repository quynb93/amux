use crate::launch::{capture, run};
use crate::overlay::sync_overlay;
use crate::paths::tildify;
use crate::providers::{seed_overlay, ProviderSpec};
use crate::secrets::{delete_token, set_token};
use crate::store::{
    account_dir, ensure_amux_home, get_provider, load_accounts, load_config, load_providers,
    overlay_home, resolve_account, save_accounts, save_config, Account, Auth,
};
use crate::time;
use crate::util::{need, parse_args, read_secret, table, Kind};
use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Validates a gateway URL for `spec` and normalizes it (no trailing slash).
pub fn validate_base_url(url: &str, spec: &ProviderSpec) -> Result<String> {
    if spec.base_url_env.is_none() {
        bail!("{} has no base-URL env var configured. Add \"baseUrlEnv\" for \"{}\" in ~/.amux/config.json.", spec.label, spec.id);
    }
    let parsed = url::Url::parse(url).map_err(|_| {
        anyhow!("\"{url}\" is not a valid URL (expected e.g. https://gateway.example.com).")
    })?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        bail!("Base URL must use http or https.");
    }
    let s = parsed.to_string();
    Ok(s.strip_suffix('/').map(str::to_string).unwrap_or(s))
}

/// Applies `change` to one stored account and saves the list.
fn update_account(acct: &Account, change: impl Fn(&mut Account)) -> Result<()> {
    let mut accounts = load_accounts()?;
    for a in accounts.iter_mut().filter(|a| a.id() == acct.id()) {
        change(a);
    }
    save_accounts(accounts)
}

fn add(argv: &[String]) -> Result<i32> {
    let usage = "amux account add <provider> <name> [--token] [--privacy] [--base-url <url>] [--no-login] [--note <text>]";
    let a = parse_args(
        argv,
        &[
            ("token", Kind::Bool),
            ("no-login", Kind::Bool),
            ("privacy", Kind::Bool),
            ("note", Kind::Value),
            ("base-url", Kind::Value),
        ],
    );
    let provider_id = need(a.pos.first(), usage)?;
    let name = need(a.pos.get(1), usage)?;
    if !valid_name(name) {
        bail!("Account names may use letters, digits, '.', '_' and '-'.");
    }
    let spec = get_provider(provider_id)?;
    if a.has("privacy") && spec.privacy_env.is_none() {
        bail!("{} has no privacy settings configured.", spec.label);
    }
    let base_url = a
        .value("base-url")
        .map(|u| validate_base_url(u, &spec))
        .transpose()?;
    if a.has("token") && spec.token_env.is_none() {
        bail!("{} has no token env var configured.", spec.label);
    }
    let mut accounts = load_accounts()?;
    if accounts
        .iter()
        .any(|x| x.provider == provider_id && x.name == name)
    {
        bail!("{provider_id}@{name} already exists.");
    }

    let acct = Account {
        provider: provider_id.to_string(),
        name: name.to_string(),
        auth: if a.has("token") {
            Auth::Token
        } else {
            Auth::Login
        },
        privacy: a.has("privacy").then_some(true),
        base_url,
        created_at: time::iso(time::now_ms()),
        note: a.value("note").map(str::to_string),
        extra: Map::new(),
    };
    ensure_amux_home()?;
    let home = overlay_home(&spec, &acct);
    sync_overlay(&spec, &home)?;
    seed_overlay(&spec, &home);
    accounts.push(acct.clone());
    save_accounts(accounts)?;
    println!("Created {} → {}", acct.id(), tildify(&home));

    if acct.auth == Auth::Token {
        let hint = if provider_id == "claude" {
            " (create one with `claude setup-token`)"
        } else {
            ""
        };
        let token = read_secret(&format!(
            "Paste {} for {}{hint}: ",
            spec.token_env.as_deref().unwrap_or(""),
            acct.id()
        ))?;
        set_token(&acct, &token)?;
        println!("Stored token for {}.", acct.id());
    } else if !a.has("no-login") {
        if let Some(args) = &spec.login_args {
            println!("Signing in: {} {}", spec.bin, args.join(" "));
            return run(&acct, args, None);
        }
        println!(
            "{} signs in on first run: amux run {}",
            spec.label,
            acct.id()
        );
    }
    Ok(0)
}

fn list() -> Result<i32> {
    let accounts = load_accounts()?;
    if accounts.is_empty() {
        println!("No accounts yet. Add one with: amux account add <provider> <name>");
        return Ok(0);
    }
    let cfg = load_config()?;
    let providers = load_providers()?;
    let mut rows = vec![[
        "ACCOUNT", "AUTH", "PRIVACY", "BASE URL", "DEFAULT", "HOME", "NOTE",
    ]
    .map(String::from)
    .to_vec()];
    for a in &accounts {
        let spec = providers.iter().find(|p| p.id == a.provider);
        rows.push(vec![
            a.id(),
            a.auth.as_str().into(),
            if a.privacy() {
                "on".into()
            } else {
                String::new()
            },
            a.base_url.clone().unwrap_or_default(),
            if cfg.default_for(&a.provider) == Some(a.name.as_str()) {
                "*".into()
            } else {
                String::new()
            },
            spec.map(|s| tildify(&overlay_home(s, a)))
                .unwrap_or_else(|| "(unknown provider)".into()),
            a.note.clone().unwrap_or_default(),
        ]);
    }
    println!("{}", table(&rows));
    Ok(0)
}

fn set_default(argv: &[String]) -> Result<i32> {
    let acct = resolve_account(need(
        argv.first(),
        "amux account default <provider>@<name>",
    )?)?;
    let mut cfg = load_config()?;
    cfg.defaults
        .get_or_insert_with(Map::new)
        .insert(acct.provider.clone(), Value::String(acct.name.clone()));
    save_config(&cfg)?;
    println!("Default {} account: {}", acct.provider, acct.name);
    Ok(0)
}

fn token(argv: &[String]) -> Result<i32> {
    let acct = resolve_account(need(argv.first(), "amux account token <provider>@<name>")?)?;
    let spec = get_provider(&acct.provider)?;
    let Some(var) = &spec.token_env else {
        bail!("{} has no token env var configured.", spec.label)
    };
    set_token(
        &acct,
        &read_secret(&format!("Paste {var} for {}: ", acct.id()))?,
    )?;
    if acct.auth != Auth::Token {
        update_account(&acct, |a| a.auth = Auth::Token)?;
    }
    println!("Stored token for {}.", acct.id());
    Ok(0)
}

fn base_url(argv: &[String]) -> Result<i32> {
    let usage = "amux account baseurl <provider>@<name> <url|off>";
    let acct = resolve_account(need(argv.first(), usage)?)?;
    let value = need(argv.get(1), usage)?;
    let spec = get_provider(&acct.provider)?;
    let url = if ["off", "none", "-"].contains(&value) {
        None
    } else {
        Some(validate_base_url(value, &spec)?)
    };
    update_account(&acct, |a| a.base_url = url.clone())?;
    match url {
        Some(url) => {
            println!("Base URL for {}: {url}", acct.id());
            println!(
                "  Launches set {}={url}",
                spec.base_url_env.as_deref().unwrap_or("")
            );
            println!("Point this only at an endpoint you are allowed to use. Organization-managed settings are unaffected.");
        }
        None => println!(
            "Base URL cleared for {}; it uses the provider default.",
            acct.id()
        ),
    }
    Ok(0)
}

fn privacy(argv: &[String]) -> Result<i32> {
    let usage = "amux account privacy <provider>@<name> on|off";
    let acct = resolve_account(need(argv.first(), usage)?)?;
    let mode = need(argv.get(1), usage)?;
    if mode != "on" && mode != "off" {
        bail!("Usage: {usage}");
    }
    let spec = get_provider(&acct.provider)?;
    if mode == "on" && spec.privacy_env.is_none() {
        bail!("{} has no privacy settings configured.", spec.label);
    }
    update_account(&acct, |a| a.privacy = Some(mode == "on"))?;
    println!("Privacy {mode} for {}.", acct.id());
    if mode == "on" {
        for (k, v) in spec.privacy_vars() {
            println!("  {k}={v}");
        }
        println!("Organization-managed settings still take precedence over these.");
    }
    Ok(0)
}

fn status(argv: &[String]) -> Result<i32> {
    let targets = match argv.first() {
        Some(r) => vec![resolve_account(r)?],
        None => load_accounts()?,
    };
    for acct in targets {
        let spec = get_provider(&acct.provider)?;
        let Some(args) = &spec.status_args else {
            println!("{}: {} has no status command", acct.id(), spec.label);
            continue;
        };
        let (_, out) = capture(&acct, args)?;
        println!(
            "{}:\n  {}",
            acct.id(),
            out.split('\n').collect::<Vec<_>>().join("\n  ")
        );
    }
    Ok(0)
}

fn remove(argv: &[String]) -> Result<i32> {
    let a = parse_args(argv, &[("purge", Kind::Bool)]);
    let acct = resolve_account(need(
        a.pos.first(),
        "amux account rm <provider>@<name> [--purge]",
    )?)?;
    save_accounts(
        load_accounts()?
            .into_iter()
            .filter(|x| x.id() != acct.id())
            .collect(),
    )?;
    let mut cfg = load_config()?;
    if cfg.default_for(&acct.provider) == Some(acct.name.as_str()) {
        if let Some(d) = cfg.defaults.as_mut() {
            d.remove(&acct.provider);
        }
        save_config(&cfg)?;
    }
    delete_token(&acct);
    let dir = account_dir(&acct);
    if a.has("purge") {
        // remove_dir_all unlinks symlinks without following them, so the shared home is untouched.
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        println!("Removed {} and {}.", acct.id(), tildify(&dir));
        if acct.provider == "claude" {
            println!("Its Claude Code Keychain login (\"Claude Code-credentials-…\") is left in place; run `claude auth logout` first to clear it.");
        }
    } else {
        println!(
            "Removed {}. Its private files remain in {} (use --purge to delete).",
            acct.id(),
            tildify(&dir)
        );
    }
    Ok(0)
}

pub fn account_cmd(argv: &[String]) -> Result<i32> {
    let args = argv.get(1..).unwrap_or_default();
    match argv.first().map(String::as_str) {
        Some("add") => add(args),
        Some("list" | "ls") | None => list(),
        Some("default") => set_default(args),
        Some("token") => token(args),
        Some("status") => status(args),
        Some("privacy") => privacy(args),
        Some("baseurl" | "base-url") => base_url(args),
        Some("rm" | "remove") => remove(args),
        Some(sub) => bail!("Unknown subcommand \"account {sub}\". See `amux help`."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::load_providers_from;

    #[test]
    fn validates_base_urls() {
        let specs = load_providers_from(&Default::default()).unwrap();
        let claude = specs.iter().find(|p| p.id == "claude").unwrap();
        let gemini = specs.iter().find(|p| p.id == "gemini").unwrap();
        assert_eq!(
            validate_base_url("https://gw.example.com/", claude).unwrap(),
            "https://gw.example.com"
        );
        assert_eq!(
            validate_base_url("http://localhost:8080/v1", claude).unwrap(),
            "http://localhost:8080/v1"
        );
        assert!(validate_base_url("ftp://x", claude)
            .unwrap_err()
            .to_string()
            .contains("http or https"));
        assert!(validate_base_url("not a url", claude)
            .unwrap_err()
            .to_string()
            .contains("not a valid URL"));
        assert!(validate_base_url("https://x.example.com", gemini)
            .unwrap_err()
            .to_string()
            .contains("no base-URL env var"));
    }

    #[test]
    fn checks_account_names() {
        assert!(valid_name("work"));
        assert!(valid_name("a.b_c-1"));
        assert!(!valid_name("bad name"));
        assert!(!valid_name("-x"));
        assert!(!valid_name(""));
    }
}
