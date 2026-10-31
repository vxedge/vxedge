//! Dev tool: attach an already-running CN pod (see `fiveg_up_debug`) to a
//! bridge. Used to spot-check `bridge_up` against a real bridge, mirroring
//! how `fiveg_up_debug` was used to spot-check the lifecycle port and the
//! CN/RAN split.
//!
//! Usage: cargo run -p vxedge-core --example bridge_up_debug -- <pod-name> <bridge-name>
//!
//! The bridge must already exist (`sudo ip link add <bridge-name> type
//! bridge && sudo ip link set <bridge-name> up`) — this tool does not
//! create it, deliberately: whether to create a throwaway test bridge or
//! attach to `vxedge up`'s real `vxbr0` is the caller's call, not this
//! tool's.

use std::{env, process::ExitCode};
use vxedge_core::containers::ContainerClient;
use vxedge_core::fiveg::{FiveGLifecycleConfig, bridge_up, pod_pid};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let (pod_name, bridge) = match args.as_slice() {
        [_, p, b] => (p, b),
        _ => {
            eprintln!("usage: bridge_up_debug <pod-name> <bridge-name>");
            return ExitCode::from(2);
        }
    };
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
    let cn_pid = match pod_pid(&client, pod_name).await {
        Ok(pid) => pid,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match bridge_up(&cfg, bridge, cn_pid).await {
        Ok(()) => {
            println!("CN pod `{pod_name}` attached to bridge `{bridge}`");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
