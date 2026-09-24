//! vxAPI service implementations. GatewayService, ZoneService and ComputeService are live
//! end-to-end: gateway Up provisions bridge/VLAN/HTB, zone/compute Up run nodes under their
//! packaging backend (container / qemu-microvm / libvirt) — the same lifecycle for both,
//! addressed by role rather than by a difference in provisioning (see `ComputeSvc`'s own doc).

use sha2::Digest;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tonic::{Request, Response, Status};

use vxedge_api::v1::compute_service_server::ComputeService;
use vxedge_api::v1::event_service_server::EventService;
use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::job_service_server::JobService;
use vxedge_api::v1::manifest_ref::Source;
use vxedge_api::v1::nos_check_service_server::NosCheckService;
use vxedge_api::v1::svc_service_server::SvcService;
use vxedge_api::v1::traffic_service_server::TrafficService;
use vxedge_api::v1::zone_service_server::ZoneService;
use vxedge_api::v1::{
    ComputeApplyRequest, ComputeDownRequest, ComputeListRequest, ComputeListResponse,
    ComputeRemoveRequest, ComputeUpRequest, Event, GatewayApplyRequest, GatewayDownRequest,
    GatewayStatus, GatewayStatusRequest, GatewayUpRequest, GetJobRequest, ImagePushRequest,
    ImagePushResponse, ImpairmentStatus, IngressFilterStatus, Job, ManifestRef,
    NosCheckDownRequest, NosCheckStatus, NosCheckStatusRequest, NosCheckUpRequest,
    NosCheckVerifyRequest, ResourceKind, ResourceState, ResourceStatus, SliceStatus,
    SubscribeRequest, SvcDownRequest, SvcStatusRequest, SvcUpRequest, TrafficDownRequest,
    TrafficUpRequest, TunnelStatus, WatchJobRequest, ZoneApplyRequest, ZoneDownRequest,
    ZoneListRequest, ZoneListResponse, ZoneRemoveRequest, ZoneUpRequest,
};
use vxedge_core::Workspace;
use vxedge_core::fiveg::{self, FiveGLifecycleConfig, FiveGPlan};
use vxedge_core::gateway::{self, GatewayPlan, Shell};
use vxedge_core::traffic::{self, TrafficPlan};
use vxedge_core::zones::{self, NodePlan};

use crate::audit::Audit;
use crate::events::EventBus;
use crate::jobs::{JobEngine, is_terminal, now_ms};

/// The experiment a successful `gateway up` registered: the built model, where it came from,
/// and the plans derived from it. Consumed by zone verbs, cleared by `gateway down`.
///
/// The **model** is held, not the YAML it was built from. It used to be the document, so every
/// zone verb re-parsed it — the same workspace built, validated and invariant-checked once per
/// RPC, with a `Status::internal("stored manifest no longer models")` arm for a failure that
/// could only mean the daemon had corrupted its own state. Holding the value makes the active
/// experiment a thing the daemon *knows* rather than a string it re-derives, and it is the
/// what let the experiment's `services`, `telemetry` and `results` blocks become typed fields.
pub struct ActiveExperiment {
    pub model: vxedge_core::model::Experiment,
    /// The workspace this was built from. Bundles and relative part refs resolve against it, so
    /// it travels with the model rather than being recomputed per verb.
    pub dir: PathBuf,
    /// Checksum of the source documents, for audit records and results provenance.
    pub sha256: String,
}

/// Daemon-wide view of the active experiment: set by a successful
/// `gateway up`, consumed by zone verbs, cleared by `gateway down`.
#[derive(Default)]
pub struct EdgeState {
    pub gateway: Option<GatewayPlan>,
    pub active: Option<ActiveExperiment>,
    /// Set alongside `gateway` whenever the experiment's `network.slices`
    /// include any `mode: s-nssai` — the fiveg CN/RAN pods' desired state,
    /// for `down`/`status` to act on without re-planning.
    pub fiveg: Option<FiveGPlan>,
}

pub type SharedState = Arc<tokio::sync::Mutex<EdgeState>>;

/// Resolve a ManifestRef to (yaml, sha256, source dir), enforcing the
/// checksum guard when the caller supplied one.
/// Just the YAML, for callers that resolve once and pass the document on (`ExperimentSvc`).
pub(crate) async fn resolve_manifest_yaml(mref: &ManifestRef) -> Result<String, Status> {
    resolve_manifest(Some(mref)).await.map(|(yaml, _, _)| yaml)
}

/// The workspace directory a `ManifestRef` names, if it names one. An inline manifest has no
/// directory and therefore no `.vxp`.
pub(crate) async fn resolve_manifest_dir(mref: &ManifestRef) -> Option<PathBuf> {
    resolve_manifest(Some(mref))
        .await
        .ok()
        .and_then(|(_, _, d)| d)
}

/// Refuse a workspace this host cannot run, **before** anything is provisioned.
///
/// Daemon-side on purpose: the host that matters is the one doing the provisioning, and a
/// CLI-side check asks the wrong machine the moment `--endpoint` points elsewhere. An inline
/// manifest carries no `.vxp`, so there is nothing to check and nothing is claimed.
pub(crate) fn preflight_requires(dir: Option<&std::path::Path>) -> Result<(), Status> {
    let Some(dir) = dir else { return Ok(()) };
    let Ok(found) = vxedge_core::discover(dir) else {
        // No `.vxp` is not an error here: `up` accepts a bare directory of documents, and
        // `experiment` verbs already report a missing descriptor with their own message.
        return Ok(());
    };
    found
        .package
        .requires
        .check(
            &found.package.id,
            &vxedge_core::package::HostCapabilities::probe(),
        )
        .map_err(|e| Status::failed_precondition(e.to_string()))
}

async fn resolve_manifest(
    mref: Option<&ManifestRef>,
) -> Result<(String, String, Option<PathBuf>), Status> {
    let mref = mref.ok_or_else(|| {
        Status::invalid_argument("a manifest is required (pass --manifest <path> on the CLI)")
    })?;
    let (yaml, dir) = match &mref.source {
        Some(Source::Path(path)) if PathBuf::from(path).is_dir() => {
            (read_workspace(path).await?, Some(PathBuf::from(path)))
        }
        Some(Source::Path(path)) => {
            let yaml = tokio::fs::read_to_string(path).await.map_err(|e| {
                Status::invalid_argument(format!(
                    "cannot read manifest `{path}` on the daemon host: {e}"
                ))
            })?;
            (yaml, PathBuf::from(path).parent().map(PathBuf::from))
        }
        Some(Source::InlineYaml(yaml)) => {
            // See ManifestRef.resolved_dir's own doc comment (common.proto): set only by
            // ExperimentService's own stage orchestration, carrying the workspace directory
            // the inline document was actually read from. Without it, a local `kind: bundle`
            // ref or baseline part resolves against the daemon process's own cwd instead —
            // found live 2026-08-04, `zone up` after `experiment up --stage gateway`.
            let dir = (!mref.resolved_dir.is_empty()).then(|| PathBuf::from(&mref.resolved_dir));
            (yaml.clone(), dir)
        }
        None => {
            return Err(Status::invalid_argument(
                "manifest has no source (path or inline_yaml)",
            ));
        }
    };
    // The workspace identity rule (`ARCHITECTURE.md` §13), not a hash of the concatenated
    // documents: a sandbox's local parts, assets and scripts are part of what ran, and an
    // identity that ignored them would call two different experiments the same. An inline
    // manifest has no directory, so it falls back to hashing what it was actually given.
    let sha256 = match &dir {
        Some(root) => Workspace::content_sha256(root).map_err(|e| {
            Status::invalid_argument(format!("workspace `{}`: {e}", root.display()))
        })?,
        None => hex::encode(sha2::Sha256::digest(yaml.as_bytes())),
    };
    if !mref.sha256.is_empty() && mref.sha256 != sha256 {
        return Err(Status::failed_precondition(format!(
            "manifest checksum mismatch: expected {}, got {sha256}",
            mref.sha256
        )));
    }
    Ok((yaml, sha256, dir))
}

