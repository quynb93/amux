use anyhow::{bail, Result};
use std::collections::HashMap;
use std::io::{IsTerminal, Read};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Value,
}

#[derive(Default)]
pub struct Parsed {
    pub pos: Vec<String>,
    flags: HashMap<String, Option<String>>,
}

impl Parsed {
    pub fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }
    /// The value of a value flag, if given (possibly empty).
    pub fn value(&self, name: &str) -> Option<&str> {
        self.flags.get(name).and_then(|v| v.as_deref())
    }
}

/// Splits known flags (`--x`, `--x value`, `--x=value`, `-n`) from positionals. Unknown
/// flags stay positional so they pass through to the provider CLI untouched.
pub fn parse_args(argv: &[String], known: &[(&str, Kind)]) -> Parsed {
    let mut out = Parsed::default();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        let (name, inline) = if let Some(rest) = a.strip_prefix("--") {
            match rest.split_once('=') {
                Some((n, v)) => (Some(n), Some(v.to_string())),
                None => (Some(rest), None),
            }
        } else if a == "-n" {
            (Some("n"), None)
        } else {
            (None, None)
        };
        match name.and_then(|n| known.iter().find(|(k, _)| *k == n)) {
            Some((k, Kind::Value)) => {
                let v = inline.or_else(|| {
                    i += 1;
                    argv.get(i).cloned()
                });
                out.flags.insert(k.to_string(), Some(v.unwrap_or_default()));
            }
            Some((k, Kind::Bool)) => {
                out.flags.insert(k.to_string(), None);
            }
            None => out.pos.push(a.clone()),
        }
        i += 1;
    }
    out
}

/// Returns `v`, or a usage error when it is missing or empty.
pub fn need<'a>(v: Option<&'a String>, usage: &str) -> Result<&'a str> {
    match v {
        Some(s) if !s.is_empty() => Ok(s),
        _ => bail!("Usage: {usage}"),
    }
}

fn width(s: &str) -> usize {
    s.chars().count()
}

/// `s` without what a terminal would act on rather than show: ESC and the other C0
/// controls, DEL, and the C1 controls (U+0080–U+009F, which some terminals also take as
/// escapes). Tabs and newlines stay when `keep_lines`, else they become spaces. For text
/// from outside amux (issue titles, branch names, a run's arguments, another machine's
/// answers) before it is printed.
pub fn clean(s: &str, keep_lines: bool) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\n' | '\t' if keep_lines => Some(c),
            '\n' | '\t' | '\r' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// A header row plus rows in left-aligned columns; the last column is not padded. Cells
