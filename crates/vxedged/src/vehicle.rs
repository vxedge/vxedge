//! `VehicleService` — one verb that brings a bare vehicle (no experiment wrapping it) up or
//! down: "full software-bootstrapped SDV" (`ARCHITECTURE.md` §5, root `CLAUDE.md`'s own CLI
//! command table).
//!
//! # Composition, not reimplementation
//!
//! Same principle `ExperimentSvc` states for itself, and the same [`Handles`] type: each stage
//! submits the existing service's job and waits for it, against a synthesized workspace. The
//! order is §5.4's: gateway, zones, compute, signal plane, services, and the reverse on `down`.
//! Compute is its own stage because hpc devices go through `ComputeService`, so their audit lines
//! and `compute.up` events say what they are. A vehicle with no `signal_plane` block has no signal
//! plane stage.
//!
//! # The workspace decides, not the daemon's memory
//!
//! Both verbs plan every node from the workspace before touching the host. `up` therefore
//! refuses a workspace that cannot provision before the gateway exists, and `down` still knows
//! what to remove after a daemon restart has emptied `EdgeState`.
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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tonic::{Request, Response, Status};

use vxedge_api::v1::svc_service_server::SvcService;
use vxedge_api::v1::vehicle_service_server::VehicleService;
use vxedge_api::v1::{
    ExperimentStage, ManifestRef, ResourceKind, ResourceState, ResourceStatus, SvcStatusRequest,
    VehicleDownRequest, VehicleStatus, VehicleStatusRequest, VehicleUpRequest,
};
use vxedge_core::gateway::GatewayPlan;
use vxedge_core::model::{DeviceRole, Experiment};
use vxedge_core::zones::NodePlan;

use crate::audit::Audit;
use crate::experiment::{Handles, NodeStage, check_active_target, check_stages, inline, wants};
use crate::jobs::{JobFailure, JobOpts};

pub struct VehicleSvc {
    pub audit: Arc<Audit>,
    pub handles: Handles,
}

/// One workspace, resolved once, so `up`/`down`/`status` share one path from a `ManifestRef` to
/// what they act on. Every stage gets `yaml` rather than `model`, because
/// `GatewaySvc`/`ZoneSvc`/`SvcSvc` each parse their manifest independently.
struct Resolved {
    yaml: String,
    dir: Option<PathBuf>,
    sha256: String,
    model: Experiment,
    id: String,
}

async fn resolve_and_synthesize(mref: Option<&ManifestRef>) -> Result<Resolved, Status> {
    let mref = mref.ok_or_else(|| {
        crate::errors::invalid(
            crate::errors::R::InvalidRequest,
            "no vehicle to run: the daemon received no manifest or workspace reference",
        )
    })?;
    let (raw_yaml, sha256, dir) = crate::services::resolve_manifest(Some(mref)).await?;
    let yaml = vxedge_core::model::synthesize_vehicle_workspace(&raw_yaml)
        .map_err(|e| crate::errors::invalid(crate::errors::R::WorkspaceInvalid, e.to_string()))?;
    let model = vxedge_core::model::load_yaml(&yaml)
        .map_err(|e| crate::errors::invalid(crate::errors::R::WorkspaceInvalid, e.to_string()))?;
    // Exactly one entity: `synthesize_vehicle_workspace` refuses anything but exactly one
    // `kind: vehicle` document, and the synthesized experiment names only that one.
    let id = model
        .entities
        .first()
        .map(|e| e.id.clone())
        .ok_or_else(|| Status::internal("synthesized vehicle workspace built with no entity"))?;
    Ok(Resolved {
        yaml,
        dir,
        sha256,
        model,
        id,
    })
}

