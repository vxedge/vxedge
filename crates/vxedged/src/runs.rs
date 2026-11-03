//! `RunService`: the provisioned workspace on this host, as an agent addresses it.
//!
//! A run is exactly as long-lived as `active.json`: the gateway's `up` writes it and its `down`
//! removes it, so a run survives a repeated `up` and a daemon restart. The host runs one at a time,
//! so `List` returns zero or one until the fleet layer makes it plural.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tonic::{Request, Response, Status};

use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::run_service_server::RunService;
use vxedge_api::v1::{
    GatewayStatusRequest, ResourceKind, Run, RunGetRequest, RunListRequest, RunListResponse,
};

use crate::services::{ActiveRecord, GatewaySvc, active_run, read_workspace};

pub struct RunSvc {
    pub gateway: Arc<GatewaySvc>,
    pub runtime_dir: PathBuf,
}

impl RunSvc {
    /// Whether the workspace is a bare vehicle, and the id its author wrote. A vehicle's record
    /// holds the id of its synthesized experiment.
    async fn target(record: &ActiveRecord) -> (ResourceKind, String) {
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

    async fn current(&self) -> Result<Option<Run>, Status> {
        let Some(record) = active_run(&self.runtime_dir) else {
            return Ok(None);
        };
        let gateway = self
            .gateway
            .status(Request::new(GatewayStatusRequest {}))
            .await?
            .into_inner();
        let (kind, target_id) = Self::target(&record).await;
        Ok(Some(Run {
            id: record.run_id().to_string(),
            kind: kind as i32,
            target_id,
            workspace_sha256: record.sha256.unwrap_or_default(),
            state: gateway.status.unwrap_or_default().state,
            started_unix_ms: record.started_unix_ms.unwrap_or_default(),
        }))
    }
}

#[tonic::async_trait]
impl RunService for RunSvc {
    async fn get(&self, req: Request<RunGetRequest>) -> Result<Response<Run>, Status> {
        let wanted = req.into_inner().run.map(|r| r.run_id).unwrap_or_default();
        match self.current().await? {
            Some(run) if wanted.is_empty() || run.id == wanted => Ok(Response::new(run)),
            Some(_) => Err(crate::errors::not_found(
                crate::errors::R::RunMismatch,
                format!("no run `{wanted}` on this host"),
            )),
            None => Err(crate::errors::not_found(
                crate::errors::R::NoActiveRun,
                "no run is active on this host",
            )),
        }
    }

    async fn list(&self, _: Request<RunListRequest>) -> Result<Response<RunListResponse>, Status> {
        let runs = self.current().await?.into_iter().collect();
        Ok(Response::new(RunListResponse { runs }))
    }
}
