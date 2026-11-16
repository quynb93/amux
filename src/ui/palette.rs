//! The command palette: `/` opens it, you type a command (fuzzy), and its argument
//! completes from your runs, accounts or themes. Every command also says which single key
//! does the same thing, so the palette teaches the shortcuts.

/// What a command's argument is, for completion and defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arg {
    None,
    /// A run; the selected one when left out.
    Run,
    /// An account, `provider@name`. Required.
    Account,
    /// A theme name; lists the themes when left out.
    Theme,
    /// Free text, optional.
    Text,
}

pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub arg: Arg,
    /// How to call it, after the name: `<run>`, `<provider> <name>`…
    pub usage: &'static str,
    pub summary: &'static str,
    /// The key that does the same in the run list, if any.
    pub key: &'static str,
}

pub const COMMANDS: &[Command] = &[
    Command {
        name: "new",
        aliases: &["spawn", "run"],
        arg: Arg::Text,
        usage: "[account] [prompt]",
        summary: "Start a new run",
        key: "n",
    },
    Command {
        name: "attach",
        aliases: &["open"],
        arg: Arg::Run,
        usage: "[run]",
        summary: "Work in a run, full screen (Ctrl-] comes back)",
        key: "⏎",
    },
    Command {
        name: "stop",
        aliases: &[],
        arg: Arg::Run,
        usage: "[run]",
        summary: "Stop a run (SIGTERM)",
        key: "s",
    },
    Command {
        name: "kill",
        aliases: &[],
        arg: Arg::Run,
        usage: "[run]",
        summary: "Kill a run (SIGKILL)",
        key: "S",
    },
    Command {
        name: "rm",
        aliases: &["remove", "delete"],
        arg: Arg::Run,
        usage: "[run]",
        summary: "Remove a finished run and its clean worktree",
        key: "d",
    },
    Command {
        name: "clean",
        aliases: &["prune"],
        arg: Arg::None,
        usage: "",
        summary: "Remove every finished run",
        key: "",
    },
    Command {
        name: "logs",
        aliases: &["log"],
        arg: Arg::Run,
        usage: "[run]",
        summary: "Read everything a run printed, in your pager",
        key: "",
    },
    Command {
        name: "filter",
        aliases: &["find", "search"],
        arg: Arg::Text,
        usage: "[text]",
        summary: "Show only runs that match; no text shows all",
        key: "",
    },
    Command {
        name: "switch-account",
        aliases: &["account", "use"],
        arg: Arg::Account,
        usage: "<provider@name>",
        summary: "Make an account the default for its CLI and for new runs",
        key: "",
    },
    Command {
        name: "add-account",
        aliases: &["login"],
        arg: Arg::Text,
        usage: "<provider> <name> [--token]",
        summary: "Add an account and sign in to it",
        key: "",
    },
    Command {
        name: "accounts",
        aliases: &[],
        arg: Arg::None,
        usage: "",
        summary: "List accounts and their sign-in status",
        key: "",
    },
    Command {
        name: "theme",
        aliases: &["themes", "colors"],
        arg: Arg::Theme,
        usage: "[name]",
        summary: "Change colors (saved in config.json)",
        key: "",
    },
    Command {
        name: "doctor",
        aliases: &[],
        arg: Arg::None,
        usage: "",
        summary: "Check CLIs, shared homes and accounts",
        key: "",
    },
    Command {
        name: "help",
        aliases: &["keys", "?"],
        arg: Arg::None,
        usage: "",
        summary: "Keys and commands",
        key: "?",
    },
    Command {
        name: "quit",
        aliases: &["exit", "q"],
        arg: Arg::None,
        usage: "",
        summary: "Leave amux ui; runs keep going",
        key: "q",
    },
];

/// What the palette can complete from.
#[derive(Default)]
pub struct Ctx {
    pub runs: Vec<String>,
    pub accounts: Vec<String>,
    pub themes: Vec<String>,
}

/// One line in the palette's list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    /// For a command: how to call it, after its name.
    pub usage: String,
    pub detail: String,
    pub key: String,
    /// The input after choosing this item.
    pub fill: String,
    /// Choosing it runs at once; otherwise the palette waits for the argument.
    pub complete: bool,
}

/// What the user asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    New {
        account: Option<String>,
        prompt: String,
    },
    Attach(Option<String>),
    Stop(Option<String>),
    Kill(Option<String>),
    Remove(Option<String>),
    Clean,
    Logs(Option<String>),
    Filter(String),
    SwitchAccount(String),
    AddAccount(Vec<String>),
    Accounts,
    Theme(Option<String>),
    Doctor,
    Help,
    Quit,
}

