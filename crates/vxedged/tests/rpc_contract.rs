//! vxAPI contract tests: a real `vxedged` over a real Unix socket, spoken to through the generated
//! clients. Nothing here needs privileges, so it runs in plain `cargo test`.
//!
//! A vehicle that survives a daemon restart is simulated by writing the `active.json` a previous
//! daemon would have left. Its `bridge` is `lo`, a link every host has, so the record counts as
//! live without creating anything. No test provisions the host.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use vxedge_api::v1::compute_service_client::ComputeServiceClient;
use vxedge_api::v1::event_service_client::EventServiceClient;
use vxedge_api::v1::experiment_service_client::ExperimentServiceClient;
use vxedge_api::v1::job_service_client::JobServiceClient;
use vxedge_api::v1::nos_check_service_client::NosCheckServiceClient;
use vxedge_api::v1::run_service_client::RunServiceClient;
use vxedge_api::v1::svc_service_client::SvcServiceClient;
use vxedge_api::v1::traffic_service_client::TrafficServiceClient;
use vxedge_api::v1::vehicle_service_client::VehicleServiceClient;
use vxedge_api::v1::zone_service_client::ZoneServiceClient;
use vxedge_api::v1::{
    ComputeListRequest, ExperimentStage, ExperimentStatusRequest, ExperimentUpRequest,
    GetJobRequest, JobState, ManifestRef, NodeBackend, NosCheckStatusRequest, PayloadKind,
    ResourceKind, ResourceState, RunGetRequest, RunListRequest, RunRef, SubscribeRequest,
    SvcDownRequest, SvcStatusRequest, SvcUpRequest, TrafficDownRequest, VehicleStatusRequest,
    VehicleUpRequest, VxidRef, ZoneListRequest, ZoneUpRequest, manifest_ref::Source,
};

const VEHICLE: &str = r#"
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
    config: { artifacts: artifacts, listen: "0.0.0.0:8080" }
---
kind: baseline
id: bl
parts:
  - { ref: 'local/vxprobe:host-process', target_ids: f-ecu }
  - { ref: localhost/vxedge/probe:1, target_ids: f-hpc }
"#;

static NEXT: AtomicU32 = AtomicU32::new(0);

fn scratch() -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("vxd-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A bare vehicle workspace: `kind: vehicle` and `kind: baseline`, no experiment.
fn vehicle_workspace() -> PathBuf {
    let dir = scratch();
    std::fs::create_dir_all(dir.join("artifacts")).unwrap();
    std::fs::write(dir.join("vehicle.yaml"), VEHICLE).unwrap();
    dir
}

/// Whether this user's rootless Podman socket answers.
fn podman_is_reachable() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| {
        std::os::unix::net::UnixStream::connect(Path::new(&dir).join("podman/podman.sock")).is_ok()
    })
}

/// [`VEHICLE`] with a signal plane on `hpc1`, in an image no host has.
fn vehicle_with_signal_plane() -> String {
    VEHICLE.replace(
        "---\nkind: baseline",
        "signal_plane:\n  kuksa:\n    image: { url: localhost/vxedge/no-such-kuksa:1, kind: \
         container }\n    on: hpc1\n---\nkind: baseline",
    )
}

fn signal_plane_workspace() -> PathBuf {
    let dir = scratch();
    std::fs::create_dir_all(dir.join("artifacts")).unwrap();
    std::fs::write(dir.join("vehicle.yaml"), vehicle_with_signal_plane()).unwrap();
    dir
}

fn signal_plane_experiment() -> PathBuf {
    let dir = scratch();
    std::fs::create_dir_all(dir.join("artifacts")).unwrap();
    // A published, digest-pinned databroker, so no local image check stands in front of the stage.
    let pinned = |r: &str| format!("{r}@sha256:{}", "b".repeat(64));
    let vehicle = vehicle_with_signal_plane()
        .replace(
            "localhost/vxedge/no-such-kuksa:1",
            &pinned("ghcr.io/eclipse-kuksa/kuksa-databroker"),
        )
        .replace("localhost/vxedge/ota:1", &pinned("ghcr.io/vxedge/ota"))
        .replace(
            "{ ref: localhost/vxedge/probe:1, target_ids: f-hpc }",
            &format!(
                "{{ ref: ghcr.io/vxedge/probe:1, digest: 'sha256:{}', target_ids: f-hpc }}",
                "b".repeat(64)
            ),
        );
    let yaml = vxedge_core::model::synthesize_vehicle_workspace(&vehicle).unwrap();
    std::fs::write(dir.join("main.yaml"), yaml).unwrap();
    dir
}

/// What a daemon records for this vehicle: the id of its synthesized experiment.
fn vehicle_id() -> String {
    vxedge_core::model::load_vehicle_yaml(VEHICLE).unwrap().id
}

/// The same vehicle as an experiment workspace, and that experiment's id.
fn experiment_workspace() -> (PathBuf, String) {
    let dir = scratch();
    std::fs::create_dir_all(dir.join("artifacts")).unwrap();
    let yaml = vxedge_core::model::synthesize_vehicle_workspace(VEHICLE).unwrap();
    let id = vxedge_core::model::load_yaml(&yaml).unwrap().id;
    std::fs::write(dir.join("main.yaml"), yaml).unwrap();
    (dir, id)
}

fn path_ref(dir: &Path) -> Option<ManifestRef> {
    Some(ManifestRef {
        source: Some(Source::Path(dir.display().to_string())),
        ..Default::default()
    })
}

/// A `vxedged` of its own, killed on drop.
struct Daemon {
    child: Child,
    root: PathBuf,
    socket: PathBuf,
}

impl Daemon {
    fn start() -> Self {
        Self::start_with(|_| {})
    }

    /// A daemon whose runtime dir is `name` under its scratch dir.
    fn start_in(name: &str) -> Self {
        Self::start_at(name, |_| {})
    }

    /// `seed` runs on the runtime dir before the daemon starts, as a previous daemon's leftovers.
    fn start_with(seed: impl FnOnce(&Path)) -> Self {
        Self::start_at("run", seed)
    }

    fn start_at(name: &str, seed: impl FnOnce(&Path)) -> Self {
        Self::start_full(name, seed, &[])
    }

    /// A daemon with extra environment, such as a stand-in binary for one of its tools.
    fn start_env(seed: impl FnOnce(&Path), env: &[(&str, &Path)]) -> Self {
        Self::start_full("run", seed, env)
    }

    fn start_full(name: &str, seed: impl FnOnce(&Path), env: &[(&str, &Path)]) -> Self {
        let root = scratch();
        let runtime = root.join(name);
        std::fs::create_dir_all(&runtime).unwrap();
        seed(&runtime);
        let socket = root.join("d.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_vxedged"))
            .arg("--listen")
            .arg(format!("unix://{}", socket.display()))
            .arg("--runtime-dir")
            .arg(&runtime)
            .arg("--cache-dir")
            .arg(root.join("cache"))
            // Daemons run side by side here: each exporter takes a free port.
            .arg("--metrics-listen")
            .arg("127.0.0.1:0")
            .envs(env.iter().map(|(k, v)| (*k, v.as_os_str())))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("vxedged starts");
        Self {
            child,
            root,
            socket,
        }
    }

    async fn channel(&self) -> Channel {
        for _ in 0..200 {
            if self.socket.exists() {
                let path = self.socket.clone();
                let connected = Endpoint::try_from("http://vxedge.uds")
                    .unwrap()
                    .connect_with_connector(service_fn(move |_: Uri| {
                        let path = path.clone();
                        async move {
                            Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(path).await?))
                        }
                    }))
                    .await;
                if let Ok(channel) = connected {
                    return channel;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("vxedged did not start listening");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What a daemon leaves behind for a workspace that is still up.
fn write_active_record(runtime: &Path, id: &str, dir: &Path, sha256: Option<&str>) {
    let mut record = serde_json::json!({ "id": id, "dir": dir, "bridge": "lo" });
    if let Some(sha) = sha256 {
        record["sha256"] = sha.into();
    }
    std::fs::write(runtime.join("active.json"), record.to_string()).unwrap();
}

async fn wait_for_job(channel: Channel, id: String) -> i32 {
    let mut jobs = JobServiceClient::new(channel);
    for _ in 0..400 {
        let job = jobs
            .get(GetJobRequest { id: id.clone() })
            .await
            .unwrap()
            .into_inner();
        if job.state != JobState::Pending as i32 && job.state != JobState::Running as i32 {
            return job.state;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} never finished");
}

#[tokio::test]
async fn the_daemon_serves_a_status_call() {
    let daemon = Daemon::start();
    let status = SvcServiceClient::new(daemon.channel().await)
        .status(SvcStatusRequest {
            service_type: "ota".into(),
            experiment: None,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.vxid, "svc-ota");
    assert_eq!(status.state, ResourceState::Down as i32);
}

#[tokio::test]
async fn svc_reads_a_bare_vehicle_workspace() {
    let daemon = Daemon::start();
    let ws = vehicle_workspace();
    let mut svc = SvcServiceClient::new(daemon.channel().await);
    let status = svc
        .status(SvcStatusRequest {
            service_type: "ota".into(),
            experiment: path_ref(&ws),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        status.detail.contains("on `hpc1`"),
        "a placed service must be read as placed: {}",
        status.detail
    );
    let up = svc
        .up(SvcUpRequest {
            service_type: "ota".into(),
            experiment: path_ref(&ws),
            ..Default::default()
        })
        .await;
    assert!(up.is_ok(), "a bare vehicle workspace is valid: {up:?}");
}

#[tokio::test]
async fn svc_status_with_no_workspace_uses_the_active_one() {
    let ws = vehicle_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &vehicle_id(), &ws, None));
    let status = SvcServiceClient::new(daemon.channel().await)
        .status(SvcStatusRequest {
            service_type: "ota".into(),
            experiment: None,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        status.detail.contains("on `hpc1`"),
        "the active vehicle places this service: {}",
        status.detail
    );
}

#[tokio::test]
async fn experiment_status_refuses_a_workspace_that_is_not_active() {
    let (active, id) = experiment_workspace();
    let (other, _) = experiment_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &id, &active, None));
    let refused = ExperimentServiceClient::new(daemon.channel().await)
        .status(ExperimentStatusRequest {
            manifest: path_ref(&other),
        })
        .await
        .unwrap_err();
    assert_eq!(
        refused.code(),
        tonic::Code::FailedPrecondition,
        "{refused:?}"
    );
}

#[tokio::test]
async fn experiment_status_reports_services() {
    let (active, id) = experiment_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &id, &active, None));
    let status = ExperimentServiceClient::new(daemon.channel().await)
        .status(ExperimentStatusRequest {
            manifest: path_ref(&active),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        status
            .components
            .iter()
            .any(|c| c.kind == ResourceKind::Service as i32 && c.vxid == "svc-ota"),
        "{:?}",
        status.components
    );
}

#[tokio::test]
async fn zone_and_compute_verbs_work_after_a_daemon_restart() {
    let ws = vehicle_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &vehicle_id(), &ws, None));
    let channel = daemon.channel().await;
    let zones = ZoneServiceClient::new(channel.clone())
        .list(ZoneListRequest::default())
        .await
        .unwrap()
        .into_inner()
        .zones;
    let ids: Vec<&str> = zones.iter().map(|z| z.vxid.as_str()).collect();
    assert_eq!(ids, ["ecu1"]);
    let nodes = ComputeServiceClient::new(channel)
        .list(ComputeListRequest::default())
        .await
        .unwrap()
        .into_inner()
        .nodes;
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].vxid, "hpc1");
}

#[tokio::test]
async fn a_workspace_edited_since_up_is_not_restored() {
    let ws = vehicle_workspace();
    let daemon =
        Daemon::start_with(|run| write_active_record(run, &vehicle_id(), &ws, Some("stale")));
    let refused = ZoneServiceClient::new(daemon.channel().await)
        .list(ZoneListRequest::default())
        .await
        .unwrap_err();
    assert_eq!(
        refused.code(),
        tonic::Code::FailedPrecondition,
        "{refused:?}"
    );
    assert!(refused.message().contains("changed"), "{refused:?}");
}

/// A run recorded by a daemon from before `vxws/2` holds a `vxws/1` identity and names no
/// algorithm. Upgrading the daemon under a running vehicle must not read that as an edit.
#[tokio::test]
async fn a_run_recorded_under_vxws1_is_still_restored() {
    let ws = vehicle_workspace();
    let v1 = vxedge_core::Workspace::identity_with(&ws, vxedge_core::IdentityAlgorithm::Vxws1)
        .unwrap()
        .value;
    let daemon = Daemon::start_with(|run| write_active_record(run, &vehicle_id(), &ws, Some(&v1)));
    let zones = ZoneServiceClient::new(daemon.channel().await)
        .list(ZoneListRequest::default())
        .await
        .unwrap()
        .into_inner()
        .zones;
    assert_eq!(zones.len(), 1);
    // Run reads the record the same way: the value it holds, under the rule that made it.
    let runs = RunServiceClient::new(daemon.channel().await)
        .list(RunListRequest::default())
        .await
        .unwrap()
        .into_inner()
        .runs;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].workspace_sha256, v1);
    assert_eq!(runs[0].workspace_identity_algorithm, "vxws/1");
}

#[tokio::test]
async fn hpc_devices_belong_to_compute_not_zones() {
    let ws = vehicle_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &vehicle_id(), &ws, None));
    let channel = daemon.channel().await;
    let mut zones = ZoneServiceClient::new(channel.clone());
    let listed = zones
        .list(ZoneListRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert!(
        listed
            .zones
            .iter()
            .all(|z| z.kind == ResourceKind::Zone as i32)
    );
    assert!(listed.zones.iter().all(|z| z.vxid != "hpc1"));
    let refused = zones
        .up(ZoneUpRequest {
            r#ref: Some(VxidRef {
                vxid: "hpc1".into(),
            }),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::NotFound, "{refused:?}");
    let nodes = ComputeServiceClient::new(channel)
        .list(ComputeListRequest::default())
        .await
        .unwrap()
        .into_inner()
        .nodes;
    assert_eq!(nodes[0].kind, ResourceKind::Compute as i32);
}

