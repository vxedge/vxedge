//! vxAPI service implementations. Scaffold: stubs only — the reconciler and
//! job engine replace these as TASKS.md §1–2 land.

use tonic::{Request, Response, Status};

use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::job_service_server::JobService;
use vxedge_api::v1::{
    GatewayApplyRequest, GatewayDownRequest, GatewayStatus, GatewayStatusRequest, GatewayUpRequest,
    GetJobRequest, Job, ResourceKind, ResourceState, ResourceStatus, WatchJobRequest,
};

pub struct GatewayStub;

#[tonic::async_trait]
impl GatewayService for GatewayStub {
    async fn apply(&self, _: Request<GatewayApplyRequest>) -> Result<Response<Job>, Status> {
        Err(Status::unimplemented(
            "GatewayService.Apply: reconciler not implemented yet",
        ))
    }

    async fn up(&self, _: Request<GatewayUpRequest>) -> Result<Response<Job>, Status> {
        Err(Status::unimplemented(
            "GatewayService.Up: vxgateway backend not implemented yet",
        ))
    }

    async fn down(&self, _: Request<GatewayDownRequest>) -> Result<Response<Job>, Status> {
        Err(Status::unimplemented(
            "GatewayService.Down: vxgateway backend not implemented yet",
        ))
    }

    async fn status(
        &self,
        _: Request<GatewayStatusRequest>,
    ) -> Result<Response<GatewayStatus>, Status> {
        // Scaffold placeholder: no gateway is provisioned, report DOWN honestly.
        Ok(Response::new(GatewayStatus {
            status: Some(ResourceStatus {
                vxid: "vxgateway".into(),
                kind: ResourceKind::Gateway as i32,
                state: ResourceState::Down as i32,
                detail: "scaffold: no backend provisioned".into(),
                since_unix_ms: 0,
            }),
            slices: vec![],
        }))
    }
}

pub struct JobStub;

#[tonic::async_trait]
impl JobService for JobStub {
    type WatchStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<Job, Status>> + Send>>;

    async fn get(&self, _: Request<GetJobRequest>) -> Result<Response<Job>, Status> {
        Err(Status::unimplemented(
            "JobService.Get: job engine not implemented yet",
        ))
    }

    async fn watch(
        &self,
        _: Request<WatchJobRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        Err(Status::unimplemented(
            "JobService.Watch: job engine not implemented yet",
        ))
    }
}
