//! Privileged integration test: a service placed on a container device (`ARCHITECTURE.md`
//! §6.0.1). It runs as one more member of the device's group, in the namespace that holds the
//! gateway port, and leaves with that member's `down` without taking the node with it.
//!
//! The image is the pause image the infra container already uses, which ignores its arguments,
//! so the test needs nothing built. It proves the placement. Reachability over a slice is what
//! `vx-vehicle-02`'s evidence shows.
//!
//! **Do not run under `sudo`**, for the reason `zones_podman.rs` gives. Run with:
//!   sudo -v
//!   BIN=$(cargo test -p vxedge-core --test services_podman --no-run --message-format=json \
//!     | jq -r 'select(.target.name=="services_podman" and .executable) | .executable')
//!   sudo setcap cap_net_admin,cap_sys_admin+ep "$BIN"   # again after every rebuild
//!   "$BIN" --ignored --nocapture

#![cfg(target_os = "linux")]

use vxedge_core::containers::PAUSE_IMAGE;
use vxedge_core::gateway::{GatewayPlan, Shell, apply, teardown};
use vxedge_core::services::placed_spec;
use vxedge_core::zones::backend::NodeState;
use vxedge_core::zones::{
    member_down, member_status, member_up, node_down, node_status, node_up, plan_nodes_from_model,
};

fn workspace() -> String {
    let (reference, digest) = PAUSE_IMAGE.split_once('@').expect("pause image is pinned");
    format!(
        r#"
kind: vehicle
id: veh
zones: {{ central: {{}} }}
devices:
  hpc-svc-01: {{ role: hpc, zone: central, backend: container }}
functions:
  fn-hpc: {{ deployed_on: hpc-svc-01 }}
links:
  - {{ id: l-hpc, endpoints: [{{ node: hpc-svc-01, port: eth0, ip: 10.101.0.31/24 }}], slice: adas }}
services:
  ota:
    id: ota-01
    type: ota
    image: {{ url: '{PAUSE_IMAGE}', kind: container }}
    on: hpc-svc-01
    config: {{ artifacts: ./store, listen: "0.0.0.0:9900" }}
---
kind: baseline
id: bl
parts:
  - {{ ref: '{reference}', kind: container, digest: '{digest}', target_ids: fn-hpc }}
"#
    )
}

/// Deletes the test netns even if assertions panic.
struct NetnsGuard(String);

impl Drop for NetnsGuard {
    fn drop(&mut self) {
        let _ = sudo(&["ip", "netns", "del", &self.0]);
    }
}

fn sudo(args: &[&str]) -> std::process::Output {
    std::process::Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .expect("sudo runs")
}

/// `podman inspect` as this user, never through sudo.
fn podman_pid(container: &str) -> Option<u32> {
    let out = std::process::Command::new("podman")
        .args(["inspect", "-f", "{{.State.Pid}}", container])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn netns_of(pid: u32) -> std::path::PathBuf {
    std::fs::read_link(format!("/proc/{pid}/ns/net")).expect("own container's ns link is readable")
}

#[test]
#[ignore = "needs the user Podman socket, a cached sudo credential and file capabilities; run \
            unprivileged (see module docs)"]
fn a_placed_service_joins_its_device_namespace_and_leaves_without_taking_the_node() {
    let ns = format!("vxsvc{}", std::process::id());
    let created = sudo(&["ip", "netns", "add", &ns]);
    assert!(
        created.status.success(),
        "cannot create netns (cache a credential with `sudo -v` first): {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let _guard = NetnsGuard(ns.clone());
    let sh = Shell {
        netns: Some(ns.clone()),
        escalate: true,
    };

    let dir = std::env::temp_dir().join(format!("vxsvc-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("store")).unwrap();
    let model = vxedge_core::model::load_vehicle_yaml(&workspace()).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();
    apply(&sh, &plan).unwrap();
    let nodes = plan_nodes_from_model(&model, &plan, &dir).unwrap();
    let node = &nodes[0];
    let spec = placed_spec(&model.services["ota"], &dir, &dir).unwrap();

    // Before the node exists there is nothing to join.
    assert!(member_up(&sh, node, &dir, &spec).is_err());

    node_up(&sh, node, &dir, &dir).unwrap();
    assert_eq!(member_status(node, &spec), NodeState::Down);
    member_up(&sh, node, &dir, &spec).unwrap();
    assert_eq!(member_status(node, &spec), NodeState::Up);

    let group = node.runtime_name();
    let infra = podman_pid(&format!("{group}-infra")).expect("infra running");
    let member = podman_pid(&format!("{group}-ota-01")).expect("service running");
    assert_eq!(
        netns_of(member),
        netns_of(infra),
        "the service is in the device's namespace"
    );

    // Second `up` converges without touching anything: same infra, same service.
    node_up(&sh, node, &dir, &dir).unwrap();
    member_up(&sh, node, &dir, &spec).unwrap();
    assert_eq!(podman_pid(&format!("{group}-infra")), Some(infra));
    assert_eq!(podman_pid(&format!("{group}-ota-01")), Some(member));

    member_down(node, &spec).unwrap();
    assert_eq!(member_status(node, &spec), NodeState::Down);
    assert_eq!(podman_pid(&format!("{group}-ota-01")), None);
    assert_eq!(
        node_status(&sh, node, &dir),
        NodeState::Up,
        "the node outlives its service"
    );
    member_down(node, &spec).unwrap(); // idempotent

    // A service still running when its node goes down leaves with the group.
    member_up(&sh, node, &dir, &spec).unwrap();
    node_down(&sh, node, &dir).unwrap();
    assert_eq!(podman_pid(&format!("{group}-ota-01")), None);

    teardown(&sh, &plan).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
