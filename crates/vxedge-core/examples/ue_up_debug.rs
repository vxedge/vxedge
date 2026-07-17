//! Dev tool: bring the fiveg CN/RAN pods up (like `fiveg_up_debug`), bring
//! every slice's UE up in its own pod, and start a `vxprobe` reflector
//! inside each UE — leaving everything running. Used to spot-check the
//! routing-table-1000 fix (2026-07-17: with each UE in its own pod/netns,
//! `ip rule show`/`ip route show table 1000` inside any two UE pods should
//! be *independent* — no collision) and, since H2, the actual probe data
//! path an H2 campaign needs: a reflector running *on* the UE, reachable
//! through its PDU session, not just a bare ping into the tunnel.
//!
//! Usage: cargo run -p vxedge-core --example ue_up_debug -- <manifest.yaml> <runtime-dir> <pod-name>

use std::{env, fs, process::ExitCode};
use vxedge_core::ExperimentManifest;
use vxedge_core::fiveg::{
    FiveGLifecycleConfig, FiveGPlan, fiveg_up, pod_pid, ue_pod_name, ue_probe_up, ue_up,
};

const PROBE_PORT: u16 = 9800;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let (manifest_path, runtime_dir, pod_name) = match args.as_slice() {
        [_, m, r, p] => (m, r, p),
        _ => {
            eprintln!("usage: ue_up_debug <manifest.yaml> <runtime-dir> <pod-name>");
            return ExitCode::from(2);
        }
    };
    let yaml = fs::read_to_string(manifest_path).expect("read manifest");
    let manifest = ExperimentManifest::from_yaml(&yaml).expect("parse manifest");
    let plan = FiveGPlan::from_manifest(&manifest).expect("plan");
    let cfg = FiveGLifecycleConfig {
        pod_name: pod_name.clone(),
        ran_pod_name: format!("{pod_name}-ran"),
        core_image: "vx5g-core".to_string(),
        ran_image: "vx5g-ran".to_string(),
    };
    let runtime_path = std::path::Path::new(runtime_dir);

    match fiveg_up(&plan, runtime_path, &cfg) {
        Ok(msg) => println!("{msg}"),
        Err(e) => {
            eprintln!("fiveg_up error: {e}");
            return ExitCode::FAILURE;
        }
    }

    let ran_pid = match pod_pid(&cfg.ran_pod_name) {
        Ok(pid) => pid,
        Err(e) => {
            eprintln!("pod_pid error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cfg_dir = runtime_path.join("fiveg-config");

    for (i, session) in plan.sessions.iter().enumerate() {
        match ue_up(&cfg, &cfg_dir, ran_pid, i, session) {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("ue_up error for `{}`: {e}", session.slice_id);
                return ExitCode::FAILURE;
            }
        }
        let container = format!("{}-ue", ue_pod_name(&cfg.ran_pod_name, &session.slice_id));
        println!(
            "  inspect: podman exec {container} ip rule show; podman exec {container} ip route show table 1000"
        );

        match ue_probe_up(&cfg, &cfg_dir, session, PROBE_PORT) {
            Ok(msg) => println!("  {msg}"),
            Err(e) => {
                eprintln!("ue_probe_up error for `{}`: {e}", session.slice_id);
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}
