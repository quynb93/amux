//! What the run list shows for each run: a coarse kind (for color and the header counts)
//! and a short label. Built on the same signals as `amux ps`: `Run::state()`, the host's
//! `Run.waiting`, and the age of `output.log`.

use crate::runs::{ProgramStatus, Run, State, Waiting};
use crate::time;

/// Output newer than this means the CLI is working; quiet usually means it waits for you.
pub const IDLE_AFTER_MS: i64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Starting,
    Working,
    Idle,
    /// A state the host reports that needs you (for example waiting for a permission).
    Attention,
    Done,
    Failed,
    Lost,
}

impl Kind {
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Kind::Starting | Kind::Working | Kind::Idle | Kind::Attention
        )
    }
}

pub fn last_output(run: &Run) -> Option<i64> {
    std::fs::metadata(run.dir().join("output.log"))
        .and_then(|m| m.modified())
        .ok()
        .map(time::to_ms)
}

/// Seconds, minutes, hours or days, compact: "12s", "4m", "3h", "2d".
pub fn short_age(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Classifies a run from its state, whether its host saw the CLI ask you something
/// (`Run.waiting`), what the CLI reports about itself (`Run.status`, OSC 7501), and when
/// it last printed something.
pub fn classify(
    state: &State,
    waiting: Option<&Waiting>,
    status: Option<&ProgramStatus>,
    last_output: Option<i64>,
    now: i64,
) -> (Kind, String) {
    match state {
        State::Starting => (Kind::Starting, "starting".into()),
        State::Running if waiting.is_none() && status.is_some() => {
            let s = status.expect("checked above");
            let age = short_age(now - s.since);
            match s.state.as_str() {
                "working" => (Kind::Working, "working".into()),
                // An error stops the CLI until you step in, but it is still running.
                "error" => (Kind::Attention, format!("error {age}")),
                "blocked" => (Kind::Attention, format!("waiting {age}")),
                _ => (Kind::Idle, format!("idle {age}")),
            }
        }
        State::Running => match (waiting, last_output) {
            (Some(w), _) => (
                Kind::Attention,
                format!("waiting {}", short_age(now - w.since)),
            ),
            (None, Some(t)) if now - t >= IDLE_AFTER_MS => {
                (Kind::Idle, format!("idle {}", short_age(now - t)))
            }
            _ => (Kind::Working, "working".into()),
        },
        State::Exited(0) => (Kind::Done, "done".into()),
        State::Exited(code) => (Kind::Failed, format!("exit {code}")),
        State::Failed => (Kind::Failed, "failed".into()),
        State::Lost => (Kind::Lost, "lost".into()),
    }
}

/// Runs to visit with Tab, in order of urgency: waiting for you, then failed, then
/// finished. Returns the index after `from` (wrapping) of the most urgent kind present.
pub fn next_attention(kinds: &[Kind], from: usize) -> Option<usize> {
    let n = kinds.len();
    for want in [Kind::Attention, Kind::Failed, Kind::Done, Kind::Idle] {
        if let Some(i) = (1..=n).map(|d| (from + d) % n).find(|&i| kinds[i] == want) {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_states() {
        let now = 100_000;
        let c = |st: State, w: Option<&Waiting>, t: Option<i64>| classify(&st, w, None, t, now);
        assert_eq!(c(State::Starting, None, None).0, Kind::Starting);
        assert_eq!(
            c(State::Running, None, Some(now - 1_000)),
            (Kind::Working, "working".into())
        );
        assert_eq!(c(State::Running, None, None).0, Kind::Working);
        assert_eq!(
            c(State::Running, None, Some(now - 90_000)),
            (Kind::Idle, "idle 1m".into())
        );
        let w = Waiting {
            reason: "Do you want to".into(),
            since: now - 12_000,
        };
        assert_eq!(
            c(State::Running, Some(&w), Some(now)),
            (Kind::Attention, "waiting 12s".into())
        );
        assert_eq!(c(State::Exited(0), None, None).0, Kind::Done);
        assert_eq!(
            c(State::Exited(130), None, None),
            (Kind::Failed, "exit 130".into())
        );
        assert_eq!(c(State::Failed, None, None).0, Kind::Failed);
        assert_eq!(c(State::Lost, None, None).0, Kind::Lost);
        assert!(Kind::Idle.is_live() && !Kind::Done.is_live());
    }

    #[test]
    fn believes_what_the_cli_reports() {
        let now = 100_000;
        let st = |state: &str| ProgramStatus {
            state: state.into(),
            kind: None,
            message: None,
            since: now - 3_000,
        };
        let c = |s: &ProgramStatus, out: Option<i64>| {
            classify(&State::Running, None, Some(s), out, now)
        };
        // Reported working wins over quiet output, and idle over fresh output.
        assert_eq!(
            c(&st("working"), Some(now - 60_000)),
            (Kind::Working, "working".into())
        );
        assert_eq!(c(&st("idle"), Some(now)), (Kind::Idle, "idle 3s".into()));
        assert_eq!(c(&st("done"), Some(now)), (Kind::Idle, "idle 3s".into()));
        assert_eq!(c(&st("error"), None), (Kind::Attention, "error 3s".into()));
        // Run.waiting, which the host sets from the same report, comes first.
        let w = Waiting {
            reason: "approve Bash".into(),
            since: now - 5_000,
        };
        assert_eq!(
            classify(&State::Running, Some(&w), Some(&st("blocked")), None, now),
            (Kind::Attention, "waiting 5s".into())
        );
        // A finished run is finished, whatever it last reported.
        assert_eq!(
            classify(&State::Exited(0), None, Some(&st("working")), None, now).0,
            Kind::Done
        );
    }

    #[test]
    fn tab_visits_the_most_urgent_run_next() {
        use Kind::*;
        let k = [Working, Done, Attention, Idle, Attention];
        assert_eq!(next_attention(&k, 0), Some(2));
        assert_eq!(next_attention(&k, 2), Some(4));
        assert_eq!(next_attention(&k, 4), Some(2));
        assert_eq!(next_attention(&[Working, Done], 1), Some(1));
        assert_eq!(next_attention(&[Working, Starting], 0), None);
        assert_eq!(next_attention(&[], 0), None);
    }

    #[test]
    fn ages_are_compact() {
        assert_eq!(short_age(-5), "0s");
        assert_eq!(short_age(59_999), "59s");
        assert_eq!(short_age(60_000), "1m");
        assert_eq!(short_age(7_200_000), "2h");
        assert_eq!(short_age(3 * 86_400_000), "3d");
    }
}
