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
//! `hashline-mcp` — stdio MCP server entrypoint.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use hashline::{persist::Durability, server::HashlineServer};
use rmcp::ServiceExt as _;

/// Standalone MCP server providing hashline file reading, editing, and
/// searching tools over stdio.
#[derive(Debug, Parser)]
#[command(
    name = "hashline-mcp",
    version,
    about = "Standalone MCP server for hashline file reading, editing, and searching"
)]
struct Cli {
    /// Workspace root that relative paths resolve against.
    #[arg(long, env = "HASHLINE_ROOT")]
    root: Option<PathBuf>,

    /// Confine all tool paths to the workspace root (reject absolute paths
    /// and symlinks escaping it).
    #[arg(long, env = "HASHLINE_RESTRICT")]
    restrict: bool,

    /// Persistence durability policy: full fsync (default), a write-ordering
    /// barrier, or rename ordering only. See R019 in docs/protocol.md.
    #[arg(long, env = "HASHLINE_DURABILITY", value_enum, default_value_t = DurabilityArg::Full)]
    durability: DurabilityArg,
}

/// CLI surface for [`Durability`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum DurabilityArg {
    /// fsync temp file and parent directory (power-loss durable).
    Full,
    /// Write-ordering barrier (F_BARRIERFSYNC on macOS, fdatasync elsewhere).
    Barrier,
    /// No explicit sync; atomic-rename ordering only.
    None,
}

impl From<DurabilityArg> for Durability {
    fn from(arg: DurabilityArg) -> Self {
        match arg {
            DurabilityArg::Full => Self::Full,
            DurabilityArg::Barrier => Self::Barrier,
            DurabilityArg::None => Self::None,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout carries the MCP transport — logs must go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();

    // A root given via --root or HASHLINE_ROOT is pinned; otherwise the CWD
    // is only a fallback that a client advertising the MCP roots capability
    // may replace after initialization.
    let explicit_root = cli.root.is_some();
    let root = match cli.root {
        Some(root) => root,
        None => std::env::current_dir().context("failed to determine current directory")?,
    };
    let root = root
        .canonicalize()
        .with_context(|| format!("workspace root does not exist: {}", root.display()))?;

    if !explicit_root {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        if root == std::path::Path::new("/") || home.as_deref() == Some(&root) {
            tracing::warn!(
                root = %root.display(),
                "no --root/HASHLINE_ROOT given and the fallback CWD looks wrong; \
                 relative paths may misresolve unless the client provides MCP roots"
            );
        }
    }

    let server = HashlineServer::new(root.clone())
        .with_root_pinned(explicit_root)
        .with_restrict(cli.restrict)
        .with_durability(cli.durability.into());
    tracing::info!(
        root = %root.display(),
        root_pinned = explicit_root,
        restrict = cli.restrict,
        "starting hashline MCP server on stdio"
    );

    let service = server
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await
        .context("failed to initialize MCP session")?;
    service.waiting().await.context("MCP session terminated")?;

    Ok(())
}