/// A `ManifestRef.path` pointing at a **directory** is a §6.0 workspace: read its documents
/// into one `---`-separated string.
///
/// The daemon used to *lower* a workspace into a §6.1 manifest here, because the provisioning
/// layer consumed §6.1. It consumes the domain model now, and a workspace is several documents
/// that `Workspace::from_yaml` reads directly, so the transformation — and the whole second
/// schema it targeted — is gone. Concatenating rather than passing the directory keeps the one
/// property lowering was carrying: `ExperimentSvc` resolves once and hands the *same document*
/// to every sub-RPC, so `up` cannot fan out to a topology that changed underneath it.
///
/// The reproducibility checksum therefore covers exactly the documents that were read.
async fn read_workspace(dir: &str) -> Result<String, Status> {
    let root = PathBuf::from(dir);
    tokio::task::spawn_blocking(move || {
        let bad =
            |e: String| Status::invalid_argument(format!("workspace `{}`: {e}", root.display()));
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&root)
            .map_err(|e| bad(e.to_string()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
            .collect();
        // Sorted, so the same workspace always produces the same document and the same hash.
        paths.sort();
        if paths.is_empty() {
            return Err(bad("no `*.yaml` documents here".to_string()));
        }
        let mut out = String::new();
        for path in paths {
            let text = std::fs::read_to_string(&path).map_err(|e| bad(e.to_string()))?;
            if !out.is_empty() {
                out.push_str("---\n");
            }
            out.push_str(&text);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| Status::internal(format!("workspace task panicked: {e}")))?
}

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Workspace documents → checked domain model. The daemon's single entry point for manifest
/// text, whether it arrived as a directory (concatenated by [`read_workspace`]) or inline.
fn parse_to_model(yaml: &str) -> Result<vxedge_core::model::Experiment, String> {
    vxedge_core::model::load_yaml(yaml).map_err(|e| e.to_string())
}

/// Re-parses `yaml` to compile its `mode: s-nssai` slices, if any. Only
/// called when `GatewayPlan.snssai_slice_ids` is non-empty, so a real error
/// here (bad `s_nssai` block, duplicate SST/SD, ...) is a genuine manifest
/// problem, not the "no s-nssai slices" case (already ruled out by the
/// caller) — surfaced as a hard `Up` failure rather than silently skipped.
fn parse_and_fiveg_plan(yaml: &str) -> Result<FiveGPlan, String> {
    FiveGPlan::from_model(&parse_to_model(yaml)?).map_err(|e| e.to_string())
}

/// Run a blocking gateway operation off the async runtime.
async fn blocking<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("gateway task panicked: {e}"))?
}

#[derive(Clone, Copy)]
enum Verb {
    Apply,
    Up,
    Down,
}

pub struct GatewaySvc {
    pub engine: JobEngine,
    pub audit: Arc<Audit>,
    pub shell: Shell,
    /// Active experiment (plan + manifest). Set on up, cleared on down; lost
    /// on daemon restart, in which case Status degrades honestly.
    pub state: SharedState,
    /// Where fiveg renders its config set (`<runtime_dir>/fiveg-config`).
    /// Requires `vxedged` deployed as an **unprivileged** process (never
    /// `sudo vxedged`, never setcap — see [`Shell::escalate`]'s doc comment)
    /// with a cached or NOPASSWD `sudo` for `ip`/`bridge`/`tc`/`nsenter`:
    /// running the whole daemon as root switches `podman` to root's own
    /// separate rootless-podman identity, the exact bug found live
    /// 2026-07-17 running `sudo cargo test` (see `fiveg::lifecycle`'s module
    /// doc comment). `TESTBED-SETUP.md` documents the sudoers requirement.
    pub runtime_dir: PathBuf,
}

impl GatewaySvc {
    async fn lifecycle_job(
        &self,
        rpc: &'static str,
        kind: &'static str,
        mref: Option<&ManifestRef>,
        verb: Verb,
    ) -> Result<Response<Job>, Status> {
        let (yaml, sha256, manifest_dir) = resolve_manifest(mref).await?;
        self.audit
            .record(rpc, serde_json::json!({ "manifest_sha256": sha256 }));
        let shell = self.shell.clone();
        let state = self.state.clone();
        let runtime_dir = self.runtime_dir.clone();
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit(kind, move |p| async move {
                p.update(10, "parsing workspace");
                // Built once, here, and kept: every verb downstream reads the model rather
                // than re-deriving it from the document.
                let model = parse_to_model(&yaml)?;
                let plan = GatewayPlan::from_model(&model).map_err(|e| e.to_string())?;
                p.update(30, &format!("plan: {}", plan.summary()));
                match verb {
                    Verb::Apply => Ok(format!(
                        "validated and planned — {} (nothing started; apply is plan-only)",
                        plan.summary()
                    )),
                    Verb::Up => {
                        // Converging reconciler (`docs/ROADMAP.md` #16 / vxedge/TASKS.md §2):
                        // `apply_links`/`apply_slices`/`apply_endpoints` are each individually
                        // idempotent (the netns integration test proves double-apply is a
                        // no-op), but until now a single observed gap after one pass — a
                        // transient kernel-level hiccup, not a real misconfiguration — failed
                        // the whole `up` outright and left the operator to notice and re-run
                        // it themselves, even though a second, identical apply pass would very
                        // likely have converged. Retrying the same idempotent sequence in
                        // place is that same re-run, just automatic and bounded rather than
                        // manual and open-ended. `MAX_ATTEMPTS` bounds it: a persistent
                        // failure (a real bug, not a transient one) still fails `up`, with the
                        // observed delta after every attempt, not silently retried forever.
                        const MAX_ATTEMPTS: u32 = 3;
                        let mut last_obs = None;
                        for attempt in 1..=MAX_ATTEMPTS {
                            let note = if attempt == 1 {
                                String::new()
                            } else {
                                format!(" (reconcile attempt {attempt}/{MAX_ATTEMPTS})")
                            };
                            p.update(50, &format!("provisioning bridge + uplink{note}"));
                            let sh = shell.clone();
                            let pl = plan.clone();
                            blocking(move || {
                                gateway::data_plane(pl.data_plane)
                                    .apply_links(&sh, &pl)
                                    .map_err(|e| e.to_string())
                            })
                            .await?;
                            p.update(65, &format!("applying slice VLANs + HTB queue tree{note}"));
                            let sh = shell.clone();
                            let pl = plan.clone();
                            blocking(move || {
                                gateway::data_plane(pl.data_plane)
                                    .apply_slices(&sh, &pl)
                                    .map_err(|e| e.to_string())
                            })
                            .await?;
                            p.update(80, &format!("attaching zone endpoints{note}"));
                            let sh = shell.clone();
                            let pl = plan.clone();
                            blocking(move || {
                                gateway::data_plane(pl.data_plane)
                                    .apply_endpoints(&sh, &pl)
                                    .map_err(|e| e.to_string())
                            })
                            .await?;
                            // Found live 2026-08-04, testing VXLAN provisioning end to end
                            // through this exact RPC: this reconciler loop calls the first three
                            // `DataPlane` stages individually (for the per-stage progress
                            // updates above) but never called `apply_tunnels` — so no declared
                            // tunnel, WireGuard or vxlan, was ever actually provisioned through
                            // `vxedge up`, only through the lower-level `apply()` helper the
                            // netns test suite calls directly. `observe()` below already checked
                            // tunnel status via `fully_up()`, so a manifest declaring one would
                            // retry three times and fail with a confusing "did not converge"
                            // rather than the real cause (the stage that would have provisioned
                            // it was simply never called).
                            p.update(85, &format!("applying tunnels{note}"));
                            let sh = shell.clone();
                            let pl = plan.clone();
                            blocking(move || {
                                gateway::data_plane(pl.data_plane)
                                    .apply_tunnels(&sh, &pl)
                                    .map_err(|e| e.to_string())
                            })
                            .await?;
                            p.update(90, "verifying observed state");
                            let sh = shell.clone();
                            let pl = plan.clone();
                            let obs = blocking(move || {
                                gateway::data_plane(pl.data_plane)
                                    .observe(&sh, &pl)
                                    .map_err(|e| e.to_string())
                            })
                            .await?;
                            let converged = obs.fully_up();
                            last_obs = Some(obs);
                            if converged {
                                break;
                            }
                            if attempt < MAX_ATTEMPTS {
                                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                            }
                        }
                        let obs = last_obs.expect("loop runs at least once");
                        if !obs.fully_up() {
                            return Err(format!(
                                "provisioning incomplete after {MAX_ATTEMPTS} attempts (idempotent \
                                 re-apply did not converge): bridge={} uplink={} endpoints={}/{}",
                                obs.bridge_present,
                                obs.uplink_present,
                                obs.endpoints_present,
                                obs.endpoints_expected
                            ));
                        }

                        let five_g_plan = if plan.snssai_slice_ids.is_empty() {
                            None
                        } else {
                            p.update(93, "provisioning 5G core (fiveg)");
                            let fp = parse_and_fiveg_plan(&yaml)?;
                            let rt = runtime_dir.clone();
                            let fp2 = fp.clone();
                            blocking(move || {
                                fiveg::fiveg_up(&fp2, &rt, &FiveGLifecycleConfig::default())
                                    .map_err(|e| e.to_string())
                            })
                            .await?;

                            p.update(97, "attaching 5G core to vxbr0");
                            let sh = shell.clone();
                            let bridge = plan.bridge.clone();
                            let ue_subnets: Vec<String> =
                                fp.sessions.iter().map(|s| s.ue_subnet.clone()).collect();
                            let slice_qos: Vec<gateway::provision::FiveGSliceQos> = fp
                                .sessions
                                .iter()
                                .filter_map(|s| {
                                    s.qos.map(|q| gateway::provision::FiveGSliceQos {
                                        ue_subnet: s.ue_subnet.clone(),
                                        min_rate_mbps: q.min_rate_mbps,
                                        max_rate_mbps: q.max_rate_mbps,
                                        priority: q.priority,
                                        class_minor: q.class_minor,
                                    })
                                })
                                .collect();
                            let root_rate_mbps = plan.root_rate_mbps;
                            blocking(move || {
                                let cfg = FiveGLifecycleConfig::default();
                                let cn_pid =
                                    fiveg::pod_pid(&cfg.pod_name).map_err(|e| e.to_string())?;
                                fiveg::bridge_up(&cfg, &bridge, cn_pid)
                                    .map_err(|e| e.to_string())?;
                                let host_veth = fiveg::bridge_host_veth(&cfg.pod_name);
                                let host_iface =
                                    gateway::provision::apply_fiveg_uplink(&sh, &host_veth)
                                        .map_err(|e| e.to_string())?;
                                gateway::provision::apply_fiveg_ue_routes(
                                    &sh,
                                    &host_iface,
                                    &ue_subnets,
                                )
                                .map_err(|e| e.to_string())?;
                                gateway::provision::apply_fiveg_slice_qos(
                                    &sh,
                                    &host_iface,
                                    root_rate_mbps,
                                    &slice_qos,
                                )
                                .map_err(|e| e.to_string())
                            })
                            .await?;

                            p.update(99, "bringing up each slice's UE pod");
                            let rt = runtime_dir.clone();
                            let fp3 = fp.clone();
                            blocking(move || {
                                let cfg = FiveGLifecycleConfig::default();
                                let ran_pid =
                                    fiveg::pod_pid(&cfg.ran_pod_name).map_err(|e| e.to_string())?;
                                let cfg_dir = rt.join("fiveg-config");
                                for (i, session) in fp3.sessions.iter().enumerate() {
                                    fiveg::ue_up(&cfg, &cfg_dir, ran_pid, i, session)
                                        .map_err(|e| e.to_string())?;
                                }
                                Ok::<(), String>(())
                            })
                            .await?;
                            Some(fp)
                        };

                        // Persist exactly what was provisioned. When the request named a §6.0
                        // workspace this is the *lowered* manifest, which exists nowhere on
                        // disk otherwise — so without this there is no artifact recording what
                        // actually ran. Campaign tooling that still consumes §6.1 files
                        // (vxtelemetry) reads it from here, and it makes a run inspectable
                        // after the fact rather than only reconstructable.
                        let active = runtime_dir.join("manifest.yaml");
                        if let Err(e) = tokio::fs::write(&active, &yaml).await {
                            tracing::warn!(path = %active.display(), error = %e,
                                "could not persist the active manifest");
                        }

                        let summary = plan.summary();
                        let mut st = state.lock().await;
                        st.gateway = Some(plan.clone());
                        st.active = Some(ActiveExperiment {
                            model: model.clone(),
                            dir: manifest_dir.clone().unwrap_or_else(|| PathBuf::from(".")),
                            sha256: sha256.clone(),
                        });
                        st.fiveg = five_g_plan;
                        drop(st);

                        // Emitted after the state is durable, never before: a subscriber told of
                        // a transition that then fails has been told something untrue.
                        for slice in &plan.slices {
                            events.emit(
                                crate::events::EventKind::SliceApplied,
                                &slice.id,
                                &format!(
                                    "vlan {}, {}..{} mbit, prio {}",
                                    slice.vlan,
                                    slice.min_rate_mbps,
                                    slice.max_rate_mbps,
                                    slice.priority
                                ),
                            );
                        }
                        events.emit(crate::events::EventKind::GatewayUp, "vxgateway", &summary);
                        Ok(format!("vxgateway up — {summary}"))
                    }
                    Verb::Down => {
                        if !plan.snssai_slice_ids.is_empty() {
                            p.update(45, "removing UE pods");
                            let fp = parse_and_fiveg_plan(&yaml)?;
                            blocking(move || {
                                let cfg = FiveGLifecycleConfig::default();
                                for session in &fp.sessions {
                                    // Best-effort: an already-absent UE pod
                                    // isn't a teardown failure (ue_down
                                    // itself is idempotent), so don't let
                                    // one missing pod abort the rest.
                                    let _ = fiveg::ue_down(&cfg, session);
                                }
                                Ok::<(), String>(())
                            })
                            .await?;

                            p.update(50, "removing 5G core + uplink attachment");
                            let sh = shell.clone();
                            let bridge = plan.bridge.clone();
                            blocking(move || {
                                let cfg = FiveGLifecycleConfig::default();
                                fiveg::bridge_down(&cfg).map_err(|e| e.to_string())?;
                                gateway::provision::teardown_fiveg_uplink(&sh, &bridge)
                                    .map_err(|e| e.to_string())?;
                                fiveg::fiveg_down(&cfg)
                                    .map(|_| ())
                                    .map_err(|e| e.to_string())
                            })
                            .await?;
                        }
                        p.update(60, "removing endpoints, uplink, bridge");
                        let sh = shell.clone();
                        let pl = plan.clone();
                        blocking(move || {
                            gateway::data_plane(pl.data_plane)
                                .teardown(&sh, &pl)
                                .map_err(|e| e.to_string())
                        })
                        .await?;
                        *state.lock().await = EdgeState::default();
                        events.emit(
                            crate::events::EventKind::GatewayDown,
                            "vxgateway",
                            "bridge, uplink and endpoints removed",
                        );
                        Ok("vxgateway down — all gateway objects removed".to_string())
                    }
                }
            })
            .await;
        Ok(Response::new(job))
    }
}

#[tonic::async_trait]
impl GatewayService for GatewaySvc {
    async fn apply(&self, req: Request<GatewayApplyRequest>) -> Result<Response<Job>, Status> {
        self.lifecycle_job(
            "GatewayService.Apply",
            "gateway.apply",
            req.get_ref().manifest.as_ref(),
            Verb::Apply,
        )
        .await
    }

    async fn up(&self, req: Request<GatewayUpRequest>) -> Result<Response<Job>, Status> {
        self.lifecycle_job(
            "GatewayService.Up",
            "gateway.up",
            req.get_ref().manifest.as_ref(),
            Verb::Up,
        )
        .await
    }

    async fn down(&self, req: Request<GatewayDownRequest>) -> Result<Response<Job>, Status> {
        self.lifecycle_job(
            "GatewayService.Down",
            "gateway.down",
            req.get_ref().manifest.as_ref(),
            Verb::Down,
        )
        .await
    }

    async fn status(
        &self,
        _: Request<GatewayStatusRequest>,
    ) -> Result<Response<GatewayStatus>, Status> {
        let (plan, fiveg_plan) = {
            let st = self.state.lock().await;
            (st.gateway.clone(), st.fiveg.clone())
        };
        let (state, detail, slices, impairments, ingress_filters, tunnels) = match plan {
            Some(plan) => {
                let snssai_count = plan.snssai_slice_ids.len();
                let shell = self.shell.clone();
                let obs = tokio::task::spawn_blocking(move || {
                    gateway::data_plane(plan.data_plane).observe(&shell, &plan)
                })
                .await
                .map_err(|e| Status::internal(format!("observe task panicked: {e}")))?
                .map_err(|e| Status::internal(e.to_string()))?;
                let slices: Vec<SliceStatus> = obs
                    .slices
                    .iter()
                    .map(|s| SliceStatus {
                        slice_id: s.id.clone(),
                        state: if s.healthy() {
                            ResourceState::Up as i32
                        } else {
                            ResourceState::Degraded as i32
                        },
                        detail: format!(
                            "vlan {}: class={} trunk_vlan={}",
                            s.vlan, s.class_present, s.trunk_vlan_present
                        ),
                    })
                    .collect();
                // Impairments change what every latency number in the run means, so `status`
                // names them rather than leaving the operator to infer a healthy gateway is an
                // unimpaired one. A half-installed impairment already makes `fully_up` false;
                // without this, the degraded line would not say which one. Addressable per-link
                // (`docs/ROADMAP.md` #29) rather than folded only into the free-text `detail`
                // string below — a `--json` client can now tell which link, not just how many.
                let impairments: Vec<ImpairmentStatus> = obs
                    .impairments
                    .iter()
                    .map(|i| {
                        let state = if i.healthy() {
                            ResourceState::Up
                        } else if i.egress_present || i.ingress_present {
                            ResourceState::Degraded
                        } else {
                            ResourceState::Down
                        };
                        ImpairmentStatus {
                            link: i.link.clone(),
                            access_port: i.access_port.clone(),
                            state: state as i32,
                            detail: format!(
                                "egress={} ingress={}",
                                i.egress_present, i.ingress_present
                            ),
                        }
                    })
                    .collect();
                let harness = if obs.impairments.is_empty() {
                    String::new()
                } else {
                    let broken: Vec<&str> = obs
                        .impairments
                        .iter()
                        .filter(|i| !i.healthy())
                        .map(|i| i.link.as_str())
                        .collect();
                    if broken.is_empty() {
                        format!(", {} impaired link(s)", obs.impairments.len())
                    } else {
                        format!(
                            ", impairment not installed on link(s): [{}]",
                            broken.join(", ")
                        )
                    }
                };
                // Same reasoning as impairments above, for a security control instead of a
                // measurement one: a declared anti-spoof filter that silently failed to
                // install is worse than invisible, and `GatewayObservation::fully_up` already
                // refuses to converge on it — `status` should say which endpoint, not just
                // that the gateway overall is degraded.
                let ingress_filters: Vec<IngressFilterStatus> = obs
                    .ingress_filters
                    .iter()
                    .map(|f| IngressFilterStatus {
                        node: f.node.clone(),
                        access_port: f.access_port.clone(),
                        state: if f.present {
                            ResourceState::Up as i32
                        } else {
                            ResourceState::Down as i32
                        },
                        detail: format!("present={}", f.present),
                    })
                    .collect();
                // A tunnel's own public key belongs in `status` for the same reason a filter's
                // presence does — it's the value an operator hands to the peer, and the
                // manifest never carries it (`model::Tunnel`'s own doc comment).
                let tunnels: Vec<TunnelStatus> = obs
                    .tunnels
                    .iter()
                    .map(|t| TunnelStatus {
                        id: t.id.clone(),
                        state: if t.present {
                            ResourceState::Up as i32
                        } else {
                            ResourceState::Down as i32
                        },
                        public_key: t.public_key.clone(),
                        handshake_established: t.handshake_established,
                        detail: format!("present={}", t.present),
                        kind: crate::status::wire_tunnel_kind(t.kind) as i32,
                    })
                    .collect();
                if obs.fully_up() {
                    (
                        ResourceState::Up,
                        format!(
                            "up: {} vlan slice(s), {snssai_count} s-nssai slice(s), {}/{} endpoint(s){harness}",
                            slices.len(),
                            obs.endpoints_present,
                            obs.endpoints_expected
                        ),
                        slices,
                        impairments,
                        ingress_filters,
                        tunnels,
                    )
                } else {
                    (
                        ResourceState::Degraded,
                        format!(
                            "degraded: bridge={} uplink={} endpoints={}/{} ({snssai_count} s-nssai slice(s) declared){harness}",
                            obs.bridge_present,
                            obs.uplink_present,
                            obs.endpoints_present,
                            obs.endpoints_expected
                        ),
                        slices,
                        impairments,
                        ingress_filters,
                        tunnels,
                    )
                }
            }
            None => {
                // No plan in memory. If a bridge exists anyway, say so
                // honestly (daemon restarted after an up?).
                let shell = self.shell.clone();
                let orphan = tokio::task::spawn_blocking(move || shell.link_exists("vxbr0"))
                    .await
                    .unwrap_or(false);
                if orphan {
                    (
                        ResourceState::Degraded,
                        "bridge vxbr0 exists but no plan in memory (daemon restarted?) — \
                         run `vxedge up --manifest ...` to reconcile or `vxedge down` to remove"
                            .to_string(),
                        vec![],
                        vec![],
                        vec![],
                        vec![],
                    )
                } else {
                    (
                        ResourceState::Down,
                        "no gateway provisioned".to_string(),
                        vec![],
                        vec![],
                        vec![],
                        vec![],
                    )
                }
            }
        };

        let mut slices = slices;
        let (mut state, mut detail) = (state, detail);
        if let Some(fp) = fiveg_plan {
            let fstatus = tokio::task::spawn_blocking(|| {
                fiveg::fiveg_status(&FiveGLifecycleConfig::default())
            })
            .await
            .map_err(|e| Status::internal(format!("fiveg status task panicked: {e}")))?
            .map_err(|e| Status::internal(e.to_string()))?;
            let fiveg_state = if fstatus.all_running() {
                ResourceState::Up
            } else {
                ResourceState::Degraded
            };
            let fiveg_detail = format!(
                "fiveg: CN {}/{} RAN {}/{} container(s) running",
                fstatus
                    .containers
                    .iter()
                    .filter(|c| c.state == "running")
                    .count(),
                fstatus.containers.len(),
                fstatus
                    .ran_containers
                    .iter()
                    .filter(|c| c.state == "running")
                    .count(),
                fstatus.ran_containers.len(),
            );
            for session in &fp.sessions {
                slices.push(SliceStatus {
                    slice_id: session.slice_id.clone(),
                    state: fiveg_state as i32,
                    detail: fiveg_detail.clone(),
                });
            }
            detail = format!("{detail}; {fiveg_detail}");
            if fiveg_state != ResourceState::Up {
                state = ResourceState::Degraded;
            }
        }

        Ok(Response::new(GatewayStatus {
            status: Some(ResourceStatus {
                vxid: "vxgateway".into(),
                kind: ResourceKind::Gateway as i32,
                state: state as i32,
                detail,
                since_unix_ms: now_ms(),
            }),
            slices,
            impairments,
            ingress_filters,
            tunnels,
        }))
    }
}

pub struct JobSvc {
    pub engine: JobEngine,
}

#[tonic::async_trait]
impl JobService for JobSvc {
    type WatchStream = Pin<Box<dyn Stream<Item = Result<Job, Status>> + Send>>;

    async fn get(&self, req: Request<GetJobRequest>) -> Result<Response<Job>, Status> {
        let id = &req.get_ref().id;
        self.engine
            .get(id)
            .await
            .map(Response::new)
            .ok_or_else(|| Status::not_found(format!("unknown job `{id}`")))
    }

    async fn watch(
        &self,
        req: Request<WatchJobRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let id = req.get_ref().id.clone();
        let mut rx = self
            .engine
            .watch(&id)
            .await
            .ok_or_else(|| Status::not_found(format!("unknown job `{id}`")))?;

        let (tx, out) = tokio::sync::mpsc::channel::<Result<Job, Status>>(16);
        tokio::spawn(async move {
            loop {
                let snapshot = rx.borrow_and_update().clone();
                let terminal = is_terminal(snapshot.state);
                if tx.send(Ok(snapshot)).await.is_err() {
                    return; // client went away
                }
                if terminal {
                    return;
                }
                if rx.changed().await.is_err() {
                    return; // engine dropped the job channel
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(out),
        )))
    }
}

pub struct EventSvc {
    pub bus: EventBus,
}

#[tonic::async_trait]
impl EventService for EventSvc {
    type SubscribeStream = Pin<Box<dyn Stream<Item = Result<Event, Status>> + Send>>;

    async fn subscribe(
        &self,
        req: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let kinds = req.into_inner().kinds;
        let stream = BroadcastStream::new(self.bus.subscribe()).filter_map(move |item| {
            match item {
                Ok(event) if kinds.is_empty() || kinds.contains(&event.kind) => Some(Ok(event)),
                Ok(_) => None,
                // Slow consumer skipped `n` events; keep streaming rather than erroring.
                Err(_lagged) => None,
            }
        });
        Ok(Response::new(Box::pin(stream)))
    }
}

// ---------------------------------------------------------------------------
// ZoneService
// ---------------------------------------------------------------------------

pub struct ZoneSvc {
    pub engine: JobEngine,
    pub audit: Arc<Audit>,
    pub shell: Shell,
    pub state: SharedState,
    pub runtime_dir: PathBuf,
    /// Digest-addressed cache dir for published parts, passed to `zones::node_up` — same
    /// directory `NosCheckSvc` fetches into, so a part pulled via one path is cached for the
    /// other too.
    pub cache_dir: PathBuf,
}

impl ZoneSvc {
    /// Node plans for the active experiment, optionally filtered to one zone.
    async fn plans_for(&self, zone_vxid: Option<&str>) -> Result<Vec<NodePlan>, Status> {
        let st = self.state.lock().await;
        let (model, dir, gateway) = match (&st.active, &st.gateway) {
            (Some(a), Some(g)) => (a.model.clone(), a.dir.clone(), g.clone()),
            _ => {
                return Err(Status::failed_precondition(
                    "no active experiment: zone verbs act on the topology the gateway registered, so \
                     bring one up first (`vxedge experiment up` from a workspace directory)",
                ));
            }
        };
        drop(st);
        let plans = zones::plan_nodes_from_model(&model, &gateway, &dir)
            .map_err(|e| Status::internal(e.to_string()))?;
        // A zone is a *region*, so `--vxid front-left` brings up everything in it. A single
        // device is addressable by its own id as well, and that is not a convenience: exp-002's
        // isolation contract is exactly one arm up at a time, and its three arms are three
        // packaging backends of one region — under region-only selection they could only be
        // started together, which is the concurrent-guest CPU contention that invalidated
        // exp-002b's first campaign. The model forbids a zone and a device sharing an id
        // (`InvariantError::ZoneDeviceCollision`), so accepting either is unambiguous.
        let filtered: Vec<NodePlan> = match zone_vxid {
            Some(z) => plans
                .into_iter()
                .filter(|p| p.zone_id == z || p.node_id == z)
                .collect(),
            None => plans,
        };
        if filtered.is_empty() {
            return Err(Status::not_found(format!(
                "no zone or device `{}` in the active experiment",
                zone_vxid.unwrap_or("<all>")
            )));
        }
        Ok(filtered)
    }

    async fn lifecycle(
        &self,
        rpc: &'static str,
        kind: &'static str,
        vxid: String,
        up: bool,
    ) -> Result<Response<Job>, Status> {
        let plans = self.plans_for(Some(&vxid)).await?;
        // The workspace checksum rides along: an audit line saying a zone came up is only
        // provenance if it says which topology it came up in.
        let workspace = self
            .state
            .lock()
            .await
            .active
            .as_ref()
            .map(|a| a.sha256.clone())
            .unwrap_or_default();
        self.audit.record(
            rpc,
            serde_json::json!({ "zone": vxid, "nodes": plans.len(), "workspace_sha256": workspace }),
        );
        let shell = self.shell.clone();
        let runtime_dir = self.runtime_dir.clone();
        let cache_dir = self.cache_dir.clone();
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit(kind, move |p| async move {
                let total = plans.len();
                for (i, np) in plans.iter().enumerate() {
                    let pct = 10 + (i as u32 * 80) / total as u32;
                    p.update(
                        pct,
                        &format!(
                            "{} node {}/{}: {} ({:?})",
                            if up { "starting" } else { "stopping" },
                            i + 1,
                            total,
                            np.node_id,
                            np.backend
                        ),
                    );
                    let sh = shell.clone();
                    let rd = runtime_dir.clone();
                    let cd = cache_dir.clone();
                    let node = np.clone();
                    blocking(move || {
                        if up {
                            zones::node_up(&sh, &node, &rd, &cd).map_err(|e| e.to_string())
                        } else {
                            zones::node_down(&sh, &node, &rd).map_err(|e| e.to_string())
                        }
                    })
                    .await?;
                    // Per device, not per job: a subscriber watching one arm of exp-002 wants
                    // to know when *that* device came up, and a job-level event cannot say it.
                    events.emit(
                        if up {
                            crate::events::EventKind::ZoneUp
                        } else {
                            crate::events::EventKind::ZoneDown
                        },
                        &np.node_id,
                        &format!("{:?} on {} (vlan {})", np.backend, np.endpoint, np.vlan),
                    );
                }
                // Verify observed state matches the verb before claiming success.
                let sh = shell.clone();
                let rd = runtime_dir.clone();
                let verify = plans.clone();
                let wrong: Vec<String> = blocking(move || {
                    Ok(verify
                        .iter()
                        .filter(|np| {
                            let s = zones::node_status(&sh, np, &rd);
                            up != (s == vxedge_core::zones::backend::NodeState::Up)
                        })
                        .map(|np| np.node_id.clone())
                        .collect())
                })
                .await?;
                if !wrong.is_empty() {
                    return Err(format!(
                        "verification failed — nodes not {}: {}",
                        if up { "up" } else { "down" },
                        wrong.join(", ")
                    ));
                }
                Ok(format!(
                    "zone `{vxid}` {}: {total} node(s)",
                    if up { "up" } else { "down" }
                ))
            })
            .await;
        Ok(Response::new(job))
    }
}

#[tonic::async_trait]
impl ZoneService for ZoneSvc {
    async fn apply(&self, _: Request<ZoneApplyRequest>) -> Result<Response<Job>, Status> {
        Err(Status::unimplemented(
            "ZoneService.Apply: zone manifests register via the experiment manifest for now",
        ))
    }

    async fn up(&self, req: Request<ZoneUpRequest>) -> Result<Response<Job>, Status> {
        let vxid = req
            .into_inner()
            .r#ref
            .map(|r| r.vxid)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Status::invalid_argument("--vxid <zone id> is required"))?;
        self.lifecycle("ZoneService.Up", "zone.up", vxid, true)
            .await
    }

    async fn down(&self, req: Request<ZoneDownRequest>) -> Result<Response<Job>, Status> {
        let vxid = req
            .into_inner()
            .r#ref
            .map(|r| r.vxid)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Status::invalid_argument("--vxid <zone id> is required"))?;
        self.lifecycle("ZoneService.Down", "zone.down", vxid, false)
            .await
    }

    async fn list(
        &self,
        _: Request<ZoneListRequest>,
    ) -> Result<Response<ZoneListResponse>, Status> {
        let plans = self.plans_for(None).await?;
        let shell = self.shell.clone();
        let rd = self.runtime_dir.clone();
        let statuses = tokio::task::spawn_blocking(move || {
            plans
                .iter()
                .map(|np| {
                    let s = zones::node_status(&shell, np, &rd);
                    crate::status::resource_status(
                        np.node_id.clone(),
                        ResourceKind::Zone,
                        &s,
                        // `peer` is here because interface names are derived from the device id
                        // and shortened to fit IFNAMSIZ, so `vz-node-a-c06a` is not something an
                        // operator reconstructs by eye. This listing is the answer to "which
                        // interface is this device", and for a container-backed node the useful
                        // half is the peer — that is the end campaign scripts address.
                        format!(
                            "zone={} backend={:?} port={} peer={} vlan={}",
                            np.zone_id, np.backend, np.endpoint, np.peer, np.vlan
                        ),
                    )
                })
                .collect()
        })
        .await
        .map_err(|e| Status::internal(format!("status task panicked: {e}")))?;
        Ok(Response::new(ZoneListResponse { zones: statuses }))
    }

    async fn remove(&self, _: Request<ZoneRemoveRequest>) -> Result<Response<Job>, Status> {
        Err(Status::unimplemented(
            "ZoneService.Remove: zones are defined by the experiment manifest for now",
        ))
    }

    async fn push_image(
        &self,
        _: Request<tonic::Streaming<ImagePushRequest>>,
    ) -> Result<Response<ImagePushResponse>, Status> {
        Err(Status::unimplemented(
            "ZoneService.PushImage: image store lands with the registry work",
        ))
    }
}

