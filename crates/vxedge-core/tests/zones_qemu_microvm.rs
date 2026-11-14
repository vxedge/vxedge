//! Privileged integration test: the zone `qemu-microvm` backend booting a probe guest bundle on a
//! real gateway tap, inside a throwaway network namespace. The proof is the guest's own reflector
//! answering UDP :9800 at the address the manifest pins, which is what a bundle declaring
//! `org.vxedge.reflector: udp/9800` promises.
//!
//! Needs root (the namespace and QEMU's `ip netns exec`), `socat`, and the guest's emulator on
//! root's PATH (`qemu-system-x86_64` for an x86_64 or i386 bundle, `qemu-system-aarch64` for an
//! aarch64 one). `VXEDGE_TEST_BUNDLE` names a bundle directory (`vmlinuz`, `initrd.img`,
//! `meta.json`), for example a pulled `zone/*/stack/probe-bundle`. `VXEDGE_TEST_QEMU`, optional,
//! gives the device a `qemu:` block as `<machine>,<transport>[,vhost]` (`q35,pci,vhost`); a PCI
//! transport needs a bundle that declares `org.vxedge.virtio-pci`. Run with:
//!   cargo test -p vxedge-core --test zones_qemu_microvm --no-run
//!   sudo env "PATH=$PATH" VXEDGE_TEST_BUNDLE=/path/to/bundle [VXEDGE_TEST_QEMU=q35,pci] \
//!     <test-binary> --ignored --nocapture

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

/// The `qemu:` block `VXEDGE_TEST_QEMU` asks for, and the `-M` and NIC QEMU must then run.
struct Arm {
    block: String,
    machine: &'static str,
    nic: &'static str,
    vhost: bool,
}

fn arm() -> Arm {
    let spec = std::env::var("VXEDGE_TEST_QEMU").unwrap_or_default();
    let parts: Vec<&str> = spec.split(',').filter(|s| !s.is_empty()).collect();
    let (machine, transport) = match parts.as_slice() {
        [] => ("microvm", "mmio"),
        [m, t, ..] => (*m, *t),
        _ => panic!("VXEDGE_TEST_QEMU is <machine>,<transport>[,vhost]: {spec}"),
    };
    let vhost = parts.get(2) == Some(&"vhost");
    Arm {
        block: if parts.is_empty() {
            String::new()
        } else {
            format!(", qemu: {{ machine: {machine}, transport: {transport}, vhost: {vhost} }}")
        },
        machine: match (machine, transport) {
            ("q35", _) => "q35",
            ("microvm", "pci") => "microvm,pcie=on,acpi=on",
            _ => "microvm,acpi=off",
        },
        nic: if transport == "pci" {
            "virtio-net-pci"
        } else {
            "virtio-net-device"
        },
        vhost,
    }
}

fn workspace(bundle: &str, qemu: &str) -> String {
    format!(
        r#"
kind: experiment
id: exp-microvm
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
  ecu-vm: {{ role: ecu, zone: front-left, backend: qemu-microvm{qemu} }}
functions:
  fn-vm: {{ deployed_on: ecu-vm }}
links:
  - {{ id: l-vm, endpoints: [{{ node: ecu-vm, port: eth0, ip: {GUEST_IP}/24 }}], slice: adas }}
---
kind: baseline
id: bl
parts:
  - {{ ref: {bundle}, kind: bundle, target_ids: fn-vm }}
"#
    )
}

