//! Privileged integration test: the zone `qemu-mcu` backend booting microcontroller firmware on
//! a real gateway tap, inside a throwaway network namespace. The proof is the firmware's own
//! reflector answering UDP across the bridge, at the address it read through semihosting.
//!
//! Needs root (the namespace and QEMU's `ip netns exec`), `socat`, and `qemu-system-arm` on
//! root's PATH. `VXEDGE_TEST_FIRMWARE` names a `qemu-mcu` bundle directory (`firmware.elf` and a
//! `meta.json` naming `mps2-an385`). Run with:
//!   cargo test -p vxedge-core --test zones_qemu_mcu --no-run
//!   sudo env "PATH=$PATH" VXEDGE_TEST_FIRMWARE=/path/to/bundle <test-binary> --ignored --nocapture

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vxedge_core::gateway::{GatewayPlan, Shell, apply, teardown};
use vxedge_core::zones::backend::NodeState;
use vxedge_core::zones::{
    Acceleration, GuestArch, guest_platform, node_down, node_status, node_up, plan_nodes_from_model,
};

const GUEST_IP: &str = "10.101.0.2";
const FAR_SIDE_CIDR: &str = "10.101.0.1/24";
const REFLECTOR_PORT: u16 = 9800;

fn workspace(bundle: &str) -> String {
    format!(
        r#"
kind: experiment
id: exp-mcu
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
  ecu-m3: {{ role: ecu, zone: front-left, backend: qemu-mcu }}
functions:
  fn-m3: {{ deployed_on: ecu-m3 }}
links:
  - {{ id: l-m3, endpoints: [{{ node: ecu-m3, port: eth0, ip: {GUEST_IP}/24 }}], slice: adas }}
---
kind: baseline
id: bl
parts:
  - {{ ref: {bundle}, kind: bundle, target_ids: fn-m3 }}
"#
    )
}

/// The QEMU this test's runtime directory names in its pidfile, if it is still running.
fn guest(runtime: &Path) -> Option<i32> {
    std::fs::read_dir(runtime)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "pid"))
        .find_map(|e| std::fs::read_to_string(e.path()).ok()?.trim().parse().ok())
        .filter(|pid: &i32| Path::new(&format!("/proc/{pid}")).exists())
}

/// Removes the guest, the namespace and the runtime directory even if an assertion panics.
struct Guard {
    ns: String,
    runtime: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(pid) = guest(&self.runtime) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
        let _ = Command::new("ip").args(["netns", "del", &self.ns]).output();
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

/// One UDP round trip to the firmware's reflector, from inside the gateway namespace.
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
        if reflected(ns, "vxedge-mcu") {
            return;
        }
    }
    let log = std::fs::read_to_string(serial).unwrap_or_default();
    panic!("the firmware's reflector never answered; serial log:\n{log}");
}

#[test]
#[ignore = "needs root, socat, qemu-system-arm and VXEDGE_TEST_FIRMWARE (see module docs)"]
fn firmware_boots_on_the_gateway_tap_reads_its_address_and_answers() {
    let bundle =
        std::env::var("VXEDGE_TEST_FIRMWARE").expect("VXEDGE_TEST_FIRMWARE names a bundle");
    let ns = format!("vxmcutest{}", std::process::id());
    let created = Command::new("ip")
        .args(["netns", "add", &ns])
        .output()
        .expect("iproute2 installed");
    assert!(
        created.status.success(),
        "cannot create netns (run as root): {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let runtime = std::env::temp_dir().join(format!("vxmcu{}", std::process::id()));
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
    let log = std::fs::read_to_string(&serial).unwrap();
    assert!(
        log.contains(&format!("vx.ip={GUEST_IP}/24")) && log.contains("vx.node=ecu-m3"),
        "the firmware did not read its command line through semihosting:\n{log}"
    );
    assert!(
        log.contains(&node.guest_mac()),
        "the firmware did not take the node's MAC from its NIC:\n{log}"
    );
    let platform = guest_platform(node, &runtime).expect("a running guest has a platform");
    assert_eq!(platform.arch, GuestArch::Armv7m);
    assert_eq!(platform.acceleration, Acceleration::Tcg);

    // --- idempotent up --------------------------------------------------------
    let first = guest(&runtime).expect("the guest is running");
    node_up(&sh, node, &runtime, &runtime).unwrap();
    assert_eq!(
        guest(&runtime),
        Some(first),
        "a second `up` started another QEMU"
    );

    // --- down, twice, then a fresh boot -----------------------------------------
    node_down(&sh, node, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Down);
    assert!(
        !Path::new(&format!("/proc/{first}")).exists(),
        "`down` left QEMU running"
    );
    node_down(&sh, node, &runtime).unwrap();
    node_up(&sh, node, &runtime, &runtime).unwrap();
    wait_for_reflector(&ns, &serial);
    node_down(&sh, node, &runtime).unwrap();

    teardown(&sh, &plan).unwrap();
}
