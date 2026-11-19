//! Prompt and usage-limit text for the built-in providers, updated without a release.
//!
//! CLIs reword their permission prompts and limit messages between releases. The text
//! amux watches for (`waitingPatterns`, `limitPatterns`) is kept in `patterns.json` at
//! the root of the amux repo, and `amux update --patterns` (or the daily update check)
//! copies it to ~/.amux/patterns.json after checking it. Layers, lowest first: the
//! built-in definitions, that file, then the user's own config.json.
//!
//! These patterns only decide what a run shows (`waiting`, limited) and when you're
//! notified. Auto-approve never reads them: it parses the prompt on the screen itself
//! (work/approve.rs), so a fetched file can't make amux answer anything.

use crate::fsx;
use crate::paths;
use crate::providers;
use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};
use std::path::PathBuf;

/// The only fields the file may set.
pub const KEYS: [&str; 2] = ["waitingPatterns", "limitPatterns"];
/// Larger than any sane file; stops a bad response from filling the disk or memory.
pub const MAX_BYTES: usize = 64 * 1024;
const MAX_PER_LIST: usize = 32;
const MIN_LEN: usize = 4;
const MAX_LEN: usize = 200;

pub fn file() -> PathBuf {
    paths::AMUX_HOME.join("patterns.json")
}

fn builtin_ids() -> Vec<String> {
    providers::builtin()
        .iter()
        .filter_map(|p| p.get("id")?.as_str().map(str::to_string))
        .collect()
}

/// Checks a patterns file and returns what to keep: provider id → its fields. Fails on
/// anything malformed: too large, not JSON, a field other than `KEYS`, or a pattern that
/// isn't plain text of a sensible length. Patterns are matched as text, case-insensitively,
/// not as regular expressions, so "valid" means non-empty, one line, and no control
/// characters. Providers this amux doesn't have built in are skipped (a newer file may
/// add one) and listed in the second value.
pub fn validate(bytes: &[u8]) -> Result<(Map<String, Value>, Vec<String>)> {
    if bytes.len() > MAX_BYTES {
        bail!("it is larger than {} KB", MAX_BYTES / 1024);
    }
    let v: Value = serde_json::from_slice(bytes).map_err(|e| anyhow!("it is not JSON: {e}"))?;
    let Value::Object(top) = v else {
        bail!("it is not a JSON object");
    };
    let known = builtin_ids();
    let mut keep = Map::new();
    let mut skipped = vec![];
    for (id, fields) in top {
        let Value::Object(fields) = fields else {
            bail!("\"{id}\" is not an object");
        };
        if !known.contains(&id) {
            skipped.push(id);
            continue;
        }
        for (key, list) in &fields {
            if !KEYS.contains(&key.as_str()) {
                bail!(
                    "\"{id}\" has \"{key}\"; only {} are allowed",
                    KEYS.join(" and ")
                );
            }
            let Value::Array(list) = list else {
                bail!("\"{id}\".{key} is not a list");
            };
            if list.is_empty() {
                // An empty list would quietly turn the detection off.
                bail!("\"{id}\".{key} has no patterns");
            }
            if list.len() > MAX_PER_LIST {
                bail!("\"{id}\".{key} has more than {MAX_PER_LIST} patterns");
            }
            for p in list {
                let Some(s) = p.as_str() else {
                    bail!("\"{id}\".{key} has a pattern that is not text");
                };
                let n = s.trim().chars().count();
                if !(MIN_LEN..=MAX_LEN).contains(&n) || s.chars().any(char::is_control) {
                    bail!(
                        "\"{id}\".{key} has {s:?}: patterns are one line of {MIN_LEN} to {MAX_LEN} characters"
                    );
                }
            }
        }
        keep.insert(id, Value::Object(fields));
    }
    Ok((keep, skipped))
}

/// The saved file, if there is one and it still checks out (it may have been edited by
/// hand). Anything wrong with it means the built-in patterns are used.
pub fn load() -> Map<String, Value> {
    std::fs::read(file())
        .ok()
        .and_then(|b| validate(&b).ok())
        .map(|(m, _)| m)
        .unwrap_or_default()
}

/// Puts the fetched fields over the built-in providers' own, field by field.
pub fn apply(raw: &mut [Map<String, Value>], fetched: &Map<String, Value>) {
    for p in raw {
        let Some(id) = p.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(Value::Object(fields)) = fetched.get(id) {
            for (k, v) in fields {
                p.insert(k.clone(), v.clone());
            }
        }
    }
}

