//! vxAPI service implementations. Job engine + event bus are live; the
//! gateway provisioning backend is still pending (TASKS.md §2), so Up/Down
//! validate the manifest and then fail honestly at the provisioning step,
//! while Apply (validate + plan) succeeds.

use sha2::Digest;
use std::pin::Pin;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tonic::{Request, Response, Status};

use vxedge_api::v1::event_service_server::EventService;
use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::job_service_server::JobService;
use vxedge_api::v1::manifest_ref::Source;
use vxedge_api::v1::{
    Event, GatewayApplyRequest, GatewayDownRequest, GatewayStatus, GatewayStatusRequest,
    GatewayUpRequest, GetJobRequest, Job, ManifestRef, ResourceKind, ResourceState, ResourceStatus,
    SubscribeRequest, WatchJobRequest,
};
use vxedge_core::ExperimentManifest;

use crate::audit::Audit;
use crate::events::EventBus;
use crate::jobs::{JobEngine, is_terminal};

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

pub struct GatewaySvc {
    pub engine: JobEngine,
    pub audit: std::sync::Arc<Audit>,
}

impl GatewaySvc {
    /// Shared body for Apply/Up/Down: parse + validate, then either stop at
    /// the plan (Apply) or attempt provisioning (which is not built yet).
    async fn lifecycle_job(
        &self,
        rpc: &'static str,
        kind: &'static str,
        mref: Option<&ManifestRef>,
        provision: bool,
    ) -> Result<Response<Job>, Status> {
        let (yaml, sha256) = resolve_manifest(mref).await?;
        self.audit
            .record(rpc, serde_json::json!({ "manifest_sha256": sha256 }));
        let job = self
            .engine
            .submit(kind, move |p| async move {
                p.update(10, "parsing manifest");
                let manifest = ExperimentManifest::from_yaml(&yaml).map_err(|e| e.to_string())?;
                let slices = manifest
                    .network
                    .as_ref()
                    .map(|n| n.slices.len())
                    .unwrap_or(0);
                p.update(
                    40,
                    &format!(
                        "manifest `{}` valid: {slices} slice(s), {} zone(s), {} compute node(s)",
                        manifest.experiment.id,
                        manifest.zones.len(),
                        manifest.compute.len()
                    ),
                );
                if !provision {
                    return Ok(format!(
                        "manifest `{}` validated and planned ({slices} slice(s)); nothing started (apply)",
                        manifest.experiment.id
                    ));
                }
                p.update(60, "provisioning vxgateway");
                Err(
                    "vxgateway provisioning backend not implemented yet (vxedge/TASKS.md §2); \
                     manifest validated, no changes made"
                        .to_string(),
                )
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
            false,
        )
        .await
    }

    async fn up(&self, req: Request<GatewayUpRequest>) -> Result<Response<Job>, Status> {
        self.lifecycle_job(
            "GatewayService.Up",
            "gateway.up",
            req.get_ref().manifest.as_ref(),
            true,
        )
        .await
    }

    async fn down(&self, req: Request<GatewayDownRequest>) -> Result<Response<Job>, Status> {
        self.lifecycle_job(
            "GatewayService.Down",
            "gateway.down",
            req.get_ref().manifest.as_ref(),
            true,
        )
        .await
    }

    async fn status(
        &self,
        _: Request<GatewayStatusRequest>,
    ) -> Result<Response<GatewayStatus>, Status> {
        // Honest placeholder until the reconciler observes real state.
        Ok(Response::new(GatewayStatus {
            status: Some(ResourceStatus {
                vxid: "vxgateway".into(),
                kind: ResourceKind::Gateway as i32,
                state: ResourceState::Down as i32,
                detail: "no gateway provisioned (backend pending)".into(),
                since_unix_ms: 0,
            }),
            slices: vec![],
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
