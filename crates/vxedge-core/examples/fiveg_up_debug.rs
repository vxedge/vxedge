//! Dev tool: bring the fiveg lifecycle pod up from a manifest and leave it
//! running (unlike the integration test, which tears down). Used to
//! spot-check the Rust-driven pod against a live gNB/UE the same way the
//! shell `pod-up.sh` reference was validated.
//!
//! Usage: cargo run -p vxedge-core --example fiveg_up_debug -- <manifest.yaml> <runtime-dir> <pod-name>

use std::{env, fs, process::ExitCode};
use vxedge_core::ExperimentManifest;
use vxedge_core::fiveg::{FiveGLifecycleConfig, FiveGPlan, fiveg_up};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let (manifest_path, runtime_dir, pod_name) = match args.as_slice() {
        [_, m, r, p] => (m, r, p),
        _ => {
            eprintln!("usage: fiveg_up_debug <manifest.yaml> <runtime-dir> <pod-name>");
            return ExitCode::from(2);
        }
    };
    let yaml = fs::read_to_string(manifest_path).expect("read manifest");
    let manifest = ExperimentManifest::from_yaml(&yaml).expect("parse manifest");
    let plan = FiveGPlan::from_model(&manifest.to_model().expect("adapts")).expect("plan");
    let cfg = FiveGLifecycleConfig {
        pod_name: pod_name.clone(),
        ran_pod_name: format!("{pod_name}-ran"),
        core_image: "vx5g-core".to_string(),
        ran_image: "vx5g-ran".to_string(),
    };
    match fiveg_up(&plan, std::path::Path::new(runtime_dir), &cfg) {
        Ok(msg) => {
            println!("{msg}");
            println!("config dir: {runtime_dir}/fiveg-config");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
