//! Privileged integration test: the zone `wasm-capsule` backend (Track V5) as a deployment runs
//! it. A `role: sensor` node runs a capsule in its own network namespace behind a real gateway in
//! the host namespace, and the proof is the capsule answering UDP across the bridge.
//!
//! **Do not run under `sudo`**: the runner must be unprivileged, and the test refuses root.
//! Privilege comes from file capabilities on the test binary, plus a cached sudo credential for
//! `tc` and `nft`. It needs no gateway already on this host. Run with:
//!   cargo build -p vxcapsule
//!   <vxparts>/parts/zone/wasm/app/reflector-capsule/build.sh /path/to/capsule
//!   sudo -v
//!   BIN=$(cargo test -p vxedge-core --test zones_wasm_capsule --no-run --message-format=json \
//!     | jq -r 'select(.target.name=="zones_wasm_capsule" and .executable) | .executable')
//!   sudo setcap cap_net_admin,cap_sys_admin+ep "$BIN"   # again after every rebuild
//!   VXEDGE_CAPSULE_BIN=target/debug/vxcapsule \
//!     VXEDGE_TEST_WASM=/path/to/capsule/reflector-capsule.wasm \
//!     "$BIN" --ignored --nocapture --test-threads=1
//! `VXEDGE_TEST_PART_REF` and `VXEDGE_TEST_PART_DIGEST` name a published part for the fetch test,
//! which also needs registry access through `oras`.

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

fn workspace(part: &str, device_extra: &str) -> String {
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
  {NODE}: {{ role: sensor, zone: front-left, backend: wasm-capsule{device_extra} }}
functions:
  fn-sensor: {{ deployed_on: {NODE} }}
links:
  - {{ id: l-sensor, endpoints: [{{ node: {NODE}, port: eth0, ip: {GUEST_IP}/24 }}], slice: adas }}
---
kind: baseline
id: bl
parts:
  - {{ {part}, kind: wasm, target_ids: fn-sensor }}
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

/// A gateway in the host namespace with the far side up, planned from `yaml`.
fn gateway_for(yaml: &str) -> (Shell, GatewayPlan, NodePlan, PathBuf) {
    assert_ne!(
        own_uid(),
        0,
        "this test proves an unprivileged runner: not as root"
    );
    assert!(
        !Path::new("/sys/class/net/vxbr0").exists(),
        "a gateway is already on this host; bring it down first"
    );
    let runtime = std::env::temp_dir().join(format!("vxcap{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();

    // As `vxedged` builds its Shell: the host namespace, escalating what needs it.
    let sh = Shell {
        netns: None,
        escalate: true,
    };
    let model = vxedge_core::model::load_yaml(yaml).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();
    apply(&sh, &plan).unwrap();
    let nodes = plan_nodes_from_model(&model, &plan, Path::new("/")).unwrap();
    let node = nodes[0].clone();

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
    (sh, plan, node, runtime)
}

fn local_module_part() -> String {
    let module = std::env::var("VXEDGE_TEST_WASM").expect("VXEDGE_TEST_WASM names a module");
    format!("ref: {module}")
}

#[test]
#[ignore = "needs file capabilities, cached sudo, and a built capsule (see module docs)"]
fn a_capsule_answers_across_the_gateway_and_stops_while_idle() {
    let (sh, plan, node, runtime) = gateway_for(&workspace(&local_module_part(), ""));
    let _guard = Guard {
        sh: sh.clone(),
        plan: plan.clone(),
        node: node.clone(),
        runtime: runtime.clone(),
    };

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

/// The whole runner stays on the node's CPUs, every thread of it, wasmtime's included.
#[test]
#[ignore = "needs file capabilities, cached sudo, and a built capsule (see module docs)"]
fn a_pinned_capsule_keeps_every_thread_on_its_cpu() {
    let extra = ", realtime: { pinned_cpus: [1] }";
    let (sh, plan, node, runtime) = gateway_for(&workspace(&local_module_part(), extra));
    let _guard = Guard {
        sh: sh.clone(),
        plan: plan.clone(),
        node: node.clone(),
        runtime: runtime.clone(),
    };
    assert_eq!(node.pinned_cpus, [1]);
    node_up(&sh, &node, &runtime, &runtime).unwrap();
    wait_for_reflector();

    let pids = runner_pids();
    assert_eq!(pids.len(), 1, "one runner: {pids:?}");
    let mut threads = 0;
    for task in std::fs::read_dir(format!("/proc/{}/task", pids[0])).unwrap() {
        let status = std::fs::read_to_string(task.unwrap().path().join("status")).unwrap();
        let allowed = status
            .lines()
            .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
            .expect("status lists Cpus_allowed_list");
        assert_eq!(allowed.trim(), "1", "every thread stays on the node's CPU");
        threads += 1;
    }
    assert!(threads >= 1);
}

/// A published part, fetched by digest through `node_up`, with no local file named anywhere.
#[test]
#[ignore = "needs a published part and registry access, as well as the above (see module docs)"]
fn a_published_capsule_is_fetched_by_digest() {
    let reference = std::env::var("VXEDGE_TEST_PART_REF").expect("VXEDGE_TEST_PART_REF is set");
    let digest = std::env::var("VXEDGE_TEST_PART_DIGEST").expect("VXEDGE_TEST_PART_DIGEST is set");
    let part = format!("ref: {reference}, digest: '{digest}'");
    let (sh, plan, node, runtime) = gateway_for(&workspace(&part, ""));
    let _guard = Guard {
        sh: sh.clone(),
        plan: plan.clone(),
        node: node.clone(),
        runtime: runtime.clone(),
    };
    assert_eq!(node.module, None);
    node_up(&sh, &node, &runtime, &runtime).unwrap();
    wait_for_reflector();
    assert_eq!(node_status(&sh, &node, &runtime), NodeState::Up);
}