// ---------------------------------------------------------------------------
// ComputeService — central HPC nodes
// ---------------------------------------------------------------------------

/// `ComputeService` and `ZoneService` share one lifecycle (`zones::node_up`/`node_down`/
/// `node_status` over one `NodePlan`) — a compute node is provisioned exactly like a
/// zone-controller/ECU, because the container/qemu-microvm/libvirt backends never distinguished
/// them (`plan_nodes_from_model` didn't carry `role` before this service needed to filter on
/// it). What differs is the RPC surface and vocabulary: `vxedge compute up --vxid hpc1` speaks
/// to the role an OEM engineer thinks in ("central compute"), the same way `vxedge zone` speaks
/// to zone-controllers/ECUs, even though both ultimately call the same provisioning code.
///
/// Deliberately **not** wired into `ExperimentService`'s `zones` stage: that stage already
/// brings up every device regardless of role (nothing in it has ever filtered on `role`), so an
/// hpc-role device in today's five frozen experiments is already provisioned by `zone up`. Adding
/// a compute stage that also claims hpc-role devices would double-provision them, or require
/// carving them out of the zones stage first — a real, separate design decision (`VehicleService`
/// is where `docs/ROADMAP.md` already plans a real gateway→zones→compute→signal-plane→services
/// order) deliberately left for when that orchestration exists, not guessed at here. This
/// service is additive and standalone: it gives compute nodes their own addressable verb without
/// changing what `experiment up`/`zone up` already do to a single frozen experiment.
pub struct ComputeSvc {
    pub engine: JobEngine,
    pub audit: Arc<Audit>,
    pub shell: Shell,
    pub state: SharedState,
    pub runtime_dir: PathBuf,
    /// Digest-addressed cache dir for published parts, passed to `zones::node_up` — see
    /// `ZoneSvc::cache_dir`'s own doc comment; `ComputeSvc` shares the exact same lifecycle.
    pub cache_dir: PathBuf,
}

