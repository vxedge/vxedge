//! `MeasurementService`: an experiment's `measurements`, run inside each end's network namespace
//! (decisions X-19, X-20). `vxedge_core::measurements::run` does the work; this resolves the
//! workspace, refuses a measurement that cannot run, and wraps start and stop in jobs.

use std::path::PathBuf;
use std::sync::Arc;

use tonic::{Request, Response, Status};

use vxedge_api::v1::measurement_service_server::MeasurementService;
use vxedge_api::v1::{
    Job, ManifestRef, MeasurementStartRequest, MeasurementState, MeasurementStatus,
    MeasurementStatusRequest, MeasurementStatusResponse, MeasurementStopRequest,
};
use vxedge_core::containers::LazyClient;
use vxedge_core::gateway::{GatewayPlan, Shell};
use vxedge_core::measurements::run::{self, Placement};

use crate::audit::Audit;
use crate::errors::{self, R};
use crate::jobs::JobEngine;
use crate::services::{require_run, resolve_manifest};

pub struct MeasurementSvc {
    pub engine: JobEngine,
    pub audit: Arc<Audit>,
    pub shell: Shell,
    pub runtime_dir: PathBuf,
    pub vxprobe_bin: String,
    pub containers: LazyClient,
    /// The part cache, where a published guest bundle's `meta.json` declares its reflector.
    pub cache_dir: PathBuf,
}

/// A workspace's measurements, placed: what `Start` runs and `Stop` and `Status` name.
struct Resolved {
    model: vxedge_core::model::Experiment,
    placements: Vec<Placement>,
    sha256: String,
}

impl MeasurementSvc {
    async fn resolve(&self, mref: Option<&ManifestRef>) -> Result<Resolved, Status> {
        let (yaml, sha256, dir) = resolve_manifest(mref).await?;
        let invalid = |e: String| errors::invalid(R::WorkspaceInvalid, e);
        let model = vxedge_core::model::load_yaml(&yaml).map_err(|e| invalid(e.to_string()))?;
        if let Some(p) = vxedge_core::measurements::problems(&model).first() {
            return Err(errors::invalid(R::MeasurementInvalid, p.to_string()));
        }
        let gateway = GatewayPlan::from_model(&model, crate::services::RUN_SLOT)
            .map_err(|e| invalid(format!("gateway plan: {e}")))?;
        let workspace = dir.unwrap_or_else(|| PathBuf::from("."));
        let nodes = vxedge_core::zones::plan_nodes_from_model(&model, &gateway, &workspace)
            .map_err(|e| invalid(format!("node plan: {e}")))?;
        let placements = run::plan(&model, &gateway, &nodes, &self.cache_dir)
            .map_err(|e| errors::invalid(R::MeasurementInvalid, e))?;
        Ok(Resolved {
            model,
            placements,
            sha256,
        })
    }
}

/// The placements `ids` names, or all of them when it names none.
fn select(placements: Vec<Placement>, ids: &[String]) -> Result<Vec<Placement>, Status> {
    if let Some(unknown) = ids
        .iter()
        .find(|id| !placements.iter().any(|p| &p.id == *id))
    {
        return Err(errors::invalid(
            R::InvalidRequest,
            format!("no measurement `{unknown}` in the workspace"),
        ));
    }
    Ok(placements
        .into_iter()
        .filter(|p| ids.is_empty() || ids.contains(&p.id))
        .collect())
}