/// Every plan the vehicle needs, computed without touching the host.
fn plan_vehicle(
    model: &Experiment,
    dir: Option<&Path>,
) -> Result<(GatewayPlan, Vec<NodePlan>), Status> {
    let refuse = |e: String| crate::errors::precondition(crate::errors::R::WorkspaceInvalid, e);
    let gateway = GatewayPlan::from_model(model).map_err(|e| refuse(e.to_string()))?;
    let nodes = vxedge_core::zones::plan_nodes_from_model(
        model,
        &gateway,
        dir.unwrap_or_else(|| Path::new(".")),
    )
    .map_err(|e| refuse(e.to_string()))?;
    Ok((gateway, nodes))
}

/// Node ids by the service that addresses them: `(zones, compute)`.
fn by_stage(nodes: &[NodePlan]) -> (Vec<String>, Vec<String>) {
    let (compute, zones): (Vec<&NodePlan>, Vec<&NodePlan>) =
        nodes.iter().partition(|n| n.role == DeviceRole::Hpc);
    let ids = |plans: Vec<&NodePlan>| plans.iter().map(|n| n.node_id.clone()).collect();
    (ids(zones), ids(compute))
}

impl VehicleSvc {
    /// Every verb acts on the host's one gateway, so each refuses a workspace other than the
    /// active one. What is active comes from this daemon's memory and record first, and then from
    /// the host, which also shows a vehicle brought up by a daemon with another runtime dir.
    async fn require_active(
        &self,
        verb: &str,
        model: &Experiment,
        dir: Option<&Path>,
        nodes: &[NodePlan],
    ) -> Result<(), Status> {
        let active = self.handles.gateway.active_on_host().await;
        check_active_target(
            verb,
            active
                .as_ref()
                .map(|(id, dir)| (id.as_str(), dir.as_path())),
            &model.id,
            dir,
        )?;
        let plans = nodes.to_vec();
        let foreign =
            tokio::task::spawn_blocking(move || vxedge_core::zones::foreign_objects(&plans))
                .await
                .map_err(|e| Status::internal(format!("host scan panicked: {e}")))?;
        refuse_foreign(verb, &model.id, &foreign)
    }
}

fn refuse_foreign(verb: &str, id: &str, foreign: &[String]) -> Result<(), Status> {
    if foreign.is_empty() {
        return Ok(());
    }
    Err(crate::errors::precondition(
        crate::errors::R::ForeignWorkload,
        format!(
            "this host is running what `{id}` does not declare: {}. Another vehicle is up, possibly \
         under a daemon with a different runtime dir, and `{verb}` would act on its gateway. Run \
         it from that workspace, or bring that vehicle down first",
            foreign.join(", ")
        ),
    ))
}

#[tonic::async_trait]
impl VehicleService for VehicleSvc {
    async fn up(
        &self,
        req: Request<VehicleUpRequest>,
    ) -> Result<Response<vxedge_api::v1::Job>, Status> {
        check_stages(&req.get_ref().stages)?;
        let stages = req.get_ref().stages.clone();
        let Resolved {
            yaml,
            dir,
            sha256,
            model,
            id,
        } = resolve_and_synthesize(req.get_ref().manifest.as_ref()).await?;
        // Every check `up` makes before touching the host, shared with `WorkspaceService.Plan` so
        // the two cannot disagree. Refusing here costs nothing; refusing halfway strands a gateway.
        let (_, nodes) = plan_vehicle(&model, dir.as_deref())?;
        let pf = crate::plan::preflight(
            "vehicle up",
            &id,
            &model,
            &nodes,
            dir.as_deref(),
            &self.handles.zones.runtime_dir,
            &self.handles.gateway,
        )
        .await;
        if let Some(refusal) = pf.refusal() {
            return Err(refusal);
        }
        self.audit.record(
            "VehicleService.Up",
            serde_json::json!({
                "vehicle": id, "workspace_sha256": sha256,
                "workspace_identity_algorithm": crate::services::identity_algorithm(dir.as_deref()),
            }),
        );
        let (zones, compute) = by_stage(&nodes);
        let svc = self.handles.clone();
        let job = self
            .handles
            .engine
            .submit_with(
                JobOpts::lifecycle(
                    &req,
                    crate::experiment::staged_target(format!("vehicle {id}"), &stages),
                ),
                "vehicle.up",
                move |p| async move {
                    let mut done: Vec<String> = Vec::new();
                    p.checkpoint()?;
                    if wants(&stages, ExperimentStage::Gateway) {
                        svc.stage_gateway(&yaml, dir.as_deref(), true, &p).await?;
                        done.push("gateway".to_string());
                    }
                    p.checkpoint()?;
                    if wants(&stages, ExperimentStage::Zones) {
                        done.push(svc.stage_nodes(NodeStage::Zones, &zones, true, &p).await?);
                    }
                    p.checkpoint()?;
                    if wants(&stages, ExperimentStage::Compute) {
                        done.push(
                            svc.stage_nodes(NodeStage::Compute, &compute, true, &p)
                                .await?,
                        );
                    }
                    p.checkpoint()?;
                    if wants(&stages, ExperimentStage::SignalPlane) {
                        done.push(
                            svc.stage_signal_plane(&model, dir.as_deref(), true, &p)
                                .await?,
                        );
                    }
                    p.checkpoint()?;
                    if wants(&stages, ExperimentStage::Services) {
                        done.push(
                            svc.stage_services(&model, &yaml, dir.as_deref(), true, &p)
                                .await?,
                        );
                    }
                    Ok::<_, crate::jobs::JobFailure>(format!(
                        "vehicle `{id}` up — {}",
                        done.join(", ")
                    ))
                },
            )
            .await?;
        Ok(Response::new(job))
    }

