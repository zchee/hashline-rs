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
//! MCP server wiring for the hashline tools.
//!
//! Implements [`rmcp::ServerHandler`] manually (instead of via the tool
//! macros) so tool descriptions can embed scheme-accurate example anchors —
//! the anchor format shown to the model depends on the configured scheme.
//!
//! ## Workspace root resolution
//!
//! The root all relative paths resolve against is dynamic:
//!
//! 1. An explicitly configured root (`--root` / `HASHLINE_ROOT`) is pinned
//!    and never changes.
//! 2. Otherwise the startup fallback (process CWD) is used until the client
//!    completes initialization; if the client advertises the MCP `roots`
//!    capability, the first usable `file://` root from `roots/list` is
//!    adopted, and `notifications/roots/list_changed` re-queries it.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, Peer, RequestContext, RoleServer};
use serde_json::Value;

use crate::config::{ConfigError, SchemeConfig};
use crate::edit::{HashlineEditInput, run_edit};
use crate::grep::{HashlineGrepInput, run_grep};
use crate::read::{HashlineReadInput, MAX_LINES_READ, run_read};
use crate::scheme::Scheme;
use crate::util::{ToolOutcome, Workspace};

const READ_TEMPLATE: &str = r#"Read a file with line-anchored output for use with hashline_edit.

Each line is formatted as ANCHOR→CONTENT, for example:
{example_line1}
{example_line2}

This read format uses `→` between the anchor and content. By contrast,
hashline_grep keeps grep-style separators after the anchor: `:` for
match lines and `-` for context lines.

The ANCHOR (e.g. "{example_anchor}") is a compact fingerprint of the line's content
and surrounding context. Pass anchors to hashline_edit to make edits —
they verify the targeted location still matches the snapshot you saw.
Anchors are valid only for the file state at read time — after any edit,
use the fresh anchors returned by hashline_edit or re-read the file.

Usage:
- The path parameter accepts either a relative path in the workspace or an absolute path
- By default reads up to {max_lines_read} lines from the beginning
- Optionally specify offset and limit for large files
- If you read a file that exists but has empty contents you will receive a warning in place of file contents."#;

const EDIT_TEMPLATE: &str = r#"Edit a file using anchors from hashline_read or hashline_grep.

Operations (use the "op" field):

  "replace" — Replace one line or a range with new content.
    { "op": "replace", "anchor": "{example_anchor}", "content": "    let x = 42;" }
    Range: add "end_anchor" to replace from anchor through end_anchor (INCLUSIVE —
    both the anchor line and end_anchor line are replaced along with everything
    between them). If the anchor or end_anchor line contains a closing delimiter
    like `}` that must be preserved, include it in "content".
    Delete one line: { "op": "replace", "anchor": "{example_anchor}", "content": "" }
    Delete a range: { "op": "replace", "anchor": "{example_anchor}", "end_anchor": "...", "content": "" }

  "insert_after" — Insert new lines after the anchored line.
    { "op": "insert_after", "anchor": "{example_anchor}", "content": "    let y = 1;" }
    Add a blank line: { "op": "insert_after", "anchor": "{example_anchor}", "content": "" }
    Multi-line insert: content with newlines adds multiple lines.
    Beginning of file: use "0:" as anchor.
    End of file: use "EOF" as anchor.
    Existing lines below the anchor are preserved — only include new content.
    Prefer insert_after over replace when adding lines without removing existing ones.

  "write" — Replace entire file content (no anchors needed).
    { "op": "write", "content": "full file content here" }

Batch edits: pass multiple operations in "edits". They are validated against the
pre-edit snapshot and applied atomically bottom-up — if any anchor fails
validation, ALL edits in the batch are rejected (none are applied).
Overlapping ranges are also rejected.

Range safety:
- Multi-line edits may return caution warnings, especially for broader rewrites.
- Larger rewrites are allowed, but use them when you are confident about the target range.
- For very large rewrites (most of the file), prefer a single "write" op over many replace ops.