pub fn find(name: &str) -> Option<&'static Command> {
    let name = name.to_ascii_lowercase();
    COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name.as_str()))
}

/// How well `query` matches `name`: prefix beats substring beats letters in order.
/// None for no match. Lower is better.
fn score(query: &str, name: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    if name.starts_with(query) {
        return Some(0);
    }
    if let Some(i) = name.find(query) {
        return Some(1 + i);
    }
    let mut chars = name.chars();
    let in_order = query.chars().all(|q| chars.any(|c| c == q));
    in_order.then_some(100 + name.len())
}

/// A match on the command's own name beats one on an alias.
fn command_score(query: &str, c: &Command) -> Option<usize> {
    let alias = c.aliases.iter().filter_map(|n| score(query, n)).min();
    match (score(query, c.name), alias) {
        (Some(n), Some(a)) => Some((n * 2).min(a * 2 + 1)),
        (n, a) => n.map(|n| n * 2).or(a.map(|a| a * 2 + 1)),
    }
}

/// Splits `/name rest` into the command word and the rest (None before any space).
fn split(input: &str) -> (&str, Option<&str>) {
    let s = input.trim_start().trim_start_matches('/');
    match s.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, Some(rest.trim_start())),
        None => (s, None),
    }
}

/// The list to show for `input`.
pub fn items(input: &str, ctx: &Ctx) -> Vec<Item> {
    let (word, rest) = split(input);
    let Some(rest) = rest else {
        let query = word.to_ascii_lowercase();
        let mut found: Vec<(usize, usize, &Command)> = COMMANDS
            .iter()
            .enumerate()
            .filter_map(|(i, c)| Some((command_score(&query, c)?, i, c)))
            .collect();
        found.sort_by_key(|(s, i, _)| (*s, *i));
        return found
            .into_iter()
            .map(|(_, _, c)| {
                let complete = matches!(c.arg, Arg::None | Arg::Run | Arg::Theme)
                    || (c.arg == Arg::Text && c.name != "add-account");
                Item {
                    label: format!("/{}", c.name),
                    usage: c.usage.to_string(),
                    detail: c.summary.to_string(),
                    key: c.key.to_string(),
                    fill: if complete {
                        format!("/{}", c.name)
                    } else {
                        format!("/{} ", c.name)
                    },
                    complete,
                }
            })
            .collect();
    };
    let Some(c) = find(word) else {
        return vec![];
    };
    let choices: &[String] = match c.arg {
        Arg::Run => &ctx.runs,
        Arg::Account => &ctx.accounts,
        Arg::Theme => &ctx.themes,
        // `new` takes an account first.
        Arg::Text if c.name == "new" && !rest.contains(' ') => &ctx.accounts,
        _ => &[],
    };
    let q = rest.to_ascii_lowercase();
    let mut found: Vec<(usize, &String)> = choices
        .iter()
        .filter_map(|v| Some((score(&q, &v.to_ascii_lowercase())?, v)))
        .collect();
    found.sort_by_key(|(s, _)| *s);
    let mut out: Vec<Item> = found
        .into_iter()
        .map(|(_, v)| Item {
            label: v.clone(),
            usage: String::new(),
            detail: String::new(),
            key: String::new(),
            fill: if c.name == "new" {
                format!("/{} {v} ", c.name)
            } else {
                format!("/{} {v}", c.name)
            },
            complete: c.name != "new",
        })
        .collect();
    if out.is_empty() {
        // Nothing to pick: one line that shows how to call it.
        out.push(Item {
            label: format!("/{} {}", c.name, c.usage),
            usage: String::new(),
            detail: c.summary.to_string(),
            key: c.key.to_string(),
            fill: input.to_string(),
            complete: true,
        });
    }
    out
}

