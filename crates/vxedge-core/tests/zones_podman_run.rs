//! Privileged integration test: `run` on a baseline part (`ARCHITECTURE.md` §6.0.2). Two
//! `container` devices on one vlan slice run the vxprobe reflector, each started by its own
//! entry's `run`, and the test checks that it took effect: the command line is the image's
//! entrypoint followed by `run.args`, each reflector answers on the port its args name and not on
//! the image's default, `cap_add: [NET_ADMIN]` lets one payload change its routes while the other
//! cannot, and a changed `run` recreates the payload while the node keeps its namespace.
//!
//! It also records how UDP multicast to 224.0.0.0/4 behaves between the two devices across the
//! gateway's kernel bridge, for DDS discovery (239.255.0.1:7400) and SOME/IP service discovery
//! (224.244.224.245:30490): sent with no multicast route, sent with `IP_MULTICAST_IF`, and sent
//! after the payload adds a `224.0.0.0/4` route itself.
//!
//! **Do not run under `sudo`**, for the reason `zones_podman.rs` gives. It also needs `python3` on
//! the host, run inside each node's namespace through `nsenter`. Run with:
//!   sudo -v
//!   BIN=$(cargo test -p vxedge-core --test zones_podman_run --no-run --message-format=json \
//!     | jq -r 'select(.target.name=="zones_podman_run" and .executable) | .executable')
//!   sudo setcap cap_net_admin,cap_sys_admin+ep "$BIN"   # again after every rebuild
//!   "$BIN" --ignored --nocapture

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use vxedge_core::gateway::{GatewayPlan, Shell, apply, teardown};
use vxedge_core::zones::backend::NodeState;
use vxedge_core::zones::{NodePlan, node_down, node_status, node_up, plan_nodes_from_model};

/// vxprobe 0.1.1, the published reflector the vxperiments workspaces pin. Its entrypoint is
/// `/vxprobe reflect`, so `run.args` lands after `reflect`, and it carries busybox.
const PROBE: &str = "ghcr.io/vxedge/parts/agent/probe/app/vxprobe";
const PROBE_DIGEST: &str =
    "sha256:b3f22650146dcae004589d311fcd8f231bd2e058f8cb50a7d2a766519210fd6d";

const IP_A: &str = "10.101.0.41";
const IP_B: &str = "10.101.0.42";

