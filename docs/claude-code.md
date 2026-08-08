# Replacing Claude Code's built-in file tools with hashline

This recipe makes Claude Code do all of its text-file work through the
hashline MCP server instead of the built-in `Read`, `Edit`, `Write`, `Grep`,
and `Glob` tools. It requires the five-tool server (`read`, `edit`, `write`,
`grep`, `glob` — check with `/mcp` or `tools/list`); older builds that lack
`write` and `glob` must not be used with the deny rules below, or the session
loses the ability to create and discover files.

The mechanics below were verified against the official documentation:

- Permission rules: <https://code.claude.com/docs/en/permissions.md>
- Hooks reference: <https://code.claude.com/docs/en/hooks.md>
- Environment variables: <https://code.claude.com/docs/en/env-vars.md>

Two facts do the heavy lifting:

1. **A bare tool-name deny rule removes the tool from Claude's context** —
   not merely blocks it at call time. `"deny": ["Edit"]` means the model
   never sees an `Edit` tool at all, so it reaches for the MCP equivalents
   naturally. (MCP tools cannot shadow built-in names; they always appear as
   `mcp__hashline__read` and so on.)
2. **A PreToolUse hook can deny one call with an in-band reason** the model
   reads and acts on, which makes per-file carve-outs possible where a
   blanket deny would be too blunt.

## 1. Register the server

```console
claude mcp add hashline -- hashline-mcp --root /path/to/workspace
```

Or in the project's `.mcp.json`:

```json
{
  "mcpServers": {
    "hashline": {
      "command": "hashline-mcp",
      "args": ["--root", "."]
    }
  }
}
```

## 2. Recommended profile: deny four, redirect Read

`Edit`, `Write`, `Grep`, and `Glob` have complete hashline replacements, so
they are removed outright. `Read` is special: the built-in tool also renders
images, PDFs, and Jupyter notebooks, which a strict-UTF-8 text protocol
deliberately does not. Keeping built-in `Read` in context but denying text
reads through a hook preserves those formats while routing every text read
to hashline.

Project `.claude/settings.json`:

```json
{
  "permissions": {
    "deny": ["Edit", "Write", "Grep", "Glob"]
  },
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Read",
        "hooks": [
          {
            "type": "command",
            "command": "$CLAUDE_PROJECT_DIR/.claude/hooks/redirect-read.sh"
          }
        ]
      }
    ]
  }
}
```

Copy [`examples/claude-code/redirect-read.sh`](../examples/claude-code/redirect-read.sh)
to `.claude/hooks/redirect-read.sh` and make it executable
(`chmod +x .claude/hooks/redirect-read.sh`). The script allows binary and
notebook formats through to the built-in `Read` and answers every other read
with the documented deny shape:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "…use mcp__hashline__read…"
  }
}
```

The reason text is shown to the model, so every denied attempt carries its
own correction — the redirect is self-healing rather than configured once
and hoped for.

## 3. Strict profile: deny all five

```json
{
  "permissions": {
    "deny": ["Read", "Edit", "Write", "Grep", "Glob"]
  }
}
```

Maximal replacement: the model sees only the hashline file tools. The cost
is that images, PDFs, and notebooks become unreadable (hashline rejects
non-UTF-8 bytes by contract, R007), and `@file` mentions plus IDE
open-file context are best-effort blocked by `Read` rules as well. Prefer
the recommended profile unless the workspace is text-only.

## 4. Caveats

- **Bash remains an escape hatch.** `cat`, `sed -i`, and `tee` still touch
  files without versioning. Denying Bash file mutation patterns is possible
  (`"deny": ["Bash(sed -i *)"]`, …) but out of scope here; treat unversioned
  Bash edits as a review-time concern.
- **`NotebookEdit` is untouched.** Notebook editing keeps its built-in path;
  hashline's text contract does not model notebook cells.
- **MCP output cap.** Claude Code truncates MCP tool results at
  `MAX_MCP_OUTPUT_TOKENS` (default 25000 tokens, warning at 10000). A full
  2000-line `read` page can exceed it on wide files; prefer `limit` around
  500–800 with `start_line`/cursor continuation, or raise the variable. The
  snapshot header is the first line, so truncation never loses the version
  identity.
- **Subagents and other MCP clients** inherit whatever permissions their
  context grants; this recipe configures one project. Managed/global
  settings can widen it.
- **Path-scoped rules**: `Read(path)`/`Edit(path)` deny rules keep working
  alongside this setup; hashline's own `--restrict` flag confines the server
  to its workspace root independently of Claude Code's rules.

## 5. Verify the replacement

Start a session in the configured project and check:

1. `/context` (or ask the model to list its tools) — `Edit`, `Write`,
   `Grep`, and `Glob` must be absent; `mcp__hashline__*` present.
2. Ask for a small task that creates a file: the model must call
   `mcp__hashline__write` with `expect: "absent"`.
3. Ask it to read a text file: the hook must deny built-in `Read` once and
   the model must switch to `mcp__hashline__read`.
4. Ask it to find files (`glob`) and search content in files-only mode
   (`grep` with `output_mode: "files_with_matches"`).
