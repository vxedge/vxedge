//! Dev tool: render a manifest's VLAN slice intent to SONiC `config_db.json`, for validating
//! it against a live SONiC-VS instance (`vxparts/parts/gw/nos/os/sonic-vs`,
//! `docs/ROADMAP.md` #16). Trunk port is a required argument, not a default, for the same
//! reason `SonicPlan` itself takes one: this codebase does not assume SONiC-VS's own virtual
//! port naming without it being supplied explicitly.
//!
//! Usage: cargo run -p vxedge-core --example render_sonic -- <manifest.yaml> <trunk-port>

use std::{env, path::Path, process::ExitCode};
use vxedge_core::gateway::GatewayPlan;
use vxedge_core::sonic::plan::SonicPlan;
use vxedge_core::sonic::render::render_json;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let (manifest_path, trunk_port) = match args.as_slice() {
        [_, m, t] => (m, t),
        _ => {
            eprintln!("usage: render_sonic <manifest.yaml> <trunk-port>");
            return ExitCode::from(2);
        }
    };
    match run(manifest_path, trunk_port) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(manifest_path: &str, trunk_port: &str) -> Result<(), Box<dyn std::error::Error>> {
    let model = load_workspace(manifest_path)?;
    let gateway_plan = GatewayPlan::from_model(&model)?;
    let sonic_plan = SonicPlan::from_gateway_plan(&gateway_plan, trunk_port);
    println!("{}", render_json(&sonic_plan)?);
    Ok(())
}

/// A §6.0 workspace, given as a directory or as a single file of `---`-separated documents.
fn load_workspace(
    path: &str,
) -> Result<vxedge_core::model::Experiment, vxedge_core::model::LoadError> {
    let path = Path::new(path);
    if path.is_dir() {
        vxedge_core::model::load_dir(path)
    } else {
        vxedge_core::model::load_yaml(&std::fs::read_to_string(path).expect("read workspace"))
    }
}
