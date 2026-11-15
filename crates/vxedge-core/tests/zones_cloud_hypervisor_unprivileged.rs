//! Privileged integration test: the zone `cloud-hypervisor` backend as a real deployment runs it.
//! The gateway is in the host namespace and the VMM runs as this process's own user, so the guest
//! gets its network through a tap this user owns.
//!
//! **Do not run under `sudo`**: the point is an unprivileged VMM, and the test refuses root.
//! Privilege comes from file capabilities on the test binary for netlink, plus a cached sudo
//! credential for the `ip`, `tc` and `nft` calls. It needs `/dev/kvm`, `socat`,
//! `cloud-hypervisor` on PATH, no gateway already on this host, and `VXEDGE_TEST_BUNDLE` naming a
//! built bundle directory. Run with:
//!   sudo -v
//!   BIN=$(cargo test -p vxedge-core --test zones_cloud_hypervisor_unprivileged --no-run \
//!     --message-format=json \
//!     | jq -r 'select(.target.name=="zones_cloud_hypervisor_unprivileged" and .executable) | .executable')
//!   sudo setcap cap_net_admin,cap_sys_admin+ep "$BIN"   # again after every rebuild
//!   VXEDGE_TEST_BUNDLE=/path/to/bundle "$BIN" --ignored --nocapture

#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vxedge_core::gateway::{GatewayPlan, Shell, apply, teardown};
use vxedge_core::zones::backend::NodeState;
use vxedge_core::zones::{node_down, node_status, node_up, plan_nodes_from_model};

// Unlikely to meet a subnet already on a host, since this test shares the host namespace.
const GUEST_IP: &str = "10.250.101.2";
const FAR_SIDE_CIDR: &str = "10.250.101.1/24";
const REFLECTOR_PORT: u16 = 9800;

fn workspace(bundle: &str) -> String {
    format!(
        r#"
kind: experiment
id: exp-ch-host
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
  node-chu-01: {{ role: ecu, zone: front-left, backend: cloud-hypervisor }}
functions:
  fn-ch: {{ deployed_on: node-chu-01 }}
links:
  - {{ id: l-ch, endpoints: [{{ node: node-chu-01, port: eth0, ip: {GUEST_IP}/24 }}], slice: adas }}
---
kind: baseline
id: bl
parts:
  - {{ ref: {bundle}, kind: bundle, target_ids: fn-ch }}
"#
    )
}

/// Stops the guest and removes the gateway even if an assertion panics.
struct Guard {
    sh: Shell,
    plan: GatewayPlan,
    runtime: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        for pid in vmm_pids(&self.runtime) {
            let _ = Command::new("kill").args(["-9", &pid]).output();
        }
        let _ = teardown(&self.sh, &self.plan);
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

fn vmm_pids(runtime: &Path) -> Vec<String> {
    let out = Command::new("pgrep")
        .args(["-f", &runtime.display().to_string()])
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

/// One UDP round trip to the guest's reflector.
fn reflected(payload: &str) -> bool {
    let target = format!("UDP:{GUEST_IP}:{REFLECTOR_PORT}");
    let mut child = Command::new("socat")
        .args(["-t1", "-", &target])
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

#[test]
#[ignore = "needs file capabilities, cached sudo, /dev/kvm, socat, cloud-hypervisor and VXEDGE_TEST_BUNDLE (see module docs)"]
fn an_unprivileged_vmm_carries_traffic_on_a_tap_its_user_owns() {
    assert_ne!(
        own_uid(),
        0,
        "this test proves an unprivileged VMM: not as root"
    );
    assert!(
        !Path::new("/sys/class/net/vxbr0").exists(),
        "a gateway is already on this host; bring it down first"
    );
    let bundle = std::env::var("VXEDGE_TEST_BUNDLE").expect("VXEDGE_TEST_BUNDLE names a bundle");
    // Short on purpose: this path, plus the socket name, has to fit a Unix socket address.
    let runtime = std::env::temp_dir().join(format!("vxchu{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();

    // As `vxedged` builds its Shell: the host namespace, escalating what needs it.
    let sh = Shell {
        netns: None,
        escalate: true,
    };
    let model = vxedge_core::model::load_yaml(&workspace(&bundle)).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model, vxedge_core::instance::Instance::FIRST).unwrap();
    let _guard = Guard {
        sh: sh.clone(),
        plan: plan.clone(),
        runtime: runtime.clone(),
    };
    apply(&sh, &plan).unwrap();
    let nodes = plan_nodes_from_model(&model, &plan, Path::new("/")).unwrap();
    let node = &nodes[0];

    // The far side, on the guest's slice and subnet.
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

    node_up(&sh, node, &runtime, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Up);

    // The VMM and everything it made belong to this user, and so does the tap it opened.
    let uid = own_uid();
    let pids = vmm_pids(&runtime);
    assert_eq!(pids.len(), 1, "one VMM: {pids:?}");
    let vmm_uid = std::fs::metadata(format!("/proc/{}", pids[0]))
        .unwrap()
        .uid();
    assert_eq!(vmm_uid, uid, "the VMM runs as this user");
    let socket = runtime.join(format!("{}.ch.sock", node.runtime_name()));
    assert_eq!(std::fs::metadata(&socket).unwrap().uid(), uid);
    let tap_owner = std::fs::read_to_string(format!("/sys/class/net/{}/owner", node.endpoint))
        .expect("the endpoint is a tap");
    assert_eq!(tap_owner.trim(), uid.to_string(), "the tap is this user's");

    let serial = runtime.join(format!("{}.serial.log", node.runtime_name()));
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut answered = false;
    while !answered && Instant::now() < deadline {
        answered = reflected("vxedge-unprivileged");
    }
    let log = std::fs::read_to_string(&serial).unwrap_or_default();
    assert!(answered, "the reflector never answered; serial:\n{log}");

    node_down(&sh, node, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Down);
    assert!(vmm_pids(&runtime).is_empty(), "`down` left a VMM running");
}
