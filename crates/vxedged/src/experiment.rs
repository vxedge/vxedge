//! `ExperimentService` — one verb that brings a whole experiment up or down.
//!
//! The RPC has been in `proto/vxedge/v1/lifecycle.proto` since phase 1 with no implementation
//! behind it; campaign scripts did the sequencing themselves, calling `vxedge up`, then driving
//! probes and load generators directly. This moves that sequencing into the daemon, where the
//! hard rule says orchestration belongs.
//!
//! # What `up` does, and what it deliberately does not
//!
//! `up` provisions the gateway (including the 5G core, when the manifest declares `s-nssai`
//! slices), then every zone, then the declared services. **It does not start traffic**, and that
//! is a design decision rather than an omission.
//!
//! `up` is required to be idempotent and reconciling — running it twice is a no-op (AGENTS.md).
//! Traffic generation is not a converged state; it is a time-bounded process with a duration.
//! "Start a 60-second load run" cannot be made idempotent, so it does not belong inside a
//! reconciling verb. It stays `vxedge traffic up`, which already exists.
//!
//! The same split is what a measurement campaign needs anyway: exp-001 sweeps offered load
//! across arms over a topology that must be identical each time. Provisioning and load are
//! different lifetimes, and collapsing them would make the sweep impossible to express.
//!
//! # Composition, not reimplementation
//!
//! Each stage submits the *existing* service's job and waits for it. Nothing about gateway,
//! zone or service provisioning is duplicated here, so a fix in one of them is a fix here too.
//! The sub-jobs stay individually visible through `JobService`, which makes a failure
//! attributable to a stage rather than to "the experiment".

use std::sync::Arc;
use tonic::{Request, Response, Status};

use vxedge_api::v1::experiment_service_server::ExperimentService;
use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::svc_service_server::SvcService;
use vxedge_api::v1::zone_service_server::ZoneService;
use vxedge_api::v1::{
    ExperimentDownRequest, ExperimentStatus, ExperimentStatusRequest, ExperimentUpRequest,
    GatewayDownRequest, GatewayStatusRequest, GatewayUpRequest, Job, JobState, ManifestRef,
    ResourceKind, ResourceState, ResourceStatus, SvcDownRequest, SvcUpRequest, VxidRef,
    ZoneDownRequest, ZoneUpRequest, manifest_ref::Source,
};

use crate::audit::Audit;
use crate::jobs::{JobEngine, Progress, is_terminal};
use crate::services::{GatewaySvc, SvcSvc, ZoneSvc};

pub struct ExperimentSvc {
    pub audit: Arc<Audit>,
    pub handles: Handles,
}

/// The sub-services a stage drives, in a form a job body can own.
///
/// `JobEngine::submit` takes a `'static` future, so the stages cannot borrow from `&self`.
/// `Arc` per service rather than cloning the services themselves: they hold the shared
/// `EdgeState`, and duplicating the struct around it would invite two handles that look
/// independent but are not.
#[derive(Clone)]
pub struct Handles {
    pub engine: JobEngine,
    pub gateway: Arc<GatewaySvc>,
    pub zones: Arc<ZoneSvc>,
    pub svcs: Arc<SvcSvc>,
}

/// Wait for a submitted sub-job to reach a terminal state.
///
/// Returns the job's own message on success and its error on failure, so the experiment job's
/// failure text names what actually broke instead of "stage 2 failed".
async fn await_job(engine: &JobEngine, job: Job) -> Result<String, String> {
    let mut rx = engine
        .watch(&job.id)
        .await
        .ok_or_else(|| format!("job {} vanished before it could be watched", job.id))?;
    loop {
        {
            let current = rx.borrow_and_update();
            if is_terminal(current.state) {
                return if current.state == JobState::Succeeded as i32 {
                    Ok(current.message.clone())
                } else {
                    Err(current.error.clone())
                };
            }
        }
        if rx.changed().await.is_err() {
            return Err(format!("job {} stopped reporting before finishing", job.id));
        }
    }
}

fn inline(yaml: &str) -> Option<ManifestRef> {
    Some(ManifestRef {
        source: Some(Source::InlineYaml(yaml.to_string())),
        sha256: String::new(),
    })
}

