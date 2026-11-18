//! What runs do to code and to the machine, below the CLI and the UI: sharing the machine
//! (`slots`), checking a run's work (`verify`), its pull request and CI (`github`),
//! reviewing its changes (`review`), and issues from GitHub, Linear and Jira (`issues`,
//! `trackers`). Nothing here prints; `commands/` and `ui/` show it.

pub mod approve;
pub mod autopilot;
pub mod github;
pub mod guards;
pub mod hunks;
pub mod issues;
pub mod limits;
pub mod notes;
pub mod overlap;
pub mod quota;
pub mod review;
pub mod slots;
pub mod trackers;
pub mod verify;
