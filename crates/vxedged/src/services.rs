//! vxAPI service implementations. GatewayService and ZoneService are live
//! end-to-end: gateway Up provisions bridge/VLAN/HTB, zone Up runs nodes
//! under their packaging backend (container / qemu-microvm / libvirt).

use sha2::Digest;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tonic::{Request, Response, Status};

use vxedge_api::v1::event_service_server::EventService;
use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::job_service_server::JobService;
use vxedge_api::v1::manifest_ref::Source;
use vxedge_api::v1::svc_service_server::SvcService;
use vxedge_api::v1::zone_service_server::ZoneService;
use vxedge_api::v1::{
    Event, GatewayApplyRequest, GatewayDownRequest, GatewayStatus, GatewayStatusRequest,
    GatewayUpRequest, GetJobRequest, ImagePushRequest, ImagePushResponse, Job, ManifestRef,
    ResourceKind, ResourceState, ResourceStatus, SliceStatus, SubscribeRequest, SvcDownRequest,
    SvcStatusRequest, SvcUpRequest, WatchJobRequest, ZoneApplyRequest, ZoneDownRequest,
    ZoneListRequest, ZoneListResponse, ZoneRemoveRequest, ZoneUpRequest,
};
use vxedge_core::ExperimentManifest;
use vxedge_core::fiveg::{self, FiveGLifecycleConfig, FiveGPlan};
use vxedge_core::gateway::{self, GatewayPlan, Shell};
use vxedge_core::zones::{self, NodePlan};

use crate::audit::Audit;
use crate::events::EventBus;
use crate::jobs::{JobEngine, is_terminal, now_ms};

/// Daemon-wide view of the active experiment: set by a successful
/// `gateway up`, consumed by zone verbs, cleared by `gateway down`.
#[derive(Default)]
pub struct EdgeState {
    pub gateway: Option<GatewayPlan>,
    pub manifest_yaml: Option<String>,
    pub manifest_dir: Option<PathBuf>,
    /// Set alongside `gateway` whenever the manifest's `network.slices`
    /// include any `mode: s-nssai` — the fiveg CN/RAN pods' desired state,
    /// for `down`/`status` to act on without re-parsing the manifest.
    pub fiveg: Option<FiveGPlan>,
}

pub type SharedState = Arc<tokio::sync::Mutex<EdgeState>>;

/// Resolve a ManifestRef to (yaml, sha256, source dir), enforcing the
/// checksum guard when the caller supplied one.
async fn resolve_manifest(
    mref: Option<&ManifestRef>,
) -> Result<(String, String, Option<PathBuf>), Status> {
    let mref = mref.ok_or_else(|| {
        Status::invalid_argument("a manifest is required (pass --manifest <path> on the CLI)")
    })?;
    let (yaml, dir) = match &mref.source {
        Some(Source::Path(path)) => {
            let yaml = tokio::fs::read_to_string(path).await.map_err(|e| {
                Status::invalid_argument(format!(
                    "cannot read manifest `{path}` on the daemon host: {e}"
                ))
            })?;
            (yaml, PathBuf::from(path).parent().map(PathBuf::from))
        }
        Some(Source::InlineYaml(yaml)) => (yaml.clone(), None),
        None => {
            return Err(Status::invalid_argument(
                "manifest has no source (path or inline_yaml)",
            ));
        }
    };
    let sha256 = hex::encode(sha2::Sha256::digest(yaml.as_bytes()));
    if !mref.sha256.is_empty() && mref.sha256 != sha256 {
        return Err(Status::failed_precondition(format!(
            "manifest checksum mismatch: expected {}, got {sha256}",
            mref.sha256
        )));
    }
    Ok((yaml, sha256, dir))
}

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn parse_and_plan(yaml: &str) -> Result<GatewayPlan, String> {
    let manifest = ExperimentManifest::from_yaml(yaml).map_err(|e| e.to_string())?;
    GatewayPlan::from_manifest(&manifest).map_err(|e| e.to_string())
}

