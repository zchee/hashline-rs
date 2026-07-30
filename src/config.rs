//! Hashline scheme configuration — shared across all hashline tools.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::scheme::Scheme;

/// Selectable anchor scheme kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[value(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum SchemeKind {
    /// Local line hash + fixed-size chunk fingerprint (default).
    Chunk,
    /// Content-only line hash — weakest freshness, least anchor churn.
    ContentOnly,
    /// Local line hash + checkpoint-chained fingerprint — strongest freshness.
    Checkpoint,
}

/// Errors produced by [`SchemeConfig::validate`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// `hash_len` outside `1..=4`.
    #[error("hash_len must be 1..=4, got {0}")]
    HashLen(usize),
    /// `chunk_size` must be positive for the chunk scheme.
    #[error("chunk_size must be > 0")]
    ChunkSize,
    /// `checkpoint_interval` must be positive for the checkpoint scheme.
    #[error("checkpoint_interval must be > 0")]
    CheckpointInterval,
}

/// Example anchor strings for tool descriptions.
#[derive(Debug, Clone)]
pub struct ExampleAnchors {
    /// A bare anchor (e.g. `"22:abc:rst"`).
    pub anchor: String,
    /// First example read line (`ANCHOR→CONTENT`).
    pub read_line1: String,
    /// Second example read line.
    pub read_line2: String,
    /// Example grep match line (`ANCHOR:CONTENT`).
    pub grep_match: String,
    /// Example grep context line (`ANCHOR-CONTENT`).
    pub grep_context: String,
}

/// Configurable parameters for the hashline anchor scheme.
///
/// One instance is shared by all three hashline tools so anchors produced by
/// `hashline_read`/`hashline_grep` always validate under `hashline_edit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemeConfig {
    /// Active scheme.
    pub kind: SchemeKind,
    /// Anchor hash length in characters (1–4).
    pub hash_len: usize,
    /// Chunk size for the chunk scheme.
    pub chunk_size: usize,
    /// Checkpoint interval for the checkpoint scheme.
    pub checkpoint_interval: usize,
}

impl Default for SchemeConfig {
    fn default() -> Self {
        Self {
            kind: SchemeKind::Chunk,
            hash_len: 3,
            chunk_size: 8,
            checkpoint_interval: 32,
        }
    }
}

