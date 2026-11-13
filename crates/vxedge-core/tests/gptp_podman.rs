//! Privileged integration test: `network.time_sync` (`kind: gptp`, `ARCHITECTURE.md` §6.0.3) on
//! a real gateway. The gateway runs in a throwaway network namespace, as in `gateway_netns.rs`;
//! two container devices, whose payload is the pause image, follow its clock.
//!
//! What it shows:
//! - the grandmaster runs from the linuxptp image's hold container, as root, in the gateway's
//!   namespace, with one master port per follower's access port, and goes when its container
//!   does;
//! - each follower's `ptp4l`, a member of its device's container group, follows it, with an
//!   |offset| under 10 µs;
//! - the exporter's `vxedge_gptp_*` text serves both followers, and a follower that stops
//!   answering leaves a gap.
//!
//! The linuxptp image is [`vxedge_core::gptp::LINUXPTP_IMAGE`], pinned by digest. Pull it first:
//!   podman pull ghcr.io/vxedge/parts/agent/time/app/linuxptp:0.1.0@sha256:cc81350e76480ee37237abcc78f42871d97c71ed1cd35e431a02924a521cc94b
//!
//! **Do not run under `sudo`**, for the reason `zones_podman.rs` gives: Podman must stay this
//! user's. Privilege comes from file capabilities on the test binary, plus a sudo credential
//! cached in the terminal the binary runs from, for the throwaway namespace and the
//! grandmaster's `sudo -n nsenter`. Run with:
//!   sudo -v
//!   BIN=$(cargo test -p vxedge-core --test gptp_podman --no-run --message-format=json \
//!     | jq -r 'select(.target.name=="gptp_podman" and .executable) | .executable')
//!   sudo setcap cap_net_admin,cap_sys_admin+ep "$BIN"   # again after every rebuild
//!   "$BIN" --ignored --nocapture

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vxedge_core::containers::{ContainerClient, PAUSE_IMAGE};
use vxedge_core::gateway::{GatewayPlan, Shell, apply, observe, teardown};
use vxedge_core::gptp;
use vxedge_core::zones::backend::NodeState;
use vxedge_core::zones::{member_down, member_up, node_down, node_status, node_up};

const FOLLOWERS: [&str; 2] = ["tsecu1", "tshpc1"];

fn workspace() -> String {
    let (reference, digest) = PAUSE_IMAGE.split_once('@').expect("pause image is pinned");
    format!(
        r#"
kind: experiment
id: exp-gptp
vehicles: [veh]
baseline: bl
network:
  slices:
    - {{ id: adas, mode: vlan, vlan: 101 }}
  time_sync:
    kind: gptp
    followers: [tsecu1, tshpc1]
---
kind: vehicle
id: veh
zones: {{ front: {{}}, central: {{}} }}
devices:
  tsecu1: {{ role: zone-controller, zone: front, backend: container }}
  tshpc1: {{ role: hpc, zone: central, backend: container }}
functions:
  f-ecu: {{ deployed_on: tsecu1 }}
  f-hpc: {{ deployed_on: tshpc1 }}
links:
  - {{ id: l-ecu, endpoints: [{{ node: tsecu1, port: eth0, ip: 10.101.0.41/24 }}], slice: adas }}
  - {{ id: l-hpc, endpoints: [{{ node: tshpc1, port: eth0, ip: 10.101.0.42/24 }}], slice: adas }}
---
kind: baseline
id: bl
parts:
  - {{ ref: '{reference}', kind: container, digest: '{digest}', target_ids: [f-ecu, f-hpc] }}
"#
    )
}

fn sudo(args: &[&str]) -> std::process::Output {
    std::process::Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .expect("sudo runs")
}

/// Removes everything this test made, even if an assertion panics: the containers by name, the
/// throwaway namespace (which takes the bridge and its ports with it) and the runtime directory.
struct Cleanup {
    ns: String,
    runtime: PathBuf,
    bridge: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let mut names = vec![format!(
            "{}-infra",
            gptp::grandmaster_group_name(&self.bridge)
        )];
        for f in FOLLOWERS {
            let group = vxedge_core::zones::runtime_name_of(f);
            names.extend([
                format!("{group}-{}", gptp::FOLLOWER_MEMBER),
                format!("{group}-payload"),
                format!("{group}-infra"),
            ]);
        }
        let _ = std::process::Command::new("podman")
            .args(["rm", "-f", "-t", "2"])
            .args(&names)
            .output();
        let _ = sudo(&["ip", "netns", "del", &self.ns]);
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

/// Every `ptp4l` on the host whose command line names `runtime`, with its owner's uid.
fn ptp4l_of(runtime: &Path) -> Vec<(u32, u32)> {
    let needle = runtime.display().to_string();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if !cmdline.starts_with("ptp4l ") || !cmdline.contains(&needle) {
            continue;
        }
        let uid = std::fs::read_to_string(entry.path().join("status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("Uid:"))
                    .and_then(|v| v.split_whitespace().next()?.parse().ok())
            })
            .unwrap_or(u32::MAX);
        found.push((pid, uid));
    }
    found
}

