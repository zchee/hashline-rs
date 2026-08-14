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
//! macros) so the tool listing can be rendered once and annotated with
//! behavior hints.
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

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock, RwLock},
};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
        Tool, ToolAnnotations,
    },
    service::{NotificationContext, Peer, RequestContext, RoleServer},
};
use serde_json::Value;

use crate::{
    edit::run_edit,
    glob::run_glob,
    grep::run_grep,
    persist::Durability,
    protocol::{EditRequest, GlobRequest, GrepRequest, ReadRequest, WriteRequest},
    read::{MAX_LINES_READ, run_read},
    util::{ToolOutcome, Workspace},
    write::run_write,
};

const READ_TEMPLATE: &str = r#"Read a file with versioned positional output for use with edit.

Every response starts with a snapshot header:
[hashline snapshot=<ID> lines=<N> bytes=<B> path=<JSON_STRING>]

Each following line is LINE@BYTE|CONTENT. Pass the snapshot ID and positions to edit.
Pagination footer: [hashline next snapshot=<ID> position=LINE@BYTE]

Usage:
- path is workspace-relative or absolute
- limit defaults to {max_lines_read} (max {max_lines_read})
- optional start_line begins the page at that 1-based line (no cursor needed)
- optional cursor continues a prior page (never combined with start_line)
"#;

const EDIT_TEMPLATE: &str = r#"Edit a file using versioned byte-range replace operations.

{
  "file_path": "src/lib.rs",
  "snapshot": "<32-hex from read>",
  "edits": [
    { "op": "replace", "start": "21@418", "end": "22@430", "content": "mod render;
" }
  ]
}

start inclusive, end exclusive. Stale snapshot fails closed with snapshot_conflict.
"#;

const WRITE_TEMPLATE: &str = r#"Create a new file, or replace a whole file you have read, with versioned safety.

{
  "file_path": "src/new.rs",
  "content": "mod fresh;\n",
  "expect": "absent"
}

expect "absent" creates the file exclusively; if it already exists the error
returns the current snapshot header so you can read or overwrite instead.
expect <32-hex snapshot from read> replaces exactly that version; a stale
snapshot fails closed with snapshot_conflict. Success returns the new
snapshot for immediate use with edit. Parent directories are created.
"#;

const GREP_TEMPLATE: &str = r#"Search file contents with position-annotated results for use with edit.

Each matching file section starts with its snapshot header; match lines carry
LINE@BYTE positions you can pass directly to edit without reading the file
first. Grep-style separators follow the position: `:` for match lines and
`-` for context lines.

Content output format:

  12@345:matched line content    ← match (:)
  11@331-context line content    ← context (-)

Usage:
- pattern is a regex: `log.*Error`, `function\s+\w+`, `TODO`
- Search scope defaults to the workspace root; narrow it with path and glob
- Use after_context/before_context/context for context lines around matches (like -A/-B/-C)
- output_mode "files_with_matches" lists matching paths only; "count" lists PATH: N
  match counts; both are cheap discovery modes (context only applies to "content")
- Respects .gitignore; hidden files and binary files are skipped
- Results are capped; truncated results show "at least" counts"#;

const GLOB_TEMPLATE: &str = r#"Find files by glob pattern, newest first, for use with read and edit.

{
  "pattern": "**/*.rs",
  "path": "src",
  "max_results": 1000
}

`*` stops at path separators; `**` crosses them. The walk respects
.gitignore and skips hidden files. Results are paths you can pass directly
to read, edit, and write, sorted by modification time (newest first), and
end with: [hashline files=<N> truncated=<true|false>]
"#;

/// How long a client may treat a `tools/list` result as fresh, in
/// milliseconds (SEP-2549).
///
/// The listing is a pure function of this binary — descriptions, annotations,
/// and input schemas are all fixed at compile time — so the only event that
/// can change it is a server upgrade, which arrives with a new connection
/// anyway. Five minutes bounds the staleness a client can carry across a
/// reconnect while still letting it skip re-listing within a session.
const TOOLS_LIST_TTL_MS: u64 = 5 * 60 * 1_000;

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

/// The hashline MCP server: five tools sharing one snapshot protocol.
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
    /// Persistence durability policy for the mutation tools.
    durability: Durability,
    read_description: Arc<str>,
    edit_description: Arc<str>,
    write_description: Arc<str>,
    grep_description: Arc<str>,
    glob_description: Arc<str>,
    /// Tool listing, rendered on first use and shared across clones — schema
    /// generation and JSON serialization are far too costly to repeat per
    /// `tools/list` request.
    tools: Arc<OnceLock<Vec<Tool>>>,
}

