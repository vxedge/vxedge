//! Privileged integration tests: gateway provisioning + container zone
//! backend inside a dedicated network namespace. Idempotency proven by
//! double-apply/double-teardown.
//!
//! Ignored by default (needs CAP_NET_ADMIN / root). Run with:
//!   sudo -E env "PATH=$PATH" cargo test -p vxedge-core --test gateway_netns -- --ignored
//! CI runs this on ubuntu-latest (sudo available on hosted runners).

#![cfg(target_os = "linux")]

use vxedge_core::gateway::{GatewayPlan, Shell, apply, observe, teardown};
use vxedge_core::zones::backend::NodeState;
use vxedge_core::zones::{node_down, node_status, node_up, plan_nodes_from_model};

const WORKSPACE: &str = r#"
kind: experiment
id: exp-netns
vehicles: [veh]
baseline: bl
network:
  defaults: { bandwidth: 500000000 }
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }
    - { id: infotainment, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, max_rate_mbps: 400, priority: 5 } }
---
kind: vehicle
id: veh
zones: { front-left: {} }
devices:
  node-ecu-01:  { role: ecu, zone: front-left, backend: qemu-microvm }
  node-info-01: { role: ecu, zone: front-left, backend: container }
functions:
  fn-ecu:  { deployed_on: node-ecu-01 }
  fn-info: { deployed_on: node-info-01 }
links:
  - { id: l-adas,  endpoints: [node-ecu-01:eth0], slice: adas }
  - { id: l-infot, endpoints: [{ node: node-info-01, port: eth0, ip: 10.103.0.2/24 }], slice: infotainment }
---
kind: baseline
id: bl
parts:
  - { ref: /nonexistent/bundle, kind: bundle, target_ids: fn-ecu }
  - { ref: 'local/probe:host-process', target_ids: fn-info }
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

    let model = vxedge_core::model::load_yaml(WORKSPACE).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();

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
    let nodes = plan_nodes_from_model(&model, &plan, std::path::Path::new("/")).unwrap();
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

/// A vehicle whose one slice-bound link emulates a 100BASE-T1 segment. Deliberately separate
/// from `WORKSPACE` above: that fixture pins the unimpaired provisioning cycle every frozen
/// experiment runs on, and it stays that way.
const IMPAIRED_WORKSPACE: &str = r#"
kind: experiment
id: exp-netem
vehicles: [veh]
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
---
kind: vehicle
id: veh
devices:
  node-sensor-01: { role: sensor, backend: container }
links:
  - id: seg-t1
    endpoints: [node-sensor-01:eth0]
    slice: adas
    impairments: { rate_mbps: 100, delay_us: 250, jitter_us: 40, loss_pct: 0.01 }
"#;

/// Harness impairments against a real kernel.
///
/// The unit tests pin which port is elected and which tokens `tc` is handed; only a kernel can
/// say whether netem actually installs, whether the ingress redirect works at all, and whether
/// re-applying stacks a second redirect on top of the first. That last one is the reason this
/// test exists rather than being folded into the cycle test above: `tc filter add` appends, so
/// the idempotency failure would be invisible to `observe` — a second `up` would double the
/// inbound impairment while every check still reported a healthy gateway.
#[test]
#[ignore = "needs CAP_NET_ADMIN; run under sudo (see module docs)"]
fn impairments_install_both_directions_and_survive_reapply() {
    let ns = format!("vxnetem{}", std::process::id());
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

    let model = vxedge_core::model::load_yaml(IMPAIRED_WORKSPACE).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();
    let ep = &plan.endpoints[0];
    let imp = ep.impairment.as_ref().expect("the link is impaired");

    apply(&sh, &plan).unwrap();
    let obs = observe(&sh, &plan).unwrap();
    assert!(
        obs.fully_up(),
        "gateway not fully up with impairments (is the `ifb` module available?): {obs:?}"
    );
    assert_eq!(obs.impairments.len(), 1);
    assert_eq!(obs.impairments[0].link, "seg-t1");

    // Egress, and the declared values reaching the kernel — not merely "netem is present".
    let egress = sh
        .run("tc", &["qdisc", "show", "dev", &ep.name])
        .expect("read the access port's qdisc");
    assert!(
        egress.contains("netem"),
        "no netem on {}: {egress}",
        ep.name
    );
    // Rate is exact; delay is not, and asserting it exactly is the wrong test. netem stores
    // delay internally as a count of psched ticks and converts back to microseconds for display,
    // so a live kernel can (and, first run against a real one, did) report 249us for a declared
    // 250us — a one-tick rounding artifact of the kernel's own clock granularity, not a value
    // this code lost. Whatever "close enough" means is entirely kernel-and-config dependent, so
    // this checks the only thing actually promised: within a handful of microseconds, not a
    // gross unit slip (a millisecond or a percent read as microseconds).
    assert!(
        egress.contains("100Mbit"),
        "declared rate did not reach the kernel: {egress}"
    );
    let reported_delay: u32 = egress
        .split_whitespace()
        .skip_while(|w| *w != "delay")
        .nth(1)
        .and_then(|w| w.strip_suffix("us"))
        .and_then(|w| w.parse().ok())
        .unwrap_or_else(|| panic!("no parseable `delay <n>us` in: {egress}"));
    assert!(
        reported_delay.abs_diff(250) <= 5,
        "declared 250us landed as {reported_delay}us — outside plausible tick-rounding: {egress}"
    );

    // Ingress: the IFB exists, carries the same netem, and the access port redirects onto it.
    let ifb = sh
        .run("tc", &["qdisc", "show", "dev", &imp.ifb])
        .expect("read the ifb's qdisc");
    assert!(ifb.contains("netem"), "no netem on {}: {ifb}", imp.ifb);

    let redirects = |sh: &Shell| -> usize {
        sh.run(
            "tc",
            &["filter", "show", "dev", &ep.name, "parent", "ffff:"],
        )
        .expect("read the ingress filters")
        .lines()
        .filter(|l| l.contains("Egress Redirect to device"))
        .count()
    };
    assert_eq!(redirects(&sh), 1, "expected exactly one ingress redirect");

    // The whole point: a second `up` converges rather than accumulating.
    apply(&sh, &plan).unwrap();
    assert!(observe(&sh, &plan).unwrap().fully_up());
    assert_eq!(
        redirects(&sh),
        1,
        "re-apply stacked a second ingress redirect — inbound traffic is now impaired twice"
    );

    // Teardown takes the IFB with it. Left behind, it would be silently reused by the next run.
    teardown(&sh, &plan).unwrap();
    assert!(
        !sh.link_exists(&imp.ifb),
        "{} outlived the gateway that created it",
        imp.ifb
    );
    let gone = observe(&sh, &plan).unwrap();
    assert!(!gone.bridge_present && gone.endpoints_present == 0);
    teardown(&sh, &plan).unwrap();
}
