//! Input/output types for the `hashline_edit` tool.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Input for the `hashline_edit` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HashlineEditInput {
    /// The path of the file to edit (relative to the workspace root or absolute).
    pub file_path: String,

    /// One or more edit operations to apply (validated and applied bottom-up).
    #[serde(deserialize_with = "deserialize_edits")]
    #[schemars(with = "Vec<HashlineOp>")]
    pub edits: Vec<HashlineOp>,
}

/// Accept `edits` as either a native JSON array or a double-encoded JSON string.
///
/// Models sometimes wrap the edits array in quotes, producing
/// `"edits": "[{\"op\":...}]"` instead of `"edits": [{...}]`.
/// This deserializer transparently handles both forms, plus a bare object
/// for a single edit.
fn deserialize_edits<'de, D>(deserializer: D) -> Result<Vec<HashlineOp>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Array(_) => serde_json::from_value(value).map_err(D::Error::custom),
        serde_json::Value::String(ref s) => serde_json::from_str(s).map_err(|e| {
            D::Error::custom(format!(
                "edits was a JSON string but could not be parsed as an array of operations: {e}"
            ))
        }),
        serde_json::Value::Object(_) => {
            serde_json::from_value(serde_json::Value::Array(vec![value])).map_err(D::Error::custom)
        }
        _ => Err(D::Error::custom(
            "edits must be an array of edit operations",
        )),
    }
}

/// A single edit operation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op")]
pub enum HashlineOp {
    /// Replace one line (anchor) or a range (anchor + end_anchor) with new
    /// content. Empty content deletes the line(s).
    #[serde(rename = "replace")]
    Replace {
        /// Anchor string (e.g. `"22:abc:rst"`).
        anchor: String,
        /// Optional end anchor for range replacement (inclusive).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_anchor: Option<String>,
        /// Replacement text. Empty string deletes the matched line(s).
        content: String,
    },

    /// Insert new content after the anchored line.
    /// Use anchor `"0:"` for beginning-of-file or `"EOF"` for end-of-file.
    #[serde(rename = "insert_after")]
    InsertAfter {
        /// Anchor string, `"0:"` (BOF), or `"EOF"`.
        anchor: String,
        /// Content to insert.
        content: String,
    },

    /// Replace entire file content. No anchors needed.
    #[serde(rename = "write")]
    Write {
        /// Complete new file content.
        content: String,
    },
}

/// Output of a successful `hashline_edit` application.
#[derive(Debug, Clone)]
pub struct HashlineEditsApplied {
    /// Number of operations applied.
    pub applied: usize,

    /// Scheme name used for validation/anchors.
    pub scheme: String,

    /// 1-based line number where the snippet starts.
    pub snippet_start_line: usize,

    /// Fresh-anchor snippet of the edited region (±context lines).
    pub snippet: String,

    /// Warnings (e.g. large edits).
    pub warnings: Vec<String>,
}

/// Structured error for hashline edit failures.
#[derive(Debug, Clone)]
pub struct HashlineEditError {
    /// Error category.
    pub error: HashlineEditErrorKind,

    /// Human-readable error message.
    pub message: String,

    /// Context snippet around the failure (with fresh anchors).
    pub context: Option<String>,

    /// 1-based line number where the context snippet starts.
    pub context_start_line: Option<usize>,

    /// If a shifted match was found, the fresh anchor string the model can
    /// retry with (e.g. `"25:abc:rst"`).
    pub shifted_anchor: Option<String>,
}

impl HashlineEditError {
    /// Create an error with only a kind and message.
    pub fn new(error: HashlineEditErrorKind, message: String) -> Self {
        Self {
            error,
            message,
            context: None,
            context_start_line: None,
            shifted_anchor: None,
        }
    }
}

/// Result of applying a batch of hashline edits.
#[derive(Debug, Clone)]
pub enum HashlineEditOutput {
    /// Edits applied successfully.
    EditsApplied(HashlineEditsApplied),

    /// One or more anchors failed validation.
    Error(HashlineEditError),
}

/// Classification of hashline edit errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashlineEditErrorKind {
    /// Anchor no longer validates at the specified line.
    AnchorStale,
    /// Recovery found multiple plausible shifted targets.
    AmbiguousAnchor,
    /// Line number is out of range.
    AnchorNotFound,
    /// File does not exist.
    FileNotFound,
    /// Batch contains overlapping edit ranges.
    OverlappingEdits,
    /// Malformed anchor, invalid op, etc.
    InvalidInput,
    /// File I/O or permission error.
    IoError,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_edits_from_array() {
        let json =
            r#"{"file_path":"f.py","edits":[{"op":"replace","anchor":"1:ab:cd","content":"x"}]}"#;
        let input: HashlineEditInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.edits.len(), 1);
    }

    #[test]
    fn deserialize_edits_from_double_encoded_string() {
        // Model wraps the array in quotes — should still parse.
        let json = r#"{"file_path":"f.py","edits":"[{\"op\":\"replace\",\"anchor\":\"1:ab:cd\",\"content\":\"x\"}]"}"#;
        let input: HashlineEditInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.edits.len(), 1);
        assert!(matches!(input.edits[0], HashlineOp::Replace { .. }));
    }

    #[test]
    fn deserialize_edits_rejects_non_array_string() {
        let json = r#"{"file_path":"f.py","edits":"not json"}"#;
        let err = serde_json::from_str::<HashlineEditInput>(json).unwrap_err();
        assert!(err.to_string().contains("could not be parsed"), "{err}");
    }

    #[test]
    fn deserialize_edits_from_double_encoded_empty_array() {
        let json = r#"{"file_path":"f.py","edits":"[]"}"#;
        let input: HashlineEditInput = serde_json::from_str(json).unwrap();
        assert!(input.edits.is_empty());
    }

    #[test]
    fn deserialize_edits_from_bare_object() {
        let json =
            r#"{"file_path":"f.py","edits":{"op":"replace","anchor":"1:ab:cd","content":"x"}}"#;
        let input: HashlineEditInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.edits.len(), 1);
    }

    #[test]
    fn deserialize_edits_rejects_number() {
        let json = r#"{"file_path":"f.py","edits":42}"#;
        let err = serde_json::from_str::<HashlineEditInput>(json).unwrap_err();
        assert!(err.to_string().contains("must be an array"), "{err}");
    }

    #[test]
    fn deserialize_insert_after_and_write_ops() {
        let json = r#"{"file_path":"f.py","edits":[
            {"op":"insert_after","anchor":"0:","content":"hdr"},
            {"op":"write","content":"whole file"}
        ]}"#;
        let input: HashlineEditInput = serde_json::from_str(json).unwrap();
        assert!(matches!(input.edits[0], HashlineOp::InsertAfter { .. }));
        assert!(matches!(input.edits[1], HashlineOp::Write { .. }));
    }
}
