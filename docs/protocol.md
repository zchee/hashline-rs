# Hashline Protocol

- Status: normative Phase 1 contract
- Authoritative plan: `.omx/plans/2026-07-31-incompatible-max-performance-redesign.md`
- Plan SHA-256: `db00bf029f184811b79ab709df064a3fb9b23a9ab64562e28432e43ca8a41a6f`
- Phase 0 candidate: `6afe83059de218d71d4161fb36848d849c9da0a6`

This document freezes the hashline protocol and its slow reference
model. The words MUST, MUST NOT, SHOULD, and MAY are normative.

The contract is deliberately independent of the optimized snapshot, offset,
cache, persistence, and search implementations scheduled after Phase 1. A
later implementation may change algorithms and data layout only when its
observable behavior remains identical to this document and the reference
model in `hashline::protocol`.

## 1. State and trust boundary

```text
exact file bytes
      |
      +-- validate text -------------------------+
      |                                          |
      +-- hash with process seed -> SnapshotId   |
                                                 v
read/grep -> snapshot header + positions -> versioned edit
                                                 |
                                                 +-- apply or conflict
```

A path is not a version. Metadata is not a version. The only version equality
test is equality of the 128-bit identity produced from the exact byte sequence
by one running server process.

### R001: Snapshot identity

A `SnapshotId` is exactly 128 bits and its canonical wire form is exactly 32
lowercase hexadecimal ASCII characters. Leading zeroes are required. Uppercase
hex, prefixes, separators, whitespace, and any other length are invalid.

A server creates one random seed from the operating system at startup and
fails startup if secure seed generation fails. It hashes the exact file bytes
with that seed. No path, metadata, Unicode normalization, line-ending
normalization, or trailing sentinel participates in the input.

The concrete 128-bit hash implementation is wire-opaque. Phase 2 selects the
fastest Phase 0 candidate that meets the accidental-collision and cross-target
requirements. That selection cannot change any rule in this document.

Identity equality is meaningful only inside one server process. IDs MUST NOT
be persisted or compared across restarts. The ID is not a security digest.
The per-process seed prevents precomputed chosen-input collisions; accidental
collision risk is the 128-bit hash risk accepted by the plan.

```rust
use std::str::FromStr;
use hashline::protocol::SnapshotId;

let id = SnapshotId::from_u128(0x7d9c3af08e1b4f6c9a2d1137f68582a1);
assert_eq!(id.to_string(), "7d9c3af08e1b4f6c9a2d1137f68582a1");
assert_eq!(SnapshotId::from_str(&id.to_string())?, id);
assert!(SnapshotId::from_str("7D9C3AF08E1B4F6C9A2D1137F68582A1").is_err());
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R002: Snapshot header

Every read response starts with one header. Grep emits one header for every
file section containing a reported match. The exact grammar is:

```text
[hashline snapshot=<SNAPSHOT_ID> lines=<LINE_COUNT> bytes=<BYTE_COUNT> path=<JSON_STRING>]
```

Fields appear in that order with one ASCII space between them. `LINE_COUNT`
is at least one. `BYTE_COUNT` is the exact byte length. `JSON_STRING` is a
JSON-quoted path, including its quotes, so spaces, brackets, quotes, and
newlines cannot make the header ambiguous.

Example:

```text
[hashline snapshot=7d9c3af08e1b4f6c9a2d1137f68582a1 lines=641 bytes=18492 path="src/lib.rs"]
21@418|mod render;
22@430|mod scheme;
```

### R003: Position token grammar

A position token has the exact grammar `LINE@BYTE`.

- `LINE` is a canonical base-10 `u64` in `1..=u64::MAX`.
- `BYTE` is a canonical base-10 `u64` in `0..=u64::MAX`.
- Zero is written `0`; every other number starts with `1` through `9`.
- Signs, leading zeroes, whitespace, non-ASCII digits, additional `@`
  characters, and overflow are invalid.
- Parsing a valid token performs no heap allocation.

```rust
use std::str::FromStr;
use hashline::protocol::Position;