/// Resolve the request's manifest ref to YAML once, then hand the *same* document to every
/// stage as `inline_yaml`.
///
/// Re-resolving per stage would re-read the workspace from disk each time, so an edit made
/// mid-run could leave the gateway provisioned from one revision and the zones from another.
/// One read, one document, one experiment.
async fn resolve_once(mref: Option<&ManifestRef>) -> Result<String, Status> {
    let mref = mref.ok_or_else(|| {
        Status::invalid_argument(
            "no experiment to run: the daemon received no manifest or workspace reference",
        )
    })?;
    crate::services::resolve_manifest_yaml(mref).await
}

impl Handles {
    async fn stage_gateway(&self, yaml: &str, up: bool, p: &Progress) -> Result<String, String> {
        p.update(
            10,
            if up {
                "gateway: bridge, slices, endpoints"
            } else {
                "gateway: removing bridge, slices, endpoints"
            },
        );
        let job = if up {
            self.gateway
                .up(Request::new(GatewayUpRequest {
                    manifest: inline(yaml),
                }))
                .await
        } else {
            self.gateway
                .down(Request::new(GatewayDownRequest {
                    manifest: inline(yaml),
                }))
                .await
        }
        .map_err(|e| format!("gateway: {}", e.message()))?;
        await_job(&self.engine, job.into_inner())
            .await
            .map_err(|e| format!("gateway: {e}"))
    }