impl HashlineServer {
    /// Create a server rooted at `root`.
    ///
    /// The root is canonicalized when possible. By default it is adoptable —
    /// a client advertising the MCP `roots` capability replaces it after
    /// initialization; use [`Self::with_root_pinned`] for explicit roots.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        let root = root.canonicalize().unwrap_or(root);
        let read_description =
            READ_TEMPLATE.replace("{max_lines_read}", &MAX_LINES_READ.to_string());
        Self {
            root: Arc::new(RwLock::new(root)),
            root_pinned: false,
            restrict: false,
            durability: Durability::default(),
            read_description: read_description.into(),
            edit_description: EDIT_TEMPLATE.into(),
            write_description: WRITE_TEMPLATE.into(),
            grep_description: GREP_TEMPLATE.into(),
            glob_description: GLOB_TEMPLATE.into(),
            tools: Arc::new(OnceLock::new()),
        }
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

    /// Replace the persistence durability policy (default [`Durability::Full`]).
    #[must_use]
    pub fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    /// Snapshot of the current workspace root.
    pub fn current_root(&self) -> PathBuf {
        self.root.read().expect("root lock poisoned").clone()
    }

    /// Tool-execution context for one tool call.
    fn workspace(&self) -> Workspace {
        Workspace::new(self.current_root(), self.restrict).with_durability(self.durability)
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

    fn tool<T: schemars::JsonSchema>(
        name: &'static str,
        description: &Arc<str>,
        annotations: ToolAnnotations,
    ) -> Tool {
        let schema = schemars::schema_for!(T);
        let value = serde_json::to_value(schema).expect("tool input schema serializes to JSON");
        let Value::Object(map) = value else {
            unreachable!("schemars root schema is always a JSON object");
        };
        Tool::new(name, description.to_string(), Arc::new(map)).annotate(annotations)
    }

    /// Annotations for tools that never modify the workspace.
    fn read_only_annotations(title: &str) -> ToolAnnotations {
        ToolAnnotations::with_title(title)
            .read_only(true)
            .idempotent(true)
            .open_world(false)
    }

    /// Annotations for tools that replace destination bytes.
    fn mutating_annotations(title: &str) -> ToolAnnotations {
        ToolAnnotations::with_title(title)
            .read_only(false)
            .destructive(true)
            .open_world(false)
    }

    /// The tool listing.
    ///
    /// Rendered once per server and cached: the input schemas are static, so
    /// repeated `tools/list` requests reuse the same listing.
    pub fn tools(&self) -> &[Tool] {
        self.tools.get_or_init(|| {
            vec![
                Self::tool::<ReadRequest>(
                    "read",
                    &self.read_description,
                    Self::read_only_annotations("Versioned file read"),
                ),
                Self::tool::<EditRequest>(
                    "edit",
                    &self.edit_description,
                    Self::mutating_annotations("Versioned byte-range edit"),
                ),
                Self::tool::<WriteRequest>(
                    "write",
                    &self.write_description,
                    Self::mutating_annotations("Versioned file create or replace"),
                ),
                Self::tool::<GrepRequest>(
                    "grep",
                    &self.grep_description,
                    Self::read_only_annotations("Versioned content search"),
                ),
                Self::tool::<GlobRequest>(
                    "glob",
                    &self.glob_description,
                    Self::read_only_annotations("Deterministic file discovery"),
                ),
            ]
        })
    }

    /// The `tools/list` result, carrying the cache directives SEP-2549 makes
    /// mandatory for protocol revision `2026-07-28`.
    ///
    /// `rmcp` leaves `ttlMs` and `cacheScope` unset for backward
    /// compatibility, and a client validating the `2026-07-28` schema rejects
    /// the whole listing when they are absent. The listing carries no
    /// workspace path and no caller-specific state, so it is cacheable across
    /// authorization contexts.
    #[must_use]
    pub fn tools_result(&self) -> ListToolsResult {
        ListToolsResult::with_all_items(self.tools().to_vec())
            .with_ttl_ms(TOOLS_LIST_TTL_MS)
            .with_cache_scope(CacheScope::Public)
    }

    /// Dispatch one tool call. Tool-level failures (stale snapshots, missing
    /// files, malformed arguments, confinement violations) come back as
    /// `is_error` results so the calling model can correct itself; only
    /// infrastructure failures become protocol errors.
    pub async fn dispatch(&self, name: &str, arguments: Value) -> Result<CallToolResult, McpError> {
        let outcome = match name {
            "read" => match serde_json::from_value::<ReadRequest>(arguments) {
                Ok(input) => run_read(&self.workspace(), &input).await,
                Err(e) => ToolOutcome::error(format!("Invalid arguments for read: {e}")),
            },
            "edit" => match serde_json::from_value::<EditRequest>(arguments) {
                Ok(input) => run_edit(&self.workspace(), &input).await,
                Err(e) => ToolOutcome::error(format!("Invalid arguments for edit: {e}")),
            },
            "write" => match serde_json::from_value::<WriteRequest>(arguments) {
                Ok(input) => run_write(&self.workspace(), &input).await,
                Err(e) => ToolOutcome::error(format!("Invalid arguments for write: {e}")),
            },
            "grep" => match serde_json::from_value::<GrepRequest>(arguments) {
                Ok(input) => {
                    let workspace = self.workspace();
                    tokio::task::spawn_blocking(move || run_grep(&workspace, &input))
                        .await
                        .map_err(|e| {
                            McpError::internal_error(format!("grep task failed: {e}"), None)
                        })?
                }
                Err(e) => ToolOutcome::error(format!("Invalid arguments for grep: {e}")),
            },
            "glob" => match serde_json::from_value::<GlobRequest>(arguments) {
                Ok(input) => {
                    let workspace = self.workspace();
                    tokio::task::spawn_blocking(move || run_glob(&workspace, &input))
                        .await
                        .map_err(|e| {
                            McpError::internal_error(format!("glob task failed: {e}"), None)
                        })?
                }
                Err(e) => ToolOutcome::error(format!("Invalid arguments for glob: {e}")),
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
                "Hashline versioned file tools. Workflow: read (or grep) a \
                 file to obtain its snapshot and positions, then pass them to \
                 edit for validated in-place edits. Use write to create new \
                 files (expect \"absent\") or to replace a whole read file \
                 against its exact snapshot. Stale snapshots fail closed and \
                 return fresh recovery context.",
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
        Ok(self.tools_result())
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
    use serde_json::json;

    use super::*;

    fn server(root: &std::path::Path) -> HashlineServer {
        HashlineServer::new(root.to_path_buf())
    }

    #[test]
    fn tools_listed_with_rendered_descriptions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path());
        let tools = server.tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, ["read", "edit", "write", "grep", "glob"]);

        for tool in tools {
            let desc = tool.description.as_deref().unwrap();
            assert!(!desc.contains("{max_lines_read}"), "unrendered: {desc}");
        }
        let edit_desc = tools[1].description.as_deref().unwrap();
        assert!(edit_desc.contains("snapshot"), "{edit_desc}");
    }

    #[test]
    fn tools_carry_behavior_annotations() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path());
        for tool in server.tools() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", tool.name));
            assert_eq!(annotations.open_world_hint, Some(false), "{}", tool.name);
            assert!(annotations.title.is_some(), "{}", tool.name);
            match tool.name.as_ref() {
                "read" | "grep" | "glob" => {
                    assert_eq!(annotations.read_only_hint, Some(true), "{}", tool.name);
                    assert_eq!(annotations.idempotent_hint, Some(true), "{}", tool.name);
                }
                "edit" | "write" => {
                    assert_eq!(annotations.read_only_hint, Some(false), "{}", tool.name);
                    assert_eq!(annotations.destructive_hint, Some(true), "{}", tool.name);
                }
                other => panic!("unexpected tool {other}"),
            }
        }
    }

    #[test]
    fn tools_result_carries_cache_directives() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path());
        let wire = serde_json::to_value(server.tools_result()).unwrap();
        assert_eq!(wire["resultType"], json!("complete"), "{wire}");
        assert_eq!(wire["ttlMs"], json!(TOOLS_LIST_TTL_MS), "{wire}");
        assert_eq!(wire["cacheScope"], json!("public"), "{wire}");
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
        assert!(edit_schema["properties"].get("snapshot").is_some());
        assert!(edit_schema["properties"].get("edits").is_some());
        let write_schema = serde_json::to_value(tools[2].input_schema.as_ref()).unwrap();
        assert!(write_schema["properties"].get("file_path").is_some());
        assert!(write_schema["properties"].get("content").is_some());
        assert!(write_schema["properties"].get("expect").is_some());
        let glob_schema = serde_json::to_value(tools[4].input_schema.as_ref()).unwrap();
        assert!(glob_schema["properties"].get("pattern").is_some());
        assert!(glob_schema["properties"].get("max_results").is_some());
    }

    #[test]
    fn frozen_tool_schemas_are_strict_and_incompatible() {
        let read = serde_json::to_value(schemars::schema_for!(crate::protocol::ReadRequest))
            .expect("read schema serializes");
        let edit = serde_json::to_value(schemars::schema_for!(crate::protocol::EditRequest))
            .expect("edit schema serializes");
        let operation = serde_json::to_value(schemars::schema_for!(crate::protocol::EditOperation))
            .expect("edit operation schema serializes");
        let write = serde_json::to_value(schemars::schema_for!(crate::protocol::WriteRequest))
            .expect("write schema serializes");
        let grep = serde_json::to_value(schemars::schema_for!(crate::protocol::GrepRequest))
            .expect("grep schema serializes");
        let glob = serde_json::to_value(schemars::schema_for!(crate::protocol::GlobRequest))
            .expect("glob schema serializes");

        for schema in [&read, &edit, &write, &grep, &glob] {
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
        }
        assert_eq!(read["required"], json!(["path"]));
        assert!(read["properties"].get("start_line").is_some());

        assert_eq!(write["required"], json!(["file_path", "content", "expect"]));
        assert_eq!(
            write["properties"]["expect"]["pattern"],
            json!("^(absent|[0-9a-f]{32})$")
        );
        assert_eq!(glob["required"], json!(["pattern"]));
        assert_eq!(glob["properties"]["max_results"]["maximum"], json!(1000));
        assert!(read["properties"].get("cursor").is_some());
        assert!(read["properties"].get("offset").is_none());

        assert_eq!(edit["required"], json!(["file_path", "snapshot", "edits"]));
        let edit_text = serde_json::to_string(&edit).expect("render edit schema");
        let operation_text = serde_json::to_string(&operation).expect("render operation schema");
        for legacy in [
            "anchor",
            "end_anchor",
            "insert_after",
            "\"write\"",
            "\"scheme\"",
        ] {
            assert!(!edit_text.contains(legacy), "{legacy} in {edit_text}");
            assert!(
                !operation_text.contains(legacy),
                "{legacy} in {operation_text}"
            );
        }
        assert!(operation_text.contains("\"replace\""));

        let grep_properties = grep["properties"].as_object().expect("grep properties");
        assert_eq!(grep_properties.len(), 9);
        for field in [
            "pattern",
            "path",
            "glob",
            "ignore_case",
            "before_context",
            "after_context",
            "context",
            "max_matches",
            "output_mode",
        ] {
            assert!(grep_properties.contains_key(field), "missing {field}");
        }
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
        use crate::snapshot::Snapshot;

        let tmp = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path());
        let empty = Snapshot::from_bytes(Vec::new()).unwrap();
        let result = server
            .dispatch(
                "edit",
                json!({
                    "file_path": "demo.txt",
                    "snapshot": empty.id().to_string(),
                    "edits": [{
                        "op": "replace",
                        "start": "1@0",
                        "end": "2@0",
                        "content": "one\ntwo\nthree\n"
                    }]
                }),
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");

        let result = server
            .dispatch("read", json!({"path": "demo.txt", "limit": 2000}))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let text = match &result.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text, {other:?}"),
        };
        assert!(text.contains("[hashline snapshot="), "{text}");
        let snapshot = text
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .find_map(|p| p.strip_prefix("snapshot="))
            .unwrap();
        let start = text
            .lines()
            .find(|l| l.starts_with("2@"))
            .unwrap()
            .split('|')
            .next()
            .unwrap();
        let end = text
            .lines()
            .find(|l| l.starts_with("3@"))
            .unwrap()
            .split('|')
            .next()
            .unwrap();
        let result = server
            .dispatch(
                "edit",
                json!({
                    "file_path": "demo.txt",
                    "snapshot": snapshot,
                    "edits": [{
                        "op": "replace",
                        "start": start,
                        "end": end,
                        "content": "TWO\n"
                    }]
                }),
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("demo.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
    }

    #[tokio::test]
    async fn dispatch_write_create_and_versioned_replace() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path());

        let result = server
            .dispatch(
                "write",
                json!({"file_path": "made.txt", "content": "alpha\n", "expect": "absent"}),
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let text = match &result.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text, {other:?}"),
        };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["created"], json!(true));
        let snapshot = value["snapshot"].as_str().unwrap().to_owned();

        let duplicate = server
            .dispatch(
                "write",
                json!({"file_path": "made.txt", "content": "beta\n", "expect": "absent"}),
            )
            .await
            .unwrap();
        assert_eq!(duplicate.is_error, Some(true), "{duplicate:?}");

        let replaced = server
            .dispatch(
                "write",
                json!({"file_path": "made.txt", "content": "beta\n", "expect": snapshot}),
            )
            .await
            .unwrap();
        assert_ne!(replaced.is_error, Some(true), "{replaced:?}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("made.txt")).unwrap(),
            "beta\n"
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
            ("read", json!({"path": secret.to_str().unwrap()})),
            (
                "edit",
                json!({
                    "file_path": secret.to_str().unwrap(),
                    "snapshot": "00000000000000000000000000000000",
                    "edits": [{"op": "replace", "start": "1@0", "end": "2@0", "content": "x"}]
                }),
            ),
            (
                "write",
                json!({
                    "file_path": secret.to_str().unwrap(),
                    "content": "overwritten",
                    "expect": "absent"
                }),
            ),
            (
                "grep",
                json!({"pattern": "secret", "path": outside.path().to_str().unwrap()}),
            ),
            (
                "glob",
                json!({"pattern": "*", "path": outside.path().to_str().unwrap()}),
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
            .dispatch("read", json!({"path": "ok.txt"}))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn dispatch_invalid_arguments_is_tool_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = server(tmp.path())
            .dispatch("read", json!({"nope": true}))
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
    async fn dispatch_rejects_legacy_tool_names() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server = server(tmp.path());

        for legacy_name in ["hashline_read", "hashline_edit", "hashline_grep"] {
            let error = server
                .dispatch(legacy_name, json!({}))
                .await
                .expect_err("legacy tool aliases must remain removed");
            assert!(
                error.message.contains("Unknown tool"),
                "{legacy_name}: {}",
                error.message
            );
        }
    }

    #[tokio::test]
    async fn dispatch_glob_lists_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("x.rs"), "fn x() {}\n").unwrap();
        std::fs::write(tmp.path().join("y.txt"), "text\n").unwrap();
        let result = server(tmp.path())
            .dispatch("glob", json!({"pattern": "*.rs"}))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let text = match &result.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(text.contains("x.rs"), "{text}");
        assert!(!text.contains("y.txt"), "{text}");
        assert!(
            text.contains("[hashline files=1 truncated=false]"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn dispatch_grep_finds_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("s.rs"), "fn needle() {}\n").unwrap();
        let result = server(tmp.path())
            .dispatch("grep", json!({"pattern": "needle"}))
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
