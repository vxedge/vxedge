//! Dev tool: bring the fiveg lifecycle pod up from a manifest and leave it
//! running (unlike the integration test, which tears down). Used to
//! spot-check the Rust-driven pod against a live gNB/UE the same way the
//! shell `pod-up.sh` reference was validated.
//!
//! Usage: cargo run -p vxedge-core --example fiveg_up_debug -- <manifest.yaml> <runtime-dir> <pod-name>

use std::{env, fs, path::Path, process::ExitCode};
use vxedge_core::containers::ContainerClient;
use vxedge_core::fiveg::{FiveGLifecycleConfig, FiveGPlan, fiveg_up};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let (manifest_path, runtime_dir, pod_name) = match args.as_slice() {
        [_, m, r, p] => (m, r, p),
        _ => {
            eprintln!("usage: fiveg_up_debug <manifest.yaml> <runtime-dir> <pod-name>");
            return ExitCode::from(2);
        }
    };
    let model = load_workspace(manifest_path).expect("load workspace");
    let plan = FiveGPlan::from_model(&model).expect("plan");
    let cfg = FiveGLifecycleConfig {
        pod_name: pod_name.clone(),
        ran_pod_name: format!("{pod_name}-ran"),
        core_image: "vx5g-core".to_string(),
        ran_image: "vx5g-ran".to_string(),
        ..FiveGLifecycleConfig::default()
    };
    let client = match ContainerClient::connect_user().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match fiveg_up(&client, &plan, std::path::Path::new(runtime_dir), &cfg).await {
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

/// A §6.0 workspace, given as a directory or as a single file of `---`-separated documents.
fn load_workspace(
    path: &str,
) -> Result<vxedge_core::model::Experiment, vxedge_core::model::LoadError> {
    let path = Path::new(path);
    if path.is_dir() {
        vxedge_core::model::load_dir(path)
    } else {
        vxedge_core::model::load_yaml(&fs::read_to_string(path).expect("read workspace"))
    }
}
