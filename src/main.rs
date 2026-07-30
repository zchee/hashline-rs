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

    let root = match cli.root {
        Some(root) => root,
        None => std::env::current_dir().context("failed to determine current directory")?,
    };
    let root = root
        .canonicalize()
        .with_context(|| format!("workspace root does not exist: {}", root.display()))?;

    let config = SchemeConfig {
        kind: cli.scheme,
        hash_len: usize::from(cli.hash_len),
        chunk_size: usize::from(cli.chunk_size),
        checkpoint_interval: usize::from(cli.checkpoint_interval),
    };

    let server =
        HashlineServer::new(root.clone(), config).context("invalid scheme configuration")?;
    tracing::info!(
        root = %root.display(),
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