#[tonic::async_trait]
impl MeasurementService for MeasurementSvc {
    async fn start(&self, req: Request<MeasurementStartRequest>) -> Result<Response<Job>, Status> {
        require_run(req.get_ref().run.as_ref(), &self.runtime_dir)?;
        let opts = crate::jobs::JobOpts::request(&req, &req.get_ref().request_id);
        let req = req.into_inner();
        let resolved = self.resolve(req.manifest.as_ref()).await?;
        self.audit.record(
            "MeasurementService.Start",
            serde_json::json!({ "manifest_sha256": resolved.sha256, "ids": req.ids }),
        );
        let duration_s = match req.duration_s {
            0 => u64::from(resolved.model.method.duration_s.unwrap_or(10)),
            d => u64::from(d),
        };
        let selected = select(resolved.placements, &req.ids)?;
        let (shell, containers, runtime_dir, bin) = (
            self.shell.clone(),
            self.containers.clone(),
            self.runtime_dir.clone(),
            self.vxprobe_bin.clone(),
        );
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit_with(opts, "measurement.start", move |p| async move {
                let total = selected.len().max(1);
                let mut messages = Vec::with_capacity(selected.len());
                for (i, placement) in selected.iter().enumerate() {
                    p.update(
                        ((i * 100) / total) as u32,
                        &format!("starting measurement `{}`", placement.id),
                    );
                    let msg = run::start(
                        &shell,
                        &containers,
                        &runtime_dir,
                        &bin,
                        placement,
                        duration_s,
                    )
                    .await?;
                    events.emit(
                        crate::events::EventKind::MeasurementStarted,
                        &placement.id,
                        &msg,
                    );
                    messages.push(msg);
                }
                p.update(100, "measurements started");
                Ok::<_, crate::jobs::JobFailure>(messages.join("; "))
            })
            .await?;
        Ok(Response::new(job))
    }

    async fn stop(&self, req: Request<MeasurementStopRequest>) -> Result<Response<Job>, Status> {
        require_run(req.get_ref().run.as_ref(), &self.runtime_dir)?;
        let req = req.into_inner();
        let resolved = self.resolve(req.manifest.as_ref()).await?;
        self.audit.record(
            "MeasurementService.Stop",
            serde_json::json!({ "manifest_sha256": resolved.sha256, "ids": req.ids }),
        );
        let ids: Vec<String> = select(resolved.placements, &req.ids)?
            .into_iter()
            .map(|p| p.id)
            .collect();
        let (shell, runtime_dir) = (self.shell.clone(), self.runtime_dir.clone());
        let events = self.engine.events().clone();
        let job = self
            .engine
            .submit("measurement.stop", move |p| async move {
                let stopped = tokio::task::spawn_blocking(move || {
                    ids.iter()
                        .map(|id| (id.clone(), run::stop(&shell, &runtime_dir, id)))
                        .collect::<Vec<_>>()
                })
                .await
                .map_err(|e| format!("stop task panicked: {e}"))?;
                let mut messages = Vec::with_capacity(stopped.len());
                for (id, msg) in stopped {
                    events.emit(crate::events::EventKind::MeasurementStopped, &id, &msg);
                    messages.push(msg);
                }
                p.update(100, "measurements stopped");
                Ok::<_, crate::jobs::JobFailure>(messages.join("; "))
            })
            .await;
        Ok(Response::new(job))
    }

    async fn status(
        &self,
        req: Request<MeasurementStatusRequest>,
    ) -> Result<Response<MeasurementStatusResponse>, Status> {
        require_run(req.get_ref().run.as_ref(), &self.runtime_dir)?;
        let resolved = self.resolve(req.get_ref().manifest.as_ref()).await?;
        let kinds: Vec<(String, String)> = resolved
            .model
            .measurements
            .iter()
            .map(|m| (m.id.clone(), m.kind.as_str().to_string()))
            .collect();
        let measurements = resolved
            .placements
            .into_iter()
            .map(|p| {
                use run::State as S;
                let (state, detail) = match run::status(&self.runtime_dir, &p.id) {
                    S::NotStarted => (MeasurementState::Unspecified, "not started".to_string()),
                    S::Running => (MeasurementState::Running, String::new()),
                    S::Finished => (MeasurementState::Finished, String::new()),
                    S::Failed(why) => (MeasurementState::Failed, why),
                    S::Unknown => (
                        MeasurementState::Unspecified,
                        "stopped, and no exit status was recorded: it outlived the daemon that \
                         started it"
                            .to_string(),
                    ),
                };
                MeasurementStatus {
                    kind: kinds
                        .iter()
                        .find(|(id, _)| id == &p.id)
                        .map(|(_, k)| k.clone())
                        .unwrap_or_default(),
                    id: p.id,
                    entity: p.entity,
                    state: state as i32,
                    detail,
                }
            })
            .collect();
        Ok(Response::new(MeasurementStatusResponse { measurements }))
    }
}