/// The instruction set the bundle's `meta.json` names, as the backend reads it.
fn bundle_arch(bundle: &str) -> GuestArch {
    let meta = std::fs::read_to_string(Path::new(bundle).join("meta.json")).expect("meta.json");
    let meta: serde_json::Value = serde_json::from_str(&meta).expect("meta.json is JSON");
    let arch = meta
        .get("arch")
        .or_else(|| meta.get("org.vxedge.arch"))
        .and_then(|a| a.as_str())
        .unwrap_or("x86_64");
    match arch {
        "aarch64" | "arm64" => GuestArch::Aarch64,
        _ => GuestArch::X86_64,
    }
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

/// One UDP round trip to the guest's reflector, from inside the gateway namespace.
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

fn wait_for_reflector(ns: &str, serial: &Path, within: Duration) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if reflected(ns, "vxedge-microvm") {
            return;
        }
    }
    let log = std::fs::read_to_string(serial).unwrap_or_default();
    let tail: Vec<&str> = log.lines().rev().take(15).collect();
    panic!(
        "the guest's reflector never answered on :{REFLECTOR_PORT}; serial tail, newest first:\n{tail:#?}"
    );
}

#[test]
#[ignore = "needs root, socat, the guest's qemu-system-* and VXEDGE_TEST_BUNDLE (see module docs)"]
fn a_probe_guest_boots_on_the_gateway_tap_and_its_reflector_answers_at_the_pinned_ip() {
    let bundle = std::env::var("VXEDGE_TEST_BUNDLE").expect("VXEDGE_TEST_BUNDLE names a bundle");
    let arch = bundle_arch(&bundle);
    let arm = arm();
    let ns = format!("vxmvmtest{}", std::process::id());
    let created = Command::new("ip")
        .args(["netns", "add", &ns])
        .output()
        .expect("iproute2 installed");
    assert!(
        created.status.success(),
        "cannot create netns (run as root): {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let runtime = std::env::temp_dir().join(format!("vxmvm{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let _guard = Guard {
        ns: ns.clone(),
        runtime: runtime.clone(),
    };
    let sh = Shell {
        netns: Some(ns.clone()),
        escalate: false,
    };

    let model =
        vxedge_core::model::load_yaml(&workspace(&bundle, &arm.block)).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model, vxedge_core::instance::Instance::FIRST).unwrap();
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
    let platform = guest_platform(node, &runtime).expect("a running guest has a platform");
    assert_eq!(
        platform.arch, arch,
        "the guest runs another instruction set than its bundle names"
    );
    // QEMU runs the machine, NIC and data path the device's `qemu:` block names.
    let pid = guest(&runtime).expect("the guest is running");
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    let argv: Vec<String> = String::from_utf8_lossy(&cmdline)
        .split('\0')
        .map(str::to_string)
        .collect();
    let after = |flag: &str| {
        let i = argv.iter().position(|a| a == flag).expect(flag);
        argv[i + 1].clone()
    };
    assert_eq!(after("-M"), arm.machine);
    assert!(after("-device").starts_with(arm.nic), "{argv:?}");
    assert_eq!(
        after("-netdev").ends_with(",vhost=on"),
        arm.vhost,
        "{argv:?}"
    );
    // An emulated guest boots slower; it is functional evidence only, never timed.
    let within = match platform.acceleration {
        Acceleration::Kvm => Duration::from_secs(30),
        Acceleration::Tcg => Duration::from_secs(120),
    };
    wait_for_reflector(&ns, &serial, within);

    // --- idempotent up --------------------------------------------------------
    let first = guest(&runtime).expect("the guest is running");
    node_up(&sh, node, &runtime, &runtime).unwrap();
    assert_eq!(
        guest(&runtime),
        Some(first),
        "a second `up` started another QEMU"
    );

    // --- down, twice, then a fresh boot answers again -----------------------------
    node_down(&sh, node, &runtime).unwrap();
    assert_eq!(node_status(&sh, node, &runtime), NodeState::Down);
    assert!(
        !Path::new(&format!("/proc/{first}")).exists(),
        "`down` left QEMU running"
    );
    assert!(
        !reflected(&ns, "vxedge-down"),
        "a guest that is down still answered"
    );
    node_down(&sh, node, &runtime).unwrap();
    node_up(&sh, node, &runtime, &runtime).unwrap();
    wait_for_reflector(&ns, &serial, within);
    node_down(&sh, node, &runtime).unwrap();

    teardown(&sh, &plan).unwrap();
}