/// Parses a palette line, such as `/stop claude-1` or `/switch-account codex@work`.
pub fn parse(input: &str) -> Result<Action, String> {
    let (word, rest) = split(input);
    if word.is_empty() {
        return Err("Type a command, like /new or /help.".into());
    }
    let c = find(word).ok_or_else(|| format!("No command \"/{word}\". Try /help."))?;
    let rest = rest.unwrap_or("").trim();
    let one = || -> Option<String> { rest.split_whitespace().next().map(str::to_string) };
    let usage = || format!("Usage: /{} {}", c.name, c.usage);
    Ok(match c.name {
        "new" => {
            // The first word is the account when it looks like one (`provider@name`).
            let (first, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if first.contains('@') {
                Action::New {
                    account: Some(first.to_string()),
                    prompt: after.trim().to_string(),
                }
            } else {
                Action::New {
                    account: None,
                    prompt: rest.to_string(),
                }
            }
        }
        "attach" => Action::Attach(one()),
        "stop" => Action::Stop(one()),
        "kill" => Action::Kill(one()),
        "rm" => Action::Remove(one()),
        "clean" => Action::Clean,
        "logs" => Action::Logs(one()),
        "filter" => Action::Filter(rest.to_string()),
        "switch-account" => Action::SwitchAccount(one().ok_or_else(usage)?),
        "add-account" => {
            let args: Vec<String> = rest.split_whitespace().map(str::to_string).collect();
            if args.iter().filter(|a| !a.starts_with('-')).count() < 2 {
                return Err(usage());
            }
            Action::AddAccount(args)
        }
        "accounts" => Action::Accounts,
        "theme" => Action::Theme(one()),
        "doctor" => Action::Doctor,
        "help" => Action::Help,
        "quit" => Action::Quit,
        _ => return Err(usage()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            runs: vec!["claude-1".into(), "codex-1".into(), "review".into()],
            accounts: vec!["claude@work".into(), "codex@me".into()],
            themes: vec!["default".into(), "dark".into(), "light".into()],
        }
    }

    fn labels(input: &str) -> Vec<String> {
        items(input, &ctx()).into_iter().map(|i| i.label).collect()
    }

    #[test]
    fn lists_and_ranks_commands() {
        assert_eq!(labels("/").len(), COMMANDS.len());
        // Prefix first, aliases count, letters in order last.
        assert_eq!(labels("/st")[0], "/stop");
        // Its own name first: /stop before /new (whose alias is "spawn").
        assert_eq!(labels("/s")[0], "/stop");
        assert_eq!(labels("/login"), ["/add-account"]);
        assert_eq!(labels("/swa")[0], "/switch-account");
        assert!(labels("/zzz").is_empty());
    }

    #[test]
    fn completes_arguments() {
        assert_eq!(labels("/stop co"), ["codex-1"]);
        assert_eq!(labels("/switch-account w")[0], "claude@work");
        assert_eq!(labels("/theme l")[0], "light");
        let pick = &items("/switch-account cod", &ctx())[0];
        assert_eq!(pick.fill, "/switch-account codex@me");
        assert!(pick.complete);
        // new: an account first, then free text.
        let pick = &items("/new cl", &ctx())[0];
        assert_eq!(
            (pick.fill.as_str(), pick.complete),
            ("/new claude@work ", false)
        );
        // Nothing to complete: the usage line.
        assert_eq!(
            labels("/add-account x"),
            ["/add-account <provider> <name> [--token]"]
        );
        // A command that needs its argument waits for it.
        let add = &items("/add", &ctx())[0];
        assert_eq!((add.fill.as_str(), add.complete), ("/add-account ", false));
        assert!(items("/doctor", &ctx())[0].complete);
    }

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse("/stop claude-1"),
            Ok(Action::Stop(Some("claude-1".into())))
        );
        assert_eq!(parse("/stop"), Ok(Action::Stop(None)));
        assert_eq!(parse("/log"), Ok(Action::Logs(None)));
        assert_eq!(
            parse("/use codex@me"),
            Ok(Action::SwitchAccount("codex@me".into()))
        );
        assert!(parse("/switch-account").unwrap_err().contains("Usage"));
        assert_eq!(
            parse("/add-account claude work --token"),
            Ok(Action::AddAccount(vec![
                "claude".into(),
                "work".into(),
                "--token".into()
            ]))
        );
        assert!(parse("/add-account claude").is_err());
        assert_eq!(
            parse("/new claude@work fix the login bug"),
            Ok(Action::New {
                account: Some("claude@work".into()),
                prompt: "fix the login bug".into()
            })
        );
        assert_eq!(
            parse("/new fix the bug"),
            Ok(Action::New {
                account: None,
                prompt: "fix the bug".into()
            })
        );
        assert_eq!(
            parse("/new codex@me"),
            Ok(Action::New {
                account: Some("codex@me".into()),
                prompt: String::new()
            })
        );
        assert_eq!(parse("/filter  login "), Ok(Action::Filter("login".into())));
        assert_eq!(parse("/theme"), Ok(Action::Theme(None)));
        assert_eq!(parse("/q"), Ok(Action::Quit));
        assert!(parse("/bogus").unwrap_err().contains("No command"));
        assert!(parse("/").is_err());
    }
}
