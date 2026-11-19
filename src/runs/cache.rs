//! Runs' files as last read, re-read only when they change. `amux events` and `amux ui`
//! look at every run several times a second; with a hundred runs, parsing every
//! run.json, verify.json and pr.json each time cost a few percent of a CPU per watcher.
//! A file is parsed again only when its modification time, size or inode differ (amux
//! writes them by renaming a new file over the old one, which always changes the inode).

use super::{run_dir, Run};
use crate::work::{github::PullRequest, verify::Verify};
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

type Stamp = (SystemTime, u64, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len(), m.ino()))
}

/// Parsed files by path, with the stamp they had when parsed.
struct Files<T> {
    map: HashMap<PathBuf, (Option<Stamp>, Option<T>)>,
}

impl<T: Clone> Files<T> {
    fn new() -> Self {
        Files {
            map: HashMap::new(),
        }
    }

    /// The file's contents, parsed with `load` if it changed since last time.
    fn get(&mut self, path: PathBuf, load: impl FnOnce() -> Option<T>) -> Option<T> {
        let now = stamp(&path);
        if let Some((was, v)) = self.map.get(&path) {
            if *was == now {
                return v.clone();
            }
        }
        // Missing: nothing to parse.
        let v = now.and_then(|_| load());
        self.map.insert(path, (now, v.clone()));
        v
    }

    fn retain(&mut self, keep: impl Fn(&Path) -> bool) {
        self.map.retain(|p, _| keep(p));
    }
}

/// Every run's run.json, verify.json and pr.json, kept between looks.
pub struct RunFiles {
    runs: Files<Run>,
    verify: Files<Verify>,
    pr: Files<PullRequest>,
}

impl Default for RunFiles {
    fn default() -> Self {
        RunFiles {
            runs: Files::new(),
            verify: Files::new(),
            pr: Files::new(),
        }
    }
}

impl RunFiles {
    /// All runs, oldest first, like `runs::list`.
    pub fn list(&mut self) -> Vec<Run> {
        let Ok(rd) = std::fs::read_dir(crate::paths::runs_dir()) else {
            return vec![];
        };
        let mut dirs = std::collections::HashSet::new();
        let mut out: Vec<Run> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let id = e.file_name().to_string_lossy().into_owned();
                let dir = e.path();
                let run = self
                    .runs
                    .get(dir.join("run.json"), || super::load(&id).ok());
                dirs.insert(dir);
                run
            })
            .collect();
        out.sort_by_key(|r| (r.started_at, r.id.clone()));
        // Forget runs that are gone. A folder whose run.json can't be read stays, so it
        // isn't parsed again until it changes.
        let keep = |p: &Path| p.parent().is_some_and(|d| dirs.contains(d));
        self.runs.retain(keep);
        self.verify.retain(keep);
        self.pr.retain(keep);
        out
    }

    pub fn verify(&mut self, id: &str) -> Option<Verify> {
        self.verify.get(run_dir(id).join("verify.json"), || {
            crate::work::verify::load(id)
        })
    }

    pub fn pr(&mut self, id: &str) -> Option<PullRequest> {
        self.pr.get(run_dir(id).join("pr.json"), || {
            crate::work::github::load(id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_file_again_only_when_it_changed() {
        let dir = std::env::temp_dir().join(format!("amux-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.json");
        let mut files: Files<String> = Files::new();
        let mut loads = 0;
        let mut get = |files: &mut Files<String>| {
            files.get(f.clone(), || {
                loads += 1;
                std::fs::read_to_string(&f).ok()
            })
        };
        assert_eq!(get(&mut files), None, "missing");
        std::fs::write(&f, "one").unwrap();
        assert_eq!(get(&mut files).as_deref(), Some("one"));
        assert_eq!(get(&mut files).as_deref(), Some("one"));
        // Written the way amux writes: a new file renamed over the old one.
        std::fs::write(dir.join("x.tmp"), "two").unwrap();
        std::fs::rename(dir.join("x.tmp"), &f).unwrap();
        assert_eq!(get(&mut files).as_deref(), Some("two"));
        drop(get);
        assert_eq!(loads, 2);
        std::fs::remove_file(&f).unwrap();
        let _ = std::fs::remove_dir(&dir);
    }
}
