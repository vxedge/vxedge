//! Privileged integration test: provisions a full gateway inside a dedicated
//! network namespace, asserts observation, proves idempotency, tears down.
//!
//! Ignored by default (needs CAP_NET_ADMIN / root). Run with:
//!   sudo -E env "PATH=$PATH" cargo test -p vxedge-core --test gateway_netns -- --ignored
//! CI runs this on ubuntu-latest (sudo available on hosted runners).

#![cfg(target_os = "linux")]

use vxedge_core::ExperimentManifest;
use vxedge_core::gateway::{GatewayPlan, Shell, apply, observe, teardown};

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
        image: { url: x, kind: rtimage }
        network: [{ iface: eth0, vlan: 101, slice: adas }]
      - id: node-info-01
        type: infotainment
        image: { url: x, kind: vm }
        network: [{ iface: eth0, vlan: 103, slice: infotainment }]
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
fn gateway_provisions_idempotently_in_netns() {
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
    let sh = Shell { netns: Some(ns) };

    let manifest = ExperimentManifest::from_yaml(MANIFEST).unwrap();
    let plan = GatewayPlan::from_manifest(&manifest).unwrap();

    // Apply, then verify every planned object is observed.
    apply(&sh, &plan).unwrap();
    let obs = observe(&sh, &plan).unwrap();
    assert!(obs.bridge_present, "bridge missing after apply");
    assert!(obs.uplink_present, "uplink missing after apply");
    assert_eq!(obs.endpoints_present, 2, "endpoints missing after apply");
    for s in &obs.slices {
        assert!(s.class_present, "slice {} missing HTB class", s.id);
        assert!(s.trunk_vlan_present, "slice {} missing trunk VLAN", s.id);
    }
    assert!(obs.fully_up());

    // Idempotency: applying again must succeed and change nothing observable.
    apply(&sh, &plan).unwrap();
    assert!(observe(&sh, &plan).unwrap().fully_up());

    // Teardown removes everything; a second teardown is a no-op.
    teardown(&sh, &plan).unwrap();
    let gone = observe(&sh, &plan).unwrap();
    assert!(!gone.bridge_present, "bridge survived teardown");
    assert!(!gone.uplink_present, "uplink survived teardown");
    assert_eq!(gone.endpoints_present, 0, "endpoints survived teardown");
    teardown(&sh, &plan).unwrap();
}
