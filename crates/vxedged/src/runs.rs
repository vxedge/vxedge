//! `RunService`: the provisioned workspace on this host, as an agent addresses it.
//!
//! A run is exactly as long-lived as `active.json`: the gateway's `up` writes it and its `down`
//! removes it, so a run survives a repeated `up` and a daemon restart. The host runs one at a time,
//! so `List` returns zero or one until the fleet layer makes it plural.
//!
//! A run's state is observed, not remembered: `UP` only when the gateway and every node it
//! provisions are up. A half-built run, for example after a daemon stopped in the middle of `up`,
//! reads `DEGRADED` until an `up` or `down` converges it, and `STAGING` while one runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tonic::{Request, Response, Status};

use vxedge_api::v1::experiment_service_server::ExperimentService;
use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::manifest_ref::Source;
use vxedge_api::v1::run_service_server::RunService;
use vxedge_api::v1::vehicle_service_server::VehicleService;
use vxedge_api::v1::{
    ExperimentStatusRequest, GatewayStatusRequest, Kernel, ManifestRef, PackageRef, ResourceKind,
    ResourceState, Run, RunGetRequest, RunListRequest, RunListResponse, RunRef,
    VehicleStatusRequest,
};

use crate::experiment::ExperimentSvc;
use crate::services::{ActiveRecord, GatewaySvc, read_workspace};
use crate::vehicle::VehicleSvc;

/// The run's state from what is observed. `STAGING` while a lifecycle job holds the host; the
/// gateway's own state unless it is up; then the workspace's status, which rolls up every zone,
/// compute node, service and signal plane it declares, so a half-built run reads `DEGRADED`.
pub(crate) fn run_state(gateway: i32, workspace: Option<i32>, busy: bool) -> i32 {
    if busy {
        return ResourceState::Staging as i32;
    }
    if gateway != ResourceState::Up as i32 {
        return gateway;
    }
    // A status that cannot be read leaves the gateway's state: the run exists either way.
    workspace.unwrap_or(gateway)
}

/// Whether the workspace is a bare vehicle, and the id its author wrote. A vehicle's record
/// holds the id of its synthesized experiment.
pub(crate) async fn target(record: &ActiveRecord) -> (ResourceKind, String) {
    let experiment = (ResourceKind::Experiment, record.id.clone());
    if record.dir == Path::new(".") {
        return experiment;
    }
    let Ok(yaml) = read_workspace(&record.dir.display().to_string()).await else {
        return experiment;
    };
    match vxedge_core::Workspace::from_yaml(&yaml) {
        Ok(ws) if ws.experiments.is_empty() => match ws.vehicles.first() {
            Some(vehicle) => (ResourceKind::Vehicle, vehicle.id.clone()),
            None => experiment,
        },
        _ => experiment,
    }
}

pub struct RunSvc {
    pub gateway: Arc<GatewaySvc>,
    pub experiments: Arc<ExperimentSvc>,
    pub vehicles: Arc<VehicleSvc>,
    pub runtime_dir: PathBuf,
}

impl RunSvc {
    /// The gateway's state and the workspace's rollup, from one workspace status read: vehicle and
    /// experiment status both carry the gateway, so it is not observed twice.
    async fn observe(
        &self,
        record: &ActiveRecord,
        kind: ResourceKind,
    ) -> Result<(i32, Option<i32>), Status> {
        let run_ref = RunRef {
            run_id: record.run_id().to_string(),
        };
        let manifest = (record.dir != Path::new(".")).then(|| ManifestRef {
            source: Some(Source::Path(record.dir.display().to_string())),
            ..Default::default()
        });
        let read = match (manifest, kind) {
            (None, _) => None,
            (Some(m), ResourceKind::Vehicle) => self
                .vehicles
                .status(Request::new(VehicleStatusRequest {
                    run: Some(run_ref.clone()),
                    manifest: Some(m),
                }))
                .await
                .ok()
                .map(|r| {
                    let r = r.into_inner();
                    (r.gateway.and_then(|g| g.status), r.status)
                }),
            (Some(m), _) => self
                .experiments
                .status(Request::new(ExperimentStatusRequest {
                    run: Some(run_ref.clone()),
                    manifest: Some(m),
                }))
                .await
                .ok()
                .map(|r| {
                    let r = r.into_inner();
                    (r.gateway.and_then(|g| g.status), r.status)
                }),
        };
        let (gateway, workspace) = read.unwrap_or((None, None));
        let gateway = match gateway {
            Some(g) => g.state,
            None => {
                self.gateway
                    .status(Request::new(GatewayStatusRequest { run: Some(run_ref) }))
                    .await?
                    .into_inner()
                    .status
                    .unwrap_or_default()
                    .state
            }
        };
        Ok((gateway, workspace.map(|s| s.state)))
    }

