//! vxedged — the vxedge control-plane daemon.
//!
//! Serves vxAPI (gRPC) on a Unix domain socket by default. Scaffold status:
//! GatewayService.Status responds with a placeholder; every other RPC returns
//! UNIMPLEMENTED until the job engine and backends land (TASKS.md §1–2).

mod services;

use clap::Parser;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::info;

use vxedge_api::v1::gateway_service_server::GatewayServiceServer;
use vxedge_api::v1::job_service_server::JobServiceServer;

#[derive(Parser, Debug)]
#[command(name = "vxedged", about = "vxedge control-plane daemon (vxAPI)")]
struct Args {
    /// Listen address: unix:///path/to.sock (TCP+mTLS arrives with the auth work).
    #[arg(long, default_value = "unix:///run/vxedge.sock", env = "VXEDGE_LISTEN")]
    listen: String,

    /// Federate site daemons instead of provisioning locally (phase 3).
    #[arg(long)]
    controller: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    if args.controller {
        return Err("--controller mode is not implemented yet (phase 3)".into());
    }

    let Some(path) = args.listen.strip_prefix("unix://") else {
        return Err(format!(
            "unsupported listen address `{}` — only unix:// sockets are supported in this scaffold",
            args.listen
        )
        .into());
    };

    // Stale socket from a previous run: remove it (single-daemon assumption).
    if std::path::Path::new(path).exists() {
        std::fs::remove_file(path)?;
    }
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = tokio::net::UnixListener::bind(path)?;
    info!(socket = %path, "vxedged listening (vxAPI, package vxedge.v1)");

    Server::builder()
        .add_service(GatewayServiceServer::new(services::GatewayStub))
        .add_service(JobServiceServer::new(services::JobStub))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutdown signal received");
        })
        .await?;

    // Best-effort cleanup of the socket file on graceful shutdown.
    let _ = std::fs::remove_file(path);
    Ok(())
}
