//! Colors for `amux ui`, by role rather than by value, so a theme changes them all at once.
//!
//! Each role is a style spec in the spirit of git's color config: colors and attributes
//! separated by spaces, with `on <color>` for the background:
//!
//! ```text
//! "waiting": "#e5b454 bold"     "selection": "bold on #283044"     "muted": "dim"
//! ```
//!
//! Colors are `#rrggbb`, a 256-color index (`0`–`255`), a terminal color name (`red`,
//! `bright-blue`, …), or `default`. Attributes are `bold`, `dim`, `italic`, `underline`
//! and `reverse`.
//!
//! The theme comes from `~/.amux/config.json`:
//!
//! ```json
//! { "ui": { "theme": "dark", "colors": { "accent": "#ff9e64" } } }
//! ```
//!
//! `theme` names a built-in theme (`default`, `dark`, `light`, `mono`) or a file
//! `~/.amux/themes/<name>.json` of the form `{ "extends": "dark", "colors": { … } }`.
//! `colors` overrides single roles on top. With no theme set, `NO_COLOR` in the
//! environment picks `mono`, and a light terminal background that supports truecolor
//! (`COLORFGBG` and `COLORTERM`) picks `light`.

use crate::paths;
use crate::store::{self, valid_name};
use ratatui::style::{Color, Modifier, Style};
use serde_json::{Map, Value};
use std::path::PathBuf;

/// Every role a theme sets. Names are what `colors` uses.
pub const ROLES: [&str; 19] = [
    "text",
    "muted",
    "border",
    "title",
    "accent",
    "key",
    "selection",
    "header",
    "input",
    "starting",
    "working",
    "waiting",
    "idle",
    "done",
    "failed",
    "lost",
    "stopped",
    "success",
    "error",
];

pub const BUILT_IN: [&str; 4] = ["default", "dark", "light", "mono"];

/// Whether the terminal says its background is light: `COLORFGBG` is `fg;bg` (or
/// `fg;default;bg`), and colors 7 and 9–15 are light.
pub fn light_background(colorfgbg: Option<&str>) -> bool {
    let bg = colorfgbg.and_then(|v| v.rsplit(';').next()?.parse::<u8>().ok());
    matches!(bg, Some(7 | 9..=15))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: String,
    /// Body text: run names, the preview, dialogs.
    pub text: Style,
    /// Secondary text: details, hints, ages.
    pub muted: Style,
    pub border: Style,
    /// Pane titles.
    pub title: Style,
    /// The current selection's marker, the spinner, focused fields: the one accent.
    pub accent: Style,
    /// Key names in hints.
    pub key: Style,
    /// The selected row.
    pub selection: Style,
    /// The "amux" mark in the header.
    pub header: Style,
    /// A text field being edited.
    pub input: Style,
    pub starting: Style,
    pub working: Style,
    pub waiting: Style,
    pub idle: Style,
    pub done: Style,
    pub failed: Style,
    pub lost: Style,
    /// Ended because you stopped it.
    pub stopped: Style,
    /// Messages after an action that worked, or didn't.
    pub success: Style,
    pub error: Style,
}

impl Theme {
    fn role_mut(&mut self, role: &str) -> Option<&mut Style> {
        Some(match role {
            "text" => &mut self.text,
            "muted" => &mut self.muted,
            "border" => &mut self.border,
            "title" => &mut self.title,
            "accent" => &mut self.accent,
            "key" => &mut self.key,
            "selection" => &mut self.selection,
            "header" => &mut self.header,
            "input" => &mut self.input,
            "starting" => &mut self.starting,
            "working" => &mut self.working,
            "waiting" => &mut self.waiting,
            "idle" => &mut self.idle,
            "done" => &mut self.done,
            "failed" => &mut self.failed,
            "lost" => &mut self.lost,
            "stopped" => &mut self.stopped,
            "success" => &mut self.success,
            "error" => &mut self.error,
            _ => return None,
        })
    }

    /// Sets roles from `{ role: spec }`. Returns a note for each one it couldn't use.
    pub fn apply(&mut self, colors: &Map<String, Value>) -> Vec<String> {
        let mut bad = vec![];
        for (role, spec) in colors {
            let parsed = spec
                .as_str()
                .ok_or_else(|| "not a string".to_string())
                .and_then(parse_style);
            match (self.role_mut(role), parsed) {
                (Some(slot), Ok(style)) => *slot = style,
                (None, _) => bad.push(format!("unknown color role \"{role}\"")),
                (_, Err(e)) => bad.push(format!("color \"{role}\": {e}")),
            }
        }
        bad
    }

