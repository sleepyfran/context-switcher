//! Username resolution.
//!
//! Resolution order:
//! 1. Explicit `username` field in [`Config`].
//! 2. `git config user.email` with the `@domain` stripped.
//! 3. `$USER` (POSIX) or `$USERNAME` (Windows-style).

use crate::Config;
use crate::errors::CswError;
use std::process::Command;

/// Trait abstraction for testability — production uses [`SystemEnv`].
pub trait Env {
    fn git_user_email(&self) -> Option<String>;
    fn user_var(&self) -> Option<String>;
}

pub struct SystemEnv;

impl Env for SystemEnv {
    fn git_user_email(&self) -> Option<String> {
        let out = Command::new("git")
            .args(["config", "--global", "--get", "user.email"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8(out.stdout).ok()?;
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn user_var(&self) -> Option<String> {
        std::env::var("USER")
            .ok()
            .or_else(|| std::env::var("USERNAME").ok())
            .filter(|v| !v.is_empty())
    }
}

pub fn resolve_username(config: &Config) -> Result<String, CswError> {
    resolve_username_with(config, &SystemEnv)
}

pub fn resolve_username_with(config: &Config, env: &dyn Env) -> Result<String, CswError> {
    if let Some(name) = config.username.as_ref().filter(|s| !s.is_empty()) {
        return Ok(name.clone());
    }
    if let Some(email) = env.git_user_email() {
        if let Some(prefix) = email.split('@').next() {
            if !prefix.is_empty() {
                return Ok(prefix.to_string());
            }
        }
    }
    if let Some(user) = env.user_var() {
        return Ok(user);
    }
    Err(CswError::UsernameUnresolvable)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubEnv {
        email: Option<String>,
        user: Option<String>,
    }
    impl Env for StubEnv {
        fn git_user_email(&self) -> Option<String> {
            self.email.clone()
        }
        fn user_var(&self) -> Option<String> {
            self.user.clone()
        }
    }

    fn cfg_with_username(name: Option<&str>) -> Config {
        Config {
            username: name.map(|s| s.to_string()),
            ..Config::default()
        }
    }

    #[test]
    fn config_value_wins() {
        let cfg = cfg_with_username(Some("alice"));
        let env = StubEnv {
            email: Some("bob@example.com".into()),
            user: Some("carol".into()),
        };
        assert_eq!(resolve_username_with(&cfg, &env).unwrap(), "alice");
    }

    #[test]
    fn falls_back_to_email_prefix() {
        let cfg = Config::default();
        let env = StubEnv {
            email: Some("fran.gonzalez@sentinelone.com".into()),
            user: Some("fran".into()),
        };
        assert_eq!(resolve_username_with(&cfg, &env).unwrap(), "fran.gonzalez");
    }

    #[test]
    fn falls_back_to_user_var_when_no_email() {
        let cfg = Config::default();
        let env = StubEnv {
            email: None,
            user: Some("fran".into()),
        };
        assert_eq!(resolve_username_with(&cfg, &env).unwrap(), "fran");
    }

    #[test]
    fn empty_config_username_falls_through() {
        let cfg = cfg_with_username(Some(""));
        let env = StubEnv {
            email: None,
            user: Some("fran".into()),
        };
        assert_eq!(resolve_username_with(&cfg, &env).unwrap(), "fran");
    }

    #[test]
    fn empty_email_prefix_skipped() {
        // "@example.com" should not yield an empty username.
        let cfg = Config::default();
        let env = StubEnv {
            email: Some("@example.com".into()),
            user: Some("fran".into()),
        };
        assert_eq!(resolve_username_with(&cfg, &env).unwrap(), "fran");
    }

    #[test]
    fn unresolvable_when_nothing_available() {
        let cfg = Config::default();
        let env = StubEnv {
            email: None,
            user: None,
        };
        assert!(matches!(
            resolve_username_with(&cfg, &env),
            Err(CswError::UsernameUnresolvable)
        ));
    }
}
