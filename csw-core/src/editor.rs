//! Editor invocation. Templates are command lines containing `{path}`,
//! split with shell-like quoting and spawned detached so the CLI returns
//! immediately.

use crate::errors::CswError;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

const PLACEHOLDER: &str = "{path}";

/// Render a template with the path substituted in. Used for previewing the
/// command without spawning it.
pub fn render(template: &str, path: &Path) -> Result<Vec<String>, CswError> {
    if !template.contains(PLACEHOLDER) {
        return Err(CswError::EditorTemplateMissingPath);
    }
    let substituted = template.replace(PLACEHOLDER, &path.to_string_lossy());
    let argv = shlex::split(&substituted).ok_or(CswError::EditorTemplateUnparseable)?;
    if argv.is_empty() {
        return Err(CswError::EditorTemplateUnparseable);
    }
    Ok(argv)
}

/// Spawn the editor described by `template` against `path`, detaching it
/// from this process. Returns immediately. An empty template is a no-op.
pub fn spawn(template: &str, path: &Path) -> Result<()> {
    if template.is_empty() {
        return Ok(());
    }
    let argv = render(template, path).map_err(anyhow::Error::from)?;
    let (program, args) = argv.split_first().expect("render rejects empty argv");

    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning editor `{program}`"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn render_substitutes_path() {
        let argv = render("zed {path}", &PathBuf::from("/x/y")).unwrap();
        assert_eq!(argv, vec!["zed", "/x/y"]);
    }

    #[test]
    fn render_handles_quoted_args() {
        let argv = render(
            r#"my-editor --flag "with spaces" {path}"#,
            &PathBuf::from("/x"),
        )
        .unwrap();
        assert_eq!(argv, vec!["my-editor", "--flag", "with spaces", "/x"]);
    }

    #[test]
    fn render_rejects_template_without_placeholder() {
        let err = render("zed", &PathBuf::from("/x")).unwrap_err();
        assert!(matches!(err, CswError::EditorTemplateMissingPath));
    }

    #[test]
    fn render_rejects_empty_template() {
        // No placeholder → MissingPath wins.
        let err = render("", &PathBuf::from("/x")).unwrap_err();
        assert!(matches!(err, CswError::EditorTemplateMissingPath));
    }

    #[test]
    fn render_rejects_unparseable_quoting() {
        // Unbalanced quote.
        let err = render(r#"zed "{path}"#, &PathBuf::from("/x")).unwrap_err();
        assert!(matches!(err, CswError::EditorTemplateUnparseable));
    }

    #[test]
    fn spawn_empty_template_is_noop() {
        // Should not spawn anything, should not error.
        spawn("", &PathBuf::from("/tmp")).unwrap();
    }

    #[test]
    fn spawn_runs_a_real_command() {
        // `true` exits 0; we don't wait, but the spawn should at least succeed.
        spawn("true {path}", &PathBuf::from("/tmp")).unwrap();
    }

    #[test]
    fn spawn_errors_for_missing_binary() {
        let err = spawn(
            "definitely-not-a-real-binary-csw {path}",
            &PathBuf::from("/x"),
        );
        assert!(err.is_err());
    }
}
