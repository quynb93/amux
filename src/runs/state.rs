//! A run's state as people see it: a coarse kind (for color, counts and filters) and a
//! short label, from `Run::state()`, what the host saw (`Run.waiting`), what the CLI
//! reports about itself (`Run.status`, OSC 7501), and the age of `output.log`. `amux ps`,
//! `amux explain`, `amux wait` and `amux ui` all classify runs here, so they agree.

use super::{ProgramStatus, Run, State, Waiting};
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
    /// Ended because it was stopped (SIGINT, SIGTERM or SIGKILL), not because it failed.
    Stopped,
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
        // Waiting for a run slot (`"limits"` in config.json): it starts by itself.
        State::Queued => (Kind::Starting, "queued".into()),
        State::Running if waiting.is_none() && status.is_some() => {
            let s = status.expect("checked above");
            let age = short_age(now - s.since);
            match s.state.as_str() {
                // A state hook can't say when you interrupt a turn, so a hook's "working"
                // only holds while the CLI prints.
                "working" if s.via.is_some() => match last_output {
                    Some(t) if now - t >= IDLE_AFTER_MS => {
                        (Kind::Idle, format!("idle {}", short_age(now - t)))
                    }
                    _ => (Kind::Working, "working".into()),
                },
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
        // 128 + SIGINT, SIGKILL or SIGTERM: what `amux stop` and Ctrl-C end a CLI with.
        State::Exited(130 | 137 | 143) => (Kind::Stopped, "stopped".into()),
        State::Exited(code) => (Kind::Failed, format!("exit {code}")),
        State::Failed => (Kind::Failed, "failed".into()),
        State::Lost => (Kind::Lost, "lost".into()),
    }
}

/// Classifies a run from its files as they are now.
pub fn run_kind(run: &Run, now: i64) -> (Kind, String) {
    let state = run.state();
    // Only a running run's state depends on when it last printed.
    let out = matches!(state, State::Running)
        .then(|| last_output(run))
        .flatten();
    classify(&state, run.waiting.as_ref(), run.status.as_ref(), out, now)
}

impl Kind {
    pub fn from_name(s: &str) -> Option<Kind> {
        Some(match s {
            "starting" => Kind::Starting,
            "working" => Kind::Working,
            "idle" => Kind::Idle,
            "waiting" => Kind::Attention,
            "done" => Kind::Done,
            "stopped" => Kind::Stopped,
            "failed" => Kind::Failed,
            "lost" => Kind::Lost,
            _ => return None,
        })
    }

    /// The name scripts see: in `amux events`, `amux ps --json` and `amux wait`.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Starting => "starting",
            Kind::Working => "working",
            Kind::Idle => "idle",
            Kind::Attention => "waiting",
            Kind::Done => "done",
            Kind::Stopped => "stopped",
            Kind::Failed => "failed",
            Kind::Lost => "lost",
        }
    }
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
            ..Default::default()
        };
        assert_eq!(
            c(State::Running, Some(&w), Some(now)),
            (Kind::Attention, "waiting 12s".into())
        );
        assert_eq!(c(State::Exited(0), None, None).0, Kind::Done);
        assert_eq!(
            c(State::Exited(1), None, None),
            (Kind::Failed, "exit 1".into())
        );
        for code in [130, 137, 143] {
            assert_eq!(
                c(State::Exited(code), None, None),
                (Kind::Stopped, "stopped".into())
            );
        }
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
            via: None,
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
            ..Default::default()
        };
        assert_eq!(
            classify(&State::Running, Some(&w), Some(&st("blocked")), None, now),
            (Kind::Attention, "waiting 5s".into())
        );
        // A hook's working needs output to back it.
        let hooked = ProgramStatus {
            via: Some("hook".into()),
            ..st("working")
        };
        assert_eq!(c(&hooked, Some(now - 1_000)).0, Kind::Working);
        assert_eq!(
            c(&hooked, Some(now - 60_000)),
            (Kind::Idle, "idle 1m".into())
        );
        // A finished run is finished, whatever it last reported.
        assert_eq!(
            classify(&State::Exited(0), None, Some(&st("working")), None, now).0,
            Kind::Done
        );
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
