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
#![doc = include_str!("../docs/protocol.md")]

use std::{
    borrow::Cow,
    fmt::{self, Display, Write as _},
    str::FromStr,
    time::SystemTime,
};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Stable protocol tag serialized in structured responses.
pub const PROTOCOL: &str = "hashline";

/// Number of lowercase hexadecimal characters in a snapshot ID.
pub const SNAPSHOT_ID_HEX_LEN: usize = 32;

/// Default and maximum number of lines in one read page.
pub const MAX_PAGE_LINES: u16 = 2_000;

/// Default and maximum number of grep match lines.
pub const MAX_GREP_MATCHES: u16 = 200;

/// Default and maximum number of glob result paths.
pub const MAX_GLOB_RESULTS: u16 = 1_000;

/// Maximum context distance accepted by grep.
pub const MAX_CONTEXT_LINES: u16 = 2_000;

/// Maximum edit operations in one atomic request.
pub const MAX_EDIT_OPERATIONS: usize = 1_024;

/// Maximum file size accepted by the protocol.
pub const MAX_FILE_BYTES: u64 = i64::MAX as u64;

/// Maximum number of fresh lines returned with a snapshot conflict.
pub const CONFLICT_CONTEXT_LINES: usize = 5;

const SNAPSHOT_CONFLICT_MESSAGE: &str = "the file no longer matches the requested snapshot";
const ALREADY_EXISTS_MESSAGE: &str = "the destination file already exists";

/// Frozen semantic rules covered by the Phase 1 exit gate.
pub const SEMANTIC_RULE_IDS: [&str; 24] = [
    "R001", "R002", "R003", "R004", "R005", "R006", "R007", "R008", "R009", "R010", "R011", "R012",
    "R013", "R014", "R015", "R016", "R017", "R018", "R019", "R020", "R021", "R022", "R023", "R024",
];

const SNAPSHOT_ID_PATTERN: &str = "^[0-9a-f]{32}$";
const POSITION_PATTERN: &str = "^[1-9][0-9]*@(0|[1-9][0-9]*)$";
const WRITE_EXPECT_PATTERN: &str = "^(absent|[0-9a-f]{32})$";

/// An opaque, process-scoped identity for one exact file byte sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotId(u128);

impl SnapshotId {
    /// Construct an ID from its raw 128-bit representation.
    #[must_use]
    pub const fn from_u128(value: u128) -> Self {
        Self(value)
    }

    /// Return the raw 128-bit representation.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

impl Display for SnapshotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:032x}", self.0)
    }
}

/// Error returned when parsing a canonical snapshot ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SnapshotIdParseError {
    /// The token is not exactly 32 bytes.
    #[error("snapshot ID must contain exactly 32 lowercase hexadecimal characters")]
    InvalidLength,
    /// The token contains a non-lowercase-hexadecimal byte.
    #[error("snapshot ID contains a character outside [0-9a-f]")]
    InvalidCharacter,
}

impl FromStr for SnapshotId {
    type Err = SnapshotIdParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.len() != SNAPSHOT_ID_HEX_LEN {
            return Err(SnapshotIdParseError::InvalidLength);
        }
        if !input
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SnapshotIdParseError::InvalidCharacter);
        }

        u128::from_str_radix(input, 16)
            .map(Self)
            .map_err(|_| SnapshotIdParseError::InvalidCharacter)
    }
}

impl Serialize for SnapshotId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SnapshotId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for SnapshotId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "SnapshotId".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::SnapshotId").into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": SNAPSHOT_ID_PATTERN,
            "minLength": SNAPSHOT_ID_HEX_LEN,
            "maxLength": SNAPSHOT_ID_HEX_LEN,
        })
    }
}

/// A canonical logical-line and byte-offset pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    line: u64,
    byte: u64,
}

impl Position {
    /// Construct a position, rejecting line zero.
    ///
    /// # Errors
    ///
    /// Returns PositionParseError::ZeroLine when line is zero.
    pub const fn new(line: u64, byte: u64) -> Result<Self, PositionParseError> {
        if line == 0 {
            return Err(PositionParseError::ZeroLine);
        }
        Ok(Self { line, byte })
    }

    /// Return the 1-based logical line number.
    #[must_use]
    pub const fn line(self) -> u64 {
        self.line
    }

    /// Return the authoritative UTF-8 byte offset.
    #[must_use]
    pub const fn byte(self) -> u64 {
        self.byte
    }
}

impl Display for Position {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.line, self.byte)
    }
}

/// Error returned when parsing a canonical position token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PositionParseError {
    /// The token has no at-sign separator.
    #[error("position must contain one @ separator")]
    MissingSeparator,
    /// The token has more than one at-sign separator.
    #[error("position must contain exactly one @ separator")]
    ExtraSeparator,
    /// The line component is not canonical decimal.
    #[error("position line must be canonical unsigned decimal")]
    InvalidLine,
    /// The line component is zero.
    #[error("position line must be at least 1")]
    ZeroLine,
    /// The line component exceeds u64.
    #[error("position line exceeds u64")]
    LineOverflow,
    /// The byte component is not canonical decimal.
    #[error("position byte must be canonical unsigned decimal")]
    InvalidByte,
    /// The byte component exceeds u64.
    #[error("position byte exceeds u64")]
    ByteOverflow,
}

#[derive(Debug, Clone, Copy)]
enum DecimalComponent {
    Line,
    Byte,
}

fn parse_canonical_decimal(
    input: &str,
    component: DecimalComponent,
) -> Result<u64, PositionParseError> {
    let invalid = match component {
        DecimalComponent::Line => PositionParseError::InvalidLine,
        DecimalComponent::Byte => PositionParseError::InvalidByte,
    };
    let overflow = match component {
        DecimalComponent::Line => PositionParseError::LineOverflow,
        DecimalComponent::Byte => PositionParseError::ByteOverflow,
    };

    if input.is_empty()
        || (input.len() > 1 && input.starts_with('0'))
        || !input.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid);
    }

    let mut value = 0_u64;
    for byte in input.bytes() {
        value = value
            .checked_mul(10)
            .and_then(|current| current.checked_add(u64::from(byte - b'0')))
            .ok_or(overflow)?;
    }
    if matches!(component, DecimalComponent::Line) && value == 0 {
        return Err(PositionParseError::ZeroLine);
    }
    Ok(value)
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (line, byte) = input
            .split_once('@')
            .ok_or(PositionParseError::MissingSeparator)?;
        if byte.contains('@') {
            return Err(PositionParseError::ExtraSeparator);
        }

        Self::new(
            parse_canonical_decimal(line, DecimalComponent::Line)?,
            parse_canonical_decimal(byte, DecimalComponent::Byte)?,
        )
    }
}

impl Serialize for Position {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Position {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Position {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "Position".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::Position").into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": POSITION_PATTERN,
            "minLength": 3,
            "maxLength": 41,
        })
    }
}

/// The single protocol tag accepted and emitted by structured content.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ProtocolTag {
    /// The hashline protocol.
    #[default]
    #[serde(rename = "hashline")]
    Hashline,
}

/// Metadata preceding one read or grep file section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotHeader {
    /// File path associated with the snapshot.
    pub path: String,
    /// Exact-byte identity.
    pub snapshot: SnapshotId,
    /// Logical line count, always at least one.
    #[schemars(range(min = 1))]
    pub lines: u64,
    /// Exact byte count.
    #[schemars(range(max = 9_223_372_036_854_775_807_u64))]
    pub bytes: u64,
}

impl SnapshotHeader {
    /// Construct validated snapshot metadata.
    ///
    /// # Errors
    ///
    /// Returns InvalidLineCount for zero lines or FileTooLarge when bytes
    /// exceeds the protocol cap.
    pub fn new(
        path: String,
        snapshot: SnapshotId,
        lines: u64,
        bytes: u64,
    ) -> Result<Self, ContractError> {
        if lines == 0 {
            return Err(ContractError::InvalidLineCount);
        }
        validate_file_size(bytes)?;
        Ok(Self {
            path,
            snapshot,
            lines,
            bytes,
        })
    }

    /// Render the canonical textual section header.
    #[must_use]
    pub fn render(&self) -> String {
        let path = serde_json::to_string(&self.path)
            .expect("serializing a Rust String as JSON cannot fail");
        format!(
            "[hashline snapshot={} lines={} bytes={} path={path}]",
            self.snapshot, self.lines, self.bytes
        )
    }
}

/// Cursor for the next read page in the same exact snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PageCursor {
    /// Snapshot to which next belongs.
    pub snapshot: SnapshotId,
    /// Next logical line start.
    pub next: Position,
}

impl PageCursor {
    /// Render the exact continuation footer for a non-terminal read page.
    #[must_use]
    pub fn render_footer(&self) -> String {
        format!(
            "[hashline next snapshot={} position={}]",
            self.snapshot, self.next
        )
    }
}

const fn default_read_limit() -> u16 {
    MAX_PAGE_LINES
}

/// Frozen input schema for `read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    /// Path of the file to read.
    pub path: String,
    /// Maximum logical lines in this page.
    #[serde(default = "default_read_limit")]
    #[schemars(default = "default_read_limit", range(min = 1, max = 2_000))]
    pub limit: u16,
    /// Continuation cursor; absent for the first page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<PageCursor>,
}

impl ReadRequest {
    /// Validate request limits before file work begins.
    ///
    /// # Errors
    ///
    /// Returns InvalidReadLimit unless the limit is in the fixed range.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.limit == 0 || self.limit > MAX_PAGE_LINES {
            return Err(ContractError::InvalidReadLimit { limit: self.limit });
        }
        Ok(())
    }
}

const fn default_max_matches() -> u16 {
    MAX_GREP_MATCHES
}

/// Output shape for one grep response.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    /// R002-headed sections with position-annotated match and context lines.
    #[default]
    Content,
    /// One matching file path per line.
    FilesWithMatches,
    /// One `PATH: N` match-count line per matching file.
    Count,
}

/// Frozen input schema for `grep`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepRequest {
    /// Regular expression to search for.
    pub pattern: String,
    /// File or directory; absent means the workspace root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Optional glob override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Whether matching ignores case according to the regex engine.
    #[serde(default)]
    pub ignore_case: bool,
    /// Context lines before each match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(max = 2_000))]
    pub before_context: Option<u16>,
    /// Context lines after each match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(max = 2_000))]
    pub after_context: Option<u16>,
    /// Symmetric context overriding before and after counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(max = 2_000))]
    pub context: Option<u16>,
    /// Exact global match-line cap.
    #[serde(default = "default_max_matches")]
    #[schemars(default = "default_max_matches", range(min = 1, max = 200))]
    pub max_matches: u16,
    /// Response shape; content is the default.
    #[serde(default)]
    pub output_mode: GrepOutputMode,
}