Follow-up edits:
- On success, the tool returns a snippet with fresh anchors around the edited region.
- On stale-anchor errors, the tool returns fresh anchors around the target line.
  Use these anchors to immediately retry your edit — do not re-read the file.
- The anchor is the full "LINE:HASH" or "LINE:HASH:HASH" before the → separator
  (e.g. "{example_anchor}"). Always include the line number. Do NOT include → or
  the line content after it.
- Never fabricate or modify anchors — only use exact anchors as returned by
  previous tool outputs."#;

const GREP_TEMPLATE: &str = r#"Search file contents with anchor-annotated results for use with hashline_edit.

Match lines include anchors you can pass directly to hashline_edit without
needing to hashline_read the file first. This grep format keeps grep-style
separators after the anchor: `:` for match lines and `-` for context lines.

Content output format:

  {grep_match}    ← match (:)
  {grep_context}    ← context (-)

Usage:
- pattern is a regex: `log.*Error`, `function\s+\w+`, `TODO`
- Search scope defaults to the workspace root; narrow it with path and glob
- Use after_context/before_context/context for context lines around matches (like -A/-B/-C)
- Respects .gitignore; hidden files and binary files are skipped
- Results are capped; truncated results show "at least" counts"#;

/// Convert a `file://` URI into a filesystem path.
///
/// Accepts an empty or `localhost` authority; any other host (or scheme)
/// yields `None`. Percent-encoded octets are decoded.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest
    } else {
        let host = rest.split('/').next()?;
        if host != "localhost" {
            return None;
        }
        rest.get(host.len()..)?
    };
    if path.is_empty() {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?;
    Some(PathBuf::from(decoded.into_owned()))
}

/// The hashline MCP server: three tools sharing one anchor scheme.
#[derive(Debug, Clone)]
pub struct HashlineServer {
    /// Current workspace root. Shared across clones so adoption from client
    /// roots is visible to in-flight handlers.
    root: Arc<RwLock<PathBuf>>,
    /// When `true`, the root was configured explicitly and is never replaced
    /// by client-advertised roots.
    root_pinned: bool,
    /// When `true`, tool paths are confined to the workspace root.
    restrict: bool,
    /// The anchor scheme, built once from `config` at construction.
    scheme: Scheme,
    read_description: Arc<str>,
    edit_description: Arc<str>,
    grep_description: Arc<str>,
    /// Tool listing, rendered on first use and shared across clones — schema
    /// generation and JSON serialization are far too costly to repeat per
    /// `tools/list` request.
    tools: Arc<OnceLock<Vec<Tool>>>,
}

impl HashlineServer {
    /// Create a server rooted at `root` using `config` for all three tools.
    ///
    /// The root is canonicalized when possible. By default it is adoptable —
    /// a client advertising the MCP `roots` capability replaces it after
    /// initialization; use [`Self::with_root_pinned`] for explicit roots.
    pub fn new(root: PathBuf, config: SchemeConfig) -> Result<Self, ConfigError> {
        let scheme = config.build_scheme()?;
        let root = root.canonicalize().unwrap_or(root);
        let read_description = config
            .render_description(READ_TEMPLATE)
            .replace("{max_lines_read}", &MAX_LINES_READ.to_string());
        Ok(Self {
            root: Arc::new(RwLock::new(root)),
            root_pinned: false,
            restrict: false,
            scheme,
            read_description: read_description.into(),
            edit_description: config.render_description(EDIT_TEMPLATE).into(),
            grep_description: config.render_description(GREP_TEMPLATE).into(),
            tools: Arc::new(OnceLock::new()),
        })
    }

    /// Pin the root so client-advertised roots never replace it.
    pub fn with_root_pinned(mut self, pinned: bool) -> Self {
        self.root_pinned = pinned;
        self
    }

    /// Confine all tool paths to the workspace root.
    pub fn with_restrict(mut self, restrict: bool) -> Self {
        self.restrict = restrict;
        self
    }

    /// Snapshot of the current workspace root.
    pub fn current_root(&self) -> PathBuf {
        self.root.read().expect("root lock poisoned").clone()
    }

