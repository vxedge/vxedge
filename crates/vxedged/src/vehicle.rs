//! `VehicleService` — one verb that brings a bare vehicle (no experiment wrapping it) up or
//! down: "full software-bootstrapped SDV" (`ARCHITECTURE.md` §5, root `CLAUDE.md`'s own CLI
//! command table).
//!
//! # Composition, not reimplementation
//!
//! Same principle `ExperimentSvc` states for itself, and the same [`Handles`] type — reused
//! directly, not duplicated, since gateway/zone/service provisioning for a vehicle is *exactly*
//! `ExperimentSvc`'s own gateway/zones/services staging, run against a synthesized workspace.
//! `stage_compute` does not exist here for the same reason it does not exist in
//! `ExperimentSvc`: `ZoneService`'s own `list`/`up`/`down` already address every plannable
//! device regardless of role (`ComputeSvc`'s own doc comment: "a standalone RPC surface over
//! the same `EdgeState`/`NodePlan` machinery zones already uses, addressed by role"), so
//! `stage_zones` alone already brings up HPC-role devices too.
//!
//! # A vehicle has no network of its own
//!
//! §6.0's own rule is "the vehicle names a slice; the experiment defines it" — a bare vehicle
//! workspace (`kind: vehicle` + `kind: baseline`, no `kind: experiment`) therefore has no
//! `network.slices` for its own slice-bound links to resolve against, and `build()` in
//! `vxedge-core` hard-requires an experiment document besides. `vxedge_core::model::
//! synthesize_vehicle_workspace` closes both gaps: it wraps the vehicle in a minimal
//! synthesized `kind: experiment` document (one `mode: vlan` slice per distinct `link.slice`
//! id the vehicle references, deterministically numbered so re-running `vehicle up` reconciles
//! rather than reassigning VLANs) and returns the *augmented YAML text* — not a pre-built
//! model — so every stage below, each of which re-parses its own manifest text
//! (`GatewaySvc`/`ZoneSvc`/`SvcSvc`, via `crate::services::resolve_manifest`), sees an ordinary
//! §6.0 workspace and needs no vehicle-aware code path of its own.

use std::sync::Arc;
use tonic::{Request, Response, Status};

use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::vehicle_service_server::VehicleService;
use vxedge_api::v1::zone_service_server::ZoneService;
use vxedge_api::v1::{
    GatewayStatusRequest, ManifestRef, ResourceKind, VehicleDownRequest, VehicleStatus,
    VehicleStatusRequest, VehicleUpRequest,
};

use crate::audit::Audit;
use crate::experiment::Handles;

pub struct VehicleSvc {
    pub audit: Arc<Audit>,
    pub handles: Handles,
}

/// Resolve, synthesize and build once, so `up`/`down`/`status` share one code path for turning
/// a `ManifestRef` into (augmented YAML text, workspace dir, the built model, the vehicle's own
/// id). The model is returned alongside the YAML text it was built from — same reason
/// `ExperimentSvc::up` builds its model once and hands the *document* to every stage rather
/// than the model itself: `GatewaySvc`/`ZoneSvc`/`SvcSvc` each re-parse the text independently.
async fn resolve_and_synthesize(
    mref: Option<&ManifestRef>,
) -> Result<
    (
        String,
        Option<std::path::PathBuf>,
        vxedge_core::model::Experiment,
        String,
    ),
    Status,
> {
    let mref = mref.ok_or_else(|| {
        Status::invalid_argument(
            "no vehicle to run: the daemon received no manifest or workspace reference",
        )
    })?;
    let dir = crate::services::resolve_manifest_dir(mref).await;
    let raw_yaml = crate::services::resolve_manifest_yaml(mref).await?;
    let yaml = vxedge_core::model::synthesize_vehicle_workspace(&raw_yaml)
        .map_err(|e| Status::invalid_argument(e.to_string()))?;
    let model = vxedge_core::model::load_yaml(&yaml)
        .map_err(|e| Status::invalid_argument(e.to_string()))?;
    // Exactly one entity: `synthesize_vehicle_workspace` refuses anything but exactly one
    // `kind: vehicle` document, and the synthesized experiment names only that one.
    let vehicle_id = model
        .entities
        .first()
        .map(|e| e.id.clone())
        .ok_or_else(|| Status::internal("synthesized vehicle workspace built with no entity"))?;
    Ok((yaml, dir, model, vehicle_id))
}

#[tonic::async_trait]
impl VehicleService for VehicleSvc {
    async fn up(
        &self,
        req: Request<VehicleUpRequest>,
    ) -> Result<Response<vxedge_api::v1::Job>, Status> {
        let (yaml, dir, model, id) =
            resolve_and_synthesize(req.get_ref().manifest.as_ref()).await?;
        // Host requirements before anything is provisioned, same as `ExperimentSvc::up` — see
        // that function's own comment for why refusing here costs nothing but refusing halfway
        // through does not.
        crate::services::preflight_requires(dir.as_deref())?;
        self.audit
            .record("VehicleService.Up", serde_json::json!({ "vehicle": id }));
        let svc = self.handles.clone();
        let job = self
            .handles
            .engine
            .submit("vehicle.up", move |p| async move {
                svc.stage_gateway(&yaml, dir.as_deref(), true, &p).await?;
                let zones = svc.stage_zones(true, &p).await?;
                // Always "no services declared" today: `kind: vehicle` has no `services:`
                // block of its own in the current §6.0 schema (only experiments do), so the
                // synthesized workspace never declares any — kept for parity with
                // `ExperimentSvc::up` and to pick up vehicle-scoped services automatically if
                // the schema grows one later, not because it does anything yet.
                let services = svc
                    .stage_services(&model, &yaml, dir.as_deref(), true, &p)
                    .await?;
                Ok(format!("vehicle `{id}` up — gateway, {zones}, {services}"))
            })
            .await;
        Ok(Response::new(job))
    }

    async fn down(
        &self,
        req: Request<VehicleDownRequest>,
    ) -> Result<Response<vxedge_api::v1::Job>, Status> {
        let (yaml, dir, model, id) =
            resolve_and_synthesize(req.get_ref().manifest.as_ref()).await?;
        self.audit
            .record("VehicleService.Down", serde_json::json!({ "vehicle": id }));
        let svc = self.handles.clone();
        let job = self
            .handles
            .engine
            .submit("vehicle.down", move |p| async move {
                // Reverse order, best-effort — same reasoning as `ExperimentSvc::down`: a
                // teardown that stops at the first error strands whatever came after it.
                let mut failures: Vec<String> = Vec::new();
                if let Err(e) = svc
                    .stage_services(&model, &yaml, dir.as_deref(), false, &p)
                    .await
                {
                    failures.push(e);
                }
                if let Err(e) = svc.stage_zones(false, &p).await {
                    failures.push(e);
                }
                if let Err(e) = svc.stage_gateway(&yaml, dir.as_deref(), false, &p).await {
                    failures.push(e);
                }
                if failures.is_empty() {
                    Ok(format!("vehicle `{id}` down"))
                } else {
                    Err(format!(
                        "vehicle `{id}` down with {} failure(s); the rest was torn down: {}",
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
        req: Request<VehicleStatusRequest>,
    ) -> Result<Response<VehicleStatus>, Status> {
        let (_yaml, _dir, _model, id) =
            resolve_and_synthesize(req.get_ref().manifest.as_ref()).await?;

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

        let (status, components) =
            crate::status::composite_rollup(&id, ResourceKind::Vehicle, gw_status, zones);
        Ok(Response::new(VehicleStatus {
            status: Some(status),
            components,
        }))
    }
}
