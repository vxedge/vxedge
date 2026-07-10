//! vxedged — the vxedge control-plane daemon.
//!
//! Serves vxAPI (gRPC) on a Unix domain socket. Live: job engine, event bus,
//! audit log, Gateway/Job/Event services (gateway Up/Down validate manifests
//! and fail honestly at provisioning until the backend lands — TASKS.md §2).

mod audit;
mod events;
mod jobs;
mod services;

use clap::Parser;
use std::sync::Arc;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::info;

use vxedge_api::v1::event_service_server::EventServiceServer;
use vxedge_api::v1::gateway_service_server::GatewayServiceServer;
use vxedge_api::v1::job_service_server::JobServiceServer;

#[derive(Parser, Debug)]
#[command(name = "vxedged", about = "vxedge control-plane daemon (vxAPI)")]
struct Args {
    /// Listen address: unix:///path/to.sock (TCP+mTLS arrives with the auth work).
    #[arg(long, default_value = "unix:///run/vxedge.sock", env = "VXEDGE_LISTEN")]
    listen: String,

    /// Runtime directory (ephemeral state: audit log, future staging dirs).
    #[arg(long, default_value = "/run/vxedge", env = "VXEDGE_RUNTIME_DIR")]
    runtime_dir: std::path::PathBuf,

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
            "unsupported listen address `{}` — only unix:// sockets are supported for now",
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
    let _ = std::fs::create_dir_all(&args.runtime_dir);

    let bus = events::EventBus::new();
    let engine = jobs::JobEngine::new(bus.clone());
    let audit = Arc::new(audit::Audit::new(&args.runtime_dir));
    // Optional netns confinement of all gateway objects (used by tests/dev).
    let shell = vxedge_core::gateway::Shell {
        netns: std::env::var("VXEDGE_NETNS").ok().filter(|s| !s.is_empty()),
    };

    let listener = tokio::net::UnixListener::bind(path)?;
    info!(socket = %path, runtime_dir = %args.runtime_dir.display(),
        "vxedged listening (vxAPI, package vxedge.v1)");

    Server::builder()
        .add_service(GatewayServiceServer::new(services::GatewaySvc {
            engine: engine.clone(),
            audit,
            shell,
            state: Arc::new(tokio::sync::Mutex::new(None)),
        }))
        .add_service(JobServiceServer::new(services::JobSvc { engine }))
        .add_service(EventServiceServer::new(services::EventSvc { bus }))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutdown signal received");
        })
        .await?;

    // Best-effort cleanup of the socket file on graceful shutdown.
    let _ = std::fs::remove_file(path);
    Ok(())
}