    async fn down(
        &self,
        req: Request<VehicleDownRequest>,
    ) -> Result<Response<vxedge_api::v1::Job>, Status> {
        check_stages(&req.get_ref().stages)?;
        let stages = req.get_ref().stages.clone();
        let Resolved {
            yaml,
            dir,
            sha256,
            model,
            id,
        } = resolve_and_synthesize(req.get_ref().manifest.as_ref()).await?;
        let (gateway, nodes) = plan_vehicle(&model, dir.as_deref())?;
        self.require_active("vehicle down", &model, dir.as_deref(), &nodes)
            .await?;
        self.audit.record(
            "VehicleService.Down",
            serde_json::json!({
                "vehicle": id, "workspace_sha256": sha256,
                "workspace_identity_algorithm": crate::services::identity_algorithm(dir.as_deref()),
            }),
        );
        self.handles
            .gateway
            .adopt(model.clone(), gateway, dir.clone(), sha256)
            .await;
        let (zones, compute) = by_stage(&nodes);
        let svc = self.handles.clone();
        let job = self
            .handles
            .engine
            .submit_with(
                JobOpts::lifecycle(
                    &req,
                    crate::experiment::staged_target(format!("vehicle {id}"), &stages),
                ),
                "vehicle.down",
                move |p| async move {
                    // Reverse order, best-effort — same reasoning as `ExperimentSvc::down`: a
                    // teardown that stops at the first error strands whatever came after it.
                    let mut failures: Vec<JobFailure> = Vec::new();
                    if wants(&stages, ExperimentStage::Services)
                        && let Err(e) = svc
                            .stage_services(&model, &yaml, dir.as_deref(), false, &p)
                            .await
                    {
                        failures.push(e);
                    }
                    if wants(&stages, ExperimentStage::SignalPlane)
                        && let Err(e) = svc
                            .stage_signal_plane(&model, dir.as_deref(), false, &p)
                            .await
                    {
                        failures.push(e);
                    }
                    if wants(&stages, ExperimentStage::Compute)
                        && let Err(e) = svc
                            .stage_nodes(NodeStage::Compute, &compute, false, &p)
                            .await
                    {
                        failures.push(e);
                    }
                    if wants(&stages, ExperimentStage::Zones)
                        && let Err(e) = svc.stage_nodes(NodeStage::Zones, &zones, false, &p).await
                    {
                        failures.push(e);
                    }
                    if wants(&stages, ExperimentStage::Gateway)
                        && let Err(e) = svc.stage_gateway(&yaml, dir.as_deref(), false, &p).await
                    {
                        failures.push(e);
                    }
                    if failures.is_empty() {
                        Ok::<_, JobFailure>(format!("vehicle `{id}` down"))
                    } else {
                        Err(JobFailure::joined(
                            format!(
                                "vehicle `{id}` down with {} failure(s); the rest was torn down",
                                failures.len()
                            ),
                            failures,
                        ))
                    }
                },
            )
            .await?;
        Ok(Response::new(job))
    }

