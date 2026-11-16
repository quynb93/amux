//! `amux skill`: the agent skill that teaches Claude Code, Codex and other agents to use
//! amux to run and check other agents. Printed, or installed into each CLI's shared home.

use crate::paths::tildify;
use crate::store::load_providers;
use anyhow::{bail, Result};

pub const SKILL: &str = include_str!("../assets/SKILL.md");

/// Where each provider looks for skills, in its shared home.
fn skill_dirs() -> Result<Vec<(String, std::path::PathBuf)>> {
    let mut out = vec![];
    for p in load_providers()? {
        let dir = match p.id.as_str() {
            "claude" => p.shared_home.join("skills/amux"),
            "codex" => p.shared_home.join("skills/amux"),
            "gemini" => p.shared_home.join("skills/amux"),
            _ => continue,
        };
        out.push((p.id.clone(), dir));
    }
    Ok(out)
}

pub fn skill_cmd(argv: &[String]) -> Result<i32> {
    match argv.first().map(String::as_str) {
        None => {
            print!("{SKILL}");
            Ok(0)
        }
        Some("--install") => {
            for (id, dir) in skill_dirs()? {
                if !dir
                    .parent()
                    .is_some_and(|h| h.parent().is_some_and(|s| s.exists()))
                {
                    continue;
                }
                std::fs::create_dir_all(&dir)?;
                let file = dir.join("SKILL.md");
                if std::fs::read_to_string(&file).is_ok_and(|s| s == SKILL) {
                    println!("{id}: up to date at {}", tildify(&file));
                    continue;
                }
                crate::fsx::write_atomic(&file, SKILL.as_bytes())?;
                println!("{id}: installed {}", tildify(&file));
            }
            println!("Every account of these CLIs sees it, since skills live in the shared home.");
            Ok(0)
        }
        Some(a) => bail!("Unknown argument {a}. Usage: amux skill [--install]"),
    }
}
