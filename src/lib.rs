//! Hashline — anchor-based file reading, editing, and searching over MCP.
//!
//! A standalone Rust implementation of the `grok_build_hashline` toolset from
//! [xai-org/grok-build], exposed as a Model Context Protocol server.
//!
//! Every line of a file gets a compact anchor (`LINE:HASH` or
//! `LINE:HASH:HASH`) derived from whitespace-normalized content hashes.
//! Models reference lines by anchor instead of raw line numbers, so edits are
//! validated against the snapshot the model actually saw — stale or shifted
//! anchors are rejected with recovery hints instead of silently corrupting
//! the file.
//!
//! [xai-org/grok-build]: https://github.com/xai-org/grok-build

pub mod config;
pub mod edit;
pub mod grep;
pub mod hash;
pub mod read;
pub mod scheme;
pub mod server;
pub mod util;

pub use config::{SchemeConfig, SchemeKind};
pub use server::HashlineServer;