impl GrepRequest {
    /// Resolve context override semantics into explicit before/after counts.
    #[must_use]
    pub fn effective_context(&self) -> (u16, u16) {
        self.context.map_or(
            (
                self.before_context.unwrap_or(0),
                self.after_context.unwrap_or(0),
            ),
            |context| (context, context),
        )
    }
    /// Validate grep caps before traversal or pattern compilation.
    ///
    /// # Errors
    ///
    /// Returns a contract error when a context count or match cap exceeds its
    /// normative bound.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.max_matches == 0 || self.max_matches > MAX_GREP_MATCHES {
            return Err(ContractError::InvalidGrepLimit {
                limit: self.max_matches,
            });
        }
        for value in [self.before_context, self.after_context, self.context]
            .into_iter()
            .flatten()
        {
            if value > MAX_CONTEXT_LINES {
                return Err(ContractError::InvalidContextLimit { limit: value });
            }
        }
        if self.output_mode != GrepOutputMode::Content
            && (self.before_context.is_some()
                || self.after_context.is_some()
                || self.context.is_some())
        {
            return Err(ContractError::ContextOutsideContentMode);
        }
        Ok(())
    }
}

/// The sole edit operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub enum EditOperation {
    /// Replace one half-open line-boundary range with exact UTF-8 content.
    Replace {
        /// Inclusive start boundary.
        start: Position,
        /// Exclusive end boundary.
        end: Position,
        /// Exact replacement text; NUL is forbidden.
        content: String,
    },
}

impl EditOperation {
    /// Construct a replace operation.
    #[must_use]
    pub fn replace(start: Position, end: Position, content: String) -> Self {
        Self::Replace {
            start,
            end,
            content,
        }
    }

    /// Return the inclusive start position.
    #[must_use]
    pub const fn start(&self) -> Position {
        match self {
            Self::Replace { start, .. } => *start,
        }
    }

    /// Return the exclusive end position.
    #[must_use]
    pub const fn end(&self) -> Position {
        match self {
            Self::Replace { end, .. } => *end,
        }
    }

    /// Return the exact replacement content.
    #[must_use]
    pub fn content(&self) -> &str {
        match self {
            Self::Replace { content, .. } => content,
        }
    }
}

/// Frozen input schema for `edit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditRequest {
    /// Target file path.
    pub file_path: String,
    /// Exact pre-edit snapshot.
    pub snapshot: SnapshotId,
    /// Atomic replace batch.
    #[schemars(length(min = 1, max = 1_024))]
    pub edits: Vec<EditOperation>,
}

impl EditRequest {
    /// Validate batch shape and replacement text.
    ///
    /// Snapshot and boundary validation require current file bytes and are
    /// performed by apply_reference_edits or its optimized equivalent.
    ///
    /// # Errors
    ///
    /// Returns a contract error for an empty or oversized batch or NUL content.
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_edit_count(self.edits.len())?;
        for (index, operation) in self.edits.iter().enumerate() {
            if let Some(byte) = operation.content().bytes().position(|byte| byte == 0) {
                return Err(ContractError::NulReplacement { index, byte });
            }
        }
        Ok(())
    }
}

/// The destination precondition named by one write request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteExpect {
    /// The destination must not exist; the write creates it.
    Absent,
    /// The destination bytes must currently have this exact identity.
    Snapshot(SnapshotId),
}

impl Display for WriteExpect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => formatter.write_str("absent"),
            Self::Snapshot(snapshot) => Display::fmt(snapshot, formatter),
        }
    }
}

/// Error returned when parsing a write destination precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WriteExpectParseError {
    /// The token is neither the literal absent nor a canonical snapshot ID.
    #[error("write expect must be \"absent\" or 32 lowercase hexadecimal characters")]
    Invalid,
}

impl FromStr for WriteExpect {
    type Err = WriteExpectParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input == "absent" {
            return Ok(Self::Absent);
        }
        input
            .parse()
            .map(Self::Snapshot)
            .map_err(|_| WriteExpectParseError::Invalid)
    }
}

impl Serialize for WriteExpect {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for WriteExpect {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for WriteExpect {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "WriteExpect".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::WriteExpect").into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": WRITE_EXPECT_PATTERN,
            "minLength": 6,
            "maxLength": SNAPSHOT_ID_HEX_LEN,
        })
    }
}

/// Frozen input schema for `write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteRequest {
    /// Target file path.
    pub file_path: String,
    /// Complete new file content; NUL is forbidden.
    pub content: String,
    /// Destination precondition: the literal absent or an exact snapshot.
    pub expect: WriteExpect,
}

impl WriteRequest {
    /// Validate content shape before any file work begins.
    ///
    /// Destination-state validation requires current file bytes and is
    /// performed by validate_reference_write or its optimized equivalent.
    ///
    /// # Errors
    ///
    /// Returns a contract error for NUL content or content above the size cap.
    pub fn validate(&self) -> Result<(), ContractError> {
        if let Some(byte) = self.content.bytes().position(|byte| byte == 0) {
            return Err(ContractError::NulWriteContent { byte });
        }
        validate_file_size(
            u64::try_from(self.content.len())
                .map_err(|_| ContractError::FileTooLarge { bytes: u64::MAX })?,
        )
    }
}

/// Whether a rendered grep line is a match or context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrepLineKind {
    /// A matching logical line.
    Match,
    /// A non-matching context line.
    Context,
}

/// One position-bearing grep output line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepLine {
    /// Position at the logical line start.
    pub position: Position,
    /// Match or context classification.
    pub kind: GrepLineKind,
    /// Display content without its line terminator.
    pub content: String,
}

impl GrepLine {
    /// Render the exact grep-style line grammar.
    #[must_use]
    pub fn render(&self) -> String {
        let separator = match self.kind {
            GrepLineKind::Match => ':',
            GrepLineKind::Context => '-',
        };
        format!("{}{separator}{}", self.position, self.content)
    }
}

/// Terminal counters emitted after a grep response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepSummary {
    /// Number of match lines, excluding context.
    pub matches: u64,
    /// Whether the exact global match cap stopped traversal.
    pub truncated: bool,
    /// Tree entries skipped because they contained NUL.
    pub skipped_binary: u64,
    /// Tree entries skipped because they were not strict UTF-8.
    pub skipped_invalid_utf8: u64,
}

impl GrepSummary {
    /// Render the exact terminal grep summary.
    #[must_use]
    pub fn render(self) -> String {
        format!(
            "[hashline matches={} truncated={} skipped_binary={} skipped_invalid_utf8={}]",
            self.matches, self.truncated, self.skipped_binary, self.skipped_invalid_utf8
        )
    }
}

const fn default_max_results() -> u16 {
    MAX_GLOB_RESULTS
}

/// Frozen input schema for `glob`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobRequest {
    /// Glob pattern matched case-sensitively against workspace-relative paths.
    pub pattern: String,
    /// Directory the walk starts from; absent means the workspace root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Exact global result cap.
    #[serde(default = "default_max_results")]
    #[schemars(default = "default_max_results", range(min = 1, max = 1_000))]
    pub max_results: u16,
}

impl GlobRequest {
    /// Validate the result cap before traversal or pattern compilation.
    ///
    /// # Errors
    ///
    /// Returns InvalidGlobLimit unless the cap is in the fixed range.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.max_results == 0 || self.max_results > MAX_GLOB_RESULTS {
            return Err(ContractError::InvalidGlobLimit {
                limit: self.max_results,
            });
        }
        Ok(())
    }
}

/// Terminal counters emitted after a glob response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobSummary {
    /// Number of reported file paths.
    pub files: u64,
    /// Whether the exact global result cap stopped reporting.
    pub truncated: bool,
}

impl GlobSummary {
    /// Render the exact terminal glob summary.
    #[must_use]
    pub fn render(self) -> String {
        format!(
            "[hashline files={} truncated={}]",
            self.files, self.truncated
        )
    }
}

/// One discovered file before deterministic glob ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobEntry {
    /// Workspace-relative path.
    pub path: String,
    /// Last modification time; entries without one use the epoch.
    pub modified: SystemTime,
}

/// Order glob entries newest-first with an ascending bytewise path tie-break.
pub fn sort_reference_glob_entries(entries: &mut [GlobEntry]) {
    entries.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.path.as_bytes().cmp(right.path.as_bytes()))
    });
}

/// Whether grep was explicitly given one file or is traversing a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrepTarget {
    /// A file explicitly named by the caller.
    ExplicitFile,
    /// One entry discovered during directory traversal.
    TreeEntry,
}

/// Strict-text decision for a grep input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrepText<'a> {
    /// Search these validated UTF-8 bytes.
    Search(&'a str),
    /// Skip a tree entry containing NUL and increment skipped_binary.
    SkipBinary,
    /// Skip an invalid UTF-8 tree entry and increment skipped_invalid_utf8.
    SkipInvalidUtf8,
}

/// Apply the explicit-file versus tree-entry invalid-text policy.
///
/// # Errors
///
/// Explicit files return BinaryFile or InvalidUtf8. FileTooLarge is returned
/// for either target and is never converted into a skip.
pub fn classify_grep_text(bytes: &[u8], target: GrepTarget) -> Result<GrepText<'_>, ContractError> {
    match validate_text(bytes) {
        Ok(text) => Ok(GrepText::Search(text)),
        Err(ContractError::NulFile { .. }) if target == GrepTarget::TreeEntry => {
            Ok(GrepText::SkipBinary)
        }
        Err(ContractError::InvalidUtf8 { .. }) if target == GrepTarget::TreeEntry => {
            Ok(GrepText::SkipInvalidUtf8)
        }
        Err(error) => Err(error),
    }
}

/// Return the actual remaining global grep match capacity.
///
/// # Errors
///
/// Returns InvalidGrepLimit when max_matches is outside 1..=200.
pub fn remaining_match_capacity(
    max_matches: u16,
    reported_matches: usize,
) -> Result<usize, ContractError> {
    if max_matches == 0 || max_matches > MAX_GREP_MATCHES {
        return Err(ContractError::InvalidGrepLimit { limit: max_matches });
    }
    Ok(usize::from(max_matches).saturating_sub(reported_matches))
}

