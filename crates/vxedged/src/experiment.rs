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
    ExperimentDownRequest, ExperimentStage, ExperimentStatus, ExperimentStatusRequest,
    ExperimentUpRequest, GatewayDownRequest, GatewayStatusRequest, GatewayUpRequest, Job, JobState,
    ManifestRef, ResourceKind, ResourceStatus, SvcDownRequest, SvcUpRequest, VxidRef,
    ZoneDownRequest, ZoneListRequest, ZoneUpRequest, manifest_ref::Source,
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

/// `dir` is the workspace `resolve_once` (or the caller's own preflight resolution) already
/// read this document from — carried alongside the YAML so a sub-RPC can resolve local
/// bundle/part refs against the real workspace instead of the daemon's own cwd (see
/// `ManifestRef.resolved_dir`'s own doc comment, common.proto). `None` for a genuinely-inline
/// manifest (no workspace to name).
fn inline(yaml: &str, dir: Option<&std::path::Path>) -> Option<ManifestRef> {
    Some(ManifestRef {
        source: Some(Source::InlineYaml(yaml.to_string())),
        sha256: String::new(),
        resolved_dir: dir.map(|d| d.display().to_string()).unwrap_or_default(),
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
    async fn stage_gateway(
        &self,
        yaml: &str,
        dir: Option<&std::path::Path>,
        up: bool,
        p: &Progress,
    ) -> Result<String, String> {
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
                    manifest: inline(yaml, dir),
                }))
                .await
        } else {
            self.gateway
                .down(Request::new(GatewayDownRequest {
                    manifest: inline(yaml, dir),
                }))
                .await
        }
        .map_err(|e| format!("gateway: {}", e.message()))?;
        await_job(&self.engine, job.into_inner())
            .await
            .map_err(|e| format!("gateway: {e}"))
    }

    /// Bring every provisioned device up or down, one RPC each.
    ///
    /// The list comes from `ZoneService` rather than from the workspace, because what it can
    /// plan *is* what has to be staged: a device with no VLAN attachment has no bridge port and
    /// no node plan, and asking for it by name would fail the stage on a device that was never
    /// meant to be provisioned. Addressing devices individually rather than by region also
    /// keeps `up` from starting several guests concurrently in one call — the concurrency that
    /// invalidated exp-002b's first campaign.
    async fn stage_zones(&self, up: bool, p: &Progress) -> Result<String, String> {
        let listed = match self.zones.list(Request::new(ZoneListRequest {})).await {
            Ok(r) => r.into_inner().zones,
            // No active experiment, or nothing plannable in it: both mean there is nothing to
            // stage, which is not a failure.
            Err(_) => return Ok("no zones declared".into()),
        };
        let total = listed.len();
        if total == 0 {
            return Ok("no zones declared".into());
        }
        for (i, node) in listed.iter().enumerate() {
            p.update(
                40 + (i as u32 * 30) / total as u32,
                &format!(
                    "zone {}/{}: {} ({})",
                    i + 1,
                    total,
                    node.vxid,
                    if up { "up" } else { "down" }
                ),
            );
            let r#ref = Some(VxidRef {
                vxid: node.vxid.clone(),
            });
            let job = if up {
                self.zones.up(Request::new(ZoneUpRequest { r#ref })).await
            } else {
                self.zones
                    .down(Request::new(ZoneDownRequest { r#ref }))
                    .await
            }
            .map_err(|e| format!("zone `{}`: {}", node.vxid, e.message()))?;
            await_job(&self.engine, job.into_inner())
                .await
                .map_err(|e| format!("zone `{}`: {e}", node.vxid))?;
        }
        Ok(format!("{total} zone(s)"))
    }

    async fn stage_services(
        &self,
        model: &vxedge_core::model::Experiment,
        yaml: &str,
        dir: Option<&std::path::Path>,
        up: bool,
        p: &Progress,
    ) -> Result<String, String> {
        // Keyed by service *type* because that is what SvcService dispatches on.
        let types = vxedge_core::services::ServiceDef::declared_types(model);
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
                        experiment: inline(yaml, dir),
                    }))
                    .await
            } else {
                self.svcs
                    .down(Request::new(SvcDownRequest {
                        service_type: ty.clone(),
                        experiment: inline(yaml, dir),
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

/// Which stages a request asked for. Empty means all of them, which is what every caller that
/// predates staging sends — so the default stays "the whole experiment".
fn wants(stages: &[i32], stage: ExperimentStage) -> bool {
    stages.is_empty() || stages.contains(&(stage as i32))
}

#[tonic::async_trait]
impl ExperimentService for ExperimentSvc {
    async fn up(&self, req: Request<ExperimentUpRequest>) -> Result<Response<Job>, Status> {
        // Host requirements first: refusing here costs nothing, and refusing halfway through
        // provisioning strands a partly-built topology on a host that was never going to run it.
        // Also the one resolution of the workspace directory: carried into every stage below
        // (`inline()`'s `resolved_dir`) so a local bundle/part ref resolves against the real
        // workspace instead of the daemon's own cwd once the document is re-wrapped as inline
        // YAML — found live 2026-08-04, `zone up` after `experiment up --stage gateway`.
        let dir = match req.get_ref().manifest.as_ref() {
            Some(mref) => {
                let dir = crate::services::resolve_manifest_dir(mref).await;
                crate::services::preflight_requires(dir.as_deref())?;
                dir
            }
            None => None,
        };
        let yaml = resolve_once(req.get_ref().manifest.as_ref()).await?;
        let model = vxedge_core::model::load_yaml(&yaml)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.audit.record(
            "ExperimentService.Up",
            serde_json::json!({ "experiment": model.id }),
        );
        let id = model.id.clone();
        let svc = self.handles.clone();
        let stages = req.get_ref().stages.clone();
        let job = self
            .handles
            .engine
            .submit("experiment.up", move |p| async move {
                let mut done: Vec<String> = Vec::new();
                if wants(&stages, ExperimentStage::Gateway) {
                    svc.stage_gateway(&yaml, dir.as_deref(), true, &p).await?;
                    done.push("gateway".to_string());
                }
                if wants(&stages, ExperimentStage::Zones) {
                    done.push(svc.stage_zones(true, &p).await?);
                }
                if wants(&stages, ExperimentStage::Services) {
                    done.push(
                        svc.stage_services(&model, &yaml, dir.as_deref(), true, &p)
                            .await?,
                    );
                }
                // Said explicitly, because an engineer expecting `up` to have started the load
                // would otherwise sit waiting for numbers that are not coming.
                Ok(format!(
                    "experiment `{id}` up — {}. \
                     Traffic is not started by `up`: run `vxedge traffic up`.",
                    done.join(", ")
                ))
            })
            .await;
        Ok(Response::new(job))
    }

    async fn down(&self, req: Request<ExperimentDownRequest>) -> Result<Response<Job>, Status> {
        // Same resolution `up` does (minus preflight, which only guards provisioning) — carried
        // into every stage below for the same reason: a local bundle/part ref, or the audit
        // sha256, should resolve against the real workspace, not the daemon's own cwd.
        let dir = match req.get_ref().manifest.as_ref() {
            Some(mref) => crate::services::resolve_manifest_dir(mref).await,
            None => None,
        };
        let yaml = resolve_once(req.get_ref().manifest.as_ref()).await?;
        let model = vxedge_core::model::load_yaml(&yaml)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.audit.record(
            "ExperimentService.Down",
            serde_json::json!({ "experiment": model.id }),
        );
        let id = model.id.clone();
        let svc = self.handles.clone();
        let stages = req.get_ref().stages.clone();
        let job = self
            .handles
            .engine
            .submit("experiment.down", move |p| async move {
                // Reverse order, and **best-effort**: a teardown that stops at the first error
                // strands whatever came after it. Collect failures, tear the rest down anyway,
                // and report everything that went wrong.
                let mut failures: Vec<String> = Vec::new();
                if wants(&stages, ExperimentStage::Services)
                    && let Err(e) = svc
                        .stage_services(&model, &yaml, dir.as_deref(), false, &p)
                        .await
                {
                    failures.push(e);
                }
                if wants(&stages, ExperimentStage::Zones)
                    && let Err(e) = svc.stage_zones(false, &p).await
                {
                    failures.push(e);
                }
                if wants(&stages, ExperimentStage::Gateway)
                    && let Err(e) = svc.stage_gateway(&yaml, dir.as_deref(), false, &p).await
                {
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
        let model = vxedge_core::model::load_yaml(&yaml)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        let gw = self
            .handles
            .gateway
            .status(Request::new(GatewayStatusRequest {}))
            .await?
            .into_inner();
        let gw_status = gw.status.clone().unwrap_or_default();

        let zones = self
            .handles
            .zones
            .list(Request::new(vxedge_api::v1::ZoneListRequest {}))
            .await
            .map(|r| r.into_inner().zones)
            .unwrap_or_default();

        Ok(Response::new(build_experiment_status(
            &model.id, gw_status, zones,
        )))
    }
}

/// Pure assembly of an `ExperimentStatus` from its already-fetched components — no I/O, so it
/// unit-tests directly rather than only through a live gRPC call needing real `Handles`.
/// Composite pattern (`vxedge_core::status::aggregate`): the experiment's own state is
/// *derived* from its components, never independently declared, so it can't silently disagree
/// with what's actually running underneath it. Also fixes a real edge case the old hand-rolled
/// `all_up` check got wrong: zero resources used to report `Degraded`
/// (`!resources.is_empty() && ...` is false either way an empty list gets there) — now
/// correctly `Down`, since nothing partially worked, nothing is declared at all. (The gateway
/// entry below is always present in the real handler, so that exact empty case can't occur
/// through `status()` today — kept general here since this function has no way to know that,
/// and testing the boundary directly is cheaper than trusting the caller's invariant.)
fn build_experiment_status(
    experiment_id: &str,
    gw_status: ResourceStatus,
    zones: Vec<ResourceStatus>,
) -> ExperimentStatus {
    let mut resources: Vec<ResourceStatus> = vec![ResourceStatus {
        vxid: "vxgateway".into(),
        kind: ResourceKind::Gateway as i32,
        state: gw_status.state,
        detail: gw_status.detail.clone(),
        since_unix_ms: gw_status.since_unix_ms,
    }];
    resources.extend(zones);

    let state = crate::status::aggregate_resource_states(&resources);
    ExperimentStatus {
        status: Some(ResourceStatus {
            vxid: experiment_id.to_string(),
            kind: ResourceKind::Experiment as i32,
            state: state as i32,
            detail: format!("{} resource(s)", resources.len()),
            since_unix_ms: gw_status.since_unix_ms,
        }),
        components: resources,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        vxid: &str,
        kind: ResourceKind,
        state: vxedge_api::v1::ResourceState,
    ) -> ResourceStatus {
        ResourceStatus {
            vxid: vxid.into(),
            kind: kind as i32,
            state: state as i32,
            detail: String::new(),
            since_unix_ms: 0,
        }
    }

    #[test]
    fn a_healthy_gateway_and_no_zones_is_up() {
        let gw = status(
            "vxgateway",
            ResourceKind::Gateway,
            vxedge_api::v1::ResourceState::Up,
        );
        let out = build_experiment_status("exp-a", gw, vec![]);
        assert_eq!(
            out.status.unwrap().state,
            vxedge_api::v1::ResourceState::Up as i32
        );
    }

    #[test]
    fn a_down_zone_degrades_an_otherwise_healthy_experiment() {
        let gw = status(
            "vxgateway",
            ResourceKind::Gateway,
            vxedge_api::v1::ResourceState::Up,
        );
        let zone = status(
            "node-a",
            ResourceKind::Zone,
            vxedge_api::v1::ResourceState::Down,
        );
        let out = build_experiment_status("exp-a", gw, vec![zone]);
        assert_eq!(
            out.status.unwrap().state,
            vxedge_api::v1::ResourceState::Degraded as i32
        );
    }

    #[test]
    fn a_down_gateway_and_no_zones_is_down_not_degraded() {
        // The exact bug this rollup used to have: zero-or-uniformly-down must report Down,
        // never Degraded, since nothing partially worked.
        let gw = status(
            "vxgateway",
            ResourceKind::Gateway,
            vxedge_api::v1::ResourceState::Down,
        );
        let out = build_experiment_status("exp-a", gw, vec![]);
        assert_eq!(
            out.status.unwrap().state,
            vxedge_api::v1::ResourceState::Down as i32
        );
    }

    #[test]
    fn the_gateway_entry_is_always_first_and_correctly_kinded() {
        let gw = status(
            "vxgateway",
            ResourceKind::Gateway,
            vxedge_api::v1::ResourceState::Up,
        );
        let zone = status(
            "node-a",
            ResourceKind::Zone,
            vxedge_api::v1::ResourceState::Up,
        );
        let out = build_experiment_status("exp-a", gw, vec![zone]);
        assert_eq!(out.components.len(), 2);
        assert_eq!(out.components[0].vxid, "vxgateway");
        assert_eq!(out.components[0].kind, ResourceKind::Gateway as i32);
        assert_eq!(out.components[1].vxid, "node-a");
        assert_eq!(out.components[1].kind, ResourceKind::Zone as i32);
    }

    #[test]
    fn the_experiment_status_carries_the_supplied_id_and_kind() {
        let gw = status(
            "vxgateway",
            ResourceKind::Gateway,
            vxedge_api::v1::ResourceState::Up,
        );
        let out = build_experiment_status("exp-a", gw, vec![]);
        let s = out.status.unwrap();
        assert_eq!(s.vxid, "exp-a");
        assert_eq!(s.kind, ResourceKind::Experiment as i32);
    }
}
