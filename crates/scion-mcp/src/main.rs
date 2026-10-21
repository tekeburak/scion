//! `scion-mcp`: serve Scion's MCP tools on stdio (the default, for an agent
//! that starts it) or as streamable HTTP at `/mcp` (with `--http <addr>`).
//! It calls the server's HTTP/JSON API at `--api` with the token in
//! `SCN_TOKEN`; without a token only public repositories read.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use rmcp::ServiceExt;
use scion_mcp::Scion;

#[derive(Parser)]
#[command(name = "scion-mcp", version = scion_types::version(), about = "Scion MCP server for agents")]
struct Cli {
    /// The server's API, e.g. `http://127.0.0.1:8080/api/v1` (`scion-server --web`).
    #[arg(long, env = "SCION_API")]
    api: String,
    /// Serve streamable HTTP at `http://<addr>/mcp` instead of stdio, e.g.
    /// `127.0.0.1:7421`.
    #[arg(long)]
    http: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let token = std::env::var("SCN_TOKEN").ok().filter(|t| !t.is_empty());
    let scion = Scion::new(&cli.api, token);
    let served = match cli.http {
        None => stdio(scion).await,
        Some(addr) => http(scion, &addr).await,
    };
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("scion-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn stdio(scion: Scion) -> Result<(), Box<dyn std::error::Error>> {
    scion
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

async fn http(scion: Scion, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("scion-mcp: serving http://{}/mcp", listener.local_addr()?);
    scion.serve_http(listener).await?;
    Ok(())
}