/// One fresh context line carried by a snapshot conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextLine {
    /// Current logical-line position.
    pub position: Position,
    /// Current line content without its line terminator.
    pub content: String,
}

/// Stable classification for recognized-tool errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Missing or unknown fields, empty edits, or an invalid request cap.
    InvalidRequest,
    /// Malformed snapshot text.
    InvalidSnapshot,
    /// Malformed or snapshot-inconsistent position.
    InvalidPosition,
    /// Reversed or otherwise invalid range.
    InvalidRange,
    /// Conflicting ranges in one batch.
    OverlappingEdits,
    /// File bytes are not strict UTF-8.
    InvalidUtf8,
    /// File or replacement contains NUL.
    BinaryFile,
    /// File exceeds the protocol size cap.
    FileTooLarge,
    /// Path does not exist.
    NotFound,
    /// Path is not a regular file.
    NotAFile,
    /// File-system access was denied.
    PermissionDenied,
    /// Restricted workspace boundary was crossed.
    RootEscape,
    /// Current exact bytes do not match the requested snapshot.
    SnapshotConflict,
    /// Exclusive-create destination already exists.
    AlreadyExists,
    /// Grep pattern cannot compile.
    InvalidPattern,
    /// Other file-system failure.
    Io,
}

impl ErrorCode {
    /// Complete stable tool-error code set.
    pub const ALL: [Self; 16] = [
        Self::InvalidRequest,
        Self::InvalidSnapshot,
        Self::InvalidPosition,
        Self::InvalidRange,
        Self::OverlappingEdits,
        Self::InvalidUtf8,
        Self::BinaryFile,
        Self::FileTooLarge,
        Self::NotFound,
        Self::NotAFile,
        Self::PermissionDenied,
        Self::RootEscape,
        Self::SnapshotConflict,
        Self::AlreadyExists,
        Self::InvalidPattern,
        Self::Io,
    ];

    /// Whether retrying after refreshing external state can succeed.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::NotFound
                | Self::PermissionDenied
                | Self::SnapshotConflict
                | Self::AlreadyExists
                | Self::Io
        )
    }
}

/// Typed data attached only to a snapshot conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotConflict {
    /// Snapshot named by the failed request.
    pub requested_snapshot: SnapshotId,
    /// Fresh metadata for current exact bytes.
    pub current_header: SnapshotHeader,
    /// At most five current context lines.
    pub context: Vec<ContextLine>,
}

/// Typed data attached only to an already-exists failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExistingFile {
    /// Fresh metadata for the current destination bytes.
    pub current_header: SnapshotHeader,
    /// At most five current context lines.
    pub context: Vec<ContextLine>,
}

/// Structured failure for one recognized tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    /// Stable machine-readable classification.
    pub code: ErrorCode,
    /// Human-readable diagnostic.
    pub message: String,
    /// Whether refreshing external state can make a retry succeed.
    pub retryable: bool,
    /// Present only for snapshot_conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<Box<SnapshotConflict>>,
    /// Present only for already_exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing: Option<Box<ExistingFile>>,
}

impl ProtocolError {
    /// Construct a non-conflict tool error.
    #[must_use]
    pub fn new(code: ErrorCode, message: String) -> Self {
        Self {
            code,
            message,
            retryable: code.retryable(),
            conflict: None,
            existing: None,
        }
    }

    /// Construct a fully populated snapshot conflict.
    ///
    /// # Errors
    ///
    /// Returns TooManyConflictLines when context exceeds the fixed cap.
    pub fn snapshot_conflict(
        requested_snapshot: SnapshotId,
        current_header: SnapshotHeader,
        context: Vec<ContextLine>,
        message: String,
    ) -> Result<Self, ContractError> {
        if context.len() > CONFLICT_CONTEXT_LINES {
            return Err(ContractError::TooManyConflictLines {
                lines: context.len(),
            });
        }
        Ok(Self {
            code: ErrorCode::SnapshotConflict,
            message,
            retryable: true,
            conflict: Some(Box::new(SnapshotConflict {
                requested_snapshot,
                current_header,
                context,
            })),
            existing: None,
        })
    }

    /// Construct a fully populated already-exists failure.
    ///
    /// # Errors
    ///
    /// Returns TooManyConflictLines when context exceeds the fixed cap.
    pub fn already_exists(
        current_header: SnapshotHeader,
        context: Vec<ContextLine>,
        message: String,
    ) -> Result<Self, ContractError> {
        if context.len() > CONFLICT_CONTEXT_LINES {
            return Err(ContractError::TooManyConflictLines {
                lines: context.len(),
            });
        }
        Ok(Self {
            code: ErrorCode::AlreadyExists,
            message,
            retryable: true,
            conflict: None,
            existing: Some(Box::new(ExistingFile {
                current_header,
                context,
            })),
        })
    }
}

/// Convert reference-model validation failures into the stable tool envelope.
impl From<ContractError> for ProtocolError {
    fn from(error: ContractError) -> Self {
        let code = error.code();
        Self::new(code, error.to_string())
    }
}

/// MCP structured-content envelope for a tool error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    /// Fixed protocol tag.
    pub protocol: ProtocolTag,
    /// Typed tool failure.
    pub error: ProtocolError,
}

impl ErrorResponse {
    /// Wrap a typed error with the protocol tag.
    #[must_use]
    pub const fn new(error: ProtocolError) -> Self {
        Self {
            protocol: ProtocolTag::Hashline,
            error,
        }
    }
}

/// Structured content returned after persistence of a successful edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditSuccess {
    /// Fixed protocol tag.
    pub protocol: ProtocolTag,
    /// Edited path.
    pub path: String,
    /// Snapshot validated before the edit.
    pub previous_snapshot: SnapshotId,
    /// Snapshot of successfully persisted result bytes.
    pub snapshot: SnapshotId,
    /// Number of operations applied.
    pub applied: usize,
    /// Persisted byte count.
    pub bytes: u64,
    /// Persisted logical line count.
    pub lines: u64,
}

impl EditSuccess {
    /// Construct a successful persisted-edit response.
    #[must_use]
    pub fn new(
        path: String,
        previous_snapshot: SnapshotId,
        snapshot: SnapshotId,
        applied: usize,
        bytes: u64,
        lines: u64,
    ) -> Self {
        Self {
            protocol: ProtocolTag::Hashline,
            path,
            previous_snapshot,
            snapshot,
            applied,
            bytes,
            lines,
        }
    }
}

/// Structured content returned after persistence of a successful write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteSuccess {
    /// Fixed protocol tag.
    pub protocol: ProtocolTag,
    /// Written path.
    pub path: String,
    /// Snapshot of successfully persisted destination bytes.
    pub snapshot: SnapshotId,
    /// Persisted byte count.
    pub bytes: u64,
    /// Persisted logical line count.
    pub lines: u64,
    /// Whether the write created the destination.
    pub created: bool,
}

impl WriteSuccess {
    /// Construct a successful persisted-write response.
    #[must_use]
    pub fn new(path: String, snapshot: SnapshotId, bytes: u64, lines: u64, created: bool) -> Self {
        Self {
            protocol: ProtocolTag::Hashline,
            path,
            snapshot,
            bytes,
            lines,
            created,
        }
    }
}

/// Errors produced by request validation and the slow reference model.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContractError {
    /// Read page limit is outside the normative range.
    #[error("read limit must be in 1..={MAX_PAGE_LINES}, got {limit}")]
    InvalidReadLimit {
        /// Rejected value.
        limit: u16,
    },
    /// Grep match cap is outside the normative range.
    #[error("grep max_matches must be in 1..={MAX_GREP_MATCHES}, got {limit}")]
    InvalidGrepLimit {
        /// Rejected value.
        limit: u16,
    },
    /// Grep context exceeds the normative bound.
    #[error("grep context must be <= {MAX_CONTEXT_LINES}, got {limit}")]
    InvalidContextLimit {
        /// Rejected value.
        limit: u16,
    },
    /// Glob result cap is outside the normative range.
    #[error("glob max_results must be in 1..={MAX_GLOB_RESULTS}, got {limit}")]
    InvalidGlobLimit {
        /// Rejected value.
        limit: u16,
    },
    /// Context lines were requested outside content output.
    #[error("grep context requires output_mode \"content\"")]
    ContextOutsideContentMode,
    /// Edit batch is empty.
    #[error("edit batch must contain at least one operation")]
    EmptyEditBatch,
    /// Edit batch exceeds the operation cap.
    #[error("edit batch has {operations} operations; maximum is {MAX_EDIT_OPERATIONS}")]
    TooManyEdits {
        /// Rejected operation count.
        operations: usize,
    },
    /// Replacement content contains NUL.
    #[error("edit {index} replacement contains NUL at byte {byte}")]
    NulReplacement {
        /// Request-order operation index.
        index: usize,
        /// Byte offset inside replacement content.
        byte: usize,
    },
    /// Write content contains NUL.
    #[error("write content contains NUL at byte {byte}")]
    NulWriteContent {
        /// Byte offset inside write content.
        byte: usize,
    },
    /// File contains NUL.
    #[error("file contains NUL at byte {byte}")]
    NulFile {
        /// Byte offset of the first NUL.
        byte: u64,
    },
    /// File is not valid UTF-8.
    #[error("file is invalid UTF-8 at byte {valid_up_to}")]
    InvalidUtf8 {
        /// First byte not covered by the valid prefix.
        valid_up_to: u64,
    },
    /// File length exceeds the protocol cap.
    #[error("file has {bytes} bytes; maximum is {MAX_FILE_BYTES}")]
    FileTooLarge {
        /// Rejected length.
        bytes: u64,
    },
    /// Snapshot metadata reported zero logical lines.
    #[error("snapshot line count must be at least one")]
    InvalidLineCount,
    /// Position is not the named line or terminal boundary.
    #[error("position {position} is not a valid boundary in the snapshot")]
    InvalidPosition {
        /// Rejected token.
        position: Position,
    },
    /// Start follows end in logical boundary order.
    #[error("range start {start} follows end {end}")]
    ReversedRange {
        /// Inclusive start.
        start: Position,
        /// Exclusive end.
        end: Position,
    },
    /// Two operations conflict.
    #[error("edit {first} overlaps edit {second}")]
    OverlappingEdits {
        /// Earlier request-order index.
        first: usize,
        /// Later request-order index.
        second: usize,
    },
    /// Checked output length arithmetic overflowed.
    #[error("edited output length overflowed")]
    OutputLengthOverflow,
    /// Conflict context exceeded its fixed cap.
    #[error("snapshot conflict has {lines} context lines; maximum is {CONFLICT_CONTEXT_LINES}")]
    TooManyConflictLines {
        /// Rejected context line count.
        lines: usize,
    },
}

