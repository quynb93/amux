use crate::fsx;
use crate::providers::ProviderSpec;
use anyhow::Result;
use std::fs;
use std::path::Path;

#[derive(Default, Debug)]
pub struct SyncReport {
    pub linked: Vec<String>,
    pub pruned: Vec<String>,
    /// Real files/dirs in the overlay that are not private: the CLI wrote them before they were shared.
    pub stray: Vec<String>,
}

pub fn is_private(spec: &ProviderSpec, name: &str) -> bool {
    spec.private_entries
        .iter()
        .any(|pat| match pat.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == pat,
        })
}

fn names(dir: &Path) -> Result<Vec<String>> {
    let mut out: Vec<String> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    Ok(out)
}

/// Makes `overlay` mirror the shared home with symlinks, except private entries.
/// Safe to run on every launch: it only adds missing links, repoints wrong ones and
/// removes links whose target is gone. It never deletes real files.
pub fn sync_overlay(spec: &ProviderSpec, overlay: &Path) -> Result<SyncReport> {
    let shared = &spec.shared_home;
    let mut report = SyncReport::default();

    // Owner-only when amux creates them: the shared home holds transcripts and settings.
    fsx::mkdir_private(shared)?;
    fsx::mkdir_private(overlay)?;
    for dir in &spec.shared_dirs {
        fsx::mkdir_private(&shared.join(dir))?;
    }

    for name in names(shared)? {
        if is_private(spec, &name) {
            continue;
        }
        let target = shared.join(&name);
        let link = overlay.join(&name);
        match fs::symlink_metadata(&link) {
            Err(_) => {
                fsx::symlink(&target, &link)?;
                report.linked.push(name);
            }
            Ok(st) if st.file_type().is_symlink() => {
                if fs::read_link(&link)? != target {
                    fs::remove_file(&link)?;
                    fsx::symlink(&target, &link)?;
                    report.linked.push(name);
                }
            }
            Ok(_) => report.stray.push(name),
        }
    }

    for name in names(overlay)? {
        let link = overlay.join(&name);
        let Ok(st) = fs::symlink_metadata(&link) else {
            continue;
        };
        let is_link = st.file_type().is_symlink();
        if is_link && is_private(spec, &name) && fs::read_link(&link)? == shared.join(&name) {
            // Linked before the entry became private (e.g. a provider spec update): give the account its own copy.
            fs::remove_file(&link)?;
            report.pruned.push(name);
        } else if is_link && !link.exists() {
            fs::remove_file(&link)?;
            report.pruned.push(name);
        } else if !is_link && !is_private(spec, &name) && !shared.join(&name).exists() {
            report.stray.push(name);
        }
    }
    Ok(report)
}

