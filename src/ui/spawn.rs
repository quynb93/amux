//! The spawn dialog: choose an account, type a prompt, and optionally a run name and a
//! worktree branch. Submitting builds the same `amux spawn` command you would type, so a
//! run started from the UI is indistinguishable from one started in a shell.

use crate::launch::fill_args;
use crate::store::valid_name;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Account,
    Prompt,
    Name,
    Worktree,
    Branch,
}

pub const FIELDS: [Field; 5] = [
    Field::Account,
    Field::Prompt,
    Field::Name,
    Field::Worktree,
    Field::Branch,
];

#[derive(Debug, Clone)]
pub struct SpawnForm {
    /// `provider@name` of every account.
    pub accounts: Vec<String>,
    pub account: usize,
    pub prompt: String,
    pub name: String,
    pub worktree: bool,
    pub branch: String,
    pub focus: Field,
    pub error: Option<String>,
}

impl SpawnForm {
    /// `in_repo`: the UI was started in a git repository, so a worktree is the default:
    /// agents working side by side in one checkout trip over each other.
    pub fn new(accounts: Vec<String>, preferred: Option<&str>, in_repo: bool) -> Self {
        let account = preferred
            .and_then(|p| accounts.iter().position(|a| a == p))
            .unwrap_or(0);
        SpawnForm {
            accounts,
            account,
            prompt: String::new(),
            name: String::new(),
            worktree: in_repo,
            branch: String::new(),
            focus: Field::Prompt,
            error: None,
        }
    }

    pub fn account(&self) -> Option<&str> {
        self.accounts.get(self.account).map(String::as_str)
    }

    /// Moves focus, skipping Branch while it is hidden (no worktree).
    pub fn next_field(&mut self, back: bool) {
        let fields: Vec<Field> = FIELDS
            .into_iter()
            .filter(|f| self.worktree || *f != Field::Branch)
            .collect();
        let n = fields.len();
        let i = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = fields[if back { (i + n - 1) % n } else { (i + 1) % n }];
    }

    pub fn cycle_account(&mut self, back: bool) {
        let n = self.accounts.len().max(1);
        self.account = if back {
            (self.account + n - 1) % n
        } else {
            (self.account + 1) % n
        };
    }

    /// The text being edited, if the focused field is a text field.
    /// Pasted text goes into the field being edited, on one line.
    pub fn paste(&mut self, text: &str) {
        if let Some(t) = self.text_mut() {
            t.push_str(&text.replace(['\n', '\r'], " "));
        }
    }

    pub fn text_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Field::Prompt => Some(&mut self.prompt),
            Field::Name => Some(&mut self.name),
            Field::Branch => Some(&mut self.branch),
            Field::Account | Field::Worktree => None,
        }
    }

    /// The arguments for `amux spawn`. `prompt_args` is the provider's template for
    /// starting an interactive session with a first prompt (e.g. `["-i", "{prompt}"]`).
    pub fn argv(&self, prompt_args: Option<&[String]>) -> Result<Vec<String>, String> {
        let acct = self
            .account()
            .ok_or("No accounts yet. Add one with `amux account add`.")?;
        let name = self.name.trim();
        let branch = self.branch.trim();
        if !name.is_empty() && !valid_name(name) {
            return Err("Run names use letters, digits, '.', '_' and '-'.".into());
        }
        if branch.starts_with('-') || branch.contains(char::is_whitespace) {
            return Err("That is not a valid branch name.".into());
        }
        let mut v = vec!["spawn".to_string()];
        if !name.is_empty() {
            v.extend(["--name".into(), name.into()]);
        }
        if self.worktree {
            v.push("--worktree".into());
            if !branch.is_empty() {
                v.extend(["--branch".into(), branch.into()]);
            }
        }
        v.push(acct.to_string());
        let prompt = self.prompt.trim();
        // It goes to the CLI as an argument, where it would read as an option.
        if prompt.starts_with('-') {
            return Err(
                "A prompt can't start with '-'; the CLI would read it as an option.".into(),
            );
        }
        if !prompt.is_empty() {
            match prompt_args {
                Some(t) => v.extend(fill_args(t, &[("prompt", prompt)])),
                None => v.push(prompt.to_string()),
            }
        }
        Ok(v)
    }
}

/// Deletes the last word of `s`, as Ctrl-W does in a shell.
pub fn delete_word(s: &mut String) {
    let trimmed = s.trim_end().len();
    s.truncate(trimmed);
    // After the whitespace character, whatever its width (U+3000, NBSP…).
    let cut = s
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    s.truncate(cut);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> SpawnForm {
        SpawnForm::new(
            vec!["claude@work".into(), "gemini@me".into()],
            Some("gemini@me"),
            true,
        )
    }

    #[test]
    fn builds_spawn_arguments() {
        let mut f = form();
        assert_eq!(f.account(), Some("gemini@me"));
        f.prompt = "  fix the login bug ".into();
        f.name = "login".into();
        f.branch = "fix/login".into();
        let t = vec!["-i".to_string(), "{prompt}".to_string()];
        assert_eq!(
            f.argv(Some(&t)).unwrap(),
            [
                "spawn",
                "--name",
                "login",
                "--worktree",
                "--branch",
                "fix/login",
                "gemini@me",
                "-i",
                "fix the login bug"
            ]
        );
        f.worktree = false;
        f.name.clear();
        f.prompt.clear();
        assert_eq!(f.argv(Some(&t)).unwrap(), ["spawn", "gemini@me"]);
    }

    #[test]
    fn rejects_bad_names_and_no_accounts() {
        let mut f = form();
        f.name = "a b".into();
        assert!(f.argv(None).is_err());
        f.name.clear();
        f.branch = "--force".into();
        assert!(f.argv(None).is_err());
        assert!(SpawnForm::new(vec![], None, false).argv(None).is_err());
    }

    #[test]
    fn moves_between_fields_and_accounts() {
        let mut f = form();
        assert_eq!(f.focus, Field::Prompt);
        f.next_field(true);
        assert_eq!(f.focus, Field::Account);
        f.next_field(true);
        assert_eq!(f.focus, Field::Branch);
        f.cycle_account(false);
        assert_eq!(f.account(), Some("claude@work"));
        f.cycle_account(true);
        assert_eq!(f.account(), Some("gemini@me"));
    }

    #[test]
    fn skips_the_hidden_branch_field() {
        let mut f = form();
        f.worktree = false;
        f.focus = Field::Worktree;
        f.next_field(false);
        assert_eq!(f.focus, Field::Account);
        f.next_field(true);
        assert_eq!(f.focus, Field::Worktree);
    }

    #[test]
    fn rejects_prompts_that_read_as_options() {
        let mut f = form();
        f.prompt = " --dangerous".into();
        assert!(f.argv(None).unwrap_err().contains("start with '-'"));
        f.prompt = "fix a-b".into();
        assert!(f.argv(None).is_ok());
    }

    #[test]
    fn deletes_words() {
        let mut s = "fix the  bug  ".to_string();
        delete_word(&mut s);
        assert_eq!(s, "fix the  ");
        delete_word(&mut s);
        assert_eq!(s, "fix ");
        delete_word(&mut s);
        assert_eq!(s, "");
        let mut s = "/filter a\u{3000}b".to_string();
        delete_word(&mut s);
        assert_eq!(s, "/filter a\u{3000}");
        let mut s = "x\u{a0}yz".to_string();
        delete_word(&mut s);
        assert_eq!(s, "x\u{a0}");
    }
}