    async fn stage_zones(
        &self,
        manifest: &vxedge_core::ExperimentManifest,
        up: bool,
        p: &Progress,
    ) -> Result<String, String> {
        let total = manifest.zones.len();
        if total == 0 {
            return Ok("no zones declared".into());
        }
        for (i, zone) in manifest.zones.iter().enumerate() {
            p.update(
                40 + (i as u32 * 30) / total as u32,
                &format!(
                    "zone {}/{}: {} ({})",
                    i + 1,
                    total,
                    zone.id,
                    if up { "up" } else { "down" }
                ),
            );
            let r#ref = Some(VxidRef {
                vxid: zone.id.clone(),
            });
            let job = if up {
                self.zones.up(Request::new(ZoneUpRequest { r#ref })).await
            } else {
                self.zones
                    .down(Request::new(ZoneDownRequest { r#ref }))
                    .await
            }
            .map_err(|e| format!("zone `{}`: {}", zone.id, e.message()))?;
            await_job(&self.engine, job.into_inner())
                .await
                .map_err(|e| format!("zone `{}`: {e}", zone.id))?;
        }
        Ok(format!("{total} zone(s)"))
    }

    async fn stage_services(
        &self,
        manifest: &vxedge_core::ExperimentManifest,
        yaml: &str,
        up: bool,
        p: &Progress,
    ) -> Result<String, String> {
        // Keyed by service *type* because that is what SvcService dispatches on; the manifest's
        // own key is the type in every schema-valid document.
        let types: Vec<String> = manifest.services.keys().cloned().collect();
        if types.is_empty() {
            return Ok("no services declared".into());
        }
        for (i, ty) in types.iter().enumerate() {
            p.update(
                75 + (i as u32 * 20) / types.len() as u32,
                &format!("service {}: {}", ty, if up { "up" } else { "down" }),
            );
            let job = if up {
                self.svcs
                    .up(Request::new(SvcUpRequest {
                        service_type: ty.clone(),
                        experiment: inline(yaml),
                    }))
                    .await
            } else {
                self.svcs
                    .down(Request::new(SvcDownRequest {
                        service_type: ty.clone(),
                        experiment: inline(yaml),
                    }))
                    .await
            }
            .map_err(|e| format!("service `{ty}`: {}", e.message()))?;
            await_job(&self.engine, job.into_inner())
                .await
                .map_err(|e| format!("service `{ty}`: {e}"))?;
        }
        Ok(format!("{} service(s)", types.len()))
    }
}

#[tonic::async_trait]
impl ExperimentService for ExperimentSvc {
    async fn up(&self, req: Request<ExperimentUpRequest>) -> Result<Response<Job>, Status> {
        let yaml = resolve_once(req.get_ref().manifest.as_ref()).await?;
        let manifest = vxedge_core::ExperimentManifest::from_yaml(&yaml)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.audit.record(
            "ExperimentService.Up",
            serde_json::json!({ "experiment": manifest.experiment.id }),
        );
        let id = manifest.experiment.id.clone();
        let svc = self.handles.clone();
        let job = self
            .handles
            .engine
            .submit("experiment.up", move |p| async move {
                svc.stage_gateway(&yaml, true, &p).await?;
                let zones = svc.stage_zones(&manifest, true, &p).await?;
                let services = svc.stage_services(&manifest, &yaml, true, &p).await?;
                // Said explicitly, because an engineer expecting `up` to have started the load
                // would otherwise sit waiting for numbers that are not coming.
                Ok(format!(
                    "experiment `{id}` up — gateway, {zones}, {services}. \
                     Traffic is not started by `up`: run `vxedge traffic up`."
                ))
            })
            .await;
        Ok(Response::new(job))
    }

    async fn down(&self, req: Request<ExperimentDownRequest>) -> Result<Response<Job>, Status> {
        let yaml = resolve_once(req.get_ref().manifest.as_ref()).await?;
        let manifest = vxedge_core::ExperimentManifest::from_yaml(&yaml)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.audit.record(
            "ExperimentService.Down",
            serde_json::json!({ "experiment": manifest.experiment.id }),
        );
        let id = manifest.experiment.id.clone();
        let svc = self.handles.clone();
        let job = self
            .handles
            .engine
            .submit("experiment.down", move |p| async move {
                // Reverse order, and **best-effort**: a teardown that stops at the first error
                // strands whatever came after it. Collect failures, tear the rest down anyway,
                // and report everything that went wrong.
                let mut failures: Vec<String> = Vec::new();
                if let Err(e) = svc.stage_services(&manifest, &yaml, false, &p).await {
                    failures.push(e);
                }
                if let Err(e) = svc.stage_zones(&manifest, false, &p).await {
                    failures.push(e);
                }
                if let Err(e) = svc.stage_gateway(&yaml, false, &p).await {
                    failures.push(e);
                }
                if failures.is_empty() {
                    Ok(format!("experiment `{id}` down"))
                } else {
                    Err(format!(
                        "experiment `{id}` down with {} failure(s); the rest was torn down: {}",
                        failures.len(),
                        failures.join("; ")
                    ))
                }
            })
            .await;
        Ok(Response::new(job))
    }

    async fn status(
        &self,
        req: Request<ExperimentStatusRequest>,
    ) -> Result<Response<ExperimentStatus>, Status> {
        let yaml = resolve_once(req.get_ref().manifest.as_ref()).await?;
        let manifest = vxedge_core::ExperimentManifest::from_yaml(&yaml)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        let gw = self
            .handles
            .gateway
            .status(Request::new(GatewayStatusRequest {}))
            .await?
            .into_inner();
        let gw_status = gw.status.clone().unwrap_or_default();

        let mut resources: Vec<ResourceStatus> = vec![ResourceStatus {
            vxid: "vxgateway".into(),
            kind: ResourceKind::Gateway as i32,
            state: gw_status.state,
            detail: gw_status.detail.clone(),
            since_unix_ms: gw_status.since_unix_ms,
        }];
        resources.extend(
            self.handles
                .zones
                .list(Request::new(vxedge_api::v1::ZoneListRequest {}))
                .await
                .map(|r| r.into_inner().zones)
                .unwrap_or_default(),
        );

        // The experiment is up only when everything it declares is up. Reporting `Up` while a
        // zone is down would let a campaign start measuring an incomplete topology.
        let all_up = !resources.is_empty()
            && resources
                .iter()
                .all(|r| r.state == ResourceState::Up as i32);
        Ok(Response::new(ExperimentStatus {
            status: Some(ResourceStatus {
                vxid: manifest.experiment.id.clone(),
                kind: ResourceKind::Experiment as i32,
                state: if all_up {
                    ResourceState::Up as i32
                } else {
                    ResourceState::Degraded as i32
                },
                detail: format!("{} resource(s)", resources.len()),
                since_unix_ms: gw_status.since_unix_ms,
            }),
            components: resources,
        }))
    }
}