impl ContractError {
    /// Map a reference or validation failure to the stable error taxonomy.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidReadLimit { .. }
            | Self::InvalidGrepLimit { .. }
            | Self::InvalidContextLimit { .. }
            | Self::InvalidGlobLimit { .. }
            | Self::ContextOutsideContentMode
            | Self::EmptyEditBatch
            | Self::TooManyEdits { .. }
            | Self::InvalidLineCount
            | Self::TooManyConflictLines { .. } => ErrorCode::InvalidRequest,
            Self::NulReplacement { .. } | Self::NulWriteContent { .. } | Self::NulFile { .. } => {
                ErrorCode::BinaryFile
            }
            Self::InvalidUtf8 { .. } => ErrorCode::InvalidUtf8,
            Self::FileTooLarge { .. } | Self::OutputLengthOverflow => ErrorCode::FileTooLarge,
            Self::InvalidPosition { .. } => ErrorCode::InvalidPosition,
            Self::ReversedRange { .. } => ErrorCode::InvalidRange,
            Self::OverlappingEdits { .. } => ErrorCode::OverlappingEdits,
        }
    }
}

/// Reject a file length above the fixed protocol limit.
///
/// # Errors
///
/// Returns FileTooLarge for a length above MAX_FILE_BYTES.
pub const fn validate_file_size(bytes: u64) -> Result<(), ContractError> {
    if bytes > MAX_FILE_BYTES {
        return Err(ContractError::FileTooLarge { bytes });
    }
    Ok(())
}

/// Validate complete file bytes as strict, NUL-free UTF-8.
///
/// # Errors
///
/// Returns FileTooLarge, NulFile, or InvalidUtf8 for rejected bytes.
pub fn validate_text(bytes: &[u8]) -> Result<&str, ContractError> {
    let length =
        u64::try_from(bytes.len()).map_err(|_| ContractError::FileTooLarge { bytes: u64::MAX })?;
    validate_file_size(length)?;

    if let Some(byte) = bytes.iter().position(|byte| *byte == 0) {
        return Err(ContractError::NulFile {
            byte: u64::try_from(byte)
                .expect("a slice index always fits u64 on supported 64-bit targets"),
        });
    }

    std::str::from_utf8(bytes).map_err(|error| ContractError::InvalidUtf8 {
        valid_up_to: u64::try_from(error.valid_up_to())
            .expect("a slice index always fits u64 on supported 64-bit targets"),
    })
}

/// Materialize the slow reference vector of logical line starts.
#[must_use]
pub fn reference_line_starts(text: &str) -> Vec<u64> {
    let newline_count = text.bytes().filter(|byte| *byte == b'\n').count();
    let mut starts = Vec::with_capacity(newline_count.saturating_add(1));
    starts.push(0);
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(
                u64::try_from(index.saturating_add(1))
                    .expect("a string index always fits u64 on supported 64-bit targets"),
            );
        }
    }
    starts
}

fn boundary_ordinal(text: &str, starts: &[u64], position: Position) -> Result<u64, ContractError> {
    let line_count =
        u64::try_from(starts.len()).expect("line-start count fits u64 on supported targets");
    if position.line <= line_count {
        let index = usize::try_from(position.line - 1)
            .expect("validated line count fits usize on supported targets");
        if starts.get(index) == Some(&position.byte) {
            return Ok(position.line - 1);
        }
    } else if position.line == line_count.saturating_add(1)
        && position.byte
            == u64::try_from(text.len())
                .expect("string length fits u64 on supported 64-bit targets")
    {
        return Ok(line_count);
    }

    Err(ContractError::InvalidPosition { position })
}

/// Verify that a position is an exact logical-line or terminal boundary.
///
/// # Errors
///
/// Returns a text-validation error or InvalidPosition.
pub fn validate_reference_position(bytes: &[u8], position: Position) -> Result<(), ContractError> {
    let text = validate_text(bytes)?;
    let starts = reference_line_starts(text);
    let _ = boundary_ordinal(text, &starts, position)?;
    Ok(())
}

fn validate_edit_count(count: usize) -> Result<(), ContractError> {
    if count == 0 {
        return Err(ContractError::EmptyEditBatch);
    }
    if count > MAX_EDIT_OPERATIONS {
        return Err(ContractError::TooManyEdits { operations: count });
    }
    Ok(())
}

#[derive(Debug)]
struct ValidatedEdit<'a> {
    index: usize,
    start_ordinal: u64,
    end_ordinal: u64,
    start_byte: usize,
    end_byte: usize,
    content: &'a str,
}

impl ValidatedEdit<'_> {
    const fn is_insertion(&self) -> bool {
        self.start_ordinal == self.end_ordinal
    }
}

fn edits_overlap(left: &ValidatedEdit<'_>, right: &ValidatedEdit<'_>) -> bool {
    match (left.is_insertion(), right.is_insertion()) {
        (false, false) => {
            left.start_ordinal < right.end_ordinal && right.start_ordinal < left.end_ordinal
        }
        (true, false) => {
            right.start_ordinal < left.start_ordinal && left.start_ordinal < right.end_ordinal
        }
        (false, true) => {
            left.start_ordinal < right.start_ordinal && right.start_ordinal < left.end_ordinal
        }
        (true, true) => false,
    }
}

/// Apply replace ranges using the slow, obviously-correct byte-vector model.
///
/// This function performs no hashing, file I/O, caching, locking, or
/// persistence. Callers compare the requested snapshot before invoking it and
/// persist its returned bytes atomically.
///
/// # Errors
///
/// Returns a contract error for invalid text, positions, ranges, overlaps, or
/// checked length arithmetic failure.
pub fn apply_reference_edits(
    source: &[u8],
    operations: &[EditOperation],
) -> Result<Vec<u8>, ContractError> {
    validate_edit_count(operations.len())?;
    let text = validate_text(source)?;
    let starts = reference_line_starts(text);
    let mut validated = Vec::with_capacity(operations.len());

    for (index, operation) in operations.iter().enumerate() {
        if let Some(byte) = operation.content().bytes().position(|byte| byte == 0) {
            return Err(ContractError::NulReplacement { index, byte });
        }

        let start = operation.start();
        let end = operation.end();
        let start_ordinal = boundary_ordinal(text, &starts, start)?;
        let end_ordinal = boundary_ordinal(text, &starts, end)?;
        if start_ordinal > end_ordinal {
            return Err(ContractError::ReversedRange { start, end });
        }

        validated.push(ValidatedEdit {
            index,
            start_ordinal,
            end_ordinal,
            start_byte: usize::try_from(start.byte)
                .map_err(|_| ContractError::FileTooLarge { bytes: start.byte })?,
            end_byte: usize::try_from(end.byte)
                .map_err(|_| ContractError::FileTooLarge { bytes: end.byte })?,
            content: operation.content(),
        });
    }

    for left in 0..validated.len() {
        for right in left + 1..validated.len() {
            if edits_overlap(&validated[left], &validated[right]) {
                return Err(ContractError::OverlappingEdits {
                    first: validated[left].index,
                    second: validated[right].index,
                });
            }
        }
    }

    let removed = validated.iter().try_fold(0_usize, |total, operation| {
        total
            .checked_add(operation.end_byte - operation.start_byte)
            .ok_or(ContractError::OutputLengthOverflow)
    })?;
    let added = validated.iter().try_fold(0_usize, |total, operation| {
        total
            .checked_add(operation.content.len())
            .ok_or(ContractError::OutputLengthOverflow)
    })?;
    let final_length = source
        .len()
        .checked_sub(removed)
        .and_then(|length| length.checked_add(added))
        .ok_or(ContractError::OutputLengthOverflow)?;
    validate_file_size(
        u64::try_from(final_length).map_err(|_| ContractError::FileTooLarge { bytes: u64::MAX })?,
    )?;

    validated.sort_by_key(|operation| {
        (
            operation.start_ordinal,
            u8::from(!operation.is_insertion()),
            operation.index,
        )
    });

    let mut output = Vec::with_capacity(final_length);
    let mut cursor = 0;
    for operation in validated {
        if operation.start_byte > cursor {
            output.extend_from_slice(&source[cursor..operation.start_byte]);
            cursor = operation.start_byte;
        }
        output.extend_from_slice(operation.content.as_bytes());
        if !operation.is_insertion() {
            cursor = operation.end_byte;
        }
    }
    output.extend_from_slice(&source[cursor..]);
    Ok(output)
}

/// Build a canonical header from validated reference bytes.
///
/// # Errors
///
/// Returns a text-validation, size, or metadata error.
pub fn reference_header(
    path: String,
    snapshot: SnapshotId,
    bytes: &[u8],
) -> Result<SnapshotHeader, ContractError> {
    let text = validate_text(bytes)?;
    let lines = u64::try_from(reference_line_starts(text).len())
        .expect("line-start count fits u64 on supported targets");
    let byte_count =
        u64::try_from(bytes.len()).map_err(|_| ContractError::FileTooLarge { bytes: u64::MAX })?;
    SnapshotHeader::new(path, snapshot, lines, byte_count)
}

/// Return at most five current lines centered on center_line.
///
/// A center below one is clamped to one; a center beyond EOF is clamped to the
/// last logical line.
///
/// # Errors
///
/// Returns a text-validation error.
pub fn reference_context(
    bytes: &[u8],
    center_line: u64,
) -> Result<Vec<ContextLine>, ContractError> {
    let text = validate_text(bytes)?;
    let starts = reference_line_starts(text);
    let line_count =
        u64::try_from(starts.len()).expect("line-start count fits u64 on supported targets");
    let center = center_line.clamp(1, line_count);
    let mut start_line = center.saturating_sub(2).max(1);
    let end_line = center.saturating_add(2).min(line_count);
    start_line = start_line.max(end_line.saturating_sub(4).max(1));

    let mut context = Vec::with_capacity(CONFLICT_CONTEXT_LINES);
    for line in start_line..=end_line {
        let index = usize::try_from(line - 1)
            .expect("validated line number fits usize on supported targets");
        let start = usize::try_from(starts[index])
            .expect("validated byte offset fits usize on supported targets");
        let raw_end = starts.get(index + 1).map_or(text.len(), |offset| {
            usize::try_from(*offset).expect("validated byte offset fits usize on supported targets")
        });
        let mut content_end = raw_end;
        if content_end > start && text.as_bytes()[content_end - 1] == b'\n' {
            content_end -= 1;
            if content_end > start && text.as_bytes()[content_end - 1] == b'\r' {
                content_end -= 1;
            }
        }
        let content = text
            .get(start..content_end)
            .expect("line starts and ASCII terminators preserve UTF-8 boundaries")
            .to_owned();
        context.push(ContextLine {
            position: Position {
                line,
                byte: starts[index],
            },
            content,
        });
    }
    Ok(context)
}