impl ComputeSvc {
    /// Node plans for the active experiment's compute (hpc-role) devices, optionally filtered
    /// to one. Mirrors `ZoneSvc::plans_for` exactly except for the role filter — kept as its own
    /// copy rather than shared with `ZoneSvc`, since `ZoneSvc` is load-bearing for H1/H3/H4 and a
    /// shared helper would put a second caller's requirements on code those frozen results
    /// depend on.
    async fn plans_for(&self, vxid: Option<&str>) -> Result<Vec<NodePlan>, Status> {
        let st = self.state.lock().await;
        let (model, dir, gateway) = match (&st.active, &st.gateway) {
            (Some(a), Some(g)) => (a.model.clone(), a.dir.clone(), g.clone()),
            _ => {
                return Err(Status::failed_precondition(
                    "no active experiment: compute verbs act on the topology the gateway \
                     registered, so bring one up first (`vxedge experiment up` from a workspace \
                     directory)",
                ));
            }
        };
        drop(st);
        let plans = zones::plan_nodes_from_model(&model, &gateway, &dir)
            .map_err(|e| Status::internal(e.to_string()))?;
        let compute: Vec<NodePlan> = plans
            .into_iter()
            .filter(|p| p.role == vxedge_core::model::DeviceRole::Hpc)
            .filter(|p| vxid.is_none_or(|v| p.node_id == v))
            .collect();
        if compute.is_empty() {
            return Err(Status::not_found(format!(
                "no compute (hpc-role) device `{}` in the active experiment",
                vxid.unwrap_or("<all>")
            )));
        }
        Ok(compute)
    }