impl SchemeConfig {
    /// Validate the parameters.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.hash_len == 0 || self.hash_len > 4 {
            return Err(ConfigError::HashLen(self.hash_len));
        }
        if self.kind == SchemeKind::Chunk && self.chunk_size == 0 {
            return Err(ConfigError::ChunkSize);
        }
        if self.kind == SchemeKind::Checkpoint && self.checkpoint_interval == 0 {
            return Err(ConfigError::CheckpointInterval);
        }
        Ok(())
    }

    /// Validate and build the anchor scheme.
    pub fn build_scheme(&self) -> Result<Scheme, ConfigError> {
        self.validate()?;
        Ok(match self.kind {
            SchemeKind::ContentOnly => Scheme::content_only(self.hash_len),
            SchemeKind::Chunk => Scheme::chunk(self.hash_len, self.chunk_size),
            SchemeKind::Checkpoint => Scheme::checkpoint(self.hash_len, self.checkpoint_interval),
        })
    }

    /// Generate example anchor strings for use in tool descriptions.
    pub fn example_anchors(&self) -> ExampleAnchors {
        let len = self.hash_len.clamp(1, 4);
        let hash = &"abcd"[..len];
        let ctx = &"rstu"[..len];
        match self.kind {
            SchemeKind::ContentOnly => ExampleAnchors {
                anchor: format!("22:{hash}"),
                read_line1: format!("1:{hash}→fn main() {{"),
                read_line2: format!("2:{hash}→    let x = 1;"),
                grep_match: format!("2:{hash}:    let x = 1;"),
                grep_context: format!("3:{hash}-    let y = 2;"),
            },
            SchemeKind::Chunk | SchemeKind::Checkpoint => ExampleAnchors {
                anchor: format!("22:{hash}:{ctx}"),
                read_line1: format!("1:{hash}:{ctx}→fn main() {{"),
                read_line2: format!("2:{hash}:{ctx}→    let x = 1;"),
                grep_match: format!("2:{hash}:{ctx}:    let x = 1;"),
                grep_context: format!("3:{hash}:{ctx}-    let y = 2;"),
            },
        }
    }

    /// Replace description placeholders with scheme-appropriate examples.
    pub fn render_description(&self, template: &str) -> String {
        let ex = self.example_anchors();
        template
            .replace("{example_anchor}", &ex.anchor)
            .replace("{example_line1}", &ex.read_line1)
            .replace("{example_line2}", &ex.read_line2)
            .replace("{grep_match}", &ex.grep_match)
            .replace("{grep_context}", &ex.grep_context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_builds_chunk_scheme() {
        let scheme = SchemeConfig::default().build_scheme().unwrap();
        assert_eq!(scheme.name(), "chunk_v1");
        assert_eq!(scheme.hash_len(), 3);
    }

    #[test]
    fn content_only_builds() {
        let config = SchemeConfig {
            kind: SchemeKind::ContentOnly,
            hash_len: 2,
            ..Default::default()
        };
        let scheme = config.build_scheme().unwrap();
        assert_eq!(scheme.name(), "content_only_v1");
        assert_eq!(scheme.hash_len(), 2);
    }

    #[test]
    fn checkpoint_builds() {
        let config = SchemeConfig {
            kind: SchemeKind::Checkpoint,
            ..Default::default()
        };
        let scheme = config.build_scheme().unwrap();
        assert_eq!(scheme.name(), "checkpoint_v1");
    }

    #[test]
    fn hash_len_bounds_rejected() {
        for hash_len in [0, 5] {
            let config = SchemeConfig {
                hash_len,
                ..Default::default()
            };
            assert_eq!(config.validate(), Err(ConfigError::HashLen(hash_len)));
        }
    }

    #[test]
    fn chunk_size_zero_rejected() {
        let config = SchemeConfig {
            chunk_size: 0,
            ..Default::default()
        };
        assert_eq!(config.validate(), Err(ConfigError::ChunkSize));
    }

    #[test]
    fn content_only_ignores_chunk_size() {
        let config = SchemeConfig {
            kind: SchemeKind::ContentOnly,
            chunk_size: 0,
            ..Default::default()
        };
        assert!(config.build_scheme().is_ok());
    }

    #[test]
    fn checkpoint_interval_zero_rejected() {
        let config = SchemeConfig {
            kind: SchemeKind::Checkpoint,
            checkpoint_interval: 0,
            ..Default::default()
        };
        assert_eq!(config.validate(), Err(ConfigError::CheckpointInterval));
    }

    #[test]
    fn render_description_chunk_3() {
        let rendered = SchemeConfig::default().render_description("anchor={example_anchor}");
        assert_eq!(rendered, "anchor=22:abc:rst");
    }

    #[test]
    fn render_description_content_only_2() {
        let config = SchemeConfig {
            kind: SchemeKind::ContentOnly,
            hash_len: 2,
            ..Default::default()
        };
        let rendered = config.render_description("anchor={example_anchor}");
        assert_eq!(rendered, "anchor=22:ab");
    }

    #[test]
    fn render_grep_examples_chunk() {
        let rendered = SchemeConfig::default().render_description("{grep_match} / {grep_context}");
        assert!(rendered.contains("2:abc:rst:"), "match: {rendered}");
        assert!(rendered.contains("3:abc:rst-"), "context: {rendered}");
    }

    #[test]
    fn scheme_kind_parses_snake_case_values() {
        use clap::ValueEnum as _;
        assert_eq!(
            SchemeKind::from_str("content_only", false).unwrap(),
            SchemeKind::ContentOnly
        );
        assert_eq!(
            SchemeKind::from_str("chunk", false).unwrap(),
            SchemeKind::Chunk
        );
        assert_eq!(
            SchemeKind::from_str("checkpoint", false).unwrap(),
            SchemeKind::Checkpoint
        );
    }
}