/// Render one canonical read output line.
#[must_use]
pub fn render_read_line(position: Position, content: &str) -> String {
    let mut output = String::with_capacity(42 + content.len());
    write!(output, "{position}|{content}").expect("writing to String cannot fail");
    output
}

/// Apply a request only when its requested snapshot matches freshly read bytes.
///
/// current_snapshot must be computed from source with the running process seed.
/// The mismatch check precedes all position resolution, so stale requests never
/// relocate or edit a different snapshot.
///
/// # Errors
///
/// Returns a structured snapshot conflict on mismatch, or another stable tool
/// error for invalid request shape, current text, position, range, or batch.
pub fn apply_versioned_reference_edits(
    source: &[u8],
    current_snapshot: SnapshotId,
    request: &EditRequest,
) -> Result<Vec<u8>, ProtocolError> {
    request.validate().map_err(ProtocolError::from)?;
    let current_header = reference_header(request.file_path.clone(), current_snapshot, source)
        .map_err(ProtocolError::from)?;

    if request.snapshot != current_snapshot {
        let center_line = request
            .edits
            .first()
            .map(EditOperation::start)
            .map_or(1, Position::line);
        let context = reference_context(source, center_line).map_err(ProtocolError::from)?;
        return Err(ProtocolError::snapshot_conflict(
            request.snapshot,
            current_header,
            context,
            SNAPSHOT_CONFLICT_MESSAGE.to_owned(),
        )
        .map_err(ProtocolError::from)?);
    }

    apply_reference_edits(source, &request.edits).map_err(ProtocolError::from)
}

/// Validate a read cursor against freshly identified reference bytes.
///
/// Text validation and snapshot comparison precede boundary resolution. A stale
/// cursor therefore conflicts even when its old position is not a boundary in
/// the current bytes.
///
/// # Errors
///
/// Returns a structured snapshot conflict on identity mismatch, or a stable
/// text or position error when the identity matches.
pub fn validate_reference_cursor(
    path: &str,
    source: &[u8],
    current_snapshot: SnapshotId,
    cursor: &PageCursor,
) -> Result<Position, ProtocolError> {
    let current_header =
        reference_header(path.to_owned(), current_snapshot, source).map_err(ProtocolError::from)?;

    if cursor.snapshot != current_snapshot {
        let context = reference_context(source, cursor.next.line()).map_err(ProtocolError::from)?;
        return Err(ProtocolError::snapshot_conflict(
            cursor.snapshot,
            current_header,
            context,
            SNAPSHOT_CONFLICT_MESSAGE.to_owned(),
        )
        .map_err(ProtocolError::from)?);
    }

    validate_reference_position(source, cursor.next).map_err(ProtocolError::from)?;
    Ok(cursor.next)
}