    async fn lifecycle(
        &self,
        rpc: &'static str,
        kind: &'static str,
        vxid: String,
        up: bool,
    ) -> Result<Response<Job>, Status> {
        let plans = self.plans_for(Some(&vxid)).await?;
        let workspace = self
            .state
            .lock()
            .await
            .active
            .as_ref()
            .map(|a| a.sha256.clone())
            .unwrap_or_default();
        self.audit.record(
            rpc,
            serde_json::json!({ "compute": vxid, "nodes": plans.len(), "workspace_sha256": workspace }),
        );
        let shell = self.shell.clone();
        let runtime_dir = self.runtime_dir.clone();
        let cache_dir = self.cache_dir.clone();
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit(kind, move |p| async move {
                let total = plans.len();
                for (i, np) in plans.iter().enumerate() {
                    let pct = 10 + (i as u32 * 80) / total as u32;
                    p.update(
                        pct,
                        &format!(
                            "{} node {}/{}: {} ({:?})",
                            if up { "starting" } else { "stopping" },
                            i + 1,
                            total,
                            np.node_id,
                            np.backend
                        ),
                    );
                    let sh = shell.clone();
                    let rd = runtime_dir.clone();
                    let cd = cache_dir.clone();
                    let node = np.clone();
                    blocking(move || {
                        if up {
                            zones::node_up(&sh, &node, &rd, &cd).map_err(|e| e.to_string())
                        } else {
                            zones::node_down(&sh, &node, &rd).map_err(|e| e.to_string())
                        }
                    })
                    .await?;
                    events.emit(
                        if up {
                            crate::events::EventKind::ComputeUp
                        } else {
                            crate::events::EventKind::ComputeDown
                        },
                        &np.node_id,
                        &format!("{:?} on {} (vlan {})", np.backend, np.endpoint, np.vlan),
                    );
                }
                let sh = shell.clone();
                let rd = runtime_dir.clone();
                let verify = plans.clone();
                let wrong: Vec<String> = blocking(move || {
                    Ok(verify
                        .iter()
                        .filter(|np| {
                            let s = zones::node_status(&sh, np, &rd);
                            up != (s == vxedge_core::zones::backend::NodeState::Up)
                        })
                        .map(|np| np.node_id.clone())
                        .collect())
                })
                .await?;
                if !wrong.is_empty() {
                    return Err(format!(
                        "verification failed — nodes not {}: {}",
                        if up { "up" } else { "down" },
                        wrong.join(", ")
                    ));
                }
                Ok(format!(
                    "compute `{vxid}` {}: {total} node(s)",
                    if up { "up" } else { "down" }
                ))
            })
            .await;
        Ok(Response::new(job))
    }
}

#[tonic::async_trait]
impl ComputeService for ComputeSvc {
    async fn apply(&self, _: Request<ComputeApplyRequest>) -> Result<Response<Job>, Status> {
        Err(Status::unimplemented(
            "ComputeService.Apply: compute nodes register via the experiment manifest for now",
        ))
    }

