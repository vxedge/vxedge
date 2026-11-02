//! Privileged integration test: the zone `cloud-hypervisor` backend (Track V4) booting the
//! `linux-probe` bundle on a real gateway tap, inside a throwaway network namespace. The proof
//! is the guest's own reflector answering UDP across the bridge.
//!
//! Needs root (the namespace and the VMM's `ip netns exec`), `/dev/kvm`, `socat`, and
//! `cloud-hypervisor` on root's PATH. `VXEDGE_TEST_BUNDLE` names a built bundle directory
//! (`vxparts/parts/zone/linux/stack/probe-bundle/build.sh`). Run with:
//!   cargo test -p vxedge-core --test zones_cloud_hypervisor --no-run
//!   sudo env "PATH=$PATH" VXEDGE_TEST_BUNDLE=/path/to/bundle <test-binary> --ignored --nocapture

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vxedge_core::gateway::{GatewayPlan, Shell, apply, teardown};
use vxedge_core::zones::backend::NodeState;
use vxedge_core::zones::{node_down, node_status, node_up, plan_nodes_from_model};

const GUEST_IP: &str = "10.101.0.2";
const FAR_SIDE_CIDR: &str = "10.101.0.1/24";
const REFLECTOR_PORT: u16 = 9800;

fn workspace(bundle: &str) -> String {
    format!(
        r#"
kind: experiment
id: exp-ch
vehicles: [veh]
baseline: bl
network:
  slices:
    - {{ id: adas, mode: vlan, vlan: 101 }}
---
kind: vehicle
id: veh
zones: {{ front-left: {{}} }}
devices:
  node-ch-01: {{ role: ecu, zone: front-left, backend: cloud-hypervisor }}
functions:
  fn-ch: {{ deployed_on: node-ch-01 }}
links:
  - {{ id: l-ch, endpoints: [{{ node: node-ch-01, port: eth0, ip: {GUEST_IP}/24 }}], slice: adas }}
---
kind: baseline
id: bl
parts:
  - {{ ref: {bundle}, kind: bundle, target_ids: fn-ch }}
"#
    )
}

/// Removes the VMM, the namespace and the runtime directory even if an assertion panics.
struct Guard {
    ns: String,
    runtime: std::path::PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        kill_vmm(&self.runtime);
        let _ = Command::new("ip").args(["netns", "del", &self.ns]).output();
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

/// Removes the second runtime directory.
struct OtherDir(std::path::PathBuf);

impl Drop for OtherDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The runtime directory is unique to this test and on the VMM's command line.
fn kill_vmm(runtime: &Path) {
    let _ = Command::new("pkill")
        .args(["-9", "-f", &runtime.display().to_string()])
        .output();
}

fn vmm_processes(runtime: &Path) -> usize {
    let out = Command::new("pgrep")
        .args(["-c", "-f", &runtime.display().to_string()])
        .output()
        .expect("pgrep installed");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// One UDP round trip to the guest's reflector, from inside the gateway namespace.
fn reflected(ns: &str, payload: &str) -> bool {
    let target = format!("UDP:{GUEST_IP}:{REFLECTOR_PORT}");
    let mut child = Command::new("ip")
        .args(["netns", "exec", ns, "socat", "-t1", "-", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("socat installed");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).contains(payload)
}

fn wait_for_reflector(ns: &str, serial: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if reflected(ns, "vxedge-v4") {
            return;
        }
    }
    let log = std::fs::read_to_string(serial).unwrap_or_default();
    let tail: Vec<&str> = log.lines().rev().take(15).collect();
    panic!("the guest's reflector never answered; serial tail, newest first:\n{tail:#?}");
}

#[test]
#[ignore = "needs root, /dev/kvm, socat, cloud-hypervisor and VXEDGE_TEST_BUNDLE (see module docs)"]
fn a_guest_boots_on_the_gateway_tap_answers_and_survives_its_vmm_dying() {
    let bundle = std::env::var("VXEDGE_TEST_BUNDLE").expect("VXEDGE_TEST_BUNDLE names a bundle");
    let ns = format!("vxchtest{}", std::process::id());
    let created = Command::new("ip")
        .args(["netns", "add", &ns])
        .output()
        .expect("iproute2 installed");
    assert!(
        created.status.success(),
        "cannot create netns (run as root): {}",
        String::from_utf8_lossy(&created.stderr)
    );
    // Short on purpose: this path, plus the socket name, has to fit a Unix socket address.
    let runtime = std::env::temp_dir().join(format!("vxch{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let _guard = Guard {
        ns: ns.clone(),
        runtime: runtime.clone(),
    };
    let sh = Shell {
        netns: Some(ns.clone()),
        escalate: false,
    };

    let model = vxedge_core::model::load_yaml(&workspace(&bundle)).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();
    apply(&sh, &plan).unwrap();
    let nodes = plan_nodes_from_model(&model, &plan, Path::new("/")).unwrap();
    let node = &nodes[0];
    let serial = runtime.join(format!("{}.serial.log", node.runtime_name()));
    let socket = runtime.join(format!("{}.ch.sock", node.runtime_name()));

    // The far side, on the guest's slice and subnet, the same shape as `gateway_netns.rs`.
    let far_dev = format!("{}.101", plan.uplink_peer);
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
            "101",
        ],
    )
    .unwrap();
    sh.run("ip", &["addr", "add", FAR_SIDE_CIDR, "dev", &far_dev])
        .unwrap();
    sh.run("ip", &["link", "set", &far_dev, "up"]).unwrap();
    sh.run("ip", &["link", "set", &plan.uplink_peer, "up"])
        .unwrap();

    // --- boot -----------------------------------------------------------------
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Down);
    node_up(&sh, node, &runtime, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Up);
    wait_for_reflector(&ns, &serial);