/// Decide one write request against the current destination state.
///
/// current carries freshly read exact destination bytes and their identity,
/// or None when the destination does not exist. On success the returned flag
/// is whether the write creates the destination; the bytes to persist are
/// always exactly the request content.
///
/// # Errors
///
/// Returns already_exists, not_found, or a structured snapshot conflict per
/// R023, or a stable content error for invalid request shape.
pub fn validate_reference_write(
    current: Option<(&[u8], SnapshotId)>,
    request: &WriteRequest,
) -> Result<bool, ProtocolError> {
    request.validate().map_err(ProtocolError::from)?;
    match (request.expect, current) {
        (WriteExpect::Absent, None) => Ok(true),
        (WriteExpect::Absent, Some((bytes, snapshot))) => {
            let header = reference_header(request.file_path.clone(), snapshot, bytes)
                .map_err(ProtocolError::from)?;
            let context = reference_context(bytes, 1).map_err(ProtocolError::from)?;
            Err(
                ProtocolError::already_exists(header, context, ALREADY_EXISTS_MESSAGE.to_owned())
                    .map_err(ProtocolError::from)?,
            )
        }
        (WriteExpect::Snapshot(_), None) => Err(ProtocolError::new(
            ErrorCode::NotFound,
            format!(
                "no file exists at {}; use expect \"absent\" to create it",
                request.file_path
            ),
        )),
        (WriteExpect::Snapshot(requested), Some((bytes, current_snapshot))) => {
            if requested != current_snapshot {
                let header = reference_header(request.file_path.clone(), current_snapshot, bytes)
                    .map_err(ProtocolError::from)?;
                let context = reference_context(bytes, 1).map_err(ProtocolError::from)?;
                return Err(ProtocolError::snapshot_conflict(
                    requested,
                    header,
                    context,
                    SNAPSHOT_CONFLICT_MESSAGE.to_owned(),
                )
                .map_err(ProtocolError::from)?);
            }
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        time::{Duration, UNIX_EPOCH},
    };

    use schemars::schema_for;
    use serde_json::{Value, json};

    use super::*;
    use crate::testutil::{Xorshift32, corpus};

    fn position(line: u64, byte: u64) -> Position {
        Position::new(line, byte).expect("test positions always have a nonzero line")
    }

    fn test_snapshot(seed_byte: u8, bytes: &[u8]) -> SnapshotId {
        let digest = blake3::keyed_hash(&[seed_byte; 32], bytes);
        let mut raw = [0_u8; 16];
        raw.copy_from_slice(&digest.as_bytes()[..16]);
        SnapshotId::from_u128(u128::from_be_bytes(raw))
    }

    fn boundaries(text: &str) -> Vec<Position> {
        let starts = reference_line_starts(text);
        let mut positions = starts
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                position(
                    u64::try_from(index + 1).expect("test line count fits u64"),
                    *byte,
                )
            })
            .collect::<Vec<_>>();
        positions.push(position(
            u64::try_from(starts.len() + 1).expect("test line count fits u64"),
            u64::try_from(text.len()).expect("test text length fits u64"),
        ));
        positions
    }

    fn direct_splice(source: &[u8], start: Position, end: Position, content: &str) -> Vec<u8> {
        let start = usize::try_from(start.byte()).expect("test offset fits usize");
        let end = usize::try_from(end.byte()).expect("test offset fits usize");
        let mut output = Vec::with_capacity(source.len() - (end - start) + content.len());
        output.extend_from_slice(&source[..start]);
        output.extend_from_slice(content.as_bytes());
        output.extend_from_slice(&source[end..]);
        output
    }

    fn request(file_path: &str, snapshot: SnapshotId, edits: Vec<EditOperation>) -> EditRequest {
        EditRequest {
            file_path: file_path.to_owned(),
            snapshot,
            edits,
        }
    }

    #[test]
    fn r001_snapshot_identity_is_canonical_and_seed_scoped() {
        let exact = b"alpha\r\n\xce\xb2eta\n";
        let first = test_snapshot(0x11, exact);
        assert_eq!(test_snapshot(0x11, exact), first);
        assert_ne!(test_snapshot(0x22, exact), first);
        assert_ne!(test_snapshot(0x11, b"alpha\n\xce\xb2eta\n"), first);

        let wire = first.to_string();
        assert_eq!(wire.len(), SNAPSHOT_ID_HEX_LEN);
        assert!(wire.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(wire.parse::<SnapshotId>(), Ok(first));
        for invalid in [
            "",
            "0",
            "0000000000000000000000000000000",
            "000000000000000000000000000000000",
            "7D9C3AF08E1B4F6C9A2D1137F68582A1",
            "0x7d9c3af08e1b4f6c9a2d1137f68582a1",
            " 7d9c3af08e1b4f6c9a2d1137f68582a1",
        ] {
            assert!(invalid.parse::<SnapshotId>().is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn r002_snapshot_header_has_one_unambiguous_grammar() {
        let header = SnapshotHeader::new(
            "src/\"odd]\n.rs".to_owned(),
            SnapshotId::from_u128(1),
            3,
            11,
        )
        .expect("valid test header");
        assert_eq!(
            header.render(),
            r#"[hashline snapshot=00000000000000000000000000000001 lines=3 bytes=11 path="src/\"odd]\n.rs"]"#
        );
        assert_eq!(header.render().lines().count(), 1);
        assert!(SnapshotHeader::new("x".to_owned(), header.snapshot, 0, 0).is_err());
    }

    #[test]
    fn r003_position_parser_is_strict_and_canonical() {
        let maximum = format!("{}@{}", u64::MAX, u64::MAX);
        let parsed = maximum.parse::<Position>().expect("u64 maxima are valid");
        assert_eq!(parsed.line(), u64::MAX);
        assert_eq!(parsed.byte(), u64::MAX);
        assert_eq!(parsed.to_string(), maximum);

        for invalid in [
            "",
            "@",
            "1",
            "0@0",
            "01@0",
            "1@00",
            "+1@0",
            "-1@0",
            "1@+0",
            "1@0@0",
            " 1@0",
            "1@0 ",
            "1@18446744073709551616",
            "18446744073709551616@0",
            "\u{0661}@0",
        ] {
            assert!(invalid.parse::<Position>().is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn r004_byte_offset_is_authoritative_and_line_pair_is_checked() {
        let bytes = "a\né\n".as_bytes();
        for valid in [
            position(1, 0),
            position(2, 2),
            position(3, 5),
            position(4, 5),
        ] {
            validate_reference_position(bytes, valid).expect("valid exact boundary");
        }
        for invalid in [
            position(2, 1),
            position(2, 3),
            position(3, 4),
            position(4, 4),
        ] {
            assert!(matches!(
                validate_reference_position(bytes, invalid),
                Err(ContractError::InvalidPosition { .. })
            ));
        }
    }

    #[test]
    fn r005_ranges_are_half_open_ordered_boundaries() {
        let source = b"a\nb\n";
        let replace_second =
            EditOperation::replace(position(2, 2), position(3, 4), "B\n".to_owned());
        assert_eq!(
            apply_reference_edits(source, &[replace_second]).expect("valid half-open range"),
            b"a\nB\n"
        );

        let replace_all =
            EditOperation::replace(position(1, 0), position(4, 4), "whole".to_owned());
        assert_eq!(
            apply_reference_edits(source, &[replace_all]).expect("whole-file range"),
            b"whole"
        );

        let synthetic_empty =
            EditOperation::replace(position(3, 4), position(3, 4), "x".to_owned());
        let terminal = EditOperation::replace(position(4, 4), position(4, 4), "x".to_owned());
        assert_eq!(
            apply_reference_edits(source, &[synthetic_empty]).expect("synthetic insertion"),
            b"a\nb\nx"
        );
        assert_eq!(
            apply_reference_edits(source, &[terminal]).expect("terminal insertion"),
            b"a\nb\nx"
        );

        let equal_byte_boundaries = [
            EditOperation::replace(position(4, 4), position(4, 4), "T".to_owned()),
            EditOperation::replace(position(3, 4), position(3, 4), "S".to_owned()),
            EditOperation::replace(position(3, 4), position(4, 4), "R".to_owned()),
        ];
        assert_eq!(
            apply_reference_edits(source, &equal_byte_boundaries)
                .expect("boundary order disambiguates an equal byte offset"),
            b"a\nb\nSRT"
        );

        let reversed = EditOperation::replace(position(4, 4), position(3, 4), String::new());
        assert!(matches!(
            apply_reference_edits(source, &[reversed]),
            Err(ContractError::ReversedRange { .. })
        ));
    }

    #[test]
    fn r006_batches_are_atomic_and_same_position_insertions_are_stable() {
        let source = b"one\ntwo\nthree\n";
        let operations = [
            EditOperation::replace(position(2, 4), position(2, 4), "A".to_owned()),
            EditOperation::replace(position(2, 4), position(3, 8), "TWO\n".to_owned()),
            EditOperation::replace(position(2, 4), position(2, 4), "B".to_owned()),
            EditOperation::replace(position(3, 8), position(3, 8), "C".to_owned()),
        ];
        assert_eq!(
            apply_reference_edits(source, &operations).expect("valid atomic batch"),
            b"one\nABTWO\nCthree\n"
        );

        let overlap = [
            EditOperation::replace(position(1, 0), position(4, 14), String::new()),
            EditOperation::replace(position(2, 4), position(2, 4), "inside".to_owned()),
        ];
        assert!(matches!(
            apply_reference_edits(source, &overlap),
            Err(ContractError::OverlappingEdits {
                first: 0,
                second: 1
            })
        ));
        assert_eq!(source, b"one\ntwo\nthree\n");

        let adjacent = [
            EditOperation::replace(position(1, 0), position(2, 4), "ONE\n".to_owned()),
            EditOperation::replace(position(2, 4), position(3, 8), "TWO\n".to_owned()),
        ];
        assert_eq!(
            apply_reference_edits(source, &adjacent).expect("adjacent ranges do not overlap"),
            b"ONE\nTWO\nthree\n"
        );
    }

    #[test]
    fn r007_all_text_paths_reject_invalid_utf8_and_nul() {
        assert_eq!(
            validate_text(b"\xef\xbb\xbfvalid").expect("a UTF-8 BOM is content"),
            "\u{feff}valid"
        );
        assert!(matches!(
            validate_text(b"prefix\0suffix"),
            Err(ContractError::NulFile { byte: 6 })
        ));
        assert!(matches!(
            validate_text(b"prefix\xff"),
            Err(ContractError::InvalidUtf8 { valid_up_to: 6 })
        ));

        let edit = request(
            "x",
            SnapshotId::from_u128(1),
            vec![EditOperation::replace(
                position(1, 0),
                position(1, 0),
                "bad\0replacement".to_owned(),
            )],
        );
        assert!(matches!(
            edit.validate(),
            Err(ContractError::NulReplacement { index: 0, byte: 3 })
        ));
    }

    #[test]
    fn r008_line_endings_outside_ranges_are_byte_exact() {
        let source = b"first\r\nsecond\nthird\r";
        let edit = EditOperation::replace(position(2, 7), position(3, 14), "SECOND\r\n".to_owned());
        assert_eq!(
            apply_reference_edits(source, &[edit]).expect("valid CRLF/LF replacement"),
            b"first\r\nSECOND\r\nthird\r"
        );
        assert_eq!(
            reference_context(source, 2).expect("valid strict text"),
            vec![
                ContextLine {
                    position: position(1, 0),
                    content: "first".to_owned(),
                },
                ContextLine {
                    position: position(2, 7),
                    content: "second".to_owned(),
                },
                ContextLine {
                    position: position(3, 14),
                    content: "third\r".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn r009_empty_file_has_one_line_and_distinct_terminal_boundary() {
        assert_eq!(reference_line_starts(""), vec![0]);
        validate_reference_position(b"", position(1, 0)).expect("empty logical line");
        validate_reference_position(b"", position(2, 0)).expect("terminal boundary");
        assert_eq!(render_read_line(position(1, 0), ""), "1@0|");

        let whole = EditOperation::replace(position(1, 0), position(2, 0), "created\n".to_owned());
        assert_eq!(
            apply_reference_edits(b"", &[whole]).expect("empty-file whole range"),
            b"created\n"
        );
        let header = reference_header("empty".to_owned(), SnapshotId::from_u128(0), b"")
            .expect("empty-file header");
        assert_eq!((header.lines, header.bytes), (1, 0));
    }

    #[test]
    fn r010_file_size_and_u64_offsets_never_truncate() {
        validate_file_size(MAX_FILE_BYTES).expect("the cap is inclusive");
        assert!(matches!(
            validate_file_size(MAX_FILE_BYTES + 1),
            Err(ContractError::FileTooLarge { bytes }) if bytes == MAX_FILE_BYTES + 1
        ));

        let maximum = position(u64::MAX, u64::MAX);
        assert_eq!(
            maximum.to_string(),
            "18446744073709551615@18446744073709551615"
        );
        assert_eq!(
            maximum.to_string().parse::<Position>().expect("round trip"),
            maximum
        );
        assert!(
            SnapshotHeader::new(
                "max".to_owned(),
                SnapshotId::from_u128(0),
                1,
                MAX_FILE_BYTES,
            )
            .is_ok()
        );
    }

    #[test]
    fn r011_conflict_carries_fresh_header_and_bounded_context() {
        let source = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
        let current_snapshot = test_snapshot(7, source);
        let requested_snapshot = SnapshotId::from_u128(9);
        let edit = request(
            "src/data.txt",
            requested_snapshot,
            vec![EditOperation::replace(
                position(6, 15),
                position(7, 18),
                "changed\n".to_owned(),
            )],
        );
        let error = apply_versioned_reference_edits(source, current_snapshot, &edit)
            .expect_err("mismatched snapshot must conflict");

        assert_eq!(error.code, ErrorCode::SnapshotConflict);
        assert!(error.retryable);
        let conflict = error.conflict.expect("conflict payload");
        assert_eq!(conflict.requested_snapshot, requested_snapshot);
        assert_eq!(conflict.current_header.snapshot, current_snapshot);
        assert_eq!(conflict.current_header.path, "src/data.txt");
        assert_eq!(conflict.context.len(), CONFLICT_CONTEXT_LINES);
        assert_eq!(
            conflict
                .context
                .iter()
                .map(|line| line.position.line())
                .collect::<Vec<_>>(),
            vec![4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn r012_cache_eviction_recomputes_and_restart_rotates_identity() {
        let bytes = b"same exact bytes\r\n";
        let initial = test_snapshot(0x41, bytes);
        let recomputed_after_eviction = test_snapshot(0x41, bytes);
        let after_restart = test_snapshot(0x42, bytes);

        assert_eq!(recomputed_after_eviction, initial);
        assert_ne!(after_restart, initial);

        let edit = request(
            "x",
            initial,
            vec![EditOperation::replace(
                position(1, 0),
                position(2, 18),
                "new\n".to_owned(),
            )],
        );
        assert_eq!(
            apply_versioned_reference_edits(bytes, after_restart, &edit)
                .expect_err("pre-restart identity is outside the new process scope")
                .code,
            ErrorCode::SnapshotConflict
        );
    }

    #[test]
    fn r013_page_cursor_binds_next_position_to_snapshot() {
        let source = b"one\ntwo\n";
        let current_snapshot = SnapshotId::from_u128(0x7d9c);
        let cursor = PageCursor {
            snapshot: current_snapshot,
            next: position(2, 4),
        };
        assert_eq!(
            cursor.render_footer(),
            "[hashline next snapshot=00000000000000000000000000007d9c position=2@4]"
        );
        assert_eq!(
            validate_reference_cursor("page.txt", source, current_snapshot, &cursor),
            Ok(cursor.next)
        );
        assert_eq!(
            serde_json::to_value(&cursor).expect("serialize cursor"),
            json!({
                "snapshot": "00000000000000000000000000007d9c",
                "next": "2@4"
            })
        );

        let stale = PageCursor {
            snapshot: SnapshotId::from_u128(1),
            next: position(99, 999),
        };
        let stale_error = validate_reference_cursor("page.txt", source, current_snapshot, &stale)
            .expect_err("snapshot comparison precedes old boundary resolution");
        assert_eq!(stale_error.code, ErrorCode::SnapshotConflict);
        let conflict = stale_error.conflict.expect("stale cursor conflict payload");
        assert_eq!(conflict.requested_snapshot, stale.snapshot);
        assert_eq!(conflict.current_header.snapshot, current_snapshot);

        let invalid = PageCursor {
            snapshot: current_snapshot,
            next: position(2, 0),
        };
        assert_eq!(
            validate_reference_cursor("page.txt", source, current_snapshot, &invalid)
                .expect_err("matching snapshot still requires an exact boundary")
                .code,
            ErrorCode::InvalidPosition
        );

        assert!(
            serde_json::from_value::<PageCursor>(json!({
                "snapshot": "00000000000000000000000000007d9c",
                "next": "2@4",
                "offset": 4
            }))
            .is_err()
        );
    }

    #[test]
    fn r014_read_schema_uses_cursor_and_enforces_the_page_cap() {
        let request: ReadRequest =
            serde_json::from_value(json!({"path": "src/lib.rs"})).expect("defaults apply");
        assert_eq!(request.limit, MAX_PAGE_LINES);
        assert_eq!(request.cursor, None);
        request.validate().expect("default request is valid");

        assert!(
            serde_json::from_value::<ReadRequest>(json!({
                "path": "src/lib.rs",
                "offset": 10
            }))
            .is_err()
        );

        let zero: ReadRequest = serde_json::from_value(json!({
            "path": "src/lib.rs",
            "limit": 0
        }))
        .expect("numeric shape deserializes before semantic validation");
        assert!(matches!(
            zero.validate(),
            Err(ContractError::InvalidReadLimit { limit: 0 })
        ));

        let schema = serde_json::to_value(schema_for!(ReadRequest)).expect("serialize schema");
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"].get("cursor").is_some());
        assert!(schema["properties"].get("offset").is_none());
        assert_eq!(schema["properties"]["limit"]["maximum"], MAX_PAGE_LINES);
    }

    #[test]
    fn r015_grep_lines_share_positions_and_exact_cap_semantics() {
        let snapshot = SnapshotId::from_u128(3);
        let header = SnapshotHeader::new("src/lib.rs".to_owned(), snapshot, 2, 9)
            .expect("valid grep header");
        let match_line = GrepLine {
            position: position(2, 5),
            kind: GrepLineKind::Match,
            content: "beta".to_owned(),
        };
        let context_line = GrepLine {
            position: position(1, 0),
            kind: GrepLineKind::Context,
            content: "alpha".to_owned(),
        };
        assert!(header.render().contains(&snapshot.to_string()));
        assert_eq!(match_line.render(), "2@5:beta");
        assert_eq!(context_line.render(), "1@0-alpha");

        let defaults: GrepRequest =
            serde_json::from_value(json!({"pattern": "beta"})).expect("grep defaults");
        assert_eq!(defaults.max_matches, MAX_GREP_MATCHES);
        assert_eq!(defaults.effective_context(), (0, 0));
        assert_eq!(defaults.output_mode, GrepOutputMode::Content);
        defaults.validate().expect("default grep request");

        for mode in ["files_with_matches", "count"] {
            let alternate: GrepRequest = serde_json::from_value(json!({
                "pattern": "beta",
                "output_mode": mode
            }))
            .expect("alternate output mode");
            alternate.validate().expect("context-free alternate mode");

            let with_context: GrepRequest = serde_json::from_value(json!({
                "pattern": "beta",
                "output_mode": mode,
                "context": 1
            }))
            .expect("shape deserializes before semantic validation");
            assert!(matches!(
                with_context.validate(),
                Err(ContractError::ContextOutsideContentMode)
            ));
        }
        assert!(
            serde_json::from_value::<GrepRequest>(json!({
                "pattern": "beta",
                "output_mode": "paths"
            }))
            .is_err()
        );

        let overridden: GrepRequest = serde_json::from_value(json!({
            "pattern": "beta",
            "before_context": 1,
            "after_context": 2,
            "context": 3
        }))
        .expect("grep context override");
        assert_eq!(overridden.effective_context(), (3, 3));
        overridden.validate().expect("bounded grep context");

        assert_eq!(remaining_match_capacity(200, 199), Ok(1));
        assert_eq!(remaining_match_capacity(200, 200), Ok(0));
        assert_eq!(remaining_match_capacity(200, 201), Ok(0));
        assert!(remaining_match_capacity(201, 0).is_err());

        let summary = GrepSummary {
            matches: 200,
            truncated: true,
            skipped_binary: 2,
            skipped_invalid_utf8: 3,
        };
        assert_eq!(
            summary.render(),
            "[hashline matches=200 truncated=true skipped_binary=2 skipped_invalid_utf8=3]"
        );
    }

    #[test]
    fn r016_grep_invalid_text_policy_has_no_lossy_path() {
        assert!(matches!(
            classify_grep_text(b"ok", GrepTarget::ExplicitFile),
            Ok(GrepText::Search("ok"))
        ));
        assert!(matches!(
            classify_grep_text(b"a\0b", GrepTarget::ExplicitFile),
            Err(ContractError::NulFile { .. })
        ));
        assert!(matches!(
            classify_grep_text(b"a\xffb", GrepTarget::ExplicitFile),
            Err(ContractError::InvalidUtf8 { .. })
        ));
        assert_eq!(
            classify_grep_text(b"a\0b", GrepTarget::TreeEntry),
            Ok(GrepText::SkipBinary)
        );
        assert_eq!(
            classify_grep_text(b"a\xffb", GrepTarget::TreeEntry),
            Ok(GrepText::SkipInvalidUtf8)
        );
    }

    #[test]
    fn r017_error_codes_and_envelope_are_stable() {
        let names = ErrorCode::ALL
            .into_iter()
            .map(|code| serde_json::to_value(code).expect("serialize error code"))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                json!("invalid_request"),
                json!("invalid_snapshot"),
                json!("invalid_position"),
                json!("invalid_range"),
                json!("overlapping_edits"),
                json!("invalid_utf8"),
                json!("binary_file"),
                json!("file_too_large"),
                json!("not_found"),
                json!("not_a_file"),
                json!("permission_denied"),
                json!("root_escape"),
                json!("snapshot_conflict"),
                json!("already_exists"),
                json!("invalid_pattern"),
                json!("io"),
            ]
        );
        assert_eq!(
            ErrorCode::ALL
                .into_iter()
                .filter(|code| code.retryable())
                .collect::<Vec<_>>(),
            vec![
                ErrorCode::NotFound,
                ErrorCode::PermissionDenied,
                ErrorCode::SnapshotConflict,
                ErrorCode::AlreadyExists,
                ErrorCode::Io,
            ]
        );

        let response = ErrorResponse::new(ProtocolError::new(
            ErrorCode::InvalidPattern,
            "unclosed group".to_owned(),
        ));
        assert_eq!(
            serde_json::to_value(response).expect("serialize error envelope"),
            json!({
                "protocol": PROTOCOL,
                "error": {
                    "code": "invalid_pattern",
                    "message": "unclosed group",
                    "retryable": false
                }
            })
        );
    }

    #[test]
    fn r018_tool_errors_do_not_absorb_json_rpc_failures() {
        let response = serde_json::to_value(ErrorResponse::new(ProtocolError::new(
            ErrorCode::InvalidRequest,
            "unknown field".to_owned(),
        )))
        .expect("serialize tool error");
        assert_eq!(response["protocol"], PROTOCOL);
        assert!(response.get("jsonrpc").is_none());
        assert!(response.get("id").is_none());

        for transport_name in [
            "unknown_tool",
            "invalid_json_rpc",
            "transport_loss",
            "cancelled",
            "server_failure",
        ] {
            assert!(ErrorCode::ALL.into_iter().all(|code| {
                serde_json::to_string(&code)
                    .expect("serialize code")
                    .trim_matches('"')
                    != transport_name
            }));
        }
        assert!(
            serde_json::from_value::<ReadRequest>(json!({
                "path": "x",
                "unexpected": true
            }))
            .is_err()
        );
    }

    #[test]
    fn r019_edit_success_names_only_the_persisted_snapshot() {
        let previous = SnapshotId::from_u128(1);
        let persisted = SnapshotId::from_u128(2);
        let success = EditSuccess::new("src/lib.rs".to_owned(), previous, persisted, 2, 17, 3);
        let value = serde_json::to_value(success).expect("serialize success");
        let keys = value
            .as_object()
            .expect("success is an object")
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            keys,
            [
                "applied",
                "bytes",
                "lines",
                "path",
                "previous_snapshot",
                "protocol",
                "snapshot",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );
        assert_eq!(value["previous_snapshot"], previous.to_string());
        assert_eq!(value["snapshot"], persisted.to_string());
        assert_eq!(value["protocol"], PROTOCOL);
    }

    #[test]
    fn r020_read_or_grep_position_applies_only_to_named_bytes() {
        let original = b"same\nline\n";
        let changed = b"same\nLINE\n";
        let original_snapshot = test_snapshot(0x51, original);
        let changed_snapshot = test_snapshot(0x51, changed);
        assert_ne!(original_snapshot, changed_snapshot);
        assert_eq!(
            test_snapshot(0x51, original),
            test_snapshot(0x51, b"same\nline\n")
        );

        let edit = request(
            "target.txt",
            original_snapshot,
            vec![EditOperation::replace(
                position(1, 0),
                position(2, 5),
                "SAME\n".to_owned(),
            )],
        );
        assert_eq!(
            apply_versioned_reference_edits(changed, changed_snapshot, &edit)
                .expect_err("the visible first line cannot authorize changed bytes")
                .code,
            ErrorCode::SnapshotConflict
        );
        assert_eq!(
            apply_versioned_reference_edits(original, original_snapshot, &edit)
                .expect("the exact named snapshot applies"),
            b"SAME\nline\n"
        );
    }

    #[test]
    fn r021_position_validation_never_fuzzy_relocates() {
        let source = b"duplicated\nduplicated\n";
        let current = test_snapshot(0x61, source);
        let invalid_position =
            EditOperation::replace(position(99, 999), position(100, 1_000), "moved".to_owned());

        let stale = request(
            "x",
            SnapshotId::from_u128(0),
            vec![invalid_position.clone()],
        );
        assert_eq!(
            apply_versioned_reference_edits(source, current, &stale)
                .expect_err("staleness precedes position resolution")
                .code,
            ErrorCode::SnapshotConflict
        );

        let current_request = request("x", current, vec![invalid_position]);
        assert_eq!(
            apply_versioned_reference_edits(source, current, &current_request)
                .expect_err("matching bytes do not enable fuzzy relocation")
                .code,
            ErrorCode::InvalidPosition
        );
    }

    #[test]
    fn r022_cli_and_edit_schema_have_no_legacy_compatibility_surface() {
        let snapshot = "00000000000000000000000000000001";
        for legacy in [
            json!({"file_path": "x", "snapshot": snapshot, "anchor": "1:abc", "edits": []}),
            json!({"file_path": "x", "snapshot": snapshot, "end_anchor": "2:def", "edits": []}),
            json!({"file_path": "x", "snapshot": snapshot, "insert_after": "1:abc", "edits": []}),
            json!({"file_path": "x", "snapshot": snapshot, "write": "body", "edits": []}),
            json!({"file_path": "x", "snapshot": snapshot, "edits": "[]"}),
        ] {
            assert!(serde_json::from_value::<EditRequest>(legacy).is_err());
        }
        assert!(
            serde_json::from_value::<EditOperation>(json!({
                "op": "insert_after",
                "start": "1@0",
                "end": "1@0",
                "content": "x"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ReadRequest>(json!({
                "path": "x",
                "offset": 1
            }))
            .is_err()
        );

        let schema = serde_json::to_value(schema_for!(EditRequest)).expect("serialize schema");
        let rendered = serde_json::to_string(&schema).expect("render schema");
        for forbidden in [
            "anchor",
            "end_anchor",
            "insert_after",
            "\"write\"",
            "\"offset\"",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "{forbidden} leaked into the frozen schema"
            );
        }
        assert!(rendered.contains("\"replace\""));
    }

    #[test]
    fn r023_write_expect_grammar_and_reference_decisions() {
        for (input, expected) in [
            ("absent", WriteExpect::Absent),
            (
                "00000000000000000000000000000001",
                WriteExpect::Snapshot(SnapshotId::from_u128(1)),
            ),
        ] {
            assert_eq!(input.parse::<WriteExpect>(), Ok(expected), "{input:?}");
            assert_eq!(expected.to_string(), input);
        }
        for invalid in [
            "",
            "Absent",
            "ABSENT",
            " absent",
            "absent ",
            "0000000000000000000000000000001",
            "000000000000000000000000000000001",
            "7D9C3AF08E1B4F6C9A2D1137F68582A1",
        ] {
            assert!(invalid.parse::<WriteExpect>().is_err(), "{invalid:?}");
        }

        let create: WriteRequest = serde_json::from_value(json!({
            "file_path": "src/new.rs",
            "content": "mod fresh;\n",
            "expect": "absent"
        }))
        .expect("canonical create request");
        create.validate().expect("NUL-free content");
        assert_eq!(
            serde_json::to_value(&create).expect("serialize create")["expect"],
            json!("absent")
        );
        assert!(
            serde_json::from_value::<WriteRequest>(json!({
                "file_path": "src/new.rs",
                "content": "x",
                "expect": "absent",
                "force": true
            }))
            .is_err()
        );
        let nul = WriteRequest {
            file_path: "x".to_owned(),
            content: "bad\0content".to_owned(),
            expect: WriteExpect::Absent,
        };
        assert!(matches!(
            nul.validate(),
            Err(ContractError::NulWriteContent { byte: 3 })
        ));

        assert_eq!(validate_reference_write(None, &create), Ok(true));

        let current = b"mod old;\nmod other;\n";
        let current_snapshot = test_snapshot(0x71, current);
        let error = validate_reference_write(Some((current, current_snapshot)), &create)
            .expect_err("an existing destination rejects an exclusive create");
        assert_eq!(error.code, ErrorCode::AlreadyExists);
        assert!(error.retryable);
        assert!(error.conflict.is_none());
        let existing = error.existing.clone().expect("already-exists payload");
        assert_eq!(existing.current_header.snapshot, current_snapshot);
        assert_eq!(existing.current_header.path, "src/new.rs");
        assert_eq!(
            existing.context.first().map(|line| line.position.line()),
            Some(1)
        );
        let envelope = serde_json::to_value(ErrorResponse::new(error)).expect("serialize envelope");
        assert_eq!(envelope["error"]["code"], "already_exists");
        assert!(envelope["error"].get("conflict").is_none());
        assert!(envelope["error"]["existing"]["current_header"].is_object());

        let overwrite = WriteRequest {
            file_path: "src/new.rs".to_owned(),
            content: "mod newer;\n".to_owned(),
            expect: WriteExpect::Snapshot(current_snapshot),
        };
        assert_eq!(
            serde_json::to_value(&overwrite).expect("serialize overwrite")["expect"],
            json!(current_snapshot.to_string())
        );
        assert_eq!(
            validate_reference_write(Some((current, current_snapshot)), &overwrite),
            Ok(false)
        );
        assert_eq!(
            validate_reference_write(None, &overwrite)
                .expect_err("an overwrite never creates")
                .code,
            ErrorCode::NotFound
        );
        let stale =
            validate_reference_write(Some((current, test_snapshot(0x72, current))), &overwrite)
                .expect_err("a stale overwrite conflicts");
        assert_eq!(stale.code, ErrorCode::SnapshotConflict);
        assert_eq!(
            stale.conflict.expect("conflict payload").requested_snapshot,
            current_snapshot
        );

        let success = WriteSuccess::new("src/new.rs".to_owned(), current_snapshot, 11, 2, true);
        let value = serde_json::to_value(success).expect("serialize write success");
        let keys = value
            .as_object()
            .expect("success is an object")
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            keys,
            ["bytes", "created", "lines", "path", "protocol", "snapshot"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    #[test]
    fn r024_glob_request_order_and_summary_are_deterministic() {
        let defaults: GlobRequest =
            serde_json::from_value(json!({"pattern": "**/*.rs"})).expect("glob defaults");
        assert_eq!(defaults.max_results, MAX_GLOB_RESULTS);
        assert_eq!(defaults.path, None);
        defaults.validate().expect("default glob request");

        for limit in [0_u16, 1_001] {
            let request: GlobRequest = serde_json::from_value(json!({
                "pattern": "*",
                "max_results": limit
            }))
            .expect("numeric shape deserializes before semantic validation");
            assert!(matches!(
                request.validate(),
                Err(ContractError::InvalidGlobLimit { limit: rejected }) if rejected == limit
            ));
        }
        assert!(
            serde_json::from_value::<GlobRequest>(json!({
                "pattern": "*",
                "recursive": true
            }))
            .is_err()
        );

        let newer = UNIX_EPOCH + Duration::from_secs(20);
        let older = UNIX_EPOCH + Duration::from_secs(10);
        let mut entries = vec![
            GlobEntry {
                path: "src/z.rs".to_owned(),
                modified: older,
            },
            GlobEntry {
                path: "src/b.rs".to_owned(),
                modified: newer,
            },
            GlobEntry {
                path: "src/A.rs".to_owned(),
                modified: newer,
            },
            GlobEntry {
                path: "src/a.rs".to_owned(),
                modified: newer,
            },
        ];
        let mut reversed = entries.clone();
        reversed.reverse();
        sort_reference_glob_entries(&mut entries);
        sort_reference_glob_entries(&mut reversed);
        assert_eq!(entries, reversed, "ordering is independent of input order");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/A.rs", "src/a.rs", "src/b.rs", "src/z.rs"]
        );

        assert_eq!(
            GlobSummary {
                files: 4,
                truncated: true
            }
            .render(),
            "[hashline files=4 truncated=true]"
        );
    }

    #[test]
    fn randomized_utf8_crlf_ranges_match_direct_byte_splice() {
        const BODIES: [&str; 7] = [
            "",
            "ascii",
            "βeta",
            "🦀 rust",
            "bare\rcarriage",
            "combining e\u{301}",
            "日本語",
        ];
        const REPLACEMENTS: [&str; 7] = [
            "",
            "replacement",
            "λ\n",
            "new\r\nlines\r\n",
            "bare\rreturn",
            "🧪",
            "終端",
        ];

        let mut rng = Xorshift32::new(0x5eed_cafe);
        for case in 0..512 {
            let line_count =
                usize::try_from(rng.next_range(12) + 1).expect("small generated line count");
            let mut source = String::new();
            for line in 0..line_count {
                let body = BODIES[usize::try_from(rng.next_range(BODIES.len() as u32))
                    .expect("small body index")];
                source.push_str(body);
                let trailing = line + 1 < line_count || rng.next_range(2) == 0;
                if trailing {
                    if rng.next_range(2) == 0 {
                        source.push('\n');
                    } else {
                        source.push_str("\r\n");
                    }
                }
            }

            let positions = boundaries(&source);
            let start_index = usize::try_from(
                rng.next_range(u32::try_from(positions.len()).expect("small boundary count")),
            )
            .expect("small boundary index");
            let remaining = positions.len() - start_index;
            let end_index = start_index
                + usize::try_from(
                    rng.next_range(u32::try_from(remaining).expect("small remaining count")),
                )
                .expect("small range width");
            let start = positions[start_index];
            let end = positions[end_index];
            let content = REPLACEMENTS[usize::try_from(rng.next_range(REPLACEMENTS.len() as u32))
                .expect("small replacement index")];
            let operation = EditOperation::replace(start, end, content.to_owned());

            let expected = direct_splice(source.as_bytes(), start, end, content);
            let actual = apply_reference_edits(source.as_bytes(), &[operation])
                .unwrap_or_else(|error| panic!("case {case} failed: {error}"));
            assert_eq!(actual, expected, "case {case}: {start}..{end}");
            validate_text(&actual)
                .unwrap_or_else(|error| panic!("case {case} produced invalid text: {error}"));
        }
    }

    #[test]
    fn generated_corpus_batches_preserve_untouched_bytes() {
        for seed in 1..=64 {
            let source = corpus(24, seed, seed % 2 == 0);
            let positions = boundaries(&source);
            let first_content = format!("β-{seed}\r\n");
            let first = EditOperation::replace(positions[2], positions[4], first_content.clone());
            let second_start = positions.len() - 3;
            let second_content = format!("tail-{seed}\n");
            let second = EditOperation::replace(
                positions[second_start],
                positions[second_start + 1],
                second_content.clone(),
            );
            let actual = apply_reference_edits(source.as_bytes(), &[second, first])
                .unwrap_or_else(|error| panic!("seed {seed} failed: {error}"));

            let first_start = usize::try_from(positions[2].byte()).expect("test offset fits usize");
            let first_end = usize::try_from(positions[4].byte()).expect("test offset fits usize");
            let second_byte =
                usize::try_from(positions[second_start].byte()).expect("test offset fits usize");
            let second_end = usize::try_from(positions[second_start + 1].byte())
                .expect("test offset fits usize");
            assert!(first_end <= second_byte, "generated ranges remain ordered");

            let mut expected = Vec::new();
            expected.extend_from_slice(&source.as_bytes()[..first_start]);
            expected.extend_from_slice(first_content.as_bytes());
            expected.extend_from_slice(&source.as_bytes()[first_end..second_byte]);
            expected.extend_from_slice(second_content.as_bytes());
            expected.extend_from_slice(&source.as_bytes()[second_end..]);
            assert_eq!(actual, expected, "seed {seed}");
            validate_text(&actual).expect("generated output remains strict UTF-8");
        }
    }

    #[test]
    fn strict_request_schemas_reject_unknown_properties() {
        for schema in [
            serde_json::to_value(schema_for!(ReadRequest)).expect("read schema"),
            serde_json::to_value(schema_for!(GrepRequest)).expect("grep schema"),
            serde_json::to_value(schema_for!(EditRequest)).expect("edit schema"),
            serde_json::to_value(schema_for!(WriteRequest)).expect("write schema"),
            serde_json::to_value(schema_for!(GlobRequest)).expect("glob schema"),
            serde_json::to_value(schema_for!(ErrorResponse)).expect("error schema"),
        ] {
            assert_eq!(schema["additionalProperties"], Value::Bool(false));
        }
    }
}
