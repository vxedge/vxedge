//! Privileged integration test: a vehicle with a signal plane, driven through a real `vxedged` on
//! the host's own gateway. `rpc_contract.rs` checks the same verbs without provisioning anything;
//! this test provisions, so it can check what only a real vehicle shows:
//!
//! - a second `vehicle up` changes nothing: the same containers, links, VLANs, queues and rules;
//! - `status`, `up` and `down` of another vehicle are refused while this one is up, and change
//!   nothing, before and after a daemon restart;
//! - a restarted daemon still reports the vehicle up, signal plane included, and brings it down;
//! - `down` twice succeeds, and leaves nothing behind;
//! - after an `up` that fails, `down` leaves nothing behind;
//! - a databroker that exits straight after it starts fails the job, naming the databroker.
//!
//! The vehicle is two container devices whose payload is the pause image, a databroker on the
//! compute one, and an OTA service that is the pause image again. The databroker is the published
//! `kuksa-databroker` 0.7.1 by digest, pulled once beforehand.
//!
//! The gateway is the host's own `vxbr0`, as in a deployment, so the host must have no vehicle up,
//! and the test refuses to start if it sees a `vx`/`vz` link, a `vx` namespace or a `vx` container.
//! Only those names are read, listed or removed: nothing else on the host is touched.
//!
//! **Do not run under `sudo`**, for the reason `zones_podman.rs` gives: Podman must stay this
//! user's. The daemon this test starts is the one that needs privilege, through file capabilities,
//! plus a cached sudo credential for its `tc` and `nft` calls. Podman is reached over the user's
//! API socket (`systemctl --user enable --now podman.socket`). Ignored by default, and not run in
//! CI. Run with:
//!   podman pull ghcr.io/eclipse-kuksa/kuksa-databroker@sha256:529d8c6d3cc223362667672978bcb8f92c4d356b25aedd336e7a2e004d7279bd
//!   podman pull registry.k8s.io/pause@sha256:278fb9dbcca9518083ad1e11276933a2e96f23de604a3a08cc3c80002767d24c
//!   BIN=$(cargo test -p vxedged --test vehicle_lifecycle_live --no-run --message-format=json \
//!     | jq -r 'select(.target.name=="vehicle_lifecycle_live" and .executable) | .executable')
//!   sudo -v
//!   sudo setcap cap_net_admin,cap_sys_admin+ep target/debug/vxedged   # again after every rebuild
//!   "$BIN" --ignored --nocapture --test-threads=1

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use vxedge_api::v1::job_service_client::JobServiceClient;
use vxedge_api::v1::vehicle_service_client::VehicleServiceClient;
use vxedge_api::v1::{
    GetJobRequest, Job, JobState, ManifestRef, ResourceKind, ResourceState, VehicleDownRequest,
    VehicleStatus, VehicleStatusRequest, VehicleUpRequest, manifest_ref::Source,
};
use vxedge_core::containers::PAUSE_IMAGE;
use vxedge_core::services::KUKSA_MEMBER;

/// The databroker exp-015 ran: `kuksa-databroker` 0.7.1.
const DATABROKER: &str = "ghcr.io/eclipse-kuksa/kuksa-databroker@sha256:\
                          529d8c6d3cc223362667672978bcb8f92c4d356b25aedd336e7a2e004d7279bd";

/// How long one lifecycle job may take. A real `up` here takes a few seconds.
const JOB_TIMEOUT: Duration = Duration::from_secs(300);

/// A vehicle with a zone device, a compute device holding the databroker and an OTA service, and
/// the databroker listening on `listen`.
fn vehicle(id: &str, listen: &str) -> String {
    let (reference, digest) = PAUSE_IMAGE.split_once('@').expect("pause image is pinned");
    format!(
        r#"
kind: vehicle
id: {id}
zones: {{ front: {{}}, central: {{}} }}
devices:
  ecu1: {{ role: zone-controller, zone: front, backend: container }}
  hpc1: {{ role: hpc, zone: central, backend: container }}
functions:
  fn-ecu: {{ deployed_on: ecu1 }}
  fn-hpc: {{ deployed_on: hpc1 }}
links:
  - {{ id: l-ecu, endpoints: [{{ node: ecu1, port: eth0, ip: 10.101.0.30/24 }}], slice: adas }}
  - {{ id: l-hpc, endpoints: [{{ node: hpc1, port: eth0, ip: 10.101.0.31/24 }}], slice: adas }}
services:
  ota:
    id: ota-01
    type: ota
    image: {{ url: '{PAUSE_IMAGE}', kind: container }}
    on: hpc1
    config: {{ artifacts: ./store, listen: "0.0.0.0:9900" }}
signal_plane:
  kuksa:
    image: {{ url: '{DATABROKER}', kind: container }}
    on: hpc1
    config: {{ listen: "{listen}" }}
---
kind: baseline
id: bl
parts:
  - {{ ref: '{reference}', kind: container, digest: '{digest}', target_ids: fn-ecu }}
  - {{ ref: '{reference}', kind: container, digest: '{digest}', target_ids: fn-hpc }}
"#
    )
}