let position = Position::from_str("21@418")?;
assert_eq!(position.line(), 21);
assert_eq!(position.byte(), 418);
assert_eq!(position.to_string(), "21@418");
for invalid in ["0@0", "01@0", "1@00", "+1@0", "1@0@0", " 1@0"] {
    assert!(Position::from_str(invalid).is_err(), "{invalid}");
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R004: Byte authority and line diagnostics

The byte component is an offset into the exact snapshot bytes and is
authoritative. It is never a Unicode scalar, UTF-16 code-unit, grapheme, or
display-column index.

The line component is for humans and diagnostics, but a request MUST pair it
with the correct boundary. A mismatch is `invalid_position`; the server MUST
NOT use the line number to select or relocate bytes. This check catches
corrupted or manually edited tokens without making line numbers authoritative.

Read and grep emit positions only at logical line starts. Edit also accepts the
terminal boundary described below.

### R005: Half-open line-boundary ranges

The only edit operation is `replace`. Its `start` is inclusive and `end` is
exclusive. Both positions MUST be valid boundaries in the named snapshot.

Logical line starts are byte zero and the byte immediately after every
`0x0a` LF byte. A terminal boundary follows the last logical line and has
line number `LINE_COUNT + 1` and byte offset `BYTE_COUNT`. When a file ends
in LF, the synthetic final empty line start and terminal boundary have the
same byte offset but consecutive line numbers.

A range is ordered by complete boundary order, not only by byte value. It is
an insertion only when the complete `start` and `end` tokens are identical.
Therefore the synthetic final empty-line start through the terminal boundary
is a non-insertion range even though both byte components equal `BYTE_COUNT`.
Replacing `1@0` through the terminal boundary replaces the whole file.
Replacement content is the exact UTF-8 bytes decoded from the JSON string.

### R006: Batch atomicity, overlap, and insertion order

An edit request contains between 1 and 1024 operations. Every operation is
validated against the same named pre-edit snapshot before any output is
constructed or persisted. One failure rejects the whole batch.

Two non-empty half-open ranges may be adjacent but MUST NOT overlap. A
zero-width insertion is invalid when it lies strictly inside a non-empty
range. Insertions at a range start or end are valid and occur before a
replacement starting at that same boundary. Operations at distinct
boundaries sharing one byte offset follow boundary order: the synthetic final
empty-line start precedes the terminal boundary. Multiple insertions at one
complete boundary appear in request order.

The reference apply result is independent of internal sort strategy. Integer
capacity arithmetic is checked; overflow is an error.

```rust
use std::str::FromStr;
use hashline::protocol::{EditOperation, Position, apply_reference_edits};

let source = b"alpha\r\nbeta\r\n";
let edits = [
    EditOperation::replace(
        Position::from_str("2@7")?,
        Position::from_str("3@13")?,
        "BETA\n".to_owned(),
    ),
];
let output = apply_reference_edits(source, &edits)?;
assert_eq!(output, b"alpha\r\nBETA\n");
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R007: UTF-8 and NUL policy

Read, edit, and grep are text tools. They accept only strictly valid UTF-8 and
reject a NUL byte at any offset. Validation covers the complete file, not a
prefix. Lossy decoding and replacement characters are forbidden because they
change byte offsets.

JSON replacement strings are valid Unicode by construction but are still
rejected when they contain NUL. A UTF-8 BOM, when present, is ordinary file
content and participates in byte offsets and snapshot identity.

### R008: Line endings and unterminated lines

LF is the only boundary byte. CR immediately before LF is part of the line
terminator for display and is omitted from rendered line content. A bare CR is
ordinary content.

Bytes outside replaced ranges are copied byte-for-byte. Replacement bytes are
copied byte-for-byte. The server never chooses or normalizes a line-ending
style. A final unterminated line remains unterminated unless replacement
content changes it.

### R009: Empty-file model

An empty file has one logical empty line at `1@0` and a terminal boundary at
`2@0`. A read renders `1@0|`. The line count is one and byte count is zero.
Replacing `1@0` through `2@0` is the canonical whole-file write for an
empty file.

### R010: Size and offset limits

Wire offsets and line numbers are `u64`. The maximum accepted file length is
`i64::MAX` bytes (`9_223_372_036_854_775_807`). This matches the maximum
portable contiguous Rust allocation on the supported 64-bit targets while
allowing the Phase 2 `u32`/ `u64` offset representation split.

A larger metadata length fails with `file_too_large` before allocation or
content reads. Every conversion to `usize`, every offset addition, and every
output capacity calculation is checked. No truncating cast is permitted.

### R011: Snapshot conflict

A syntactically valid requested snapshot that differs from the freshly
computed identity is a `snapshot_conflict`. The server performs no edits.

The structured error includes:

- the requested snapshot;
- a current snapshot header containing the fresh identity, byte count, line
  count, and path;
- at most five fresh context lines centered on the first requested start line,
  clamped to the current file;
- `retryable: true`.

Context lines use current positions and the same display rules as read. A
missing, unreadable, or non-text current file returns its specific file/text
error because no truthful fresh snapshot can be supplied.

### R012: Cache eviction and restart

The cache is an accelerator, never an authority. A cache miss or eviction
forces a fresh read and identity computation. If the recomputed identity
equals the request, the request remains valid; eviction alone cannot change
semantics. If it differs, rule R011 applies.

A restart creates a new seed. A pre-restart snapshot or pagination cursor is
therefore outside its identity scope and conflicts with a fresh header. There
is no persisted snapshot registry and no compatibility lookup.

### R013: Read pagination cursor

A read request uses an optional structured cursor:

```json
{
  "snapshot": "7d9c3af08e1b4f6c9a2d1137f68582a1",
  "next": "2001@58102"
}
```

The cursor names the snapshot and the next logical line start. Request decoding
first rejects a malformed token as `invalid_position`. For a decoded cursor,
current-file text validation and snapshot comparison precede boundary
resolution. A snapshot mismatch therefore uses R011 even when the old
position is not a boundary in the current bytes. Only a matching snapshot with
a boundary mismatch returns `invalid_position`.

When more lines remain, the response ends with:

```text
[hashline next snapshot=7d9c3af08e1b4f6c9a2d1137f68582a1 position=2001@58102]
```

A complete page has no footer. The next byte offset lets the optimized path
continue without rescanning from byte zero. Pagination applies to read.
Grep truncation is terminal and requires a narrower query rather than a tree
cursor.

### R014: Read request and cap

The exact read input is:

```json
{
  "path": "src/lib.rs",
  "limit": 2000,
  "cursor": null
}
```

`path` is required. `limit` defaults to 2000 and is in `1..=2000`.
`cursor` defaults to null. Unknown properties are invalid. The first page
omits the cursor. A cursor controls the start; the legacy `offset` is not accepted.

### R015: Grep sections and positions

The exact grep input fields are `pattern`, `path`, `glob`,
`ignore_case`, `before_context`, `after_context`, `context`, and
`max_matches`. Unknown properties are invalid. `pattern` is required.
`path` and `glob` default to null; `ignore_case` defaults to false.
Context counts are in `0..=2000`; `context` overrides before/after.
`max_matches` defaults to and cannot exceed 200.

Each matching file has one R002 header. A match line is
`POSITION:CONTENT`, a context line is `POSITION-CONTENT`, and `--`
separates non-adjacent groups. Every position belongs to the section snapshot.
The final summary is:

```text
[hashline matches=<COUNT> truncated=<true|false> skipped_binary=<COUNT> skipped_invalid_utf8=<COUNT>]
```

The count is the number of match lines, excluding context. The cap is exact;
workers stop at the actual remaining global capacity.

### R016: Grep invalid-text behavior

An explicitly requested file containing NUL returns `binary_file`; invalid
UTF-8 returns `invalid_utf8`. A tree search skips such files, increments the
corresponding final-summary counter, and continues. Grep never emits lossy or
escaped substitute text because either would break position-to-byte identity.
I/O, permission, and pattern failures are not classified as invalid text.

### R017: Structured tool-error taxonomy

Every recognized-tool failure uses this envelope in MCP structured content:

```json
{
  "protocol": "hashline",
  "error": {
    "code": "snapshot_conflict",
    "message": "the file no longer matches the requested snapshot",
    "retryable": true,
    "conflict": {
      "requested_snapshot": "7d9c3af08e1b4f6c9a2d1137f68582a1",
      "current_header": {
        "path": "src/lib.rs",
        "snapshot": "11111111111111111111111111111111",
        "lines": 641,
        "bytes": 18492
      },
      "context": []
    }
  }
}
```

The complete, stable code set is:

| Code | Meaning | Retryable |
|---|---|---:|
| `invalid_request` | Missing field, unknown field, empty batch, or invalid cap | false |
| `invalid_snapshot` | Malformed snapshot text | false |
| `invalid_position` | Malformed token or token/boundary mismatch | false |
| `invalid_range` | Reversed range or invalid terminal ordering | false |
| `overlapping_edits` | Batch ranges conflict | false |
| `invalid_utf8` | File bytes are not strict UTF-8 | false |
| `binary_file` | File or replacement contains NUL | false |
| `file_too_large` | Length exceeds R010 | false |
| `not_found` | Path does not exist | true |
| `not_a_file` | Path resolves to a non-file | false |
| `permission_denied` | Access was denied | true |
| `root_escape` | Restricted workspace boundary was crossed | false |
| `snapshot_conflict` | Current exact bytes have another identity | true |
| `already_exists` | Exclusive-create destination already exists | true |
| `invalid_pattern` | Grep pattern cannot compile | false |
| `io` | Other file-system failure, including failure to obtain stable bytes after one concurrent-read retry | true |

`conflict` is present only for `snapshot_conflict`, and `existing` — the
current header plus at most five context lines — only for `already_exists`.
Error messages are diagnostic, while `code` and typed fields carry semantics.

### R018: MCP error boundary

Malformed arguments for a recognized hashline tool are a structured
`invalid_request` tool result with `isError: true`. File, text, conflict,
range, and pattern failures are also tool results.

Unknown tool names, invalid JSON-RPC framing, transport loss, cancellation,
and server infrastructure failure remain MCP/JSON-RPC errors. They are not
invented members of the tool-error taxonomy. Stdout contains MCP transport
only; diagnostics and tracing remain on stderr.

### R019: Edit success and persistence ordering

A successful edit returns structured content with exactly:
`protocol`, `path`, `previous_snapshot`, `snapshot`, `applied`,
`bytes`, and `lines`.

The response's new snapshot describes the persisted bytes, not merely an
in-memory candidate. The server MUST NOT return success or publish the new
snapshot before persistence succeeds. The complete batch is one logical
transaction. Same-server writes to one canonical path are serialized.
The plan's residual final TOCTOU window with a noncooperating external writer
is documented rather than hidden.

### R020: Cross-tool invariant

A position emitted by read or grep names one exact snapshot. Edit either
applies that byte range to the same exact snapshot or returns R011. It
never edits a different snapshot, even when the referenced line text happens
to be identical.

Snapshot IDs hash content only, so equal bytes at two paths can have equal IDs
inside one process. The request path remains the target; identity equality
does not redirect paths.

### R021: No fuzzy relocation

The protocol has no suffix recovery, contextual fingerprint search, shifted-line retry,
or "closest" match. A stale ID conflicts before position resolution. A
position inconsistent with a matching snapshot is `invalid_position`.
Recovery consists of using the fresh conflict context or performing a new
read/grep, then sending a new request.

### R022: Incompatible surface

The server exposes one protocol. The binary accepts workspace `--root` and
`--restrict` controls, but no scheme, hash-length, chunk-size, checkpoint,
or legacy compatibility option or environment variable.

Edit accepts only `replace { start, end, content }`. It rejects the legacy
`anchor`, `end_anchor`, `insert_after`, `write`, double-encoded edit
arrays, and bare-object edit shorthands. Read rejects `offset`. No source,
CLI, schema, wire, anchor, or persisted-reference compatibility is promised.

### R023: Versioned write

The exact write input is:

```json
{
  "file_path": "src/new.rs",
  "content": "mod fresh;\n",
  "expect": "absent"
}
```

`file_path` and `content` are required. `expect` is required and is either
the literal string `absent` or one canonical R001 snapshot ID. Unknown
properties are invalid. There is no unconditional overwrite: every write
names the destination state it was decided against, exactly as R005 edits
name their snapshot. Mutation tools name their target `file_path`; read-only
tools use `path`.

`content` is the complete new file byte sequence, decoded from the JSON
string as exact UTF-8. R007 applies: NUL anywhere in the content is
`binary_file`. R010 applies to the decoded length. The server never appends,
strips, or normalizes a trailing newline; an empty `content` creates the
R009 empty-file model.

`expect: "absent"` is an exclusive create. When the destination already
exists, the write fails with `already_exists` and leaves the destination
untouched. The structured error carries an `existing` payload with the
current R002 header and at most five current context lines from the start of
the file, mirroring the R011 recovery shape: the model can immediately read,
edit, or overwrite against the returned identity. A destination that exists
but is unreadable or non-text returns its specific file/text error because no
truthful header can be supplied. Missing parent directories are created;
under `--restrict` a destination or parent outside the workspace root is
`root_escape` before any directory is created.

`expect: <snapshot>` is a versioned overwrite. The freshly computed identity
of the current destination bytes must equal the named snapshot; a mismatch is
an R011 `snapshot_conflict` with the standard conflict payload and context
centered on line one. A missing destination is `not_found` — an overwrite
never creates, and `absent` is never satisfied by an existing file, so the
two forms cannot drift into each other under retry.

A successful write returns structured content with exactly: `protocol`,
`path`, `snapshot`, `bytes`, `lines`, and `created`. R019 ordering applies
verbatim: the response snapshot describes persisted destination bytes, and
success is never published before persistence. Creates are exclusive at the
filesystem level: two concurrent creates of one path produce exactly one
success and one `already_exists`. The R019 residual TOCTOU window with a
noncooperating external writer applies to versioned overwrites and is
documented rather than hidden.

### R024: Deterministic glob

The exact glob input fields are `pattern`, `path`, and `max_results`.
Unknown properties are invalid. `pattern` is required and is matched
case-sensitively against walk-root-relative file paths, with `*` stopping at
path separators and `**` crossing them. `path` defaults to null, names the
directory the walk starts from, and must resolve to a directory — a
resolvable non-directory is `not_a_file`. `max_results` defaults to and
cannot exceed 1000.

The walk shares the R015/R016 traversal stance: `.gitignore` is respected,
hidden entries are skipped, and symbolic links are not followed. Glob reports
paths only — file bytes are never read, so text validation and binary
skipping do not apply. Only regular files are reported. A pattern that cannot
compile is `invalid_pattern`; a `path` that does not exist is `not_found`.

Output is one path per line: each walk-root-relative match re-prefixed with
the request `path`, so reported paths feed read, edit, and write directly.
Paths are ordered by last modification time descending with ties broken by
ascending bytewise path comparison. An entry whose modification time cannot
be read sorts as the epoch (oldest), and a path that cannot round-trip
through UTF-8 tool arguments is not reported. When more than `max_results`
paths match, the newest `max_results` under this same ordering are reported
and `truncated` is true. The ordering is fully deterministic for a fixed set
of path and modification-time pairs.

The final summary is:

```text
[hashline files=<COUNT> truncated=<true|false>]
```

The count is the number of reported paths. The cap is exact; truncation is
terminal and asks for a narrower pattern rather than a cursor, matching the
R015 grep stance.

## 2. Canonical requests

Read first page:

```json
{
  "path": "src/lib.rs",
  "limit": 2000
}
```

Read continuation:

```json
{
  "path": "src/lib.rs",
  "limit": 2000,
  "cursor": {
    "snapshot": "7d9c3af08e1b4f6c9a2d1137f68582a1",
    "next": "2001@58102"
  }
}
```

Edit:

```json
{
  "file_path": "src/lib.rs",
  "snapshot": "7d9c3af08e1b4f6c9a2d1137f68582a1",
  "edits": [
    {
      "op": "replace",
      "start": "21@418",
      "end": "23@442",
      "content": "mod protocol;\nmod snapshot;\n"
    }
  ]
}
```

Grep:

```json
{
  "pattern": "SnapshotId",
  "path": "src",
  "glob": "*.rs",
  "ignore_case": false,
  "before_context": 1,
  "after_context": 1,
  "max_matches": 200
}
```

Write, creating a new file:

```json
{
  "file_path": "src/new.rs",
  "content": "mod fresh;\n",
  "expect": "absent"
}
```

Write, replacing a previously read file:

```json
{
  "file_path": "src/lib.rs",
  "content": "pub mod protocol;\n",
  "expect": "7d9c3af08e1b4f6c9a2d1137f68582a1"
}
```

Glob:

```json
{
  "pattern": "**/*.rs",
  "path": "src",
  "max_results": 1000
}
```

## 3. Reference-model role

`apply_reference_edits`, `reference_line_starts`,
`validate_reference_position`, and `reference_context` intentionally favor
clarity over hot-path layout. Later optimized implementations are compared
against them with randomized valid UTF-8, LF, CRLF, bare-CR, Unicode,
unterminated-final-line, empty-file, insertion-order, and non-overlapping-batch
cases.

The reference model performs no file I/O, hashing, caching, locking, or
persistence. Those mechanisms cannot alter its byte-range result.

## 4. Executable rule coverage

Every block below is compiled and run by `cargo test --doc`. The inline
examples above cover R001, R003, and R006; these blocks cover the
remaining rules. Each rule also has a realistic named regression in
`src/protocol.rs`.

### R002

```rust
use hashline::protocol::{SnapshotHeader, SnapshotId};

let header = SnapshotHeader::new(
    "src/lib.rs".to_owned(),
    SnapshotId::from_u128(1),
    1,
    0,
)?;
assert_eq!(
    header.render(),
    r#"[hashline snapshot=00000000000000000000000000000001 lines=1 bytes=0 path="src/lib.rs"]"#
);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R004

```rust
use hashline::protocol::{Position, validate_reference_position};

let bytes = "a\né\n".as_bytes();
validate_reference_position(bytes, Position::new(2, 2)?)?;
assert!(validate_reference_position(bytes, Position::new(2, 3)?).is_err());
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R005

```rust
use hashline::protocol::{EditOperation, Position, apply_reference_edits};

let source = b"a\n";
let edits = [
    EditOperation::replace(Position::new(3, 2)?, Position::new(3, 2)?, "T".to_owned()),
    EditOperation::replace(Position::new(2, 2)?, Position::new(2, 2)?, "S".to_owned()),
    EditOperation::replace(Position::new(2, 2)?, Position::new(3, 2)?, "R".to_owned()),
];
assert_eq!(apply_reference_edits(source, &edits)?, b"a\nSRT");
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R007

```rust
use hashline::protocol::{ContractError, validate_text};

assert_eq!(validate_text("\u{feff}ok".as_bytes())?, "\u{feff}ok");
assert!(matches!(validate_text(b"a\0b"), Err(ContractError::NulFile { .. })));
assert!(matches!(validate_text(b"a\xff"), Err(ContractError::InvalidUtf8 { .. })));
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R008

```rust
use hashline::protocol::{EditOperation, Position, apply_reference_edits};

let source = b"first\r\nsecond\nthird\r";
let edit = EditOperation::replace(
    Position::new(2, 7)?,
    Position::new(3, 14)?,
    "SECOND\r\n".to_owned(),
);
assert_eq!(
    apply_reference_edits(source, &[edit])?,
    b"first\r\nSECOND\r\nthird\r"
);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R009

```rust
use hashline::protocol::{
    EditOperation, Position, apply_reference_edits, reference_header, render_read_line,
    SnapshotId,
};

let header = reference_header("empty".to_owned(), SnapshotId::from_u128(0), b"")?;
assert_eq!((header.lines, header.bytes), (1, 0));
assert_eq!(render_read_line(Position::new(1, 0)?, ""), "1@0|");
let write = EditOperation::replace(
    Position::new(1, 0)?,
    Position::new(2, 0)?,
    "created".to_owned(),
);
assert_eq!(apply_reference_edits(b"", &[write])?, b"created");
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R010

```rust
use hashline::protocol::{MAX_FILE_BYTES, Position, validate_file_size};

validate_file_size(MAX_FILE_BYTES)?;
assert!(validate_file_size(MAX_FILE_BYTES + 1).is_err());
let maximum = Position::new(u64::MAX, u64::MAX)?;
assert_eq!(maximum.to_string().parse::<Position>()?, maximum);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R011

```rust
use hashline::protocol::{
    CONFLICT_CONTEXT_LINES, ErrorCode, ProtocolError, SnapshotId, reference_context,
    reference_header,
};

let current = b"one\ntwo\nthree\n";
let current_id = SnapshotId::from_u128(2);
let error = ProtocolError::snapshot_conflict(
    SnapshotId::from_u128(1),
    reference_header("x".to_owned(), current_id, current)?,
    reference_context(current, 2)?,
    "stale snapshot".to_owned(),
)?;
assert_eq!(error.code, ErrorCode::SnapshotConflict);
assert!(error.retryable);
assert!(error.conflict.as_ref().unwrap().context.len() <= CONFLICT_CONTEXT_LINES);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R012

```rust
use hashline::protocol::SnapshotId;

let requested = SnapshotId::from_u128(1);
let recomputed_after_eviction = SnapshotId::from_u128(1);
let recomputed_after_restart = SnapshotId::from_u128(2);
assert_eq!(requested, recomputed_after_eviction);
assert_ne!(requested, recomputed_after_restart);
```

### R013

```rust
use hashline::protocol::{
    PageCursor, Position, SnapshotId, validate_reference_cursor,
};

let source = b"one\ntwo\n";
let snapshot = SnapshotId::from_u128(1);
let cursor = PageCursor {
    snapshot,
    next: Position::new(2, 4)?,
};
assert_eq!(
    validate_reference_cursor("page.txt", source, snapshot, &cursor),
    Ok(cursor.next),
);
assert_eq!(
    cursor.render_footer(),
    "[hashline next snapshot=00000000000000000000000000000001 position=2@4]"
);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R014

```rust
use hashline::protocol::{MAX_PAGE_LINES, ReadRequest};
use serde_json::json;

let request: ReadRequest = serde_json::from_value(json!({"path": "src/lib.rs"}))?;
assert_eq!(request.limit, MAX_PAGE_LINES);
assert!(serde_json::from_value::<ReadRequest>(
    json!({"path": "src/lib.rs", "offset": 2})
).is_err());
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R015

```rust
use hashline::protocol::{
    GrepRequest, GrepSummary, MAX_GREP_MATCHES, remaining_match_capacity,
};
use serde_json::json;

let request: GrepRequest = serde_json::from_value(json!({
    "pattern": "SnapshotId",
    "before_context": 1,
    "after_context": 2,
    "context": 3
}))?;
assert_eq!(request.effective_context(), (3, 3));
assert_eq!(request.max_matches, MAX_GREP_MATCHES);
assert_eq!(remaining_match_capacity(200, 199)?, 1);
assert_eq!(
    GrepSummary {
        matches: 1,
        truncated: false,
        skipped_binary: 0,
        skipped_invalid_utf8: 0,
    }.render(),
    "[hashline matches=1 truncated=false skipped_binary=0 skipped_invalid_utf8=0]"
);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R016

```rust
use hashline::protocol::{
    ContractError, GrepTarget, GrepText, classify_grep_text,
};

assert!(matches!(
    classify_grep_text(b"a\0b", GrepTarget::ExplicitFile),
    Err(ContractError::NulFile { .. })
));
assert_eq!(
    classify_grep_text(b"a\0b", GrepTarget::TreeEntry),
    Ok(GrepText::SkipBinary)
);
assert_eq!(
    classify_grep_text(b"a\xff", GrepTarget::TreeEntry),
    Ok(GrepText::SkipInvalidUtf8)
);
```

### R017

```rust
use hashline::protocol::ErrorCode;

assert_eq!(ErrorCode::ALL.len(), 16);
assert!(ErrorCode::SnapshotConflict.retryable());
assert!(ErrorCode::AlreadyExists.retryable());
assert!(!ErrorCode::InvalidPosition.retryable());
```

### R018

```rust
use hashline::protocol::{ErrorCode, ErrorResponse, ProtocolError};
use serde_json::Value;

let response = ErrorResponse::new(ProtocolError::new(
    ErrorCode::InvalidRequest,
    "unknown field".to_owned(),
));
let Value::Object(object) = serde_json::to_value(response)? else {
    unreachable!();
};
assert!(!object.contains_key("jsonrpc"));
assert!(!object.contains_key("id"));
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R019

```rust
use hashline::protocol::{EditSuccess, SnapshotId};

let persisted = SnapshotId::from_u128(2);
let response = EditSuccess::new(
    "src/lib.rs".to_owned(),
    SnapshotId::from_u128(1),
    persisted,
    1,
    7,
    2,
);
let value = serde_json::to_value(response)?;
assert_eq!(value["snapshot"], persisted.to_string());
assert_eq!(value["protocol"], "hashline");
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R020

```rust
use hashline::protocol::{
    EditOperation, EditRequest, ErrorCode, Position, SnapshotId,
    apply_versioned_reference_edits,
};

let request = EditRequest {
    file_path: "target.txt".to_owned(),
    snapshot: SnapshotId::from_u128(1),
    edits: vec![EditOperation::replace(
        Position::new(1, 0)?,
        Position::new(2, 5)?,
        "same\n".to_owned(),
    )],
};
let error = apply_versioned_reference_edits(
    b"SAME\n",
    SnapshotId::from_u128(2),
    &request,
).unwrap_err();
assert_eq!(error.code, ErrorCode::SnapshotConflict);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R021

```rust
use hashline::protocol::{
    EditOperation, EditRequest, ErrorCode, Position, SnapshotId,
    apply_versioned_reference_edits,
};

let request = EditRequest {
    file_path: "target.txt".to_owned(),
    snapshot: SnapshotId::from_u128(1),
    edits: vec![EditOperation::replace(
        Position::new(99, 999)?,
        Position::new(100, 1_000)?,
        "never relocated".to_owned(),
    )],
};
let error = apply_versioned_reference_edits(
    b"duplicated\nduplicated\n",
    SnapshotId::from_u128(2),
    &request,
).unwrap_err();
assert_eq!(error.code, ErrorCode::SnapshotConflict);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R022

```rust
use hashline::protocol::{EditRequest, ReadRequest};
use serde_json::json;

assert!(serde_json::from_value::<ReadRequest>(
    json!({"path": "x", "offset": 1})
).is_err());
assert!(serde_json::from_value::<EditRequest>(json!({
    "file_path": "x",
    "snapshot": "00000000000000000000000000000001",
    "edits": [{"op": "insert_after", "anchor": "0:", "content": "x"}]
})).is_err());
```

### R023

```rust
use hashline::protocol::{
    ErrorCode, SnapshotId, WriteExpect, WriteRequest, validate_reference_write,
};

let create = WriteRequest {
    file_path: "src/new.rs".to_owned(),
    content: "mod fresh;\n".to_owned(),
    expect: "absent".parse()?,
};
assert_eq!(validate_reference_write(None, &create), Ok(true));

let current = b"mod old;\n";
let current_id = SnapshotId::from_u128(1);
let error = validate_reference_write(Some((current, current_id)), &create).unwrap_err();
assert_eq!(error.code, ErrorCode::AlreadyExists);
assert!(error.retryable);
assert_eq!(error.existing.unwrap().current_header.snapshot, current_id);

let overwrite = WriteRequest {
    file_path: "src/new.rs".to_owned(),
    content: "mod newer;\n".to_owned(),
    expect: WriteExpect::Snapshot(current_id),
};
assert_eq!(
    validate_reference_write(Some((current, current_id)), &overwrite),
    Ok(false)
);
assert_eq!(
    validate_reference_write(Some((current, SnapshotId::from_u128(2))), &overwrite)
        .unwrap_err()
        .code,
    ErrorCode::SnapshotConflict
);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### R024

```rust
use std::time::{Duration, UNIX_EPOCH};

use hashline::protocol::{
    GlobEntry, GlobRequest, GlobSummary, MAX_GLOB_RESULTS, sort_reference_glob_entries,
};

let request: GlobRequest = serde_json::from_value(serde_json::json!({"pattern": "**/*.rs"}))?;
assert_eq!(request.max_results, MAX_GLOB_RESULTS);
request.validate()?;

let newer = UNIX_EPOCH + Duration::from_secs(2);
let older = UNIX_EPOCH + Duration::from_secs(1);
let mut entries = vec![
    GlobEntry { path: "src/b.rs".to_owned(), modified: older },
    GlobEntry { path: "src/a.rs".to_owned(), modified: newer },
    GlobEntry { path: "src/A.rs".to_owned(), modified: newer },
];
sort_reference_glob_entries(&mut entries);
let paths = entries.iter().map(|entry| entry.path.as_str()).collect::<Vec<_>>();
assert_eq!(paths, ["src/A.rs", "src/a.rs", "src/b.rs"]);
assert_eq!(
    GlobSummary { files: 3, truncated: false }.render(),
    "[hashline files=3 truncated=false]"
);
# Ok::<(), Box<dyn std::error::Error>>(())
```
