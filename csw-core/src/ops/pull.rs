//! `csw pull` orchestration.
//!
//! Fetches each canonical clone and fast-forwards its base branch from
//! `origin`. The base branch is resolved per repo using the same precedence
//! as `csw start`: explicit `repo.base_branch` if set, else `origin/HEAD`.
//! Best-effort per-repo: a single repo's failure (wrong branch, dirty tree,
//! non-ff history, missing canonical) is recorded into the report but
//! doesn't abort siblings.

use crate::errors::CswError;
use crate::progress::Reporter;
use crate::{Config, git};
use anyhow::{Result, anyhow};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub repos: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PullSuccess {
    pub repo: String,
    pub canonical: PathBuf,
}

#[derive(Debug)]
pub struct PullReport {
    pub successes: Vec<PullSuccess>,
    pub failures: Vec<(String, anyhow::Error)>,
}

impl PullReport {
    pub fn any_failure(&self) -> bool {
        !self.failures.is_empty()
    }
}

pub fn pull(cfg: &Config, request: PullRequest, reporter: &dyn Reporter) -> Result<PullReport> {
    // Pre-flight: every requested repo must be configured. Mistyped names
    // are unrecoverable input errors, so we bail before any side effects.
    for name in &request.repos {
        if cfg.repo(name).is_none() {
            let configured = if cfg.repos.is_empty() {
                "(none)".to_string()
            } else {
                cfg.repos.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            return Err(anyhow!("unknown repo `{name}` (configured: {configured})"));
        }
    }

    let mut report = PullReport {
        successes: Vec::new(),
        failures: Vec::new(),
    };

    for name in &request.repos {
        let progress = reporter.begin(name, "pulling");
        let repo = cfg.repo(name).expect("validated above");
        let canonical = cfg.canonical_path(repo);

        let result = pull_one(&canonical, repo.base_branch.as_deref());

        match result {
            Ok(()) => {
                progress.ok("done");
                report.successes.push(PullSuccess {
                    repo: name.clone(),
                    canonical,
                });
            }
            Err(e) => {
                progress.err(&format!("{e}"));
                report.failures.push((name.clone(), e));
            }
        }
    }

    Ok(report)
}

fn pull_one(canonical: &std::path::Path, configured_base: Option<&str>) -> Result<()> {
    if !canonical.exists() {
        return Err(CswError::CanonicalMissing(canonical.to_path_buf()).into());
    }
    if !git::is_git_repo(canonical) {
        return Err(CswError::NotAGitRepo {
            path: canonical.to_path_buf(),
        }
        .into());
    }

    let base = match configured_base {
        Some(b) => b.to_string(),
        None => git::resolve_origin_head(canonical)?,
    };

    let actual = git::current_branch(canonical)?;
    if actual != base {
        return Err(CswError::WrongBranch {
            path: canonical.to_path_buf(),
            actual,
            expected: base,
        }
        .into());
    }

    let dirty = git::status_porcelain(canonical)?;
    if !dirty.is_empty() {
        let files = dirty.lines().map(|l| l.to_string()).collect();
        return Err(CswError::Dirty {
            path: canonical.to_path_buf(),
            files,
        }
        .into());
    }

    git::pull_ff_only(canonical, "origin", &base)?;
    Ok(())
}
