//! Privileged integration test: the OVS/OVSDB data plane against a real `ovsdb-server`.
//!
//! Ignored by default (needs CAP_NET_ADMIN / root, and OVS installed and running). Run with:
//!   sudo -E env "PATH=$PATH" cargo test -p vxedge-core --test gateway_ovs -- --ignored
//!
//! **Unlike `gateway_netns.rs`, this test is not namespace-isolated.** A Linux bridge created
//! inside a disposable `ip netns add` vanishes with the namespace; an OVS bridge does not — it
//! lives in the system-wide `ovsdb-server` regardless of which netns the `ovs-vsctl` command
//! that created it ran from. So this test uses a `GatewayPlan` built directly (not through
//! `GatewayPlan::from_model`, which always names `vxbr0`/`vxup0`) with names that cannot collide
//! with a real deployment's gateway, and cleans up explicitly rather than relying on a namespace
//! teardown to do it for free.
#![cfg(target_os = "linux")]

use vxedge_core::gateway::plan::{EndpointImpairment, EndpointKind, EndpointPlan, SlicePlan};
use vxedge_core::gateway::provision::{DataPlane, Ovs};
use vxedge_core::gateway::{GatewayPlan, Shell};
use vxedge_core::model::{DataPlaneKind, Impairment};

/// Test-only names throughout — never `vxbr0`/`vxup0`, which a real gateway or the
/// `gateway_netns` test's own plan would use. All well under `IFNAMSIZ`.
fn test_plan() -> GatewayPlan {
    GatewayPlan {
        bridge: "vxovstest".into(),
        uplink: "vxovsup".into(),
        uplink_peer: "vxovsupp".into(),
        root_rate_mbps: 500,
        slices: vec![SlicePlan {
            id: "adas".into(),
            vlan: 101,
            min_rate_mbps: 100,
            max_rate_mbps: 200,
            priority: 1,
            class_minor: 0x10,
        }],
        endpoints: vec![EndpointPlan {
            name: "vxoep1".into(),
            peer: "vxoep1p".into(),
            kind: EndpointKind::Veth,
            node: "node-a".into(),
            vlan: 101,
            slice: Some("adas".into()),
            impairment: Some(EndpointImpairment {
                link: "seg1".into(),
                ifb: "vxoifb1".into(),
                params: Impairment {
                    rate_mbps: Some(100),
                    delay_us: Some(200),
                    jitter_us: Some(20),
                    loss_ppm: None,
                },
            }),
        }],
        snssai_slice_ids: vec![],
        data_plane: DataPlaneKind::Ovs,
    }
}

/// Removes everything this test could have created, even on panic — the OVS bridge, its ports,
/// and the IFB, none of which a namespace teardown would take care of here.
struct OvsCleanup {
    bridge: &'static str,
    ports: &'static [&'static str],
    ifbs: &'static [&'static str],
}

impl Drop for OvsCleanup {
    fn drop(&mut self) {
        for p in self.ports {
            let _ = std::process::Command::new("ovs-vsctl")
                .args(["--if-exists", "del-port", self.bridge, p])
                .output();
        }
        let _ = std::process::Command::new("ovs-vsctl")
            .args(["--if-exists", "del-br", self.bridge])
            .output();
        for p in self.ports.iter().chain(self.ifbs.iter()) {
            let _ = std::process::Command::new("ip")
                .args(["link", "del", p])
                .output();
        }
    }
}

#[test]
#[ignore = "needs CAP_NET_ADMIN and a running ovsdb-server; run under sudo (see module docs)"]
fn ovs_data_plane_cycle() {
    let _guard = OvsCleanup {
        bridge: "vxovstest",
        ports: &["vxovsup", "vxoep1"],
        ifbs: &["vxoifb1"],
    };
    let sh = Shell::default();
    let plan = test_plan();

    Ovs.apply(&sh, &plan).expect("first apply");
    let obs = Ovs.observe(&sh, &plan).expect("observe");
    assert!(obs.fully_up(), "not fully up after first apply: {obs:?}");
    assert_eq!(obs.impairments.len(), 1);
    assert!(obs.impairments[0].healthy());

    // Idempotency: OVS's `--may-exist`/`set` and this file's own `--if-exists` teardown idiom
    // are worth nothing if a second apply doesn't converge cleanly.
    Ovs.apply(&sh, &plan).expect("second apply");
    let obs = Ovs.observe(&sh, &plan).expect("observe after re-apply");
    assert!(obs.fully_up(), "not fully up after re-apply: {obs:?}");

    // The trunk VLAN really lives on the OVS port, not merely tracked in the plan.
    let trunks = std::process::Command::new("ovs-vsctl")
        .args(["get", "port", &plan.uplink, "trunks"])
        .output()
        .expect("ovs-vsctl get trunks");
    let trunks = String::from_utf8_lossy(&trunks.stdout);
    assert!(
        trunks.contains("101"),
        "vlan 101 not in uplink trunks: {trunks}"
    );

    Ovs.teardown(&sh, &plan).expect("teardown");
    let obs = Ovs.observe(&sh, &plan).expect("observe after teardown");
    assert!(!obs.bridge_present, "bridge survived teardown");
    assert_eq!(obs.endpoints_present, 0);

    // Teardown on an already-torn-down plan must not error.
    Ovs.teardown(&sh, &plan).expect("second teardown");
}
