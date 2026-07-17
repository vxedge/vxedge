//! Dev tool: render a manifest's `mode: s-nssai` config set to a directory,
//! for validating rendered configs against live Open5GS/UERANSIM binaries
//! (e.g. `open5gs-amfd -c <outdir>/open5gs/amf.yaml`). The process-lifecycle
//! stage of the fiveg driver reuses this write-out shape.
//!
//! Usage: cargo run -p vxedge-core --example render_fiveg -- <manifest.yaml> <outdir>

use std::{env, fs, path::Path, process::ExitCode};
use vxedge_core::ExperimentManifest;
use vxedge_core::fiveg::FiveGPlan;
use vxedge_core::fiveg::render::write_all;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let (manifest_path, outdir) = match args.as_slice() {
        [_, m, o] => (m, o),
        _ => {
            eprintln!("usage: render_fiveg <manifest.yaml> <outdir>");
            return ExitCode::from(2);
        }
    };
    match run(manifest_path, outdir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(manifest_path: &str, outdir: &str) -> Result<(), Box<dyn std::error::Error>> {
    let yaml = fs::read_to_string(manifest_path)?;
    let manifest = ExperimentManifest::from_yaml(&yaml)?;
    let plan = FiveGPlan::from_manifest(&manifest)?;
    for path in write_all(&plan, Path::new(outdir))? {
        println!("wrote {}", path.display());
    }
    Ok(())
}
