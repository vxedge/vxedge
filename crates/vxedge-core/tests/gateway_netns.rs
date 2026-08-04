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

/// A second usable IPv4 address in the same subnet as a declared CIDR (e.g. `10.103.0.2/24`
/// -> `("10.103.0.1", 24)`) — for placing a "far side" address on the same subnet as a node's
/// own, the production shape (`vxup0p.<vlan>` sharing a /24 with the zone), derived from the
/// node's actual declared address rather than a second literal picked to happen to match it.
fn sibling_address(cidr: &str) -> (String, u8) {
    let (ip, prefix) = cidr.split_once('/').expect("CIDR has a prefix");
    let prefix: u8 = prefix.parse().expect("prefix is numeric");
    let mut octets: Vec<u8> = ip.split('.').map(|o| o.parse().unwrap()).collect();
    assert_eq!(octets.len(), 4, "IPv4 only: {cidr}");
    octets[3] = if octets[3] == 1 { 2 } else { 1 };
    let addr = octets
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(".");
    (addr, prefix)
}

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

/// Deliberately its own fixture, not a reuse of `WORKSPACE` above: a device's netns name
/// (`vx-<device-id>`) carries no per-test salt, so two tests sharing a device id race for the
/// identical netns when `cargo test` runs them concurrently (found live — this test's first
/// version reused `node-info-01` and broke `gateway_and_container_zone_cycle_in_netns` with
/// "File exists" on `eth0`, an actual collision, not a hypothetical one). Same reasoning
/// `IMPAIRED_WORKSPACE` above already established for its own distinct device id.
const LOCALITY_WORKSPACE: &str = r#"
kind: experiment
id: exp-locality
vehicles: [veh]
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
---
kind: vehicle
id: veh
devices:
  node-locality-01: { role: ecu, backend: container }
links:
  - { id: l-locality, endpoints: [{ node: node-locality-01, port: eth0, ip: 10.109.0.2/24 }], slice: adas }
"#;
/// The one place this test's device id is spelled out — every other reference below reads
/// from here, so renaming or reusing the fixture can't silently drift the two apart again.
const LOCALITY_DEVICE_ID: &str = "node-locality-01";

