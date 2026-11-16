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
        if dest.exists() {
            conflicts.push(name.clone());
            continue;
        }
        fs::rename(&src, &dest)?;
        fsx::symlink(&dest, &src)?;
        adopted.push(name.clone());
    }
    Ok((adopted, conflicts))
}
