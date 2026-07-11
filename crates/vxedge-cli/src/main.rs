//! vxedge — thin vxAPI client. Every subcommand is a 1:1 wrapper over a vxAPI
//! RPC; no orchestration logic lives here (hard rule, AGENTS.md).
//!
//! Wired today: `status`, `up`, `down` (job submission + live progress
//! streaming). Remaining verbs print a scaffold notice until their
//! daemon-side services land.

use clap::{Parser, Subcommand};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use vxedge_api::v1::gateway_service_client::GatewayServiceClient;
use vxedge_api::v1::job_service_client::JobServiceClient;
use vxedge_api::v1::manifest_ref::Source;
use vxedge_api::v1::zone_service_client::ZoneServiceClient;
use vxedge_api::v1::{
    GatewayDownRequest, GatewayStatusRequest, GatewayUpRequest, Job, JobState, ManifestRef,
    VxidRef, WatchJobRequest, ZoneDownRequest, ZoneListRequest, ZoneUpRequest,
};

#[derive(Parser, Debug)]
#[command(
    name = "vxedge",
    version,
    about = "vxedge — manifest-driven SDV testbed orchestrator"
)]
struct Cli {
    /// vxAPI endpoint (unix:///path.sock; TCP arrives with the auth work).
    #[arg(
        long,
        global = true,
        default_value = "unix:///run/vxedge.sock",
        env = "VXEDGE_ENDPOINT"
    )]
    endpoint: String,

    /// Emit machine-readable JSON instead of human output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Bring the edge gateway (vxgateway) up.
    Up {
        /// Experiment manifest declaring the slices (path readable by the daemon).
        #[arg(long)]
        manifest: Option<String>,
    },
    /// Tear the edge gateway down.
    Down {
        #[arg(long)]
        manifest: Option<String>,
    },
    /// Show gateway status.
    Status,
    /// Zone lifecycle (vxzones).
    Zone {
        #[command(subcommand)]
        cmd: ZoneCmd,
    },
    /// Compute lifecycle (vxcompute).
    Compute {
        #[command(subcommand)]
        cmd: ComputeCmd,
    },
    /// OTA service lifecycle.
    Ota {
        #[command(subcommand)]
        cmd: SvcCmd,
    },
    /// Telemetry service lifecycle + collection.
    Telemetry {
        #[command(subcommand)]
        cmd: TelemetryCmd,
    },
    /// Full software-bootstrapped SDV (vehicle manifest).
    Vehicle {
        #[command(subcommand)]
        cmd: ManifestLifecycleCmd,
    },
    /// Manifest-driven experiments.
    Experiment {
        #[command(subcommand)]
        cmd: ManifestLifecycleCmd,
    },
    /// Traffic generation per manifest profiles.
    Traffic {
        #[command(subcommand)]
        cmd: TrafficCmd,
    },
}

#[derive(Subcommand, Debug)]
enum ZoneCmd {
    Up {
        #[arg(long)]
        vxid: String,
    },
    Down {
        #[arg(long)]
        vxid: String,
    },
    List,
    Add {
        #[arg(long)]
        manifest: String,
    },
    Rm {
        #[arg(long)]
        vxid: String,
    },
    /// Push a zone image to the daemon store / registry.
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeCmd {
    Up {
        #[arg(long)]
        vxid: String,
    },
    Down {
        #[arg(long)]
        vxid: String,
    },
    Add {
        #[arg(long)]
        manifest: String,
    },
    Rm {
        #[arg(long)]
        vxid: String,
    },
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
}

#[derive(Subcommand, Debug)]
enum ImageCmd {
    Push {
        #[arg(long)]
        vxid: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        registry: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SvcCmd {
    Up {
        #[arg(long)]
        experiment: String,
    },
    Down {
        #[arg(long)]
        experiment: String,
    },
}

#[derive(Subcommand, Debug)]
enum TelemetryCmd {
    Up {
        #[arg(long)]
        experiment: String,
    },
    Down {
        #[arg(long)]
        experiment: String,
    },
    Collect {
        #[arg(long)]
        manifest: String,
        #[arg(long)]
        out: String,
    },
}

#[derive(Subcommand, Debug)]
enum ManifestLifecycleCmd {
    Up {
        #[arg(long)]
        manifest: String,
    },
    Down {
        #[arg(long)]
        manifest: String,
    },
    Status {
        #[arg(long)]
        manifest: String,
    },
}

#[derive(Subcommand, Debug)]
enum TrafficCmd {
    Up {
        #[arg(long)]
        manifest: String,
    },
    Down {
        #[arg(long)]
        manifest: String,
    },
}

async fn connect(endpoint: &str) -> Result<Channel, Box<dyn std::error::Error>> {
    let Some(path) = endpoint.strip_prefix("unix://") else {
        return Err(format!(
            "unsupported endpoint `{endpoint}` — only unix:// is supported for now"
        )
        .into());
    };
    let path = path.to_string();
    // The URI is ignored for UDS; the connector below does the real work.
    let channel = Endpoint::try_from("http://vxedge.uds")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let path = path.clone();
            async move { Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(path).await?)) }
        }))
        .await?;
    Ok(channel)
}

fn manifest_ref(path: &Option<String>) -> Option<ManifestRef> {
    // The manifest path travels to the daemon, which reads it. Absolutize so
    // the daemon resolves it regardless of its own working directory.
    path.as_ref().map(|p| {
        let abs = std::path::Path::new(p)
            .canonicalize()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| p.clone());
        ManifestRef {
            source: Some(Source::Path(abs)),
            sha256: String::new(),
        }
    })
}

