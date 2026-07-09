//! vxedge — thin vxAPI client. Every subcommand is a 1:1 wrapper over a vxAPI
//! RPC; no orchestration logic lives here (hard rule, AGENTS.md).
//!
//! Scaffold status: the full canonical command tree parses; `status` performs
//! a real RPC against vxedged. Remaining verbs print a scaffold notice until
//! their daemon-side services land.

use clap::{Parser, Subcommand};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use vxedge_api::v1::gateway_service_client::GatewayServiceClient;
use vxedge_api::v1::GatewayStatusRequest;

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
    Up,
    /// Tear the edge gateway down.
    Down,
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
            "unsupported endpoint `{endpoint}` — only unix:// is supported in this scaffold"
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
        Command::Up => scaffold_notice("up"),
        Command::Down => scaffold_notice("down"),
        Command::Zone { .. } => scaffold_notice("zone"),
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