    async fn up(&self, req: Request<ComputeUpRequest>) -> Result<Response<Job>, Status> {
        let vxid = req
            .into_inner()
            .r#ref
            .map(|r| r.vxid)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Status::invalid_argument("--vxid <compute node id> is required"))?;
        self.lifecycle("ComputeService.Up", "compute.up", vxid, true)
            .await
    }

    async fn down(&self, req: Request<ComputeDownRequest>) -> Result<Response<Job>, Status> {
        let vxid = req
            .into_inner()
            .r#ref
            .map(|r| r.vxid)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Status::invalid_argument("--vxid <compute node id> is required"))?;
        self.lifecycle("ComputeService.Down", "compute.down", vxid, false)
            .await
    }

    async fn list(
        &self,
        _: Request<ComputeListRequest>,
    ) -> Result<Response<ComputeListResponse>, Status> {
        let plans = self.plans_for(None).await?;
        let shell = self.shell.clone();
        let rd = self.runtime_dir.clone();
        let statuses = tokio::task::spawn_blocking(move || {
            plans
                .iter()
                .map(|np| {
                    let s = zones::node_status(&shell, np, &rd);
                    crate::status::resource_status(
                        np.node_id.clone(),
                        ResourceKind::Compute,
                        &s,
                        format!(
                            "backend={:?} port={} peer={} vlan={}",
                            np.backend, np.endpoint, np.peer, np.vlan
                        ),
                    )
                })
                .collect()
        })
        .await
        .map_err(|e| Status::internal(format!("status task panicked: {e}")))?;
        Ok(Response::new(ComputeListResponse { nodes: statuses }))
    }

    async fn remove(&self, _: Request<ComputeRemoveRequest>) -> Result<Response<Job>, Status> {
        Err(Status::unimplemented(
            "ComputeService.Remove: compute nodes are defined by the experiment manifest for now",
        ))
    }

    async fn push_image(
        &self,
        _: Request<tonic::Streaming<ImagePushRequest>>,
    ) -> Result<Response<ImagePushResponse>, Status> {
        Err(Status::unimplemented(
            "ComputeService.PushImage: image store lands with the registry work — likely the \
             same store `ZoneService.PushImage` needs, and possibly redundant with vxparts' own \
             ORAS registry; a design decision for whichever lands first, not assumed here",
        ))
    }
}

// ---------------------------------------------------------------------------
// SvcService (ota, telemetry — §8)
// ---------------------------------------------------------------------------

pub struct SvcSvc {
    pub engine: JobEngine,
    pub audit: Arc<Audit>,
    pub runtime_dir: PathBuf,
    /// Binaries the process backend spawns (v1 co-locates every service with vxedged — see
    /// ARCHITECTURE.md §8). A struct rather than one path per service, so a new service type
    /// adds a field instead of a parameter to every caller.
    pub bins: vxedge_core::services::ServiceBinaries,
}

impl SvcSvc {
    async fn service_def(
        &self,
        service_type: &str,
        experiment: Option<&ManifestRef>,
    ) -> Result<(vxedge_core::services::ServiceDef, String, PathBuf), Status> {
        if service_type.is_empty() {
            return Err(Status::invalid_argument(
                "service_type is required (\"ota\" or \"telemetry\")",
            ));
        }
        let (yaml, sha256, dir) = resolve_manifest(experiment).await?;
        let model = parse_to_model(&yaml)
            .map_err(|e| Status::invalid_argument(format!("workspace invalid: {e}")))?;
        let def = vxedge_core::services::ServiceDef::find(&model, service_type)
            .cloned()
            .ok_or_else(|| {
                Status::not_found(format!(
                    "no `{service_type}` service declared under `services` in this experiment"
                ))
            })?;
        // The workspace dir travels with the def: a telemetry service reads its scrape targets
        // from the workspace rather than from its own config, so the backend needs to know
        // which one is active. An inline manifest has no directory; the runtime dir stands in,
        // and telemetry will fail honestly there rather than scrape a workspace that is not on
        // this host.
        Ok((def, sha256, dir.unwrap_or_else(|| self.runtime_dir.clone())))
    }
}

#[tonic::async_trait]
impl SvcService for SvcSvc {
    async fn up(&self, req: Request<SvcUpRequest>) -> Result<Response<Job>, Status> {
        let req = req.into_inner();
        let (def, sha256, workspace_dir) = self
            .service_def(&req.service_type, req.experiment.as_ref())
            .await?;
        self.audit.record(
            "SvcService.Up",
            serde_json::json!({ "service_type": req.service_type, "manifest_sha256": sha256 }),
        );
        let bins = self.bins.clone();
        let runtime_dir = self.runtime_dir.clone();
        let service_type = req.service_type.clone();
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit("svc.up", move |p| async move {
                p.update(30, &format!("starting {service_type} service"));
                let msg = blocking(move || {
                    vxedge_core::services::svc_up(&def, &bins, &workspace_dir, &runtime_dir)
                        .map_err(|e| e.to_string())
                })
                .await?;
                events.emit(crate::events::EventKind::ServiceUp, &service_type, &msg);
                p.update(100, &msg);
                Ok(msg)
            })
            .await;
        Ok(Response::new(job))
    }

    async fn down(&self, req: Request<SvcDownRequest>) -> Result<Response<Job>, Status> {
        let req = req.into_inner();
        if req.service_type.is_empty() {
            return Err(Status::invalid_argument(
                "service_type is required (\"ota\" or \"telemetry\")",
            ));
        }
        self.audit.record(
            "SvcService.Down",
            serde_json::json!({ "service_type": req.service_type }),
        );
        let runtime_dir = self.runtime_dir.clone();
        let service_type = req.service_type.clone();
        let events = self.engine.events().clone();
        let service_type_for_event = service_type.clone();
        let job = self
            .engine
            .submit("svc.down", move |p| async move {
                p.update(50, &format!("stopping {service_type} service"));
                let msg = blocking(move || {
                    vxedge_core::services::svc_down(&service_type, &runtime_dir)
                        .map_err(|e| e.to_string())
                })
                .await?;
                events.emit(
                    crate::events::EventKind::ServiceDown,
                    &service_type_for_event,
                    &msg,
                );
                p.update(100, &msg);
                Ok(msg)
            })
            .await;
        Ok(Response::new(job))
    }

    async fn status(
        &self,
        req: Request<SvcStatusRequest>,
    ) -> Result<Response<ResourceStatus>, Status> {
        let service_type = req.into_inner().service_type;
        if service_type.is_empty() {
            return Err(Status::invalid_argument(
                "service_type is required (\"ota\" or \"telemetry\")",
            ));
        }
        let runtime_dir = self.runtime_dir.clone();
        let st_for_task = service_type.clone();
        let state = tokio::task::spawn_blocking(move || {
            vxedge_core::services::svc_status(&runtime_dir, &st_for_task)
        })
        .await
        .map_err(|e| Status::internal(format!("status task panicked: {e}")))?;
        let (state, detail) = match state {
            vxedge_core::services::SvcState::Up => {
                (ResourceState::Up, format!("{service_type} service running"))
            }
            vxedge_core::services::SvcState::Down => (
                ResourceState::Down,
                format!("{service_type} service not running"),
            ),
        };
        Ok(Response::new(ResourceStatus {
            vxid: format!("svc-{service_type}"),
            kind: ResourceKind::Service as i32,
            state: state as i32,
            detail,
            since_unix_ms: now_ms(),
        }))
    }
}

/// Port every `vxprobe`/`vxtraffic` endpoint this service provisions binds
/// to — matches `run-exp-001.sh`'s own hardcoded convention (`9800`) rather
/// than inventing a new one; `TrafficProfile` has no `port` field in the
/// manifest schema to read one from.
const TRAFFIC_PORT: u16 = 9800;

pub struct TrafficSvc {
    pub engine: JobEngine,
    pub audit: Arc<Audit>,
    pub shell: Shell,
    pub runtime_dir: PathBuf,
    pub vxprobe_bin: String,
    pub vxtraffic_bin: String,
}

impl TrafficSvc {
    /// Resolve a manifest into everything `TrafficPlan::from_manifest`
    /// needs. Builds a fresh `GatewayPlan`/`FiveGPlan` from the manifest
    /// alone rather than reading `GatewaySvc`'s shared `EdgeState` — both
    /// are pure, deterministic functions of the manifest, so this only
    /// requires `vxedge up` to have *already run* against the same
    /// manifest (the interfaces `TrafficPlan` resolves against must exist
    /// on the host/in podman), not that this service reach into another
    /// service's in-memory state.
    async fn resolve_plans(
        &self,
        mref: Option<&ManifestRef>,
    ) -> Result<(TrafficPlan, Option<FiveGPlan>, String, u64), Status> {
        let (yaml, sha256, _dir) = resolve_manifest(mref).await?;
        let model = parse_to_model(&yaml)
            .map_err(|e| Status::invalid_argument(format!("manifest invalid: {e}")))?;
        let gateway_plan = GatewayPlan::from_model(&model)
            .map_err(|e| Status::invalid_argument(format!("gateway plan: {e}")))?;
        let five_g_plan = match FiveGPlan::from_model(&model) {
            Ok(p) => Some(p),
            Err(fiveg::FiveGPlanError::NoSnssaiSlices) => None,
            Err(e) => return Err(Status::invalid_argument(format!("fiveg plan: {e}"))),
        };
        let traffic_plan = TrafficPlan::from_model(&model, &gateway_plan)
            .map_err(|e| Status::invalid_argument(format!("traffic plan: {e}")))?;
        // `experiment.duration_s` is the manifest's own probe/load-window
        // knob (already used for `results_prefix`-adjacent metadata
        // elsewhere) — reused here as the run duration handed to
        // `vxprobe send`/`vxtraffic` rather than letting them silently fall
        // back to their own 10s CLI default, which would undershoot a real
        // campaign's measured window. Falls back to 10 (their own default)
        // when the manifest doesn't set it, e.g. the quick smoke manifests.
        let duration_s = u64::from(model.method.duration_s.unwrap_or(10));
        Ok((traffic_plan, five_g_plan, sha256, duration_s))
    }
}