/// What a refresh did.
pub struct Refreshed {
    pub providers: Vec<String>,
    pub skipped: Vec<String>,
    pub changed: bool,
}

/// Downloads patterns.json from the amux repo, checks it, and saves it. A file that
/// doesn't check out is never saved, so the last good one (or the built-ins) stays.
pub fn refresh() -> Result<Refreshed> {
    let url = crate::update::raw_url("main/patterns.json");
    let body = fetch(&url)?;
    let (keep, skipped) = validate(&body).map_err(|e| anyhow!("{url} was not used: {e:#}"))?;
    let text = serde_json::to_string_pretty(&Value::Object(keep.clone()))? + "\n";
    let changed = std::fs::read(file()).ok().as_deref() != Some(text.as_bytes());
    if changed {
        fsx::mkdir_private(&paths::AMUX_HOME)?;
        fsx::write_atomic(&file(), text.as_bytes())?;
    }
    Ok(Refreshed {
        providers: keep.keys().cloned().collect(),
        skipped,
        changed,
    })
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let r = ureq::get(url)
        .set("User-Agent", &format!("amux/{}", crate::update::version()))
        .timeout(std::time::Duration::from_secs(5))
        .call()
        .map_err(|e| anyhow!("could not download {url}: {e}"))?;
    let mut body = Vec::new();
    // One byte past the cap is enough to know it's too large.
    r.into_reader()
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut body)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(v: Value) -> Result<(Map<String, Value>, Vec<String>)> {
        validate(v.to_string().as_bytes())
    }

    #[test]
    fn the_repo_file_matches_the_builtins_and_checks_out() {
        let text = include_str!("../patterns.json");
        let (file, skipped) = validate(text.as_bytes()).unwrap();
        assert!(skipped.is_empty());
        for p in providers::builtin() {
            let id = p["id"].as_str().unwrap();
            for k in KEYS {
                let builtin = p.get(k).cloned().unwrap_or(json!([]));
                let fetched = file[id].get(k).cloned().unwrap_or(json!([]));
                assert_eq!(builtin, fetched, "patterns.json {id}.{k} is out of date");
            }
        }
    }

    #[test]
    fn accepts_only_pattern_fields_for_known_providers() {
        let (m, skipped) = check(json!({
            "claude": {"waitingPatterns": ["Do you want to"]},
            "future-cli": {"waitingPatterns": ["Proceed?"]}
        }))
        .unwrap();
        assert_eq!(m.keys().collect::<Vec<_>>(), ["claude"]);
        assert_eq!(skipped, ["future-cli"]);
        for (bad, why) in [
            (
                json!({"claude": {"bin": "/tmp/evil"}}),
                "only waitingPatterns",
            ),
            (json!({"claude": {"waitingPatterns": "x"}}), "not a list"),
            (json!({"claude": {"limitPatterns": []}}), "no patterns"),
            (json!({"claude": {"waitingPatterns": [1]}}), "not text"),
            (json!({"claude": {"waitingPatterns": ["a"]}}), "one line of"),
            (
                json!({"claude": {"waitingPatterns": ["ok\nno"]}}),
                "one line of",
            ),
            (json!({"claude": "x"}), "not an object"),
            (json!([1]), "not a JSON object"),
        ] {
            let e = check(bad).unwrap_err().to_string();
            assert!(e.contains(why), "{e}");
        }
        assert!(validate(b"{ nope")
            .unwrap_err()
            .to_string()
            .contains("not JSON"));
        let big = format!("{{\"x\": \"{}\"}}", "a".repeat(MAX_BYTES));
        assert!(validate(big.as_bytes())
            .unwrap_err()
            .to_string()
            .contains("larger than"));
    }

    #[test]
    fn fetched_patterns_sit_between_builtins_and_config() {
        let fetched = check(json!({"claude": {"waitingPatterns": ["Fetched prompt"]}}))
            .unwrap()
            .0;
        let p = crate::store::load_providers_with(&Default::default(), &fetched).unwrap();
        let claude = p.iter().find(|s| s.id == "claude").unwrap();
        assert_eq!(claude.waiting_patterns, ["Fetched prompt"]);
        assert!(
            claude
                .limit_patterns
                .contains(&"You've hit your limit".to_string()),
            "fields it doesn't set stay built in"
        );
        let cfg: crate::store::Config = serde_json::from_value(json!({"providers": [
            {"id": "claude", "waitingPatterns": ["Mine"]}
        ]}))
        .unwrap();
        let p = crate::store::load_providers_with(&cfg, &fetched).unwrap();
        assert_eq!(p[0].waiting_patterns, ["Mine"], "config.json wins");
    }
}
