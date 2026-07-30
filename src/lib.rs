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
pub mod index;
pub mod read;
mod render;
pub mod scheme;
pub mod server;
pub mod util;

#[cfg(test)]
mod testutil;

pub use config::{SchemeConfig, SchemeKind};
pub use index::FileIndex;
pub use scheme::Scheme;
pub use server::HashlineServer;
