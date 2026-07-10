//! vxAPI service implementations. GatewayService is live end-to-end:
//! Apply plans, Up provisions the Linux bridge/VLAN/HTB gateway via
//! vxedge-core::gateway, Down removes it, Status reports observed state.

use sha2::Digest;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tonic::{Request, Response, Status};

use vxedge_api::v1::event_service_server::EventService;
use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::job_service_server::JobService;
use vxedge_api::v1::manifest_ref::Source;
use vxedge_api::v1::{
    Event, GatewayApplyRequest, GatewayDownRequest, GatewayStatus, GatewayStatusRequest,
    GatewayUpRequest, GetJobRequest, Job, ManifestRef, ResourceKind, ResourceState, ResourceStatus,
    SliceStatus, SubscribeRequest, WatchJobRequest,
};
use vxedge_core::ExperimentManifest;
use vxedge_core::gateway::{self, GatewayPlan, Shell};

use crate::audit::Audit;
use crate::events::EventBus;
use crate::jobs::{JobEngine, is_terminal, now_ms};

/// Resolve a ManifestRef to its YAML content + SHA-256, enforcing the
/// checksum guard when the caller supplied one.
async fn resolve_manifest(mref: Option<&ManifestRef>) -> Result<(String, String), Status> {
    let mref = mref.ok_or_else(|| {
        Status::invalid_argument("a manifest is required (pass --manifest <path> on the CLI)")
    })?;
    let yaml = match &mref.source {
        Some(Source::Path(path)) => tokio::fs::read_to_string(path).await.map_err(|e| {
            Status::invalid_argument(format!(
                "cannot read manifest `{path}` on the daemon host: {e}"
            ))
        })?,
        Some(Source::InlineYaml(yaml)) => yaml.clone(),
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
    Ok((yaml, sha256))
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
    /// Last successfully applied plan (cleared on down). The observed side of
    /// Status; lost on daemon restart, in which case Status degrades honestly.
    pub state: Arc<tokio::sync::Mutex<Option<GatewayPlan>>>,
}

impl GatewaySvc {
    async fn lifecycle_job(
        &self,
        rpc: &'static str,
        kind: &'static str,
        mref: Option<&ManifestRef>,
        verb: Verb,
    ) -> Result<Response<Job>, Status> {
        let (yaml, sha256) = resolve_manifest(mref).await?;
        self.audit
            .record(rpc, serde_json::json!({ "manifest_sha256": sha256 }));
        let shell = self.shell.clone();
        let state = self.state.clone();
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
                        let summary = plan.summary();
                        *state.lock().await = Some(plan);
                        Ok(format!("vxgateway up — {summary}"))
                    }
                    Verb::Down => {
                        p.update(60, "removing endpoints, uplink, bridge");
                        let sh = shell.clone();
                        let pl = plan.clone();
                        blocking(move || gateway::teardown(&sh, &pl).map_err(|e| e.to_string()))
                            .await?;
                        *state.lock().await = None;
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
        let plan = self.state.lock().await.clone();
        let (state, detail, slices) = match plan {
            Some(plan) => {
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
                            "up: {} slice(s), {}/{} endpoint(s)",
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
                            "degraded: bridge={} uplink={} endpoints={}/{}",
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
