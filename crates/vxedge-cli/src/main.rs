//! vxedge — thin vxAPI client. Every subcommand is a 1:1 wrapper over a vxAPI
//! RPC; no orchestration logic lives here (hard rule, AGENTS.md).
//!
//! Wired today: `status`, `up`, `down`, `zone up|down|list`, `compute up|down|list`,
//! `ota up|down`, `traffic up|down` (job submission + live progress streaming); `manifest
//! validate` and `doctor` are local-only (no daemon connection — see their
//! own doc comments for why that's not a hard-rule violation). Remaining
//! verbs print a scaffold notice until their daemon-side services land — this includes
//! `zone`/`compute`'s own `add`/`rm`/`image` verbs, which are deliberate `unimplemented` RPC
//! stubs server-side, not merely unwired.
//!
//! `experiment` verbs are **addressed by the working directory**, not by
//! `--manifest`: they resolve the single `.vxp` where they were run, the way
//! `terraform apply` resolves the configuration in the current directory.
//! `--chdir` is the escape hatch for scripts.

use clap::{Parser, Subcommand};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

mod doctor;

use vxedge_api::v1::compute_service_client::ComputeServiceClient;
use vxedge_api::v1::experiment_service_client::ExperimentServiceClient;
use vxedge_api::v1::gateway_service_client::GatewayServiceClient;
use vxedge_api::v1::job_service_client::JobServiceClient;
use vxedge_api::v1::manifest_ref::Source;
use vxedge_api::v1::svc_service_client::SvcServiceClient;
use vxedge_api::v1::traffic_service_client::TrafficServiceClient;
use vxedge_api::v1::zone_service_client::ZoneServiceClient;
use vxedge_api::v1::{
    ComputeDownRequest, ComputeListRequest, ComputeUpRequest, ExperimentDownRequest,
    ExperimentStatusRequest, ExperimentUpRequest, GatewayDownRequest, GatewayStatusRequest,
    GatewayUpRequest, Job, JobState, ManifestRef, SvcDownRequest, SvcUpRequest, TrafficDownRequest,
    TrafficUpRequest, VxidRef, WatchJobRequest, ZoneDownRequest, ZoneListRequest, ZoneUpRequest,
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

    /// Run as if started in this directory. The escape hatch for scripts that would otherwise
    /// have to `cd` — experiment verbs resolve the workspace from the working directory.
    #[arg(long, global = true, value_name = "DIR")]
    chdir: Option<String>,

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
        cmd: ExperimentCmd,
    },
    /// Traffic generation per manifest profiles.
    Traffic {
        #[command(subcommand)]
        cmd: TrafficCmd,
    },
    /// Manifest schema + semantic checks (local — no daemon connection needed;
    /// this is the same read-only check `vxedge up` runs before provisioning,
    /// exposed standalone for CI and quick iteration).
    Manifest {
        #[command(subcommand)]
        cmd: ManifestCmd,
    },
    /// Stream what the daemon is doing, live.
    ///
    /// Domain facts about resources — `gateway.up`, `gateway.slice-applied`, `zone.up`,
    /// `service.up`, `traffic.started` — not job progress, which belongs to the job that
    /// reports it. The stream is lossy by design: a subscriber that falls behind skips rather
    /// than slowing the daemon down, and it is never a record. For "what happened, in order,
    /// without loss", read the audit log.
    Events {
        /// Only these kinds (repeatable, exact match). Default: everything.
        #[arg(long = "kind")]
        kinds: Vec<String>,
    },
    /// Host preflight per docs/TESTBED-SETUP.md tiers (local — no daemon
    /// connection needed, since its job is to check whether the environment
    /// can even run one).
    Doctor {
        /// Check up to this tier (0=control-plane dev, 1=H1 experiments,
        /// 2=real workloads/5G/CARLA, 3=full vehicle+HMI). Default: 1, the
        /// tier phase 1 experiments need.
        #[arg(long, default_value_t = 1)]
        tier: u8,
    },
}