/// Re-parses `yaml` to compile its `mode: s-nssai` slices, if any. Only
/// called when `GatewayPlan.snssai_slice_ids` is non-empty, so a real error
/// here (bad `s_nssai` block, duplicate SST/SD, ...) is a genuine manifest
/// problem, not the "no s-nssai slices" case (already ruled out by the
/// caller) — surfaced as a hard `Up` failure rather than silently skipped.
fn parse_and_fiveg_plan(yaml: &str) -> Result<FiveGPlan, String> {
    let manifest = ExperimentManifest::from_yaml(yaml).map_err(|e| e.to_string())?;
    FiveGPlan::from_manifest(&manifest).map_err(|e| e.to_string())
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
        let job = self
            .engine
            .submit(kind, move |p| async move {
                p.update(10, "parsing manifest");
                let plan = parse_and_plan(&yaml)?;
                p.update(30, &format!("plan: {}", plan.summary()));
                match verb {
                    Verb::Apply => Ok(format!(
                        "validated and planned — {} (nothing started; apply is plan-only)",
                        plan.summary()
                    )),
                    Verb::Up => {
                        p.update(50, "provisioning bridge + uplink");
                        let sh = shell.clone();
                        let pl = plan.clone();
                        blocking(move || {
                            gateway::provision::apply_links(&sh, &pl).map_err(|e| e.to_string())
                        })
                        .await?;
                        p.update(65, "applying slice VLANs + HTB queue tree");
                        let sh = shell.clone();
                        let pl = plan.clone();
                        blocking(move || {
                            gateway::provision::apply_slices(&sh, &pl).map_err(|e| e.to_string())
                        })
                        .await?;
                        p.update(80, "attaching zone endpoints");
                        let sh = shell.clone();
                        let pl = plan.clone();
                        blocking(move || {
                            gateway::provision::apply_endpoints(&sh, &pl).map_err(|e| e.to_string())
                        })
                        .await?;
                        p.update(90, "verifying observed state");
                        let sh = shell.clone();
                        let pl = plan.clone();
                        let obs =
                            blocking(move || gateway::observe(&sh, &pl).map_err(|e| e.to_string()))
                                .await?;
                        if !obs.fully_up() {
                            return Err(format!(
                                "provisioning incomplete: bridge={} uplink={} endpoints={}/{}",
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
                            blocking(move || {
                                let cfg = FiveGLifecycleConfig::default();
                                let cn_pid =
                                    fiveg::pod_pid(&cfg.pod_name).map_err(|e| e.to_string())?;
                                fiveg::bridge_up(&cfg, &bridge, cn_pid)
                                    .map_err(|e| e.to_string())?;
                                let host_veth = fiveg::bridge_host_veth(&cfg.pod_name);
                                gateway::provision::apply_fiveg_uplink(&sh, &host_veth)
                                    .map_err(|e| e.to_string())
                            })
                            .await?;
                            Some(fp)
                        };

                        let summary = plan.summary();
                        let mut st = state.lock().await;
                        st.gateway = Some(plan);
                        st.manifest_yaml = Some(yaml.clone());
                        st.manifest_dir = manifest_dir.clone();
                        st.fiveg = five_g_plan;
                        Ok(format!("vxgateway up — {summary}"))
                    }
                    Verb::Down => {
                        if !plan.snssai_slice_ids.is_empty() {
                            p.update(50, "removing 5G core + uplink attachment");
                            blocking(move || {
                                let cfg = FiveGLifecycleConfig::default();
                                fiveg::bridge_down(&cfg).map_err(|e| e.to_string())?;
                                fiveg::fiveg_down(&cfg)
                                    .map(|_| ())
                                    .map_err(|e| e.to_string())
                            })
                            .await?;
                        }
                        p.update(60, "removing endpoints, uplink, bridge");
                        let sh = shell.clone();
                        let pl = plan.clone();
                        blocking(move || gateway::teardown(&sh, &pl).map_err(|e| e.to_string()))
                            .await?;
                        *state.lock().await = EdgeState::default();
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
        let (state, detail, slices) = match plan {
            Some(plan) => {
                let snssai_count = plan.snssai_slice_ids.len();
                let shell = self.shell.clone();
                let obs = tokio::task::spawn_blocking(move || gateway::observe(&shell, &plan))
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
                if obs.fully_up() {
                    (
                        ResourceState::Up,
                        format!(
                            "up: {} vlan slice(s), {snssai_count} s-nssai slice(s), {}/{} endpoint(s)",
                            slices.len(),
                            obs.endpoints_present,
                            obs.endpoints_expected
                        ),
                        slices,
                    )
                } else {
                    (
                        ResourceState::Degraded,
                        format!(
                            "degraded: bridge={} uplink={} endpoints={}/{} ({snssai_count} s-nssai slice(s) declared)",
                            obs.bridge_present,
                            obs.uplink_present,
                            obs.endpoints_present,
                            obs.endpoints_expected
                        ),
                        slices,
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
                    )
                } else {
                    (
                        ResourceState::Down,
                        "no gateway provisioned".to_string(),
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
}

impl ZoneSvc {
    /// Node plans for the active experiment, optionally filtered to one zone.
    async fn plans_for(&self, zone_vxid: Option<&str>) -> Result<Vec<NodePlan>, Status> {
        let st = self.state.lock().await;
        let (yaml, gateway) = match (&st.manifest_yaml, &st.gateway) {
            (Some(y), Some(g)) => (y.clone(), g.clone()),
            _ => {
                return Err(Status::failed_precondition(
                    "no active experiment — run `vxedge up --manifest <exp.yaml>` first",
                ));
            }
        };
        let dir = st
            .manifest_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        drop(st);
        let manifest = ExperimentManifest::from_yaml(&yaml)
            .map_err(|e| Status::internal(format!("stored manifest no longer parses: {e}")))?;
        let plans = zones::plan_nodes(&manifest, &gateway, &dir)
            .map_err(|e| Status::internal(e.to_string()))?;
        let filtered: Vec<NodePlan> = match zone_vxid {
            Some(z) => plans.into_iter().filter(|p| p.zone_id == z).collect(),
            None => plans,
        };
        if filtered.is_empty() {
            return Err(Status::not_found(format!(
                "no nodes for zone `{}` in the active experiment",
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
        self.audit.record(
            rpc,
            serde_json::json!({ "zone": vxid, "nodes": plans.len() }),
        );
        let shell = self.shell.clone();
        let runtime_dir = self.runtime_dir.clone();
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
                    let node = np.clone();
                    blocking(move || {
                        if up {
                            zones::node_up(&sh, &node, &rd).map_err(|e| e.to_string())
                        } else {
                            zones::node_down(&sh, &node, &rd).map_err(|e| e.to_string())
                        }
                    })
                    .await?;
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
                    ResourceStatus {
                        vxid: np.node_id.clone(),
                        kind: ResourceKind::Zone as i32,
                        state: if s == vxedge_core::zones::backend::NodeState::Up {
                            ResourceState::Up as i32
                        } else {
                            ResourceState::Down as i32
                        },
                        detail: format!(
                            "zone={} backend={:?} port={} vlan={}",
                            np.zone_id, np.backend, np.endpoint, np.vlan
                        ),
                        since_unix_ms: now_ms(),
                    }
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
// SvcService (ota, telemetry — §8)
// ---------------------------------------------------------------------------

fn service_type_str(t: vxedge_core::manifest::ServiceType) -> &'static str {
    match t {
        vxedge_core::manifest::ServiceType::Ota => "ota",
        vxedge_core::manifest::ServiceType::Telemetry => "telemetry",
    }
}

pub struct SvcSvc {
    pub engine: JobEngine,
    pub audit: Arc<Audit>,
    pub runtime_dir: PathBuf,
    /// Path to the `vxota` binary the process backend spawns (v1 co-locates
    /// the service with vxedged — see ARCHITECTURE.md §8).
    pub vxota_bin: String,
}

impl SvcSvc {
    async fn service_def(
        &self,
        service_type: &str,
        experiment: Option<&ManifestRef>,
    ) -> Result<(vxedge_core::manifest::ServiceDef, String), Status> {
        if service_type.is_empty() {
            return Err(Status::invalid_argument(
                "service_type is required (\"ota\" or \"telemetry\")",
            ));
        }
        let (yaml, sha256, _dir) = resolve_manifest(experiment).await?;
        let manifest = ExperimentManifest::from_yaml(&yaml)
            .map_err(|e| Status::invalid_argument(format!("manifest invalid: {e}")))?;
        let def = manifest
            .services
            .values()
            .find(|s| service_type_str(s.service_type) == service_type)
            .cloned()
            .ok_or_else(|| {
                Status::not_found(format!(
                    "no `{service_type}` service declared under `services` in this manifest"
                ))
            })?;
        Ok((def, sha256))
    }
}

#[tonic::async_trait]
impl SvcService for SvcSvc {
    async fn up(&self, req: Request<SvcUpRequest>) -> Result<Response<Job>, Status> {
        let req = req.into_inner();
        let (def, sha256) = self
            .service_def(&req.service_type, req.experiment.as_ref())
            .await?;
        self.audit.record(
            "SvcService.Up",
            serde_json::json!({ "service_type": req.service_type, "manifest_sha256": sha256 }),
        );
        let vxota_bin = self.vxota_bin.clone();
        let runtime_dir = self.runtime_dir.clone();
        let service_type = req.service_type.clone();
        let job = self
            .engine
            .submit("svc.up", move |p| async move {
                p.update(30, &format!("starting {service_type} service"));
                let msg = blocking(move || {
                    vxedge_core::services::svc_up(&def, &vxota_bin, &runtime_dir)
                        .map_err(|e| e.to_string())
                })
                .await?;
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
        let job = self
            .engine
            .submit("svc.down", move |p| async move {
                p.update(50, &format!("stopping {service_type} service"));
                let msg = blocking(move || {
                    vxedge_core::services::svc_down(&service_type, &runtime_dir)
                        .map_err(|e| e.to_string())
                })
                .await?;
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
