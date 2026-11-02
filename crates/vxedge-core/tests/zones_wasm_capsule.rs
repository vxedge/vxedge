//! Privileged integration test: the zone `wasm-capsule` backend (Track V5) as a deployment runs
//! it. A `role: sensor` node runs a capsule in its own network namespace behind a real gateway in
//! the host namespace, and the proof is the capsule answering UDP across the bridge.
//!
//! **Do not run under `sudo`**: the runner must be unprivileged, and the test refuses root.
//! Privilege comes from file capabilities on the test binary, plus a cached sudo credential for
//! `tc` and `nft`. It needs no gateway already on this host. Run with:
//!   cargo build -p vxcapsule
//!   cargo build --release --target wasm32-wasip2 --manifest-path \
//!     crates/vxedge-core/tests/fixtures/reflector-capsule/Cargo.toml
//!   sudo -v
//!   BIN=$(cargo test -p vxedge-core --test zones_wasm_capsule --no-run --message-format=json \
//!     | jq -r 'select(.target.name=="zones_wasm_capsule" and .executable) | .executable')
//!   sudo setcap cap_net_admin,cap_sys_admin+ep "$BIN"   # again after every rebuild
//!   VXEDGE_CAPSULE_BIN=target/debug/vxcapsule VXEDGE_TEST_WASM=/path/to/reflector-capsule.wasm \
//!     "$BIN" --ignored --nocapture

#![cfg(target_os = "linux")]

use std::net::UdpSocket;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use vxedge_core::gateway::{GatewayPlan, Shell, apply, netlink, teardown};
use vxedge_core::zones::backend::NodeState;
use vxedge_core::zones::{
    NodePlan, foreign_objects, node_down, node_status, node_up, plan_nodes_from_model,
};

const NODE: &str = "sensor-capsule-01";
const GUEST_IP: &str = "10.250.102.2";
const FAR_SIDE_CIDR: &str = "10.250.102.1/24";
const REFLECTOR: &str = "10.250.102.2:9800";

fn workspace(module: &str) -> String {
    format!(
        r#"
kind: experiment
id: exp-capsule
vehicles: [veh]
baseline: bl
network:
  slices:
    - {{ id: adas, mode: vlan, vlan: 102 }}
---
kind: vehicle
id: veh
zones: {{ front-left: {{}} }}
devices:
  {NODE}: {{ role: sensor, zone: front-left, backend: wasm-capsule }}
functions:
  fn-sensor: {{ deployed_on: {NODE} }}
links:
  - {{ id: l-sensor, endpoints: [{{ node: {NODE}, port: eth0, ip: {GUEST_IP}/24 }}], slice: adas }}
---
kind: baseline
id: bl
parts:
  - {{ ref: {module}, kind: wasm, target_ids: fn-sensor }}
"#
    )
}