#[derive(Subcommand, Debug)]
enum ManifestCmd {
    /// Parse + validate an experiment manifest (schema + semantic checks).
    Validate {
        #[arg(long)]
        manifest: String,
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
    List,
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
        /// Workspace to deploy from. Defaults to the current directory, like `experiment up`.
        #[arg(long)]
        experiment: Option<String>,
    },
    Down {
        #[arg(long)]
        experiment: Option<String>,
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

/// Experiments add `new` to the shared lifecycle verbs: scaffolding a workspace is local
/// file generation, not a daemon call, so it does not fit `ManifestLifecycleCmd`.
///
/// Every verb except `new` is **addressed by the working directory** — no `--manifest`, no
/// name. `vxedge experiment up` resolves the single `.vxp` where it was run, exactly as
/// `terraform apply` resolves the configuration in the current directory. Use `--chdir` to
/// run from elsewhere.
#[derive(Subcommand, Debug)]
enum ExperimentCmd {
    /// Create a workspace that is ready to edit — no daemon, no network, no GPU.
    New {
        /// Directory to create. Also the experiment id unless `--id` overrides it, and the
        /// published repository path component.
        name: String,
        /// Experiment id, when it should differ from the directory name — a migration keeps the
        /// folder distinct from the frozen manifest beside it (`exp-003-ws`) while the
        /// experiment id stays `exp-003`, so results and comparisons line up.
        #[arg(long, value_name = "EXPERIMENT_ID")]
        id: Option<String>,
        #[arg(long, value_enum, default_value_t = TemplateArg::Minimal)]
        template: TemplateArg,
        /// Parent directory to create the workspace in.
        #[arg(long, default_value = ".")]
        path: String,
    },
    /// Load and cross-check the workspace here (local — no daemon, no network).
    Validate,
    /// Bring the workspace here up.
    Up {
        /// Restrict to specific stages, repeatable (default: all, in dependency order).
        ///
        /// A campaign frequently wants a subset. exp-001 measures on the host-side veth peers
        /// and never starts its zones; exp-002's isolation contract is one packaging arm at a
        /// time, so it takes `--stage gateway` and then brings each arm up with
        /// `vxedge zone up --vxid <device>`.
        #[arg(long = "stage", value_enum)]
        stages: Vec<StageArg>,
    },
    /// Tear the workspace here down.
    Down {
        /// Restrict to specific stages, repeatable (default: all, in reverse order).
        #[arg(long = "stage", value_enum)]
        stages: Vec<StageArg>,
    },
    /// Show status for the workspace here.
    Status,
}

/// Mirrors `vxedge_core::Template`. Kept here so the domain library stays free of a clap
/// dependency (house rule: binaries own arg parsing, libraries own logic).
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum TemplateArg {
    Minimal,
    Slicing,
}

impl From<TemplateArg> for vxedge_core::Template {
    fn from(value: TemplateArg) -> Self {
        match value {
            TemplateArg::Minimal => vxedge_core::Template::Minimal,
            TemplateArg::Slicing => vxedge_core::Template::Slicing,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum StageArg {
    Gateway,
    Zones,
    Services,
}

impl StageArg {
    fn as_proto(self) -> i32 {
        match self {
            StageArg::Gateway => vxedge_api::v1::ExperimentStage::Gateway as i32,
            StageArg::Zones => vxedge_api::v1::ExperimentStage::Zones as i32,
            StageArg::Services => vxedge_api::v1::ExperimentStage::Services as i32,
        }
    }
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
        /// Restrict to specific `traffic_profiles` ids (repeatable; default:
        /// every profile in the manifest).
        #[arg(long = "profile")]
        profiles: Vec<String>,
    },
    Down {
        #[arg(long)]
        manifest: String,
        #[arg(long = "profile")]
        profiles: Vec<String>,
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
            // A watch stream delivers the current snapshot on subscribe and then again on the
            // first change, so the same step would otherwise print twice. Compare on what is
            // actually shown rather than on the whole job — `updated_unix_ms` differs every
            // time and would defeat the check.
            let shown = (snapshot.progress_percent, snapshot.message.as_str());
            let repeat = last
                .as_ref()
                .is_some_and(|l| (l.progress_percent, l.message.as_str()) == shown);
            if !repeat {
                println!(
                    "[{:>3}%] {} {}",
                    snapshot.progress_percent, snapshot.kind, snapshot.message
                );
            }
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

/// The directory an experiment verb is addressed against. Always the process working
/// directory — `--chdir` has already moved it by the time this is called, so there is exactly
/// one notion of "here" and no second code path to keep in agreement.
fn here() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    Ok(std::env::current_dir()?)
}

/// Resolves a verb's workspace argument: an explicit path wins, otherwise fall back to
/// discovering the workspace from the working directory — the same rule `experiment` verbs
/// use, extended to verbs (`ota`) that predate that convention and still take an explicit
/// override for scripts that address a workspace they are not standing inside.
fn resolve_experiment_arg(
    experiment: &Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    match experiment {
        Some(path) => Ok(path.clone()),
        None => {
            let found = vxedge_core::discover(&here()?)?;
            Ok(found.root.to_string_lossy().to_string())
        }
    }
}

/// Errors reach the user through `Display`, not `Debug`.
///
/// `fn main() -> Result<_, _>` formats its error with `{:?}`, which for a `thiserror` type
/// prints the struct — `NotAWorkspace { dir: "/x" }` instead of the sentence explaining what to
/// do about it. Every actionable error message in this crate and in `vxedge-core` was being
/// discarded that way. So `main` handles the error itself and walks the `source` chain, which
/// also surfaces the underlying cause (an io error, a YAML parse position) that
/// `Box<dyn Error>`'s own `Display` stops short of.
#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        let mut source = e.source();
        while let Some(cause) = source {
            eprintln!("  caused by: {cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Applied before anything else, so every relative path in the run — workspace discovery,
    // `--manifest` on the verbs that still take one, output files — resolves against the same
    // directory. Terraform's `-chdir` works this way for the same reason.
    if let Some(dir) = &cli.chdir {
        std::env::set_current_dir(dir).map_err(|e| format!("--chdir {dir}: {e}"))?;
    }

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
                for sl in &status.slices {
                    println!(
                        "  slice {}: state={} detail={}",
                        sl.slice_id, sl.state, sl.detail
                    );
                }
                for imp in &status.impairments {
                    println!(
                        "  impairment {} ({}): state={} {}",
                        imp.link, imp.access_port, imp.state, imp.detail
                    );
                }
                for f in &status.ingress_filters {
                    println!(
                        "  ingress filter {} ({}): state={} {}",
                        f.node, f.access_port, f.state, f.detail
                    );
                }
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
        Command::Compute { cmd } => match cmd {
            ComputeCmd::Up { vxid } => {
                let channel = connect(&cli.endpoint).await?;
                let mut cc = ComputeServiceClient::new(channel.clone());
                let job = cc
                    .up(ComputeUpRequest {
                        r#ref: Some(VxidRef { vxid: vxid.clone() }),
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            ComputeCmd::Down { vxid } => {
                let channel = connect(&cli.endpoint).await?;
                let mut cc = ComputeServiceClient::new(channel.clone());
                let job = cc
                    .down(ComputeDownRequest {
                        r#ref: Some(VxidRef { vxid: vxid.clone() }),
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            ComputeCmd::List => {
                let channel = connect(&cli.endpoint).await?;
                let mut cc = ComputeServiceClient::new(channel);
                let resp = cc.list(ComputeListRequest {}).await?.into_inner();
                if cli.json {
                    let rows: Vec<serde_json::Value> = resp
                        .nodes
                        .iter()
                        .map(|s| {
                            serde_json::json!({
                                "vxid": s.vxid, "state": s.state, "detail": s.detail
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    for s in &resp.nodes {
                        println!("node {}: state={} {}", s.vxid, s.state, s.detail);
                    }
                }
            }
            other => scaffold_notice(&format!("compute {other:?}")),
        },
        Command::Ota { cmd } => match cmd {
            SvcCmd::Up { experiment } => {
                let workspace = resolve_experiment_arg(experiment)?;
                let channel = connect(&cli.endpoint).await?;
                let mut svc = SvcServiceClient::new(channel.clone());
                let job = svc
                    .up(SvcUpRequest {
                        service_type: "ota".to_string(),
                        experiment: manifest_ref(&Some(workspace)),
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            SvcCmd::Down { experiment } => {
                let workspace = resolve_experiment_arg(experiment)?;
                let channel = connect(&cli.endpoint).await?;
                let mut svc = SvcServiceClient::new(channel.clone());
                let job = svc
                    .down(SvcDownRequest {
                        service_type: "ota".to_string(),
                        experiment: manifest_ref(&Some(workspace)),
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
        },
        Command::Telemetry { .. } => scaffold_notice("telemetry"),
        Command::Vehicle { .. } => scaffold_notice("vehicle"),
        Command::Experiment { cmd } => match cmd {
            ExperimentCmd::New {
                name,
                id,
                template,
                path,
            } => {
                let template: vxedge_core::Template = (*template).into();
                let ws = vxedge_core::WorkspacePlan::plan_with_id(
                    name,
                    id.as_deref().unwrap_or(name),
                    template,
                )?;
                let root = ws.write(std::path::Path::new(path))?;
                println!("created {}", root.display());
                for file in &ws.files {
                    println!("  {}", file.path.display());
                }
                println!(
                    "\nnext:  cd {} && $EDITOR main.yaml\nsee:   {}/README.md",
                    root.display(),
                    root.display()
                );
                if template.requires_gpu() {
                    // Stated at scaffold time so the requirement is known before a long
                    // build rather than after one.
                    eprintln!(
                        "note: this template needs a Tier 2 GPU host \
                         (RTX-class, >=12 GB VRAM, tensor cores)"
                    );
                }
            }
            ExperimentCmd::Validate => {
                let here = here()?;
                let found = vxedge_core::discover(&here)?;
                let ws = vxedge_core::Workspace::load(&found.root)?;
                let errors = ws.validate();
                if errors.is_empty() {
                    println!(
                        "ok: {} — {} experiment, {} vehicle(s), {} baseline(s), {} road(s)",
                        found.package.id,
                        ws.experiments.len(),
                        ws.vehicles.len(),
                        ws.baselines.len(),
                        ws.roads.len()
                    );
                } else {
                    // Every problem at once: an engineer fixing a workspace wants the list,
                    // not a fix-one-rerun loop.
                    eprintln!("{} problem(s) in {}:", errors.len(), found.root.display());
                    for e in &errors {
                        eprintln!("  - {e}");
                    }
                    std::process::exit(1);
                }
            }
            // The daemon receives the workspace *directory* and lowers it itself. The CLI
            // never learns that §6.1 exists — which is the point of §6.0 being the only
            // authored schema.
            ExperimentCmd::Up { .. } | ExperimentCmd::Down { .. } | ExperimentCmd::Status => {
                let found = vxedge_core::discover(&here()?)?;
                let workspace = found.root.to_string_lossy().to_string();
                let channel = connect(&cli.endpoint).await?;
                let mut exp = ExperimentServiceClient::new(channel.clone());
                match cmd {
                    ExperimentCmd::Status => {
                        let st = exp
                            .status(ExperimentStatusRequest {
                                manifest: manifest_ref(&Some(workspace)),
                            })
                            .await?
                            .into_inner();
                        let top = st.status.clone().unwrap_or_default();
                        println!("{}: {}", top.vxid, top.detail);
                        for c in &st.components {
                            println!("  {} [{}] {}", c.vxid, c.state, c.detail);
                        }
                    }
                    ExperimentCmd::Up { stages } => {
                        let job = exp
                            .up(ExperimentUpRequest {
                                manifest: manifest_ref(&Some(workspace)),
                                stages: stages.iter().map(|s| s.as_proto()).collect(),
                            })
                            .await?
                            .into_inner();
                        watch_job(channel, job, cli.json).await?;
                    }
                    _ => {
                        let stages = match cmd {
                            ExperimentCmd::Down { stages } => {
                                stages.iter().map(|s| s.as_proto()).collect()
                            }
                            _ => Vec::new(),
                        };
                        let job = exp
                            .down(ExperimentDownRequest {
                                manifest: manifest_ref(&Some(workspace)),
                                stages,
                            })
                            .await?
                            .into_inner();
                        watch_job(channel, job, cli.json).await?;
                    }
                }
            }
        },
        Command::Traffic { cmd } => match cmd {
            TrafficCmd::Up { manifest, profiles } => {
                let channel = connect(&cli.endpoint).await?;
                let mut traffic = TrafficServiceClient::new(channel.clone());
                let job = traffic
                    .up(TrafficUpRequest {
                        manifest: manifest_ref(&Some(manifest.clone())),
                        profiles: profiles.clone(),
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            TrafficCmd::Down { manifest, profiles } => {
                let channel = connect(&cli.endpoint).await?;
                let mut traffic = TrafficServiceClient::new(channel.clone());
                let job = traffic
                    .down(TrafficDownRequest {
                        manifest: manifest_ref(&Some(manifest.clone())),
                        profiles: profiles.clone(),
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
        },
        Command::Manifest { cmd } => match cmd {
            ManifestCmd::Validate { manifest } => {
                // A §6.0 workspace: a directory, or one file of `---`-separated documents.
                // It is the only schema there is — §6.1 experiment manifests and §6.2 vehicle
                // manifests were retired on 2026-07-31 and their structs deleted, once
                // `vxmodel render` (track M) began emitting workspaces too.
                let path = std::path::Path::new(manifest);
                let result = if path.is_dir() {
                    validate_workspace_dir(path)
                } else {
                    let yaml = std::fs::read_to_string(path)
                        .map_err(|e| format!("cannot read `{manifest}`: {e}"))?;
                    validate_workspace_yaml(&yaml)
                };
                let kind = "workspace";
                match result {
                    Ok((summary, human)) => {
                        if cli.json {
                            let mut obj = serde_json::json!({
                                "manifest": manifest, "valid": true, "kind": kind,
                            });
                            if let (Some(o), Some(extra)) =
                                (obj.as_object_mut(), summary.as_object())
                            {
                                for (k, v) in extra {
                                    o.insert(k.clone(), v.clone());
                                }
                            }
                            println!("{}", serde_json::to_string_pretty(&obj)?);
                        } else {
                            println!("OK: {manifest} — {kind}: {human}");
                        }
                    }
                    Err(e) => {
                        if cli.json {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "manifest": manifest, "valid": false, "kind": kind,
                                    "error": e,
                                })
                            );
                        } else {
                            eprintln!("INVALID: {manifest}\n{e}");
                        }
                        std::process::exit(1);
                    }
                }
            }
        },
        Command::Events { kinds } => {
            let channel = connect(&cli.endpoint).await?;
            let mut client = vxedge_api::v1::event_service_client::EventServiceClient::new(channel);
            let mut stream = client
                .subscribe(vxedge_api::v1::SubscribeRequest {
                    kinds: kinds.clone(),
                })
                .await?
                .into_inner();
            while let Some(event) = stream.message().await? {
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "id": event.id, "ts_unix_ms": event.ts_unix_ms,
                            "kind": event.kind, "resource_vxid": event.resource_vxid,
                            "detail": event.detail,
                        })
                    );
                } else {
                    println!(
                        "{:<22} {:<20} {}",
                        event.kind, event.resource_vxid, event.detail
                    );
                }
            }
        }
        Command::Doctor { tier } => doctor::run(*tier, cli.json)?,
    }
    Ok(())
}

/// A §6.0 workspace directory: every cross-file join, then every model invariant.
fn validate_workspace_dir(root: &std::path::Path) -> Result<Summary, String> {
    let ws = vxedge_core::Workspace::load(root).map_err(|e| e.to_string())?;
    finish_workspace(ws, || vxedge_core::model::load_dir(root))
}

/// The same checks over a workspace held in one file of `---`-separated documents.
fn validate_workspace_yaml(yaml: &str) -> Result<Summary, String> {
    let ws = vxedge_core::Workspace::from_yaml(yaml).map_err(|e| e.to_string())?;
    finish_workspace(ws, || vxedge_core::model::load_yaml(yaml))
}

/// Validating is not "does it parse": a workspace that passes its cross-file joins can still
/// violate a model invariant, and `up` would be the one to find out. Both run here, so the
/// local check is the same check provisioning runs.
fn finish_workspace(
    ws: vxedge_core::Workspace,
    model: impl FnOnce() -> Result<vxedge_core::model::Experiment, vxedge_core::model::LoadError>,
) -> Result<Summary, String> {
    let errors = ws.validate();
    if !errors.is_empty() {
        return Err(errors
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let model = model().map_err(|e| e.to_string())?;
    let devices: usize = model.entities.iter().map(|e| e.devices.len()).sum();
    let (vehicles, slices) = (model.entities.len(), model.slices.len());
    Ok((
        serde_json::json!({
            "experiment_id": model.id,
            "vehicles": vehicles,
            "devices": devices,
            "slices": slices,
        }),
        format!(
            "experiment `{}`, {vehicles} vehicle(s), {devices} device(s), {slices} slice(s)",
            model.id
        ),
    ))
}

/// A machine-readable summary and the sentence a human reads.
type Summary = (serde_json::Value, String);

#[derive(serde::Serialize)]
struct JsonSliceStatus {
    slice_id: String,
    state: i32,
    detail: String,
}

#[derive(serde::Serialize)]
struct JsonImpairmentStatus {
    link: String,
    access_port: String,
    state: i32,
    detail: String,
}

#[derive(serde::Serialize)]
struct JsonIngressFilterStatus {
    node: String,
    access_port: String,
    state: i32,
    detail: String,
}

#[derive(serde::Serialize)]
struct JsonStatus {
    vxid: String,
    state: i32,
    detail: String,
    slices: Vec<JsonSliceStatus>,
    impairments: Vec<JsonImpairmentStatus>,
    ingress_filters: Vec<JsonIngressFilterStatus>,
}

impl From<&vxedge_api::v1::GatewayStatus> for JsonStatus {
    fn from(g: &vxedge_api::v1::GatewayStatus) -> Self {
        let s = g.status.clone().unwrap_or_default();
        Self {
            vxid: s.vxid,
            state: s.state,
            detail: s.detail,
            slices: g
                .slices
                .iter()
                .map(|sl| JsonSliceStatus {
                    slice_id: sl.slice_id.clone(),
                    state: sl.state,
                    detail: sl.detail.clone(),
                })
                .collect(),
            impairments: g
                .impairments
                .iter()
                .map(|imp| JsonImpairmentStatus {
                    link: imp.link.clone(),
                    access_port: imp.access_port.clone(),
                    state: imp.state,
                    detail: imp.detail.clone(),
                })
                .collect(),
            ingress_filters: g
                .ingress_filters
                .iter()
                .map(|f| JsonIngressFilterStatus {
                    node: f.node.clone(),
                    access_port: f.access_port.clone(),
                    state: f.state,
                    detail: f.detail.clone(),
                })
                .collect(),
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
