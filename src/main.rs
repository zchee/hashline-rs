//! `hashline-mcp` — stdio MCP server entrypoint.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use rmcp::ServiceExt as _;

use hashline::config::{SchemeConfig, SchemeKind};
use hashline::server::HashlineServer;

/// Standalone MCP server providing hashline anchor-based file tools
/// (hashline_read, hashline_edit, hashline_grep) over stdio.
#[derive(Debug, Parser)]
#[command(name = "hashline-mcp", version, about)]
struct Cli {
    /// Workspace root that relative paths resolve against.
    #[arg(long, env = "HASHLINE_ROOT")]
    root: Option<PathBuf>,

    /// Anchor scheme.
    #[arg(long, env = "HASHLINE_SCHEME", value_enum, default_value = "chunk")]
    scheme: SchemeKind,

    /// Anchor hash length in characters.
    #[arg(long, env = "HASHLINE_HASH_LEN", default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=4))]
    hash_len: u8,

    /// Chunk size for the chunk scheme.
    #[arg(long, env = "HASHLINE_CHUNK_SIZE", default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..))]
    chunk_size: u16,

    /// Checkpoint interval for the checkpoint scheme.
    #[arg(long, env = "HASHLINE_CHECKPOINT_INTERVAL", default_value_t = 32, value_parser = clap::value_parser!(u16).range(1..))]
    checkpoint_interval: u16,

    /// Confine all tool paths to the workspace root (reject absolute paths
    /// and symlinks escaping it).
    #[arg(long, env = "HASHLINE_RESTRICT")]
    restrict: bool,
}

/// Fail fast if this CPU lacks the AES extension the line hash was built for.
///
/// `.cargo/config.toml` compiles `+aes` into the x86_64 and aarch64 targets
/// that do not have it by default, which makes the resulting binary use AES
/// instructions unconditionally. On a CPU without them — pre-2010 x86_64
/// without AES-NI, or an aarch64 part lacking the optional crypto extension —
/// that faults with SIGILL somewhere inside the first request. Checking once at
/// startup turns an unexplained mid-session crash into a message that says what
/// to do about it.
///
/// Compiled out entirely on builds that did not enable `+aes`: those use the
/// portable FNV-1a line hash and run anywhere.
fn check_cpu_features() -> anyhow::Result<()> {
    #[cfg(all(target_arch = "x86_64", target_feature = "aes"))]
    anyhow::ensure!(
        std::arch::is_x86_feature_detected!("aes"),
        "this hashline-mcp binary was built with AES-NI (-C target-feature=+aes) \
         but this CPU does not have it. Rebuild without the +aes entry for this \
         target in .cargo/config.toml to get the portable line hash."
    );

    // Feature detection needs OS support; these are the aarch64 targets whose
    // `.cargo/config.toml` entries enable `+aes` and where `std` can check.
    #[cfg(all(
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
         (-C target-feature=+aes) but this CPU does not have it. Rebuild without \
         the +aes entry for this target in .cargo/config.toml to get the \
         portable line hash."
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

    let config = SchemeConfig {
        kind: cli.scheme,
        hash_len: usize::from(cli.hash_len),
        chunk_size: usize::from(cli.chunk_size),
        checkpoint_interval: usize::from(cli.checkpoint_interval),
    };

    let server = HashlineServer::new(root.clone(), config)
        .context("invalid scheme configuration")?
        .with_root_pinned(explicit_root)
        .with_restrict(cli.restrict);
    tracing::info!(
        root = %root.display(),
        root_pinned = explicit_root,
        restrict = cli.restrict,
        scheme = ?config.kind,
        hash_len = config.hash_len,
        "starting hashline MCP server on stdio"
    );

    let service = server
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await
        .context("failed to initialize MCP session")?;
    service.waiting().await.context("MCP session terminated")?;

    Ok(())
}
