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
use hashline::{config::SchemeConfig, server::HashlineServer};
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
}

/// Fail fast if this CPU lacks the AES extension the line hash was built for.
///
/// `.cargo/config.toml` compiles `+aes` into the x86_64 and aarch64 targets
/// that do not have it by default, which makes a `gxhash` line hash use AES
/// instructions unconditionally. On a CPU without them — pre-2010 x86_64
/// without AES-NI, or an aarch64 part lacking the optional crypto extension —
/// that faults with SIGILL somewhere inside the first request. Checking once at
/// startup turns an unexplained mid-session crash into a message that says what
/// to do about it.
///
/// Compiled out entirely on any build that does not hash with `gxhash` —
/// `--no-default-features`, or a target without `+aes`. Those use the portable
/// FNV-1a line hash and run anywhere.
fn check_cpu_features() -> anyhow::Result<()> {
    #[cfg(all(feature = "gxhash", target_arch = "x86_64", target_feature = "aes"))]
    anyhow::ensure!(
        std::arch::is_x86_feature_detected!("aes"),
        "this hashline-mcp binary was built with AES-NI (-C target-feature=+aes) \
         but this CPU does not have it. Rebuild with --no-default-features to \
         get the portable line hash."
    );

    // Feature detection needs OS support; these are the aarch64 targets whose
    // `.cargo/config.toml` entries enable `+aes` and where `std` can check.
    #[cfg(all(
        feature = "gxhash",
        target_arch = "aarch64",
        target_feature = "aes",
        any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "windows"
        )
    ))]
    anyhow::ensure!(
        std::arch::is_aarch64_feature_detected!("aes"),
        "this hashline-mcp binary was built with the AES extension \
         (-C target-feature=+aes) but this CPU does not have it. Rebuild with \
         --no-default-features to get the portable line hash."
    );

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    check_cpu_features()?;

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

    let server = HashlineServer::new(root.clone(), SchemeConfig::default())
        .context("invalid internal scheme configuration")?
        .with_root_pinned(explicit_root)
        .with_restrict(cli.restrict);
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