fn state_name(state: i32) -> &'static str {
    match JobState::try_from(state) {
        Ok(JobState::Pending) => "pending",
        Ok(JobState::Running) => "running",
        Ok(JobState::Succeeded) => "succeeded",
        Ok(JobState::Failed) => "failed",
        Ok(JobState::Cancelled) => "cancelled",
        _ => "unknown",
    }
}

/// Stream a job's progress to the terminal; exit non-zero on failure.
async fn watch_job(
    channel: Channel,
    job: Job,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut jobs = JobServiceClient::new(channel);
    let mut stream = jobs
        .watch(WatchJobRequest { id: job.id.clone() })
        .await?
        .into_inner();
    let mut last: Option<Job> = None;
    while let Some(snapshot) = stream.message().await? {
        if json {
            println!("{}", serde_json::to_string(&JsonJob::from(&snapshot))?);
        } else if !snapshot.message.is_empty() || snapshot.progress_percent > 0 {
            println!(
                "[{:>3}%] {} {}",
                snapshot.progress_percent, snapshot.kind, snapshot.message
            );
        }
        last = Some(snapshot);
    }
    match last {
        Some(job) if job.state == JobState::Succeeded as i32 => {
            if !json {
                println!("{}: succeeded", job.kind);
            }
            Ok(())
        }
        Some(job) => {
            if !json {
                eprintln!("{}: {} — {}", job.kind, state_name(job.state), job.error);
            }
            std::process::exit(1);
        }
        None => Err("job stream ended without a terminal state".into()),
    }
}

fn scaffold_notice(what: &str) {
    eprintln!(
        "vxedge: `{what}` is not wired to the daemon yet (scaffold) — see crates/vxedged TASKS"
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match &cli.command {
        Command::Status => {
            let channel = connect(&cli.endpoint).await.map_err(|e| {
                format!(
                    "cannot reach vxedged at {}: {e} (is `vxedged serve` running?)",
                    cli.endpoint
                )
            })?;
            let mut client = GatewayServiceClient::new(channel);
            let status = client.status(GatewayStatusRequest {}).await?.into_inner();
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&JsonStatus::from(&status))?
                );
            } else if let Some(s) = &status.status {
                println!("gateway {}: state={} detail={}", s.vxid, s.state, s.detail);
            } else {
                println!("gateway: no status reported");
            }
        }
        Command::Up { manifest } => {
            let channel = connect(&cli.endpoint).await?;
            let mut gw = GatewayServiceClient::new(channel.clone());
            let job = gw
                .up(GatewayUpRequest {
                    manifest: manifest_ref(manifest),
                })
                .await?
                .into_inner();
            watch_job(channel, job, cli.json).await?;
        }
        Command::Down { manifest } => {
            let channel = connect(&cli.endpoint).await?;
            let mut gw = GatewayServiceClient::new(channel.clone());
            let job = gw
                .down(GatewayDownRequest {
                    manifest: manifest_ref(manifest),
                })
                .await?
                .into_inner();
            watch_job(channel, job, cli.json).await?;
        }
        Command::Zone { cmd } => match cmd {
            ZoneCmd::Up { vxid } => {
                let channel = connect(&cli.endpoint).await?;
                let mut zc = ZoneServiceClient::new(channel.clone());
                let job = zc
                    .up(ZoneUpRequest {
                        r#ref: Some(VxidRef { vxid: vxid.clone() }),
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            ZoneCmd::Down { vxid } => {
                let channel = connect(&cli.endpoint).await?;
                let mut zc = ZoneServiceClient::new(channel.clone());
                let job = zc
                    .down(ZoneDownRequest {
                        r#ref: Some(VxidRef { vxid: vxid.clone() }),
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            ZoneCmd::List => {
                let channel = connect(&cli.endpoint).await?;
                let mut zc = ZoneServiceClient::new(channel);
                let resp = zc.list(ZoneListRequest {}).await?.into_inner();
                if cli.json {
                    let rows: Vec<serde_json::Value> = resp
                        .zones
                        .iter()
                        .map(|s| {
                            serde_json::json!({
                                "vxid": s.vxid, "state": s.state, "detail": s.detail
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    for s in &resp.zones {
                        println!("node {}: state={} {}", s.vxid, s.state, s.detail);
                    }
                }
            }
            other => scaffold_notice(&format!("zone {other:?}")),
        },
        Command::Compute { .. } => scaffold_notice("compute"),
        Command::Ota { .. } => scaffold_notice("ota"),
        Command::Telemetry { .. } => scaffold_notice("telemetry"),
        Command::Vehicle { .. } => scaffold_notice("vehicle"),
        Command::Experiment { .. } => scaffold_notice("experiment"),
        Command::Traffic { .. } => scaffold_notice("traffic"),
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct JsonStatus {
    vxid: String,
    state: i32,
    detail: String,
}

impl From<&vxedge_api::v1::GatewayStatus> for JsonStatus {
    fn from(g: &vxedge_api::v1::GatewayStatus) -> Self {
        let s = g.status.clone().unwrap_or_default();
        Self {
            vxid: s.vxid,
            state: s.state,
            detail: s.detail,
        }
    }
}

#[derive(serde::Serialize)]
struct JsonJob {
    id: String,
    kind: String,
    state: String,
    progress_percent: u32,
    message: String,
    error: String,
}

impl From<&Job> for JsonJob {
    fn from(j: &Job) -> Self {
        Self {
            id: j.id.clone(),
            kind: j.kind.clone(),
            state: state_name(j.state).to_string(),
            progress_percent: j.progress_percent,
            message: j.message.clone(),
            error: j.error.clone(),
        }
    }
}
