//! `csw fetch` orchestration.
//!
//! Refreshes the canonical clones of configured repositories so subsequent
//! `csw start` operations land on current state. Best-effort per-repo: a
//! single repo's failure (missing canonical, wrong path, network error)
//! is recorded into the report but doesn't abort siblings.

use crate::errors::CswError;
use crate::progress::Reporter;
use crate::{Config, git};
use anyhow::{Result, anyhow};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub repos: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FetchSuccess {
    pub repo: String,
    pub canonical: PathBuf,
}

#[derive(Debug)]
pub struct FetchReport {
    pub successes: Vec<FetchSuccess>,
    pub failures: Vec<(String, anyhow::Error)>,
}

impl FetchReport {
    pub fn any_failure(&self) -> bool {
        !self.failures.is_empty()
    }
}

pub fn fetch(cfg: &Config, request: FetchRequest, reporter: &dyn Reporter) -> Result<FetchReport> {
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

    let mut report = FetchReport {
        successes: Vec::new(),
        failures: Vec::new(),
    };

    for name in &request.repos {
        let progress = reporter.begin(name, "fetching");
        let repo = cfg.repo(name).expect("validated above");
        let canonical = cfg.canonical_path(repo);

        let result = if !canonical.exists() {
            Err(anyhow::Error::from(CswError::CanonicalMissing(
                canonical.clone(),
            )))
        } else if !git::is_git_repo(&canonical) {
            Err(anyhow::Error::from(CswError::NotAGitRepo {
                path: canonical.clone(),
            }))
        } else {
            git::fetch_prune(&canonical, "origin")
        };

        match result {
            Ok(()) => {
                progress.ok("done");
                report.successes.push(FetchSuccess {
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
