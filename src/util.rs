//! Small shared helpers for the hashline tools.

use std::path::{Path, PathBuf};

/// Model-facing result of a tool invocation: rendered text plus an error flag.
///
/// Errors here are *tool-level* (visible to the calling model so it can
/// retry with corrected input), not protocol-level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Rendered output text.
    pub text: String,
    /// Whether this outcome represents a tool-level failure.
    pub is_error: bool,
}

impl ToolOutcome {
    /// Successful outcome.
    pub fn success(text: String) -> Self {
        Self {
            text,
            is_error: false,
        }
    }

    /// Failed outcome (model-visible, retryable).
    pub fn error(text: String) -> Self {
        Self {
            text,
            is_error: true,
        }
    }
}

/// Resolve a model-supplied path against the workspace root.
///
/// Absolute paths are used as-is; relative paths are joined onto `root`.
pub fn resolve_path(root: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative_joins_root() {
        let root = Path::new("/workspace");
        assert_eq!(
            resolve_path(root, "src/main.rs"),
            PathBuf::from("/workspace/src/main.rs")
        );
    }

    #[test]
    fn resolve_absolute_passthrough() {
        let root = Path::new("/workspace");
        assert_eq!(
            resolve_path(root, "/etc/hosts"),
            PathBuf::from("/etc/hosts")
        );
    }

    #[test]
    fn outcome_constructors() {
        assert!(!ToolOutcome::success(String::new()).is_error);
        assert!(ToolOutcome::error(String::new()).is_error);
    }
}
