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
use vxedge_api::v1::measurement_service_client::MeasurementServiceClient;
use vxedge_api::v1::nos_check_service_client::NosCheckServiceClient;
use vxedge_api::v1::svc_service_client::SvcServiceClient;
use vxedge_api::v1::traffic_service_client::TrafficServiceClient;
use vxedge_api::v1::vehicle_service_client::VehicleServiceClient;
use vxedge_api::v1::zone_service_client::ZoneServiceClient;
use vxedge_api::v1::{
    ComputeDownRequest, ComputeListRequest, ComputeUpRequest, ExperimentDownRequest,
    ExperimentStatusRequest, ExperimentUpRequest, GatewayDownRequest, GatewayStatusRequest,
    GatewayUpRequest, Job, JobState, ManifestRef, NosCheckDownRequest, NosCheckStatusRequest,
    NosCheckUpRequest, NosCheckVerifyRequest, SvcDownRequest, SvcStatusRequest, SvcUpRequest,
    TrafficDownRequest, TrafficUpRequest, VehicleDownRequest, VehicleStatusRequest,
    VehicleUpRequest, VxidRef, WatchJobRequest, ZoneDownRequest, ZoneListRequest, ZoneUpRequest,
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
        /// Skip the `vxp.lock` check while authoring. The run records that it ran unlocked.
        #[arg(long)]
        unlocked: bool,
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
    /// An experiment's criteria judged over results archives on this machine, offline: no
    /// daemon, no campaign record (ARCHITECTURE.md §6.0.3).
    Results {
        #[command(subcommand)]
        cmd: ResultsCmd,
    },
    /// `vxp/1` packages: scaffold, validate, lock, pack, unpack and inspect (local and offline,
    /// ARCHITECTURE.md §6.0.7).
    Pkg {
        #[command(subcommand)]
        cmd: PkgCmd,
    },
    /// Traffic generation per manifest profiles.
    Traffic {
        #[command(subcommand)]
        cmd: TrafficCmd,
    },
    /// The experiment's campaign: its trials, and running them. Addressed by the working
    /// directory, as the experiment verbs are.
    Campaign {
        #[command(subcommand)]
        cmd: CampaignCmd,
    },
    /// The experiment's `measurements`, run inside each end's network namespace. Addressed by
    /// the working directory, as the experiment verbs are; the experiment must be up.
    Measure {
        #[command(subcommand)]
        cmd: MeasureCmd,
    },
    /// `network.nos_check` — check an experiment's declared VLAN intent against a real
    /// production NOS (ARCHITECTURE.md §7/§6.0.3). Never part of `experiment up`: a missing or
    /// failed check never blocks or changes what an experiment actually provisions or measures.
    NosCheck {
        #[command(subcommand)]
        cmd: NosCheckCmd,
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
    /// Report the service, wherever the workspace runs it.
    Status {
        #[arg(long)]
        experiment: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum NosCheckCmd {
    /// Boot a long-lived instance for manual inspection.
    Up {
        /// Workspace to check. Defaults to the current directory, like `experiment up`.
        #[arg(long)]
        experiment: Option<String>,
    },
    Down {
        #[arg(long)]
        experiment: Option<String>,
    },
    Status {
        #[arg(long)]
        experiment: Option<String>,
    },
    /// Boot a fresh, disposable instance, load the experiment's declared `network.slices` onto
    /// it, confirm every field reads back exactly, then tear down — one self-contained check.
    Verify {
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
    /// Archive what the telemetry service ingested, with the workspace and image digests.
    Collect {
        #[arg(long)]
        manifest: String,
        /// Where to write the archive; relative to the workspace. Default:
        /// `results/<id>-<time>.zip`.
        #[arg(long, default_value = "")]
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
        /// The organisation that will publish the package: the `namespace` in its
        /// `namespace/name` id. `local` means not yet published.
        #[arg(long, default_value = vxedge_core::scaffold::DEFAULT_NAMESPACE)]
        namespace: String,
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
        /// Skip the `vxp.lock` check while authoring. The run records that it ran unlocked.
        #[arg(long)]
        unlocked: bool,
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

#[derive(Subcommand, Debug)]
enum PkgCmd {
    /// Scaffold a `vxp/1` package and write its first `vxp.lock` (same as `experiment new`).
    New {
        /// Directory to create. Also the experiment id, and the package name, unless `--id`
        /// overrides it.
        name: String,
        #[arg(long, value_name = "EXPERIMENT_ID")]
        id: Option<String>,
        #[arg(long, value_enum, default_value_t = TemplateArg::Minimal)]
        template: TemplateArg,
        /// Parent directory to create the package in.
        #[arg(long, default_value = ".")]
        path: String,
        /// The organisation that will publish the package. `local` means not yet published.
        #[arg(long, default_value = vxedge_core::scaffold::DEFAULT_NAMESPACE)]
        namespace: String,
    },
    /// Every check `experiment validate` makes, plus the package's own: regular files only,
    /// packable paths, and a `vxp.lock` that matches.
    Validate,
    /// Write `vxp.lock` for the package here.
    Lock {
        /// Report whether the lock matches, without writing it. Exits non-zero when it does not.
        #[arg(long)]
        check: bool,
    },
    /// Write `<name>-<version>.vxp` and print its digest. Refuses a package whose lock does not
    /// match.
    Pack {
        /// Directory to write into (default: the folder that holds the package). Never inside
        /// the package: the file would be an input to its own next pack.
        #[arg(long, value_name = "DIR")]
        out: Option<String>,
        /// Also copy every published part and image the lock lists into an OCI layout beside the
        /// packed file, `<name>-<version>.parts/`, for an air-gapped host (`pkg pull --layout`).
        /// The packed file and its digest are unchanged.
        #[arg(long)]
        offline: bool,
    },
    /// Restore a source folder from a packed `.vxp`.
    Unpack {
        /// The packed file.
        file: String,
        /// Directory to create (default: the file's name without `.vxp`, here). Must not exist
        /// or be empty.
        #[arg(long, value_name = "DIR")]
        into: Option<String>,
    },
    /// Print a package's descriptor, lock and digest, from a packed file or the source here.
    Inspect {
        /// A packed `.vxp`. Without it, the package in the current directory.
        file: Option<String>,
    },
    /// Push a packed package to an OCI registry, tagged with its version, through `oras`. The
    /// layer is the packed file unchanged, so its digest is the package digest `pull` takes.
    Push {
        /// The repository, without a tag or digest, such as `ghcr.io/acme/experiments/demo`.
        repository: String,
        /// The packed file to push (default: pack the package here, which needs a matching lock).
        #[arg(long, value_name = "FILE")]
        file: Option<String>,
    },
    /// Pull a package by its package digest into the part cache, checking that the bytes hash to
    /// it and are a canonical packed package.
    Pull {
        /// `<repository>@sha256:<hex>`, the digest `pkg pack` or `pkg push` printed.
        #[arg(required_unless_present = "layout")]
        reference: Option<String>,
        /// Load a layout written by `pkg pack --offline`: parts into the part cache, images into
        /// the Podman store under the refs and digests the documents use. Needs no network.
        #[arg(long, value_name = "DIR")]
        layout: Option<String>,
        /// Also restore a source folder here (must not exist or be empty).
        #[arg(long, value_name = "DIR")]
        into: Option<String>,
        /// The part cache to pull into: the daemon's `--cache-dir`, so its `up` finds the
        /// package. Default: `$XDG_CACHE_HOME/vxedge/parts` or `$HOME/.cache/vxedge/parts`.
        #[arg(long, value_name = "DIR", env = "VXEDGE_CACHE_DIR")]
        cache_dir: Option<String>,
    },
    /// Sign a packed package with a cosign private key, for an air-gapped lab. Writes
    /// `<file>.sigstore.json` beside it; no transparency log is used.
    Sign {
        /// The packed `.vxp` file.
        file: String,
        /// The cosign private key (`cosign generate-key-pair`). `COSIGN_PASSWORD` unlocks it.
        #[arg(long, value_name = "KEY")]
        key: String,
    },
    /// Verify a packed package's signature and record the verification on this host, keyed by
    /// its package digest, where a trust policy finds it. Give `--key` for a key-pair signature
    /// or `--repo` for a GitHub artifact attestation.
    Verify {
        /// The packed `.vxp` file.
        file: String,
        /// The signer's cosign public key; the signature is `<file>.sigstore.json`.
        #[arg(
            long,
            value_name = "PUBLIC_KEY",
            conflicts_with = "repo",
            required_unless_present = "repo"
        )]
        key: Option<String>,
        /// The repository whose workflow attested the package, `<owner>/<repo>`.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
        /// The workflow that must have made the attestation, such as
        /// `<owner>/<repo>/.github/workflows/release.yml`.
        #[arg(long, value_name = "WORKFLOW", requires = "repo")]
        signer_workflow: Option<String>,
        /// The part cache to record in: the daemon's `--cache-dir`, so its trust policy finds the
        /// record. Default: `$XDG_CACHE_HOME/vxedge/parts` or `$HOME/.cache/vxedge/parts`.
        #[arg(long, value_name = "DIR", env = "VXEDGE_CACHE_DIR")]
        cache_dir: Option<String>,
    },
    /// Copy another folder's vxedge documents into the package here, such as a reference vehicle,
    /// and record where they came from in `vxp.lock`. Documents only: scripts and build outputs
    /// stay behind and are listed.
    Vendor {
        /// The folder to copy from, for example `../../vehicles/vx-reference-sedan-01`.
        #[arg(required_unless_present = "check", conflicts_with = "check")]
        source: Option<String>,
        /// The source's name in the lock, `<owner>/<repo>/<path>`. Default: read from the
        /// source's git checkout.
        #[arg(long = "as", value_name = "SOURCE")]
        as_id: Option<String>,
        /// Overwrite documents the package has that were not vendored from this source.
        #[arg(long)]
        replace: bool,
        /// Report vendored copies edited here, or changed at a source found in this checkout.
        /// Exits non-zero when there are any.
        #[arg(long)]
        check: bool,
    },
}

/// A staging folder removed when it goes out of scope.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `git -C dir args`, trimmed, when it succeeds.
fn git(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `owner/repo` from a remote URL: `https://host/o/r(.git)`, `ssh://git@host/o/r`, `git@host:o/r`,
/// or a local path ending in `o/r`.
fn owner_repo(url: &str) -> Option<String> {
    let url = url.trim_end_matches('/').trim_end_matches(".git");
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        None => url.split_once(':').map_or(url, |(_, rest)| rest),
    };
    let mut parts = path.rsplit('/');
    let (repo, owner) = (parts.next()?, parts.next()?);
    (!repo.is_empty() && !owner.is_empty()).then(|| format!("{owner}/{repo}"))
}

/// The files git tracks under `dir`, relative to it, or `None` outside a git checkout.
fn tracked_files(dir: &std::path::Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "-z", "--", "."])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(
        out.stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect(),
    )
}

/// `<owner>/<repo>/<path>` for a folder in a git checkout with an `origin` remote.
fn source_id(dir: &std::path::Path) -> Option<String> {
    let repo = owner_repo(&git(dir, &["remote", "get-url", "origin"])?)?;
    let prefix = git(dir, &["rev-parse", "--show-prefix"])?;
    let prefix = prefix.trim_end_matches('/');
    Some(if prefix.is_empty() {
        repo
    } else {
        format!("{repo}/{prefix}")
    })
}

/// The folder a vendored `source` names, when the package sits in a checkout of that repository.
fn resolve_source(root: &std::path::Path, source: &str) -> Option<std::path::PathBuf> {
    let repo = owner_repo(&git(root, &["remote", "get-url", "origin"])?)?;
    let top = std::path::PathBuf::from(git(root, &["rev-parse", "--show-toplevel"])?);
    let dir = match source.strip_prefix(repo.as_str()) {
        Some("") => top,
        Some(rest) => top.join(rest.strip_prefix('/')?),
        None => return None,
    };
    dir.is_dir().then_some(dir)
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
    Compute,
    SignalPlane,
    Services,
}

impl StageArg {
    fn as_proto(self) -> i32 {
        match self {
            StageArg::Gateway => vxedge_api::v1::ExperimentStage::Gateway as i32,
            StageArg::Zones => vxedge_api::v1::ExperimentStage::Zones as i32,
            StageArg::Compute => vxedge_api::v1::ExperimentStage::Compute as i32,
            StageArg::SignalPlane => vxedge_api::v1::ExperimentStage::SignalPlane as i32,
            StageArg::Services => vxedge_api::v1::ExperimentStage::Services as i32,
        }
    }
}

#[derive(Subcommand, Debug)]
enum ManifestLifecycleCmd {
    Up {
        #[arg(long)]
        manifest: String,
        /// Restrict to specific stages, repeatable (default: all, in dependency order).
        #[arg(long = "stage", value_enum)]
        stages: Vec<StageArg>,
        /// Skip the `vxp.lock` check while authoring. The run records that it ran unlocked.
        #[arg(long)]
        unlocked: bool,
    },
    Down {
        #[arg(long)]
        manifest: String,
        /// Restrict to specific stages, repeatable (default: all, in reverse order).
        #[arg(long = "stage", value_enum)]
        stages: Vec<StageArg>,
    },
    Status {
        #[arg(long)]
        manifest: String,
    },
}

#[derive(Subcommand, Debug)]
enum CampaignCmd {
    /// The trials the campaign would run, checked. Touches nothing.
    Plan {
        #[arg(long, default_value = "")]
        profile: String,
    },
    /// Run the campaign's trials, one after another, and follow the job.
    Start {
        #[arg(long, default_value = "")]
        profile: String,
        /// A retry with the same id resumes the campaign, skipping archived trials.
        #[arg(long, default_value = "")]
        request_id: String,
        /// Skip each trial's `vxp.lock` check while authoring. Recorded with the campaign.
        #[arg(long)]
        unlocked: bool,
    },
    /// One campaign by id, or every campaign on the daemon.
    Status { campaign_id: Option<String> },
    /// The experiment's criteria judged over a campaign's archived trials, read from the
    /// archives (`ResultService.Verdict`).
    Verdict { campaign_id: String },
    /// A campaign against a reference campaign: each metric's median at each point against the
    /// reference's median and IQR, and each criterion's outcome (`ResultService.Compare`).
    Compare {
        campaign_id: String,
        reference_campaign_id: String,
        /// A workspace whose `results/<reference_campaign_id>/campaign.json` is the reference,
        /// for a campaign this host did not run, such as a frozen one.
        #[arg(long)]
        reference_workspace: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ResultsCmd {
    /// Judge the experiment's criteria over these archives (files, or folders of `.zip`).
    Verdict {
        /// The experiment: a workspace folder, or a file holding its `kind: experiment` document.
        #[arg(long)]
        experiment: std::path::PathBuf,
        archives: Vec<std::path::PathBuf>,
        /// Who each trial is, from its file name, for archives written before campaigns named
        /// their trials: `rep{repetition}-load{load_mbps}`, `rep{repetition}-{arm}`.
        #[arg(long)]
        name: Option<String>,
    },
    /// Set these archives against reference archives, per metric at each point and per
    /// criterion, as `ResultService.Compare` does.
    Compare {
        #[arg(long)]
        experiment: std::path::PathBuf,
        #[arg(long, num_args = 1.., required = true)]
        archives: Vec<std::path::PathBuf>,
        #[arg(long, num_args = 1.., required = true)]
        reference: Vec<std::path::PathBuf>,
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum MeasureCmd {
    /// Start measurements: each one's reflector, then its sender.
    Start {
        /// Only this measurement (repeatable; default: every one).
        #[arg(long = "id")]
        ids: Vec<String>,
        /// How long each sender runs. Default: the experiment's `method.duration_s`, else 10.
        #[arg(long, default_value_t = 0)]
        duration_s: u32,
    },
    /// Stop measurements. Idempotent.
    Stop {
        #[arg(long = "id")]
        ids: Vec<String>,
    },
    /// Where each measurement stands.
    Status,
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
            // Unused here: `Source::Path` already resolves its own directory daemon-side
            // (services.rs::resolve_manifest); `resolved_dir` only matters for the
            // daemon's own internal inline-YAML re-staging (ExperimentSvc), never for a
            // path-based ref an external caller like this CLI sends.
            resolved_dir: String::new(),
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
    eprintln!("vxedge: `{what}` is not wired to the daemon (scaffold)");
}

/// The directory an experiment verb is addressed against. Always the process working
/// directory — `--chdir` has already moved it by the time this is called, so there is exactly
/// one notion of "here" and no second code path to keep in agreement.
/// Scaffold a `vxp/1` package and write its first lock: `experiment new` and `pkg new`.
fn scaffold_package(
    name: &str,
    id: Option<&str>,
    template: TemplateArg,
    path: &str,
    namespace: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let template: vxedge_core::Template = template.into();
    let ws = vxedge_core::WorkspacePlan::plan_with_id(name, id.unwrap_or(name), template)?
        .with_namespace(namespace)?;
    let root = ws.write(std::path::Path::new(path))?;
    let lock = vxedge_core::pkg::lock(&root, env!("CARGO_PKG_VERSION"))?;
    vxedge_core::pkg::write_lock(&root, &lock)?;
    println!("created {}", root.display());
    for file in &ws.files {
        println!("  {}", file.path.display());
    }
    println!("  {}", vxedge_core::package::LOCK_FILE);
    println!(
        "\nnext:  cd {} && $EDITOR main.yaml\n       vxedge pkg lock after editing, so the lock \
         matches\nsee:   {}/README.md",
        root.display(),
        root.display()
    );
    if template.requires_gpu() {
        // Stated at scaffold time so the requirement is known before a long build rather than
        // after one.
        eprintln!(
            "note: this template needs a Tier 2 GPU host (RTX-class, >=12 GB VRAM, tensor cores)"
        );
    }
    Ok(())
}

/// The `pkg` verbs. All local: no daemon, no network.
/// The typed intent of the experiment at `path`: a workspace folder, or a file of YAML documents
/// one of which is `kind: experiment`.
fn experiment_intent(
    path: &std::path::Path,
) -> Result<vxedge_core::intent::Intent, Box<dyn std::error::Error>> {
    use serde::Deserialize as _;
    let doc = if path.is_dir() {
        vxedge_core::Workspace::load(path)?
            .experiments_raw
            .first()
            .cloned()
    } else {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_yaml_ng::Deserializer::from_str(&text)
            .filter_map(|d| serde_yaml_ng::Value::deserialize(d).ok())
            .find(|d| d.get("kind").and_then(|k| k.as_str()) == Some("experiment"))
    }
    .ok_or_else(|| format!("{}: no `kind: experiment` document", path.display()))?;
    let (intent, problems) = vxedge_core::intent::read(&doc);
    if let Some(p) = problems.first() {
        return Err(format!("{p}").into());
    }
    Ok(intent)
}

/// The trials in `paths` (archives, or folders of them), named by `name` or by their metadata.
fn local_trials(
    paths: &[std::path::PathBuf],
    name: Option<&str>,
    metrics: &[vxedge_core::intent::Metric],
) -> Result<Vec<vxedge_core::verdict::Trial>, Box<dyn std::error::Error>> {
    let mut files = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut zips: Vec<_> = std::fs::read_dir(p)?
                .flatten()
                .map(|e| e.path())
                .filter(|f| f.extension().is_some_and(|x| x == "zip"))
                .collect();
            zips.sort();
            files.extend(zips);
        } else {
            files.push(p.clone());
        }
    }
    files
        .iter()
        .map(|f| {
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let who = match name {
                Some(pattern) => Some(
                    vxedge_core::verdict::identity_from_name(pattern, stem).ok_or_else(|| {
                        format!("{}: does not match --name `{pattern}`", f.display())
                    })?,
                ),
                None => None,
            };
            let digest = vxedge_core::pkg::sha256(&std::fs::read(f)?);
            Ok(vxedge_core::verdict::trial(f, digest, who, metrics)?)
        })
        .collect()
}

fn print_judgement(j: &vxedge_core::intent::Judgement, depth: usize) {
    let pad = "  ".repeat(depth);
    let num = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.6}"));
    let values = if j.members.is_empty() {
        let tolerance = j
            .tolerance
            .map(|t| format!(" ± {}", num(Some(t))))
            .unwrap_or_default();
        format!(" {} {} {}{tolerance}", num(j.lhs), j.rule, num(j.rhs))
    } else {
        format!(" {}", j.rule)
    };
    let why = j
        .why
        .as_deref()
        .map(|w| format!(" ({w})"))
        .unwrap_or_default();
    println!(
        "{pad}{:<20} {:<12}{values}{why}",
        j.id.as_deref().unwrap_or("-"),
        j.outcome.as_str()
    );
    for m in &j.members {
        print_judgement(m, depth + 1);
    }
    for g in &j.guards {
        println!("{pad}  guard:");
        print_judgement(g, depth + 2);
    }
}

fn results(cmd: &ResultsCmd, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ResultsCmd::Verdict {
            experiment,
            archives,
            name,
        } => {
            let intent = experiment_intent(experiment)?;
            let trials = local_trials(archives, name.as_deref(), &intent.metrics)?;
            let v = vxedge_core::verdict::verdict(&intent, trials);
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                let hypothesis = v
                    .hypothesis
                    .as_deref()
                    .map(|h| format!("{h}: "))
                    .unwrap_or_default();
                println!(
                    "{hypothesis}{} over {} trials",
                    v.outcome.as_str(),
                    v.trials.len()
                );
                for c in &v.criteria {
                    print_judgement(c, 1);
                }
            }
        }
        ResultsCmd::Compare {
            experiment,
            archives,
            reference,
            name,
        } => {
            let intent = experiment_intent(experiment)?;
            let ours = local_trials(archives, name.as_deref(), &intent.metrics)?;
            let theirs = local_trials(reference, name.as_deref(), &intent.metrics)?;
            let c = vxedge_core::verdict::compare(&intent, ours, theirs);
            if json {
                println!("{}", serde_json::to_string_pretty(&c)?);
            } else {
                println!(
                    "{} of {} within the reference's IQR; verdict {} (reference {})",
                    c.within,
                    c.metrics.len(),
                    c.outcome.as_str(),
                    c.reference_outcome.as_str()
                );
                for m in &c.metrics {
                    println!(
                        "  {:<16} {:<20} {:>12.6} vs {:>12.6} ± {:<12.6} {:+.6} {}",
                        m.metric,
                        m.point,
                        m.median,
                        m.reference_median,
                        m.reference_iqr,
                        m.delta,
                        if m.within_reference_iqr {
                            "within"
                        } else {
                            "outside"
                        }
                    );
                }
                for d in &c.criteria {
                    println!(
                        "  {:<20} {} (reference {})",
                        d.id,
                        d.outcome.as_str(),
                        d.reference_outcome.as_str()
                    );
                }
            }
        }
    }
    Ok(())
}

fn pkg(cmd: &PkgCmd, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    use vxedge_core::pkg;
    let fail = |problems: &[String], root: &std::path::Path| -> ! {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "workspace": root.display().to_string(), "valid": false, "problems": problems,
                })
            );
        } else {
            eprintln!("problem(s) in {}:", root.display());
            for p in problems {
                eprintln!("  - {p}");
            }
        }
        std::process::exit(1);
    };
    match cmd {
        PkgCmd::New {
            name,
            id,
            template,
            path,
            namespace,
        } => scaffold_package(name, id.as_deref(), *template, path, namespace)?,
        PkgCmd::Validate => {
            let root = vxedge_core::discover(&here()?)?.root;
            let mut problems = Vec::new();
            if let Err(e) = validate_workspace_dir(&root) {
                problems.push(e);
            }
            if let Err(e) = pkg::entries(&root) {
                problems.push(e.to_string());
            }
            if problems.is_empty() {
                match pkg::lock_problems(&root) {
                    Ok(p) => problems.extend(p),
                    Err(e) => problems.push(e.to_string()),
                }
            }
            if !problems.is_empty() {
                fail(&problems, &root);
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "workspace": root.display().to_string(), "valid": true })
                );
            } else {
                println!(
                    "ok: {} is a valid package and its lock matches",
                    root.display()
                );
            }
        }
        PkgCmd::Vendor { check: true, .. } => {
            let root = vxedge_core::discover(&here()?)?.root;
            let found = pkg::vendor_check(&root, &|source| resolve_source(&root, source))?;
            if !found.problems.is_empty() {
                fail(&found.problems, &root);
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "workspace": root.display().to_string(), "valid": true,
                        "unchecked": found.unchecked,
                    })
                );
            } else {
                println!("ok: every vendored copy matches what was recorded");
                for u in &found.unchecked {
                    println!("  not checked against its source: {u}");
                }
            }
        }
        PkgCmd::Vendor {
            source,
            as_id,
            replace,
            ..
        } => {
            let root = vxedge_core::discover(&here()?)?.root;
            let source = std::path::PathBuf::from(source.as_deref().expect("clap requires it"));
            let id = match as_id {
                Some(id) => id.clone(),
                None => source_id(&source).ok_or_else(|| {
                    format!(
                        "`{}` is not in a git checkout with an `origin` remote, so its source name \
                         cannot be read; pass `--as <owner>/<repo>/<path>`",
                        source.display()
                    )
                })?,
            };
            let tracked = tracked_files(&source);
            let report = pkg::vendor(
                &root,
                &source,
                &id,
                *replace,
                env!("CARGO_PKG_VERSION"),
                tracked.as_deref(),
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "workspace": root.display().to_string(),
                        "source": id,
                        "vendored": report.copied.iter().map(|v| serde_json::json!({
                            "file": v.file, "digest": v.digest,
                        })).collect::<Vec<_>>(),
                        "not_vendored": report.left_behind.iter().map(|(n, why)| serde_json::json!({
                            "name": n, "reason": why,
                        })).collect::<Vec<_>>(),
                        "relock_error": report.relock_error,
                        "local_refs": report.local_refs,
                    })
                );
            } else {
                for v in &report.copied {
                    println!("vendored {} from {id}", v.file);
                }
                for (name, why) in &report.left_behind {
                    println!("  not vendored: {name} ({why})");
                }
                match &report.relock_error {
                    Some(fix) => println!("the package no longer builds: {fix}"),
                    None => println!(
                        "wrote {}",
                        root.join(vxedge_core::package::LOCK_FILE).display()
                    ),
                }
                if !report.local_refs.is_empty() {
                    println!("the package needs these here before `up`:");
                    for r in &report.local_refs {
                        println!("  {r}");
                    }
                }
            }
        }
        PkgCmd::Sign { file, key } => {
            let bundle = vxedge_core::trust::sign_with_key(
                std::path::Path::new(file),
                std::path::Path::new(key),
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "bundle": bundle.display().to_string() })
                );
            } else {
                println!("signed {file}\nbundle {}", bundle.display());
            }
        }
        PkgCmd::Verify {
            file,
            key,
            repo,
            signer_workflow,
            cache_dir,
        } => {
            use vxedge_core::trust;
            let cache = cache_dir
                .as_ref()
                .map(std::path::PathBuf::from)
                .or_else(vxedge_core::registry::default_cache_dir)
                .ok_or("no cache directory: pass --cache-dir, or set XDG_CACHE_HOME or HOME")?;
            let file = std::path::Path::new(file);
            let record = match (key, repo) {
                (Some(k), _) => trust::verify_with_key(file, std::path::Path::new(k), &cache)?,
                (None, Some(r)) => {
                    trust::verify_github(file, r, signer_workflow.as_deref(), &cache)?
                }
                (None, None) => unreachable!("clap requires --key or --repo"),
            };
            if json {
                println!("{}", serde_json::to_string(&record)?);
            } else {
                println!(
                    "verified {}\ndigest {}\nsigner {}",
                    file.display(),
                    record.package_digest,
                    record.signer
                );
            }
        }
        PkgCmd::Push { repository, file } => {
            use vxedge_core::distribution;
            let (file, _stage) = match file {
                Some(f) => (std::path::PathBuf::from(f), None),
                None => {
                    let root = vxedge_core::discover(&here()?)?.root;
                    let problems = pkg::lock_problems(&root)?;
                    if !problems.is_empty() {
                        return Err(format!(
                            "the lock does not match the package ({}); run `vxedge pkg lock` and \
                             push again",
                            problems.join("; ")
                        )
                        .into());
                    }
                    let stage = std::env::temp_dir()
                        .join(format!("vxedge-push-src-{}", std::process::id()));
                    std::fs::create_dir_all(&stage)?;
                    let file = stage.join(pkg::packed_file_name(&root)?);
                    std::fs::write(&file, pkg::pack(&root)?)?;
                    (file, Some(TempDir(stage)))
                }
            };
            let pushed = distribution::push(&file, repository)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "reference": pushed.reference, "digest": pushed.package_digest,
                    })
                );
            } else {
                println!(
                    "pushed {}\ndigest {}\npull it with: vxedge pkg pull {repository}@{}",
                    pushed.reference, pushed.package_digest, pushed.package_digest
                );
            }
        }
        PkgCmd::Pull {
            reference,
            layout,
            into,
            cache_dir,
        } => {
            use vxedge_core::distribution;
            let cache = cache_dir
                .as_ref()
                .map(std::path::PathBuf::from)
                .or_else(vxedge_core::registry::default_cache_dir)
                .ok_or("no cache directory: pass --cache-dir, or set XDG_CACHE_HOME or HOME")?;
            if let Some(layout) = layout {
                let loaded = vxedge_core::offline::load(std::path::Path::new(layout), &cache)?;
                for (e, state) in &loaded {
                    let what = match state {
                        vxedge_core::offline::Loaded::Loaded => "loaded",
                        vxedge_core::offline::Loaded::AlreadyHere => "already here",
                    };
                    let kind = match e.kind {
                        vxedge_core::offline::Kind::Part => "part",
                        vxedge_core::offline::Kind::Image => "image",
                    };
                    if !json {
                        println!("{what}: {kind} {}@{}", e.reference, e.digest);
                    }
                }
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "layout": layout, "entries": loaded.len() })
                    );
                }
            }
            let Some(reference) = reference else {
                return Ok(());
            };
            let file = distribution::pull(reference, &cache)?;
            let (_, digest) = distribution::split_pull_ref(reference)?;
            if let Some(dir) = into {
                pkg::unpack(&std::fs::read(&file)?, std::path::Path::new(dir))?;
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "file": file.display().to_string(), "digest": digest, "into": into,
                    })
                );
            } else {
                println!("pulled {}\ndigest {digest}", file.display());
                if let Some(dir) = into {
                    println!("unpacked into {dir}");
                }
            }
        }
        PkgCmd::Lock { check } => {
            let root = vxedge_core::discover(&here()?)?.root;
            if *check {
                let problems = pkg::lock_problems(&root)?;
                if !problems.is_empty() {
                    fail(&problems, &root);
                }
                println!("ok: the lock matches");
            } else {
                let lock = pkg::lock(&root, env!("CARGO_PKG_VERSION"))?;
                pkg::write_lock(&root, &lock)?;
                println!(
                    "wrote {}: {} files, {} parts, {} images",
                    root.join(vxedge_core::package::LOCK_FILE).display(),
                    lock.files.len(),
                    lock.parts.len(),
                    lock.images.len()
                );
            }
        }
        PkgCmd::Pack { out, offline } => {
            let root = vxedge_core::discover(&here()?)?.root;
            let problems = pkg::lock_problems(&root)?;
            if !problems.is_empty() {
                return Err(format!(
                    "the lock does not match the package ({}); run `vxedge pkg lock` and pack again",
                    problems.join("; ")
                )
                .into());
            }
            let dir = match out {
                Some(d) => std::path::PathBuf::from(d),
                None => root
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .ok_or("the package has no parent folder to pack into; pass --out")?,
            };
            let inside = std::fs::canonicalize(&dir)
                .ok()
                .zip(std::fs::canonicalize(&root).ok())
                .is_some_and(|(d, r)| d.starts_with(r));
            if inside {
                return Err(format!(
                    "{} is inside the package; a packed file there would be an input to its own \
                     next pack. Pass --out with a folder outside it.",
                    dir.display()
                )
                .into());
            }
            // Refuse what cannot travel before writing anything.
            let offline_lock = if *offline {
                let lock = pkg::read_lock(&root)?.ok_or("the package has no lock")?;
                vxedge_core::offline::entries(&lock)?;
                Some(lock)
            } else {
                None
            };
            let bytes = pkg::pack(&root)?;
            let file = dir.join(pkg::packed_file_name(&root)?);
            std::fs::write(&file, &bytes)?;
            let digest = pkg::sha256(&bytes);
            let carried = if let Some(lock) = offline_lock {
                let layout = vxedge_core::offline::layout_path(&file);
                let entries = vxedge_core::offline::pack(&lock, &layout)?;
                Some((layout, entries))
            } else {
                None
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "file": file.display().to_string(), "digest": digest,
                        "layout": carried.as_ref().map(|(l, _)| l.display().to_string()),
                    })
                );
            } else {
                println!("packed {}\ndigest {digest}", file.display());
                if let Some((layout, entries)) = &carried {
                    println!(
                        "offline layout {} ({} parts and images)",
                        layout.display(),
                        entries.len()
                    );
                }
            }
        }
        PkgCmd::Unpack { file, into } => {
            let bytes = std::fs::read(file)?;
            let dest = match into {
                Some(d) => std::path::PathBuf::from(d),
                None => here()?.join(
                    std::path::Path::new(file)
                        .file_stem()
                        .ok_or("the packed file has no name")?,
                ),
            };
            pkg::unpack(&bytes, &dest)?;
            let digest = pkg::sha256(&bytes);
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "workspace": dest.display().to_string(), "digest": digest })
                );
            } else {
                println!("unpacked {}\ndigest {digest}", dest.display());
            }
        }
        PkgCmd::Inspect { file } => {
            let (descriptor, lock, digest) = match file {
                Some(f) => {
                    let bytes = std::fs::read(f)?;
                    let (d, l) = pkg::packed_descriptor_and_lock(&bytes)?;
                    (d, l, pkg::sha256(&bytes))
                }
                None => {
                    let found = vxedge_core::discover(&here()?)?;
                    let digest = pkg::package_digest(&found.root)?;
                    let d = std::fs::read_to_string(&found.descriptor_path)?;
                    let l =
                        std::fs::read_to_string(found.root.join(vxedge_core::package::LOCK_FILE))
                            .ok();
                    (d, l, digest)
                }
            };
            let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&descriptor)?;
            let field = |k: &str| {
                value
                    .get(k)
                    .and_then(|v| {
                        v.as_str().map(str::to_string).or_else(|| {
                            serde_yaml_ng::to_string(v)
                                .ok()
                                .map(|s| s.trim().to_string())
                        })
                    })
                    .unwrap_or_default()
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "id": field("id"), "version": field("version"), "digest": digest,
                        "descriptor": descriptor, "lock": lock,
                    })
                );
            } else {
                println!(
                    "package {} {}\ndigest {digest}",
                    field("id"),
                    field("version")
                );
                println!(
                    "\n--- {}\n{descriptor}",
                    vxedge_core::package::DESCRIPTOR_FILE
                );
                match lock {
                    Some(l) => println!("--- {}\n{l}", vxedge_core::package::LOCK_FILE),
                    None => println!("--- no {}", vxedge_core::package::LOCK_FILE),
                }
            }
        }
    }
    Ok(())
}

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
/// A daemon refusal as a person reads it: the daemon's sentence, then the typed reason an agent
/// branches on (`google.rpc.ErrorInfo`) and the gRPC code. `tonic::Status`'s own `Display` leads
/// with the code's generic description instead.
fn describe_status(status: &tonic::Status) -> String {
    use tonic_types::StatusExt;
    let reason = status
        .get_error_details()
        .error_info()
        .map(|info| info.reason.clone())
        .filter(|r| !r.is_empty());
    match reason {
        Some(reason) => format!(
            "{}\n  reason: {reason} ({:?})",
            status.message(),
            status.code()
        ),
        None => format!("{} ({:?})", status.message(), status.code()),
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        if let Some(status) = e.downcast_ref::<tonic::Status>() {
            eprintln!("error: {}", describe_status(status));
            std::process::exit(1);
        }
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
                for t in &status.tunnels {
                    println!(
                        "  tunnel {} ({}): state={} handshake={} public_key={} {}",
                        t.id, t.kind, t.state, t.handshake_established, t.public_key, t.detail
                    );
                }
            } else {
                println!("gateway: no status reported");
            }
        }
        Command::Up { manifest, unlocked } => {
            let channel = connect(&cli.endpoint).await?;
            let mut gw = GatewayServiceClient::new(channel.clone());
            let job = gw
                .up(GatewayUpRequest {
                    manifest: manifest_ref(manifest),
                    unlocked: *unlocked,
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
                        ..Default::default()
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
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            ZoneCmd::List => {
                let channel = connect(&cli.endpoint).await?;
                let mut zc = ZoneServiceClient::new(channel);
                let resp = zc.list(ZoneListRequest::default()).await?.into_inner();
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
                        ..Default::default()
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
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            ComputeCmd::List => {
                let channel = connect(&cli.endpoint).await?;
                let mut cc = ComputeServiceClient::new(channel);
                let resp = cc.list(ComputeListRequest::default()).await?.into_inner();
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
                        ..Default::default()
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
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            SvcCmd::Status { experiment } => {
                let workspace = resolve_experiment_arg(experiment)?;
                let channel = connect(&cli.endpoint).await?;
                let status = SvcServiceClient::new(channel)
                    .status(SvcStatusRequest {
                        service_type: "ota".to_string(),
                        experiment: manifest_ref(&Some(workspace)),
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&JsonResource::from(&status))?
                    );
                } else {
                    println!(
                        "{}: {} ({})",
                        status.vxid,
                        resource_state_name(status.state),
                        status.detail
                    );
                }
            }
        },
        Command::NosCheck { cmd } => match cmd {
            NosCheckCmd::Up { experiment } => {
                let workspace = resolve_experiment_arg(experiment)?;
                let channel = connect(&cli.endpoint).await?;
                let mut nc = NosCheckServiceClient::new(channel.clone());
                let job = nc
                    .up(NosCheckUpRequest {
                        experiment: manifest_ref(&Some(workspace)),
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            NosCheckCmd::Down { experiment } => {
                let workspace = resolve_experiment_arg(experiment)?;
                let channel = connect(&cli.endpoint).await?;
                let mut nc = NosCheckServiceClient::new(channel.clone());
                let job = nc
                    .down(NosCheckDownRequest {
                        experiment: manifest_ref(&Some(workspace)),
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
            NosCheckCmd::Status { experiment } => {
                let workspace = resolve_experiment_arg(experiment)?;
                let channel = connect(&cli.endpoint).await?;
                let mut nc = NosCheckServiceClient::new(channel);
                let resp = nc
                    .status(NosCheckStatusRequest {
                        experiment: manifest_ref(&Some(workspace)),
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                if let Some(s) = &resp.status {
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "vxid": s.vxid, "state": s.state, "detail": s.detail
                            }))?
                        );
                    } else {
                        println!("nos_check {}: state={} {}", s.vxid, s.state, s.detail);
                    }
                } else {
                    println!("nos_check: no status reported");
                }
            }
            NosCheckCmd::Verify { experiment } => {
                let workspace = resolve_experiment_arg(experiment)?;
                let channel = connect(&cli.endpoint).await?;
                let mut nc = NosCheckServiceClient::new(channel.clone());
                let job = nc
                    .verify(NosCheckVerifyRequest {
                        experiment: manifest_ref(&Some(workspace)),
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
        },
        Command::Telemetry { cmd } => {
            let channel = connect(&cli.endpoint).await?;
            match cmd {
                TelemetryCmd::Up { experiment } | TelemetryCmd::Down { experiment } => {
                    let mut svc = SvcServiceClient::new(channel.clone());
                    let workspace = manifest_ref(&Some(experiment.clone()));
                    let job = if matches!(cmd, TelemetryCmd::Up { .. }) {
                        svc.up(SvcUpRequest {
                            service_type: "telemetry".to_string(),
                            experiment: workspace,
                            ..Default::default()
                        })
                        .await?
                    } else {
                        svc.down(SvcDownRequest {
                            service_type: "telemetry".to_string(),
                            experiment: workspace,
                            ..Default::default()
                        })
                        .await?
                    }
                    .into_inner();
                    watch_job(channel, job, cli.json).await?;
                }
                TelemetryCmd::Collect { manifest, out } => {
                    let job =
                        vxedge_api::v1::telemetry_service_client::TelemetryServiceClient::new(
                            channel.clone(),
                        )
                        .collect(vxedge_api::v1::CollectRequest {
                            manifest: manifest_ref(&Some(manifest.clone())),
                            out_path: out.clone(),
                            ..Default::default()
                        })
                        .await?
                        .into_inner();
                    watch_job(channel, job, cli.json).await?;
                }
            }
        }
        // Unlike `experiment` verbs, kept `--manifest`-addressed rather than cwd-discovered:
        // that Terraform-style decision (`docs/ROADMAP.md`'s 2026-07-30 workspace-model note) is
        // scoped to experiment verbs specifically, and root `CLAUDE.md`'s own CLI command table
        // still shows `vxedge vehicle up|down|status --manifest <vehicle.yaml>`.
        Command::Vehicle { cmd } => {
            let channel = connect(&cli.endpoint).await?;
            let mut veh = VehicleServiceClient::new(channel.clone());
            match cmd {
                ManifestLifecycleCmd::Status { manifest } => {
                    let st = veh
                        .status(VehicleStatusRequest {
                            manifest: manifest_ref(&Some(manifest.clone())),
                        })
                        .await?
                        .into_inner();
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&JsonComposite::from(&st))?
                        );
                    } else {
                        print_vehicle_status(&st);
                    }
                }
                ManifestLifecycleCmd::Up {
                    manifest,
                    stages,
                    unlocked,
                } => {
                    let job = veh
                        .up(VehicleUpRequest {
                            manifest: manifest_ref(&Some(manifest.clone())),
                            stages: stages.iter().map(|s| s.as_proto()).collect(),
                            unlocked: *unlocked,
                        })
                        .await?
                        .into_inner();
                    watch_job(channel, job, cli.json).await?;
                }
                ManifestLifecycleCmd::Down { manifest, stages } => {
                    let job = veh
                        .down(VehicleDownRequest {
                            manifest: manifest_ref(&Some(manifest.clone())),
                            stages: stages.iter().map(|s| s.as_proto()).collect(),
                        })
                        .await?
                        .into_inner();
                    watch_job(channel, job, cli.json).await?;
                }
            }
        }
        Command::Experiment { cmd } => match cmd {
            ExperimentCmd::New {
                name,
                id,
                template,
                path,
                namespace,
            } => scaffold_package(name, id.as_deref(), *template, path, namespace)?,
            ExperimentCmd::Validate => {
                // Every cross-file join, *then* every model invariant — found live 2026-08-04:
                // this used to stop at `ws.validate()` (cross-file joins only) and never called
                // `model::build()`, so a manifest with an unknown `network.data_plane`, an
                // unrecognized tunnel/nos_check kind, or any other model-level error reported
                // "ok" here and only failed later — at `up`, or worse, inside `vxedged`'s own
                // model build during provisioning. `vxedge manifest validate <path>` already did
                // this correctly; this command just wasn't calling the same function.
                let here = here()?;
                let found = vxedge_core::discover(&here)?;
                match validate_workspace_dir(&found.root) {
                    Ok((summary, human)) => {
                        if cli.json {
                            let mut obj = serde_json::json!({
                                "workspace": found.root.display().to_string(), "valid": true,
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
                            println!("ok: {human}");
                        }
                    }
                    Err(e) => {
                        if cli.json {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "workspace": found.root.display().to_string(),
                                    "valid": false, "error": e,
                                })
                            );
                        } else {
                            // `validate_workspace_dir` already joins every cross-file problem
                            // it finds into one "  - "-prefixed list (an engineer fixing a
                            // workspace wants the whole list, not a fix-one-rerun loop); a
                            // model-level error is a single, specific line. Either way, `e` is
                            // already the complete message.
                            eprintln!("problem(s) in {}:\n{e}", found.root.display());
                        }
                        std::process::exit(1);
                    }
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
                        if cli.json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&JsonComposite::from(&st))?
                            );
                        } else {
                            let top = st.status.clone().unwrap_or_default();
                            println!(
                                "experiment {}: {} ({})",
                                top.vxid,
                                resource_state_name(top.state),
                                top.detail
                            );
                            for c in &st.components {
                                println!(
                                    "  {:<12} {:<16} {:<16} {}",
                                    kind_name(c.kind),
                                    c.vxid,
                                    resource_state_name(c.state),
                                    c.detail
                                );
                            }
                        }
                    }
                    ExperimentCmd::Up { stages, unlocked } => {
                        let job = exp
                            .up(ExperimentUpRequest {
                                manifest: manifest_ref(&Some(workspace)),
                                stages: stages.iter().map(|s| s.as_proto()).collect(),
                                unlocked: *unlocked,
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
                        ..Default::default()
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
                        ..Default::default()
                    })
                    .await?
                    .into_inner();
                watch_job(channel, job, cli.json).await?;
            }
        },
        Command::Campaign { cmd } => {
            use vxedge_api::v1::campaign_service_client::CampaignServiceClient;
            use vxedge_api::v1::{
                CampaignGetRequest, CampaignListRequest, CampaignPlanRequest, CampaignStartRequest,
            };
            let channel = connect(&cli.endpoint).await?;
            let mut campaigns = CampaignServiceClient::new(channel.clone());
            let here_ref = || -> Result<_, Box<dyn std::error::Error>> {
                let found = vxedge_core::discover(&here()?)?;
                Ok(manifest_ref(&Some(
                    found.root.to_string_lossy().to_string(),
                )))
            };
            match cmd {
                CampaignCmd::Plan { profile } => {
                    let plan = campaigns
                        .plan(CampaignPlanRequest {
                            manifest: here_ref()?,
                            profile: profile.clone(),
                        })
                        .await?
                        .into_inner();
                    for t in &plan.trials {
                        println!(
                            "{:<24} {:>6.0}s warmup {:>6.0}s",
                            t.id, t.duration_s, t.warmup_s
                        );
                    }
                    println!(
                        "{} trials ({} points x {} repetitions), about {:.0} s",
                        plan.trials.len(),
                        plan.points,
                        plan.repetitions,
                        plan.estimated_s
                    );
                    for p in &plan.problems {
                        println!("problem: {}", p.detail);
                    }
                    if !plan.problems.is_empty() {
                        std::process::exit(1);
                    }
                }
                CampaignCmd::Start {
                    profile,
                    request_id,
                    unlocked,
                } => {
                    let job = campaigns
                        .start(CampaignStartRequest {
                            manifest: here_ref()?,
                            profile: profile.clone(),
                            request_id: request_id.clone(),
                            caller: "vxedge-cli".into(),
                            unlocked: *unlocked,
                            ..Default::default()
                        })
                        .await?
                        .into_inner();
                    watch_job(channel, job, cli.json).await?;
                }
                CampaignCmd::Status { campaign_id } => {
                    let list = match campaign_id {
                        Some(id) => vec![
                            campaigns
                                .get(CampaignGetRequest {
                                    campaign_id: id.clone(),
                                })
                                .await?
                                .into_inner(),
                        ],
                        None => {
                            campaigns
                                .list(CampaignListRequest {})
                                .await?
                                .into_inner()
                                .campaigns
                        }
                    };
                    if cli.json {
                        let out: Vec<serde_json::Value> = list
                            .iter()
                            .map(|c| {
                                serde_json::json!({
                                    "campaign_id": c.campaign_id, "experiment_id": c.experiment_id,
                                    "state": c.state, "results_dir": c.results_dir, "job_id": c.job_id,
                                    "trials": c.trials.iter().map(|t| serde_json::json!({
                                        "id": t.id, "arm": t.arm, "state": t.state, "phase": t.phase,
                                        "archive": t.archive, "sha256": t.sha256, "error": t.error,
                                    })).collect::<Vec<_>>(),
                                    "verdict": outcome_name(c.verdict),
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&out)?);
                    } else {
                        for c in &list {
                            let state = vxedge_api::v1::CampaignState::try_from(c.state)
                                .map(|s| {
                                    s.as_str_name()
                                        .trim_start_matches("CAMPAIGN_STATE_")
                                        .to_lowercase()
                                })
                                .unwrap_or_default();
                            let verdict = vxedge_api::v1::VerdictOutcome::try_from(c.verdict)
                                .ok()
                                .filter(|v| *v != vxedge_api::v1::VerdictOutcome::Unspecified)
                                .map(|v| format!(" verdict {}", outcome_name(v as i32)))
                                .unwrap_or_default();
                            println!(
                                "{} {} {}{verdict} ({})",
                                c.campaign_id, c.experiment_id, state, c.results_dir
                            );
                            for t in &c.trials {
                                let ts = vxedge_api::v1::TrialState::try_from(t.state)
                                    .map(|s| {
                                        s.as_str_name()
                                            .trim_start_matches("TRIAL_STATE_")
                                            .to_lowercase()
                                    })
                                    .unwrap_or_default();
                                println!(
                                    "  {:<24} {:<10} {:<10} {}",
                                    t.id,
                                    ts,
                                    t.phase,
                                    if t.error.is_empty() {
                                        &t.archive
                                    } else {
                                        &t.error
                                    }
                                );
                            }
                        }
                    }
                }
                CampaignCmd::Compare {
                    campaign_id,
                    reference_campaign_id,
                    reference_workspace,
                } => {
                    use vxedge_api::v1::ResultCompareRequest;
                    use vxedge_api::v1::result_service_client::ResultServiceClient;
                    let c = ResultServiceClient::new(channel.clone())
                        .compare(ResultCompareRequest {
                            campaign_id: campaign_id.clone(),
                            reference_campaign_id: reference_campaign_id.clone(),
                            reference_workspace: manifest_ref(reference_workspace),
                        })
                        .await?
                        .into_inner();
                    if cli.json {
                        let out = serde_json::json!({
                            "campaign_id": c.campaign_id,
                            "reference_campaign_id": c.reference_campaign_id,
                            "outcome": outcome_name(c.outcome),
                            "reference_outcome": outcome_name(c.reference_outcome),
                            "within": c.within,
                            "metrics": c.metrics.iter().map(|m| serde_json::json!({
                                "metric": m.metric, "point": m.point, "count": m.count,
                                "reference_count": m.reference_count, "median": m.median,
                                "iqr": m.iqr, "reference_median": m.reference_median,
                                "reference_iqr": m.reference_iqr, "delta": m.delta,
                                "within_reference_iqr": m.within_reference_iqr,
                            })).collect::<Vec<_>>(),
                            "criteria": c.criteria.iter().map(|d| serde_json::json!({
                                "id": d.id, "outcome": outcome_name(d.outcome),
                                "reference_outcome": outcome_name(d.reference_outcome),
                            })).collect::<Vec<_>>(),
                        });
                        println!("{}", serde_json::to_string_pretty(&out)?);
                    } else {
                        println!(
                            "{} vs {}: {} of {} within the reference's IQR; verdict {} (reference {})",
                            c.campaign_id,
                            c.reference_campaign_id,
                            c.within,
                            c.metrics.len(),
                            outcome_name(c.outcome),
                            outcome_name(c.reference_outcome)
                        );
                        for m in &c.metrics {
                            println!(
                                "  {:<16} {:<20} {:>12.6} vs {:>12.6} ± {:<12.6} {:+.6} {}",
                                m.metric,
                                m.point,
                                m.median,
                                m.reference_median,
                                m.reference_iqr,
                                m.delta,
                                if m.within_reference_iqr {
                                    "within"
                                } else {
                                    "outside"
                                }
                            );
                        }
                        for d in &c.criteria {
                            println!(
                                "  {:<20} {} (reference {})",
                                d.id,
                                outcome_name(d.outcome),
                                outcome_name(d.reference_outcome)
                            );
                        }
                    }
                }
                CampaignCmd::Verdict { campaign_id } => {
                    use vxedge_api::v1::ResultVerdictRequest;
                    use vxedge_api::v1::result_service_client::ResultServiceClient;
                    let v = ResultServiceClient::new(channel.clone())
                        .verdict(ResultVerdictRequest {
                            campaign_id: campaign_id.clone(),
                        })
                        .await?
                        .into_inner();
                    if cli.json {
                        let out = serde_json::json!({
                            "campaign_id": v.campaign_id, "experiment_id": v.experiment_id,
                            "hypothesis": v.hypothesis, "outcome": outcome_name(v.outcome),
                            "criteria": v.criteria.iter().map(criterion_json).collect::<Vec<_>>(),
                            "trials": v.trials.iter().map(|t| serde_json::json!({
                                "trial_id": t.trial_id, "arm": t.arm, "repetition": t.repetition,
                                "archive_digest": t.archive_digest,
                                "values": t.values.iter().map(|(k, m)| (k.clone(), serde_json::json!({
                                    "value": m.value, "complete": m.complete, "why": m.why,
                                }))).collect::<serde_json::Map<_, _>>(),
                            })).collect::<Vec<_>>(),
                        });
                        println!("{}", serde_json::to_string_pretty(&out)?);
                    } else {
                        let hypothesis = if v.hypothesis.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", v.hypothesis)
                        };
                        println!(
                            "{} {}{hypothesis}: {} over {} trials",
                            v.campaign_id,
                            v.experiment_id,
                            outcome_name(v.outcome),
                            v.trials.len()
                        );
                        for c in &v.criteria {
                            print_criterion(c, 1);
                        }
                    }
                }
            }
        }
        Command::Measure { cmd } => {
            use vxedge_api::v1::{
                MeasurementStartRequest, MeasurementState, MeasurementStatusRequest,
                MeasurementStopRequest,
            };
            let found = vxedge_core::discover(&here()?)?;
            let manifest = manifest_ref(&Some(found.root.to_string_lossy().to_string()));
            let channel = connect(&cli.endpoint).await?;
            let mut measure = MeasurementServiceClient::new(channel.clone());
            match cmd {
                MeasureCmd::Start { ids, duration_s } => {
                    let job = measure
                        .start(MeasurementStartRequest {
                            manifest,
                            ids: ids.clone(),
                            duration_s: *duration_s,
                            ..Default::default()
                        })
                        .await?
                        .into_inner();
                    watch_job(channel, job, cli.json).await?;
                }
                MeasureCmd::Stop { ids } => {
                    let job = measure
                        .stop(MeasurementStopRequest {
                            manifest,
                            ids: ids.clone(),
                            ..Default::default()
                        })
                        .await?
                        .into_inner();
                    watch_job(channel, job, cli.json).await?;
                }
                MeasureCmd::Status => {
                    let st = measure
                        .status(MeasurementStatusRequest {
                            manifest,
                            ..Default::default()
                        })
                        .await?
                        .into_inner();
                    for m in &st.measurements {
                        let state = match MeasurementState::try_from(m.state) {
                            Ok(MeasurementState::Running) => "running",
                            Ok(MeasurementState::Finished) => "finished",
                            Ok(MeasurementState::Failed) => "failed",
                            _ => "not running",
                        };
                        if cli.json {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "id": m.id, "entity": m.entity, "kind": m.kind,
                                    "state": state, "detail": m.detail,
                                })
                            );
                        } else {
                            println!("{:<16} {:<10} {:<12} {}", m.id, m.kind, state, m.detail);
                        }
                    }
                }
            }
        }
        Command::Pkg { cmd } => pkg(cmd, cli.json)?,
        Command::Results { cmd } => results(cmd, cli.json)?,
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
                    ..Default::default()
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

/// A §6.0 workspace directory: every cross-file join, then every model invariant. A bare
/// vehicle (no `kind: experiment`) is validated the way `vehicle up` loads it.
/// `RESOURCE_STATE_UP` as `up`, and `not provisioned` for a device nothing brings up.
fn resource_state_name(state: i32) -> String {
    match vxedge_api::v1::ResourceState::try_from(state) {
        Ok(vxedge_api::v1::ResourceState::Unspecified)
        | Ok(vxedge_api::v1::ResourceState::Unprovisioned)
        | Err(_) => "not provisioned".to_string(),
        Ok(s) => s
            .as_str_name()
            .trim_start_matches("RESOURCE_STATE_")
            .to_lowercase(),
    }
}

fn kind_name(kind: i32) -> String {
    vxedge_api::v1::ResourceKind::try_from(kind)
        .map(|k| {
            k.as_str_name()
                .trim_start_matches("RESOURCE_KIND_")
                .to_lowercase()
        })
        .unwrap_or_else(|_| "unknown".to_string())
}

fn print_vehicle_status(st: &vxedge_api::v1::VehicleStatus) {
    let top = st.status.clone().unwrap_or_default();
    println!(
        "vehicle {}: {} ({})",
        top.vxid,
        resource_state_name(top.state),
        top.detail
    );
    for c in &st.components {
        println!(
            "  {:<12} {:<16} {:<16} {}",
            kind_name(c.kind),
            c.vxid,
            resource_state_name(c.state),
            c.detail
        );
    }
    let Some(gw) = &st.gateway else { return };
    for s in &gw.slices {
        println!(
            "  slice        {:<16} {:<16} {}",
            s.slice_id,
            resource_state_name(s.state),
            s.detail
        );
    }
    for f in &gw.ingress_filters {
        println!(
            "  filter       {:<16} {:<16} {}",
            f.access_port,
            resource_state_name(f.state),
            f.detail
        );
    }
    for i in &gw.impairments {
        println!(
            "  impaired     {:<16} {:<16} {}",
            i.link,
            resource_state_name(i.state),
            i.detail
        );
    }
}

fn validate_workspace_dir(root: &std::path::Path) -> Result<Summary, String> {
    let ws = vxedge_core::Workspace::load(root).map_err(|e| e.to_string())?;
    if ws.is_bare_vehicle() {
        let yaml = vxedge_core::Workspace::read_text(root).map_err(|e| e.to_string())?;
        return validate_bare_vehicle(&yaml);
    }
    finish_workspace(ws, || vxedge_core::model::load_dir(root))
}

/// The same checks over a workspace held in one file of `---`-separated documents.
fn validate_workspace_yaml(yaml: &str) -> Result<Summary, String> {
    let ws = vxedge_core::Workspace::from_yaml(yaml).map_err(|e| e.to_string())?;
    if ws.is_bare_vehicle() {
        return validate_bare_vehicle(yaml);
    }
    finish_workspace(ws, || vxedge_core::model::load_yaml(yaml))
}

/// `vehicle up` wraps a bare vehicle in a synthesized experiment before anything else sees it
/// (`synthesize_vehicle_workspace`); validating the same synthesized text keeps "validate
/// passes" and "up accepts" the same statement.
fn validate_bare_vehicle(yaml: &str) -> Result<Summary, String> {
    let synthesized =
        vxedge_core::model::synthesize_vehicle_workspace(yaml).map_err(|e| e.to_string())?;
    let ws = vxedge_core::Workspace::from_yaml(&synthesized).map_err(|e| e.to_string())?;
    let (json, human) = finish_workspace(ws, || vxedge_core::model::load_yaml(&synthesized))?;
    Ok((
        json,
        format!("bare vehicle workspace (experiment synthesized as for `vehicle up`): {human}"),
    ))
}

/// Validating is not "does it parse": a workspace that passes its cross-file joins can still
/// violate a model invariant, and `up` would be the one to find out. Both run here, so the
/// local check is the same check provisioning runs.
fn finish_workspace(
    ws: vxedge_core::Workspace,
    model: impl FnOnce() -> Result<vxedge_core::model::Experiment, vxedge_core::model::LoadError>,
) -> Result<Summary, String> {
    let errors: Vec<String> = ws
        .validate()
        .iter()
        .map(ToString::to_string)
        .chain(ws.intent_problems().iter().map(ToString::to_string))
        .collect();
    if !errors.is_empty() {
        return Err(errors
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let model = model().map_err(|e| e.to_string())?;
    let measured = vxedge_core::measurements::problems(&model);
    if !measured.is_empty() {
        return Err(measured
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n"));
    }
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
struct JsonTunnelStatus {
    id: String,
    state: i32,
    public_key: String,
    handshake_established: bool,
    detail: String,
    kind: i32,
}

/// One resource, with its kind and state as words, like `vehicle status` prints them.
#[derive(serde::Serialize)]
struct JsonResource {
    vxid: String,
    kind: String,
    state: String,
    detail: String,
}

impl From<&vxedge_api::v1::ResourceStatus> for JsonResource {
    fn from(s: &vxedge_api::v1::ResourceStatus) -> Self {
        Self {
            vxid: s.vxid.clone(),
            kind: kind_name(s.kind),
            state: resource_state_name(s.state),
            detail: s.detail.clone(),
        }
    }
}

#[derive(serde::Serialize)]
struct JsonComposite {
    status: JsonResource,
    components: Vec<JsonResource>,
    gateway: Option<JsonStatus>,
}

impl From<&vxedge_api::v1::VehicleStatus> for JsonComposite {
    fn from(v: &vxedge_api::v1::VehicleStatus) -> Self {
        Self {
            status: JsonResource::from(&v.status.clone().unwrap_or_default()),
            components: v.components.iter().map(JsonResource::from).collect(),
            gateway: v.gateway.as_ref().map(JsonStatus::from),
        }
    }
}

impl From<&vxedge_api::v1::ExperimentStatus> for JsonComposite {
    fn from(e: &vxedge_api::v1::ExperimentStatus) -> Self {
        Self {
            status: JsonResource::from(&e.status.clone().unwrap_or_default()),
            components: e.components.iter().map(JsonResource::from).collect(),
            gateway: e.gateway.as_ref().map(JsonStatus::from),
        }
    }
}

#[derive(serde::Serialize)]
struct JsonStatus {
    vxid: String,
    state: i32,
    detail: String,
    slices: Vec<JsonSliceStatus>,
    impairments: Vec<JsonImpairmentStatus>,
    ingress_filters: Vec<JsonIngressFilterStatus>,
    tunnels: Vec<JsonTunnelStatus>,
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
            tunnels: g
                .tunnels
                .iter()
                .map(|t| JsonTunnelStatus {
                    id: t.id.clone(),
                    state: t.state,
                    public_key: t.public_key.clone(),
                    handshake_established: t.handshake_established,
                    detail: t.detail.clone(),
                    kind: t.kind,
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

/// `pass`, `fail`, `inconclusive`, or empty for none.
fn outcome_name(o: i32) -> &'static str {
    match vxedge_api::v1::VerdictOutcome::try_from(o) {
        Ok(vxedge_api::v1::VerdictOutcome::Pass) => "pass",
        Ok(vxedge_api::v1::VerdictOutcome::Fail) => "fail",
        Ok(vxedge_api::v1::VerdictOutcome::Inconclusive) => "inconclusive",
        _ => "",
    }
}

fn criterion_json(c: &vxedge_api::v1::CriterionVerdict) -> serde_json::Value {
    serde_json::json!({
        "id": c.id, "outcome": outcome_name(c.outcome), "rule": c.rule,
        "lhs": c.lhs, "rhs": c.rhs, "tolerance": c.tolerance, "why": c.why,
        "reads": c.reads.iter().map(|r| serde_json::json!({
            "metric": r.metric, "aggregate": r.aggregate, "trials": r.trials,
            "values": r.values, "complete": r.complete, "value": r.value,
        })).collect::<Vec<_>>(),
        "members": c.members.iter().map(criterion_json).collect::<Vec<_>>(),
        "guards": c.guards.iter().map(criterion_json).collect::<Vec<_>>(),
    })
}

fn print_criterion(c: &vxedge_api::v1::CriterionVerdict, depth: usize) {
    let pad = "  ".repeat(depth);
    let num = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.6}"));
    let values = if c.members.is_empty() {
        let tolerance = c
            .tolerance
            .map(|t| format!(" ± {}", num(Some(t))))
            .unwrap_or_default();
        format!(" {} {} {}{tolerance}", num(c.lhs), c.rule, num(c.rhs))
    } else {
        format!(" {}", c.rule)
    };
    let why = if c.why.is_empty() {
        String::new()
    } else {
        format!(" ({})", c.why)
    };
    println!(
        "{pad}{:<20} {:<12}{values}{why}",
        if c.id.is_empty() { "-" } else { &c.id },
        outcome_name(c.outcome)
    );
    for m in &c.members {
        print_criterion(m, depth + 1);
    }
    for g in &c.guards {
        println!("{pad}  guard:");
        print_criterion(g, depth + 2);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_refusal_prints_the_daemons_sentence_and_its_reason() {
        use tonic_types::{ErrorDetails, StatusExt};
        let status = tonic::Status::with_error_details(
            tonic::Code::FailedPrecondition,
            "the package does not match its `vxp.lock`: `main.yaml` changed.",
            ErrorDetails::with_error_info("ERROR_REASON_LOCK_MISMATCH", "vxedge.v1", []),
        );
        assert_eq!(
            describe_status(&status),
            "the package does not match its `vxp.lock`: `main.yaml` changed.\n  reason: \
             ERROR_REASON_LOCK_MISMATCH (FailedPrecondition)"
        );
        let plain = tonic::Status::unavailable("vxedged is not answering");
        assert_eq!(
            describe_status(&plain),
            "vxedged is not answering (Unavailable)"
        );
    }

    #[test]
    fn a_remote_url_names_its_owner_and_repo() {
        for url in [
            "https://github.com/vxedge/vxperiments.git",
            "https://github.com/vxedge/vxperiments",
            "ssh://git@github.com/vxedge/vxperiments.git",
            "git@github.com:vxedge/vxperiments.git",
            "/srv/git/vxedge/vxperiments/",
        ] {
            assert_eq!(
                owner_repo(url).as_deref(),
                Some("vxedge/vxperiments"),
                "{url}"
            );
        }
        assert_eq!(owner_repo("https://github.com/"), None);
    }

    use super::*;
    use vxedge_api::v1::{ResourceKind, ResourceState, ResourceStatus, VehicleStatus};

    #[test]
    fn vehicle_status_json_keeps_every_part_of_the_reply() {
        let status = VehicleStatus {
            status: Some(ResourceStatus {
                vxid: "car".into(),
                kind: ResourceKind::Vehicle as i32,
                state: ResourceState::Up as i32,
                ..Default::default()
            }),
            components: vec![ResourceStatus {
                vxid: "ecu2".into(),
                kind: ResourceKind::Zone as i32,
                state: ResourceState::Unprovisioned as i32,
                detail: "declared, not provisioned: no slice-bound link".into(),
                ..Default::default()
            }],
            gateway: Some(Default::default()),
            ..Default::default()
        };
        let json = serde_json::to_value(JsonComposite::from(&status)).unwrap();
        assert_eq!(json["status"]["vxid"], "car");
        assert_eq!(json["status"]["state"], "up");
        assert_eq!(json["components"][0]["kind"], "zone");
        assert_eq!(json["components"][0]["state"], "not provisioned");
        assert!(json["gateway"]["slices"].is_array());
    }
}