/// The value of `family{entity="<entity>"}` in exporter text.
fn sample(text: &str, family: &str, entity: &str) -> Option<f64> {
    let prefix = format!("{family}{{entity=\"{entity}\"}} ");
    text.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .and_then(|v| v.trim().parse().ok())
}

#[test]
#[ignore = "needs the user Podman socket, a cached sudo credential, file capabilities and the \
            linuxptp image; run unprivileged (see module docs)"]
fn followers_sync_to_the_gateway_grandmaster_and_the_exporter_serves_them() {
    let ns = format!("vxgptp{}", std::process::id());
    let created = sudo(&["ip", "netns", "add", &ns]);
    assert!(
        created.status.success(),
        "cannot create netns (cache a credential with `sudo -v` first): {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let runtime = PathBuf::from(format!("/tmp/vxgptp-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let sh = Shell {
        netns: Some(ns.clone()),
        escalate: true,
    };
    let model = vxedge_core::model::load_yaml(&workspace()).expect("workspace loads");
    let plan = GatewayPlan::from_model(&model).unwrap();
    let _cleanup = Cleanup {
        ns: ns.clone(),
        runtime: runtime.clone(),
        bridge: plan.bridge.clone(),
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let client = rt
        .block_on(ContainerClient::connect_user())
        .expect("the user's Podman socket answers");
    assert!(
        rt.block_on(client.image_present(gptp::LINUXPTP_IMAGE))
            .unwrap(),
        "{} is not on this host: pull it first (see module docs)",
        gptp::LINUXPTP_IMAGE
    );

    apply(&sh, &plan).unwrap();
    assert!(observe(&sh, &plan).unwrap().fully_up());
    let ts = plan.time_sync.as_ref().expect("planned");
    println!("plan: {}", plan.summary());

    // The gateway stage starts the grandmaster before any node is up, as `up` does: its ports
    // exist, their peers are not attached yet.
    rt.block_on(gptp::grandmaster_up(
        &client,
        &sh,
        &plan,
        &runtime,
        gptp::LINUXPTP_IMAGE,
    ))
    .expect("grandmaster starts");
    assert!(gptp::grandmaster_running(&runtime, &plan.bridge));
    let roots: Vec<_> = ptp4l_of(&runtime)
        .into_iter()
        .filter(|(_, uid)| *uid == 0)
        .collect();
    assert_eq!(roots.len(), 1, "one root ptp4l: {roots:?}");
    // A second `up` with the same plan keeps it.
    rt.block_on(gptp::grandmaster_up(
        &client,
        &sh,
        &plan,
        &runtime,
        gptp::LINUXPTP_IMAGE,
    ))
    .unwrap();
    assert_eq!(
        ptp4l_of(&runtime)
            .into_iter()
            .filter(|(_, uid)| *uid == 0)
            .collect::<Vec<_>>(),
        roots,
        "idempotent"
    );
    // Its master ports are the followers' access ports, in the gateway's namespace.
    let cmd = std::fs::read_to_string(
        gptp::grandmaster_dir(&runtime, &plan.bridge).join("grandmaster.cmd"),
    )
    .unwrap();
    for port in ts.master_ports() {
        assert!(cmd.contains(&format!(" {port}")), "{cmd}");
    }
    assert!(cmd.contains(&format!("--net=/run/netns/{ns}")), "{cmd}");

    let nodes = vxedge_core::zones::plan_nodes_from_model(&model, &plan, Path::new("/")).unwrap();
    let mut followers = Vec::new();
    for device in FOLLOWERS {
        let node = nodes.iter().find(|n| n.node_id == device).unwrap().clone();
        node_up(&sh, &node, &runtime, &runtime).unwrap();
        assert_eq!(node_status(&sh, &node, &runtime), NodeState::Up);
        let spec = gptp::follower_spec(gptp::LINUXPTP_IMAGE, &runtime, device);
        gptp::prepare_dir(&gptp::follower_dir(&runtime, device)).unwrap();
        member_up(&sh, &node, &runtime, &spec).unwrap();
        followers.push((node, spec));
    }

    // Election takes a few announce intervals; then each follower answers with a grandmaster.
    let deadline = Instant::now() + Duration::from_secs(60);
    while FOLLOWERS
        .iter()
        .any(|d| gptp::read_follower(&gptp::follower_dir(&runtime, d)).is_none())
    {
        assert!(
            Instant::now() < deadline,
            "followers did not follow within 60 s; grandmaster log:\n{}",
            std::fs::read_to_string(gptp::grandmaster_log(&runtime, &plan.bridge))
                .unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    // 20 readings of each, a second apart.
    let mut offsets: Vec<Vec<f64>> = vec![Vec::new(); FOLLOWERS.len()];
    let mut delays: Vec<Vec<f64>> = vec![Vec::new(); FOLLOWERS.len()];
    for _ in 0..20 {
        for (i, d) in FOLLOWERS.iter().enumerate() {
            let r = gptp::read_follower(&gptp::follower_dir(&runtime, d))
                .unwrap_or_else(|| panic!("follower {d} stopped answering"));
            offsets[i].push(r.offset_s);
            delays[i].push(r.path_delay_s);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    for (i, d) in FOLLOWERS.iter().enumerate() {
        let abs: Vec<f64> = offsets[i].iter().map(|o| o.abs()).collect();
        let mean_abs = abs.iter().sum::<f64>() / abs.len() as f64;
        let max_abs = abs.iter().cloned().fold(0.0, f64::max);
        let mean_delay = delays[i].iter().sum::<f64>() / delays[i].len() as f64;
        let (min_d, max_d) = delays[i]
            .iter()
            .fold((f64::MAX, 0.0f64), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
        println!(
            "follower {d}: mean |offset| {:.3} us, max |offset| {:.3} us, peer delay mean {:.0} ns \
             (min {:.0}, max {:.0})",
            mean_abs * 1e6,
            max_abs * 1e6,
            mean_delay * 1e9,
            min_d * 1e9,
            max_d * 1e9
        );
        assert!(max_abs < 10e-6, "follower {d}: |offset| {max_abs} s");
        assert!(mean_delay > 0.0 && mean_delay < 100e-6, "{d}: {mean_delay}");
    }

    // The exporter's text: both followers, entity = the follower.
    let text = gptp::metrics(&model, &runtime);
    println!("{text}");
    for d in FOLLOWERS {
        let entity = vxedge_core::entities::node_path(&model, d).unwrap();
        let offset = sample(&text, "vxedge_gptp_offset_seconds", &entity)
            .unwrap_or_else(|| panic!("no offset for {entity}:\n{text}"));
        let delay = sample(&text, "vxedge_gptp_path_delay_seconds", &entity)
            .unwrap_or_else(|| panic!("no delay for {entity}:\n{text}"));
        assert!(offset.abs() < 10e-6, "{entity}: {offset}");
        assert!(delay > 0.0, "{entity}: {delay}");
    }

    // A follower that stops answering leaves a gap; the other is still served.
    let (node, spec) = &followers[0];
    member_down(node, spec).unwrap();
    let text = gptp::metrics(&model, &runtime);
    let gone = vxedge_core::entities::node_path(&model, FOLLOWERS[0]).unwrap();
    let kept = vxedge_core::entities::node_path(&model, FOLLOWERS[1]).unwrap();
    assert!(
        sample(&text, "vxedge_gptp_offset_seconds", &gone).is_none(),
        "{text}"
    );
    assert!(
        sample(&text, "vxedge_gptp_offset_seconds", &kept).is_some(),
        "{text}"
    );

    // Down: removing the hold container ends the root ptp4l; nothing of it is left.
    rt.block_on(gptp::grandmaster_down(&client, &plan.bridge, &runtime))
        .unwrap();
    assert!(!gptp::grandmaster_running(&runtime, &plan.bridge));
    let gm_dir = gptp::grandmaster_dir(&runtime, &plan.bridge);
    assert!(
        !gptp::socket(&gm_dir).exists(),
        "the grandmaster's socket is removed"
    );
    assert!(
        ptp4l_of(&runtime).iter().all(|(_, uid)| *uid != 0),
        "a root ptp4l outlived its container: {:?}",
        ptp4l_of(&runtime)
    );
    rt.block_on(gptp::grandmaster_down(&client, &plan.bridge, &runtime))
        .expect("down twice");
    // With no grandmaster, the remaining follower soon has none to follow: a gap too.
    let deadline = Instant::now() + Duration::from_secs(30);
    while sample(
        &gptp::metrics(&model, &runtime),
        "vxedge_gptp_offset_seconds",
        &kept,
    )
    .is_some()
    {
        assert!(
            Instant::now() < deadline,
            "follower still reports a grandmaster 30 s after it went"
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    for (node, spec) in &followers {
        member_down(node, spec).unwrap();
        node_down(&sh, node, &runtime).unwrap();
    }
    assert!(ptp4l_of(&runtime).is_empty(), "{:?}", ptp4l_of(&runtime));
    teardown(&sh, &plan).unwrap();
}