/// Moves stray overlay entries into the shared home (when it has no entry by that name)
/// and replaces them with links. Entries that exist in both places are returned as conflicts.
pub fn adopt_stray(
    spec: &ProviderSpec,
    overlay: &Path,
    names: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let (mut adopted, mut conflicts) = (vec![], vec![]);
    for name in names {
        let src = overlay.join(name);
        let dest = spec.shared_home.join(name);
        // symlink_metadata, so a dangling link in the shared home also counts as taken.
        if fs::symlink_metadata(&dest).is_ok() {
            conflicts.push(name.clone());
            continue;
        }
        if fs::symlink_metadata(&src)?.is_dir() {
            // rename never replaces a non-empty directory.
            fs::rename(&src, &dest)?;
        } else {
            // rename would silently replace a file the CLI created in the shared home since
            // the check above; a hard link fails instead if the name is taken.
            match fs::hard_link(&src, &dest) {
                Ok(()) => fs::remove_file(&src)?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    conflicts.push(name.clone());
                    continue;
                }
                // No hard links on this filesystem (or across devices): fall back to rename.
                Err(_) => fs::rename(&src, &dest)?,
            }
        }
        fsx::symlink(&dest, &src)?;
        adopted.push(name.clone());
    }
    Ok((adopted, conflicts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{spec as base_spec, write, TempDir};
    use std::path::PathBuf;

    struct Fx {
        _root: TempDir,
        shared: PathBuf,
        overlay: PathBuf,
        spec: ProviderSpec,
    }

    fn fx() -> Fx {
        let root = TempDir::new();
        let shared = root.path().join("shared");
        let overlay = root.path().join("overlay");
        let mut spec = base_spec("fake", &shared);
        spec.private_entries = vec!["auth.json".into(), "state*".into()];
        spec.shared_dirs = vec!["sessions".into()];
        write(&shared.join("settings.json"), "{}");
        write(&shared.join("skills/a.md"), "skill");
        write(&shared.join("auth.json"), "shared-login");
        write(&shared.join("state.db"), "x");
        Fx {
            _root: root,
            shared,
            overlay,
            spec,
        }
    }

    fn link_of(f: &Fx, name: &str) -> PathBuf {
        fs::read_link(f.overlay.join(name)).unwrap()
    }

    #[test]
    fn matches_exact_names_and_prefixes() {
        let f = fx();
        assert!(is_private(&f.spec, "auth.json"));
        assert!(is_private(&f.spec, "state.db.signature"));
        assert!(!is_private(&f.spec, "settings.json"));
    }

    #[test]
    fn links_shared_entries_and_skips_private_ones() {
        let f = fx();
        let r = sync_overlay(&f.spec, &f.overlay).unwrap();
        assert_eq!(r.linked, ["sessions", "settings.json", "skills"]);
        assert_eq!(link_of(&f, "skills"), f.shared.join("skills"));
        assert!(!f.overlay.join("auth.json").exists());
        assert!(!f.overlay.join("state.db").exists());
    }

    #[test]
    fn is_idempotent() {
        let f = fx();
        sync_overlay(&f.spec, &f.overlay).unwrap();
        let r = sync_overlay(&f.spec, &f.overlay).unwrap();
        assert!(
            r.linked.is_empty() && r.pruned.is_empty() && r.stray.is_empty(),
            "{r:?}"
        );
    }

    #[test]
    fn prunes_links_whose_target_is_gone() {
        let f = fx();
        sync_overlay(&f.spec, &f.overlay).unwrap();
        fs::remove_file(f.shared.join("settings.json")).unwrap();
        let r = sync_overlay(&f.spec, &f.overlay).unwrap();
        assert_eq!(r.pruned, ["settings.json"]);
        assert!(fs::symlink_metadata(f.overlay.join("settings.json")).is_err());
    }

    #[test]
    fn repoints_links_that_point_elsewhere() {
        let f = fx();
        fs::create_dir_all(&f.overlay).unwrap();
        fsx::symlink(Path::new("/nonexistent"), &f.overlay.join("skills")).unwrap();
        sync_overlay(&f.spec, &f.overlay).unwrap();
        assert_eq!(link_of(&f, "skills"), f.shared.join("skills"));
    }

    #[test]
    fn reports_real_files_as_stray_but_never_deletes_them() {
        let f = fx();
        write(&f.overlay.join("notes.txt"), "mine");
        write(&f.overlay.join("auth.json"), "private-login");
        let r = sync_overlay(&f.spec, &f.overlay).unwrap();
        assert_eq!(r.stray, ["notes.txt"]);
        assert_eq!(
            fs::read_to_string(f.overlay.join("notes.txt")).unwrap(),
            "mine"
        );
        assert_eq!(
            fs::read_to_string(f.overlay.join("auth.json")).unwrap(),
            "private-login"
        );
    }

    #[test]
    fn unlinks_entries_shared_before_they_became_private() {
        let f = fx();
        fs::create_dir_all(&f.overlay).unwrap();
        fsx::symlink(&f.shared.join("state.db"), &f.overlay.join("state.db")).unwrap();
        let r = sync_overlay(&f.spec, &f.overlay).unwrap();
        assert_eq!(r.pruned, ["state.db"]);
        assert!(fs::symlink_metadata(f.overlay.join("state.db")).is_err());
        assert!(f.shared.join("state.db").exists(), "shared copy is kept");
    }

    #[test]
    fn adopts_stray_entries_and_reports_conflicts() {
        let f = fx();
        write(&f.overlay.join("notes.txt"), "mine");
        write(&f.overlay.join("settings.json"), "conflict");
        let (adopted, conflicts) = adopt_stray(
            &f.spec,
            &f.overlay,
            &["notes.txt".into(), "settings.json".into()],
        )
        .unwrap();
        assert_eq!(adopted, ["notes.txt"]);
        assert_eq!(conflicts, ["settings.json"]);
        assert_eq!(
            fs::read_to_string(f.shared.join("notes.txt")).unwrap(),
            "mine"
        );
        assert_eq!(link_of(&f, "notes.txt"), f.shared.join("notes.txt"));
        assert_eq!(
            fs::read_to_string(f.overlay.join("settings.json")).unwrap(),
            "conflict"
        );
    }

    #[test]
    fn adopts_stray_dirs_and_treats_dangling_shared_links_as_taken() {
        let f = fx();
        write(&f.overlay.join("cache/a.txt"), "a");
        write(&f.overlay.join("old.txt"), "mine");
        fsx::symlink(Path::new("/nonexistent"), &f.shared.join("old.txt")).unwrap();
        let (adopted, conflicts) =
            adopt_stray(&f.spec, &f.overlay, &["cache".into(), "old.txt".into()]).unwrap();
        assert_eq!(adopted, ["cache"]);
        assert_eq!(conflicts, ["old.txt"]);
        assert_eq!(
            fs::read_to_string(f.shared.join("cache/a.txt")).unwrap(),
            "a"
        );
        assert_eq!(link_of(&f, "cache"), f.shared.join("cache"));
        assert_eq!(
            fs::read_to_string(f.overlay.join("old.txt")).unwrap(),
            "mine"
        );
    }
}
