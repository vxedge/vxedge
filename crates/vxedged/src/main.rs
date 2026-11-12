//! vxedged — the vxedge control-plane daemon.
//!
//! Serves vxAPI (gRPC) on a Unix domain socket. Live: job engine, event bus,
//! audit log, Gateway/Job/Event services (gateway Up/Down validate manifests,
//! then provision through `vxedge-core`'s gateway backend).

mod audit;
mod campaign;
mod campaign_run;
mod errors;
mod events;
mod experiment;
mod exporter;
mod jobs;
mod measure;
mod package;
mod plan;
mod records;
mod results;
mod runs;
mod services;
mod status;
mod telemetry;
mod tools;
mod vehicle;
mod verdict;

use clap::Parser;
use std::sync::Arc;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::info;

use vxedge_api::v1::campaign_service_server::CampaignServiceServer;
use vxedge_api::v1::compute_service_server::ComputeServiceServer;
use vxedge_api::v1::event_service_server::EventServiceServer;
use vxedge_api::v1::experiment_service_server::ExperimentServiceServer;
use vxedge_api::v1::gateway_service_server::GatewayServiceServer;
use vxedge_api::v1::host_service_server::HostServiceServer;
use vxedge_api::v1::job_service_server::JobServiceServer;
use vxedge_api::v1::log_service_server::LogServiceServer;
use vxedge_api::v1::measurement_service_server::MeasurementServiceServer;
use vxedge_api::v1::nos_check_service_server::NosCheckServiceServer;
use vxedge_api::v1::package_service_server::PackageServiceServer;
use vxedge_api::v1::result_service_server::ResultServiceServer;
use vxedge_api::v1::run_service_server::RunServiceServer;
use vxedge_api::v1::svc_service_server::SvcServiceServer;
use vxedge_api::v1::telemetry_service_server::TelemetryServiceServer;
use vxedge_api::v1::traffic_service_server::TrafficServiceServer;
use vxedge_api::v1::vehicle_service_server::VehicleServiceServer;
use vxedge_api::v1::workspace_service_server::WorkspaceServiceServer;
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

    /// Path to the `vxota-probe` binary an `ota` service's `fetch` clients run. Same
    /// co-location caveat as `--vxota-bin`.
    #[arg(long, default_value = "vxota-probe", env = "VXEDGE_VXOTA_PROBE_BIN")]
    vxota_probe_bin: String,

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

    /// The host trust policy (`ARCHITECTURE.md` §6.0.8): whether packages must be signed and by
    /// whom. Default: `/etc/vxedge/trust.yaml` when it exists; without one, any package runs.
    #[arg(long, env = "VXEDGE_TRUST_POLICY")]
    trust_policy: Option<std::path::PathBuf>,

    /// Where the daemon's own exporter serves `/metrics` (`ARCHITECTURE.md` §12): per-node CPU
    /// and memory and per-slice queue counters, by entity. Loopback by default; port 0 picks a
    /// free one; `off` serves nothing. An address in use is logged and the daemon starts anyway.
    #[arg(long, default_value = "127.0.0.1:9469", env = "VXEDGE_METRICS_LISTEN")]
    metrics_listen: String,
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
    package::set_cache_dir(&cache_dir);
    let policy_path = args.trust_policy.clone().or_else(|| {
        let default = std::path::PathBuf::from("/etc/vxedge/trust.yaml");
        default.is_file().then_some(default)
    });
    let policy = match &policy_path {
        Some(p) => vxedge_core::trust::TrustPolicy::load(p)?,
        None => vxedge_core::trust::TrustPolicy::default(),
    };
    info!(policy = ?policy_path, require_signed = policy.require_signed, "trust policy");
    plan::set_host_trust(policy, cache_dir.clone());

    let bus = events::EventBus::with_runtime_dir(args.runtime_dir.clone());
    let engine = jobs::JobEngine::with_runtime_dir(bus.clone(), args.runtime_dir.clone());
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
    if let Some(metrics) = exporter::bind(&args.metrics_listen).await {
        info!(metrics = %exporter::targets().join(","), "metrics exporter listening");
        tokio::spawn(exporter::serve(
            metrics,
            state.clone(),
            shell.clone(),
            args.runtime_dir.clone(),
        ));
    }

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
        state: state.clone(),
        runtime_dir: args.runtime_dir.clone(),
        cache_dir: cache_dir.clone(),
    });
    let svcs = Arc::new(services::SvcSvc {
        engine: engine.clone(),
        audit: audit.clone(),
        shell: shell.clone(),
        state,
        runtime_dir: args.runtime_dir.clone(),
        bins: vxedge_core::services::ServiceBinaries {
            vxota: args.vxota_bin.clone(),
            vxota_probe: args.vxota_probe_bin.clone(),
            vxtelemetry: args.vxtelemetry_bin.clone(),
        },
        cache_dir: cache_dir.clone(),
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
        compute: compute.clone(),
        svcs: svcs.clone(),
    };

    let plan_gateway = gateway.clone();
    let telemetry_engine = engine.clone();
    let telemetry_audit = audit.clone();
    let experiments = Arc::new(experiment::ExperimentSvc {
        audit: audit.clone(),
        handles: handles.clone(),
    });
    let vehicles = Arc::new(vehicle::VehicleSvc {
        audit: audit.clone(),
        handles,
    });
    let runs = runs::RunSvc {
        gateway: gateway.clone(),
        experiments: experiments.clone(),
        vehicles: vehicles.clone(),
        runtime_dir: args.runtime_dir.clone(),
    };
    // Shared with `CampaignService`, whose trials call them as stages.
    let measure = Arc::new(measure::MeasurementSvc {
        engine: engine.clone(),
        audit: audit.clone(),
        shell: shell.clone(),
        runtime_dir: args.runtime_dir.clone(),
        vxprobe_bin: args.vxprobe_bin.clone(),
        containers: containers.clone(),
        cache_dir: cache_dir.clone(),
    });
    let traffic = Arc::new(services::TrafficSvc {
        engine: engine.clone(),
        audit: audit.clone(),
        shell,
        runtime_dir: args.runtime_dir.clone(),
        vxprobe_bin: args.vxprobe_bin.clone(),
        vxtraffic_bin: args.vxtraffic_bin.clone(),
        containers: containers.clone(),
    });
    // Every tool flag, read at the first collect and again only when a file changed.
    let tools = Arc::new(tools::Tools::new(
        [
            ("vxprobe", &args.vxprobe_bin),
            ("vxtraffic", &args.vxtraffic_bin),
            ("vxota", &args.vxota_bin),
            ("vxota-probe", &args.vxota_probe_bin),
            ("vxtelemetry", &args.vxtelemetry_bin),
        ]
        .into_iter()
        .map(|(name, bin)| (name.to_string(), bin.clone()))
        .collect(),
    ));
    let telemetry = Arc::new(telemetry::TelemetrySvc {
        engine: telemetry_engine,
        audit: telemetry_audit,
        runtime_dir: args.runtime_dir.clone(),
        vxtelemetry_bin: args.vxtelemetry_bin.clone(),
        tools,
    });
    Server::builder()
        .add_service(GatewayServiceServer::from_arc(gateway))
        .add_service(RunServiceServer::new(runs))
        .add_service(WorkspaceServiceServer::new(plan::PlanSvc {
            gateway: plan_gateway.clone(),
            runtime_dir: args.runtime_dir.clone(),
            cache_dir: cache_dir.clone(),
            engine: engine.clone(),
        }))
        .add_service(HostServiceServer::new(plan::PlanSvc {
            gateway: plan_gateway,
            runtime_dir: args.runtime_dir.clone(),
            cache_dir: cache_dir.clone(),
            engine: engine.clone(),
        }))
        .add_service(ZoneServiceServer::from_arc(zones))
        .add_service(ComputeServiceServer::from_arc(compute))
        .add_service(SvcServiceServer::from_arc(svcs.clone()))
        .add_service(NosCheckServiceServer::from_arc(nos_check))
        .add_service(ExperimentServiceServer::from_arc(experiments.clone()))
        .add_service(VehicleServiceServer::from_arc(vehicles))
        .add_service(CampaignServiceServer::new(campaign::CampaignSvc {
            steps: campaign_run::Steps {
                engine: engine.clone(),
                experiments: experiments.clone(),
                traffic: traffic.clone(),
                measure: measure.clone(),
                telemetry: telemetry.clone(),
                svcs: svcs.clone(),
                containers: containers.clone(),
            },
            audit: audit.clone(),
            runtime_dir: args.runtime_dir.clone(),
        }))
        .add_service(ResultServiceServer::new(results::ResultSvc {
            runtime_dir: args.runtime_dir.clone(),
            seen: Default::default(),
        }))
        .add_service(PackageServiceServer::new(package::PackageSvc {
            engine: engine.clone(),
            audit: audit.clone(),
            cache_dir: cache_dir.clone(),
            runtime_dir: args.runtime_dir.clone(),
        }))
        .add_service(MeasurementServiceServer::from_arc(measure))
        .add_service(TrafficServiceServer::from_arc(traffic))
        .add_service(JobServiceServer::new(services::JobSvc { engine }))
        .add_service(EventServiceServer::new(services::EventSvc { bus }))
        .add_service(TelemetryServiceServer::from_arc(telemetry))
        .add_service(LogServiceServer::new(telemetry::LogSvc {
            runtime_dir: args.runtime_dir.clone(),
        }))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutdown signal received");
        })
        .await?;

    // Best-effort cleanup of the socket file on graceful shutdown.
    let _ = std::fs::remove_file(path);
    Ok(())
}
