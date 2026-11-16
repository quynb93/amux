//! Which runs the list shows (`f`, /show) and which one Tab visits next. How a run is
//! classified lives in `runs::state`, shared with the CLI.

pub use crate::runs::state::*;

/// Which runs the list shows, by state. `f` steps through them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Show {
    #[default]
    All,
    Waiting,
    Working,
    Idle,
    /// Done, stopped, failed or lost.
    Ended,
}

impl Show {
    pub fn name(self) -> &'static str {
        match self {
            Show::All => "all",
            Show::Waiting => "waiting",
            Show::Working => "working",
            Show::Idle => "idle",
            Show::Ended => "ended",
        }
    }

    pub fn parse(s: &str) -> Option<Show> {
        Some(match s.to_ascii_lowercase().as_str() {
            "all" => Show::All,
            "waiting" | "wait" | "blocked" => Show::Waiting,
            "working" | "running" => Show::Working,
            "idle" => Show::Idle,
            "ended" | "done" | "finished" | "exited" => Show::Ended,
            _ => return None,
        })
    }

    pub fn next(self) -> Show {
        match self {
            Show::All => Show::Waiting,
            Show::Waiting => Show::Working,
            Show::Working => Show::Idle,
            Show::Idle => Show::Ended,
            Show::Ended => Show::All,
        }
    }

    pub fn matches(self, k: Kind) -> bool {
        match self {
            Show::All => true,
            Show::Waiting => k == Kind::Attention,
            Show::Working => matches!(k, Kind::Working | Kind::Starting),
            Show::Idle => k == Kind::Idle,
            Show::Ended => !k.is_live(),
        }
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
    fn shows_runs_by_state() {
        let mut s = Show::All;
        let mut seen = vec![];
        for _ in 0..5 {
            seen.push(s.name());
            s = s.next();
        }
        assert_eq!(seen, ["all", "waiting", "working", "idle", "ended"]);
        assert_eq!(s, Show::All);
        assert!(Show::Waiting.matches(Kind::Attention) && !Show::Waiting.matches(Kind::Idle));
        assert!(Show::Working.matches(Kind::Starting));
        for k in [Kind::Done, Kind::Stopped, Kind::Failed, Kind::Lost] {
            assert!(Show::Ended.matches(k) && !Show::Idle.matches(k));
        }
        assert_eq!(Show::parse("Done"), Some(Show::Ended));
        assert_eq!(Show::parse("nope"), None);
    }
}