fn select_profiles(
    plan: TrafficPlan,
    requested: &[String],
) -> Result<Vec<vxedge_core::traffic::ResolvedProfile>, Status> {
    let selected: Vec<_> = plan
        .profiles
        .into_iter()
        .filter(|p| requested.is_empty() || requested.contains(&p.id))
        .collect();
    if selected.is_empty() {
        return Err(Status::not_found(
            "no matching traffic_profiles in this manifest (check `profiles` filter and \
             `network.slices`/`traffic_profiles` in the manifest itself)",
        ));
    }
    Ok(selected)
}

#[tonic::async_trait]
impl TrafficService for TrafficSvc {
    async fn up(&self, req: Request<TrafficUpRequest>) -> Result<Response<Job>, Status> {
        let req = req.into_inner();
        let (traffic_plan, five_g_plan, sha256, duration_s) =
            self.resolve_plans(req.manifest.as_ref()).await?;
        self.audit.record(
            "TrafficService.Up",
            serde_json::json!({ "manifest_sha256": sha256, "profiles": req.profiles }),
        );
        let selected = select_profiles(traffic_plan, &req.profiles)?;

        let shell = self.shell.clone();
        let runtime_dir = self.runtime_dir.clone();
        let vxprobe_bin = self.vxprobe_bin.clone();
        let vxtraffic_bin = self.vxtraffic_bin.clone();
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit("traffic.up", move |p| async move {
                let total = selected.len().max(1);
                let mut messages = Vec::with_capacity(selected.len());
                for (i, profile) in selected.into_iter().enumerate() {
                    p.update(
                        ((i * 100) / total) as u32,
                        &format!("starting traffic profile `{}`", profile.id),
                    );
                    let sh = shell.clone();
                    let fiveg_cfg = FiveGLifecycleConfig::default();
                    let five_g_plan = five_g_plan.clone();
                    let runtime_dir = runtime_dir.clone();
                    let vxprobe_bin = vxprobe_bin.clone();
                    let vxtraffic_bin = vxtraffic_bin.clone();
                    let profile_id = profile.id.clone();
                    let msg = blocking(move || {
                        traffic::profile_up(
                            &sh,
                            &fiveg_cfg,
                            five_g_plan.as_ref(),
                            &runtime_dir,
                            &vxprobe_bin,
                            &vxtraffic_bin,
                            &profile,
                            TRAFFIC_PORT,
                            duration_s,
                        )
                        .map_err(|e| e.to_string())
                    })
                    .await?;
                    events.emit(crate::events::EventKind::TrafficStarted, &profile_id, &msg);
                    messages.push(msg);
                }
                p.update(100, "all traffic profiles up");
                Ok(messages.join("; "))
            })
            .await;
        Ok(Response::new(job))
    }

    async fn down(&self, req: Request<TrafficDownRequest>) -> Result<Response<Job>, Status> {
        let req = req.into_inner();
        let (traffic_plan, _five_g_plan, sha256, _duration_s) =
            self.resolve_plans(req.manifest.as_ref()).await?;
        self.audit.record(
            "TrafficService.Down",
            serde_json::json!({ "manifest_sha256": sha256, "profiles": req.profiles }),
        );
        let selected = select_profiles(traffic_plan, &req.profiles)?;

        let runtime_dir = self.runtime_dir.clone();
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit("traffic.down", move |p| async move {
                let total = selected.len().max(1);
                let mut messages = Vec::with_capacity(selected.len());
                for (i, profile) in selected.into_iter().enumerate() {
                    p.update(
                        ((i * 100) / total) as u32,
                        &format!("stopping traffic profile `{}`", profile.id),
                    );
                    let runtime_dir = runtime_dir.clone();
                    let profile_id = profile.id.clone();
                    let msg = blocking(move || {
                        Ok::<_, String>(traffic::profile_down(&runtime_dir, &profile.id))
                    })
                    .await?;
                    events.emit(crate::events::EventKind::TrafficStopped, &profile_id, &msg);
                    messages.push(msg);
                }
                p.update(100, "all traffic profiles down");
                Ok(messages.join("; "))
            })
            .await;
        Ok(Response::new(job))
    }
}

// ---------------------------------------------------------------------------
// NosCheckService (network.nos_check — production-NOS config-parity check,
// ARCHITECTURE.md §7/§6.0.3). Deliberately its own service, never folded into
// ExperimentService: a missing or failed check must never block or change what an
// experiment's own GatewayService/ZoneService actually provision or measure.
// ---------------------------------------------------------------------------

/// True for a local ref (`./…`, an absolute path, or the `local/` convention `PartEntry`
/// already uses) — the same "local vs. published" distinction `ARCHITECTURE.md` §6.0's
/// "Local-first" section makes for every other ref in the schema.
fn is_local_nos_check_ref(r: &str) -> bool {
    r.starts_with('.') || r.starts_with('/') || r.starts_with("local/")
}

/// Finds the single disk-image file inside a pulled `publish-vm-disk.sh`-shaped part
/// directory (`vxparts/scripts/publish/publish-vm-disk.sh`'s own convention): the part's
/// `meta.json` carries a `files` map of `{filename: digest}` with exactly one entry, since a
/// single-disk-image part ships one disk and no more. Reading it back here rather than
/// hardcoding a filename is what lets `sonic-vs.img`-shaped parts and any future single-disk
/// part share this same resolution path without a naming convention neither side actually
/// enforces.
fn disk_file_in(dir: &std::path::Path) -> Result<PathBuf, String> {
    let meta_path = dir.join("meta.json");
    let text = std::fs::read_to_string(&meta_path)
        .map_err(|e| format!("reading {}: {e}", meta_path.display()))?;
    let meta: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", meta_path.display()))?;
    let files = meta
        .get("files")
        .and_then(|f| f.as_object())
        .ok_or_else(|| format!("{} has no `files` map", meta_path.display()))?;
    match files.len() {
        1 => Ok(dir.join(files.keys().next().unwrap())),
        n => Err(format!(
            "{} declares {n} file(s) in `files` — expected exactly 1 (a single-disk-image part)",
            meta_path.display()
        )),
    }
}

pub struct NosCheckSvc {
    pub engine: JobEngine,
    pub audit: Arc<Audit>,
    pub runtime_dir: PathBuf,
    /// Local disk image used when a `network.nos_check` block omits `part_ref`/`digest` — the
    /// "use the pinned default build" case §6.0.3 documents. Still useful even now that a
    /// published `part_ref` is fetchable (`vxedge_core::registry`): it lets an operator on a
    /// slow or offline link pre-stage the image once rather than have every experiment run
    /// pull it, the same convention `--vxota-bin`/`--vxtelemetry-bin` already use for
    /// "something vxedged needs but does not itself provision."
    pub default_image: Option<PathBuf>,
    /// Where a published `part_ref` gets pulled to (`vxedge_core::registry::fetch`,
    /// `docs/ROADMAP.md` #19). Content-addressed by digest, so this is shared across every
    /// experiment that references the same pinned build — SONiC-VS is 6+ GB, and re-pulling it
    /// per experiment would be its own reproducibility footgun (slow, and a mid-run network
    /// blip becomes a flaky test failure instead of a one-time setup cost).
    pub cache_dir: PathBuf,
}

impl NosCheckSvc {
    /// Resolves the experiment's declared `network.nos_check` to a local disk image this host
    /// can boot: a local `part_ref` resolves directly, a published one is pulled through
    /// `vxedge_core::registry::fetch` (cached by digest, `docs/ROADMAP.md` #19), and no
    /// `part_ref` at all falls back to `default_image`.
    async fn resolve(
        &self,
        mref: Option<&ManifestRef>,
    ) -> Result<(vxedge_core::model::Experiment, PathBuf, String), Status> {
        let (yaml, sha256, dir) = resolve_manifest(mref).await?;
        let model = parse_to_model(&yaml)
            .map_err(|e| Status::invalid_argument(format!("workspace invalid: {e}")))?;
        let nos_check = model.nos_check.clone().ok_or_else(|| {
            Status::failed_precondition(
                "this experiment declares no `network.nos_check` block — nothing to check",
            )
        })?;
        let image = match &nos_check.part_ref {
            Some(r) if is_local_nos_check_ref(r) => {
                let p = PathBuf::from(r);
                if p.is_absolute() {
                    p
                } else {
                    let Some(dir) = &dir else {
                        return Err(Status::invalid_argument(
                            "`network.nos_check.part_ref` is a relative path, which needs a \
                             workspace directory to resolve against — an inline manifest has none",
                        ));
                    };
                    dir.join(p)
                }
            }
            Some(r) => {
                // `digest` is required alongside a non-local `part_ref` — enforced together in
                // `model::build` (§13: a published ref with no digest isn't reproducible) — so
                // this is reachable only with a real digest in hand, never `None` here.
                let Some(digest) = nos_check.digest.clone() else {
                    return Err(Status::invalid_argument(
                        "`network.nos_check.part_ref` is a published ref but declares no \
                         `digest` — published refs must be digest-pinned (§13)",
                    ));
                };
                let reference = r.clone();
                let cache_dir = self.cache_dir.clone();
                let pulled_dir = tokio::task::spawn_blocking(move || {
                    vxedge_core::registry::fetch(&reference, &digest, &cache_dir)
                })
                .await
                .map_err(|e| Status::internal(format!("registry fetch task panicked: {e}")))?
                .map_err(|e| {
                    Status::unavailable(format!(
                        "fetching `network.nos_check.part_ref` `{r}` failed: {e}"
                    ))
                })?;
                disk_file_in(&pulled_dir).map_err(|e| {
                    Status::internal(format!(
                        "pulled `{r}` but could not identify its disk image: {e}"
                    ))
                })?
            }
            None => self.default_image.clone().ok_or_else(|| {
                Status::failed_precondition(
                    "this experiment's `network.nos_check` declares no `part_ref` and this \
                     daemon has no default image configured — start vxedged with \
                     `--sonic-vs-image <path>`, or declare a local `part_ref` in the manifest",
                )
            })?,
        };
        if !image.is_file() {
            return Err(Status::failed_precondition(format!(
                "nos_check image `{}` does not exist on this host",
                image.display()
            )));
        }
        Ok((model, image, sha256))
    }

