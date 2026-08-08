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
//! `glob` — deterministic gitignore-respecting file discovery.
//!
//! [`run_glob`] walks like grep (same ignore semantics), matches the pattern
//! against walk-root-relative paths, and reports matches newest-first under
//! the R024 ordering so discovery output chains directly into read and edit.

use std::{fs, path::Path, time::UNIX_EPOCH};

use globset::GlobBuilder;
use ignore::WalkBuilder;

use crate::{
    protocol::{
        ErrorCode, GlobEntry, GlobRequest, GlobSummary, ProtocolError, sort_reference_glob_entries,
    },
    util::{ToolOutcome, Workspace, protocol_outcome},
};

/// Execute a `glob` request against the local filesystem.
///
/// Blocking — call via `spawn_blocking` from async contexts.
pub fn run_glob(workspace: &Workspace, input: &GlobRequest) -> ToolOutcome {
    if let Err(error) = input.validate() {
        return protocol_outcome(ProtocolError::from(error));
    }

    let matcher = match GlobBuilder::new(&input.pattern)
        .literal_separator(true)
        .build()
    {
        Ok(glob) => glob.compile_matcher(),
        Err(e) => {
            return protocol_outcome(ProtocolError::new(
                ErrorCode::InvalidPattern,
                format!("Invalid glob pattern \"{}\": {e}", input.pattern),
            ));
        }
    };

    let walk_root = match workspace.resolve(input.path.as_deref().unwrap_or(".")) {
        Ok(path) => path,
        Err(reason) => {
            return protocol_outcome(ProtocolError::new(ErrorCode::RootEscape, reason));
        }
    };
    let meta = match fs::metadata(&walk_root) {
        Ok(meta) => meta,
        Err(_) => {
            return protocol_outcome(ProtocolError::new(
                ErrorCode::NotFound,
                format!("Glob path not found: {}", walk_root.display()),
            ));
        }
    };
    if !meta.is_dir() {
        return protocol_outcome(ProtocolError::new(
            ErrorCode::NotAFile,
            format!("Glob path is not a directory: {}", walk_root.display()),
        ));
    }

    let display_prefix = input.path.as_deref().filter(|path| *path != ".");
    let mut entries = Vec::new();
    for entry in WalkBuilder::new(&walk_root).build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&walk_root) else {
            continue;
        };
        if !matcher.is_match(relative) {
            continue;
        }
        // A path that cannot round-trip through UTF-8 tool arguments cannot
        // be passed back to read or edit, so it is not reported.
        let display = match display_prefix {
            Some(prefix) => Path::new(prefix).join(relative),
            None => relative.to_path_buf(),
        };
        let Some(path) = display.to_str() else {
            continue;
        };
        let modified = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(UNIX_EPOCH);
        entries.push(GlobEntry {
            path: path.to_owned(),
            modified,
        });
    }

    let total = entries.len();
    sort_reference_glob_entries(&mut entries);
    let cap = usize::from(input.max_results);
    let truncated = total > cap;
    entries.truncate(cap);

    let summary = GlobSummary {
        files: u64::try_from(entries.len())
            .expect("a slice length always fits u64 on supported 64-bit targets"),
        truncated,
    };
    let mut output = String::with_capacity(entries.iter().map(|e| e.path.len() + 1).sum::<usize>());
    for entry in &entries {
        output.push_str(&entry.path);
        output.push('\n');
    }
    output.push_str(&summary.render());
    ToolOutcome::success(output)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn ws(root: &Path) -> Workspace {
        Workspace::new(root.to_path_buf(), false)
    }

    fn req(pattern: &str) -> GlobRequest {
        GlobRequest {
            pattern: pattern.to_owned(),
            path: None,
            max_results: 1000,
        }
    }

    fn set_mtime(path: &Path, secs: u64) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    /// Fixture tree with a `.git` marker so `.gitignore` semantics apply.
    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "fn b() {}\n").unwrap();
        fs::write(root.join("src/.hidden.rs"), "fn h() {}\n").unwrap();
        fs::write(root.join("target/ignored.rs"), "fn i() {}\n").unwrap();
        fs::write(root.join("notes.txt"), "text\n").unwrap();
        set_mtime(&root.join("src/b.rs"), 2_000);
        set_mtime(&root.join("a.rs"), 1_000);
        tmp
    }

    fn lines(outcome: &ToolOutcome) -> Vec<String> {
        outcome.text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn recursive_pattern_respects_ignores_and_orders_newest_first() {
        let tmp = fixture();
        let outcome = run_glob(&ws(tmp.path()), &req("**/*.rs"));
        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(
            lines(&outcome),
            vec![
                "src/b.rs".to_owned(),
                "a.rs".to_owned(),
                "[hashline files=2 truncated=false]".to_owned(),
            ]
        );
    }

    #[test]
    fn single_star_stops_at_separators() {
        let tmp = fixture();
        let outcome = run_glob(&ws(tmp.path()), &req("*.rs"));
        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(
            lines(&outcome),
            vec![
                "a.rs".to_owned(),
                "[hashline files=1 truncated=false]".to_owned(),
            ]
        );
    }

    #[test]
    fn path_scoped_results_are_reprefixed_for_reuse() {
        let tmp = fixture();
        let request = GlobRequest {
            pattern: "*.rs".to_owned(),
            path: Some("src".to_owned()),
            max_results: 1000,
        };
        let outcome = run_glob(&ws(tmp.path()), &request);
        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(
            lines(&outcome),
            vec![
                "src/b.rs".to_owned(),
                "[hashline files=1 truncated=false]".to_owned(),
            ]
        );
    }

    #[test]
    fn cap_keeps_the_newest_and_reports_truncation() {
        let tmp = fixture();
        let request = GlobRequest {
            pattern: "**/*.rs".to_owned(),
            path: None,
            max_results: 1,
        };
        let outcome = run_glob(&ws(tmp.path()), &request);
        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(
            lines(&outcome),
            vec![
                "src/b.rs".to_owned(),
                "[hashline files=1 truncated=true]".to_owned(),
            ]
        );
    }

    #[test]
    fn equal_mtimes_tie_break_bytewise_ascending() {
        let tmp = fixture();
        set_mtime(&tmp.path().join("a.rs"), 2_000);
        let outcome = run_glob(&ws(tmp.path()), &req("**/*.rs"));
        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(
            lines(&outcome),
            vec![
                "a.rs".to_owned(),
                "src/b.rs".to_owned(),
                "[hashline files=2 truncated=false]".to_owned(),
            ]
        );
    }

    #[test]
    fn invalid_pattern_and_bad_paths_use_the_taxonomy() {
        let tmp = fixture();
        let workspace = ws(tmp.path());

        let invalid = run_glob(&workspace, &req("a{"));
        assert!(invalid.is_error);
        assert!(
            invalid.text.contains("\"invalid_pattern\""),
            "{}",
            invalid.text
        );

        let missing = GlobRequest {
            pattern: "*".to_owned(),
            path: Some("no/such/dir".to_owned()),
            max_results: 1000,
        };
        let missing = run_glob(&workspace, &missing);
        assert!(missing.is_error);
        assert!(missing.text.contains("\"not_found\""), "{}", missing.text);

        let file_root = GlobRequest {
            pattern: "*".to_owned(),
            path: Some("notes.txt".to_owned()),
            max_results: 1000,
        };
        let file_root = run_glob(&workspace, &file_root);
        assert!(file_root.is_error);
        assert!(
            file_root.text.contains("\"not_a_file\""),
            "{}",
            file_root.text
        );
    }

    #[test]
    fn empty_match_set_is_a_bare_summary() {
        let tmp = fixture();
        let outcome = run_glob(&ws(tmp.path()), &req("*.zig"));
        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(outcome.text, "[hashline files=0 truncated=false]");
    }
}
