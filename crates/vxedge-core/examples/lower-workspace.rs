//! Print what a §6.0 workspace lowers to, or the provisioning plan either form produces.
//!
//! Developer tooling, deliberately not a CLI verb: lowering is an internal detail an OEM
//! engineer never needs to see (`vxedge experiment lower` was removed for exactly that reason).
//! This exists for the migration equivalence work — a workspace has replaced a frozen §6.1
//! manifest only if both produce the *same plan*, and comparing YAML cannot show that, because
//! the two forms legitimately differ in ids and defaults while planning identically.
//!
//! ```text
//! cargo run -p vxedge-core --example lower-workspace -- <workspace-dir>
//! cargo run -p vxedge-core --example lower-workspace -- --plan <workspace-dir|manifest.yaml>
//! ```

use vxedge_core::gateway::GatewayPlan;
use vxedge_core::traffic::TrafficPlan;

/// Accepts a §6.0 workspace directory or a §6.1 manifest file, so the two can be compared.
fn manifest_yaml(target: &str) -> Result<String, String> {
    let path = std::path::Path::new(target);
    if path.is_dir() {
        vxedge_core::lower::lower_path_to_yaml(path).map_err(|e| e.to_string())
    } else {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (plan_only, target) = match args.as_slice() {
        [flag, t] if flag == "--plan" => (true, t.clone()),
        [t] => (false, t.clone()),
        _ => {
            eprintln!("usage: lower-workspace [--plan] <workspace-dir|manifest.yaml>");
            std::process::exit(2);
        }
    };

    let yaml = match manifest_yaml(&target) {
        Ok(y) => y,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    if !plan_only {
        print!("{yaml}");
        return;
    }

    let manifest = match vxedge_core::ExperimentManifest::from_yaml(&yaml) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let gw = GatewayPlan::from_manifest(&manifest).expect("gateway plans");
    println!("gateway: {}", gw.summary());
    // Endpoint identity is what a campaign script depends on: run-exp-001.sh addresses `vzp0`
    // and `vzp1` by name, so a migration that renumbered them would break silently.
    for ep in &gw.endpoints {
        println!(
            "endpoint {} peer={} node={} vlan={} slice={:?} kind={:?}",
            ep.name, ep.peer, ep.node, ep.vlan, ep.slice, ep.kind
        );
    }
    match TrafficPlan::from_manifest(&manifest, &gw) {
        Ok(tp) => {
            for p in &tp.profiles {
                println!("profile {} -> {:?} / {:?}", p.id, p.source, p.target);
            }
        }
        Err(e) => println!("traffic plan: {e}"),
    }
}