    fn instance_dir(&self, experiment_id: &str) -> PathBuf {
        self.runtime_dir.join(format!("nos-check-{experiment_id}"))
    }
}

/// Copies `image` into `instance_dir` if not already there, and boots it under `name`. Shared by
/// `Up` and `Verify`: both need the same never-boot-the-shared-path-directly discipline (QEMU
/// opens a qcow2 read-write, and SONiC's first-boot sequence writes to disk — found live,
/// `docs/ROADMAP.md` #16).
fn boot_disposable_copy(
    name: &str,
    image: &std::path::Path,
    instance_dir: &std::path::Path,
) -> Result<vxedge_core::sonic::lifecycle::NosCheckLifecyclePlan, String> {
    std::fs::create_dir_all(instance_dir).map_err(|e| e.to_string())?;
    let copy = instance_dir.join("nos-check.img");
    if !copy.exists() {
        std::fs::copy(image, &copy).map_err(|e| e.to_string())?;
    }
    let plan = vxedge_core::sonic::lifecycle::NosCheckLifecyclePlan {
        name: name.to_string(),
        image: copy,
        memory_mb: 4096,
        vcpus: 2,
    };
    vxedge_core::sonic::lifecycle::nos_check_up(&plan, instance_dir).map_err(|e| e.to_string())?;
    Ok(plan)
}

#[tonic::async_trait]
impl NosCheckService for NosCheckSvc {
    async fn up(&self, req: Request<NosCheckUpRequest>) -> Result<Response<Job>, Status> {
        let req = req.into_inner();
        let (model, image, sha256) = self.resolve(req.experiment.as_ref()).await?;
        self.audit.record(
            "NosCheckService.Up",
            serde_json::json!({ "manifest_sha256": sha256, "experiment": model.id }),
        );
        let name = format!("nos-check-{}", model.id);
        let instance_dir = self.instance_dir(&model.id);
        let experiment_id = model.id.clone();
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit("nos-check.up", move |p| async move {
                p.update(20, "copying disposable boot image and starting QEMU");
                blocking(move || boot_disposable_copy(&name, &image, &instance_dir)).await?;
                let msg = "nos_check instance up".to_string();
                events.emit(crate::events::EventKind::NosCheckUp, &experiment_id, &msg);
                p.update(100, &msg);
                Ok(msg)
            })
            .await;
        Ok(Response::new(job))
    }

    async fn down(&self, req: Request<NosCheckDownRequest>) -> Result<Response<Job>, Status> {
        let req = req.into_inner();
        let (yaml, sha256, _dir) = resolve_manifest(req.experiment.as_ref()).await?;
        let model = parse_to_model(&yaml)
            .map_err(|e| Status::invalid_argument(format!("workspace invalid: {e}")))?;
        self.audit.record(
            "NosCheckService.Down",
            serde_json::json!({ "manifest_sha256": sha256, "experiment": model.id }),
        );
        let name = format!("nos-check-{}", model.id);
        let instance_dir = self.instance_dir(&model.id);
        let experiment_id = model.id.clone();
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit("nos-check.down", move |p| async move {
                p.update(50, "stopping nos_check instance");
                blocking(move || {
                    let plan = vxedge_core::sonic::lifecycle::NosCheckLifecyclePlan {
                        name,
                        // Unused by `nos_check_down` (only `.name` matters for locating the
                        // pidfile/socket), but the type demands them.
                        image: PathBuf::new(),
                        memory_mb: 0,
                        vcpus: 0,
                    };
                    vxedge_core::sonic::lifecycle::nos_check_down(&plan, &instance_dir)
                        .map_err(|e| e.to_string())?;
                    let _ = std::fs::remove_dir_all(&instance_dir);
                    Ok(())
                })
                .await?;
                let msg = "nos_check instance down".to_string();
                events.emit(crate::events::EventKind::NosCheckDown, &experiment_id, &msg);
                p.update(100, &msg);
                Ok(msg)
            })
            .await;
        Ok(Response::new(job))
    }

    async fn status(
        &self,
        req: Request<NosCheckStatusRequest>,
    ) -> Result<Response<NosCheckStatus>, Status> {
        let req = req.into_inner();
        let (yaml, _sha256, _dir) = resolve_manifest(req.experiment.as_ref()).await?;
        let model = parse_to_model(&yaml)
            .map_err(|e| Status::invalid_argument(format!("workspace invalid: {e}")))?;
        // Same requirement `Up`/`Down`/`Verify` enforce via `resolve()` — checked directly here
        // rather than through it, since `Status` needs no image resolution. Without this, an
        // experiment declaring no `network.nos_check` at all would report "not running" as if
        // a check existed and simply wasn't up, rather than naming the real reason there's
        // nothing to report on.
        if model.nos_check.is_none() {
            return Err(Status::failed_precondition(
                "this experiment declares no `network.nos_check` block — nothing to check",
            ));
        }
        let name = format!("nos-check-{}", model.id);
        let instance_dir = self.instance_dir(&model.id);
        let observation = tokio::task::spawn_blocking(move || {
            let plan = vxedge_core::sonic::lifecycle::NosCheckLifecyclePlan {
                name,
                image: PathBuf::new(),
                memory_mb: 0,
                vcpus: 0,
            };
            vxedge_core::sonic::lifecycle::nos_check_status(&plan, &instance_dir)
        })
        .await
        .map_err(|e| Status::internal(format!("status task panicked: {e}")))?;
        let detail = if observation.present {
            "nos_check instance running"
        } else {
            "nos_check instance not running"
        };
        Ok(Response::new(NosCheckStatus {
            status: Some(crate::status::resource_status(
                format!("nos-check-{}", model.id),
                ResourceKind::NosCheck,
                &observation,
                detail,
            )),
        }))
    }

    async fn verify(&self, req: Request<NosCheckVerifyRequest>) -> Result<Response<Job>, Status> {
        let req = req.into_inner();
        let (model, image, sha256) = self.resolve(req.experiment.as_ref()).await?;
        self.audit.record(
            "NosCheckService.Verify",
            serde_json::json!({ "manifest_sha256": sha256, "experiment": model.id }),
        );
        let gateway_plan = vxedge_core::gateway::GatewayPlan::from_model(&model)
            .map_err(|e| Status::invalid_argument(format!("gateway plan: {e}")))?;
        let trunk_port = model
            .nos_check
            .as_ref()
            .expect("resolve() already required network.nos_check to be declared")
            .trunk_port
            .clone();
        let sonic_plan =
            vxedge_core::sonic::plan::SonicPlan::from_gateway_plan(&gateway_plan, trunk_port);
        let config = vxedge_core::sonic::render::render(&sonic_plan);

        // A name unique per call, never `Up`'s own `nos-check-<id>`: `Verify` always does its
        // own fresh boot and always tears down when it finishes, so it must never collide with
        // — or reuse — an instance a separate `Up` call left running. Reusing one would hang:
        // a second console connection to an already-logged-in guest never sees `login:` again.
        let name = format!("nos-check-verify-{}-{}", model.id, std::process::id());
        let instance_dir = self
            .runtime_dir
            .join(format!("nos-check-verify-{}", model.id));
        let experiment_id = model.id.clone();
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit("nos-check.verify", move |p| async move {
                p.update(10, "booting a disposable nos_check instance");
                let outcome = blocking(move || {
                    let plan = boot_disposable_copy(&name, &image, &instance_dir)?;
                    let teardown = |plan: &vxedge_core::sonic::lifecycle::NosCheckLifecyclePlan,
                                    dir: &std::path::Path| {
                        let _ = vxedge_core::sonic::lifecycle::nos_check_down(plan, dir);
                        let _ = std::fs::remove_dir_all(dir);
                    };
                    let sock = instance_dir.join(format!("nos-check-{}.console.sock", plan.name));
                    let result = (|| {
                        let mut console = vxedge_core::sonic::lifecycle::Console::connect(
                            &sock,
                            &plan.name,
                            std::time::Duration::from_secs(10),
                        )
                        .map_err(|e| e.to_string())?;
                        vxedge_core::sonic::config_check::login(
                            &mut console,
                            std::time::Duration::from_secs(150),
                        )
                        .map_err(|e| e.to_string())?;
                        vxedge_core::sonic::config_check::verify_config_parity(
                            &mut console,
                            &config,
                        )
                        .map_err(|e| e.to_string())
                    })();
                    teardown(&plan, &instance_dir);
                    result
                })
                .await;
                let report = outcome?;
                if report.ok() {
                    let msg = format!(
                        "nos_check verify: {}/{} fields match",
                        report.checked, report.checked
                    );
                    events.emit(
                        crate::events::EventKind::NosCheckVerified,
                        &experiment_id,
                        &msg,
                    );
                    p.update(100, &msg);
                    Ok(msg)
                } else {
                    let matched = report.checked - report.mismatches.len();
                    let detail = report
                        .mismatches
                        .iter()
                        .map(|m| {
                            format!(
                                "{}.{}: declared `{}`, live `{}`",
                                m.key, m.field, m.declared, m.live
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    let msg = format!(
                        "nos_check verify: {matched}/{} fields match — {detail}",
                        report.checked
                    );
                    events.emit(
                        crate::events::EventKind::NosCheckVerified,
                        &experiment_id,
                        &msg,
                    );
                    Err(msg)
                }
            })
            .await;
        Ok(Response::new(job))
    }
}
