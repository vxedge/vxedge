//! Privileged integration test: the zone `container` backend with an OCI payload (Track V3).
//! A container node whose part is a published, digest-pinned image runs that image in a
//! container group: the infra container owns the node's network namespace (loopback only), the
//! gateway port moves in as `eth0` by the infra PID, and the payload joins that namespace.
//!
//! The gateway runs in a throwaway network namespace, as in `gateway_netns.rs`; the zone
//! namespace is the group's own. The payload is the pause image the infra container already
//! uses, so the test needs nothing built: it proves the attachment, not an application.
//!
//! **Do not run under `sudo`** — same reason as `fiveg_podman.rs`: it would switch `podman` to
//! root's own separate rootless identity. Privilege comes from file capabilities on the test
//! binary instead: `CAP_NET_ADMIN` for the netlink calls and `CAP_SYS_ADMIN` for entering a
//! namespace (`setns(2)` needs it, as it does for the named-netns form), plus a cached sudo
//! credential for the throwaway netns itself. Podman is reached over the user's API socket
//! (`systemctl --user enable --now podman.socket`).
//!
//! Run with:
//!   sudo -v
//!   BIN=$(cargo test -p vxedge-core --test zones_podman --no-run --message-format=json \
//!     | jq -r 'select(.target.name=="zones_podman" and .executable) | .executable')
//!   sudo setcap cap_net_admin,cap_sys_admin+ep "$BIN"   # again after every rebuild
//!   "$BIN" --ignored --nocapture

#![cfg(target_os = "linux")]

use vxedge_core::containers::PAUSE_IMAGE;
use vxedge_core::gateway::{GatewayPlan, Shell, apply, observe, teardown};
use vxedge_core::zones::backend::NodeState;
use vxedge_core::zones::{node_down, node_status, node_up, plan_nodes_from_model};

fn workspace() -> String {
    let (reference, digest) = PAUSE_IMAGE.split_once('@').expect("pause image is pinned");
    format!(
        r#"
kind: experiment
id: exp-oci
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
  node-oci-01: {{ role: ecu, zone: front-left, backend: container }}
functions:
  fn-oci: {{ deployed_on: node-oci-01 }}
links:
  - {{ id: l-oci, endpoints: [{{ node: node-oci-01, port: eth0, ip: 10.101.0.21/24 }}], slice: adas }}
---
kind: baseline
id: bl
parts:
  - {{ ref: '{reference}', kind: container, digest: '{digest}', target_ids: fn-oci }}
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

fn netns_of(pid: u32) -> String {
    std::fs::read_link(format!("/proc/{pid}/ns/net"))
        .expect("own rootless container's ns link is readable")
        .display()
        .to_string()
}

#[test]
#[ignore = "needs the user Podman socket, a cached sudo credential and file capabilities; run \
            unprivileged (see module docs)"]
fn container_node_runs_an_oci_payload_in_its_own_group_namespace() {
    let ns = format!("vxoci{}", std::process::id());
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

    let model = vxedge_core::model::load_yaml(&workspace()).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();
    apply(&sh, &plan).unwrap();
    assert!(observe(&sh, &plan).unwrap().fully_up());

    let runtime = std::env::temp_dir().join(format!("vxzones-oci-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let nodes = plan_nodes_from_model(&model, &plan, std::path::Path::new("/")).unwrap();
    let node = &nodes[0];
    assert_eq!(node.image.as_deref(), Some(PAUSE_IMAGE), "planned payload");

    node_up(&sh, node, &runtime, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Up);

    let group = node.runtime_name();
    let infra = podman_pid(&format!("{group}-infra")).expect("infra container running");
    let payload = podman_pid(&format!("{group}-payload")).expect("payload container running");
    assert_eq!(
        netns_of(payload),
        netns_of(infra),
        "the payload joins the infra container's namespace"
    );

    // eth0 carries the manifest address inside that namespace, and it is the only interface
    // besides loopback: an infra container on `pasta` would add a second path to the host.
    let addr = sudo(&[
        "nsenter",
        "-t",
        &infra.to_string(),
        "-n",
        "ip",
        "-j",
        "addr",
        "show",
    ]);
    let addr = String::from_utf8_lossy(&addr.stdout);
    assert!(addr.contains("10.101.0.21"), "node ip missing: {addr}");
    let ifaces: Vec<serde_json::Value> = serde_json::from_str(&addr).expect("ip -j output");
    let names: Vec<&str> = ifaces.iter().filter_map(|i| i["ifname"].as_str()).collect();
    assert_eq!(names, ["lo", "eth0"], "loopback plus the gateway port only");

    // Second `up` converges without touching anything: same infra, same payload.
    node_up(&sh, node, &runtime, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Up);
    assert_eq!(podman_pid(&format!("{group}-infra")), Some(infra));
    assert_eq!(podman_pid(&format!("{group}-payload")), Some(payload));

    node_down(&sh, node, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Down);
    assert_eq!(podman_pid(&format!("{group}-infra")), None, "group removed");
    node_down(&sh, node, &runtime).unwrap(); // idempotent

    // The port died with the group's namespace; re-applying the gateway restores it, and the
    // node comes back up on it.
    let gone = observe(&sh, &plan).unwrap();
    assert!(!gone.fully_up(), "port destroyed with the namespace");
    apply(&sh, &plan).unwrap();
    node_up(&sh, node, &runtime, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Up);
    node_down(&sh, node, &runtime).unwrap();

    teardown(&sh, &plan).unwrap();
    let _ = std::fs::remove_dir_all(&runtime);
}
