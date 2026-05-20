//! Repo-selection logic shared by the high-level operations.

use crate::Config;
use crate::errors::CswError;

/// Combine `default_repos`, `--repos`, and `--only` into a final list.
///
/// - If `only` is `Some`, the resulting list is exactly that (deduplicated,
///   order preserved).
/// - Otherwise the result is `default_repos` followed by `extra`,
///   deduplicated while preserving first-seen order.
///
/// Every name is validated against `cfg.repos`; an unknown name yields
/// [`CswError::UnknownRepo`].
pub fn resolve(
    cfg: &Config,
    only: Option<&[String]>,
    extra: &[String],
) -> Result<Vec<String>, CswError> {
    let candidate: Vec<String> = match only {
        Some(list) => list.to_vec(),
        None => cfg
            .default_repos
            .iter()
            .chain(extra.iter())
            .cloned()
            .collect(),
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(candidate.len());
    for name in candidate {
        if !cfg.repos.contains_key(&name) {
            return Err(CswError::UnknownRepo(name));
        }
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepoConfig;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn cfg_with(default_repos: &[&str], known: &[&str]) -> Config {
        let mut repos = BTreeMap::new();
        for n in known {
            repos.insert((*n).into(), RepoConfig::new(*n, "zed {path}"));
        }
        Config {
            base_dir: PathBuf::from("/x"),
            tasks_dir: PathBuf::from("/x/tasks"),
            username: None,
            default_repos: default_repos.iter().map(|s| (*s).into()).collect(),
            cmux: None,
            repos,
        }
    }

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).into()).collect()
    }

    #[test]
    fn defaults_used_when_no_flags() {
        let cfg = cfg_with(&["frontend"], &["frontend", "backend"]);
        let result = resolve(&cfg, None, &[]).unwrap();
        assert_eq!(result, s(&["frontend"]));
    }

    #[test]
    fn extra_appends_to_defaults() {
        let cfg = cfg_with(&["frontend"], &["frontend", "backend", "infra"]);
        let result = resolve(&cfg, None, &s(&["backend"])).unwrap();
        assert_eq!(result, s(&["frontend", "backend"]));
    }

    #[test]
    fn extra_dedupes_against_defaults() {
        let cfg = cfg_with(&["frontend"], &["frontend", "backend"]);
        let result = resolve(&cfg, None, &s(&["frontend", "backend"])).unwrap();
        assert_eq!(result, s(&["frontend", "backend"]));
    }

    #[test]
    fn only_replaces_defaults_entirely() {
        let cfg = cfg_with(&["frontend"], &["frontend", "backend"]);
        let result = resolve(&cfg, Some(&s(&["backend"])), &[]).unwrap();
        assert_eq!(result, s(&["backend"]));
    }

    #[test]
    fn only_dedupes_within_itself() {
        let cfg = cfg_with(&[], &["frontend", "backend"]);
        let result = resolve(&cfg, Some(&s(&["frontend", "backend", "frontend"])), &[]).unwrap();
        assert_eq!(result, s(&["frontend", "backend"]));
    }

    #[test]
    fn unknown_repo_in_extra_errors() {
        let cfg = cfg_with(&[], &["frontend"]);
        let err = resolve(&cfg, None, &s(&["ghost"])).unwrap_err();
        assert!(matches!(err, CswError::UnknownRepo(n) if n == "ghost"));
    }

    #[test]
    fn unknown_repo_in_only_errors() {
        let cfg = cfg_with(&[], &["frontend"]);
        let err = resolve(&cfg, Some(&s(&["ghost"])), &[]).unwrap_err();
        assert!(matches!(err, CswError::UnknownRepo(n) if n == "ghost"));
    }

    #[test]
    fn empty_resolution_when_no_defaults_no_extras() {
        let cfg = cfg_with(&[], &["frontend"]);
        assert!(resolve(&cfg, None, &[]).unwrap().is_empty());
    }
}