    /// What the host shows for this workspace, whatever the daemon remembers: planned from the
    /// workspace and observed through netlink, nftables, `/proc` and Podman.
    async fn status(
        &self,
        req: Request<VehicleStatusRequest>,
    ) -> Result<Response<VehicleStatus>, Status> {
        let Resolved {
            yaml,
            dir,
            model,
            id,
            ..
        } = resolve_and_synthesize(req.get_ref().manifest.as_ref()).await?;
        let (gateway, nodes) = plan_vehicle(&model, dir.as_deref())?;
        self.require_active("vehicle status", &model, dir.as_deref(), &nodes)
            .await?;

        let unattached = unprovisioned(&model, &nodes);
        let shell = self.handles.zones.shell.clone();
        let runtime_dir = self.handles.zones.runtime_dir.clone();
        // Every part is observed at once: the gateway, the nodes, the services and the signal
        // plane are independent reads, and doing them in turn made this call take seconds.
        let services = async {
            let mut out = Vec::new();
            for service_type in vxedge_core::services::ServiceDef::declared_types(&model) {
                let observed = self
                    .handles
                    .svcs
                    .status(Request::new(SvcStatusRequest {
                        service_type,
                        experiment: inline(&yaml, dir.as_deref()),
                        ..Default::default()
                    }))
                    .await?;
                out.push(observed.into_inner());
            }
            Ok::<_, Status>(out)
        };
        let (gw, typed, services, signal_plane) = tokio::join!(
            self.handles.gateway.observe(Some(gateway), None),
            crate::services::observed_nodes(shell, runtime_dir, nodes),
            services,
            self.handles.signal_plane_component(&model, dir.as_deref()),
        );
        let (gw, typed) = (gw?, typed?);
        let gw_status = gw.status.clone().unwrap_or_default();
        let mut components: Vec<ResourceStatus> =
            typed.iter().filter_map(|n| n.status.clone()).collect();
        components.extend(services?);
        components.extend(signal_plane?);

        let (status, mut components) =
            crate::status::composite_rollup(&id, ResourceKind::Vehicle, gw_status, components);
        // Listed after the rollup: declared but never provisioned is a fact about the workspace,
        // not a failure of the host, and counting it would leave such a vehicle forever degraded.
        components.extend(unattached);
        Ok(Response::new(VehicleStatus {
            status: Some(status),
            components,
            gateway: Some(gw),
            nodes: typed,
        }))
    }
}

