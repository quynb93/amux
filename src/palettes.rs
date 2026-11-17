//! Named color palettes (Catppuccin, Tokyo Night, Dracula, Gruvbox…), shared by `amux ui`,
//! the phone app (`amux serve`) and `amux theme`. A palette is a dozen colors; the
//! TUI's roles and the app's CSS variables are made from them, so a theme looks the same
//! everywhere. Values are from each palette's official source.

/// One palette. All colors are `#rrggbb`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub name: &'static str,
    pub dark: bool,
    /// The window's background.
    pub bg: &'static str,
    /// Raised surfaces: the selected row, cards.
    pub surface: &'static str,
    /// A field being edited.
    pub surface2: &'static str,
    pub border: &'static str,
    pub text: &'static str,
    pub muted: &'static str,
    pub accent: &'static str,
    pub green: &'static str,
    pub yellow: &'static str,
    pub red: &'static str,
    pub purple: &'static str,
}

macro_rules! palette {
    ($name:literal, $dark:literal, $bg:literal, $surface:literal, $surface2:literal, $border:literal,
     $text:literal, $muted:literal, $accent:literal, $green:literal, $yellow:literal, $red:literal,
     $purple:literal) => {
        Palette {
            name: $name,
            dark: $dark,
            bg: $bg,
            surface: $surface,
            surface2: $surface2,
            border: $border,
            text: $text,
            muted: $muted,
            accent: $accent,
            green: $green,
            yellow: $yellow,
            red: $red,
            purple: $purple,
        }
    };
}

/// Every named palette, dark ones first.
#[rustfmt::skip]
pub const ALL: [Palette; 15] = [
    //        name                dark   bg         surface    surface2   border     text       muted      accent     green      yellow     red        purple
    palette!("catppuccin-mocha", true,  "#1e1e2e", "#313244", "#45475a", "#585b70", "#cdd6f4", "#a6adc8", "#89b4fa", "#a6e3a1", "#f9e2af", "#f38ba8", "#cba6f7"),
    palette!("tokyo-night",      true,  "#1a1b26", "#292e42", "#3b4261", "#414868", "#c0caf5", "#a9b1d6", "#7aa2f7", "#9ece6a", "#e0af68", "#f7768e", "#bb9af7"),
    palette!("dracula",          true,  "#282a36", "#44475a", "#4d5066", "#6272a4", "#f8f8f2", "#bfbfd4", "#bd93f9", "#50fa7b", "#f1fa8c", "#ff5555", "#ff79c6"),
    palette!("gruvbox-dark",     true,  "#282828", "#3c3836", "#504945", "#665c54", "#ebdbb2", "#a89984", "#83a598", "#b8bb26", "#fabd2f", "#fb4934", "#d3869b"),
    palette!("nord",             true,  "#2e3440", "#3b4252", "#434c5e", "#4c566a", "#e5e9f0", "#a5adbb", "#88c0d0", "#a3be8c", "#ebcb8b", "#bf616a", "#b48ead"),
    palette!("solarized-dark",   true,  "#002b36", "#073642", "#0d4452", "#586e75", "#93a1a1", "#839496", "#268bd2", "#859900", "#b58900", "#dc322f", "#d33682"),
    palette!("rose-pine",        true,  "#191724", "#26233a", "#403d52", "#524f67", "#e0def4", "#908caa", "#c4a7e7", "#9ccfd8", "#f6c177", "#eb6f92", "#ebbcba"),
    palette!("kanagawa",         true,  "#1f1f28", "#2a2a37", "#363646", "#54546d", "#dcd7ba", "#a6a69c", "#7e9cd8", "#98bb6c", "#e6c384", "#e46876", "#957fb8"),
    palette!("everforest",       true,  "#2d353b", "#343f44", "#3d484d", "#56635f", "#d3c6aa", "#9da9a0", "#7fbbb3", "#a7c080", "#dbbc7f", "#e67e80", "#d699b6"),
    palette!("one-dark",         true,  "#282c34", "#2c313c", "#3e4451", "#4b5263", "#abb2bf", "#8b929e", "#61afef", "#98c379", "#e5c07b", "#e06c75", "#c678dd"),
    palette!("catppuccin-latte", false, "#eff1f5", "#e6e9ef", "#ccd0da", "#bcc0cc", "#4c4f69", "#6c6f85", "#1e66f5", "#40a02b", "#df8e1d", "#d20f39", "#8839ef"),
    palette!("gruvbox-light",    false, "#fbf1c7", "#ebdbb2", "#d5c4a1", "#bdae93", "#3c3836", "#665c54", "#076678", "#79740e", "#b57614", "#9d0006", "#8f3f71"),
    palette!("solarized-light",  false, "#fdf6e3", "#eee8d5", "#e4ddc8", "#93a1a1", "#586e75", "#657b83", "#268bd2", "#859900", "#b58900", "#dc322f", "#d33682"),
    palette!("rose-pine-dawn",   false, "#faf4ed", "#f2e9e1", "#dfdad9", "#cecacd", "#575279", "#797593", "#907aa9", "#286983", "#ea9d34", "#b4637a", "#d7827e"),
    palette!("github-light",     false, "#ffffff", "#f6f8fa", "#eaeef2", "#d0d7de", "#1f2328", "#59636e", "#0969da", "#1a7f37", "#9a6700", "#cf222e", "#8250df"),
];