    /// A built-in theme by name.
    pub fn built_in(name: &str) -> Option<Theme> {
        let specs: &[(&str, &str)] = match name {
            // The terminal's own 16 colors, so it suits light and dark terminals alike.
            // Color only where it means something: the accent and the states.
            "default" => &[
                ("text", "default"),
                ("muted", "dim"),
                ("border", "dim"),
                ("title", "bold"),
                ("accent", "cyan"),
                ("key", "bold"),
                ("selection", "bold"),
                ("header", "bold reverse"),
                ("input", "underline"),
                ("starting", "dim"),
                ("working", "default"),
                ("waiting", "yellow bold"),
                ("idle", "dim"),
                ("done", "green"),
                ("failed", "red"),
                ("lost", "magenta"),
                ("stopped", "dim"),
                ("success", "green"),
                ("error", "red"),
            ],
            // Tuned for a dark background (about #1a1b26 to #282c34).
            "dark" => &[
                ("text", "#d5d9e3"),
                ("muted", "#959cb0"),
                ("border", "#4a5163"),
                ("title", "#e8ebf2 bold"),
                ("accent", "#7aa2f7"),
                ("key", "#c0caf5 bold"),
                ("selection", "on #263044"),
                ("header", "#1a1b26 bold on #7aa2f7"),
                ("input", "#e8ebf2 on #303a50"),
                ("starting", "#959cb0"),
                ("working", "#d5d9e3"),
                ("waiting", "#e8b65a bold"),
                ("idle", "#959cb0"),
                ("done", "#86c98f"),
                ("failed", "#f07178"),
                ("lost", "#c792ea"),
                ("stopped", "#959cb0"),
                ("success", "#86c98f"),
                ("error", "#f07178"),
            ],
            // Tuned for a light background (white to about #f5f5f5).
            "light" => &[
                ("text", "#24292f"),
                ("muted", "#5c6370"),
                ("border", "#b8bfcc"),
                ("title", "#1a1e24 bold"),
                ("accent", "#2f5fd0"),
                ("key", "#1a1e24 bold"),
                ("selection", "on #e2e9f8"),
                ("header", "#ffffff bold on #2f5fd0"),
                ("input", "#1a1e24 on #e8ecf3"),
                ("starting", "#5c6370"),
                ("working", "#24292f"),
                ("waiting", "#8a5a00 bold"),
                ("idle", "#5c6370"),
                ("done", "#1b6e34"),
                ("failed", "#b42318"),
                ("lost", "#7c3aad"),
                ("stopped", "#5c6370"),
                ("success", "#1b6e34"),
                ("error", "#b42318"),
            ],
            // No color: weight, dimming and reverse video only.
            "mono" => &[
                ("text", "default"),
                ("muted", "dim"),
                ("border", "dim"),
                ("title", "bold"),
                ("accent", "bold"),
                ("key", "bold"),
                ("selection", "reverse"),
                ("header", "bold reverse"),
                ("input", "underline"),
                ("starting", "dim"),
                ("working", "default"),
                ("waiting", "bold underline"),
                ("idle", "dim"),
                ("done", "default"),
                ("failed", "bold"),
                ("lost", "dim italic"),
                ("stopped", "dim"),
                ("success", "bold"),
                ("error", "bold"),
            ],
            _ => return None,
        };
        let mut t = Theme::blank(name);
        for (role, spec) in specs {
            if let (Some(slot), Ok(style)) = (t.role_mut(role), parse_style(spec)) {
                *slot = style;
            }
        }
        Some(t)
    }

    fn blank(name: &str) -> Theme {
        let s = Style::new();
        Theme {
            name: name.to_string(),
            text: s,
            muted: s,
            border: s,
            title: s,
            accent: s,
            key: s,
            selection: s,
            header: s,
            input: s,
            starting: s,
            working: s,
            waiting: s,
            idle: s,
            done: s,
            failed: s,
            lost: s,
            stopped: s,
            success: s,
            error: s,
        }
    }

    /// A theme by name: built in, or `~/.amux/themes/<name>.json`, which can extend
    /// another (up to a few levels deep). Returns notes about anything it skipped.
    pub fn named(name: &str) -> Result<(Theme, Vec<String>), String> {
        Theme::named_depth(name, 0)
    }

