//! What runs do to code and to the machine, below the CLI and the UI: sharing the machine
//! (`slots`), checking a run's work (`verify`), its pull request and CI (`github`), and
//! reviewing its changes (`review`). Nothing here prints; `commands/` and `ui/` show it.

pub mod github;
pub mod guards;
pub mod limits;
pub mod notes;
pub mod overlap;
pub mod review;
pub mod slots;
pub mod verify;
