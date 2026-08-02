// Copyright 2026 The hashline-rs Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//! Small shared helpers for the hashline tools.

use std::borrow::Cow;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// Return one unpredictable seed shared by this process.
///
/// `RandomState` supplies the operating-system-seeded key material used by the
/// standard hash map. Hashing a domain label turns that opaque key into the
/// scalar seed required by the snapshot version function without adding a
/// second random-number dependency.
pub(crate) fn process_random_seed() -> u64 {
    static PROCESS_RANDOM_SEED: OnceLock<u64> = OnceLock::new();

    *PROCESS_RANDOM_SEED.get_or_init(|| {
        let state = RandomState::new();
        let mut hasher = state.build_hasher();
        hasher.write(b"hashline-v2-snapshot-version");
        hasher.finish()
    })
}

/// Decode file bytes as UTF-8, borrowing the buffer when it is already valid.
///
/// Every tool reads whole files, so UTF-8 validation runs over every byte of
/// every request. `simdutf8` validates with SIMD — several times faster than
/// the standard library's scalar check on the valid input that dominates real
/// workloads — and hands back a `&str` borrowed from `bytes`, so the happy path
/// neither copies nor allocates.
///
/// Only genuinely invalid input pays for the lossy rebuild, which re-scans with
/// [`String::from_utf8_lossy`] to place the replacement characters.
pub fn decode_utf8(bytes: &[u8]) -> Cow<'_, str> {
    match simdutf8::basic::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => Cow::Owned(String::from_utf8_lossy(bytes).into_owned()),
    }
}

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

/// Path-resolution context shared by all tools: the workspace root plus the
/// optional confinement policy.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Canonicalized workspace root that relative paths resolve against.
    pub root: PathBuf,
    /// When `true`, resolved paths must stay within `root`.
    pub restrict: bool,
}

impl Workspace {
    /// Create a workspace. `root` should already be canonicalized.
    pub fn new(root: PathBuf, restrict: bool) -> Self {
        Self { root, restrict }
    }

    /// Resolve a model-supplied path against the workspace root, enforcing
    /// confinement when `restrict` is enabled.
    ///
    /// Returns a model-visible error message on confinement violations.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let joined = resolve_path(&self.root, path);
        if self.restrict {
            confine_to_root(&self.root, &joined).map_err(|reason| {
                format!(
                    "Access to {} denied: {reason} \
                     (--restrict confines tools to {}).",
                    joined.display(),
                    self.root.display()
                )
            })?;
        }
        Ok(joined)
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

/// Verify that `path` cannot escape `root` (which must be canonicalized).
///
/// Rejects lexical escapes (`..` components) outright, then canonicalizes
/// the deepest *existing* ancestor of `path` so symlink escapes are caught
/// while still permitting paths to not-yet-created files (e.g. a `write` op
/// creating a new file).
fn confine_to_root(root: &Path, path: &Path) -> Result<(), String> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("path contains \"..\" components".to_owned());
    }

    let mut existing = path;
    while !existing.exists() {
        match existing.parent() {
            Some(parent) => existing = parent,
            None => return Err("path has no existing ancestor".to_owned()),
        }
    }

    let canon = existing
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize {}: {e}", existing.display()))?;
    if canon.starts_with(root) {
        Ok(())
    } else {
        Err(format!(
            "path resolves to {} which is outside the workspace root",
            canon.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restricted(root: &Path) -> Workspace {
        Workspace::new(root.canonicalize().unwrap(), true)
    }

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

    #[test]
    fn unrestricted_allows_outside_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = Workspace::new(tmp.path().to_path_buf(), false);
        assert!(ws.resolve("/etc/hosts").is_ok());
        assert!(ws.resolve("../elsewhere").is_ok());
    }

    #[test]
    fn restricted_allows_paths_inside_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "x").unwrap();
        let ws = restricted(tmp.path());
        assert!(ws.resolve("f.txt").is_ok());
        // Not-yet-created file in an existing directory is fine (write op).
        assert!(ws.resolve("new_file.txt").is_ok());
        // Not-yet-created nested path is fine too.
        assert!(ws.resolve("new/dir/file.txt").is_ok());
    }

    #[test]
    fn restricted_rejects_parent_dir_components() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = restricted(tmp.path());
        let err = ws.resolve("../outside.txt").unwrap_err();
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn restricted_rejects_absolute_outside_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = restricted(tmp.path());
        let err = ws.resolve("/etc/hosts").unwrap_err();
        assert!(err.contains("outside the workspace root"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn restricted_rejects_symlink_escape() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();

        let ws = restricted(tmp.path());
        let err = ws.resolve("link/secret.txt").unwrap_err();
        assert!(err.contains("outside the workspace root"), "{err}");
    }

    #[test]
    fn restricted_allows_root_itself() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = restricted(tmp.path());
        assert!(ws.resolve(".").is_ok());
    }
}
