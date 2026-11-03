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
    let yaml =
        vxedge_core::model::synthesize_vehicle_workspace(&vehicle_with_signal_plane()).unwrap();
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

    /// `seed` runs on the runtime dir before the daemon starts, as a previous daemon's leftovers.
    fn start_with(seed: impl FnOnce(&Path)) -> Self {
        let root = scratch();
        let runtime = root.join("run");
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

/// The contract is ahead of the daemon until 0.5.0. Each RPC leaves this list when it is built, and
/// a release needs the list empty, so a stub cannot ship by accident.
#[tokio::test]
async fn rpcs_still_to_build_answer_unimplemented() {
    use tonic::Code::Unimplemented;
    use vxedge_api::v1::host_service_client::HostServiceClient;
    use vxedge_api::v1::workspace_service_client::WorkspaceServiceClient;

    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let codes = [
        TrafficServiceClient::new(channel.clone())
            .status(vxedge_api::v1::TrafficStatusRequest::default())
            .await
            .map(|_| ())
            .unwrap_err()
            .code(),
        WorkspaceServiceClient::new(channel.clone())
            .plan(vxedge_api::v1::WorkspacePlanRequest::default())
            .await
            .map(|_| ())
            .unwrap_err()
            .code(),
        HostServiceClient::new(channel)
            .capabilities(vxedge_api::v1::HostCapabilitiesRequest::default())
            .await
            .map(|_| ())
            .unwrap_err()
            .code(),
    ];
    assert_eq!(codes, [Unimplemented; 3]);
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
  idle: { role: ecu, zone: z, backend: container }
functions:
  fa: { deployed_on: a }
  fb: { deployed_on: b }
  fc: { deployed_on: c }
  fd: { deployed_on: d }
  fe: { deployed_on: e }
  ff: { deployed_on: f }
links:
  - { id: la, endpoints: [{ node: a, port: eth0, ip: 10.2.0.1/24 }], slice: adas }
  - { id: lb, endpoints: [{ node: b, port: eth0, ip: 10.2.0.2/24 }], slice: adas }
  - { id: lc, endpoints: [{ node: c, port: eth0, ip: 10.2.0.3/24 }], slice: adas }
  - { id: ld, endpoints: [{ node: d, port: eth0, ip: 10.2.0.4/24 }], slice: adas }
  - { id: le, endpoints: [{ node: e, port: eth0, ip: 10.2.0.5/24 }], slice: adas }
  - { id: lf, endpoints: [{ node: f, port: eth0, ip: 10.2.0.6/24 }], slice: adas }
---
kind: baseline
id: bl
parts:
  - { ref: 'local/vxprobe:host-process', target_ids: ff }
  - ref: ghcr.io/vxedge/parts/probe:1
    digest: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    target_ids: fa
  - { ref: ./bundle, kind: bundle, target_ids: [fb, fc, fd] }
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
        "started_unix_ms": 1234, "sha256": vxedge_core::Workspace::content_sha256(dir).unwrap(),
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
        vxedge_core::Workspace::content_sha256(&ws).unwrap()
    );
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
    assert_eq!(status.nodes.len(), 6, "typed entries for provisioned nodes");
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
