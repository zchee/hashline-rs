//! MCP server wiring for the hashline tools.
//!
//! Implements [`rmcp::ServerHandler`] manually (instead of via the tool
//! macros) so tool descriptions can embed scheme-accurate example anchors —
//! the anchor format shown to the model depends on the configured scheme.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::Value;

use crate::config::{ConfigError, SchemeConfig};
use crate::edit::{HashlineEditInput, run_edit};
use crate::grep::{HashlineGrepInput, run_grep};
use crate::read::{HashlineReadInput, MAX_LINES_READ, run_read};
use crate::util::ToolOutcome;

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

/// The hashline MCP server: three tools sharing one anchor scheme.
#[derive(Debug, Clone)]
pub struct HashlineServer {
    root: Arc<PathBuf>,
    config: SchemeConfig,
    read_description: Arc<str>,
    edit_description: Arc<str>,
    grep_description: Arc<str>,
}

impl HashlineServer {
    /// Create a server rooted at `root` using `config` for all three tools.
    pub fn new(root: PathBuf, config: SchemeConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        let read_description = config
            .render_description(READ_TEMPLATE)
            .replace("{max_lines_read}", &MAX_LINES_READ.to_string());
        Ok(Self {
            root: Arc::new(root),
            config,
            read_description: read_description.into(),
            edit_description: config.render_description(EDIT_TEMPLATE).into(),
            grep_description: config.render_description(GREP_TEMPLATE).into(),
        })
    }

    /// Workspace root all relative paths resolve against.
    pub fn root(&self) -> &PathBuf {
        &self.root
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
    pub fn tools(&self) -> Vec<Tool> {
        vec![
            Self::tool::<HashlineReadInput>("hashline_read", &self.read_description),
            Self::tool::<HashlineEditInput>("hashline_edit", &self.edit_description),
            Self::tool::<HashlineGrepInput>("hashline_grep", &self.grep_description),
        ]
    }

    /// Dispatch one tool call. Tool-level failures (bad anchors, missing
    /// files, malformed arguments) come back as `is_error` results so the
    /// calling model can correct itself; only infrastructure failures become
    /// protocol errors.
    pub async fn dispatch(&self, name: &str, arguments: Value) -> Result<CallToolResult, McpError> {
        let outcome = match name {
            "hashline_read" => match serde_json::from_value::<HashlineReadInput>(arguments) {
                Ok(input) => {
                    let scheme = self.build_scheme()?;
                    run_read(&self.root, &input, &*scheme).await
                }
                Err(e) => ToolOutcome::error(format!("Invalid arguments for hashline_read: {e}")),
            },
            "hashline_edit" => match serde_json::from_value::<HashlineEditInput>(arguments) {
                Ok(input) => {
                    let scheme = self.build_scheme()?;
                    run_edit(&self.root, &input, &*scheme).await
                }
                Err(e) => ToolOutcome::error(format!("Invalid arguments for hashline_edit: {e}")),
            },
            "hashline_grep" => match serde_json::from_value::<HashlineGrepInput>(arguments) {
                Ok(input) => {
                    let root = Arc::clone(&self.root);
                    let config = self.config;
                    tokio::task::spawn_blocking(move || {
                        let scheme = config
                            .build_scheme()
                            .expect("config validated at construction");
                        run_grep(&root, &input, &*scheme)
                    })
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("hashline_grep task failed: {e}"), None)
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

    fn build_scheme(&self) -> Result<Box<dyn crate::scheme::AnchorScheme>, McpError> {
        self.config.build_scheme().map_err(|e| {
            McpError::internal_error(format!("invalid scheme configuration: {e}"), None)
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

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools()))
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
        let tools = server(tmp.path()).tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, ["hashline_read", "hashline_edit", "hashline_grep"]);

        for tool in &tools {
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
        let tools = server(tmp.path()).tools();
        let read_schema = serde_json::to_value(tools[0].input_schema.as_ref()).unwrap();
        assert_eq!(read_schema["type"], "object");
        assert!(read_schema["properties"].get("path").is_some());
        let edit_schema = serde_json::to_value(tools[1].input_schema.as_ref()).unwrap();
        assert!(edit_schema["properties"].get("file_path").is_some());
        assert!(edit_schema["properties"].get("edits").is_some());
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