/// The workspace directories of one test, removed at the end unless it failed.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("vxlife-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        eprintln!("scratch dir, daemon log in it: {}", dir.display());
        Self(dir)
    }

    /// A vehicle workspace under this scratch dir.
    fn workspace(&self, name: &str, yaml: &str) -> PathBuf {
        let dir = self.0.join(name);
        std::fs::create_dir_all(dir.join("store")).unwrap();
        std::fs::write(dir.join("vehicle.yaml"), yaml).unwrap();
        dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// A `vxedged` on the real host. Its runtime dir outlives a restart, as a deployment's does.
struct Daemon {
    child: Option<Child>,
    root: PathBuf,
}

impl Daemon {
    fn start(root: &Path) -> Self {
        let mut daemon = Self {
            child: None,
            root: root.to_path_buf(),
        };
        daemon.spawn();
        daemon
    }

    fn socket(&self) -> PathBuf {
        self.root.join("d.sock")
    }

    fn spawn(&mut self) {
        let _ = std::fs::remove_file(self.socket());
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("d.log"))
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_vxedged"))
            .arg("--listen")
            .arg(format!("unix://{}", self.socket().display()))
            .arg("--runtime-dir")
            .arg(self.root.join("rt"))
            .arg("--cache-dir")
            .arg(self.root.join("cache"))
            .arg("--metrics-listen")
            .arg("127.0.0.1:0")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("vxedged starts");
        self.child = Some(child);
    }

    /// Stop the daemon the way an operator does, with SIGTERM to its pid, and start a new one on
    /// the same runtime dir.
    fn restart(&mut self) {
        let mut child = self.child.take().expect("daemon running");
        let killed = Command::new("kill")
            .arg(child.id().to_string())
            .status()
            .unwrap();
        assert!(killed.success(), "cannot signal vxedged {}", child.id());
        child.wait().unwrap();
        self.spawn();
    }

    async fn channel(&self) -> Channel {
        connect(self.socket()).await
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn connect(socket: PathBuf) -> Channel {
    for _ in 0..400 {
        if socket.exists() {
            let path = socket.clone();
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
    panic!("vxedged did not start listening on {}", socket.display());
}

/// Brings the vehicle down if a test fails halfway, so a failure does not strand the gateway.
struct DownOnPanic {
    socket: PathBuf,
    workspace: PathBuf,
}

impl Drop for DownOnPanic {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let (socket, workspace) = (self.socket.clone(), self.workspace.clone());
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let channel = connect(socket).await;
                if let Ok(job) = VehicleServiceClient::new(channel.clone())
                    .down(VehicleDownRequest {
                        manifest: path_ref(&workspace),
                        ..Default::default()
                    })
                    .await
                {
                    let _ = wait_for_job(channel, job.into_inner().id).await;
                }
            });
        })
        .join();
    }
}

fn path_ref(dir: &Path) -> Option<ManifestRef> {
    Some(ManifestRef {
        source: Some(Source::Path(dir.display().to_string())),
        ..Default::default()
    })
}

