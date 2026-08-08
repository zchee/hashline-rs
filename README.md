# hashline-rs

A standalone [Model Context Protocol] server providing versioned, fail-closed
file tools, written in Rust on top of the official [rmcp] SDK. It grew out of
the hashline anchor toolset in [xai-org/grok-build]
(`crates/codegen/xai-grok-tools/src/implementations/grok_build_hashline`) and
now implements a single snapshot-based protocol.

Every read and grep response names the exact bytes it saw: a 128-bit
process-scoped snapshot identity plus `LINE@BYTE` positions at logical line
starts. Edits and writes are validated against that exact snapshot and fail
closed with recovery context — a fresh header and context lines — instead of
silently corrupting the file.

[Model Context Protocol]: https://modelcontextprotocol.io
[xai-org/grok-build]: https://github.com/xai-org/grok-build
[rmcp]: https://github.com/modelcontextprotocol/rust-sdk

## Tools

| Tool | Purpose |
|---|---|
| `read` | Versioned page of `LINE@BYTE\|CONTENT` lines; `start_line` random access and cursor pagination |
| `edit` | Atomic batch of byte-range `replace` operations validated against the named snapshot |
| `write` | Exclusive create (`expect: "absent"`) or whole-file replace against an exact snapshot |
| `grep` | Regex content search with position-annotated matches; `files_with_matches` / `count` discovery modes |
| `glob` | Newest-first, gitignore-respecting file discovery with deterministic ordering |

The normative contract — grammar, error taxonomy, and executable compliance
examples — lives in [docs/protocol.md](docs/protocol.md); every rule runs as
a doctest via `cargo test --doc`.

## Install

```console
cargo install --path .
```

## Usage

```console
hashline-mcp [--root <DIR>] [--restrict]
```

Both flags are also available as environment variables (`HASHLINE_ROOT`,
`HASHLINE_RESTRICT`). The server speaks MCP over stdio; logs go to stderr
(`RUST_LOG` controls verbosity).

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
outside it, `..` components, and symlink escapes are rejected.

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

### Embedding in another crate

Depend on the library through git, pinned to a revision:

```toml
[dependencies]
hashline = { git = "https://github.com/zchee/hashline-rs", rev = "<commit>" }
```

The supported embedding surface is the typed runners — `read::run`,
`edit::run`, `write::run` (async) and `grep::run`, `glob::run` (blocking) —
returning `Result<_, protocol::ProtocolError>` with every failure drawn from
the stable R017 taxonomy, plus the `protocol` request/response types,
`util::Workspace`, and `HashlineServer` for serving MCP in-process. The
`run_*` variants render the same results as MCP text.

Since the Phase 8 cleanup the crate has **no cargo features and no
target-feature requirements**: snapshot identity hashes with seeded XXH3, so
the former gxhash `+aes` build caveat is gone and any supported 64-bit
target builds out of the box.

The crate is not yet published to crates.io and makes no semver promises;
pin a `rev` and re-read [docs/protocol.md](docs/protocol.md) when moving it.

### Replace Claude Code's built-in file tools

[docs/claude-code.md](docs/claude-code.md) is a copy-paste recipe that
removes the built-in `Read`/`Edit`/`Write`/`Grep`/`Glob` in favor of the
hashline tools — including the binary-file carve-outs that keep images and
PDFs readable, and the caveats (Bash escape hatch, MCP output token cap).
The ready-made settings fragment and PreToolUse hook live in
[examples/claude-code/](examples/claude-code/).

## Development

```console
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## License

Apache-2.0
