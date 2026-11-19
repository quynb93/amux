//! `amux map`: bind a folder (and everything below it) to an account per CLI, so plain
//! `amux claude` there uses that account instead of the default.

use crate::paths::{expand_home, tildify};
use crate::store::{get_provider, load_accounts, load_config, save_config};
use crate::util::{parse_args, table, Kind};
use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};
use std::path::PathBuf;

const USAGE: &str = "amux map [<dir> <provider>@<name> | --rm <dir> [<provider>]]";

pub fn map_cmd(argv: &[String]) -> Result<i32> {
    let a = parse_args(argv, &[("rm", Kind::Value)]);
    if let Some(dir) = a.value("rm") {
        return remove(dir, a.pos.first().map(String::as_str));
    }
    match a.pos.as_slice() {
        [] => list(),
        [dir, acct] => add(dir, acct),
        _ => bail!("Usage: {USAGE}"),
    }
}

/// The folder as stored: absolute, with symlinks resolved, so lookups from any spelling
/// of it (or from a symlinked path) agree.
fn canonical(dir: &str) -> Result<PathBuf> {
    let p = expand_home(dir);
    let c = p
        .canonicalize()
        .map_err(|_| anyhow!("No folder {}.", tildify(&p)))?;
    if !c.is_dir() {
        bail!("{} is not a folder.", tildify(&c));
    }
    Ok(c)
}

fn add(dir: &str, acct: &str) -> Result<i32> {
    let Some((provider, name)) = acct
        .split_once('@')
        .filter(|(p, n)| !p.is_empty() && !n.is_empty())
    else {
        bail!("Name the account in full, e.g. `amux map {dir} claude@work`.");
    };
    get_provider(provider)?;
    if !load_accounts()?
        .iter()
        .any(|a| a.provider == provider && a.name == name)
    {
        bail!("No account \"{acct}\". Add it with `amux account add {provider} {name}`.");
    }
    let folder = canonical(dir)?;
    let mut cfg = load_config()?;
    let entry = cfg
        .folders
        .get_or_insert_with(Map::new)
        .entry(folder.to_string_lossy().into_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry
        .as_object_mut()
        .expect("an object")
        .insert(provider.to_string(), Value::String(name.to_string()));
    save_config(&cfg)?;
    println!(
        "`amux {provider}` in {} and below now uses {acct}.",
        tildify(&folder)
    );
    Ok(0)
}

fn remove(dir: &str, provider: Option<&str>) -> Result<i32> {
    let mut cfg = load_config()?;
    let folders = cfg.folders.get_or_insert_with(Map::new);
    // The folder may be gone by now; then match the path as typed, made absolute and
    // without a trailing slash, as `add` stored it.
    let key = canonical(dir)
        .or_else(|_| std::path::absolute(expand_home(dir)))
        .map(|p| p.components().collect::<PathBuf>())
        .unwrap_or_else(|_| expand_home(dir))
        .to_string_lossy()
        .into_owned();
    let Some(entry) = folders.get_mut(&key) else {
        bail!(
            "{} isn't mapped. `amux map` lists the folders that are.",
            tildify(&PathBuf::from(&key))
        );
    };
    match provider {
        Some(p) => {
            let removed = entry.as_object_mut().and_then(|m| m.remove(p)).is_some();
            if !removed {
                bail!("{} has no account for {p}.", tildify(&PathBuf::from(&key)));
            }
            if entry.as_object().is_some_and(Map::is_empty) {
                folders.remove(&key);
            }
        }
        None => {
            folders.remove(&key);
        }
    }
    if folders.is_empty() {
        cfg.folders = None;
    }
    save_config(&cfg)?;
    println!("Removed the map for {}.", tildify(&PathBuf::from(&key)));
    Ok(0)
}

fn list() -> Result<i32> {
    let cfg = load_config()?;
    let mut rows: Vec<Vec<String>> = vec![];
    for (folder, m) in cfg.folders.iter().flatten() {
        for (provider, name) in m.as_object().into_iter().flatten() {
            let name = name.as_str().unwrap_or_default();
            let gone = if PathBuf::from(folder).is_dir() {
                ""
            } else {
                " (folder gone)"
            };
            rows.push(vec![
                format!("{}{gone}", tildify(&PathBuf::from(folder))),
                format!("{provider}@{name}"),
            ]);
        }
    }
    if rows.is_empty() {
        println!("No folders mapped. Map one with `amux map <dir> <provider>@<name>`.");
        return Ok(0);
    }
    rows.sort();
    println!("{}", table(&["FOLDER", "ACCOUNT"], &rows));
    Ok(0)
}

/// Drops every folder mapping to `provider@name`, for `amux account rm`.
pub fn forget_account(folders: &mut Option<Map<String, Value>>, provider: &str, name: &str) {
    let Some(f) = folders.as_mut() else { return };
    for m in f.values_mut().filter_map(Value::as_object_mut) {
        if m.get(provider).and_then(Value::as_str) == Some(name) {
            m.remove(provider);
        }
    }
    f.retain(|_, m| m.as_object().is_none_or(|m| !m.is_empty()));
    if f.is_empty() {
        *folders = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forgetting_an_account_drops_only_its_mappings() {
        let mut f = json!({"/a": {"claude": "work", "codex": "me"}, "/b": {"claude": "work"}, "/c": {"claude": "other"}})
            .as_object()
            .cloned();
        forget_account(&mut f, "claude", "work");
        assert_eq!(
            Value::Object(f.clone().unwrap()),
            json!({"/a": {"codex": "me"}, "/c": {"claude": "other"}})
        );
        forget_account(&mut f, "codex", "me");
        forget_account(&mut f, "claude", "other");
        assert!(f.is_none());
    }
}