async fn wait_for_job(channel: Channel, id: String) -> Job {
    let mut jobs = JobServiceClient::new(channel);
    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    loop {
        let job = jobs
            .get(GetJobRequest { id: id.clone() })
            .await
            .unwrap()
            .into_inner();
        if job.state != JobState::Pending as i32 && job.state != JobState::Running as i32 {
            return job;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "job {id} still running after {JOB_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn up(channel: &Channel, dir: &Path) -> Result<Job, tonic::Status> {
    let job = VehicleServiceClient::new(channel.clone())
        .up(VehicleUpRequest {
            manifest: path_ref(dir),
            ..Default::default()
        })
        .await?
        .into_inner();
    Ok(wait_for_job(channel.clone(), job.id).await)
}

async fn down(channel: &Channel, dir: &Path) -> Result<Job, tonic::Status> {
    let job = VehicleServiceClient::new(channel.clone())
        .down(VehicleDownRequest {
            manifest: path_ref(dir),
            ..Default::default()
        })
        .await?
        .into_inner();
    Ok(wait_for_job(channel.clone(), job.id).await)
}

async fn status(channel: &Channel, dir: &Path) -> Result<VehicleStatus, tonic::Status> {
    Ok(VehicleServiceClient::new(channel.clone())
        .status(VehicleStatusRequest {
            manifest: path_ref(dir),
        })
        .await?
        .into_inner())
}

fn assert_succeeded(job: &Job, what: &str) {
    assert_eq!(
        job.state,
        JobState::Succeeded as i32,
        "{what}: {} {}",
        job.message,
        job.error
    );
}

/// The `ErrorInfo.reason` a refusal carries.
fn reason(status: &tonic::Status) -> String {
    use tonic_types::StatusExt;
    status
        .get_error_details()
        .error_info()
        .map(|info| info.reason.clone())
        .unwrap_or_else(|| panic!("no ErrorInfo on {status:?}"))
}

fn run(program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `nft` needs root, which this test only has through a cached sudo credential.
fn sudo(args: &[&str]) -> String {
    let mut all = vec!["-n"];
    all.extend_from_slice(args);
    run("sudo", &all)
}

fn vx_links() -> Vec<String> {
    let mut links: Vec<String> = std::fs::read_dir("/sys/class/net")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|l| l.starts_with("vx") || l.starts_with("vz"))
        .collect();
    links.sort();
    links
}

fn vx_netns() -> Vec<String> {
    run("ip", &["netns", "list"])
        .lines()
        .filter(|l| l.starts_with("vx"))
        .map(str::to_owned)
        .collect()
}

fn vx_containers() -> Vec<String> {
    run("podman", &["ps", "-a", "--format", "{{.Names}}"])
        .lines()
        .filter(|l| l.starts_with("vx"))
        .map(str::to_owned)
        .collect()
}

/// The `vx` nftables tables, as `family name`.
fn vx_tables() -> Vec<String> {
    sudo(&["nft", "list", "tables"])
        .lines()
        .filter(|l| l.contains("vx"))
        .filter_map(|l| l.strip_prefix("table "))
        .map(str::to_owned)
        .collect()
}

/// Whatever a vehicle could leave behind, by kind. Empty on a clean host.
fn leaks() -> Vec<String> {
    let mut found = Vec::new();
    found.extend(vx_netns().into_iter().map(|n| format!("netns {n}")));
    found.extend(vx_links().into_iter().map(|l| format!("link {l}")));
    found.extend(
        vx_containers()
            .into_iter()
            .map(|c| format!("container {c}")),
    );
    found.extend(vx_tables().into_iter().map(|t| format!("nft table {t}")));
    found
}

fn assert_host_clean(when: &str) {
    let left = leaks();
    assert!(left.is_empty(), "{when}: the host shows {left:?}");
}

/// What a second `up`, or a refused verb, must not change: each container's identity and process,
/// each link's index, VLANs and queues, and the vx rules. Rule handles and counters are dropped,
/// because `up` re-installs the anti-spoof rule by design and a rule's identity is its text.
fn snapshot() -> String {
    let mut out = String::new();
    let mut containers: Vec<String> = run(
        "podman",
        &[
            "ps",
            "--filter",
            "name=^vx",
            "--format",
            "{{.Names}} {{.ID}} {{.Pid}}",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    containers.sort();
    out.push_str(&format!("# containers\n{}\n", containers.join("\n")));
    for link in vx_links() {
        let index =
            std::fs::read_to_string(format!("/sys/class/net/{link}/ifindex")).unwrap_or_default();
        out.push_str(&format!("# link {link} {}", index));
        out.push_str(&run("bridge", &["vlan", "show", "dev", &link]));
        out.push_str(&run("tc", &["class", "show", "dev", &link]));
        out.push_str(&run("tc", &["qdisc", "show", "dev", &link]));
    }
    for table in vx_tables() {
        let mut args = vec!["nft", "list", "table"];
        args.extend(table.split_whitespace());
        out.push_str(&format!("# nft {table}\n"));
        for line in sudo(&args).lines() {
            out.push_str(&rule_text(line));
            out.push('\n');
        }
    }
    out
}

/// A ruleset line without its handle and counter values.
fn rule_text(line: &str) -> String {
    let line = line.split(" # handle ").next().unwrap_or(line);
    let mut words = Vec::new();
    let mut skip = false;
    for word in line.split_whitespace() {
        if skip {
            skip = false;
            continue;
        }
        words.push(word);
        skip = word == "packets" || word == "bytes";
    }
    words.join(" ")
}

fn podman_id(container: &str) -> Option<String> {
    let out = Command::new("podman")
        .args(["inspect", "-f", "{{.Id}} {{.State.Running}}", container])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The Podman group name of a device of `yaml`'s vehicle.
fn group_of(yaml: &str, dir: &Path, node: &str) -> String {
    let model = vxedge_core::model::load_vehicle_yaml(yaml).expect("vehicle loads");
    let gateway = vxedge_core::gateway::GatewayPlan::from_model(
        &model,
        vxedge_core::instance::Instance::FIRST,
    )
    .unwrap();
    vxedge_core::zones::plan_nodes_from_model(&model, &gateway, dir)
        .unwrap()
        .into_iter()
        .find(|n| n.node_id == node)
        .unwrap_or_else(|| panic!("no node plan for {node}"))
        .runtime_name()
}

fn signal_plane(status: &VehicleStatus) -> ResourceState {
    let plane = status
        .components
        .iter()
        .find(|c| c.kind == ResourceKind::SignalPlane as i32)
        .unwrap_or_else(|| panic!("no signal_plane component in {status:?}"));
    assert_eq!(plane.vxid, KUKSA_MEMBER);
    ResourceState::try_from(plane.state).unwrap()
}

fn rollup(status: &VehicleStatus) -> ResourceState {
    ResourceState::try_from(status.status.as_ref().expect("a rollup").state).unwrap()
}

/// `status`, `up` and `down` of a vehicle that is not the active one are each refused as the wrong
/// workspace, and none of them changes the host.
async fn wrong_vehicle_is_refused(channel: &Channel, other: &Path, before: &str, when: &str) {
    let refused = status(channel, other).await.err();
    let refused = refused.unwrap_or_else(|| panic!("{when}: status of another vehicle answered"));
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "{when}");
    assert_eq!(reason(&refused), "ERROR_REASON_WRONG_WORKSPACE", "{when}");

    match up(channel, other).await {
        Ok(job) => panic!("{when}: up of another vehicle ran a job: {job:?}"),
        Err(refused) => {
            assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "{when}");
            assert_eq!(reason(&refused), "ERROR_REASON_WRONG_WORKSPACE", "{when}");
        }
    }
    match down(channel, other).await {
        Ok(job) => panic!("{when}: down of another vehicle ran a job: {job:?}"),
        Err(refused) => {
            assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "{when}");
            assert_eq!(reason(&refused), "ERROR_REASON_WRONG_WORKSPACE", "{when}");
        }
    }
    assert_eq!(
        snapshot(),
        before,
        "{when}: a refused verb changed the host"
    );
}

#[tokio::test]
#[ignore = "provisions the host gateway; needs file capabilities on vxedged, a cached sudo \
            credential and the user Podman socket (see module docs)"]
async fn a_second_up_a_wrong_vehicle_and_a_daemon_restart_change_nothing() {
    assert_host_clean("before the test");
    let scratch = Scratch::new("restart");
    let yaml = vehicle("veh-life", "0.0.0.0:55555");
    let ws = scratch.workspace("veh-life", &yaml);
    let other = scratch.workspace("veh-other", &vehicle("veh-other", "0.0.0.0:55555"));
    let mut daemon = Daemon::start(&scratch.0);
    let _down = DownOnPanic {
        socket: daemon.socket(),
        workspace: ws.clone(),
    };
    let channel = daemon.channel().await;

    let job = up(&channel, &ws).await.expect("up accepted");
    assert_succeeded(&job, "up");
    let st = status(&channel, &ws).await.unwrap();
    assert_eq!(rollup(&st), ResourceState::Up, "{st:?}");
    assert_eq!(signal_plane(&st), ResourceState::Up, "{st:?}");
    let broker = format!("{}-{KUKSA_MEMBER}", group_of(&yaml, &ws, "hpc1"));
    let first = podman_id(&broker).expect("the databroker container exists");
    assert!(first.ends_with(" true"), "databroker not running: {first}");

    // A second `up` converges on what is there: same databroker, and nothing else changed.
    let before = snapshot();
    let compared = before.lines().count();
    assert!(
        before.contains("# link vxbr0") && before.contains("# nft "),
        "the snapshot misses the gateway: {before}"
    );
    eprintln!("snapshot: {compared} lines compared");
    let job = up(&channel, &ws).await.expect("second up accepted");
    assert_succeeded(&job, "second up");
    assert_eq!(podman_id(&broker).as_deref(), Some(first.as_str()));
    assert_eq!(snapshot(), before, "the second up changed the host");

    wrong_vehicle_is_refused(&channel, &other, &before, "daemon running").await;

    daemon.restart();
    let channel = daemon.channel().await;
    wrong_vehicle_is_refused(&channel, &other, &before, "after a restart").await;
    let st = status(&channel, &ws).await.expect("status after a restart");
    assert_eq!(rollup(&st), ResourceState::Up, "after a restart: {st:?}");
    assert_eq!(
        signal_plane(&st),
        ResourceState::Up,
        "after a restart: {st:?}"
    );
    assert_eq!(snapshot(), before, "the restart changed the host");

    assert_succeeded(&down(&channel, &ws).await.expect("down accepted"), "down");
    assert_succeeded(
        &down(&channel, &ws).await.expect("second down accepted"),
        "second down",
    );
    assert_host_clean("after down");
}

#[tokio::test]
#[ignore = "provisions the host gateway; needs file capabilities on vxedged, a cached sudo \
            credential and the user Podman socket (see module docs)"]
async fn down_after_a_failed_up_leaves_nothing_behind() {
    assert_host_clean("before the test");
    let scratch = Scratch::new("failed");
    let yaml = vehicle("veh-life", "0.0.0.0:55555");
    let ws = scratch.workspace("veh-life", &yaml);
    let daemon = Daemon::start(&scratch.0);
    let _down = DownOnPanic {
        socket: daemon.socket(),
        workspace: ws.clone(),
    };
    let channel = daemon.channel().await;

    // A container that is not the vehicle's holds the OTA service's name, so `up` fails at its
    // last stage, with the gateway, the devices and the databroker already up.
    let squatter = format!("{}-ota-01", group_of(&yaml, &ws, "hpc1"));
    struct Remove(String);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = Command::new("podman").args(["rm", "-f", &self.0]).output();
        }
    }
    run("podman", &["create", "--name", &squatter, PAUSE_IMAGE]);
    let removal = Remove(squatter.clone());

    let job = up(&channel, &ws).await.expect("up accepted");
    assert_eq!(
        job.state,
        JobState::Failed as i32,
        "up with the service's name taken: {job:?}"
    );
    assert!(
        job.error.contains("service `ota`") && job.error.contains(&squatter),
        "the failure names the service and the container: {}",
        job.error
    );
    drop(removal);

    assert_succeeded(
        &down(&channel, &ws).await.expect("down accepted"),
        "down after the failed up",
    );
    assert_host_clean("after down");
}

