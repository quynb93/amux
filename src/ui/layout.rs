//! How `amux ui` arranges itself, from `"ui": { "layout": { … } }` in config.json:
//!
//!   "view":      "list" | "board" | "tiles"   what it opens with
//!   "listWidth": 40                           columns for the run list (list view)
//!   "listSide":  "left" | "right"             which side the list is on
//!   "maxTiles":  4                            live screens in the tiles view, 1 to 6
//!
//! `/layout` changes these while the UI runs, and `/layout save` keeps them.

use super::views::View;
use crate::store;
use serde_json::{json, Map, Value};

pub const MAX_TILES: usize = 6;

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub view: View,
    /// None: about 30% of the width.
    pub list_width: Option<u16>,
    pub list_right: bool,
    pub max_tiles: usize,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            view: View::List,
            list_width: None,
            list_right: false,
            max_tiles: 4,
        }
    }
}

impl Layout {
    pub fn from_config() -> Layout {
        let cfg = store::load_config().unwrap_or_default();
        Layout::parse(cfg.extra.get("ui").and_then(|u| u.get("layout")))
    }

    pub fn parse(v: Option<&Value>) -> Layout {
        let mut l = Layout::default();
        let Some(Value::Object(m)) = v else {
            return l;
        };
        if let Some(view) = m.get("view").and_then(Value::as_str).and_then(View::parse) {
            l.view = view;
        }
        l.list_width = m
            .get("listWidth")
            .and_then(Value::as_u64)
            .map(|w| w.clamp(20, 200) as u16);
        l.list_right = m.get("listSide").and_then(Value::as_str) == Some("right");
        if let Some(n) = m.get("maxTiles").and_then(Value::as_u64) {
            l.max_tiles = (n as usize).clamp(1, MAX_TILES);
        }
        l
    }

    pub fn to_json(&self) -> Value {
        let view = match self.view {
            View::List => "list",
            View::Board => "board",
            View::Tiles => "tiles",
        };
        let mut v = json!({
            "view": view,
            "listSide": if self.list_right { "right" } else { "left" },
            "maxTiles": self.max_tiles,
        });
        if let Some(w) = self.list_width {
            v["listWidth"] = json!(w);
        }
        v
    }

    /// Keeps this layout in config.json, leaving the rest of the file as it is.
    pub fn save(&self) -> anyhow::Result<()> {
        let mut cfg = store::load_config()?;
        let ui = cfg
            .extra
            .entry("ui")
            .or_insert_with(|| Value::Object(Map::new()));
        if !ui.is_object() {
            *ui = Value::Object(Map::new());
        }
        if let Some(o) = ui.as_object_mut() {
            o.insert("layout".into(), self.to_json());
        }
        store::save_config(&cfg)
    }

    /// Applies `/layout <what> [value]`. Returns what changed, or why not.
    pub fn apply(&mut self, args: &[String]) -> Result<String, String> {
        let arg = |i: usize| args.get(i).map(String::as_str);
        match (arg(0), arg(1)) {
            (None, _) => Ok(format!(
                "Layout: {} view, list {} {}, up to {} tiles. /layout save keeps it.",
                match self.view {
                    View::List => "list",
                    View::Board => "board",
                    View::Tiles => "tiles",
                },
                self.list_width
                    .map_or("≈30%".to_string(), |w| format!("{w} columns")),
                if self.list_right {
                    "on the right"
                } else {
                    "on the left"
                },
                self.max_tiles
            )),
            (Some("width"), Some(n)) => {
                let w: u16 = n
                    .parse()
                    .map_err(|_| "Width in columns, such as 40.".to_string())?;
                self.list_width = Some(w.clamp(20, 200));
                Ok(format!("List width {} columns.", w.clamp(20, 200)))
            }
            (Some("width"), None) => {
                self.list_width = None;
                Ok("List width back to automatic.".into())
            }
            (Some("side"), Some(s @ ("left" | "right"))) => {
                self.list_right = s == "right";
                Ok(format!("List on the {s}."))
            }
            (Some("tiles"), Some(n)) => {
                let n: usize = n.parse().map_err(|_| format!("Tiles: 1 to {MAX_TILES}."))?;
                self.max_tiles = n.clamp(1, MAX_TILES);
                Ok(format!("Up to {} tiles.", self.max_tiles))
            }
            (Some("reset"), _) => {
                *self = Layout::default();
                Ok("Layout back to the defaults.".into())
            }
            _ => Err("Try /layout width <n>, side left|right, tiles <n>, reset or save.".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_changes_and_round_trips() {
        let l = Layout::parse(Some(
            &json!({"view": "tiles", "listWidth": 10, "listSide": "right", "maxTiles": 9}),
        ));
        assert_eq!(l.view, View::Tiles);
        assert_eq!(l.list_width, Some(20));
        assert!(l.list_right);
        assert_eq!(l.max_tiles, MAX_TILES);
        assert_eq!(Layout::parse(Some(&l.to_json())), l);
        let mut l = Layout::default();
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(l.apply(&a(&["width", "50"])).is_ok());
        assert_eq!(l.list_width, Some(50));
        assert!(l.apply(&a(&["side", "up"])).is_err());
        assert!(l.apply(&a(&["tiles", "2"])).is_ok());
        assert_eq!(l.max_tiles, 2);
    }
}
