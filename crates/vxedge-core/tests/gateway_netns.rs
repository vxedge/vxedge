//! Privileged integration tests: gateway provisioning + container zone
//! backend inside a dedicated network namespace. Idempotency proven by
//! double-apply/double-teardown.
//!
//! Ignored by default (needs CAP_NET_ADMIN / root). Run with:
//!   sudo -E env "PATH=$PATH" cargo test -p vxedge-core --test gateway_netns -- --ignored
//! CI runs this on ubuntu-latest (sudo available on hosted runners).

#![cfg(target_os = "linux")]

use vxedge_core::ExperimentManifest;
use vxedge_core::gateway::{GatewayPlan, Shell, apply, observe, teardown};
use vxedge_core::zones::backend::NodeState;
use vxedge_core::zones::{node_down, node_status, node_up, plan_nodes_from_model};

const MANIFEST: &str = r#"
experiment: { id: exp-netns, name: netns integration }
network:
  defaults: { bandwidth: 500000000 }
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }
    - { id: infotainment, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, max_rate_mbps: 400, priority: 5 } }
zones:
  - id: vxzone-fl-01
    role: front-left
    nodes:
      - id: node-ecu-01
        type: ecu
        image: { url: /nonexistent/bundle, kind: rtimage }
        backend: qemu-microvm
        network: [{ iface: eth0, vlan: 101, slice: adas }]
      - id: node-info-01
        type: infotainment
        image: { url: x, kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 103, slice: infotainment, ip: 10.103.0.2/24 }]
"#;

/// Deletes the test netns even if assertions panic.
struct NetnsGuard(String);

impl Drop for NetnsGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("ip")
            .args(["netns", "del", &self.0])
            .output();
    }
}

#[test]
#[ignore = "needs CAP_NET_ADMIN; run under sudo (see module docs)"]
fn gateway_and_container_zone_cycle_in_netns() {
    let ns = format!("vxtest{}", std::process::id());
    let created = std::process::Command::new("ip")
        .args(["netns", "add", &ns])
        .output()
        .expect("iproute2 installed");
    assert!(
        created.status.success(),
        "cannot create netns (run as root): {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let _guard = NetnsGuard(ns.clone());
    let sh = Shell {
        netns: Some(ns),
        escalate: false,
    };

    let manifest = ExperimentManifest::from_yaml(MANIFEST).unwrap();
    let plan = GatewayPlan::from_model(&manifest.to_model().expect("adapts")).unwrap();

    // --- gateway: apply, observe, idempotency -------------------------------
    apply(&sh, &plan).unwrap();
    let obs = observe(&sh, &plan).unwrap();
    assert!(obs.fully_up(), "gateway not fully up: {obs:?}");
    // Mixed endpoint kinds: microvm node => tap (peer == name), container => veth.
    assert_eq!(
        plan.endpoints[0].peer, plan.endpoints[0].name,
        "tap endpoint"
    );
    assert_ne!(
        plan.endpoints[1].peer, plan.endpoints[1].name,
        "veth endpoint"
    );

    apply(&sh, &plan).unwrap(); // double-apply is a no-op
    assert!(observe(&sh, &plan).unwrap().fully_up());

    // --- container zone backend ---------------------------------------------
    let runtime = std::env::temp_dir().join(format!("vxzones-test-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let nodes = plan_nodes_from_model(
        &manifest.to_model().expect("adapts"),
        &plan,
        std::path::Path::new("/"),
    )
    .unwrap();
    let container = nodes
        .iter()
        .find(|n| n.node_id == "node-info-01")
        .expect("container node planned");

    node_up(&sh, container, &runtime).unwrap();
    assert_eq!(node_status(&sh, container, &runtime), NodeState::Up);
    node_up(&sh, container, &runtime).unwrap(); // idempotent
    assert_eq!(node_status(&sh, container, &runtime), NodeState::Up);

    // The node's eth0 carries the manifest IP inside its namespace.
    let addr = sh
        .run_host(
            "ip",
            &[
                "netns",
                "exec",
                &container.runtime_name(),
                "ip",
                "-j",
                "addr",
                "show",
                "dev",
                "eth0",
            ],
        )
        .unwrap();
    assert!(addr.contains("10.103.0.2"), "node ip missing: {addr}");

    node_down(&sh, container, &runtime).unwrap();
    assert_eq!(node_status(&sh, container, &runtime), NodeState::Down);
    node_down(&sh, container, &runtime).unwrap(); // idempotent

    // Container down destroyed its veth pair; a re-apply restores endpoints.
    apply(&sh, &plan).unwrap();
    assert!(
        observe(&sh, &plan).unwrap().fully_up(),
        "re-apply after zone down"
    );

    // --- teardown -------------------------------------------------------------
    teardown(&sh, &plan).unwrap();
    let gone = observe(&sh, &plan).unwrap();
    assert!(!gone.bridge_present && !gone.uplink_present);
    assert_eq!(gone.endpoints_present, 0);
    teardown(&sh, &plan).unwrap();
    let _ = std::fs::remove_dir_all(&runtime);
}