#[tokio::test]
async fn service_events_and_status_share_one_resource_id() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let mut events = EventServiceClient::new(channel.clone())
        .subscribe(SubscribeRequest {
            kinds: vec!["service.down".into()],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let job = SvcServiceClient::new(channel.clone())
        .down(SvcDownRequest {
            service_type: "ota".into(),
            experiment: None,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    wait_for_job(channel, job.id).await;
    let event = tokio::time::timeout(Duration::from_secs(5), events.message())
        .await
        .expect("service.down arrives")
        .unwrap()
        .unwrap();
    assert_eq!(event.resource_vxid, "svc-ota");
}

#[tokio::test]
async fn vehicle_status_reports_a_declared_signal_plane() {
    let daemon = Daemon::start();
    let ws = signal_plane_workspace();
    let status = VehicleServiceClient::new(daemon.channel().await)
        .status(VehicleStatusRequest {
            manifest: path_ref(&ws),
        })
        .await
        .unwrap()
        .into_inner();
    let broker = status
        .components
        .iter()
        .find(|c| c.vxid == "kuksa-databroker")
        .expect("a declared signal plane is a status component");
    assert_eq!(broker.kind, ResourceKind::SignalPlane as i32);
    assert_eq!(broker.state, ResourceState::Down as i32);
    assert!(broker.detail.contains("on `hpc1`"), "{}", broker.detail);
}

#[tokio::test]
async fn experiment_status_reports_a_declared_signal_plane() {
    let daemon = Daemon::start();
    let ws = signal_plane_experiment();
    let status = ExperimentServiceClient::new(daemon.channel().await)
        .status(ExperimentStatusRequest {
            manifest: path_ref(&ws),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        status
            .components
            .iter()
            .any(|c| c.vxid == "kuksa-databroker"),
        "{:?}",
        status.components
    );
}

/// Refused before `up` provisions anything: nothing recorded, so no gateway was attempted.
#[tokio::test]
async fn a_missing_local_signal_plane_image_is_refused_before_the_gateway() {
    let daemon = Daemon::start();
    let ws = signal_plane_workspace();
    let refused = VehicleServiceClient::new(daemon.channel().await)
        .up(VehicleUpRequest {
            manifest: path_ref(&ws),
            ..Default::default()
        })
        .await
        .unwrap_err();
    // Where Podman answers, the refusal names the missing image. Where it does not, the refusal
    // says Podman is unavailable, which is the truer answer there. Either way, nothing was touched.
    if podman_is_reachable() {
        assert_eq!(
            refused.code(),
            tonic::Code::FailedPrecondition,
            "{refused:?}"
        );
        assert_eq!(reason(&refused), "ERROR_REASON_LOCAL_IMAGE_MISSING");
        assert!(
            refused
                .message()
                .contains("localhost/vxedge/no-such-kuksa:1"),
            "{refused:?}"
        );
    } else {
        assert_eq!(refused.code(), tonic::Code::Unavailable, "{refused:?}");
        assert_eq!(reason(&refused), "ERROR_REASON_BACKEND_UNAVAILABLE");
    }
    assert!(
        !daemon.root.join("run/audit.jsonl").exists()
            || !std::fs::read_to_string(daemon.root.join("run/audit.jsonl"))
                .unwrap()
                .contains("GatewayService.Up")
    );
}

/// The signal plane stage alone, with no node up, so it has nothing to join. It has to fail and
/// name the databroker: an `experiment up` that skipped it would report success.
#[tokio::test]
async fn experiment_up_does_not_skip_a_declared_signal_plane() {
    let daemon = Daemon::start();
    let ws = signal_plane_experiment();
    let channel = daemon.channel().await;
    let job = ExperimentServiceClient::new(channel.clone())
        .up(ExperimentUpRequest {
            manifest: path_ref(&ws),
            stages: vec![ExperimentStage::SignalPlane as i32],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let state = wait_for_job(channel.clone(), job.id.clone()).await;
    assert_eq!(state, JobState::Failed as i32);
    let failed = JobServiceClient::new(channel)
        .get(GetJobRequest { id: job.id })
        .await
        .unwrap()
        .into_inner();
    assert!(
        failed.error.contains("kuksa-databroker"),
        "{}",
        failed.error
    );
}

/// The 0.5.0 release gate: every RPC the agent surface added is built. Each answers something other
/// than `UNIMPLEMENTED`, even to an empty request.
#[tokio::test]
async fn no_rpc_of_the_agent_surface_is_a_stub() {
    use tonic::Code::Unimplemented;
    use vxedge_api::v1::*;
    let daemon = Daemon::start();
    let ch = daemon.channel().await;
    let code = |r: Result<(), tonic::Status>| r.err().map(|s| s.code());
    let codes = [
        code(
            run_service_client::RunServiceClient::new(ch.clone())
                .list(RunListRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            job_service_client::JobServiceClient::new(ch.clone())
                .list(JobListRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            job_service_client::JobServiceClient::new(ch.clone())
                .cancel(JobCancelRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            traffic_service_client::TrafficServiceClient::new(ch.clone())
                .status(TrafficStatusRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            workspace_service_client::WorkspaceServiceClient::new(ch.clone())
                .plan(WorkspacePlanRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            host_service_client::HostServiceClient::new(ch.clone())
                .capabilities(HostCapabilitiesRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            telemetry_service_client::TelemetryServiceClient::new(ch.clone())
                .collect(CollectRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            telemetry_service_client::TelemetryServiceClient::new(ch.clone())
                .stream_metrics(StreamMetricsRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            telemetry_service_client::TelemetryServiceClient::new(ch.clone())
                .stream_records(StreamRecordsRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            log_service_client::LogServiceClient::new(ch.clone())
                .follow(LogFollowRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            measurement_service_client::MeasurementServiceClient::new(ch.clone())
                .status(MeasurementStatusRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            measurement_service_client::MeasurementServiceClient::new(ch.clone())
                .start(MeasurementStartRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            measurement_service_client::MeasurementServiceClient::new(ch.clone())
                .stop(MeasurementStopRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            package_service_client::PackageServiceClient::new(ch.clone())
                .list(PackageListRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            package_service_client::PackageServiceClient::new(ch.clone())
                .inspect(PackageInspectRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            package_service_client::PackageServiceClient::new(ch.clone())
                .pull(PackagePullRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            campaign_service_client::CampaignServiceClient::new(ch.clone())
                .plan(CampaignPlanRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            campaign_service_client::CampaignServiceClient::new(ch.clone())
                .start(CampaignStartRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            campaign_service_client::CampaignServiceClient::new(ch.clone())
                .get(CampaignGetRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            campaign_service_client::CampaignServiceClient::new(ch.clone())
                .list(CampaignListRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            campaign_service_client::CampaignServiceClient::new(ch.clone())
                .watch(CampaignWatchRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            result_service_client::ResultServiceClient::new(ch.clone())
                .list(ResultListRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            result_service_client::ResultServiceClient::new(ch.clone())
                .get(ResultGetRequest::default())
                .await
                .map(|_| ()),
        ),
        code(
            result_service_client::ResultServiceClient::new(ch)
                .fetch(ResultFetchRequest::default())
                .await
                .map(|_| ()),
        ),
    ];
    assert!(codes.iter().all(|c| *c != Some(Unimplemented)), "{codes:?}");
}

const ONE_OF_EACH: &str = r#"
kind: vehicle
id: five
zones: { z: {} }
devices:
  a: { role: zone-controller, zone: z, backend: container }
  b: { role: ecu, zone: z, backend: qemu-microvm }
  c: { role: ecu, zone: z, backend: libvirt }
  d: { role: ecu, zone: z, backend: cloud-hypervisor }
  e: { role: sensor, zone: z, backend: wasm-capsule }
  f: { role: ecu, zone: z, backend: container }
  g: { role: ecu, zone: z, backend: qemu-mcu }
  idle: { role: ecu, zone: z, backend: container }
functions:
  fa: { deployed_on: a }
  fb: { deployed_on: b }
  fc: { deployed_on: c }
  fd: { deployed_on: d }
  fe: { deployed_on: e }
  ff: { deployed_on: f }
  fg: { deployed_on: g }
links:
  - { id: la, endpoints: [{ node: a, port: eth0, ip: 10.2.0.1/24 }], slice: adas }
  - { id: lb, endpoints: [{ node: b, port: eth0, ip: 10.2.0.2/24 }], slice: adas }
  - { id: lc, endpoints: [{ node: c, port: eth0, ip: 10.2.0.3/24 }], slice: adas }
  - { id: ld, endpoints: [{ node: d, port: eth0, ip: 10.2.0.4/24 }], slice: adas }
  - { id: le, endpoints: [{ node: e, port: eth0, ip: 10.2.0.5/24 }], slice: adas }
  - { id: lf, endpoints: [{ node: f, port: eth0, ip: 10.2.0.6/24 }], slice: adas }
  - { id: lg, endpoints: [{ node: g, port: eth0, ip: 10.2.0.7/24 }], slice: adas }
---
kind: baseline
id: bl
parts:
  - { ref: 'local/vxprobe:host-process', target_ids: ff }
  - ref: ghcr.io/vxedge/parts/probe:1
    digest: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    target_ids: fa
  - { ref: ./bundle, kind: bundle, target_ids: [fb, fc, fd] }
  - { ref: ./firmware, kind: bundle, target_ids: fg }
  - { ref: ./capsule.wasm, kind: wasm, target_ids: fe }
"#;

fn one_of_each_workspace() -> (PathBuf, String) {
    let dir = scratch();
    std::fs::write(dir.join("vehicle.yaml"), ONE_OF_EACH).unwrap();
    (
        dir,
        vxedge_core::model::load_vehicle_yaml(ONE_OF_EACH)
            .unwrap()
            .id,
    )
}

fn write_run_record(runtime: &Path, id: &str, dir: &Path, run_id: &str) {
    let record = serde_json::json!({
        "id": id, "dir": dir, "bridge": "lo", "run_id": run_id,
        "started_unix_ms": 1234, "sha256": vxedge_core::Workspace::identity(dir).unwrap().value,
        "identity_algorithm": "vxws/2",
    });
    std::fs::write(runtime.join("active.json"), record.to_string()).unwrap();
}

#[tokio::test]
async fn run_service_reports_the_active_run() {
    let ws = vehicle_workspace();
    let daemon = Daemon::start_with(|run| write_run_record(run, &vehicle_id(), &ws, "run-a"));
    let mut runs = RunServiceClient::new(daemon.channel().await);
    let listed = runs
        .list(RunListRequest::default())
        .await
        .unwrap()
        .into_inner()
        .runs;
    assert_eq!(listed.len(), 1);
    let run = &listed[0];
    assert_eq!(run.id, "run-a");
    assert_eq!(run.kind, ResourceKind::Vehicle as i32);
    assert_eq!(run.target_id, "car");
    assert_eq!(run.started_unix_ms, 1234);
    assert_eq!(
        run.workspace_sha256,
        vxedge_core::Workspace::identity(&ws).unwrap().value
    );
    assert_eq!(run.workspace_identity_algorithm, "vxws/2");
    let named = |id: &str| RunGetRequest {
        run: Some(RunRef { run_id: id.into() }),
    };
    assert_eq!(
        runs.get(named("run-a")).await.unwrap().into_inner().id,
        "run-a"
    );
    assert_eq!(
        runs.get(RunGetRequest::default())
            .await
            .unwrap()
            .into_inner()
            .id,
        "run-a",
        "no run named means the only run"
    );
    let missing = runs.get(named("run-b")).await.unwrap_err();
    assert_eq!(missing.code(), tonic::Code::NotFound, "{missing:?}");
}

#[tokio::test]
async fn run_service_is_empty_with_nothing_active() {
    let daemon = Daemon::start();
    let runs = RunServiceClient::new(daemon.channel().await)
        .list(RunListRequest::default())
        .await
        .unwrap()
        .into_inner()
        .runs;
    assert!(runs.is_empty());
}

#[tokio::test]
async fn a_run_that_is_not_active_is_refused() {
    let ws = vehicle_workspace();
    let daemon = Daemon::start_with(|run| write_run_record(run, &vehicle_id(), &ws, "run-a"));
    let channel = daemon.channel().await;
    let other = Some(RunRef {
        run_id: "run-b".into(),
    });
    let refused = [
        ZoneServiceClient::new(channel.clone())
            .list(ZoneListRequest { run: other.clone() })
            .await
            .map(|_| ())
            .unwrap_err(),
        ComputeServiceClient::new(channel.clone())
            .list(ComputeListRequest { run: other.clone() })
            .await
            .map(|_| ())
            .unwrap_err(),
        SvcServiceClient::new(channel.clone())
            .status(SvcStatusRequest {
                service_type: "ota".into(),
                run: other.clone(),
                ..Default::default()
            })
            .await
            .map(|_| ())
            .unwrap_err(),
        NosCheckServiceClient::new(channel.clone())
            .status(NosCheckStatusRequest {
                run: other.clone(),
                ..Default::default()
            })
            .await
            .map(|_| ())
            .unwrap_err(),
        TrafficServiceClient::new(channel.clone())
            .down(TrafficDownRequest {
                run: other,
                ..Default::default()
            })
            .await
            .map(|_| ())
            .unwrap_err(),
    ];
    for status in refused {
        assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status:?}");
        assert!(status.message().contains("run-a"), "{status:?}");
    }
    let named = ZoneServiceClient::new(channel)
        .list(ZoneListRequest {
            run: Some(RunRef {
                run_id: "run-a".into(),
            }),
        })
        .await;
    assert!(named.is_ok(), "{named:?}");
}

#[tokio::test]
async fn experiment_up_refuses_a_workspace_that_is_not_active() {
    let (active, id) = experiment_workspace();
    let (other, _) = experiment_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &id, &active, None));
    let refused = ExperimentServiceClient::new(daemon.channel().await)
        .up(ExperimentUpRequest {
            manifest: path_ref(&other),
            stages: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        refused.code(),
        tonic::Code::FailedPrecondition,
        "{refused:?}"
    );
}

#[tokio::test]
async fn node_status_is_typed_on_every_backend() {
    let (ws, id) = one_of_each_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &id, &ws, None));
    let nodes = ZoneServiceClient::new(daemon.channel().await)
        .list(ZoneListRequest::default())
        .await
        .unwrap()
        .into_inner()
        .nodes;
    let spec = |id: &str| {
        nodes
            .iter()
            .find(|n| n.status.as_ref().unwrap().vxid == id)
            .and_then(|n| n.spec.clone())
            .unwrap_or_else(|| panic!("{id} missing"))
    };
    for (id, backend, kind) in [
        ("a", NodeBackend::Container, PayloadKind::Container),
        ("b", NodeBackend::QemuMicrovm, PayloadKind::GuestBundle),
        ("c", NodeBackend::Libvirt, PayloadKind::GuestBundle),
        ("d", NodeBackend::CloudHypervisor, PayloadKind::GuestBundle),
        ("e", NodeBackend::WasmCapsule, PayloadKind::Wasm),
        ("f", NodeBackend::Container, PayloadKind::HostProcess),
        ("g", NodeBackend::QemuMcu, PayloadKind::GuestBundle),
    ] {
        let node = spec(id);
        assert_eq!(node.backend, backend as i32, "{id}");
        assert_eq!(node.payload.unwrap().kind, kind as i32, "{id}");
    }
    let a = spec("a");
    assert_eq!(a.zone, "z");
    assert_eq!(a.role, "zone-controller");
    assert_eq!(a.slice, "adas");
    assert!(a.vlan > 0);
    let payload = a.payload.unwrap();
    assert_eq!(payload.reference, "ghcr.io/vxedge/parts/probe:1");
    assert_eq!(
        payload.digest,
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
}

#[tokio::test]
async fn a_status_says_when_it_was_read_and_not_when_it_began() {
    let ws = vehicle_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &vehicle_id(), &ws, None));
    let zones = ZoneServiceClient::new(daemon.channel().await)
        .list(ZoneListRequest::default())
        .await
        .unwrap()
        .into_inner()
        .zones;
    assert!(zones[0].observed_unix_ms > 0);
    assert_eq!(zones[0].since_unix_ms, 0);
}

#[tokio::test]
async fn a_declared_device_nothing_provisions_is_unprovisioned() {
    let (ws, id) = one_of_each_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &id, &ws, None));
    let status = VehicleServiceClient::new(daemon.channel().await)
        .status(VehicleStatusRequest {
            manifest: path_ref(&ws),
        })
        .await
        .unwrap()
        .into_inner();
    let idle = status.components.iter().find(|c| c.vxid == "idle").unwrap();
    assert_eq!(idle.state, ResourceState::Unprovisioned as i32);
    assert_eq!(status.nodes.len(), 7, "typed entries for provisioned nodes");
}

#[tokio::test]
async fn experiment_status_carries_the_gateway_and_typed_nodes() {
    let (ws, id) = experiment_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &id, &ws, None));
    let status = ExperimentServiceClient::new(daemon.channel().await)
        .status(ExperimentStatusRequest {
            manifest: path_ref(&ws),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(status.gateway.is_some());
    let ids: Vec<&str> = status
        .nodes
        .iter()
        .map(|n| n.status.as_ref().unwrap().vxid.as_str())
        .collect();
    assert_eq!(ids, ["ecu1", "hpc1"]);
}

/// A vehicle with nothing that needs a host check, so a stage can run unprivileged.
const PROCESS_ONLY: &str = r#"
kind: vehicle
id: plain
zones: { z: {} }
devices:
  a: { role: ecu, zone: z, backend: container }
functions:
  fa: { deployed_on: a }
links:
  - { id: la, endpoints: [{ node: a, port: eth0, ip: 10.3.0.1/24 }], slice: adas }
---
kind: baseline
id: bl
parts:
  - { ref: 'local/vxprobe:host-process', target_ids: fa }
"#;

#[tokio::test]
async fn vehicle_up_runs_only_the_stages_it_was_given() {
    let daemon = Daemon::start();
    let dir = scratch();
    std::fs::write(dir.join("vehicle.yaml"), PROCESS_ONLY).unwrap();
    let channel = daemon.channel().await;
    let job = VehicleServiceClient::new(channel.clone())
        .up(VehicleUpRequest {
            manifest: path_ref(&dir),
            stages: vec![ExperimentStage::Services as i32],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let state = wait_for_job(channel.clone(), job.id.clone()).await;
    let done = JobServiceClient::new(channel)
        .get(GetJobRequest { id: job.id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(state, JobState::Succeeded as i32, "{}", done.error);
    assert!(!done.message.contains("gateway"), "{}", done.message);
}

#[tokio::test]
async fn a_stage_the_daemon_does_not_know_is_refused() {
    let daemon = Daemon::start();
    let dir = scratch();
    std::fs::write(dir.join("vehicle.yaml"), PROCESS_ONLY).unwrap();
    let channel = daemon.channel().await;
    for stage in [ExperimentStage::Unspecified as i32, 99] {
        let refused = VehicleServiceClient::new(channel.clone())
            .up(VehicleUpRequest {
                manifest: path_ref(&dir),
                stages: vec![stage],
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(refused.code(), tonic::Code::InvalidArgument, "{refused:?}");
    }
}

/// The `ErrorInfo.reason` a refusal carries, and its domain.
fn reason(status: &tonic::Status) -> String {
    use tonic_types::StatusExt;
    let info = status
        .get_error_details()
        .error_info()
        .cloned()
        .unwrap_or_else(|| panic!("no ErrorInfo on {status:?}"));
    assert_eq!(info.domain, "vxedge.v1");
    info.reason
}

#[tokio::test]
async fn refusals_carry_a_reason_an_agent_can_branch_on() {
    let (active, id) = experiment_workspace();
    let (other, _) = experiment_workspace();
    let daemon = Daemon::start_with(|run| write_run_record(run, &id, &active, "run-a"));
    let channel = daemon.channel().await;
    let wrong = ExperimentServiceClient::new(channel.clone())
        .status(ExperimentStatusRequest {
            manifest: path_ref(&other),
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&wrong), "ERROR_REASON_WRONG_WORKSPACE");
    let unknown = SvcServiceClient::new(channel.clone())
        .status(SvcStatusRequest {
            service_type: "bogus".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&unknown), "ERROR_REASON_UNKNOWN_SERVICE_TYPE");
    let mismatch = ZoneServiceClient::new(channel.clone())
        .list(ZoneListRequest {
            run: Some(RunRef {
                run_id: "run-b".into(),
            }),
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&mismatch), "ERROR_REASON_RUN_MISMATCH");
    let job = JobServiceClient::new(channel)
        .get(GetJobRequest {
            id: "job-000000-000000".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(job.code(), tonic::Code::NotFound);
    assert_eq!(reason(&job), "ERROR_REASON_JOB_UNKNOWN");
}

#[tokio::test]
async fn nothing_active_is_its_own_reason() {
    let daemon = Daemon::start();
    let refused = ZoneServiceClient::new(daemon.channel().await)
        .list(ZoneListRequest::default())
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_NO_ACTIVE_RUN");
}

#[tokio::test]
async fn jobs_are_listed_and_a_finished_one_cannot_be_cancelled() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let job = SvcServiceClient::new(channel.clone())
        .down(SvcDownRequest {
            service_type: "ota".into(),
            experiment: None,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    wait_for_job(channel.clone(), job.id.clone()).await;
    let mut jobs = JobServiceClient::new(channel);
    let listed = jobs
        .list(vxedge_api::v1::JobListRequest {
            kind: "svc.down".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.jobs.len(), 1);
    assert_eq!(listed.jobs[0].id, job.id);
    let none = jobs
        .list(vxedge_api::v1::JobListRequest {
            kind: "vehicle.up".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(none.jobs.is_empty());
    let refused = jobs
        .cancel(vxedge_api::v1::JobCancelRequest { id: job.id })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_NOT_CANCELLABLE");
}

/// A one-device vehicle on `backend`, with `part` as its only part.
fn single(backend: &str, part: &str) -> String {
    format!(
        r#"
kind: vehicle
id: single
zones: {{ z: {{}} }}
devices:
  a: {{ role: ecu, zone: z, backend: {backend} }}
functions:
  fa: {{ deployed_on: a }}
links:
  - {{ id: la, endpoints: [{{ node: a, port: eth0, ip: 10.4.0.1/24 }}], slice: adas }}
---
kind: baseline
id: bl
parts:
  - {part}
"#
    )
}

fn workspace_of(yaml: &str) -> PathBuf {
    let dir = scratch();
    std::fs::write(dir.join("vehicle.yaml"), yaml).unwrap();
    dir
}

async fn plan_of(channel: &Channel, dir: &Path) -> vxedge_api::v1::WorkspacePlan {
    vxedge_api::v1::workspace_service_client::WorkspaceServiceClient::new(channel.clone())
        .plan(vxedge_api::v1::WorkspacePlanRequest {
            manifest: path_ref(dir),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
}

/// `Plan` and `vehicle up` run one list of checks, so `Plan`'s first problem is the reason `up`
/// refuses with. A case where they disagreed would fail here.
#[tokio::test]
async fn plan_and_up_agree() {
    let bundle = "{ ref: ./bundle, kind: bundle, target_ids: fa }";
    let built_bundle = |dir: &Path| {
        std::fs::create_dir_all(dir.join("bundle")).unwrap();
        std::fs::write(dir.join("bundle/vmlinuz"), b"").unwrap();
        std::fs::write(dir.join("bundle/initrd.img"), b"").unwrap();
    };
    type Prepare = Box<dyn Fn(&Path)>;
    let cases: Vec<(&str, String, Prepare, &str)> = vec![
        (
            "unbuilt bundle",
            single("qemu-microvm", bundle),
            Box::new(|_| {}),
            "ERROR_REASON_LOCAL_BUNDLE_MISSING",
        ),
        (
            "missing capsule module",
            single(
                "wasm-capsule",
                "{ ref: ./capsule.wasm, kind: wasm, target_ids: fa }",
            ),
            Box::new(|_| {}),
            "ERROR_REASON_CAPSULE_MODULE_MISSING",
        ),
        (
            "socket path too long",
            single("cloud-hypervisor", bundle),
            Box::new(built_bundle),
            "ERROR_REASON_RUNTIME_DIR_TOO_LONG",
        ),
    ];
    for (name, yaml, prepare, expected) in cases {
        let daemon = if name == "socket path too long" {
            Daemon::start_in(&"r".repeat(100))
        } else {
            Daemon::start()
        };
        let channel = daemon.channel().await;
        let dir = workspace_of(&yaml);
        prepare(&dir);
        let plan = plan_of(&channel, &dir).await;
        assert!(!plan.valid, "{name}");
        let first = plan
            .problems
            .first()
            .unwrap_or_else(|| panic!("{name}: no problem"));
        assert_eq!(
            vxedge_api::v1::ErrorReason::try_from(first.reason)
                .unwrap()
                .as_str_name(),
            expected,
            "{name}: plan"
        );
        let refused = VehicleServiceClient::new(channel)
            .up(VehicleUpRequest {
                manifest: path_ref(&dir),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(reason(&refused), expected, "{name}: up");
    }
}

/// A `.vxp` whose id is not the experiment's is refused by `Plan` and by `experiment up`, with
/// the same typed reason, before anything is provisioned.
#[tokio::test]
async fn plan_and_up_agree_on_a_mismatched_descriptor() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, id) = experiment_workspace();
    std::fs::write(dir.join("pkg.vxp"), "id: not-the-experiment\n").unwrap();

    let plan = plan_of(&channel, &dir).await;
    assert!(!plan.valid);
    let first = plan.problems.first().expect("refused");
    assert_eq!(
        first.reason,
        vxedge_api::v1::ErrorReason::WorkspaceInvalid as i32
    );
    assert!(
        first.detail.contains("not-the-experiment"),
        "{}",
        first.detail
    );
    assert!(first.detail.contains(&id), "{}", first.detail);

    let refused = ExperimentServiceClient::new(channel)
        .up(ExperimentUpRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_WORKSPACE_INVALID");
}

/// A `vxp/1` descriptor for the workspace `experiment_workspace` writes, with `extra` appended.
fn write_v1(dir: &Path, id: &str, schema: &str, extra: &str) {
    std::fs::write(
        dir.join("vxp.yaml"),
        format!("schema: {schema}\nid: vxedge/{id}\nversion: 1.0.0\nentry: main.yaml\n{extra}"),
    )
    .unwrap();
}

/// A descriptor this daemon reads plans clean: `vxp.yaml` is not joined as a document.
#[tokio::test]
async fn a_vxp1_package_plans_and_its_descriptor_is_not_a_document() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, id) = experiment_workspace();
    let ours = env!("CARGO_PKG_VERSION");
    write_v1(
        &dir,
        &id,
        "vxp/1",
        &format!("requires:\n  vxedge: \"={ours}\"\n"),
    );
    let plan = plan_of(&channel, &dir).await;
    assert!(
        !plan.problems.iter().any(|p| p.reason
            == vxedge_api::v1::ErrorReason::WorkspaceInvalid as i32
            || p.reason == vxedge_api::v1::ErrorReason::PackageSchemaUnsupported as i32
            || p.reason == vxedge_api::v1::ErrorReason::VxedgeVersionUnsupported as i32),
        "{:?}",
        plan.problems
    );
}

/// A newer schema is a typed refusal an agent can branch on, from `Plan` and from `up` alike.
#[tokio::test]
async fn plan_and_up_refuse_an_unsupported_schema_by_reason() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, id) = experiment_workspace();
    write_v1(&dir, &id, "vxp/9", "");
    let plan = plan_of(&channel, &dir).await;
    assert!(!plan.valid);
    let first = plan.problems.first().expect("refused");
    assert_eq!(
        first.reason,
        vxedge_api::v1::ErrorReason::PackageSchemaUnsupported as i32,
        "{}",
        first.detail
    );
    assert!(first.detail.contains("vxp/1"), "{}", first.detail);
    let refused = ExperimentServiceClient::new(channel)
        .up(ExperimentUpRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_PACKAGE_SCHEMA_UNSUPPORTED");
}

/// A package whose `requires.vxedge` excludes this daemon is refused before anything runs.
/// The range is built from this build's own version, so the test holds across releases.
#[tokio::test]
async fn plan_and_up_refuse_a_package_for_another_vxedge() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, id) = experiment_workspace();
    let ours = env!("CARGO_PKG_VERSION");
    write_v1(
        &dir,
        &id,
        "vxp/1",
        &format!("requires:\n  vxedge: \">{ours}\"\n"),
    );
    let plan = plan_of(&channel, &dir).await;
    assert!(!plan.valid);
    assert!(
        plan.problems.iter().any(|p| p.reason
            == vxedge_api::v1::ErrorReason::VxedgeVersionUnsupported as i32
            && p.detail.contains(ours)),
        "{:?}",
        plan.problems
    );
    let refused = ExperimentServiceClient::new(channel)
        .up(ExperimentUpRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_VXEDGE_VERSION_UNSUPPORTED");
}

/// `kernel.preempt_rt: required` refuses a kernel without it, in `Plan` and `up` alike, under
/// the reason the GPU tier already uses. Skipped on a real-time host, where it is met.
#[tokio::test]
async fn plan_and_up_refuse_a_package_needing_a_realtime_kernel() {
    if vxedge_core::package::HostCapabilities::probe().preempt_rt {
        return;
    }
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, id) = experiment_workspace();
    write_v1(
        &dir,
        &id,
        "vxp/1",
        "requires:\n  kernel: { preempt_rt: required }\n",
    );
    let plan = plan_of(&channel, &dir).await;
    assert!(!plan.valid);
    assert!(
        plan.problems.iter().any(|p| p.reason
            == vxedge_api::v1::ErrorReason::HostRequirementUnmet as i32
            && p.detail.contains("PREEMPT_RT")),
        "{:?}",
        plan.problems
    );
    assert!(
        plan.host_checks
            .iter()
            .any(|c| c.name == "host requirements" && !c.passed),
        "{:?}",
        plan.host_checks
    );
    let refused = ExperimentServiceClient::new(channel)
        .up(ExperimentUpRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_HOST_REQUIREMENT_UNMET");
}

/// What asks for nothing refuses nothing: `kvm: false` and `preempt_rt: preferred` plan on any
/// host.
#[tokio::test]
async fn soft_host_requirements_plan_on_any_host() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, id) = experiment_workspace();
    write_v1(
        &dir,
        &id,
        "vxp/1",
        "requires:\n  kvm: false\n  kernel: { preempt_rt: preferred }\n",
    );
    let plan = plan_of(&channel, &dir).await;
    assert!(
        !plan
            .problems
            .iter()
            .any(|p| p.reason == vxedge_api::v1::ErrorReason::HostRequirementUnmet as i32),
        "{:?}",
        plan.problems
    );
}

#[tokio::test]
async fn plan_and_up_agree_on_the_wrong_workspace() {
    let (active, id) = experiment_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &id, &active, None));
    let channel = daemon.channel().await;
    let other = workspace_of(PROCESS_ONLY);
    let plan = plan_of(&channel, &other).await;
    let first = plan.problems.first().expect("refused");
    assert_eq!(
        first.reason,
        vxedge_api::v1::ErrorReason::WrongWorkspace as i32
    );
    let refused = VehicleServiceClient::new(channel)
        .up(VehicleUpRequest {
            manifest: path_ref(&other),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_WRONG_WORKSPACE");
}

#[tokio::test]
async fn experiment_up_runs_the_same_checks() {
    let daemon = Daemon::start();
    let dir = scratch();
    let yaml = vxedge_core::model::synthesize_vehicle_workspace(&single(
        "qemu-microvm",
        "{ ref: ./bundle, kind: bundle, target_ids: fa }",
    ))
    .unwrap();
    std::fs::write(dir.join("main.yaml"), yaml).unwrap();
    let refused = ExperimentServiceClient::new(daemon.channel().await)
        .up(ExperimentUpRequest {
            manifest: path_ref(&dir),
            stages: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_LOCAL_BUNDLE_MISSING");
}

#[tokio::test]
async fn a_valid_plan_lists_what_up_would_run() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let dir = workspace_of(PROCESS_ONLY);
    let plan = plan_of(&channel, &dir).await;
    assert!(plan.valid, "{:?}", plan.problems);
    assert_eq!(plan.nodes.len(), 1);
    assert_eq!(plan.nodes[0].vxid, "a");
    assert!(
        plan.host_checks
            .iter()
            .any(|c| c.name == "guest bundles" && c.passed)
    );
    let broken = workspace_of("kind: vehicle\nid: x\nzones: {}\nnot_a_field: 1\n");
    let invalid = plan_of(&channel, &broken).await;
    assert!(!invalid.valid);
    assert_eq!(
        invalid.problems[0].reason,
        vxedge_api::v1::ErrorReason::WorkspaceInvalid as i32
    );
}

#[tokio::test]
async fn host_capabilities_cover_every_backend() {
    let daemon = Daemon::start();
    let caps = vxedge_api::v1::host_service_client::HostServiceClient::new(daemon.channel().await)
        .capabilities(vxedge_api::v1::HostCapabilitiesRequest::default())
        .await
        .unwrap()
        .into_inner();
    let mut backends: Vec<i32> = caps.backends.iter().map(|b| b.backend).collect();
    backends.sort();
    assert_eq!(backends, [1, 2, 3, 4, 5, 6, 7]);
    assert!(
        caps.backends
            .iter()
            .all(|b| b.available || !b.detail.is_empty())
    );
    assert_eq!(caps.part_kinds, ["container", "bundle", "wasm", "attach"]);
    assert_eq!(caps.max_runs, 1);
    assert_eq!(caps.package_schemas, ["vxp/1"]);
    // The container backend is available only where `up`'s own Podman check would pass.
    let container = caps
        .backends
        .iter()
        .find(|b| b.backend == vxedge_api::v1::NodeBackend::Container as i32)
        .unwrap();
    if !podman_is_reachable() {
        assert!(!container.available, "no Podman, so no container backend");
        assert!(container.detail.contains("Podman"), "{}", container.detail);
    }
}

#[tokio::test]
async fn a_subscriber_resumes_after_the_last_event_it_saw() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let down = |channel: Channel| async move {
        let job = SvcServiceClient::new(channel.clone())
            .down(SvcDownRequest {
                service_type: "ota".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        wait_for_job(channel, job.id).await;
    };
    let mut events = EventServiceClient::new(channel.clone());
    let mut first = events
        .subscribe(SubscribeRequest {
            kinds: vec!["service".into()],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    down(channel.clone()).await;
    let seen = first.message().await.unwrap().unwrap();
    assert_eq!(
        seen.kind, "service.down",
        "a family name matches its events"
    );
    assert!(
        seen.id.starts_with("evt-") && seen.id.matches('-').count() == 2,
        "{}",
        seen.id
    );
    drop(first);
    // Missed while disconnected.
    down(channel.clone()).await;
    let mut resumed = events
        .subscribe(SubscribeRequest {
            kinds: vec!["service".into()],
            after_id: seen.id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    let missed = tokio::time::timeout(Duration::from_secs(5), resumed.message())
        .await
        .expect("replayed from history")
        .unwrap()
        .unwrap();
    assert_eq!(missed.kind, "service.down");
    assert_ne!(missed.id, seen.id);
}

#[tokio::test]
async fn a_cursor_the_daemon_never_issued_is_events_lost() {
    let daemon = Daemon::start();
    let refused = EventServiceClient::new(daemon.channel().await)
        .subscribe(SubscribeRequest {
            after_id: "evt-000000-00000000".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::OutOfRange);
    assert_eq!(reason(&refused), "ERROR_REASON_EVENTS_LOST");
}

/// A stand-in `vxtelemetry` that writes the archive it is asked for, or fails.
fn fake_vxtelemetry(exit: i32) -> PathBuf {
    let dir = scratch();
    let bin = dir.join("vxtelemetry");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\necho \"$@\" > \"{0}/args\"\necho \"${{VXTELEMETRY_PACKAGE_LOCKED-unset}}\" > \
             \"{0}/locked\"\nwhile [ $# -gt 0 ]; do [ \"$1\" = --out ] && printf archive > \"$2\"; \
             shift; done\nexit {exit}\n",
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    bin
}

#[tokio::test]
async fn collect_returns_the_archive_and_its_digest() {
    let (ws, _) = experiment_workspace();
    let bin = fake_vxtelemetry(0);
    let daemon = Daemon::start_env(
        |run| std::fs::create_dir_all(run.join("ingest")).unwrap(),
        &[("VXEDGE_VXTELEMETRY_BIN", &bin)],
    );
    let channel = daemon.channel().await;
    let job =
        vxedge_api::v1::telemetry_service_client::TelemetryServiceClient::new(channel.clone())
            .collect(vxedge_api::v1::CollectRequest {
                manifest: path_ref(&ws),
                out_path: "results/run.zip".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
    assert_eq!(job.kind, "telemetry.collect");
    assert_eq!(
        wait_for_job(channel.clone(), job.id.clone()).await,
        JobState::Succeeded as i32
    );
    let done = JobServiceClient::new(channel)
        .get(GetJobRequest { id: job.id })
        .await
        .unwrap()
        .into_inner();
    let artifact = done.artifact.expect("the archive is the job's artifact");
    let archive = ws.join("results/run.zip");
    assert_eq!(PathBuf::from(&artifact.path), archive);
    use sha2::Digest;
    let expected: String = sha2::Sha256::digest(std::fs::read(&archive).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(artifact.sha256, expected);
    let args = std::fs::read_to_string(bin.parent().unwrap().join("args")).unwrap();
    assert!(
        args.contains("--record-image localhost/vxedge/probe:1"),
        "{args}"
    );
    let locked = std::fs::read_to_string(bin.parent().unwrap().join("locked")).unwrap();
    assert_eq!(locked.trim(), "unset", "no run, so nothing to say");
}

/// `collect` tells vxtelemetry whether the live run of this workspace checked its lock, and
/// records the package digest in its audit line.
#[tokio::test]
async fn collect_passes_on_whether_the_run_was_locked() {
    let (ws, id) = experiment_workspace();
    let bin = fake_vxtelemetry(0);
    let daemon = Daemon::start_env(
        |run| {
            std::fs::create_dir_all(run.join("ingest")).unwrap();
            let record = serde_json::json!({
                "id": id, "dir": ws, "bridge": "lo", "run_id": "run-l",
                "package_id": "vxedge/x", "package_version": "1.0.0",
                "package_digest": "sha256:abc", "package_locked": false,
                "kernel_release": "6.1.0", "kernel_preempt_rt": false,
            });
            std::fs::write(run.join("active.json"), record.to_string()).unwrap();
        },
        &[("VXEDGE_VXTELEMETRY_BIN", &bin)],
    );
    let channel = daemon.channel().await;
    let job =
        vxedge_api::v1::telemetry_service_client::TelemetryServiceClient::new(channel.clone())
            .collect(vxedge_api::v1::CollectRequest {
                manifest: path_ref(&ws),
                out_path: "results/run.zip".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
    assert_eq!(
        wait_for_job(channel, job.id).await,
        JobState::Succeeded as i32
    );
    let locked = std::fs::read_to_string(bin.parent().unwrap().join("locked")).unwrap();
    assert_eq!(locked.trim(), "false");
}

#[tokio::test]
async fn a_failed_collect_fails_its_job_and_nothing_ingested_is_refused() {
    let (ws, _) = experiment_workspace();
    let bin = fake_vxtelemetry(3);
    let daemon = Daemon::start_env(
        |run| std::fs::create_dir_all(run.join("ingest")).unwrap(),
        &[("VXEDGE_VXTELEMETRY_BIN", &bin)],
    );
    let channel = daemon.channel().await;
    let mut telemetry =
        vxedge_api::v1::telemetry_service_client::TelemetryServiceClient::new(channel.clone());
    let job = telemetry
        .collect(vxedge_api::v1::CollectRequest {
            manifest: path_ref(&ws),
            out_path: String::new(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(wait_for_job(channel, job.id).await, JobState::Failed as i32);
    let empty = Daemon::start();
    let refused = vxedge_api::v1::telemetry_service_client::TelemetryServiceClient::new(
        empty.channel().await,
    )
    .collect(vxedge_api::v1::CollectRequest {
        manifest: path_ref(&ws),
        out_path: String::new(),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_NOT_FOUND");
}

/// A `vxp/1` package as `experiment_workspace` writes it, with a lock that matches.
fn locked_package() -> (PathBuf, String) {
    let (dir, id) = experiment_workspace();
    write_v1(&dir, &id, "vxp/1", "");
    let lock = vxedge_core::pkg::lock(&dir, "0.0.0-test").unwrap();
    vxedge_core::pkg::write_lock(&dir, &lock).unwrap();
    (dir, id)
}

fn lock_problems(plan: &vxedge_api::v1::WorkspacePlan) -> Vec<String> {
    plan.problems
        .iter()
        .filter(|p| p.reason == vxedge_api::v1::ErrorReason::LockMismatch as i32)
        .map(|p| p.detail.clone())
        .collect()
}

async fn plan_unlocked(channel: &Channel, dir: &Path) -> vxedge_api::v1::WorkspacePlan {
    vxedge_api::v1::workspace_service_client::WorkspaceServiceClient::new(channel.clone())
        .plan(vxedge_api::v1::WorkspacePlanRequest {
            manifest: path_ref(dir),
            unlocked: true,
        })
        .await
        .unwrap()
        .into_inner()
}

/// `up` runs what the lock says: a changed file, an added one or a missing lock is refused by
/// `Plan`, `experiment up` and `gateway up` alike, naming each difference, and `unlocked`
/// skips the check. Nothing is provisioned: every refusal comes before the job.
#[tokio::test]
async fn plan_and_up_refuse_a_package_that_does_not_match_its_lock() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, _) = locked_package();
    assert_eq!(
        lock_problems(&plan_of(&channel, &dir).await),
        Vec::<String>::new()
    );

    std::fs::write(dir.join("main.yaml"), {
        let mut yaml = std::fs::read_to_string(dir.join("main.yaml")).unwrap();
        yaml.push_str("# edited after locking\n");
        yaml
    })
    .unwrap();
    std::fs::write(dir.join("notes.txt"), "added\n").unwrap();
    let problems = lock_problems(&plan_of(&channel, &dir).await);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("`main.yaml` changed"),
        "{}",
        problems[0]
    );
    assert!(
        problems[0].contains("`notes.txt` was added"),
        "{}",
        problems[0]
    );
    assert!(problems[0].contains("--unlocked"), "{}", problems[0]);
    assert_eq!(
        lock_problems(&plan_unlocked(&channel, &dir).await),
        Vec::<String>::new()
    );

    let refused = ExperimentServiceClient::new(channel.clone())
        .up(ExperimentUpRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_LOCK_MISMATCH");
    let refused =
        vxedge_api::v1::gateway_service_client::GatewayServiceClient::new(channel.clone())
            .up(vxedge_api::v1::GatewayUpRequest {
                manifest: path_ref(&dir),
                unlocked: false,
            })
            .await
            .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_LOCK_MISMATCH");
    assert!(
        refused.message().contains("Nothing was provisioned"),
        "{}",
        refused.message()
    );

    std::fs::remove_file(dir.join("vxp.lock")).unwrap();
    let problems = lock_problems(&plan_of(&channel, &dir).await);
    assert!(problems[0].contains("no `vxp.lock`"), "{problems:?}");
}

/// A legacy descriptor is never lock-checked.
#[tokio::test]
async fn a_legacy_package_is_not_lock_checked() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, id) = experiment_workspace();
    std::fs::write(
        dir.join(format!("{id}.vxp")),
        format!("id: {id}\nversion: 0.1.0\n"),
    )
    .unwrap();
    assert_eq!(
        lock_problems(&plan_of(&channel, &dir).await),
        Vec::<String>::new()
    );
}

/// A run's package and kernel are read back from its record, and a record from before they were
/// recorded reads as unset rather than as a guess.
#[tokio::test]
async fn run_service_reports_the_package_and_kernel_a_run_recorded() {
    let ws = vehicle_workspace();
    let daemon = Daemon::start_with(|run| {
        write_run_record(run, &vehicle_id(), &ws, "run-p");
        let path = run.join("active.json");
        let mut record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for (k, v) in [
            ("package_id", serde_json::json!("vxedge/car")),
            ("package_version", serde_json::json!("1.0.0")),
            ("package_digest", serde_json::json!("sha256:abc")),
            ("package_locked", serde_json::json!(false)),
            ("kernel_release", serde_json::json!("6.12.0-rt")),
            ("kernel_preempt_rt", serde_json::json!(true)),
        ] {
            record[k] = v;
        }
        std::fs::write(&path, record.to_string()).unwrap();
    });
    let run = RunServiceClient::new(daemon.channel().await)
        .get(RunGetRequest::default())
        .await
        .unwrap()
        .into_inner();
    let package = run.package.expect("package recorded");
    assert_eq!(
        (
            package.id.as_str(),
            package.version.as_str(),
            package.digest.as_str()
        ),
        ("vxedge/car", "1.0.0", "sha256:abc")
    );
    assert!(!run.package_locked, "an unlocked run says so");
    let kernel = run.kernel.expect("kernel recorded");
    assert_eq!(
        (kernel.release.as_str(), kernel.preempt_rt),
        ("6.12.0-rt", true)
    );

    let older = Daemon::start_with(|run| write_run_record(run, &vehicle_id(), &ws, "run-o"));
    let run = RunServiceClient::new(older.channel().await)
        .get(RunGetRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (run.package, run.kernel, run.package_locked),
        (None, None, false)
    );
}

/// Typed intent appended to `experiment_workspace`'s experiment document.
const BAD_INTENT: &str = "\nparameters:\n  load_mbps: { type: integer, unit: Mbit/s, default: 5000, min: 0, max: 1000 }\n\
metrics:\n  - { id: rtt_seconds, kind: histogram, unit: s, entity: measurements/rtt, series: vxprobe_rtt_seconds }\n\
criteria:\n  - { id: bound, op: le, lhs: { metric: rtt, aggregate: p99 }, rhs: { value: 1, unit: s } }\n";

/// A parameter or criterion that cannot hold is a typed problem in `Plan`, each under its own
/// reason and naming its field, and `up` refuses on the first before anything runs.
#[tokio::test]
async fn plan_and_up_refuse_typed_intent_that_cannot_hold() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, _) = experiment_workspace();
    let main = std::fs::read_to_string(dir.join("main.yaml")).unwrap();
    std::fs::write(dir.join("main.yaml"), format!("{main}{BAD_INTENT}")).unwrap();

    let plan = plan_of(&channel, &dir).await;
    assert!(!plan.valid);
    let typed = |r: vxedge_api::v1::ErrorReason| {
        plan.problems
            .iter()
            .filter(|p| p.reason == r as i32)
            .map(|p| p.detail.clone())
            .collect::<Vec<_>>()
    };
    let params = typed(vxedge_api::v1::ErrorReason::ParameterInvalid);
    assert_eq!(params.len(), 1, "{:?}", plan.problems);
    assert!(
        params[0].contains("parameters.load_mbps.default"),
        "{}",
        params[0]
    );
    let criteria = typed(vxedge_api::v1::ErrorReason::CriterionInvalid);
    assert_eq!(criteria.len(), 1, "{:?}", plan.problems);
    assert!(
        criteria[0].contains("criteria[0].lhs.metric"),
        "{}",
        criteria[0]
    );
    assert!(
        plan.host_checks
            .iter()
            .any(|c| c.name == "typed intent" && !c.passed),
        "{:?}",
        plan.host_checks
    );

    let refused = ExperimentServiceClient::new(channel)
        .up(ExperimentUpRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_PARAMETER_INVALID");
}

/// A measurement that cannot run is a typed problem in `Plan` naming its field, and `up` refuses
/// before anything runs. One that can is listed as an entity of its own.
#[tokio::test]
async fn plan_types_measurement_problems_and_lists_measurements() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, _) = experiment_workspace();
    let main = std::fs::read_to_string(dir.join("main.yaml")).unwrap();
    std::fs::write(
        dir.join("main.yaml"),
        format!(
            "{main}\nmeasurements:\n  ok: {{ kind: rtt-probe, from: ecu1, to: internet, slice: adas }}\n  \
             bad: {{ kind: rtt-probe, from: ecu1, to: nope, slice: adas }}\n"
        ),
    )
    .unwrap();

    let plan = plan_of(&channel, &dir).await;
    assert!(!plan.valid);
    let measured: Vec<_> = plan
        .problems
        .iter()
        .filter(|p| p.reason == vxedge_api::v1::ErrorReason::MeasurementInvalid as i32)
        .collect();
    assert_eq!(measured.len(), 1, "{:?}", plan.problems);
    assert!(
        measured[0].detail.contains("measurements.bad.to"),
        "{}",
        measured[0].detail
    );
    let entity = plan
        .entities
        .iter()
        .find(|e| e.path == "measurements/ok")
        .unwrap_or_else(|| panic!("{:?}", plan.entities));
    assert_eq!(
        (entity.kind.as_str(), entity.role.as_str()),
        ("measurement", "rtt-probe")
    );

    let refused = ExperimentServiceClient::new(channel)
        .up(ExperimentUpRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_MEASUREMENT_INVALID");
}

/// On a `mode: s-nssai` slice a measurement's node end is the slice's UE, whose address the 5G
/// core assigns at `up`: `Plan` takes one from a UE to `internet` with no pinned address, and
/// types one between two UEs as a measurement problem naming `to`.
#[tokio::test]
async fn a_measurement_from_a_ue_plans_without_a_pinned_address() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let dir = scratch();
    let ws = |measurements: &str| {
        format!(
            "kind: vehicle\nid: car\nzones: {{ front: {{}}, rear: {{}} }}\ndevices:\n  \
             ue1: {{ role: zone-controller, zone: front, backend: container }}\n  \
             ue2: {{ role: zone-controller, zone: rear, backend: container }}\n\
             functions:\n  f1: {{ deployed_on: ue1 }}\nlinks:\n  \
             - {{ id: l1, endpoints: [ue1:eth0], slice: adas }}\n  \
             - {{ id: l2, endpoints: [ue2:eth0], slice: info }}\n---\n\
             kind: baseline\nid: bl\nparts:\n  - {{ ref: 'local/vxprobe:host-process', target_ids: f1 }}\n---\n\
             kind: experiment\nid: h2\nvehicles: [car]\nbaseline: bl\nnetwork:\n  slices:\n    \
             - {{ id: adas, mode: s-nssai, s_nssai: {{ sst: 1, sd: '000001' }} }}\n    \
             - {{ id: info, mode: s-nssai, s_nssai: {{ sst: 2, sd: '000002' }} }}\n\
             measurements:\n{measurements}"
        )
    };
    let measurement_problems = |plan: &vxedge_api::v1::WorkspacePlan| {
        plan.problems
            .iter()
            .filter(|p| p.reason == vxedge_api::v1::ErrorReason::MeasurementInvalid as i32)
            .map(|p| p.detail.clone())
            .collect::<Vec<_>>()
    };

    std::fs::write(
        dir.join("main.yaml"),
        ws("  ctl: { kind: rtt-probe, from: ue1, to: internet, slice: adas }\n"),
    )
    .unwrap();
    let plan = plan_of(&channel, &dir).await;
    assert!(
        measurement_problems(&plan).is_empty(),
        "{:?}",
        plan.problems
    );
    assert!(
        plan.entities.iter().any(|e| e.path == "measurements/ctl"),
        "{:?}",
        plan.entities
    );

    std::fs::write(
        dir.join("main.yaml"),
        ws("  ctl: { kind: rtt-probe, from: ue1, to: ue2, slice: adas }\n"),
    )
    .unwrap();
    let plan = plan_of(&channel, &dir).await;
    let problems = measurement_problems(&plan);
    assert_eq!(problems.len(), 1, "{:?}", plan.problems);
    assert!(
        problems[0].contains("measurements.ctl.to") && problems[0].contains("s-nssai"),
        "{}",
        problems[0]
    );
}

/// `MeasurementService` names each declared measurement with its entity, and a start whose
/// sender's node is not up fails its job naming the node, before any process runs.
#[tokio::test]
async fn measurements_report_their_state_and_need_their_nodes_up() {
    use vxedge_api::v1::measurement_service_client::MeasurementServiceClient;
    use vxedge_api::v1::{MeasurementStartRequest, MeasurementState, MeasurementStatusRequest};
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, _) = experiment_workspace();
    let main = std::fs::read_to_string(dir.join("main.yaml")).unwrap();
    std::fs::write(
        dir.join("main.yaml"),
        format!("{main}\nmeasurements:\n  rtt: {{ kind: rtt-probe, from: ecu1, to: hpc1, slice: adas }}\n"),
    )
    .unwrap();
    let mut client = MeasurementServiceClient::new(channel.clone());
    let status = client
        .status(MeasurementStatusRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.measurements.len(), 1);
    let m = &status.measurements[0];
    assert_eq!(
        (m.id.as_str(), m.entity.as_str(), m.kind.as_str()),
        ("rtt", "measurements/rtt", "rtt-probe")
    );
    assert_eq!(m.state, MeasurementState::Unspecified as i32);

    let job = client
        .start(MeasurementStartRequest {
            manifest: path_ref(&dir),
            duration_s: 1,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        wait_for_job(channel.clone(), job.id.clone()).await,
        JobState::Failed as i32
    );
    let failed = vxedge_api::v1::job_service_client::JobServiceClient::new(channel)
        .get(GetJobRequest { id: job.id })
        .await
        .unwrap()
        .into_inner();
    assert!(failed.error.contains("`ecu1`"), "{}", failed.error);
}

/// A scaffolded `vxp/1` package, locked: a folder `PackageService` can inspect, pack and import.
fn scaffolded_package() -> PathBuf {
    let parent = scratch();
    let plan =
        vxedge_core::WorkspacePlan::plan_with_id("pkgx", "pkgx", vxedge_core::Template::Minimal)
            .unwrap();
    let root = plan.write(&parent).unwrap();
    let lock = vxedge_core::pkg::lock(&root, "test").unwrap();
    vxedge_core::pkg::write_lock(&root, &lock).unwrap();
    root
}

/// `Inspect` reads a folder's descriptor, lock state, typed intent and entities, touching
/// nothing; an unknown digest is `PACKAGE_UNKNOWN`.
#[tokio::test]
async fn inspect_reads_a_package_and_refuses_an_unknown_digest() {
    use vxedge_api::v1::PackageInspectRequest;
    use vxedge_api::v1::package_service_client::PackageServiceClient;
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let root = scaffolded_package();
    let info = PackageServiceClient::new(channel.clone())
        .inspect(PackageInspectRequest {
            package: path_ref(&root),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (info.id.as_str(), info.version.as_str()),
        ("local/pkgx", "0.1.0")
    );
    assert_eq!(info.schema, "vxp/1");
    assert!(info.locked, "{:?}", info.lock_problems);
    assert_eq!(
        info.digest,
        vxedge_core::pkg::package_digest(&root).unwrap()
    );
    assert!(
        info.entities.iter().any(|e| e.path == "host"),
        "{:?}",
        info.entities
    );
    assert!(info.problems.is_empty(), "{:?}", info.problems);

    let unknown = PackageServiceClient::new(channel)
        .inspect(PackageInspectRequest {
            package: Some(ManifestRef {
                source: Some(Source::Package(format!("sha256:{}", "0".repeat(64)))),
                ..Default::default()
            }),
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&unknown), "ERROR_REASON_PACKAGE_UNKNOWN");
}

/// `Inspect` types an experiment's intent: parameters with bounds, structured criteria with a
/// rendering for people, the campaign and the measurements.
#[tokio::test]
async fn inspect_types_parameters_criteria_campaign_and_measurements() {
    use vxedge_api::v1::package_service_client::PackageServiceClient;
    use vxedge_api::v1::{
        PackageInspectRequest, criterion_operand, package_criterion, param_value,
    };
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, _) = experiment_workspace();
    let main = std::fs::read_to_string(dir.join("main.yaml")).unwrap();
    std::fs::write(
        dir.join("main.yaml"),
        format!(
            "{main}\nparameters:\n  load_mbps: {{ type: integer, unit: Mbit/s, default: 0, min: 0, max: 1000 }}\n\
             metrics:\n  - {{ id: rtt_seconds, kind: histogram, unit: s, entity: measurements/rtt, series: vxprobe_rtt_seconds }}\n\
             criteria:\n  - {{ id: bound, op: le, lhs: {{ metric: rtt_seconds, aggregate: p99, select: {{ arm: busy }} }}, rhs: {{ value: 1, unit: s }} }}\n\
             campaign:\n  arms: {{ idle: {{ load_mbps: 0 }}, busy: {{ load_mbps: 600 }} }}\n  repetitions: 5\n\
             measurements:\n  rtt: {{ kind: rtt-probe, from: ecu1, to: internet, slice: adas }}\n"
        ),
    )
    .unwrap();
    let info = PackageServiceClient::new(channel)
        .inspect(PackageInspectRequest {
            package: path_ref(&dir),
        })
        .await
        .unwrap()
        .into_inner();
    let p = &info.parameters[0];
    assert_eq!(
        (p.name.as_str(), p.r#type.as_str(), p.max),
        ("load_mbps", "integer", Some(1000.0))
    );
    assert_eq!(
        p.default.as_ref().and_then(|d| d.value.clone()),
        Some(param_value::Value::Integer(0))
    );
    let c = &info.criteria[0];
    assert_eq!(c.text, "p99(rtt_seconds)[arm=busy] <= 1 s");
    let Some(package_criterion::Criterion::Comparison(cmp)) = &c.criterion else {
        panic!("{c:?}")
    };
    assert!(matches!(
        cmp.rhs.as_ref().and_then(|o| o.operand.as_ref()),
        Some(criterion_operand::Operand::Constant(_))
    ));
    let campaign = info.campaign.unwrap();
    assert_eq!((campaign.arms.len(), campaign.repetitions), (2, Some(5)));
    assert_eq!(info.metrics[0].series, "vxprobe_rtt_seconds");
    assert_eq!(info.measurements[0].entity, "measurements/rtt");
    assert!(info.entities.iter().any(|e| e.path == "measurements/rtt"));
    assert!(info.problems.is_empty(), "{:?}", info.problems);
}

/// A minimal OpenSCENARIO storyboard and OpenDRIVE road, written for these tests.
const STORYBOARD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<OpenSCENARIO>
  <FileHeader revMajor="1" revMinor="2" date="2026-11-10T00:00:00" description="test" author="vxedge"/>
  <ParameterDeclarations>
    <ParameterDeclaration name="EgoTargetSpeed" parameterType="double" value="13.9"/>
  </ParameterDeclarations>
  <Entities><ScenarioObject name="Ego"/><ScenarioObject name="Lead"/></Entities>
</OpenSCENARIO>"#;

/// `Inspect` returns a package's scenario typed, read through the legacy `world:` too; a
/// scenario that cannot run is `ERROR_REASON_SCENARIO_INVALID` in `Plan`, and `up` refuses it
/// before anything is provisioned.
#[tokio::test]
async fn a_scenario_is_typed_in_inspect_and_checked_by_plan() {
    use vxedge_api::v1::package_service_client::PackageServiceClient;
    use vxedge_api::v1::{ErrorReason, PackageInspectRequest};
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, _) = experiment_workspace();
    let vehicle = vxedge_core::Workspace::load(&dir).unwrap().vehicles[0]
        .id
        .clone();
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("assets/stop.xosc"), STORYBOARD).unwrap();
    std::fs::write(
        dir.join("assets/grid.xodr"),
        "<?xml version=\"1.0\"?><OpenDRIVE><header revMajor=\"1\" revMinor=\"6\"/></OpenDRIVE>",
    )
    .unwrap();
    let docs = |actor: &str| {
        format!(
            "kind: road\nid: grid\nnetwork: assets/grid.xodr\n---\nkind: scenario\nid: stop\nroad: grid\n\
             storyboard: assets/stop.xosc\nsimulator: esmini\nclock: {{ tick_ms: 10 }}\nseed: 42\n\
             actors: {{ {actor}: {{ vehicle: {vehicle} }} }}\n\
             parameters: {{ EgoTargetSpeed: {{ unit: m/s, min: 0, max: 22.2 }} }}\n"
        )
    };
    std::fs::write(dir.join("scenario.yaml"), docs("Ego")).unwrap();
    let main = std::fs::read_to_string(dir.join("main.yaml")).unwrap();
    std::fs::write(dir.join("main.yaml"), format!("{main}\nworld: stop\n")).unwrap();

    let info = PackageServiceClient::new(channel.clone())
        .inspect(PackageInspectRequest {
            package: path_ref(&dir),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(info.problems.is_empty(), "{:?}", info.problems);
    let sc = info.scenario.expect("the scenario, through `world:`");
    assert_eq!(
        (
            sc.id.as_str(),
            sc.road_network.as_str(),
            sc.simulator.as_str()
        ),
        ("stop", "assets/grid.xodr", "esmini")
    );
    assert_eq!((sc.clock_tick_ms, sc.seed), (10, Some(42)));
    assert_eq!(sc.actors["Ego"], vehicle);
    assert_eq!(sc.entities, ["Ego", "Lead"]);
    let p = &sc.parameters[0];
    assert_eq!(
        (p.name.as_str(), p.max, p.declared_value.as_str()),
        ("EgoTargetSpeed", Some(22.2), "13.9")
    );

    // An actor the storyboard does not have.
    std::fs::write(dir.join("scenario.yaml"), docs("Truck")).unwrap();
    let plan = plan_of(&channel, &dir).await;
    let scenario: Vec<_> = plan
        .problems
        .iter()
        .filter(|p| p.reason == ErrorReason::ScenarioInvalid as i32)
        .collect();
    assert_eq!(scenario.len(), 1, "{:?}", plan.problems);
    assert!(
        scenario[0].detail.contains("`Truck` is not an entity"),
        "{scenario:?}"
    );
    let refused = vxedge_api::v1::experiment_service_client::ExperimentServiceClient::new(channel)
        .up(vxedge_api::v1::ExperimentUpRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(
        [
            "ERROR_REASON_SCENARIO_INVALID",
            "ERROR_REASON_LOCAL_IMAGE_MISSING"
        ]
        .contains(&reason(&refused).as_str()),
        "{refused:?}"
    );
}

/// A package whose `requires.simulator` names a simulator the host lacks is
/// `ERROR_REASON_SIMULATOR_UNAVAILABLE` in `Plan`; with the simulator's executable on vxedged's
/// PATH it is not, and `HostService.Capabilities` lists it.
#[tokio::test]
async fn a_required_simulator_is_checked_against_the_host() {
    use vxedge_api::v1::ErrorReason;
    let root = scaffolded_package();
    let descriptor = std::fs::read_to_string(root.join("vxp.yaml")).unwrap();
    std::fs::write(
        root.join("vxp.yaml"),
        descriptor.replace("  gpu: none\n", "  gpu: none\n  simulator: [esmini]\n"),
    )
    .unwrap();
    let has = |plan: &vxedge_api::v1::WorkspacePlan| {
        plan.problems
            .iter()
            .any(|p| p.reason == ErrorReason::SimulatorUnavailable as i32)
    };
    // An empty PATH plus the system's, without any esmini.
    let bare = scratch();
    let daemon = Daemon::start_env(|_| {}, &[("PATH", &bare.join("none:/usr/bin:/bin"))]);
    let channel = daemon.channel().await;
    let plan = plan_of(&channel, &root).await;
    assert!(has(&plan), "{:?}", plan.problems);
    drop(daemon);

    let fake = scratch();
    let esmini = fake.join("esmini");
    std::fs::write(&esmini, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&esmini, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = PathBuf::from(format!("{}:/usr/bin:/bin", fake.display()));
    let daemon = Daemon::start_env(|_| {}, &[("PATH", &path)]);
    let channel = daemon.channel().await;
    let plan = plan_of(&channel, &root).await;
    assert!(!has(&plan), "{:?}", plan.problems);
    let caps = vxedge_api::v1::host_service_client::HostServiceClient::new(channel)
        .capabilities(vxedge_api::v1::HostCapabilitiesRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(caps.simulators, ["esmini"]);
}

/// A package imported by its bytes is listed, and every verb then reaches it by digest.
#[tokio::test]
async fn an_imported_package_is_listed_and_addressed_by_digest() {
    use vxedge_api::v1::package_service_client::PackageServiceClient;
    use vxedge_api::v1::{PackageImportChunk, PackageInspectRequest, PackageListRequest};
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let root = scaffolded_package();
    let bytes = vxedge_core::pkg::pack(&root).unwrap();
    let digest = vxedge_core::pkg::sha256(&bytes);
    let chunks: Vec<PackageImportChunk> = bytes
        .chunks(4096)
        .enumerate()
        .map(|(i, c)| PackageImportChunk {
            digest: if i == 0 {
                digest.clone()
            } else {
                String::new()
            },
            data: c.to_vec(),
        })
        .collect();
    let mut packages = PackageServiceClient::new(channel.clone());
    let job = packages
        .import(tokio_stream::iter(chunks))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        wait_for_job(channel.clone(), job.id).await,
        JobState::Succeeded as i32
    );
    let listed = packages
        .list(PackageListRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.packages.len(), 1, "{:?}", listed.packages);
    assert_eq!(listed.packages[0].digest, digest);
    assert_eq!(listed.packages[0].id, "local/pkgx");

    let by_digest = Some(ManifestRef {
        source: Some(Source::Package(digest.clone())),
        ..Default::default()
    });
    let info = packages
        .inspect(PackageInspectRequest {
            package: by_digest.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.digest, digest);
    let plan = vxedge_api::v1::workspace_service_client::WorkspaceServiceClient::new(channel)
        .plan(vxedge_api::v1::WorkspacePlanRequest {
            manifest: by_digest,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!plan.entities.is_empty(), "{plan:?}");

    // Bytes that do not hash to the named digest are refused, and nothing is kept.
    let bad = PackageImportChunk {
        digest: format!("sha256:{}", "1".repeat(64)),
        data: bytes,
    };
    let job = packages
        .import(tokio_stream::iter(vec![bad]))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        wait_for_job(daemon.channel().await, job.id).await,
        JobState::Failed as i32
    );
}

/// An experiment-level host telemetry service, which a campaign archives every trial through.
const TELEMETRY_SERVICE: &str = "services:\n  telemetry:\n    id: telemetry\n    type: telemetry\n    image: { url: local/vxtelemetry:host-process, kind: container }\n";

/// A campaign runs every trial as stages of its job and keeps going past a failed one: here no
/// trial can provision (the test host has no privileges), so each fails at `up`, `down` still
/// runs, and `campaign.json`, the trial's identity and its first phase marker are written. A
/// campaign without a telemetry service is refused, and an unknown id is `CAMPAIGN_UNKNOWN`.
#[tokio::test]
async fn a_campaign_records_each_trial_and_keeps_going_past_a_failure() {
    use vxedge_api::v1::campaign_service_client::CampaignServiceClient;
    use vxedge_api::v1::{
        CampaignGetRequest, CampaignListRequest, CampaignStartRequest, CampaignState,
        CampaignWatchRequest, TrialState,
    };
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, _) = experiment_workspace();
    let main = std::fs::read_to_string(dir.join("main.yaml")).unwrap();
    let mut client = CampaignServiceClient::new(channel.clone());

    let refused = client
        .start(CampaignStartRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_WORKSPACE_INVALID");
    assert!(
        refused.message().contains("telemetry"),
        "{}",
        refused.message()
    );

    std::fs::write(
        dir.join("main.yaml"),
        format!(
            "{main}\n{TELEMETRY_SERVICE}parameters:\n  n: {{ type: integer, unit: \"1\", default: 1 }}\n\
             campaign:\n  sweep: {{ n: [1, 2] }}\n  duration_s: 1\n  profiles: {{ quick: {{ repetitions: 1 }} }}\n"
        ),
    )
    .unwrap();
    let job = client
        .start(CampaignStartRequest {
            manifest: path_ref(&dir),
            request_id: "contract-1".into(),
            caller: "rpc_contract".into(),
            unlocked: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(job.kind, "campaign.run");
    assert_eq!(
        wait_for_job(channel.clone(), job.id.clone()).await,
        JobState::Failed as i32
    );

    let list = client
        .list(CampaignListRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.campaigns.len(), 1);
    let id = list.campaigns[0].campaign_id.clone();
    let status = client
        .get(CampaignGetRequest {
            campaign_id: id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.state, CampaignState::Failed as i32);
    assert_eq!(
        (
            status.job_id.as_str(),
            status.caller.as_str(),
            status.unlocked
        ),
        (job.id.as_str(), "rpc_contract", true)
    );
    let ids: Vec<&str> = status.trials.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["r1-defaults", "r1-n=2"]);
    for t in &status.trials {
        assert_eq!(t.state, TrialState::Failed as i32, "{t:?}");
        assert_eq!(t.phase, "provision");
        assert!(!t.error.is_empty());
    }
    let results = std::path::PathBuf::from(&status.results_dir);
    assert!(results.starts_with(&dir) && results.join("campaign.json").is_file());
    let ingest = daemon.root.join("run/ingest");
    let trial: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ingest.join("trial.json")).unwrap()).unwrap();
    assert_eq!(
        (trial["campaign_id"].as_str(), trial["trial_id"].as_str()),
        (Some(id.as_str()), Some("r1-n=2"))
    );
    let phases = std::fs::read_to_string(ingest.join("phases.jsonl")).unwrap();
    assert!(phases.contains("\"phase\":\"provision\""), "{phases}");

    let mut watch = client
        .watch(CampaignWatchRequest {
            campaign_id: id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    let first = tokio_stream::StreamExt::next(&mut watch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.state, CampaignState::Failed as i32);
    assert!(tokio_stream::StreamExt::next(&mut watch).await.is_none());

    // A retry with the same request_id and another profile is refused, never merged; with the
    // same profile it resumes the same campaign.
    let retry = |profile: &str| CampaignStartRequest {
        manifest: path_ref(&dir),
        request_id: "contract-1".into(),
        profile: profile.into(),
        unlocked: true,
        ..Default::default()
    };
    let refused = client.start(retry("quick")).await.unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_PARAMETER_INVALID");
    assert!(
        refused.message().contains("profile"),
        "{}",
        refused.message()
    );
    let again = client.start(retry("")).await.unwrap().into_inner();
    wait_for_job(channel.clone(), again.id.clone()).await;
    let list = client
        .list(CampaignListRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.campaigns.len(), 1);
    assert_eq!(list.campaigns[0].campaign_id, id);

    let unknown = client
        .get(CampaignGetRequest {
            campaign_id: "c-nope".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&unknown), "ERROR_REASON_CAMPAIGN_UNKNOWN");
}

/// `CampaignService.Plan` expands arms and sweep into trials, labelling a sweep point that equals
/// an arm, applies a profile, and checks each point's bound document.
#[tokio::test]
async fn campaign_plan_expands_trials_and_checks_each_point() {
    use vxedge_api::v1::CampaignPlanRequest;
    use vxedge_api::v1::campaign_service_client::CampaignServiceClient;
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, _) = experiment_workspace();
    let main = std::fs::read_to_string(dir.join("main.yaml")).unwrap();
    let write = |campaign: &str| {
        std::fs::write(
            dir.join("main.yaml"),
            format!(
                "{main}\n{TELEMETRY_SERVICE}measurements:\n  rtt: {{ kind: rtt-probe, from: ecu1, to: internet, slice: adas, rate_pps: 100 }}\n\
                 parameters:\n  pps: {{ type: integer, unit: 1/s, default: 100, min: 0, max: 1000, binds: \"measurements[rtt].rate_pps\" }}\n\
                 campaign:\n{campaign}"
            ),
        )
        .unwrap()
    };
    write(
        "  arms: { low: { pps: 100 }, high: { pps: 500 } }\n  sweep: { pps: [100, 300] }\n  repetitions: 2\n  duration_s: 20\n  profiles: { quick: { repetitions: 1, duration_s: 5 } }\n",
    );
    let mut client = CampaignServiceClient::new(channel);
    let plan = client
        .plan(CampaignPlanRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let ids: Vec<&str> = plan.trials.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "r1-low",
            "r1-pps=300",
            "r1-high",
            "r2-low",
            "r2-pps=300",
            "r2-high"
        ]
    );
    assert_eq!((plan.points, plan.repetitions), (3, 2));
    assert_eq!(plan.estimated_s, 6.0 * (20.0 + 30.0));
    assert!(plan.problems.is_empty(), "{:?}", plan.problems);

    let quick = client
        .plan(CampaignPlanRequest {
            manifest: path_ref(&dir),
            profile: "quick".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!((quick.trials.len(), quick.trials[0].duration_s), (3, 5.0));

    // A value the parameter allows can still break the document it binds: rate 0 is no probe.
    write("  sweep: { pps: [100, 0] }\n");
    let broken = client
        .plan(CampaignPlanRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(broken.problems.len(), 1, "{:?}", broken.problems);
    assert_eq!(
        broken.problems[0].reason,
        vxedge_api::v1::ErrorReason::MeasurementInvalid as i32
    );
    assert!(
        broken.problems[0].detail.contains("point `pps=0`"),
        "{}",
        broken.problems[0].detail
    );

    // Measure windows: the plan lists each trial's windows with their own values, and checks
    // each window's document, since that is what the window runs.
    write(
        "  repetitions: 2
  windows:
    - { id: cold, duration_s: 3, set: { pps: 20 } }
    - { id: steady, duration_s: 60 }
",
    );
    let windowed = client
        .plan(CampaignPlanRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(windowed.problems.is_empty(), "{:?}", windowed.problems);
    let t = &windowed.trials[0];
    assert_eq!(t.duration_s, 63.0);
    let windows: Vec<(&str, f64)> = t
        .windows
        .iter()
        .map(|w| (w.id.as_str(), w.duration_s))
        .collect();
    assert_eq!(windows, [("cold", 3.0), ("steady", 60.0)]);
    use vxedge_api::v1::param_value::Value as V;
    assert_eq!(t.windows[0].parameters["pps"].value, Some(V::Integer(20)));
    assert_eq!(t.windows[1].parameters["pps"].value, Some(V::Integer(100)));

    write(
        "  windows:
    - { id: cold, duration_s: 3, set: { pps: 0 } }
    - { id: steady, duration_s: 60 }
",
    );
    let broken = client
        .plan(CampaignPlanRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(broken.problems.len(), 1, "{:?}", broken.problems);
    assert!(
        broken.problems[0].detail.contains("window `cold`"),
        "{}",
        broken.problems[0].detail
    );
}

/// A trial archive with a trial in its metadata, as a campaign writes one.
fn write_archive(path: &Path, trial_id: &str) {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
    zip.start_file("metadata.json", opts).unwrap();
    zip.write_all(
        serde_json::json!({
            "archive": "vxarchive/2", "experiment_id": "e1",
            "package": { "id": "local/e1", "version": "0.1.0", "digest": "sha256:abc", "locked": false },
            "collected_at_unix_ms": 1, "vxtelemetry_version": "0.5.0", "kernel": { "release": "7.0.0" },
            "trial": { "campaign_id": "c-1", "trial_id": trial_id, "arm": "idle", "repetition": 2,
                       "parameters": { "load_mbps": 600 } },
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    zip.finish().unwrap();
}

/// `ResultService` lists archives from their metadata only, finds them again by digest, and
/// streams their bytes; an unknown digest is `RESULT_UNKNOWN`.
#[tokio::test]
async fn results_are_listed_from_their_metadata_and_fetched_by_digest() {
    use vxedge_api::v1::result_service_client::ResultServiceClient;
    use vxedge_api::v1::{ResultFetchRequest, ResultGetRequest, ResultListRequest, param_value};
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let ws = scratch();
    let archive = ws.join("results/c-1/r2-idle.zip");
    write_archive(&archive, "r2-idle");
    let bytes = std::fs::read(&archive).unwrap();
    let digest = vxedge_core::pkg::sha256(&bytes);
    let mut client = ResultServiceClient::new(channel);
    let list = client
        .list(ResultListRequest {
            workspace: path_ref(&ws),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.results.len(), 1, "{:?}", list.results);
    let r = &list.results[0];
    assert_eq!(
        (
            r.digest.as_str(),
            r.archive.as_str(),
            r.experiment_id.as_str()
        ),
        (digest.as_str(), "vxarchive/2", "e1")
    );
    assert_eq!(
        (r.package_id.as_str(), r.package_locked),
        ("local/e1", Some(false))
    );
    let t = r.trial.as_ref().unwrap();
    assert_eq!(
        (t.trial_id.as_str(), t.arm.as_str(), t.repetition),
        ("r2-idle", "idle", 2)
    );
    assert_eq!(
        t.parameters["load_mbps"].value,
        Some(param_value::Value::Integer(600))
    );
    let got = client
        .get(ResultGetRequest {
            digest: digest.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.size_bytes, bytes.len() as u64);
    let mut stream = client
        .fetch(ResultFetchRequest {
            digest: digest.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    let mut fetched = Vec::new();
    let mut first = true;
    while let Some(chunk) = tokio_stream::StreamExt::next(&mut stream).await {
        let chunk = chunk.unwrap();
        if first {
            assert_eq!(chunk.digest, digest);
            first = false;
        }
        fetched.extend(chunk.data);
    }
    assert_eq!(fetched, bytes);
    let unknown = client
        .get(ResultGetRequest {
            digest: format!("sha256:{}", "0".repeat(64)),
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&unknown), "ERROR_REASON_RESULT_UNKNOWN");
}

/// A `vxarchive/1` archive whose probe served `p99` as its last 0.99 quantile.
fn write_v1_archive(path: &Path, p99: f64) {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
    zip.start_file("metadata.json", opts).unwrap();
    zip.write_all(br#"{"experiment_id":"e1","ingest":{"scrapes_failed":0}}"#)
        .unwrap();
    zip.start_file("ingest/samples.jsonl", opts).unwrap();
    for (ts, v) in [(1, p99 / 2.0), (2, p99)] {
        writeln!(
            zip,
            "{}",
            serde_json::json!({ "ts_unix_ms": ts, "target": "10.0.0.2:9801",
                "metric": "vxprobe_rtt_seconds", "labels": "{quantile=\"0.99\"}", "value": v })
        )
        .unwrap();
    }
    zip.finish().unwrap();
}

/// `ResultService.Verdict` judges the experiment's criteria over a campaign's archived trials,
/// reading the archives; `CampaignStatus` carries the verdict recorded at the end; an unknown
/// campaign is `CAMPAIGN_UNKNOWN`. `Compare` sets the campaign against a reference this host ran,
/// and against the same reference frozen in another workspace's `results/`, whose archives must
/// hash to its record and stay inside its folder.
#[tokio::test]
async fn a_campaign_verdict_is_judged_from_its_archives() {
    use vxedge_api::v1::campaign_service_client::CampaignServiceClient;
    use vxedge_api::v1::result_service_client::ResultServiceClient;
    use vxedge_api::v1::{
        CampaignGetRequest, ResultCompareRequest, ResultVerdictRequest, VerdictOutcome,
    };
    let (dir, _) = experiment_workspace();
    let main = std::fs::read_to_string(dir.join("main.yaml")).unwrap();
    let intent = "\nhypothesis: { id: H1, statement: busy p99 stays within 2x of idle }\n\
parameters:\n  load_mbps: { type: integer, unit: Mbit/s, default: 0, min: 0, max: 1000 }\n\
metrics:\n  - { id: p99, kind: gauge, unit: s, entity: measurements/rtt, series: vxprobe_rtt_seconds, labels: { quantile: \"0.99\" } }\n\
criteria:\n  - { id: h1, op: le, lhs: { metric: p99, aggregate: median, select: { arm: busy } }, rhs: { metric: p99, aggregate: median, select: { arm: idle }, factor: 2.0 } }\n";
    std::fs::write(dir.join("main.yaml"), format!("{main}{intent}")).unwrap();
    let results = dir.join("results/c-1");
    let mut trials = Vec::new();
    for (rep, arm, load, p99) in [
        (1, "idle", 0, 0.001),
        (1, "busy", 600, 0.0015),
        (2, "idle", 0, 0.002),
        (2, "busy", 600, 0.0025),
    ] {
        let id = format!("r{rep}-{arm}");
        let archive = results.join(format!("{id}.zip"));
        write_v1_archive(&archive, p99);
        trials.push(serde_json::json!({
            "id": id, "arm": arm, "repetition": rep, "parameters": { "load_mbps": load },
            "state": "succeeded", "archive": archive.display().to_string(), "sha256": "ab",
        }));
    }
    let record = |id: &str, trials: &[serde_json::Value]| {
        serde_json::json!({
            "campaign_id": id, "state": "succeeded", "job_id": "j", "experiment_id": "e1",
            "workspace": dir.display().to_string(), "results_dir": results.display().to_string(),
            "trials": trials, "started_unix_ms": 1, "finished_unix_ms": 2, "verdict": "pass",
        })
    };
    // The reference: the same idle trials, and a busy point twice as slow, so busy is outside.
    let mut reference = trials[..1].to_vec();
    reference.push(trials[2].clone());
    for (rep, p99) in [(1, 0.004), (2, 0.005)] {
        let id = format!("r{rep}-busy");
        let archive = dir.join(format!("results/ref/{id}.zip"));
        write_v1_archive(&archive, p99);
        reference.push(serde_json::json!({
            "id": id, "arm": "busy", "repetition": rep, "parameters": { "load_mbps": 600 },
            "state": "succeeded", "archive": archive.display().to_string(), "sha256": "cd",
        }));
    }
    let (c1, c2) = (record("c-1", &trials), record("ref", &reference));
    let daemon = Daemon::start_with(|rt| {
        std::fs::create_dir_all(rt.join("campaigns")).unwrap();
        std::fs::write(rt.join("campaigns/c-1.json"), c1.to_string()).unwrap();
        std::fs::write(rt.join("campaigns/ref.json"), c2.to_string()).unwrap();
    });
    let channel = daemon.channel().await;
    let v = ResultServiceClient::new(channel.clone())
        .verdict(ResultVerdictRequest {
            campaign_id: "c-1".into(),
            reference_workspace: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(v.outcome, VerdictOutcome::Pass as i32, "{v:?}");
    assert_eq!(v.hypothesis, "H1");
    assert_eq!(v.trials.len(), 4);
    assert_eq!(v.trials[0].archive_digest, "sha256:ab");
    let h1 = &v.criteria[0];
    assert_eq!((h1.id.as_str(), h1.rule.as_str()), ("h1", "le"));
    assert_eq!((h1.lhs, h1.rhs), (Some(0.002), Some(0.003)));
    assert_eq!(h1.reads[0].trials, ["r1-busy", "r2-busy"]);

    let status = CampaignServiceClient::new(channel.clone())
        .get(CampaignGetRequest {
            campaign_id: "c-1".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.verdict, VerdictOutcome::Pass as i32);

    let c = ResultServiceClient::new(channel.clone())
        .compare(ResultCompareRequest {
            campaign_id: "c-1".into(),
            reference_campaign_id: "ref".into(),
            reference_workspace: None,
        })
        .await
        .unwrap()
        .into_inner();
    let points: Vec<(&str, bool)> = c
        .metrics
        .iter()
        .map(|m| (m.point.as_str(), m.within_reference_iqr))
        .collect();
    assert_eq!(points, [("idle", true), ("busy", false)], "{c:?}");
    assert_eq!(c.within, 1);
    assert_eq!(
        (c.outcome, c.reference_outcome),
        (VerdictOutcome::Pass as i32, VerdictOutcome::Fail as i32)
    );
    assert_eq!(c.criteria[0].id, "h1");

    // The same reference, frozen: published in another workspace's `results/` with a record that
    // names its trials. One archive is named relative to the record, one by the absolute path
    // of the host that wrote it; both are read from the record's folder and hashed.
    let frozen = tempfile::tempdir().unwrap();
    let folder = frozen.path().join("results/exp-ref");
    let sha = |p: &Path| {
        use sha2::Digest;
        let d = sha2::Sha256::digest(std::fs::read(p).unwrap());
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let mut published = Vec::new();
    for (i, t) in reference.iter().enumerate() {
        let id = t["id"].as_str().unwrap();
        let copy = folder.join(format!("{id}.zip"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::copy(t["archive"].as_str().unwrap(), &copy).unwrap();
        let named = if i == 0 {
            format!("{id}.zip")
        } else {
            format!("/elsewhere/results/exp-ref/{id}.zip")
        };
        let mut t = t.clone();
        t["archive"] = named.into();
        t["sha256"] = sha(&copy).into();
        published.push(t);
    }
    let record = serde_json::json!({ "campaign_id": "exp-ref", "trials": published });
    std::fs::write(folder.join("campaign.json"), record.to_string()).unwrap();
    let workspace = vxedge_api::v1::ManifestRef {
        source: Some(vxedge_api::v1::manifest_ref::Source::Path(
            frozen.path().display().to_string(),
        )),
        ..Default::default()
    };
    let against = |id: &str| ResultCompareRequest {
        campaign_id: "c-1".into(),
        reference_campaign_id: id.into(),
        reference_workspace: Some(workspace.clone()),
    };
    let f = ResultServiceClient::new(channel.clone())
        .compare(against("exp-ref"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(f.reference_campaign_id, "exp-ref");
    assert_eq!(
        (&f.metrics, &f.criteria, f.within),
        (&c.metrics, &c.criteria, c.within)
    );
    assert_eq!(f.reference_outcome, c.reference_outcome);

    // An archive that is not the one published is refused, as is one outside the folder.
    let mut tampered = record.clone();
    tampered["trials"][1]["sha256"] = "00".into();
    std::fs::write(folder.join("campaign.json"), tampered.to_string()).unwrap();
    let err = ResultServiceClient::new(channel.clone())
        .compare(against("exp-ref"))
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "ERROR_REASON_CHECKSUM_MISMATCH", "{err:?}");
    let mut outside = record.clone();
    outside["trials"][0]["archive"] = "../c-1/r1-idle.zip".into();
    std::fs::write(folder.join("campaign.json"), outside.to_string()).unwrap();
    let err = ResultServiceClient::new(channel.clone())
        .compare(against("exp-ref"))
        .await
        .unwrap_err();
    assert_eq!(reason(&err), "ERROR_REASON_RESULT_UNKNOWN", "{err:?}");
    for id in ["nope", "../exp-ref"] {
        let err = ResultServiceClient::new(channel.clone())
            .compare(against(id))
            .await
            .unwrap_err();
        assert_eq!(
            reason(&err),
            "ERROR_REASON_CAMPAIGN_UNKNOWN",
            "{id}: {err:?}"
        );
    }

    // A criterion that reads a declared reference campaign: one this host ran, one recorded in
    // another workspace, or one that cannot be found, which leaves it inconclusive.
    std::fs::write(folder.join("campaign.json"), record.to_string()).unwrap();
    let with_reference = |campaign: &str| {
        let intent = intent.replace(
            "criteria:\n",
            &format!(
                "references:\n  base: {{ campaign: {campaign} }}\ncriteria:\n  - {{ id: h2, op: le, \
                 lhs: {{ metric: p99, aggregate: median, select: {{ arm: busy }} }}, \
                 rhs: {{ metric: p99, aggregate: median, select: {{ arm: busy }}, reference: base }} }}\n"
            ),
        );
        std::fs::write(dir.join("main.yaml"), format!("{main}{intent}")).unwrap();
    };
    let judge = |workspace: Option<vxedge_api::v1::ManifestRef>| {
        let channel = channel.clone();
        async move {
            ResultServiceClient::new(channel)
                .verdict(ResultVerdictRequest {
                    campaign_id: "c-1".into(),
                    reference_workspace: workspace,
                })
                .await
                .unwrap()
                .into_inner()
        }
    };
    with_reference("ref");
    let v = judge(None).await;
    let h2 = &v.criteria[0];
    assert_eq!(
        (h2.id.as_str(), h2.outcome),
        ("h2", VerdictOutcome::Pass as i32),
        "{v:?}"
    );
    assert_eq!(h2.lhs, Some(0.002));
    assert!((h2.rhs.unwrap() - 0.0045).abs() < 1e-12, "{:?}", h2.rhs);
    assert_eq!(h2.reads[1].reference, "base");
    assert_eq!(h2.reads[1].trials, ["r1-busy", "r2-busy"]);

    with_reference("exp-ref");
    let v = judge(Some(workspace.clone())).await;
    assert_eq!(v.criteria[0].outcome, VerdictOutcome::Pass as i32, "{v:?}");
    assert!((v.criteria[0].rhs.unwrap() - 0.0045).abs() < 1e-12);

    // A campaign started with `reference_workspace` records it, and a verdict that names none
    // reads the references from there.
    let recorded_at = daemon.root.join("run/campaigns/c-1.json");
    let mut started_with = c1.clone();
    started_with["reference_workspace"] = frozen.path().display().to_string().into();
    std::fs::write(&recorded_at, started_with.to_string()).unwrap();
    let v = judge(None).await;
    assert_eq!(v.criteria[0].outcome, VerdictOutcome::Pass as i32, "{v:?}");
    std::fs::write(&recorded_at, c1.to_string()).unwrap();

    with_reference("exp-gone");
    let v = judge(None).await;
    assert_eq!(v.criteria[0].outcome, VerdictOutcome::Inconclusive as i32);
    assert_eq!(v.outcome, VerdictOutcome::Inconclusive as i32);
    assert!(
        v.criteria[0].why.contains("reference `base`") && v.criteria[0].why.contains("exp-gone"),
        "{}",
        v.criteria[0].why
    );
    std::fs::write(dir.join("main.yaml"), format!("{main}{intent}")).unwrap();

    let unknown = ResultServiceClient::new(channel)
        .verdict(ResultVerdictRequest {
            campaign_id: "nope".into(),
            reference_workspace: None,
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&unknown), "ERROR_REASON_CAMPAIGN_UNKNOWN");
}

/// Typed intent that holds adds no problem.
#[tokio::test]
async fn typed_intent_that_holds_plans_clean() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let (dir, _) = experiment_workspace();
    let main = std::fs::read_to_string(dir.join("main.yaml")).unwrap();
    let good = BAD_INTENT
        .replace("default: 5000", "default: 0")
        .replace("metric: rtt,", "metric: rtt_seconds,");
    std::fs::write(dir.join("main.yaml"), format!("{main}{good}")).unwrap();
    let plan = plan_of(&channel, &dir).await;
    assert!(
        !plan.problems.iter().any(|p| p.reason
            == vxedge_api::v1::ErrorReason::ParameterInvalid as i32
            || p.reason == vxedge_api::v1::ErrorReason::CriterionInvalid as i32
            || p.reason == vxedge_api::v1::ErrorReason::WorkspaceInvalid as i32),
        "{:?}",
        plan.problems
    );
}

fn signature_problems(plan: &vxedge_api::v1::WorkspacePlan) -> Vec<String> {
    plan.problems
        .iter()
        .filter(|p| p.reason == vxedge_api::v1::ErrorReason::SignatureInvalid as i32)
        .map(|p| p.detail.clone())
        .collect()
}

/// A verification record as `pkg verify` writes it, in the daemon's part cache.
fn write_verification(cache: &Path, digest: &str, fingerprint: &str) {
    let dir = cache.join("packages");
    std::fs::create_dir_all(&dir).unwrap();
    let record = serde_json::json!({
        "package_digest": digest, "method": "sigstore-key",
        "signer": { "kind": "key", "fingerprint": fingerprint },
        "verified_at_unix_ms": 1,
    });
    std::fs::write(
        dir.join(format!(
            "{}.verified.json",
            digest.trim_start_matches("sha256:")
        )),
        record.to_string(),
    )
    .unwrap();
}

/// Under `require_signed`, `Plan` and `up` refuse a package nobody verified here, one verified
/// as signed by a signer the policy does not name, and a legacy workspace, all before anything
/// is provisioned, and accept one an allowed signer verified.
#[tokio::test]
async fn a_host_that_requires_signatures_refuses_what_is_not_verified() {
    let policy_dir = scratch();
    let policy = policy_dir.join("trust.yaml");
    std::fs::write(
        &policy,
        "require_signed: true\nsigners:\n  - { kind: key, fingerprint: \"sha256:lab\" }\n",
    )
    .unwrap();
    let daemon = Daemon::start_env(|_| {}, &[("VXEDGE_TRUST_POLICY", &policy)]);
    let channel = daemon.channel().await;
    let cache = daemon.root.join("cache");
    let (dir, _) = locked_package();
    let digest = vxedge_core::pkg::package_digest(&dir).unwrap();

    let problems = signature_problems(&plan_of(&channel, &dir).await);
    assert!(
        problems[0].contains("has not been verified"),
        "{problems:?}"
    );
    let refused = ExperimentServiceClient::new(channel.clone())
        .up(ExperimentUpRequest {
            manifest: path_ref(&dir),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_SIGNATURE_INVALID");
    let refused =
        vxedge_api::v1::gateway_service_client::GatewayServiceClient::new(channel.clone())
            .up(vxedge_api::v1::GatewayUpRequest {
                manifest: path_ref(&dir),
                ..Default::default()
            })
            .await
            .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_SIGNATURE_INVALID");
    // `PackageService.Pull` applies the same policy, before anything is fetched.
    let refused =
        vxedge_api::v1::package_service_client::PackageServiceClient::new(channel.clone())
            .pull(vxedge_api::v1::PackagePullRequest {
                reference: format!("ghcr.io/vxedge/experiments/x@{digest}"),
                ..Default::default()
            })
            .await
            .unwrap_err();
    assert_eq!(reason(&refused), "ERROR_REASON_SIGNATURE_INVALID");

    write_verification(&cache, &digest, "sha256:someone-else");
    let problems = signature_problems(&plan_of(&channel, &dir).await);
    assert!(problems[0].contains("does not allow"), "{problems:?}");

    write_verification(&cache, &digest, "sha256:lab");
    assert_eq!(
        signature_problems(&plan_of(&channel, &dir).await),
        Vec::<String>::new()
    );

    // A change after verification is another digest, so the record no longer counts.
    std::fs::write(dir.join("notes.txt"), "edited\n").unwrap();
    let edited =
        vxedge_api::v1::workspace_service_client::WorkspaceServiceClient::new(channel.clone())
            .plan(vxedge_api::v1::WorkspacePlanRequest {
                manifest: path_ref(&dir),
                unlocked: true,
            })
            .await
            .unwrap()
            .into_inner();
    assert!(
        !signature_problems(&edited).is_empty(),
        "{:?}",
        edited.problems
    );

    let (legacy, id) = experiment_workspace();
    std::fs::write(
        legacy.join(format!("{id}.vxp")),
        format!("id: {id}\nversion: 0.1.0\n"),
    )
    .unwrap();
    let problems = signature_problems(&plan_of(&channel, &legacy).await);
    assert!(problems[0].contains("legacy"), "{problems:?}");
}

/// `Plan` names every entity telemetry records will use, derived from ids, with the scrape
/// targets bound to each; `collect` writes the same list beside what was ingested.
#[tokio::test]
async fn plan_and_collect_name_the_entities_records_use() {
    let (ws, _) = experiment_workspace();
    let bin = fake_vxtelemetry(0);
    let daemon = Daemon::start_env(
        |run| std::fs::create_dir_all(run.join("ingest")).unwrap(),
        &[("VXEDGE_VXTELEMETRY_BIN", &bin)],
    );
    let channel = daemon.channel().await;
    let plan = plan_of(&channel, &ws).await;
    let paths: Vec<&str> = plan.entities.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths[0], "host", "{paths:?}");
    assert!(paths.contains(&"car/front/ecu1"), "{paths:?}");
    assert!(paths.contains(&"car/links/l1"), "{paths:?}");
    let ecu = plan
        .entities
        .iter()
        .find(|e| e.path == "car/front/ecu1")
        .unwrap();
    assert_eq!(
        (ecu.kind.as_str(), ecu.role.as_str(), ecu.backend.as_str()),
        ("node", "zone-controller", "container")
    );

    let job =
        vxedge_api::v1::telemetry_service_client::TelemetryServiceClient::new(channel.clone())
            .collect(vxedge_api::v1::CollectRequest {
                manifest: path_ref(&ws),
                out_path: "results/run.zip".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
    assert_eq!(
        wait_for_job(channel, job.id).await,
        JobState::Succeeded as i32
    );
    let written = std::fs::read_to_string(daemon.root.join("run/ingest/entities.json")).unwrap();
    let listed: serde_json::Value = serde_json::from_str(&written).unwrap();
    let listed: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(listed, paths, "Plan and the archive agree");
    let logs_of = |path: &str| -> Vec<(String, String)> {
        plan.entities
            .iter()
            .find(|e| e.path == path)
            .unwrap()
            .logs
            .iter()
            .map(|l| (l.stream.clone(), l.path.clone()))
            .collect()
    };
    let written: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(daemon.root.join("run/ingest/entities.json")).unwrap(),
    )
    .unwrap();
    for e in written.as_array().unwrap() {
        let in_file: Vec<(String, String)> = e["logs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|l| {
                (
                    l["stream"].as_str().unwrap().into(),
                    l["path"].as_str().unwrap().into(),
                )
            })
            .collect();
        assert_eq!(in_file, logs_of(e["path"].as_str().unwrap()), "{e}");
    }
}

/// `LogService.Follow` carries the log lines the ingest read, with a cursor and filters by entity
/// and stream, and follows as more are read (§11).
#[tokio::test]
async fn follow_carries_the_ingest_log_lines() {
    use tokio_stream::StreamExt;
    use vxedge_api::v1::LogFollowRequest;
    let line = |entity: &str, stream: &str, text: &str| {
        format!(
            r#"{{"record":"log","ts_ns":7,"entity":"{entity}","stream":"{stream}","line":"{text}"}}"#
        ) + "\n"
    };
    let first = line("car/_/hpc1", "serial", "login:")
        + &line("host/services/telemetry", "stdout", "ingest starting")
        + &line("car/_/hpc1", "vmm", "vcpu 0 up");
    let daemon = Daemon::start_with(|run| {
        std::fs::create_dir_all(run.join("ingest")).unwrap();
        std::fs::write(run.join("ingest/logs.jsonl"), &first).unwrap();
    });
    let mut client =
        vxedge_api::v1::log_service_client::LogServiceClient::new(daemon.channel().await);
    let all: Vec<(i64, String)> = client
        .follow(LogFollowRequest::default())
        .await
        .unwrap()
        .into_inner()
        .map(|r| {
            let r = r.unwrap();
            (r.seq, r.line)
        })
        .collect()
        .await;
    assert_eq!(
        all,
        [
            (1, "login:".into()),
            (2, "ingest starting".into()),
            (3, "vcpu 0 up".into())
        ]
    );

    let mut serial = client
        .follow(LogFollowRequest {
            entity_prefix: "car".into(),
            streams: vec!["serial".into()],
            follow: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let r = serial.next().await.unwrap().unwrap();
    assert_eq!((r.seq, r.entity.as_str(), r.ts_ns), (1, "car/_/hpc1", 7));
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(daemon.root.join("run/ingest/logs.jsonl"))
        .unwrap()
        .write_all(line("car/_/hpc1", "serial", "root@hpc1:~#").as_bytes())
        .unwrap();
    let r = serial.next().await.unwrap().unwrap();
    assert_eq!(
        (r.seq, r.line.as_str()),
        (4, "root@hpc1:~#"),
        "followed past the vmm line"
    );
}

/// `StreamRecords` carries the ingest's records in order, with a cursor and an entity filter;
/// `StreamMetrics` carries its samples by selector (`ARCHITECTURE.md` §11).
#[tokio::test]
async fn live_streams_carry_the_ingest_records() {
    use tokio_stream::StreamExt;
    use vxedge_api::v1::{StreamMetricsRequest, StreamRecordsRequest, record};
    let daemon = Daemon::start_with(|run| {
        std::fs::create_dir_all(run.join("ingest")).unwrap();
        std::fs::write(
            run.join("ingest/records.jsonl"),
            concat!(
                r#"{"record":"phase","ts_ns":1,"trial":"t1","phase":"measure"}"#, "\n",
                r#"{"record":"sample","ts_ns":2000000,"entity":"car/front/ecu1","metric":"rtt_seconds","labels":{"quantile":"0.99"},"value":0.5}"#, "\n",
                r#"{"record":"gap","entity":"host","from_ns":3,"to_ns":4,"reason":"scrape failed"}"#, "\n",
                r#"{"record":"sample","ts_ns":5000000,"entity":"host","metric":"vxedge_process_memory_bytes","value":7}"#, "\n",
            ),
        )
        .unwrap();
    });
    let mut client = vxedge_api::v1::telemetry_service_client::TelemetryServiceClient::new(
        daemon.channel().await,
    );
    let all: Vec<_> = client
        .stream_records(StreamRecordsRequest::default())
        .await
        .unwrap()
        .into_inner()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(all.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2, 3, 4]);
    assert!(matches!(all[2].record, Some(record::Record::Gap(_))));

    let car: Vec<i64> = client
        .stream_records(StreamRecordsRequest {
            after: 1,
            entity_prefix: "car".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner()
        .map(|r| r.unwrap().seq)
        .collect()
        .await;
    assert_eq!(car, [2], "after the cursor, under `car` only");

    let mut live = client
        .stream_metrics(StreamMetricsRequest {
            selector: [("entity".to_string(), "host".to_string())].into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let m = live.next().await.unwrap().unwrap();
    assert_eq!(
        (
            m.name.as_str(),
            m.entity.as_str(),
            m.value,
            m.ts_unix_ms,
            m.ts_ns
        ),
        ("vxedge_process_memory_bytes", "host", 7.0, 5, 5_000_000)
    );
}

/// The daemon's exporter is a `host` scrape target in both the plan and `entities.json`, and
/// serves Prometheus text whose series name their entities (§12).
#[tokio::test]
async fn the_daemon_exporter_is_a_host_target_serving_entity_series() {
    let (ws, _) = experiment_workspace();
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let plan = plan_of(&channel, &ws).await;
    let host = &plan.entities[0];
    assert_eq!(host.path, "host");
    let target = host
        .scrape_targets
        .iter()
        .find(|t| t.starts_with("127.0.0.1:"))
        .expect("the exporter is bound to host")
        .clone();

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let get = |path: &'static str| {
        let target = target.clone();
        async move {
            let mut s = tokio::net::TcpStream::connect(&target).await.unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nhost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        }
    };
    let page = get("/metrics").await;
    assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page}");
    assert!(
        page.contains("# TYPE vxedge_process_cpu_seconds_total counter\n")
            && page.contains("vxedge_process_memory_bytes{entity=\"host\",process=\"vxedged\"}"),
        "{page}"
    );
    assert!(get("/").await.starts_with("HTTP/1.1 404"));
}
