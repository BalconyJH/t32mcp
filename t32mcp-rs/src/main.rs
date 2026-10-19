// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

mod cli;
mod server;
mod skill_paths;

use anyhow::Result;
use cli::{CliAction, McpTransport, T32Options};
use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use server::T32;
use std::env;

/// Native Remote API calls stay on the thread that owns their channel.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let (mut options, transport) =
        match cli::parse_args(env::args().skip(1)).map_err(anyhow::Error::msg)? {
            CliAction::Help => {
                cli::print_help();
                return Ok(());
            }
            CliAction::Version => {
                cli::print_version();
                return Ok(());
            }
            CliAction::Run(options, transport) => (options, transport),
        };
    if options.skills_base.is_empty() {
        options.skills_base = cli::default_skills_base().map_err(anyhow::Error::msg)?;
    }
    if options.artifacts_base.as_os_str().is_empty() {
        options.artifacts_base = cli::default_artifacts_base();
    }

    // Stdout belongs exclusively to the MCP stdio transport.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let result = serve(options, transport).await;
    server::shutdown().await;
    result
}

async fn serve(options: T32Options, transport: McpTransport) -> Result<()> {
    match transport {
        McpTransport::Stdio => {
            tracing::info!("Starting T32 MCP server (stdio)");

            let service = T32::new(options).serve(stdio()).await.inspect_err(|e| {
                tracing::error!("serving error: {:?}", e);
            })?;

            service.waiting().await?;
        }
        McpTransport::Http { port } => {
            let bind_address = format!("127.0.0.1:{port}");
            let ct = tokio_util::sync::CancellationToken::new();

            let server = T32::new(options);
            let service = StreamableHttpService::new(
                move || Ok(server.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default().with_cancellation_token(ct.clone()),
            );

            let router = axum::Router::new().nest_service("/mcp", service);
            tracing::info!("Starting T32 MCP server (http://{bind_address}/mcp)");

            let tcp_listener = tokio::net::TcpListener::bind(bind_address).await?;
            axum::serve(tcp_listener, router)
                .with_graceful_shutdown(async move {
                    if let Err(error) = tokio::signal::ctrl_c().await {
                        tracing::error!("Cannot listen for shutdown signal: {error}");
                    }
                    ct.cancel();
                })
                .await?;
        }
    }

    Ok(())
}
