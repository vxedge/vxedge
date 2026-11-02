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
use vxedge_api::v1::svc_service_client::SvcServiceClient;
use vxedge_api::v1::zone_service_client::ZoneServiceClient;
use vxedge_api::v1::{
    ComputeListRequest, ExperimentStatusRequest, GetJobRequest, JobState, ManifestRef,
    ResourceKind, ResourceState, SubscribeRequest, SvcDownRequest, SvcStatusRequest, SvcUpRequest,
    ZoneListRequest, manifest_ref::Source,
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
        .list(ZoneListRequest {})
        .await
        .unwrap()
        .into_inner()
        .zones;
    let mut ids: Vec<&str> = zones.iter().map(|z| z.vxid.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["ecu1", "hpc1"]);
    let nodes = ComputeServiceClient::new(channel)
        .list(ComputeListRequest {})
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
        .list(ZoneListRequest {})
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
async fn a_zone_list_labels_hpc_devices_as_compute() {
    let ws = vehicle_workspace();
    let daemon = Daemon::start_with(|run| write_active_record(run, &vehicle_id(), &ws, None));
    let zones = ZoneServiceClient::new(daemon.channel().await)
        .list(ZoneListRequest {})
        .await
        .unwrap()
        .into_inner()
        .zones;
    let kind_of = |id: &str| zones.iter().find(|z| z.vxid == id).unwrap().kind;
    assert_eq!(kind_of("ecu1"), ResourceKind::Zone as i32);
    assert_eq!(kind_of("hpc1"), ResourceKind::Compute as i32);
}

#[tokio::test]
async fn service_events_and_status_share_one_resource_id() {
    let daemon = Daemon::start();
    let channel = daemon.channel().await;
    let mut events = EventServiceClient::new(channel.clone())
        .subscribe(SubscribeRequest {
            kinds: vec!["service.down".into()],
        })
        .await
        .unwrap()
        .into_inner();
    let job = SvcServiceClient::new(channel.clone())
        .down(SvcDownRequest {
            service_type: "ota".into(),
            experiment: None,
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