/// Regression test for docs/TASKS.md #24/#28: exp-001 and exp-003 measured their probe/
/// reflector exchange over loopback for months, because both addresses lived in the same
/// network namespace — the kernel's `local` routing table delivers to a same-netns local
/// address directly, never touching the bridge, HTB tree, or VLAN filtering the manifest
/// declares. Found live against the real campaign scripts, not just an isolated repro; this
/// is what makes sure it never comes back silently.
///
/// Two independent signals, matching how the original bug was actually characterized:
/// the routing *decision* from inside the zone's own netns (must not read `dev lo`), and
/// actual packet delivery on the access port via its own interface counters — `ping`, not a
/// parsed capture, the same tool the isolated repro used first ("reproduced with plain
/// ping, not vxprobe-specific").
#[test]
#[ignore = "needs CAP_NET_ADMIN; run under sudo (see module docs)"]
fn zone_traffic_reaches_the_access_port_not_loopback() {
    let ns = format!("vxlocal{}", std::process::id());
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

    let model = vxedge_core::model::load_yaml(LOCALITY_WORKSPACE).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();
    apply(&sh, &plan).unwrap();

    let runtime =
        std::env::temp_dir().join(format!("vxzones-locality-test-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let nodes = plan_nodes_from_model(&model, &plan, std::path::Path::new("/")).unwrap();
    let container = nodes
        .iter()
        .find(|n| n.node_id == LOCALITY_DEVICE_ID)
        .expect("container node planned");
    node_up(&sh, container, &runtime).unwrap();

    let endpoint = plan
        .endpoints
        .iter()
        .find(|e| e.node == LOCALITY_DEVICE_ID)
        .expect("device's endpoint planned");
    let slice = plan
        .slices
        .iter()
        .find(|s| s.id == endpoint.slice.as_deref().unwrap_or_default())
        .expect("device's slice planned");

    // Far side: a VLAN subinterface on the uplink's own peer, on the node's own slice and
    // subnet — the exact production shape (`vxup0p.<vlan>`, sharing one /24 with the zone's
    // own address, e.g. `10.103.0.2` + `10.103.0.1`), derived from the node's declared CIDR
    // rather than a second literal picked to happen to match it. A *different* subnet (tried
    // first, before this fix) makes the container's netns refuse the route entirely
    // ("Network is unreachable") rather than exercise the shortcut this test checks for —
    // same-subnet placement is precisely what let the real bug happen.
    let node_cidr = container
        .ip_cidr
        .as_deref()
        .expect("the device has a declared ip_cidr");
    let (far_side_addr, prefix_len) = sibling_address(node_cidr);
    let far_side_dev = format!("{}.{}", plan.uplink_peer, slice.vlan);
    sh.run(
        "ip",
        &[
            "link",
            "add",
            "link",
            &plan.uplink_peer,
            "name",
            &far_side_dev,
            "type",
            "vlan",
            "id",
            &slice.vlan.to_string(),
        ],
    )
    .unwrap();
    sh.run(
        "ip",
        &[
            "addr",
            "add",
            &format!("{far_side_addr}/{prefix_len}"),
            "dev",
            &far_side_dev,
        ],
    )
    .unwrap();
    sh.run("ip", &["link", "set", &far_side_dev, "up"]).unwrap();
    sh.run("ip", &["link", "set", &plan.uplink_peer, "up"])
        .unwrap();

    // Named netns are host-wide, not nested under `sh.netns` — reaching the container's own
    // sibling netns needs `run_host`, exactly as the cycle test above does for the same reason.
    let container_ns = container.runtime_name();
    let route = sh
        .run_host(
            "ip",
            &[
                "netns",
                "exec",
                &container_ns,
                "ip",
                "route",
                "get",
                &far_side_addr,
            ],
        )
        .unwrap();
    assert!(
        !route.contains(" lo "),
        "sender resolved the far side via loopback instead of the access port: {route}"
    );

    // Packet delivery: the access port's own rx counter, read from inside `sh.netns` since
    // that's where the bridge and its ports live — not a parsed tcpdump capture.
    let rx_stat_path = format!("/sys/class/net/{}/statistics/rx_packets", endpoint.name);
    let read_rx = |sh: &Shell| -> u64 {
        sh.run("cat", &[rx_stat_path.as_str()])
            .unwrap()
            .trim()
            .parse()
            .expect("rx_packets is an integer")
    };
    let rx_before = read_rx(&sh);
    let ping = sh
        .run_host(
            "ip",
            &[
                "netns",
                "exec",
                &container_ns,
                "ping",
                "-c",
                "5",
                "-W",
                "1",
                &far_side_addr,
            ],
        )
        .unwrap();
    assert!(
        ping.contains("5 received") || ping.contains("5 packets received"),
        "ping to the far side did not complete: {ping}"
    );
    let rx_after = read_rx(&sh);
    assert!(
        rx_after >= rx_before + 5,
        "only {} new packets on {} (expected >= 5) — traffic took a shortcut instead of \
         crossing the access port",
        rx_after - rx_before,
        endpoint.name
    );

    node_down(&sh, container, &runtime).unwrap();
    teardown(&sh, &plan).unwrap();
    let _ = std::fs::remove_dir_all(&runtime);
}

/// Deliberately its own fixture and device id, for the same reason `IMPAIRED_WORKSPACE` and
/// `LOCALITY_WORKSPACE` each got their own above: netns names are host-wide and carry no
/// per-test salt.
const ANTISPOOF_WORKSPACE: &str = r#"
kind: experiment
id: exp-antispoof
vehicles: [veh]
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
---
kind: vehicle
id: veh
devices:
  node-spoof-01: { role: ecu, backend: container }
links:
  - { id: l-spoof, endpoints: [{ node: node-spoof-01, port: eth0, ip: 10.113.0.2/24 }], slice: adas }
"#;
const ANTISPOOF_DEVICE_ID: &str = "node-spoof-01";

/// Regression/acceptance test for docs/TASKS.md #16's nftables anti-spoof ingress filter: not
/// just "the table exists" (the unprivileged unit tests already cover plan-building), but that
/// a real kernel actually drops a packet whose source address doesn't match the endpoint's
/// declared one, while a packet from the declared address still gets through. VLAN isolation
/// already makes cross-slice traffic structurally impossible, so the only real gap this closes
/// is same-slice spoofing — this test's spoofed address (`10.113.0.99`) is deliberately in the
/// *same* subnet as the declared one, not a different VLAN, to exercise exactly that gap.
#[test]
#[ignore = "needs CAP_NET_ADMIN; run under sudo (see module docs)"]
fn ingress_filter_drops_spoofed_source_but_allows_declared_one() {
    let ns = format!("vxspoof{}", std::process::id());
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

    let model = vxedge_core::model::load_yaml(ANTISPOOF_WORKSPACE).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();
    let endpoint = plan
        .endpoints
        .iter()
        .find(|e| e.node == ANTISPOOF_DEVICE_ID)
        .expect("device's endpoint planned");
    assert_eq!(
        endpoint.expected_source.as_deref(),
        Some("10.113.0.2/24"),
        "expected_source not derived from the link's declared ip"
    );

    apply(&sh, &plan).unwrap();
    let obs = observe(&sh, &plan).unwrap();
    assert!(obs.fully_up(), "gateway not fully up: {obs:?}");
    assert_eq!(obs.ingress_filters.len(), 1);
    assert!(obs.ingress_filters[0].present, "filter not installed");

    let runtime =
        std::env::temp_dir().join(format!("vxzones-antispoof-test-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let nodes = plan_nodes_from_model(&model, &plan, std::path::Path::new("/")).unwrap();
    let container = nodes
        .iter()
        .find(|n| n.node_id == ANTISPOOF_DEVICE_ID)
        .expect("container node planned");
    node_up(&sh, container, &runtime).unwrap();

    let slice = plan
        .slices
        .iter()
        .find(|s| s.id == endpoint.slice.as_deref().unwrap_or_default())
        .expect("device's slice planned");
    let node_cidr = container
        .ip_cidr
        .as_deref()
        .expect("the device has a declared ip_cidr");
    let (far_side_addr, prefix_len) = sibling_address(node_cidr);
    let far_side_dev = format!("{}.{}", plan.uplink_peer, slice.vlan);
    sh.run(
        "ip",
        &[
            "link",
            "add",
            "link",
            &plan.uplink_peer,
            "name",
            &far_side_dev,
            "type",
            "vlan",
            "id",
            &slice.vlan.to_string(),
        ],
    )
    .unwrap();
    sh.run(
        "ip",
        &[
            "addr",
            "add",
            &format!("{far_side_addr}/{prefix_len}"),
            "dev",
            &far_side_dev,
        ],
    )
    .unwrap();
    sh.run("ip", &["link", "set", &far_side_dev, "up"]).unwrap();
    sh.run("ip", &["link", "set", &plan.uplink_peer, "up"])
        .unwrap();

    let container_ns = container.runtime_name();

    // Legitimate traffic, from the node's own declared address (its default source for this
    // subnet) — must not be caught by its own anti-spoof filter.
    let legit = sh
        .run_host(
            "ip",
            &[
                "netns",
                "exec",
                &container_ns,
                "ping",
                "-c",
                "5",
                "-W",
                "1",
                &far_side_addr,
            ],
        )
        .unwrap();
    assert!(
        legit.contains("5 received") || legit.contains("5 packets received"),
        "legitimate ping (from the declared address) did not get through: {legit}"
    );

    // Spoofed traffic: a second address on the same interface, same subnet (not a different
    // VLAN — that's already unreachable regardless of this filter), explicitly not the
    // declared one.
    let spoofed_addr = "10.113.0.99";
    sh.run_host(
        "ip",
        &[
            "netns",
            "exec",
            &container_ns,
            "ip",
            "addr",
            "add",
            &format!("{spoofed_addr}/24"),
            "dev",
            "eth0",
        ],
    )
    .unwrap();
    // `Shell::run_host` treats a non-zero exit as `Err` and does not carry stdout on that path
    // (`netops.rs`'s `NetOpsError::Command` — by design, "the command failed" is what callers
    // act on, not its output text). `ping` exits non-zero on 100% loss, so that `Err` itself
    // *is* the signal here — there is no `Ok` text to pattern-match the way `legit` above does.
    let spoofed = sh.run_host(
        "ip",
        &[
            "netns",
            "exec",
            &container_ns,
            "ping",
            "-c",
            "5",
            "-W",
            "1",
            "-I",
            spoofed_addr,
            &far_side_addr,
        ],
    );
    assert!(
        spoofed.is_err(),
        "spoofed ping (from an address other than the declared one) unexpectedly succeeded: {spoofed:?}"
    );

    // Not just "the ping failed" — the drop rule's own counter proves *this* filter is why,
    // rather than some unrelated routing failure.
    let table = format!("vxaf-{}", endpoint.name);
    let nft_out = sh
        .run("nft", &["list", "table", "netdev", &table])
        .expect("read the ingress filter table");
    let dropped_packets: u64 = nft_out
        .lines()
        .find(|l| l.contains("drop"))
        .and_then(|l| l.split_whitespace().skip_while(|w| *w != "packets").nth(1))
        .and_then(|w| w.parse().ok())
        .unwrap_or_else(|| panic!("no parseable drop-rule packet counter in: {nft_out}"));
    assert!(
        dropped_packets >= 5,
        "drop rule counter is {dropped_packets}, expected >= 5 spoofed packets: {nft_out}"
    );

    node_down(&sh, container, &runtime).unwrap();
    teardown(&sh, &plan).unwrap();
    let gone = observe(&sh, &plan).unwrap();
    assert!(
        gone.ingress_filters.iter().all(|f| !f.present),
        "ingress filter table survived teardown"
    );
    let _ = std::fs::remove_dir_all(&runtime);
}

/// Two simulated "hosts," each with one WireGuard tunnel declared and a placeholder peer key —
/// no handshake is possible until each side's real, kernel-minted public key is learned and
/// substituted in (`PLACEHOLDER_PEER_KEY`, replaced in the test itself). `address`/`allowed_ips`
/// (10.240.0.0/30) are the tunnel's own overlay subnet; `endpoint` points at the *other* fixture
/// below's `wan*` address — the plain veth pair the test wires up to stand in for the internet
/// between two real hosts. One throwaway VLAN slice with no devices, only because
/// `GatewayPlan::from_model` refuses a manifest with zero slices of any kind
/// (`PlanError::NoSlices`) — irrelevant to what this test actually exercises.
const TUNNEL_SITE_A_WORKSPACE: &str = r#"
kind: experiment
id: exp-tunnel-a
vehicles: [veh]
network:
  slices:
    - { id: dummy, mode: vlan, vlan: 101 }
  tunnels:
    - id: ota-uplink
      listen_port: 51820
      address: 10.240.0.1/30
      peer: { public_key: 'PLACEHOLDER_PEER_KEY', endpoint: '10.250.0.2:51821', allowed_ips: [10.240.0.2/32] }
---
kind: vehicle
id: veh
devices: {}
"#;
const TUNNEL_SITE_B_WORKSPACE: &str = r#"
kind: experiment
id: exp-tunnel-b
vehicles: [veh]
network:
  slices:
    - { id: dummy, mode: vlan, vlan: 101 }
  tunnels:
    - id: ota-downlink
      listen_port: 51821
      address: 10.240.0.2/30
      peer: { public_key: 'PLACEHOLDER_PEER_KEY', endpoint: '10.250.0.1:51820', allowed_ips: [10.240.0.1/32] }
---
kind: vehicle
id: veh
devices: {}
"#;

/// Regression/acceptance test for docs/TASKS.md #16's WireGuard link setup. Not just "the
/// interface exists" — the unit tests in `plan.rs` already cover plan derivation — but that two
/// independent gateways, simulated as two network namespaces connected only by a plain veth pair
/// standing in for the internet between two real hosts, complete a **real WireGuard handshake**
/// and pass traffic over the resulting encrypted overlay. Also exercises the actual operator
/// workflow this design is built around: `up` once to mint an identity (never written to disk —
/// see `provision::apply_tunnel`'s doc comment), read it back via `observe`/`status`, hand it to
/// the other side out of band, re-apply with the corrected peer key — an idempotent
/// *reconfigure*, not a no-op, and it has to converge cleanly rather than erroring on an
/// interface that already exists.
#[test]
#[ignore = "needs CAP_NET_ADMIN and wireguard-tools (`wg`); run under sudo (see module docs)"]
fn tunnel_completes_a_real_handshake_between_two_simulated_hosts() {
    let ns_a = format!("vxwga{}", std::process::id());
    let ns_b = format!("vxwgb{}", std::process::id());
    for ns in [&ns_a, &ns_b] {
        let created = std::process::Command::new("ip")
            .args(["netns", "add", ns])
            .output()
            .expect("iproute2 installed");
        assert!(
            created.status.success(),
            "cannot create netns (run as root): {}",
            String::from_utf8_lossy(&created.stderr)
        );
    }
    let _guard_a = NetnsGuard(ns_a.clone());
    let _guard_b = NetnsGuard(ns_b.clone());
    let sh_a = Shell {
        netns: Some(ns_a.clone()),
        escalate: false,
    };
    let sh_b = Shell {
        netns: Some(ns_b.clone()),
        escalate: false,
    };

    // The "internet" between the two simulated hosts: a plain veth pair, one end per netns, on
    // its own subnet — distinct from the tunnel's own overlay addresses below. Created and moved
    // from the host context (`run_host`), same reasoning `gateway_and_container_zone_cycle_in_
    // netns` already documents for named-netns operations: a bind mount created from inside `ip
    // netns exec` does not propagate.
    sh_a.run_host(
        "ip",
        &[
            "link", "add", "wanA", "type", "veth", "peer", "name", "wanB",
        ],
    )
    .unwrap();
    sh_a.run_host("ip", &["link", "set", "wanA", "netns", &ns_a])
        .unwrap();
    sh_a.run_host("ip", &["link", "set", "wanB", "netns", &ns_b])
        .unwrap();
    sh_a.run("ip", &["addr", "add", "10.250.0.1/30", "dev", "wanA"])
        .unwrap();
    sh_a.run("ip", &["link", "set", "wanA", "up"]).unwrap();
    sh_b.run("ip", &["addr", "add", "10.250.0.2/30", "dev", "wanB"])
        .unwrap();
    sh_b.run("ip", &["link", "set", "wanB", "up"]).unwrap();

    // `wg` validates a peer's key strictly (format *and* curve validity, found live: a literal
    // placeholder string like `PLACEHOLDER_PEER_KEY` fails the whole `wg set` call with "Key is
    // not the correct length or format" before anything else in that call gets applied). The
    // fixtures' placeholder therefore has to be a real, if meaningless, WireGuard key — a
    // throwaway keypair's public half, generated the same way `apply_tunnel` generates a real
    // one, rather than a hand-encoded literal that risks the same mistake.
    let toolsh = Shell::default();
    let placeholder_priv = toolsh.run("wg", &["genkey"]).unwrap();
    let placeholder_pub = toolsh
        .run_with_stdin("wg", &["pubkey"], placeholder_priv.trim())
        .unwrap()
        .trim()
        .to_string();

    let plan_a = GatewayPlan::from_model(
        &vxedge_core::model::load_yaml(&TUNNEL_SITE_A_WORKSPACE.replacen(
            "PLACEHOLDER_PEER_KEY",
            &placeholder_pub,
            1,
        ))
        .unwrap(),
    )
    .unwrap();
    let plan_b = GatewayPlan::from_model(
        &vxedge_core::model::load_yaml(&TUNNEL_SITE_B_WORKSPACE.replacen(
            "PLACEHOLDER_PEER_KEY",
            &placeholder_pub,
            1,
        ))
        .unwrap(),
    )
    .unwrap();

    // First apply: each side mints its own identity. Still carrying the placeholder peer key —
    // no handshake is possible yet, which is expected: `apply` itself doesn't require one (see
    // `TunnelObservation::handshake_established`'s own doc comment on why that is deliberately
    // not a `fully_up` condition).
    apply(&sh_a, &plan_a).unwrap();
    apply(&sh_b, &plan_b).unwrap();

    let obs_a = observe(&sh_a, &plan_a).unwrap();
    let obs_b = observe(&sh_b, &plan_b).unwrap();
    assert!(obs_a.fully_up(), "site A not fully up: {obs_a:?}");
    assert!(obs_b.fully_up(), "site B not fully up: {obs_b:?}");
    let pubkey_a = obs_a.tunnels[0].public_key.clone();
    let pubkey_b = obs_b.tunnels[0].public_key.clone();
    assert!(
        !pubkey_a.is_empty() && !pubkey_b.is_empty(),
        "public keys not read back from the kernel: a={pubkey_a:?} b={pubkey_b:?}"
    );
    assert_ne!(pubkey_a, pubkey_b, "both sides minted the same identity");

    // The real operator workflow: correct each side's peer key to what the other side actually
    // minted, then re-apply.
    let plan_a = GatewayPlan::from_model(
        &vxedge_core::model::load_yaml(&TUNNEL_SITE_A_WORKSPACE.replacen(
            "PLACEHOLDER_PEER_KEY",
            &pubkey_b,
            1,
        ))
        .unwrap(),
    )
    .unwrap();
    let plan_b = GatewayPlan::from_model(
        &vxedge_core::model::load_yaml(&TUNNEL_SITE_B_WORKSPACE.replacen(
            "PLACEHOLDER_PEER_KEY",
            &pubkey_a,
            1,
        ))
        .unwrap(),
    )
    .unwrap();
    apply(&sh_a, &plan_a).unwrap();
    apply(&sh_b, &plan_b).unwrap();

    // Force a handshake: ping across the tunnel's own overlay addresses, not the underlying WAN
    // link — this only succeeds if the packet is actually encrypted, sent over `wanA`/`wanB`,
    // decrypted, and answered.
    let ping = sh_a
        .run("ping", &["-c", "3", "-W", "2", "10.240.0.2"])
        .unwrap();
    assert!(
        ping.contains("3 received") || ping.contains("3 packets received"),
        "ping across the tunnel overlay did not complete: {ping}"
    );

    let obs_a = observe(&sh_a, &plan_a).unwrap();
    let obs_b = observe(&sh_b, &plan_b).unwrap();
    assert!(
        obs_a.tunnels[0].handshake_established,
        "site A recorded no handshake after real traffic crossed the tunnel"
    );
    assert!(
        obs_b.tunnels[0].handshake_established,
        "site B recorded no handshake after real traffic crossed the tunnel"
    );

    teardown(&sh_a, &plan_a).unwrap();
    teardown(&sh_b, &plan_b).unwrap();
    assert!(!observe(&sh_a, &plan_a).unwrap().tunnels[0].present);
    assert!(!observe(&sh_b, &plan_b).unwrap().tunnels[0].present);
}

// Both sites declare the *same* VLAN id for their own locally-named slice — that agreement,
// plus the same VNI, is what makes the two sides' bridges join into one L2 domain once the
// VXLAN tunnel is up: an 802.1Q-tagged frame the local bridge forwards onto the tunnel arrives
// at the other side still tagged for the same VLAN, which that side's own bridge is a member of
// for the same reason. A real operator declares this agreement independently in each site's own
// manifest, exactly as these two fixtures do.
const VXLAN_SITE_A_WORKSPACE: &str = r#"
kind: experiment
id: exp-vxlan-a
vehicles: [veh]
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
  tunnels:
    - { id: site-b, kind: vxlan, vni: 100, remote: 10.250.0.2, slices: [adas] }
---
kind: vehicle
id: veh
devices:
  node-a: { role: ecu, backend: container }
links:
  - { id: l-a, endpoints: [{ node: node-a, port: eth0, ip: 10.113.0.2/24 }], slice: adas }
"#;
const VXLAN_SITE_B_WORKSPACE: &str = r#"
kind: experiment
id: exp-vxlan-b
vehicles: [veh]
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
  tunnels:
    - { id: site-a, kind: vxlan, vni: 100, remote: 10.250.0.1, slices: [adas] }
---
kind: vehicle
id: veh
devices:
  node-b: { role: ecu, backend: container }
links:
  - { id: l-b, endpoints: [{ node: node-b, port: eth0, ip: 10.113.0.3/24 }], slice: adas }
"#;

/// Regression/acceptance test for docs/TASKS.md #16's VXLAN tunnel provisioning. Not just "the
/// interface exists" (`plan.rs`'s own unit tests already cover plan derivation) but that two
/// independent gateways, simulated as two network namespaces connected only by a plain veth pair
/// standing in for the internet between two real hosts (same `wanA`/`wanB` shape the WireGuard
/// test above uses), actually join their bridges into one L2 domain: a real container zone on
/// site A reaches a real container zone on site B, over their shared `adas` slice, with the only
/// path between the two netns being the VXLAN-encapsulated UDP traffic crossing `wanA`/`wanB`.
/// Structurally different from the WireGuard test on purpose — a VXLAN tunnel joins the bridge
/// as a trunk-like port rather than staying a routed L3 endpoint, so this can't be device-less
/// the way that one is.
#[test]
#[ignore = "needs CAP_NET_ADMIN; run under sudo (see module docs)"]
fn vxlan_tunnel_carries_slice_traffic_between_two_simulated_hosts() {
    let ns_a = format!("vxvxa{}", std::process::id());
    let ns_b = format!("vxvxb{}", std::process::id());
    for ns in [&ns_a, &ns_b] {
        let created = std::process::Command::new("ip")
            .args(["netns", "add", ns])
            .output()
            .expect("iproute2 installed");
        assert!(
            created.status.success(),
            "cannot create netns (run as root): {}",
            String::from_utf8_lossy(&created.stderr)
        );
    }
    let _guard_a = NetnsGuard(ns_a.clone());
    let _guard_b = NetnsGuard(ns_b.clone());
    let sh_a = Shell {
        netns: Some(ns_a.clone()),
        escalate: false,
    };
    let sh_b = Shell {
        netns: Some(ns_b.clone()),
        escalate: false,
    };

    // The "internet" between the two simulated hosts — identical shape to the WireGuard test's
    // own `wanA`/`wanB`, and for the same reason (`run_host`: a bind mount created from inside
    // `ip netns exec` does not propagate).
    sh_a.run_host(
        "ip",
        &[
            "link", "add", "wanA", "type", "veth", "peer", "name", "wanB",
        ],
    )
    .unwrap();
    sh_a.run_host("ip", &["link", "set", "wanA", "netns", &ns_a])
        .unwrap();
    sh_a.run_host("ip", &["link", "set", "wanB", "netns", &ns_b])
        .unwrap();
    sh_a.run("ip", &["addr", "add", "10.250.0.1/30", "dev", "wanA"])
        .unwrap();
    sh_a.run("ip", &["link", "set", "wanA", "up"]).unwrap();
    sh_b.run("ip", &["addr", "add", "10.250.0.2/30", "dev", "wanB"])
        .unwrap();
    sh_b.run("ip", &["link", "set", "wanB", "up"]).unwrap();

    let model_a = vxedge_core::model::load_yaml(VXLAN_SITE_A_WORKSPACE).unwrap();
    let model_b = vxedge_core::model::load_yaml(VXLAN_SITE_B_WORKSPACE).unwrap();
    let plan_a = GatewayPlan::from_model(&model_a).unwrap();
    let plan_b = GatewayPlan::from_model(&model_b).unwrap();

    apply(&sh_a, &plan_a).unwrap();
    apply(&sh_b, &plan_b).unwrap();

    let obs_a = observe(&sh_a, &plan_a).unwrap();
    let obs_b = observe(&sh_b, &plan_b).unwrap();
    assert!(obs_a.fully_up(), "site A not fully up: {obs_a:?}");
    assert!(obs_b.fully_up(), "site B not fully up: {obs_b:?}");
    assert_eq!(obs_a.tunnels[0].kind, vxedge_core::model::TunnelKind::Vxlan);
    // No key material and no handshake concept at all — distinct from the WireGuard test's own
    // assertions on the same fields, which is the whole point of the two kinds' own distinction.
    assert!(obs_a.tunnels[0].public_key.is_empty());
    assert!(!obs_a.tunnels[0].handshake_established);

    let runtime_a = std::env::temp_dir().join(format!("vxzones-vxlan-a-{}", std::process::id()));
    let runtime_b = std::env::temp_dir().join(format!("vxzones-vxlan-b-{}", std::process::id()));
    std::fs::create_dir_all(&runtime_a).unwrap();
    std::fs::create_dir_all(&runtime_b).unwrap();
    let nodes_a = plan_nodes_from_model(&model_a, &plan_a, std::path::Path::new("/")).unwrap();
    let nodes_b = plan_nodes_from_model(&model_b, &plan_b, std::path::Path::new("/")).unwrap();
    let node_a = nodes_a
        .iter()
        .find(|n| n.node_id == "node-a")
        .expect("node-a planned");
    let node_b = nodes_b
        .iter()
        .find(|n| n.node_id == "node-b")
        .expect("node-b planned");
    node_up(&sh_a, node_a, &runtime_a).unwrap();
    node_up(&sh_b, node_b, &runtime_b).unwrap();

    // The real proof: a ping from site A's container to site B's container, over their shared
    // `adas` slice — the *only* path between the two netns is the VXLAN-encapsulated UDP
    // traffic crossing `wanA`/`wanB`, so this only succeeds if the tunnel is genuinely carrying
    // the slice's L2 traffic, not merely present as an interface.
    let node_b_addr = node_b
        .ip_cidr
        .as_deref()
        .and_then(|cidr| cidr.split_once('/'))
        .map(|(ip, _)| ip)
        .expect("node-b has a declared ip_cidr");
    let container_ns_a = node_a.runtime_name();
    let ping = sh_a
        .run_host(
            "ip",
            &[
                "netns",
                "exec",
                &container_ns_a,
                "ping",
                "-c",
                "5",
                "-W",
                "2",
                node_b_addr,
            ],
        )
        .unwrap();
    assert!(
        ping.contains("5 received") || ping.contains("5 packets received"),
        "ping across the vxlan overlay did not complete: {ping}"
    );

    node_down(&sh_a, node_a, &runtime_a).unwrap();
    node_down(&sh_b, node_b, &runtime_b).unwrap();
    teardown(&sh_a, &plan_a).unwrap();
    teardown(&sh_b, &plan_b).unwrap();
    assert!(!observe(&sh_a, &plan_a).unwrap().tunnels[0].present);
    assert!(!observe(&sh_b, &plan_b).unwrap().tunnels[0].present);
    let _ = std::fs::remove_dir_all(&runtime_a);
    let _ = std::fs::remove_dir_all(&runtime_b);
}