/// Stops the capsule and removes the gateway even if an assertion panics.
struct Guard {
    sh: Shell,
    plan: GatewayPlan,
    node: NodePlan,
    runtime: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = node_down(&self.sh, &self.node, &self.runtime);
        let _ = teardown(&self.sh, &self.plan);
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

fn runner_pids() -> Vec<String> {
    let out = Command::new("pgrep")
        .args(["-f", "--", &format!("--node {NODE}")])
        .output()
        .expect("pgrep installed");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn own_uid() -> u32 {
    std::fs::metadata("/proc/self").unwrap().uid()
}

fn wait_for_reflector() -> UdpSocket {
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.connect(REFLECTOR).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 16];
    while Instant::now() < deadline {
        let _ = socket.send(b"capsule");
        if socket.recv(&mut buf).is_ok() {
            return socket;
        }
    }
    panic!("the capsule's reflector never answered");
}

/// Median UDP round trip, in microseconds, for the evaluation.
fn median_rtt_us(socket: &UdpSocket, rounds: usize) -> u128 {
    let mut buf = [0u8; 16];
    while socket.recv(&mut buf).is_ok() {} // whatever the probing left queued
    let mut samples: Vec<u128> = (0..rounds)
        .filter_map(|_| {
            let t = Instant::now();
            socket.send(b"rtt").ok()?;
            socket.recv(&mut buf).ok()?;
            Some(t.elapsed().as_micros())
        })
        .collect();
    assert!(samples.len() > rounds / 2, "most round trips must complete");
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn rss_kib(pid: &str) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .expect("a running process has VmRSS")
}

#[test]
#[ignore = "needs file capabilities, cached sudo, and a built capsule (see module docs)"]
fn a_capsule_answers_across_the_gateway_and_stops_while_idle() {
    assert_ne!(
        own_uid(),
        0,
        "this test proves an unprivileged runner: not as root"
    );
    assert!(
        !Path::new("/sys/class/net/vxbr0").exists(),
        "a gateway is already on this host; bring it down first"
    );
    let module = std::env::var("VXEDGE_TEST_WASM").expect("VXEDGE_TEST_WASM names a module");
    let runtime = std::env::temp_dir().join(format!("vxcap{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();

    // As `vxedged` builds its Shell: the host namespace, escalating what needs it.
    let sh = Shell {
        netns: None,
        escalate: true,
    };
    let model = vxedge_core::model::load_yaml(&workspace(&module)).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();
    apply(&sh, &plan).unwrap();
    let nodes = plan_nodes_from_model(&model, &plan, Path::new("/")).unwrap();
    let node = nodes[0].clone();
    let _guard = Guard {
        sh: sh.clone(),
        plan: plan.clone(),
        node: node.clone(),
        runtime: runtime.clone(),
    };

    // The far side, on the sensor's slice and subnet.
    let far_dev = format!("{}.102", plan.uplink_peer);
    sh.run(
        "ip",
        &[
            "link",
            "add",
            "link",
            &plan.uplink_peer,
            "name",
            &far_dev,
            "type",
            "vlan",
            "id",
            "102",
        ],
    )
    .unwrap();
    sh.run("ip", &["addr", "add", FAR_SIDE_CIDR, "dev", &far_dev])
        .unwrap();
    sh.run("ip", &["link", "set", &far_dev, "up"]).unwrap();
    sh.run("ip", &["link", "set", &plan.uplink_peer, "up"])
        .unwrap();

    // --- start ----------------------------------------------------------------
    let started = Instant::now();
    node_up(&sh, &node, &runtime, &runtime).unwrap();
    let socket = wait_for_reflector();
    let up_to_echo = started.elapsed();
    assert_eq!(node_status(&sh, &node, &runtime), NodeState::Up);

    // The runner is one unprivileged process, inside the node's namespace and nowhere else.
    let pids = runner_pids();
    assert_eq!(pids.len(), 1, "one runner: {pids:?}");
    let proc_dir = format!("/proc/{}", pids[0]);
    assert_eq!(std::fs::metadata(&proc_dir).unwrap().uid(), own_uid());
    let status = std::fs::read_to_string(format!("{proc_dir}/status")).unwrap();
    let effective = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .expect("status lists CapEff");
    assert_eq!(
        effective.trim().trim_matches('0'),
        "",
        "the runner keeps no capability"
    );
    let node_ns = std::fs::metadata(format!("/run/netns/{}", node.runtime_name())).unwrap();
    let runner_ns = std::fs::metadata(format!("{proc_dir}/ns/net")).unwrap();
    assert_eq!(
        runner_ns.ino(),
        node_ns.ino(),
        "the runner is in the node's namespace"
    );

    println!(
        "evaluation: up-to-echo {} ms, median rtt {} us, runner rss {} KiB",
        up_to_echo.as_millis(),
        median_rtt_us(&socket, 1000),
        rss_kib(&pids[0])
    );

    // The host check sees it, so another workspace is refused.
    assert!(
        foreign_objects(&[]).contains(&format!("guest {NODE}")),
        "the host check must name a running capsule"
    );

    // --- idempotent up ----------------------------------------------------------
    node_up(&sh, &node, &runtime, &runtime).unwrap();
    assert_eq!(runner_pids().len(), 1, "a second `up` started a runner");

    // --- the runner dies: the node reads down, and `up` recovers it ---------------
    Command::new("kill")
        .args(["-9", &pids[0]])
        .output()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runner_pids().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(node_status(&sh, &node, &runtime), NodeState::Down);
    node_up(&sh, &node, &runtime, &runtime).unwrap();
    wait_for_reflector();

    // --- down while idle: the case that decided this is a process -------------------
    std::thread::sleep(Duration::from_millis(200));
    let stopping = Instant::now();
    node_down(&sh, &node, &runtime).unwrap();
    assert!(
        stopping.elapsed() < Duration::from_secs(10),
        "an idle capsule must stop promptly"
    );
    assert_eq!(node_status(&sh, &node, &runtime), NodeState::Down);
    assert!(runner_pids().is_empty(), "`down` left a runner");
    assert!(
        !netlink::netns_exists(&node.runtime_name()),
        "`down` left the namespace"
    );
    node_down(&sh, &node, &runtime).unwrap();

    // --- the port died with the namespace; a re-apply and a fresh start restore it ----
    apply(&sh, &plan).unwrap();
    sh.run("ip", &["link", "set", &plan.uplink_peer, "up"])
        .unwrap();
    node_up(&sh, &node, &runtime, &runtime).unwrap();
    wait_for_reflector();
}