    /// Path-resolution context for one tool call.
    fn workspace(&self) -> Workspace {
        Workspace::new(self.current_root(), self.restrict)
    }

    /// Replace the workspace root unless it is pinned. Returns whether the
    /// root was updated.
    fn set_root(&self, candidate: PathBuf, source: &str) -> bool {
        if self.root_pinned {
            tracing::debug!(
                candidate = %candidate.display(),
                source,
                "ignoring root candidate: root is pinned"
            );
            return false;
        }
        let Ok(canonical) = candidate.canonicalize() else {
            tracing::warn!(
                candidate = %candidate.display(),
                source,
                "ignoring root candidate: not a resolvable path"
            );
            return false;
        };
        if !canonical.is_dir() {
            tracing::warn!(
                candidate = %canonical.display(),
                source,
                "ignoring root candidate: not a directory"
            );
            return false;
        }
        let mut root = self.root.write().expect("root lock poisoned");
        if *root != canonical {
            tracing::info!(
                old = %root.display(),
                new = %canonical.display(),
                source,
                "workspace root updated"
            );
            *root = canonical;
        }
        true
    }

    /// Adopt the client's first usable `file://` root, if it advertises the
    /// MCP `roots` capability. No-op for pinned roots.
    //
    // `roots` is deprecated upstream by SEP-2577, but it remains the only
    // workspace-discovery mechanism today's MCP clients actually implement,
    // so we keep using it until a replacement lands in the spec and rmcp.
    #[allow(deprecated)]
    async fn adopt_client_roots(&self, peer: Peer<RoleServer>) {
        if self.root_pinned {
            return;
        }
        let Some(info) = peer.peer_info() else {
            return;
        };
        if info.capabilities.roots.is_none() {
            tracing::debug!("client does not advertise the roots capability; keeping current root");
            return;
        }
        match peer.list_roots().await {
            Ok(result) => {
                let adopted = result
                    .roots
                    .iter()
                    .filter_map(|r| file_uri_to_path(&r.uri))
                    .any(|path| self.set_root(path, "client roots/list"));
                if !adopted {
                    tracing::warn!(
                        "client advertised the roots capability but returned no usable \
                         file:// roots; keeping current root"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "roots/list request failed; keeping current root");
            }
        }
    }

    fn tool<T: schemars::JsonSchema>(name: &'static str, description: &Arc<str>) -> Tool {
        let schema = schemars::schema_for!(T);
        let value = serde_json::to_value(schema).expect("tool input schema serializes to JSON");
        let Value::Object(map) = value else {
            unreachable!("schemars root schema is always a JSON object");
        };
        Tool::new(name, description.to_string(), Arc::new(map))
    }

    /// The tool listing, with scheme-aware descriptions.
    ///
    /// Rendered once per server and cached: the input schemas are static, so
    /// repeated `tools/list` requests reuse the same listing.
    pub fn tools(&self) -> &[Tool] {
        self.tools.get_or_init(|| {
            vec![
                Self::tool::<HashlineReadInput>("hashline_read", &self.read_description),
                Self::tool::<HashlineEditInput>("hashline_edit", &self.edit_description),
                Self::tool::<HashlineGrepInput>("hashline_grep", &self.grep_description),
            ]
        })
    }

    /// Dispatch one tool call. Tool-level failures (bad anchors, missing
    /// files, malformed arguments, confinement violations) come back as
    /// `is_error` results so the calling model can correct itself; only
    /// infrastructure failures become protocol errors.
    pub async fn dispatch(&self, name: &str, arguments: Value) -> Result<CallToolResult, McpError> {
        let outcome = match name {
            "hashline_read" => match serde_json::from_value::<HashlineReadInput>(arguments) {
                Ok(input) => run_read(&self.workspace(), &input, self.scheme).await,
                Err(e) => ToolOutcome::error(format!("Invalid arguments for hashline_read: {e}")),
            },
            "hashline_edit" => match serde_json::from_value::<HashlineEditInput>(arguments) {
                Ok(input) => run_edit(&self.workspace(), &input, self.scheme).await,
                Err(e) => ToolOutcome::error(format!("Invalid arguments for hashline_edit: {e}")),
            },
            "hashline_grep" => match serde_json::from_value::<HashlineGrepInput>(arguments) {
                Ok(input) => {
                    let workspace = self.workspace();
                    let scheme = self.scheme;
                    tokio::task::spawn_blocking(move || run_grep(&workspace, &input, scheme))
                        .await
                        .map_err(|e| {
                            McpError::internal_error(
                                format!("hashline_grep task failed: {e}"),
                                None,
                            )
                        })?
                }
                Err(e) => ToolOutcome::error(format!("Invalid arguments for hashline_grep: {e}")),
            },
            other => {
                return Err(McpError::invalid_params(
                    format!("Unknown tool: {other}"),
                    None,
                ));
            }
        };

        let content = vec![ContentBlock::text(outcome.text)];
        Ok(if outcome.is_error {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        })
    }
}

impl ServerHandler for HashlineServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("hashline", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Hashline anchor-based file tools. Workflow: hashline_read (or \
                 hashline_grep) a file to obtain per-line anchors, then pass those \
                 anchors to hashline_edit to make validated edits. Anchors verify the \
                 target still matches the snapshot you saw; after an edit, use the \
                 fresh anchors it returns.",
            )
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        // Adopt in a background task: awaiting a peer request inside the
        // notification handler would stall message processing.
        let server = self.clone();
        tokio::spawn(async move { server.adopt_client_roots(context.peer).await });
    }

    async fn on_roots_list_changed(&self, context: NotificationContext<RoleServer>) {
        let server = self.clone();
        tokio::spawn(async move { server.adopt_client_roots(context.peer).await });
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools().to_vec()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        self.dispatch(&request.name, arguments)
            .await
            .map(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SchemeKind;
    use serde_json::json;

    fn server(root: &std::path::Path) -> HashlineServer {
        HashlineServer::new(root.to_path_buf(), SchemeConfig::default()).unwrap()
    }

    #[test]
    fn tools_listed_with_rendered_descriptions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path());
        let tools = server.tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, ["hashline_read", "hashline_edit", "hashline_grep"]);

        for tool in tools {
            let desc = tool.description.as_deref().unwrap();
            assert!(!desc.contains("{example_anchor}"), "unrendered: {desc}");
            assert!(!desc.contains("{max_lines_read}"), "unrendered: {desc}");
        }
        // Default chunk scheme, hash_len 3 → examples look like 22:abc:rst.
        let edit_desc = tools[1].description.as_deref().unwrap();
        assert!(edit_desc.contains("22:abc:rst"), "{edit_desc}");
    }

    #[test]
    fn content_only_descriptions_use_short_anchors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = SchemeConfig {
            kind: SchemeKind::ContentOnly,
            hash_len: 2,
            ..Default::default()
        };
        let server = HashlineServer::new(tmp.path().to_path_buf(), config).unwrap();
        let tools = server.tools();
        let edit_desc = tools[1].description.as_deref().unwrap();
        assert!(edit_desc.contains("22:ab"), "{edit_desc}");
        assert!(!edit_desc.contains("22:ab:"), "{edit_desc}");
    }

    #[test]
    fn tool_schemas_include_required_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path());
        let tools = server.tools();
        let read_schema = serde_json::to_value(tools[0].input_schema.as_ref()).unwrap();
        assert_eq!(read_schema["type"], "object");
        assert!(read_schema["properties"].get("path").is_some());
        let edit_schema = serde_json::to_value(tools[1].input_schema.as_ref()).unwrap();
        assert!(edit_schema["properties"].get("file_path").is_some());
        assert!(edit_schema["properties"].get("edits").is_some());
    }

    #[test]
    fn file_uri_to_path_variants() {
        assert_eq!(
            file_uri_to_path("file:///home/user/proj"),
            Some(PathBuf::from("/home/user/proj"))
        );
        assert_eq!(
            file_uri_to_path("file://localhost/srv/x"),
            Some(PathBuf::from("/srv/x"))
        );
        assert_eq!(
            file_uri_to_path("file:///with%20space/dir"),
            Some(PathBuf::from("/with space/dir"))
        );
        assert_eq!(file_uri_to_path("file://otherhost/x"), None);
        assert_eq!(file_uri_to_path("https://example.com/x"), None);
        assert_eq!(file_uri_to_path("file://"), None);
    }

    #[test]
    fn set_root_adopts_when_unpinned() {
        let tmp = tempfile::TempDir::new().unwrap();
        let other = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path());
        assert!(server.set_root(other.path().to_path_buf(), "test"));
        assert_eq!(server.current_root(), other.path().canonicalize().unwrap());
    }

    #[test]
    fn set_root_rejected_when_pinned() {
        let tmp = tempfile::TempDir::new().unwrap();
        let other = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path()).with_root_pinned(true);
        let before = server.current_root();
        assert!(!server.set_root(other.path().to_path_buf(), "test"));
        assert_eq!(server.current_root(), before);
    }

    #[test]
    fn set_root_rejects_missing_and_non_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "x").unwrap();
        let server = server(tmp.path());
        let before = server.current_root();
        assert!(!server.set_root(PathBuf::from("/no/such/dir"), "test"));
        assert!(!server.set_root(tmp.path().join("file.txt"), "test"));
        assert_eq!(server.current_root(), before);
    }

    #[tokio::test]
    async fn dispatch_read_write_edit_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path());

        // Create a file through the edit tool's write op.
        let result = server
            .dispatch(
                "hashline_edit",
                json!({
                    "file_path": "demo.txt",
                    "edits": [{"op": "write", "content": "one\ntwo\nthree\n"}]
                }),
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));

        // Read it back and harvest the anchor for line 2.
        let result = server
            .dispatch("hashline_read", json!({"path": "demo.txt"}))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let text = match &result.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        let line2 = text.lines().find(|l| l.starts_with("2:")).unwrap();
        let anchor = line2.split('→').next().unwrap();

        // Edit line 2 via the harvested anchor.
        let result = server
            .dispatch(
                "hashline_edit",
                json!({
                    "file_path": "demo.txt",
                    "edits": [{"op": "replace", "anchor": anchor, "content": "TWO"}]
                }),
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("demo.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
    }

    #[tokio::test]
    async fn restrict_blocks_absolute_paths_outside_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "top secret").unwrap();

        let server = server(tmp.path()).with_restrict(true);
        for (tool, args) in [
            ("hashline_read", json!({"path": secret.to_str().unwrap()})),
            (
                "hashline_edit",
                json!({
                    "file_path": secret.to_str().unwrap(),
                    "edits": [{"op": "write", "content": "clobbered"}]
                }),
            ),
            (
                "hashline_grep",
                json!({"pattern": "secret", "path": outside.path().to_str().unwrap()}),
            ),
        ] {
            let result = server.dispatch(tool, args).await.unwrap();
            assert_eq!(result.is_error, Some(true), "{tool} must be confined");
            let text = match &result.content[0] {
                ContentBlock::Text(t) => t.text.clone(),
                other => panic!("expected text content, got {other:?}"),
            };
            assert!(text.contains("denied"), "{tool}: {text}");
        }
        // The edit attempt must not have touched the outside file.
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "top secret");
    }

    #[tokio::test]
    async fn restrict_allows_workspace_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("ok.txt"), "fine\n").unwrap();
        let server = server(tmp.path()).with_restrict(true);
        let result = server
            .dispatch("hashline_read", json!({"path": "ok.txt"}))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn dispatch_invalid_arguments_is_tool_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = server(tmp.path())
            .dispatch("hashline_read", json!({"nope": true}))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_is_protocol_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = server(tmp.path())
            .dispatch("no_such_tool", json!({}))
            .await
            .unwrap_err();
        assert!(err.message.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn dispatch_grep_finds_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("s.rs"), "fn needle() {}\n").unwrap();
        let result = server(tmp.path())
            .dispatch("hashline_grep", json!({"pattern": "needle"}))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let text = match &result.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("s.rs"), "{text}");
    }
}