    fn named_depth(name: &str, depth: usize) -> Result<(Theme, Vec<String>), String> {
        if let Some(t) = Theme::built_in(name) {
            return Ok((t, vec![]));
        }
        if depth > 4 {
            return Err(format!("theme \"{name}\" extends too deeply"));
        }
        if !valid_name(name) {
            return Err(format!("\"{name}\" is not a theme name"));
        }
        let file = themes_dir().join(format!("{name}.json"));
        let text = std::fs::read_to_string(&file).map_err(|_| {
            format!(
                "no theme \"{name}\" (built in: {}; or add {})",
                BUILT_IN.join(", "),
                paths::tildify(&file)
            )
        })?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", paths::tildify(&file)))?;
        let base = v
            .get("extends")
            .and_then(Value::as_str)
            .unwrap_or("default");
        let (mut t, mut notes) = Theme::named_depth(base, depth + 1)?;
        t.name = name.to_string();
        if let Some(colors) = v.get("colors").and_then(Value::as_object) {
            notes.extend(t.apply(colors));
        }
        Ok((t, notes))
    }

    /// The theme the config asks for, with its overrides. Never fails: problems come back
    /// as notes, and the default theme stands in.
    pub fn from_config() -> (Theme, Vec<String>) {
        let (ui, bad_config) = match store::load_config() {
            Ok(c) => (c.extra.get("ui").cloned().unwrap_or(Value::Null), None),
            Err(e) => (Value::Null, Some(format!("config.json: {e:#}"))),
        };
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let truecolor = std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit");
        let light = truecolor && light_background(std::env::var("COLORFGBG").ok().as_deref());
        let (theme, mut notes) = Theme::resolve(&ui, no_color, light);
        notes.extend(bad_config);
        (theme, notes)
    }

    fn resolve(ui: &Value, no_color: bool, light: bool) -> (Theme, Vec<String>) {
        // Shapes that can't be used are reported, not silently ignored.
        let mut shape = vec![];
        if !ui.is_null() && !ui.is_object() {
            shape.push("\"ui\" in config.json should be an object".to_string());
        }
        if ui.get("theme").is_some_and(|t| !t.is_string()) {
            shape.push("\"ui.theme\" should be a theme name".to_string());
        }
        if ui.get("colors").is_some_and(|c| !c.is_object()) {
            shape.push("\"ui.colors\" should be an object of role: color".to_string());
        }
        let chosen = ui.get("theme").and_then(Value::as_str);
        let fallback = match (no_color, light) {
            (true, _) => "mono",
            (false, true) => "light",
            _ => "default",
        };
        let name = chosen.unwrap_or(fallback);
        let (mut theme, mut notes) = match Theme::named(name) {
            Ok(r) => r,
            Err(e) => (Theme::built_in("default").expect("built in"), vec![e]),
        };
        if let Some(colors) = ui.get("colors").and_then(Value::as_object) {
            notes.extend(theme.apply(colors));
        }
        notes.extend(shape);
        (theme, notes)
    }

    /// Saves `name` as the theme in config.json.
    pub fn save_choice(name: &str) -> anyhow::Result<()> {
        let mut cfg = store::load_config()?;
        let ui = cfg
            .extra
            .entry("ui")
            .or_insert_with(|| Value::Object(Map::new()));
        if !ui.is_object() {
            *ui = Value::Object(Map::new());
        }
        if let Some(o) = ui.as_object_mut() {
            o.insert("theme".into(), Value::String(name.to_string()));
        }
        store::save_config(&cfg)
    }
}

pub fn themes_dir() -> PathBuf {
    paths::AMUX_HOME.join("themes")
}

/// Built-in theme names, then the user's theme files, sorted.
pub fn available() -> Vec<String> {
    let mut v: Vec<String> = BUILT_IN.iter().map(|s| s.to_string()).collect();
    let mut user: Vec<String> = std::fs::read_dir(themes_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let n = n.strip_suffix(".json")?.to_string();
            (valid_name(&n) && !BUILT_IN.contains(&n.as_str())).then_some(n)
        })
        .collect();
    user.sort();
    v.extend(user);
    v
}

/// Parses a style spec such as `"#e5b454 bold"` or `"bold on blue"`.
pub fn parse_style(spec: &str) -> Result<Style, String> {
    let mut style = Style::new();
    let mut words = spec.split_whitespace();
    let mut fg_set = false;
    while let Some(w) = words.next() {
        let w = w.to_ascii_lowercase();
        let modifier = match w.as_str() {
            "bold" => Some(Modifier::BOLD),
            "dim" => Some(Modifier::DIM),
            "italic" => Some(Modifier::ITALIC),
            "underline" | "ul" => Some(Modifier::UNDERLINED),
            "reverse" => Some(Modifier::REVERSED),
            _ => None,
        };
        if let Some(m) = modifier {
            style = style.add_modifier(m);
        } else if w == "on" {
            let c = words.next().ok_or("\"on\" needs a color after it")?;
            style = style.bg(parse_color(c)?);
        } else if !fg_set {
            style = style.fg(parse_color(&w)?);
            fg_set = true;
        } else {
            return Err(format!(
                "unexpected \"{w}\" (two colors? use \"on\" for a background)"
            ));
        }
    }
    Ok(style)
}