/// Declared devices with no node plan: no slice-bound link, so nothing provisions them.
fn unprovisioned(model: &Experiment, nodes: &[NodePlan]) -> Vec<ResourceStatus> {
    model
        .entities
        .iter()
        .flat_map(|e| e.devices.values())
        .filter(|d| !nodes.iter().any(|n| n.node_id == d.id))
        .map(|d| ResourceStatus {
            vxid: d.id.clone(),
            kind: crate::status::node_kind(d.role) as i32,
            state: ResourceState::Unprovisioned as i32,
            detail: "declared, not provisioned: no slice-bound link".to_string(),
            observed_unix_ms: crate::jobs::now_ms(),
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = r#"
kind: vehicle
id: car
zones: { front: {}, central: {} }
devices:
  ecu1: { role: zone-controller, zone: front, backend: container }
  hpc1: { role: hpc, zone: central, backend: container }
functions:
  f-ecu: { deployed_on: ecu1 }
  f-hpc: { deployed_on: hpc1 }
links:
  - { id: l1, endpoints: [{ node: ecu1, port: eth0, ip: 10.1.0.2/24 }], slice: adas }
  - { id: l2, endpoints: [{ node: hpc1, port: eth0, ip: 10.1.0.3/24 }], slice: adas }
services:
  ota:
    id: ota-01
    type: ota
    image: { url: localhost/vxedge/ota:1, kind: container }
    on: hpc1
---
kind: baseline
id: bl
parts:
  - { ref: 'local/vxprobe:host-process', target_ids: f-ecu }
  - { ref: localhost/vxedge/probe:1, target_ids: f-hpc }
"#;

    fn model(yaml: &str) -> Experiment {
        vxedge_core::model::load_vehicle_yaml(yaml).unwrap()
    }

    #[test]
    fn hpc_devices_go_through_compute_and_the_rest_through_zones() {
        let m = model(WS);
        let (_, nodes) = plan_vehicle(&m, None).unwrap();
        assert_eq!(
            by_stage(&nodes),
            (vec!["ecu1".to_string()], vec!["hpc1".to_string()])
        );
    }

    #[test]
    fn local_images_cover_the_signal_plane() {
        let yaml = WS.replace(
            "---\nkind: baseline",
            "signal_plane:\n  kuksa:\n    image: { url: localhost/vxedge/kuksa:1, kind: container }\n    on: hpc1\n---\nkind: baseline",
        );
        let m = model(&yaml);
        let (_, nodes) = plan_vehicle(&m, None).unwrap();
        assert!(crate::plan::local_images(&m, &nodes).contains("localhost/vxedge/kuksa:1"));
    }

    #[test]
    fn local_images_cover_payloads_and_placed_services_once_each() {
        let m = model(WS);
        let (_, nodes) = plan_vehicle(&m, None).unwrap();
        assert_eq!(
            crate::plan::local_images(&m, &nodes)
                .into_iter()
                .collect::<Vec<_>>(),
            ["localhost/vxedge/ota:1", "localhost/vxedge/probe:1"]
        );
    }

    /// Reaching `up`'s job body with this would bring the gateway up and strand it.
    #[test]
    fn a_workspace_that_cannot_plan_is_refused_before_the_host_is_touched() {
        let yaml = WS.replace(
            "  - { id: l2,",
            "  - { id: l3, endpoints: [{ node: ecu1, port: eth1, ip: 10.2.0.2/24 }], slice: info }\n  - { id: l2,",
        );
        let err = plan_vehicle(&model(&yaml), None).unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("ecu1"), "{err:?}");
    }

    #[test]
    fn a_device_nothing_provisions_is_listed_and_left_out_of_the_plan() {
        let yaml = WS.replace(
            "  hpc1: { role: hpc, zone: central, backend: container }",
            "  hpc1: { role: hpc, zone: central, backend: container }\n  \
             spare: { role: ecu, zone: central, backend: container }",
        );
        let m = model(&yaml);
        let (_, nodes) = plan_vehicle(&m, None).unwrap();
        let listed = unprovisioned(&m, &nodes);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].vxid, "spare");
        assert_eq!(listed[0].state, ResourceState::Unprovisioned as i32);
    }

    #[test]
    fn a_host_with_another_vehicle_is_refused_by_name() {
        assert!(refuse_foreign("vehicle down", "vehicle-b", &[]).is_ok());
        let err =
            refuse_foreign("vehicle down", "vehicle-b", &["guest node-a".into()]).unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        for needle in ["guest node-a", "`vehicle-b`", "`vehicle down`"] {
            assert!(err.message().contains(needle), "{err:?}");
        }
    }

    #[test]
    fn hpc_nodes_report_as_compute() {
        let m = model(WS);
        let (_, nodes) = plan_vehicle(&m, None).unwrap();
        let hpc = nodes.iter().find(|n| n.node_id == "hpc1").unwrap();
        let node = crate::status::observed_node(hpc, &vxedge_core::zones::backend::NodeState::Up);
        let c = node.status.unwrap();
        assert_eq!(c.kind, ResourceKind::Compute as i32);
        assert_eq!(c.state, ResourceState::Up as i32);
    }
}
