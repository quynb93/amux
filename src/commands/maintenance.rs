use crate::launch::prepare;
use crate::overlay::{adopt_stray, sync_overlay};
use crate::paths::tildify;
use crate::store::{
    get_provider, home_env_value, load_accounts, load_providers, overlay_home, resolve_account,
};
use crate::util::{need, parse_args, sh_quote, table, Kind};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn providers_cmd() -> Result<i32> {
    let mut rows = vec![];
    for p in load_providers()? {
        rows.push(vec![
            p.id.clone(),
            p.label.clone(),
            tildify(&p.shared_home),
            p.home_env.clone(),
            p.private_entries.join(", "),
        ]);
    }
    let header = [
        "PROVIDER",
        "NAME",
        "SHARED HOME",
        "HOME ENV",
        "PRIVATE PER ACCOUNT",
    ];
    println!("{}", table(&header, &rows));
    Ok(0)
}

pub fn env_cmd(argv: &[String]) -> Result<i32> {
    let acct = resolve_account(need(argv.first(), "amux env <provider>@<name>")?)?;
    let (_, env) = prepare(&acct)?;
    for (k, v) in &env.set {
        println!("export {k}={}", sh_quote(v));
    }
    // Clear vars that would otherwise keep pointing the eval'ing shell at another account.
    for k in &env.remove {
        if env.get(k).is_none() {
            println!("unset {k}");
        }
    }
    Ok(0)
}

pub fn sync_cmd(argv: &[String]) -> Result<i32> {
    let a = parse_args(argv, &[("adopt", Kind::Bool)]);
    let targets = match a.pos.first() {
        Some(r) => vec![resolve_account(r)?],
        None => load_accounts()?,
    };
    for acct in targets {
        let spec = get_provider(&acct.provider)?;
        let home = overlay_home(&spec, &acct);
        let r = sync_overlay(&spec, &home)?;
        let mut parts = vec![
            format!("{} linked", r.linked.len()),
            format!("{} pruned", r.pruned.len()),
        ];
        if !r.stray.is_empty() && a.has("adopt") {
            let (adopted, conflicts) = adopt_stray(&spec, &home, &r.stray)?;
            if !adopted.is_empty() {
                parts.push(format!("adopted: {}", adopted.join(", ")));
            }
            if !conflicts.is_empty() {
                parts.push(format!(
                    "exists in both (merge by hand): {}",
                    conflicts.join(", ")
                ));
            }
        } else if !r.stray.is_empty() {
            parts.push(format!("unshared: {}", r.stray.join(", ")));
        }
        println!("{}: {}", acct.id(), parts.join("; "));
    }
    Ok(0)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Where `bin` resolves on PATH, like `command -v`.
pub fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') || bin.contains(std::path::MAIN_SEPARATOR) {
        let p = PathBuf::from(bin);
        return is_executable(&p).then_some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(bin))
        .find(|p| is_executable(p))
}

pub fn doctor_cmd() -> Result<i32> {
    let mut problems = 0;
    let mut check = |ok: bool, msg: String| {
        println!("{} {msg}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            problems += 1;
        }
    };
    let providers = load_providers()?;
    let accounts = load_accounts()?;
    let builtins: Vec<String> = crate::providers::builtin()
        .iter()
        .filter_map(|b| b["id"].as_str().map(str::to_string))
        .collect();
    for p in providers {
        let found = which(&p.bin);
        // A built-in CLI you don't use isn't a problem: say so, and move on. A provider
        // you defined yourself is one you meant to use, so its missing binary fails below.
        if found.is_none()
            && builtins.contains(&p.id)
            && !accounts.iter().any(|a| a.provider == p.id)
        {
            println!(
                "--   {}: binary \"{}\" not installed (no accounts)",
                p.id, p.bin
            );
            continue;
        }
        let at = found
            .as_ref()
            .map(|f| format!("at {}", tildify(f)))
            .unwrap_or_else(|| "not found".into());
        check(
            found.is_some(),
            format!("{}: binary \"{}\" {at}", p.id, p.bin),
        );
        check(
            p.shared_home.exists(),
            format!("{}: shared home {}", p.id, tildify(&p.shared_home)),
        );
    }
    for a in &accounts {
        let Some(spec) = providers.iter().find(|p| p.id == a.provider) else {
            check(
                false,
                format!("{}: provider \"{}\" is not defined", a.id(), a.provider),
            );
            continue;
        };
        let home = overlay_home(spec, a);
        check(
            home.exists(),
            format!(
                "{}: overlay {} ({}={})",
                a.id(),
                tildify(&home),
                spec.home_env,
                tildify(&home_env_value(spec, a))
            ),
        );
    }
    let mut home_envs: Vec<&str> = providers.iter().map(|p| p.home_env.as_str()).collect();
    home_envs.sort_unstable();
    home_envs.dedup();
    for k in home_envs {
        if let Some(v) = std::env::var_os(k).filter(|v| !v.is_empty()) {
            println!(
                "note {k} is set in this shell ({}); amux overrides it per account",
                tildify(Path::new(&v))
            );
        }
    }
    Ok(if problems > 0 { 1 } else { 0 })
}
