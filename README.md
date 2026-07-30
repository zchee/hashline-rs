# hashline-rs

A standalone [Model Context Protocol] server implementing the hashline
anchor-based file toolset from [xai-org/grok-build]
(`crates/codegen/xai-grok-tools/src/implementations/grok_build_hashline`),
written in Rust on top of the official [rmcp] SDK.

Every line of a file gets a compact anchor (`LINE:HASH` or `LINE:HASH:HASH`)
derived from whitespace-normalized content hashes. Models reference lines by
anchor instead of raw line numbers, so edits are validated against the
snapshot the model actually saw — stale or shifted anchors are rejected with
recovery hints (fresh anchors, shift suggestions) instead of silently
corrupting the file.

[Model Context Protocol]: https://modelcontextprotocol.io
[xai-org/grok-build]: https://github.com/xai-org/grok-build
[rmcp]: https://github.com/modelcontextprotocol/rust-sdk

## Tools

| Tool | Purpose |
|---|---|
| `hashline_read` | Read a file as `ANCHOR→CONTENT` lines (supports `offset`/`limit`) |
| `hashline_edit` | Apply `replace` / `insert_after` / `write` ops addressed by anchors; batches are validated against the pre-edit snapshot and applied atomically bottom-up |
| `hashline_grep` | Regex content search with anchor-annotated match (`:`) and context (`-`) lines, respecting `.gitignore` |

The edit tool returns fresh anchors around the edited region on success. On
stale-anchor failures it returns the current content with fresh anchors and,
when the line merely shifted, a ready-to-retry suggested anchor.

## Install

```console
cargo install --path .
```

## Usage

```console
hashline-mcp [--root <DIR>] [--restrict] [--scheme chunk|content_only|checkpoint]
             [--hash-len 1..4] [--chunk-size N] [--checkpoint-interval N]
```

All flags are also available as environment variables (`HASHLINE_ROOT`,
`HASHLINE_RESTRICT`, `HASHLINE_SCHEME`, `HASHLINE_HASH_LEN`,
`HASHLINE_CHUNK_SIZE`, `HASHLINE_CHECKPOINT_INTERVAL`). The server speaks MCP
over stdio; logs go to stderr (`RUST_LOG` controls verbosity).

### Workspace root resolution

Relative paths resolve against the workspace root, chosen as follows:

1. `--root` / `HASHLINE_ROOT` — explicit and **pinned**: never changes.
2. Otherwise, if the client advertises the MCP `roots` capability, the first
   usable `file://` root from `roots/list` is adopted after initialization
   (and re-queried on `notifications/roots/list_changed`).
3. Otherwise the process CWD at launch — fine for project-scoped
   registrations that launch in the project directory, but a warning is
   logged when it looks wrong (`/` or `$HOME`).

`--restrict` confines every tool path to the workspace root: absolute paths
outside it, `..` components, and symlink escapes are rejected. It is off by
default to match the reference implementation's behavior.

Register with Claude Code:

```console
claude mcp add hashline -- hashline-mcp --root /path/to/workspace
```

Or in `.mcp.json`:

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

## Anchor schemes

- `chunk` (default) — local line hash plus a fingerprint of the fixed-size
  chunk containing the line. Edits invalidate only anchors within the
  affected chunk. Anchor format `LINE:HASH:HASH` (e.g. `22:abc:rst`).
- `content_only` — local line hash only. Least anchor churn, weakest
  freshness (edits above a line do not invalidate it). Format `LINE:HASH`.
- `checkpoint` — local line hash plus a fingerprint chained from the nearest
  preceding checkpoint. Strongest freshness detection, most churn.

Line hashes are whitespace-normalized (trim + collapse internal runs), so
formatter-only edits such as re-indentation do not invalidate anchors.

## Development

```console
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## License

Apache-2.0