fn parse_color(s: &str) -> Result<Color, String> {
    let s = s.to_ascii_lowercase();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Ok(n) = u32::from_str_radix(hex, 16) {
                return Ok(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8));
            }
        }
        return Err(format!("\"#{hex}\" is not a #rrggbb color"));
    }
    if let Ok(i) = s.parse::<u8>() {
        return Ok(Color::Indexed(i));
    }
    Ok(match s.as_str() {
        "default" | "reset" | "none" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::Gray,
        "bright-black" | "gray" | "grey" => Color::DarkGray,
        "bright-red" => Color::LightRed,
        "bright-green" => Color::LightGreen,
        "bright-yellow" => Color::LightYellow,
        "bright-blue" => Color::LightBlue,
        "bright-magenta" => Color::LightMagenta,
        "bright-cyan" => Color::LightCyan,
        "bright-white" => Color::White,
        _ => return Err(format!("unknown color \"{s}\"")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_style_specs() {
        assert_eq!(
            parse_style("#e5b454 bold").unwrap(),
            Style::new()
                .fg(Color::Rgb(0xe5, 0xb4, 0x54))
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            parse_style("bold on bright-black").unwrap(),
            Style::new()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            parse_style("244").unwrap(),
            Style::new().fg(Color::Indexed(244))
        );
        assert_eq!(parse_style("").unwrap(), Style::new());
        assert!(parse_style("#12345").is_err());
        assert!(parse_style("#+fffff").is_err());
        assert!(parse_style("red blue").unwrap_err().contains("two colors"));
        assert!(parse_style("on").is_err());
        assert!(parse_style("chartreuse").is_err());
    }

    #[test]
    fn reads_the_background_from_colorfgbg() {
        assert!(light_background(Some("0;15")));
        assert!(light_background(Some("0;default;7")));
        assert!(!light_background(Some("15;0")));
        assert!(!light_background(Some("15;8")));
        assert!(!light_background(Some("x")));
        assert!(!light_background(None));
    }

    #[test]
    fn every_built_in_theme_sets_every_role() {
        for name in BUILT_IN {
            let t = Theme::built_in(name).unwrap();
            let blank = Theme::blank(name);
            // Each built-in spec parses: a failed parse would leave that role blank.
            let mut copy = t.clone();
            for role in ROLES {
                assert!(copy.role_mut(role).is_some(), "{role}");
            }
            assert_ne!(t, blank, "{name}");
        }
        // The roles in ROLES and role_mut agree.
        assert!(Theme::blank("x").role_mut("nope").is_none());
    }

    #[test]
    fn resolves_the_configured_theme_with_overrides() {
        let (t, notes) = Theme::resolve(
            &json!({ "theme": "dark", "colors": { "accent": "#ff9e64", "bogus": "red", "done": "zz" } }),
            false,
            false,
        );
        assert_eq!(t.name, "dark");
        assert_eq!(t.accent, Style::new().fg(Color::Rgb(0xff, 0x9e, 0x64)));
        assert_eq!(notes.len(), 2, "{notes:?}");
        // NO_COLOR picks mono, unless a theme is chosen.
        assert_eq!(Theme::resolve(&Value::Null, true, false).0.name, "mono");
        assert_eq!(
            Theme::resolve(&json!({ "theme": "light" }), true, false)
                .0
                .name,
            "light"
        );
        // A light terminal picks the light theme, unless one is chosen.
        assert_eq!(Theme::resolve(&Value::Null, false, true).0.name, "light");
        assert_eq!(
            Theme::resolve(&json!({ "theme": "dark" }), false, true)
                .0
                .name,
            "dark"
        );
        // Unusable shapes are reported.
        assert_eq!(Theme::resolve(&json!("bad"), false, false).1.len(), 1);
        let (t, notes) = Theme::resolve(&json!({ "theme": 5, "colors": [1] }), false, false);
        assert_eq!((t.name.as_str(), notes.len()), ("default", 2), "{notes:?}");
        // An unknown theme falls back to the default, with a note.
        let (t, notes) = Theme::resolve(&json!({ "theme": "nope" }), false, false);
        assert_eq!(t.name, "default");
        assert!(notes[0].contains("no theme \"nope\""), "{notes:?}");
    }
}
