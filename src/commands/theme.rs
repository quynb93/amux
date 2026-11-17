//! `amux theme [<name>]`: list the themes with a swatch of each, or choose one for
//! `amux ui` and the phone app.

use crate::ui::theme::{self, Theme};
use anyhow::{bail, Result};
use std::io::IsTerminal;

/// A palette's colors for a swatch: background, accent, then the state colors.
fn swatch(name: &str) -> Option<Vec<&'static str>> {
    let p = theme::palette_of(name)?;
    Some(vec![p.bg, p.accent, p.green, p.yellow, p.red, p.purple])
}

fn current() -> String {
    theme::configured().unwrap_or_else(|| "default".into())
}

pub fn theme_cmd(argv: &[String]) -> Result<i32> {
    match argv.first().map(String::as_str) {
        None | Some("list") => list(false),
        Some("--json") => list(true),
        Some(name) if name.starts_with('-') => {
            bail!("Usage: amux theme [<name> | --json]")
        }
        Some(name) => {
            let (_, notes) = Theme::named(name).map_err(|e| anyhow::anyhow!(e))?;
            Theme::save_choice(name)?;
            println!("Theme {name}: `amux ui` and the phone app use it now.");
            for n in notes {
                println!("  note: {n}");
            }
            Ok(0)
        }
    }
}

fn list(json: bool) -> Result<i32> {
    let cur = current();
    let names = theme::available();
    if json {
        let v: Vec<serde_json::Value> = names
            .iter()
            .map(|n| {
                serde_json::json!({
                    "name": n,
                    "current": *n == cur,
                    "dark": theme::palette_of(n).map(|p| p.dark),
                    "swatch": swatch(n).unwrap_or_default(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    let color = std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let width = names.iter().map(|n| n.len()).max().unwrap_or(8);
    for n in &names {
        let mark = if *n == cur { "●" } else { "○" };
        let mut line = format!("{mark} {n:width$}  ");
        match (color, swatch(n)) {
            (true, Some(cols)) => {
                let bg = rgb(cols[0]);
                line.push_str(&format!("\x1b[48;2;{bg}m "));
                for c in &cols[1..] {
                    line.push_str(&format!("\x1b[38;2;{}m●", rgb(c)));
                }
                line.push_str(" \x1b[0m");
            }
            (_, None) => line.push_str(match n.as_str() {
                "default" => "your terminal's colors",
                "mono" => "no color",
                _ => "",
            }),
            _ => {}
        }
        println!("{}", line.trim_end());
    }
    println!("\n`amux theme <name>` chooses one (also `/theme` in amux ui, and the phone app's Monitor tab).");
    println!(
        "Your own: {}/<name>.json  {{ \"extends\": \"dracula\", \"colors\": {{ \"accent\": \"#ff9e64\" }} }}",
        crate::paths::tildify(&theme::themes_dir())
    );
    Ok(0)
}

fn rgb(hex: &str) -> String {
    let n = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    format!("{};{};{}", (n >> 16) & 255, (n >> 8) & 255, n & 255)
}
