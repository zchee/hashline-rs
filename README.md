# hashline-rs

[![test][test-badge]][test]
[![codecov.io][codecov-badge]][codecov]
[![CodSpeed][codspeed-badge]][codspeed]

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
claude mcp add hashline -- hashline-mcp [--root /path/to/workspace]
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
[examples/claude-code/](./examples/claude-code/).

## Development

```console
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## License

Apache-2.0

<!-- badge links -->
[test]: https://github.com/zchee/hashline-rs/actions/workflows/ci.yaml
[codecov]: https://app.codecov.io/gh/zchee/hashline-rs
[codspeed]: https://app.codspeed.io/zchee/hashline-rs

[test-badge]: https://img.shields.io/github/actions/workflow/status/zchee/hashline-rs/ci.yaml?branch=main&style=for-the-badge&label=TEST&logo=github
[codecov-badge]: https://img.shields.io/codecov/c/github/zchee/hashline-rs/main?logo=codecov&style=for-the-badge
[codspeed-badge]: https://img.shields.io/badge/CodSpeed-benchmarks-ff6661?style=for-the-badge&logo=data:image/svg%2Bxml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAyNCAyNCI+PGRlZnM+PGxpbmVhckdyYWRpZW50IGlkPSJnIiB4MT0iMCIgeTE9IjAiIHgyPSIwIiB5Mj0iMSI+PHN0b3Agc3RvcC1jb2xvcj0iI0ZGOEM0QSIvPjxzdG9wIG9mZnNldD0iMSIgc3RvcC1jb2xvcj0iI0ZGNjY2MSIvPjwvbGluZWFyR3JhZGllbnQ+PC9kZWZzPjxwYXRoIGZpbGw9InVybCgjZykiIGZpbGwtcnVsZT0iZXZlbm9kZCIgZD0iTTUuMzIsMTguMzRDNi4wOSwxOC41MSA2LjYzLDE4LjM5IDYuNzIsMTguNEw2LjcyLDE4LjRDOCwxOS4xIDguOTMsMTkuNyA5LjgzLDIwLjA4QzkuOCwyMC4wOCA5Ljc2LDIwLjA4IDkuNzIsMjAuMDlDOC45OSwyMC4yIDguNDcsMjAuMjkgOC4xNSwyMC4zNEM0Ljg2LDIwLjg4IDMuMjQsMTkuOTQgMy41OSwxOC40OUMzLjc0LDE3LjkgNC41NCwxOC4xNiA1LjMyLDE4LjM0Wk0xNi4zNyw0LjI2QzE4LjQ1LDUuODEgMTkuMDUsNi42MiAyMC4xNiw3Ljg1QzIyLjg4LDcuODUgMjQuNCw5LjkgMjMuOTEsMTIuOTZDMjMuNiwxNC45MSAxOS43NCwxNC4yMiAxOC44MywxMi45NkMxOC4yMiwxMy41MiAxNi43MiwxNC43NiAxNC4zMywxNi42N0MxNC4zLDE2LjY5IDE0LjI2LDE2LjcyIDE0LjIzLDE2Ljc1QzEzLjUzLDE3LjMxIDEwLjg0LDE3LjEzIDEwLjU4LDE2Ljk0QzExLjEyLDE2LjA0IDExLjUxLDE1LjM0IDExLjc1LDE0Ljg0QzEyLjEsMTQuMDggMTIuMzUsMTMuMzMgMTIuMzUsMTIuMDdDMTIuMzUsMTAuODIgMTEuNjYsOS43NiAxMC4xOCw5Ljc4QzEwLjA1LDkuNzkgOS45LDkuODkgOS45LDEwLjA4QzkuOSwxMC4yOCAxMC4wNSwxMC4zOCAxMC4xOCwxMC4zOEMxMC45NiwxMC4zOCAxMS41MSwxMS4zNiAxMS41MSwxMi4wN0MxMS41MSwxMi43OSAxMS40LDEzLjM3IDExLjA2LDE0LjIzQzEwLjc4LDE0Ljk5IDkuNjEsMTYuODUgOS4zMSwxNy4wNEM5LjE4LDE3LjExIDkuMTUsMTcuMjkgOS4yMiwxNy40MkM5LjI1LDE3LjQ2IDkuMjcsMTcuNDggOS4zLDE3LjVDOS4zMiwxNy41MSA5LjM0LDE3LjUyIDkuMzUsMTcuNTNDOS4zNywxNy41NCA5LjQsMTcuNTUgOS40MywxNy41NkM5LjUsMTcuNTggOS41OCwxNy42MSA5LjY5LDE3LjYzQzkuOTUsMTcuNyAxMC4yNywxNy43NSAxMC42MywxNy44QzExLjYyLDE3LjkxIDEyLjczLDE3Ljg5IDEzLjkyLDE3LjY3TDEzLjk2LDE3LjY3QzE0LjMyLDE3LjYgMTQuODMsMTcuNiAxNS4zNCwxNy42NkMxNS45NCwxNy43NCAxNi40OCwxNy45MSAxNi44OSwxOC4xNkMxNy4yNSwxOC4zOCAxNy41LDE4LjY2IDE3LjYzLDE4Ljk5TDE3LjY0LDE5LjAzQzE3LjY5LDE5LjIxIDE3LjcyLDE5LjQgMTcuNzIsMTkuNkMxNy43MiwyMC43IDE1LjQsMjAuMDQgMTIuNzIsMTkuOTJDMTIuNTUsMTkuOTEgMTIuMzUsMTkuOTEgMTIuMTEsMTkuOTJDMTEuMDIsMTkuODYgMTAuMTIsMTkuNTQgOC45MSwxOC45M0w4LjkzLDE4LjkzQzguOTMsMTguOTMgOC44NCwxOC44OSA4LjY5LDE4LjgyQzguMzQsMTguNjQgNy45NSwxOC40MyA3LjUzLDE4LjJMNy41MiwxOC4yQzcuMTIsMTcuOTUgNi43NywxNy42NyA2LjY4LDE3LjQxQzYuNDUsMTYuNzUgNi41NiwxNi43NSA3LjQsMTUuNzhDNS45NywxNS45NiA1Ljk0LDE2LjA2IDQuMzEsMTYuMDZDMi42OCwxNi4wNiAwLDE1Ljk4IDAsMTQuMzdDMCwxMy4zIDAuNzIsMTIuODggMi4xNywxMy4xMkMxLjgyLDExLjE3IDIuNDIsOS40MSAzLjk4LDcuODVDOC4wOCwzLjc3IDEzLjYsOS40IDE3LjM5LDkuNEMxOS4xMSw5LjQgMTQuMzUsNi42NyAxMi4zNyw1LjlDMTAuNCw1LjEzIDEzLjUxLDIuMTIgMTYuMzcsNC4yNlpNMjEuMTQsOS44OEMyMC44Myw5Ljg4IDIwLjU3LDEwLjE1IDIwLjU3LDEwLjQ5QzIwLjU3LDEwLjgzIDIwLjgzLDExLjEgMjEuMTQsMTEuMUMyMS40NSwxMS4xIDIxLjcsMTAuODMgMjEuNywxMC40OUMyMS43LDEwLjE1IDIxLjQ1LDkuODggMjEuMTQsOS44OFoiLz48L3N2Zz4K