    // Adopting the tap changed nothing about it: still a bridge port, still no address.
    let link = sh
        .run("ip", &["-j", "-d", "addr", "show", "dev", &node.endpoint])
        .unwrap();
    assert!(link.contains(r#""master""#), "tap left the bridge: {link}");
    assert!(
        !link.contains(r#""family":"inet""#),
        "the VMM put a host address on the tap: {link}"
    );

    // --- idempotent up --------------------------------------------------------
    node_up(&sh, node, &runtime, &runtime).unwrap();
    assert_eq!(vmm_processes(&runtime), 1, "a second `up` started a VMM");

    // --- the VMM dies: the node reads down, and `up` recovers it ------------------
    kill_vmm(&runtime);
    let deadline = Instant::now() + Duration::from_secs(5);
    while vmm_processes(&runtime) != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "a killed VMM leaves its socket behind");
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Down);
    node_up(&sh, node, &runtime, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Up);
    wait_for_reflector(&ns, &serial);

    // --- down, twice, then a fresh boot -----------------------------------------
    node_down(&sh, node, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Down);
    assert_eq!(vmm_processes(&runtime), 0, "`down` left a VMM running");
    assert!(!socket.exists(), "`down` left the socket");
    node_down(&sh, node, &runtime).unwrap();

    node_up(&sh, node, &runtime, &runtime).unwrap();
    wait_for_reflector(&ns, &serial);

    // --- a daemon with another runtime dir sees the same guest ---------------------
    let other = std::env::temp_dir().join(format!("vxchb{}", std::process::id()));
    std::fs::create_dir_all(&other).unwrap();
    let _other_guard = OtherDir(other.clone());
    assert_eq!(node_status(&sh, node, &other), NodeState::Up);
    node_up(&sh, node, &other, &other).unwrap();
    assert_eq!(vmm_processes(&runtime), 1, "another daemon started a VMM");
    node_down(&sh, node, &other).unwrap();
    assert_eq!(
        vmm_processes(&runtime),
        0,
        "another daemon's `down` left it"
    );
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Down);

    teardown(&sh, &plan).unwrap();
}
