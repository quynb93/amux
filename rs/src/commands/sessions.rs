use crate::fsx;
use crate::launch::{fill_args, run};
use crate::paths::{handoff_dir, tildify};
use crate::sessions::{build_handoff, list_sessions, read_turns, SessionInfo};
use crate::store::{get_provider, load_providers, resolve_account};
use crate::time;
use crate::util::{ago, need, parse_args, table, Kind};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

fn all_sessions(provider_filter: Option<&str>) -> Result<Vec<SessionInfo>> {
    let mut out = vec![];
    for spec in load_providers()? {
        if provider_filter.is_some_and(|f| f != spec.id) {
            continue;
        }
        out.extend(list_sessions(&spec));
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    Ok(out)
}

pub fn sessions_cmd(argv: &[String]) -> Result<i32> {
    let a = parse_args(
        argv,
        &[
            ("here", Kind::Bool),
            ("n", Kind::Value),
            ("limit", Kind::Value),
        ],
    );
    let mut list = all_sessions(a.pos.first().map(String::as_str))?;
    if a.has("here") {
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        list.retain(|s| s.cwd.as_deref() == Some(cwd.as_str()));
    }
    let raw = a.value("n").or(a.value("limit")).unwrap_or("20");
    let limit = match raw.trim().parse::<f64>() {
        Ok(n) if n.fract() == 0.0 && n >= 1.0 => n as usize,
        _ => bail!("-n must be a positive whole number."),
    };
    let now = time::now_ms();
    let mut rows = vec![["PROVIDER", "SESSION", "UPDATED", "DIR", "FIRST PROMPT"]
        .map(String::from)
        .to_vec()];
    for s in list.iter().take(limit) {
        rows.push(vec![
            s.provider.clone(),
            s.id.clone(),
            ago(s.updated_at, now),
            s.cwd
                .as_deref()
                .map(|c| tildify(Path::new(c)))
                .unwrap_or_else(|| "?".into()),
            s.title.clone(),
        ]);
    }
    println!(
        "{}",
        if rows.len() > 1 {
            table(&rows)
        } else {
            "No sessions found.".into()
        }
    );
    if list.len() > limit {
        println!("… {} more (use -n)", list.len() - limit);
    }
    Ok(0)
}

fn find_session(prefix: &str) -> Result<SessionInfo> {
    let mut matches: Vec<SessionInfo> = all_sessions(None)?
        .into_iter()
        .filter(|s| s.id.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => bail!("No session matches \"{prefix}\"."),
        1 => Ok(matches.remove(0)),
        n => bail!("\"{prefix}\" matches {n} sessions; use more characters."),
    }
}

/// Keeps ids that come from transcript contents from escaping the handoff dir.
fn safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn resume_cmd(argv: &[String]) -> Result<i32> {
    let a = parse_args(argv, &[("as", Kind::Value)]);
    let session = find_session(need(
        a.pos.first(),
        "amux resume <session-id> [--as <provider>@<name>]",
    )?)?;
    let acct = resolve_account(
        a.value("as")
            .filter(|s| !s.is_empty())
            .unwrap_or(&session.provider),
    )?;
    let target = get_provider(&acct.provider)?;
    let cwd: PathBuf = match &session.cwd {
        Some(c) if Path::new(c).exists() => PathBuf::from(c),
        _ => std::env::current_dir()?,
    };
    let extra = a.pos.get(1..).unwrap_or_default();

    if acct.provider == session.provider {
        let Some(resume) = &target.resume_args else {
            bail!("{} has no resume command configured.", target.label)
        };
        eprintln!(
            "amux: resuming {} session {} as {} in {}",
            session.provider,
            session.id,
            acct.id(),
            tildify(&cwd)
        );
        let mut args = fill_args(resume, &[("id", &session.id)]);
        args.extend_from_slice(extra);
        return run(&acct, &args, Some(&cwd));
    }

    let Some(prompt_args) = &target.prompt_args else {
        bail!("{} cannot start with an initial prompt.", target.label)
    };
    let source = get_provider(&session.provider)?;
    let handoff = build_handoff(
        &session,
        &read_turns(&session, source.sessions.as_deref())?,
        60_000,
    );
    let dir = handoff_dir();
    fsx::mkdir_private(&dir)?;
    let file = dir.join(format!(
        "{}-{}-to-{}.md",
        safe(&session.provider),
        safe(&session.id),
        safe(&acct.provider)
    ));
    fsx::write_private(&file, handoff.as_bytes())?;
    eprintln!(
        "amux: handing {} session {} to {} ({})",
        session.provider,
        session.id,
        acct.id(),
        tildify(&file)
    );
    let prompt = format!(
        "Read the handoff file {} first. It contains the transcript of a previous {} session in this directory. Then continue that work.",
        file.display(),
        source.label
    );
    let mut args = fill_args(prompt_args, &[("prompt", &prompt)]);
    args.extend_from_slice(extra);
    run(&acct, &args, Some(&cwd))
}