/// `run_b` is the `run` of node B's entry; node A's is fixed.
fn workspace(run_b: &str) -> String {
    format!(
        r#"
kind: experiment
id: exp-run
vehicles: [veh]
baseline: bl
network:
  slices:
    - {{ id: adas, mode: vlan, vlan: 101 }}
---
kind: vehicle
id: veh
zones: {{ central: {{}} }}
devices:
  node-run-a: {{ role: ecu, zone: central, backend: container }}
  node-run-b: {{ role: ecu, zone: central, backend: container }}
functions:
  fn-a: {{ deployed_on: node-run-a }}
  fn-b: {{ deployed_on: node-run-b }}
links:
  - {{ id: l-a, endpoints: [{{ node: node-run-a, port: eth0, ip: {IP_A}/24 }}], slice: adas }}
  - {{ id: l-b, endpoints: [{{ node: node-run-b, port: eth0, ip: {IP_B}/24 }}], slice: adas }}
---
kind: baseline
id: bl
parts:
  - ref: {PROBE}
    kind: container
    digest: '{PROBE_DIGEST}'
    target_ids: fn-a
    run: {{ args: [--listen, '0.0.0.0:9812'], cap_add: [NET_ADMIN] }}
  - ref: {PROBE}
    kind: container
    digest: '{PROBE_DIGEST}'
    target_ids: fn-b
    run: {run_b}
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

/// Takes the nodes down even if assertions panic, so no container group outlives the test.
struct NodesGuard {
    sh: Shell,
    nodes: Vec<NodePlan>,
    runtime: PathBuf,
}

impl Drop for NodesGuard {
    fn drop(&mut self) {
        for node in &self.nodes {
            let _ = node_down(&self.sh, node, &self.runtime);
        }
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

fn sudo(args: &[&str]) -> std::process::Output {
    Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .expect("sudo runs")
}

/// `podman` as this user, never through sudo.
fn podman(args: &[&str]) -> std::process::Output {
    Command::new("podman")
        .args(args)
        .output()
        .expect("podman runs")
}

fn podman_pid(container: &str) -> Option<u32> {
    let out = podman(&["inspect", "-f", "{{.State.Pid}}", container]);
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .expect("own rootless container's cmdline is readable")
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Probes sent from node A's payload to `target` that came back.
fn probes_answered(target: &str) -> u64 {
    let out = podman(&[
        "exec",
        "vx-node-run-a-payload",
        "/vxprobe",
        "send",
        "--target",
        target,
        "--pps",
        "10",
        "--duration-s",
        "1",
        "--timeout-ms",
        "300",
    ]);
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "vxprobe send to {target}: {e}: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    report["received"].as_u64().expect("a received count")
}

/// Joins `groups` on `ifaddr` and prints every datagram that arrives for 6 s, after `ready`.
const RECEIVER: &str = r#"
import select, socket, sys, time
ifaddr = sys.argv[1]
socks = []
for spec in sys.argv[2:]:
    group, port = spec.split(":")
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((group, int(port)))
    s.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP,
                 socket.inet_aton(group) + socket.inet_aton(ifaddr))
    socks.append(s)
print("ready", flush=True)
end = time.time() + 6
while time.time() < end:
    ready, _, _ = select.select(socks, [], [], max(0, end - time.time()))
    for s in ready:
        data, src = s.recvfrom(2048)
        print("got", data.decode(), src[0], flush=True)
"#;

/// Sends three datagrams tagged `<tag>-<group>` to each `group:port`, with `IP_MULTICAST_IF` set
/// to `src` when the mode is `if`. Prints `sent <group>` or `err <group> <errno name>`.
const SENDER: &str = r#"
import errno, socket, sys
mode, src, tag = sys.argv[1:4]
for spec in sys.argv[4:]:
    group, port = spec.split(":")
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
    if mode == "if":
        s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(src))
    try:
        for _ in range(3):
            s.sendto(f"{tag}-{group}".encode(), (group, int(port)))
        print("sent", group, flush=True)
    except OSError as e:
        print("err", group, errno.errorcode.get(e.errno, e.errno), flush=True)
"#;

const GROUPS: [&str; 2] = ["239.255.0.1:7400", "224.244.224.245:30490"];

fn in_netns(pid: u32, program: &[&str]) -> Command {
    let mut cmd = Command::new("sudo");
    cmd.args(["-n", "nsenter", "-t", &pid.to_string(), "-n"])
        .args(program);
    cmd
}

fn send(pid: u32, mode: &str, tag: &str) -> String {
    let out = in_netns(pid, &["python3", "-c", SENDER, mode, IP_A, tag])
        .args(GROUPS)
        .output()
        .expect("sender runs");
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

fn gateway_dump(ns: &str) -> String {
    let mut dump = String::new();
    for cmd in [
        &["ip", "-d", "link", "show", "type", "bridge"][..],
        &["bridge", "mdb", "show"],
        &["nft", "list", "ruleset", "netdev"],
    ] {
        let mut args = vec!["ip", "netns", "exec", ns];
        args.extend_from_slice(cmd);
        let out = sudo(&args);
        dump += &format!(
            "$ {}\n{}{}\n",
            cmd.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    dump
}

fn node<'a>(nodes: &'a [NodePlan], id: &str) -> &'a NodePlan {
    nodes.iter().find(|n| n.node_id == id).unwrap()
}

#[test]
#[ignore = "needs the user Podman socket, a cached sudo credential, python3 and file \
            capabilities; run unprivileged (see module docs)"]
fn a_parts_run_starts_its_payload_and_multicast_crosses_the_slice() {
    let ns = format!("vxrun{}", std::process::id());
    let created = sudo(&["ip", "netns", "add", &ns]);
    assert!(
        created.status.success(),
        "cannot create netns (cache a credential with `sudo -v` first): {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let _netns = NetnsGuard(ns.clone());
    let sh = Shell {
        netns: Some(ns.clone()),
        escalate: true,
    };

    let run_b = "{ args: [--listen, '0.0.0.0:9811'], env: { VX_ROLE: reflector } }";
    let model = vxedge_core::model::load_yaml(&workspace(run_b)).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();
    apply(&sh, &plan).unwrap();
    let runtime = std::env::temp_dir().join(format!("vxzones-run-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let nodes = plan_nodes_from_model(&model, &plan, Path::new("/")).unwrap();
    let guard = NodesGuard {
        sh: sh.clone(),
        nodes: nodes.clone(),
        runtime: runtime.clone(),
    };
    for n in &nodes {
        node_up(&sh, n, &runtime, &runtime).unwrap();
        assert_eq!(
            node_status(&sh, n, &runtime),
            NodeState::Up,
            "{}",
            n.node_id
        );
    }

    // The entrypoint is kept and `run.args` replaces the command.
    let pid_a = podman_pid("vx-node-run-a-payload").expect("payload A running");
    let pid_b = podman_pid("vx-node-run-b-payload").expect("payload B running");
    let infra_a = podman_pid("vx-node-run-a-infra").expect("infra A running");
    let infra_b = podman_pid("vx-node-run-b-infra").expect("infra B running");
    assert_eq!(
        cmdline(pid_b),
        ["/vxprobe", "reflect", "--listen", "0.0.0.0:9811"]
    );
    assert_eq!(
        cmdline(pid_a),
        ["/vxprobe", "reflect", "--listen", "0.0.0.0:9812"]
    );
    let env = podman(&[
        "inspect",
        "-f",
        "{{json .Config.Env}}",
        "vx-node-run-b-payload",
    ]);
    assert!(
        String::from_utf8_lossy(&env.stdout).contains("VX_ROLE=reflector"),
        "{}",
        String::from_utf8_lossy(&env.stdout)
    );

    // B answers on the port its args name, across the slice, and no longer on the default.
    assert!(
        probes_answered(&format!("{IP_B}:9811")) > 0,
        "B reflects on 9811"
    );
    assert_eq!(
        probes_answered(&format!("{IP_B}:9800")),
        0,
        "B's default port is unused"
    );

    // `cap_add: [NET_ADMIN]` is A's alone: A may route, B may not.
    let route = |container: &str| {
        podman(&[
            "exec",
            container,
            "busybox",
            "ip",
            "route",
            "add",
            "224.0.0.0/4",
            "dev",
            "eth0",
        ])
    };

    // Multicast between A and B, with B joined on both groups throughout.
    let mut receiver = in_netns(infra_b, &["python3", "-c", RECEIVER, IP_B])
        .args(GROUPS)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("receiver starts");
    let mut stdout = BufReader::new(receiver.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert_eq!(first.trim(), "ready", "receiver did not start");
    let routes_before = sudo(&[
        "nsenter",
        "-t",
        &infra_a.to_string(),
        "-n",
        "ip",
        "route",
        "show",
    ]);
    let plain = send(infra_a, "plain", "plain");
    let with_if = send(infra_a, "if", "if");
    let routed_a = route("vx-node-run-a-payload");
    let routed_b = route("vx-node-run-b-payload");
    let routed = send(infra_a, "plain", "routed");
    let mut received = String::new();
    stdout.read_to_string(&mut received).unwrap();
    let mut receiver_err = String::new();
    receiver
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut receiver_err)
        .unwrap();
    receiver.wait().unwrap();
    let dump = gateway_dump(&ns);
    println!(
        "routes in A before:\n{}\nno route, no IP_MULTICAST_IF:\n{plain}\nIP_MULTICAST_IF:\n\
         {with_if}\nafter A's own 224.0.0.0/4 route:\n{routed}\nB received:\n{received}\n\
         {receiver_err}\ngateway:\n{dump}",
        String::from_utf8_lossy(&routes_before.stdout)
    );

    assert!(
        routed_a.status.success(),
        "NET_ADMIN lets A route: {}",
        String::from_utf8_lossy(&routed_a.stderr)
    );
    assert!(
        !routed_b.status.success(),
        "B has no NET_ADMIN, yet added a route"
    );
    // A node's namespace has its slice's subnet and nothing else, so a sender that names no
    // interface has nowhere to send multicast.
    for g in ["239.255.0.1", "224.244.224.245"] {
        assert!(plain.contains(&format!("err {g} ENETUNREACH")), "{plain}");
        assert!(with_if.contains(&format!("sent {g}")), "{with_if}");
        assert!(routed.contains(&format!("sent {g}")), "{routed}");
        assert!(
            received.contains(&format!("got if-{g} {IP_A}")),
            "{g} with IP_MULTICAST_IF did not cross the slice:\n{received}\n{dump}"
        );
        assert!(
            received.contains(&format!("got routed-{g} {IP_A}")),
            "{g} on A's own route did not cross the slice:\n{received}\n{dump}"
        );
    }

    // A changed `run` recreates B's payload in the namespace B already has.
    let run_b2 = "{ args: [--listen, '0.0.0.0:9813'], env: { VX_ROLE: reflector } }";
    let model2 = vxedge_core::model::load_yaml(&workspace(run_b2)).unwrap();
    let nodes2 = plan_nodes_from_model(&model2, &plan, Path::new("/")).unwrap();
    node_up(&sh, node(&nodes2, "node-run-b"), &runtime, &runtime).unwrap();
    let pid_b2 = podman_pid("vx-node-run-b-payload").expect("payload B running again");
    assert_ne!(pid_b2, pid_b, "a changed run recreates the payload");
    assert_eq!(
        podman_pid("vx-node-run-b-infra"),
        Some(infra_b),
        "same namespace"
    );
    assert_eq!(
        cmdline(pid_b2),
        ["/vxprobe", "reflect", "--listen", "0.0.0.0:9813"]
    );
    assert!(
        probes_answered(&format!("{IP_B}:9813")) > 0,
        "B reflects on 9813"
    );
    // The same `run` again is a no-op.
    node_up(&sh, node(&nodes2, "node-run-b"), &runtime, &runtime).unwrap();
    assert_eq!(podman_pid("vx-node-run-b-payload"), Some(pid_b2));

    drop(guard);
    for id in ["a", "b"] {
        for member in ["infra", "payload"] {
            let name = format!("vx-node-run-{id}-{member}");
            assert_eq!(podman_pid(&name), None, "{name} removed");
        }
    }
    teardown(&sh, &plan).unwrap();
}