/// The palettes behind amux's own `dark` and `light` themes, for the phone app.
const OWN: [Palette; 2] = [
    palette!(
        "dark", true, "#16161e", "#22222e", "#303a50", "#2e2e3c", "#e6e6ef", "#9a9aab", "#7aa2f7",
        "#9ece6a", "#e0af68", "#f7768e", "#c792ea"
    ),
    palette!(
        "light", false, "#ffffff", "#f2f2f7", "#e8ecf3", "#e3e3e8", "#1d1d1f", "#6e6e73",
        "#0a66ff", "#0a7d3e", "#b36b00", "#c62828", "#7c3aad"
    ),
];

pub fn get(name: &str) -> Option<&'static Palette> {
    ALL.iter().chain(OWN.iter()).find(|p| p.name == name)
}

fn rgb(hex: &str) -> (u8, u8, u8) {
    let n = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    ((n >> 16) as u8, (n >> 8) as u8, n as u8)
}

/// `a` mixed into `b`: `t` of `a`, the rest `b`.
pub fn mix(a: &str, b: &str, t: f32) -> String {
    let ((r1, g1, b1), (r2, g2, b2)) = (rgb(a), rgb(b));
    let m = |x: u8, y: u8| (x as f32 * t + y as f32 * (1.0 - t)).round() as u8;
    format!("#{:02x}{:02x}{:02x}", m(r1, r2), m(g1, g2), m(b1, b2))
}

impl Palette {
    /// The TUI's roles, as style specs (see `ui/theme.rs`).
    pub fn roles(&self) -> Vec<(&'static str, String)> {
        let p = self;
        vec![
            ("background", format!("{} on {}", p.text, p.bg)),
            ("text", p.text.into()),
            ("muted", p.muted.into()),
            ("border", p.border.into()),
            ("title", format!("{} bold", p.text)),
            ("accent", p.accent.into()),
            ("key", format!("{} bold", p.accent)),
            ("selection", format!("on {}", p.surface)),
            ("header", format!("{} bold on {}", p.bg, p.accent)),
            ("input", format!("{} on {}", p.text, p.surface2)),
            ("starting", p.muted.into()),
            ("working", p.text.into()),
            ("waiting", format!("{} bold", p.yellow)),
            ("idle", p.muted.into()),
            ("done", p.green.into()),
            ("failed", p.red.into()),
            ("lost", p.purple.into()),
            ("stopped", p.muted.into()),
            ("success", p.green.into()),
            ("error", p.red.into()),
        ]
    }

    /// The phone app's CSS variables.
    pub fn css(&self) -> String {
        let p = self;
        // The run's screen: the palette's own background when dark; a light palette
        // keeps a dark screen, since agents' colors are made for one.
        let (mono, monofg) = if p.dark {
            (mix(p.bg, "#000000", 0.85), p.text.to_string())
        } else {
            ("#1d1f21".to_string(), "#d6d6e0".to_string())
        };
        format!(
            ":root{{color-scheme:{scheme};--bg:{bg};--fg:{fg};--muted:{muted};--card:{card};--line:{line};\
             --accent:{accent};--wait:{wait};--work:{work};--bad:{bad};--add:{add};--del:{del};\
             --mono:{mono};--monofg:{monofg};}}\n",
            scheme = if p.dark { "dark" } else { "light" },
            bg = p.bg,
            fg = p.text,
            muted = p.muted,
            card = p.surface,
            line = p.border,
            accent = p.accent,
            wait = p.yellow,
            work = p.green,
            bad = p.red,
            add = mix(p.green, p.bg, 0.18),
            del = mix(p.red, p.bg, 0.18),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_are_complete_and_distinct() {
        let mut names: Vec<&str> = ALL.iter().map(|p| p.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), ALL.len());
        for p in ALL.iter().chain(OWN.iter()) {
            for c in [
                p.bg, p.surface, p.surface2, p.border, p.text, p.muted, p.accent, p.green,
                p.yellow, p.red, p.purple,
            ] {
                assert!(
                    c.len() == 7 && u32::from_str_radix(&c[1..], 16).is_ok(),
                    "{} {c}",
                    p.name
                );
            }
        }
        assert_eq!(mix("#ffffff", "#000000", 0.5), "#808080");
        let css = get("dracula").unwrap().css();
        assert!(css.contains("--bg:#282a36;") && css.contains("color-scheme:dark"));
        assert!(get("nope").is_none());
    }
}
