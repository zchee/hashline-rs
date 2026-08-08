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
//! Hashline MCP server and its protocol contract.
//!
//! [`protocol`] is the sole, unversioned normative interface for snapshot
//! identities, byte positions and ranges, strict text handling, pagination,
//! conflicts, and structured errors. Its slow reference model is intentionally
//! independent of the optimized snapshot, read, edit, grep, persistence, and
//! cache engines that later gated phases compare against it.
//!
//! For embedding without MCP, each tool module exposes a typed runner —
//! [`read::run`], [`edit::run`], [`write::run`], [`grep::run`], and
//! [`glob::run`] — returning `Result<_, `[`protocol::ProtocolError`]`>` with
//! every failure drawn from the stable R017 taxonomy. The `run_*` variants
//! render the same results as MCP text for [`server::HashlineServer`].
//!
//! The supported consumer surface is exactly: [`protocol`], the five tool
//! modules, [`util::Workspace`] / [`util::ToolOutcome`], and
//! [`server::HashlineServer`]. The remaining modules ([`cache`],
//! [`persist`], [`snapshot`]) are engine internals, public for the crate's
//! benches; their shapes may change without notice.

pub mod cache;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod persist;
pub mod protocol;
pub mod read;
mod render;
pub mod server;
pub mod snapshot;
pub mod util;
pub mod write;

#[cfg(test)]
mod testutil;

pub use server::HashlineServer;