    /// Every run on the host, lowest slot first.
    async fn all(&self) -> Result<Vec<Run>, Status> {
        let mut runs = Vec::new();
        for (instance, record) in crate::scope::recorded(&self.runtime_dir) {
            runs.push(self.read(instance, record).await?);
        }
        Ok(runs)
    }

    async fn read(
        &self,
        instance: vxedge_core::instance::Instance,
        record: ActiveRecord,
    ) -> Result<Run, Status> {
        let (kind, target_id) = target(&record).await;
        let busy = self.gateway.engine.lifecycle_running().await;
        // While a job holds the host the answer is `STAGING` whatever is observed, so skip the
        // observation: it is the slow part of this call.
        let (gateway_state, workspace) = if busy {
            (ResourceState::Staging as i32, None)
        } else {
            self.observe(&record, kind).await?
        };
        Ok(Run {
            instance: instance.to_string(),
            id: record.run_id().to_string(),
            kind: kind as i32,
            target_id,
            workspace_identity_algorithm: record.identity_algorithm().to_string(),
            package_locked: record.package.as_ref().is_some_and(|p| p.locked),
            package: record.package.map(|p| PackageRef {
                id: p.id,
                version: p.version,
                digest: p.digest,
            }),
            kernel: record.kernel.map(|k| Kernel {
                release: k.release,
                preempt_rt: k.preempt_rt,
            }),
            shaping: record.shaping.unwrap_or_default(),
            time_sync: record.time_sync.unwrap_or_default(),
            workspace_sha256: record.sha256.unwrap_or_default(),
            state: run_state(gateway_state, workspace, busy),
            started_unix_ms: record.started_unix_ms.unwrap_or_default(),
        })
    }
}

#[tonic::async_trait]
impl RunService for RunSvc {
    async fn get(&self, req: Request<RunGetRequest>) -> Result<Response<Run>, Status> {
        let wanted = req.into_inner().run.map(|r| r.run_id).unwrap_or_default();
        let runs = self.all().await?;
        if runs.is_empty() {
            return Err(crate::errors::not_found(
                crate::errors::R::NoActiveRun,
                "no run is active on this host",
            ));
        }
        if wanted.is_empty() {
            return match <[Run; 1]>::try_from(runs) {
                Ok([run]) => Ok(Response::new(run)),
                Err(runs) => Err(crate::errors::precondition(
                    crate::errors::R::RunAmbiguous,
                    format!(
                        "{} runs are up, so name one: {}",
                        runs.len(),
                        runs.iter()
                            .map(|r| format!("`{}` (slot {})", r.id, r.instance))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )),
            };
        }
        runs.into_iter()
            .find(|r| r.id == wanted)
            .map(Response::new)
            .ok_or_else(|| {
                crate::errors::not_found(
                    crate::errors::R::RunMismatch,
                    format!("no run `{wanted}` on this host"),
                )
            })
    }

    async fn list(&self, _: Request<RunListRequest>) -> Result<Response<RunListResponse>, Status> {
        let runs = self.all().await?;
        Ok(Response::new(RunListResponse { runs }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_reads_the_workspace_rollup_once_the_gateway_is_up() {
        let up = ResourceState::Up as i32;
        let degraded = ResourceState::Degraded as i32;
        assert_eq!(run_state(up, Some(up), false), up);
        assert_eq!(
            run_state(up, Some(degraded), false),
            degraded,
            "a half-built run is not up"
        );
        assert_eq!(
            run_state(up, Some(degraded), true),
            ResourceState::Staging as i32,
            "a run with a lifecycle job in flight is staging"
        );
        let down = ResourceState::Down as i32;
        assert_eq!(run_state(down, Some(up), false), down);
        assert_eq!(
            run_state(up, None, false),
            up,
            "no readable status keeps the gateway's"
        );
    }
}