/// are `clean`ed: one line each, with nothing a terminal would act on.
pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let header: Vec<String> = header.iter().map(|h| h.to_string()).collect();
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| clean(c, false)).collect())
        .collect();
    let all: Vec<&Vec<String>> = std::iter::once(&header).chain(&rows).collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|i| all.iter().map(|r| width(&r[i])).max().unwrap_or(0))
        .collect();
    all.iter()
        .map(|r| {
            let last = r.len() - 1;
            r.iter()
                .enumerate()
                .map(|(i, c)| {
                    if i == last {
                        c.clone()
                    } else {
                        format!("{c}{}", " ".repeat(widths[i] - width(c)))
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// JavaScript's Math.round for the non-negative values used here.
fn round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

pub fn ago(ms: i64, now: i64) -> String {
    let s = (now - ms) as f64 / 1000.0;
    if s < 3600.0 {
        format!("{}m ago", round(s / 60.0).max(1))
    } else if s < 86400.0 {
        format!("{}h ago", round(s / 3600.0))
    } else {
        format!("{}d ago", round(s / 86400.0))
    }
}

/// Single-quotes a string for POSIX shells.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Shortens to one line of at most `max` characters, without control characters.
pub fn one_line(s: &str, max: usize) -> String {
    let t = clean(s, false)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if t.chars().count() > max {
        let mut cut: String = t.chars().take(max - 1).collect();
        cut.push('…');
        cut
    } else {
        t
    }
}

/// Opens `url` in a browser: the command in `$BROWSER` if set (as `gh` and others do),
/// else `open` on macOS and `xdg-open` elsewhere.
pub fn open_url(url: &str) -> Result<()> {
    let browser = std::env::var("BROWSER")
        .ok()
        .filter(|b| !b.trim().is_empty());
    let mut words: Vec<String> = match browser {
        Some(b) => b.split_whitespace().map(str::to_string).collect(),
        None if cfg!(target_os = "macos") => vec!["open".into()],
        None => vec!["xdg-open".into()],
    };
    let bin = words.remove(0);
    let status = Command::new(&bin)
        .args(&words)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| anyhow::anyhow!("Could not run {bin} to open {url}: {e}"))?;
    if !status.success() {
        bail!("{bin} could not open {url}.");
    }
    Ok(())
}

/// Reads a secret from stdin when piped, or from the terminal with echo off.
pub fn read_secret(prompt: &str) -> Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut s = String::new();
        stdin.lock().read_to_string(&mut s)?;
        return Ok(s.trim().to_string());
    }
    eprint!("{prompt}");
    // Echo off, and -isig so Ctrl-C arrives as a byte we handle (restoring the
    // terminal) instead of a signal that would kill us with echo still off.
    let saved = Command::new("stty")
        .arg("-g")
        .stdin(Stdio::inherit())
        .output()?;
    let saved = String::from_utf8_lossy(&saved.stdout).trim().to_string();
    let restore = || {
        let _ = Command::new("stty")
            .arg(&saved)
            .stdin(Stdio::inherit())
            .status();
        eprintln!();
    };
    Command::new("stty")
        .args(["-echo", "-icanon", "-isig", "min", "1"])
        .stdin(Stdio::inherit())
        .status()?;
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    let mut lock = stdin.lock();
    loop {
        match lock.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => match byte[0] {
                b'\n' | b'\r' | 0x04 => break,
                0x03 => {
                    restore();
                    std::process::exit(130);
                }
                0x7f | 0x08 => {
                    // Backspace: drop the last UTF-8 character.
                    while let Some(b) = buf.pop() {
                        if b & 0xC0 != 0x80 {
                            break;
                        }
                    }
                }
                b => buf.push(b),
            },
            Err(e) => {
                restore();
                return Err(e.into());
            }
        }
    }
    restore();
    Ok(String::from_utf8_lossy(&buf).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_what_a_terminal_would_act_on() {
        let evil = "a\x1b]0;pwned\x07b\x1b[2Jc\u{9b}31md\x7fe\r\nf\tg";
        assert_eq!(clean(evil, false), "a]0;pwnedb[2Jc31mde  f g");
        assert_eq!(clean("x\ny\tz\x1b", true), "x\ny\tz");
        assert_eq!(clean("ünïcödé ✓", false), "ünïcödé ✓");
        let t = table(
            &["A", "B"],
            &[vec!["\x1b[31mred".into(), "two\nlines".into()]],
        );
        assert!(!t.contains('\x1b') && t.ends_with("two lines"), "{t:?}");
        assert_eq!(one_line("\x1b[1mbold\u{85}\n x", 20), "[1mbold x");
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_flags_and_keeps_unknown_ones_positional() {
        let p = parse_args(
            &s(&["a", "--x", "1", "--y=2", "--z", "--unknown", "-n", "5", "b"]),
            &[
                ("x", Kind::Value),
                ("y", Kind::Value),
                ("z", Kind::Bool),
                ("n", Kind::Value),
            ],
        );
        assert_eq!(p.pos, s(&["a", "--unknown", "b"]));
        assert_eq!(p.value("x"), Some("1"));
        assert_eq!(p.value("y"), Some("2"));
        assert!(p.has("z"));
        assert_eq!(p.value("n"), Some("5"));
    }

    #[test]
    fn formats_tables_times_and_quotes() {
        assert_eq!(
            table(&["A", "BB", "C"], &[s(&["aaa", "b", "c"])]),
            "A    BB  C\naaa  b   c"
        );
        assert_eq!(ago(0, 30_000), "1m ago");
        assert_eq!(ago(0, 2 * 3_600_000), "2h ago");
        assert_eq!(ago(0, 3 * 86_400_000), "3d ago");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(one_line("a\n  b", 80), "a b");
        assert_eq!(one_line("abcdef", 4), "abc…");
    }
}
