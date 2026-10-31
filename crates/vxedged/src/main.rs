//! vxedged — the vxedge control-plane daemon.
//!
//! Serves vxAPI (gRPC) on a Unix domain socket. Live: job engine, event bus,
//! audit log, Gateway/Job/Event services (gateway Up/Down validate manifests
//! and fail honestly at provisioning until the backend lands — TASKS.md §2).

mod audit;
mod events;
mod experiment;
mod jobs;
mod services;
mod status;
mod vehicle;

use clap::Parser;
use std::sync::Arc;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::info;

use vxedge_api::v1::compute_service_server::ComputeServiceServer;
use vxedge_api::v1::event_service_server::EventServiceServer;
use vxedge_api::v1::experiment_service_server::ExperimentServiceServer;
use vxedge_api::v1::gateway_service_server::GatewayServiceServer;
use vxedge_api::v1::job_service_server::JobServiceServer;
use vxedge_api::v1::nos_check_service_server::NosCheckServiceServer;
use vxedge_api::v1::svc_service_server::SvcServiceServer;
use vxedge_api::v1::traffic_service_server::TrafficServiceServer;
use vxedge_api::v1::vehicle_service_server::VehicleServiceServer;
use vxedge_api::v1::zone_service_server::ZoneServiceServer;

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

    /// Path to the `vxota` binary the generic service backend spawns for
    /// `ota` services (v1: host-local process, co-located with vxedged —
    /// see ARCHITECTURE.md §8). Default assumes it's on PATH.
    #[arg(long, default_value = "vxota", env = "VXEDGE_VXOTA_BIN")]
    vxota_bin: String,

    /// Path to the `vxtelemetry` binary the service backend spawns for `telemetry` services.
    /// Same co-location caveat as `--vxota-bin`.
    #[arg(long, default_value = "vxtelemetry", env = "VXEDGE_VXTELEMETRY_BIN")]
    vxtelemetry_bin: String,

    /// Path to the `vxprobe` binary `TrafficService` spawns for round-trip
    /// probe profiles (`traffic_profiles.<id>.pps`). Default assumes it's
    /// on PATH.
    #[arg(long, default_value = "vxprobe", env = "VXEDGE_VXPROBE_BIN")]
    vxprobe_bin: String,

    /// Path to the `vxtraffic` binary `TrafficService` spawns for
    /// one-directional load profiles (`traffic_profiles.<id>.bandwidth_mbps`).
    /// Default assumes it's on PATH.
    #[arg(long, default_value = "vxtraffic", env = "VXEDGE_VXTRAFFIC_BIN")]
    vxtraffic_bin: String,

    /// Local disk image `NosCheckService` boots when an experiment's `network.nos_check`
    /// omits `part_ref`/`digest` (the "use the pinned default build" case,
    /// `ARCHITECTURE.md` §6.0.3) — unset means that case falls through to an error, though a
    /// published `part_ref` (fetched via `--cache-dir`) or a local one both still work without
    /// it.
    #[arg(long, env = "VXEDGE_SONIC_VS_IMAGE")]
    sonic_vs_image: Option<std::path::PathBuf>,

    /// Where published part refs get pulled to (`vxedge_core::registry`, `docs/ROADMAP.md` #19),
    /// content-addressed by digest. Defaults to `$XDG_CACHE_HOME/vxedge/parts` or
    /// `$HOME/.cache/vxedge/parts` — deliberately not under `--runtime-dir` (tmpfs by default),
    /// since a pulled artifact should survive a restart rather than re-pulling every time.
    #[arg(long, env = "VXEDGE_CACHE_DIR")]
    cache_dir: Option<std::path::PathBuf>,
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

    // Falls back to `runtime_dir` only when even `$HOME` isn't set (no persistence guarantee
    // in that case, but still functional rather than refusing to start) — see
    // `registry::default_cache_dir`'s own doc comment for why this isn't just `runtime_dir`
    // unconditionally.
    let cache_dir = args
        .cache_dir
        .clone()
        .or_else(vxedge_core::registry::default_cache_dir)
        .unwrap_or_else(|| args.runtime_dir.join("cache/parts"));
    let _ = std::fs::create_dir_all(&cache_dir);

    let bus = events::EventBus::new();
    let engine = jobs::JobEngine::new(bus.clone());
    let audit = Arc::new(audit::Audit::new(&args.runtime_dir));
    // Optional netns confinement of all gateway objects (used by tests/dev).
    // `escalate: true` means "escalate if the children would not already be privileged" —
    // `Shell` skips `sudo` when vxedged is itself uid 0, which is how the campaign scripts run
    // it. Found live 2026-07-17 that `setcap cap_net_admin+ep` on this binary does *not* grant
    // the capability to these child processes (capabilities don't propagate across fork+exec
    // without ambient-set handling this shell-out code doesn't do), so when vxedged is
    // unprivileged it needs passwordless `sudo` for `ip`/`bridge`/`tc` regardless of its own
    // capabilities. TESTBED-SETUP.md documents the sudoers requirement.
    let shell = vxedge_core::gateway::Shell {
        netns: std::env::var("VXEDGE_NETNS").ok().filter(|s| !s.is_empty()),
        escalate: true,
    };

    let listener = tokio::net::UnixListener::bind(path)?;
    info!(socket = %path, runtime_dir = %args.runtime_dir.display(), cache_dir = %cache_dir.display(),
        "vxedged listening (vxAPI, package vxedge.v1)");

    let state: services::SharedState =
        Arc::new(tokio::sync::Mutex::new(services::EdgeState::default()));

    // One instance of each service, shared between its own gRPC endpoint and `ExperimentSvc`,
    // which composes them. Two independent instances would each hold a handle to the same
    // `EdgeState` while looking unrelated — the kind of aliasing that is fine until it isn't.
    // The one Podman API client (the user's rootless socket). Connected on first fiveg use, not
    // here: most verbs never touch containers, so a host without the socket must still get a
    // running daemon, and a socket enabled later must work without a restart.
    let containers = vxedge_core::containers::LazyClient::user();
    let gateway = Arc::new(services::GatewaySvc {
        engine: engine.clone(),
        audit: audit.clone(),
        shell: shell.clone(),
        state: state.clone(),
        runtime_dir: args.runtime_dir.clone(),
        containers: containers.clone(),
    });
    let zones = Arc::new(services::ZoneSvc {
        engine: engine.clone(),
        audit: audit.clone(),
        shell: shell.clone(),
        state: state.clone(),
        runtime_dir: args.runtime_dir.clone(),
        cache_dir: cache_dir.clone(),
    });
    // Not part of `Handles`: `ComputeService` is a standalone RPC surface over the same
    // `EdgeState`/`NodePlan` machinery `zones` already uses, addressed by role rather than
    // wired into `experiment up`'s own zones stage — see `ComputeSvc`'s own doc comment for why.
    let compute = Arc::new(services::ComputeSvc {
        engine: engine.clone(),
        audit: audit.clone(),
        shell: shell.clone(),
        state,
        runtime_dir: args.runtime_dir.clone(),
        cache_dir: cache_dir.clone(),
    });
    let svcs = Arc::new(services::SvcSvc {
        engine: engine.clone(),
        audit: audit.clone(),
        runtime_dir: args.runtime_dir.clone(),
        bins: vxedge_core::services::ServiceBinaries {
            vxota: args.vxota_bin.clone(),
            vxtelemetry: args.vxtelemetry_bin.clone(),
        },
    });
    let nos_check = Arc::new(services::NosCheckSvc {
        engine: engine.clone(),
        audit: audit.clone(),
        runtime_dir: args.runtime_dir.clone(),
        default_image: args.sonic_vs_image.clone(),
        cache_dir: cache_dir.clone(),
    });

    // Shared by `ExperimentSvc` and `VehicleSvc` — the same gateway/zones/services staging,
    // whether the caller is `experiment up` or `vehicle up` (`vehicle.rs`'s own module doc
    // explains why the latter reuses this type rather than a near-duplicate of its own).
    let handles = experiment::Handles {
        engine: engine.clone(),
        gateway: gateway.clone(),
        zones: zones.clone(),
        svcs: svcs.clone(),
    };

    Server::builder()
        .add_service(GatewayServiceServer::from_arc(gateway))
        .add_service(ZoneServiceServer::from_arc(zones))
        .add_service(ComputeServiceServer::from_arc(compute))
        .add_service(SvcServiceServer::from_arc(svcs))
        .add_service(NosCheckServiceServer::from_arc(nos_check))
        .add_service(ExperimentServiceServer::new(experiment::ExperimentSvc {
            audit: audit.clone(),
            handles: handles.clone(),
        }))
        .add_service(VehicleServiceServer::new(vehicle::VehicleSvc {
            audit: audit.clone(),
            handles,
        }))
        .add_service(TrafficServiceServer::new(services::TrafficSvc {
            engine: engine.clone(),
            audit,
            shell,
            runtime_dir: args.runtime_dir.clone(),
            vxprobe_bin: args.vxprobe_bin.clone(),
            vxtraffic_bin: args.vxtraffic_bin.clone(),
            containers,
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