#[tokio::test]
#[ignore = "provisions the host gateway; needs file capabilities on vxedged, a cached sudo \
            credential and the user Podman socket (see module docs)"]
async fn a_databroker_that_exits_at_once_fails_the_job() {
    assert_host_clean("before the test");
    let scratch = Scratch::new("exits");
    // An address no namespace has: the databroker cannot bind it, and exits as it starts.
    let yaml = vehicle("veh-life", "192.0.2.1:55555");
    let ws = scratch.workspace("veh-life", &yaml);
    let daemon = Daemon::start(&scratch.0);
    let _down = DownOnPanic {
        socket: daemon.socket(),
        workspace: ws.clone(),
    };
    let channel = daemon.channel().await;

    let job = up(&channel, &ws).await.expect("up accepted");
    assert_eq!(
        job.state,
        JobState::Failed as i32,
        "up with an exiting databroker: {job:?}"
    );
    assert!(
        job.error.contains(KUKSA_MEMBER) && job.error.contains("stopped running"),
        "the failure names the databroker and why: {}",
        job.error
    );
    let st = status(&channel, &ws)
        .await
        .expect("status after the failed up");
    assert_eq!(rollup(&st), ResourceState::Degraded, "{st:?}");
    assert_eq!(signal_plane(&st), ResourceState::Down, "{st:?}");

    assert_succeeded(
        &down(&channel, &ws).await.expect("down accepted"),
        "down after the failed up",
    );
    assert_host_clean("after down");
}
