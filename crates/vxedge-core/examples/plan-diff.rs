//! Compare the §6.1 and model planning paths on real files.
//!
//! The 5b equivalence tests use fixtures. A fixture is written to pass; a real experiment is
//! not. This runs both paths over an actual workspace and an actual manifest and diffs every
//! plan a campaign depends on.
//!
//! ```text
//! cargo run -p vxedge-core --example plan-diff -- <workspace-dir> <manifest.yaml>
//! ```

use vxedge_core::gateway::GatewayPlan;
use vxedge_core::traffic::TrafficPlan;
use vxedge_core::zones::plan::{plan_nodes, plan_nodes_from_model};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [ws_dir, manifest_path] = args.as_slice() else {
        eprintln!("usage: plan-diff <workspace-dir> <manifest.yaml>");
        std::process::exit(2);
    };

    let ws = vxedge_core::Workspace::load_validated(std::path::Path::new(ws_dir))
        .unwrap_or_else(|e| panic!("workspace: {e}"));
    let model = vxedge_core::model::build(&ws).unwrap_or_else(|e| panic!("model: {e}"));
    let invariants = model.check();
    assert!(invariants.is_empty(), "model invariants: {invariants:?}");

    let yaml = std::fs::read_to_string(manifest_path).expect("manifest readable");
    let manifest = vxedge_core::ExperimentManifest::from_yaml(&yaml).expect("manifest parses");

    let gw_m = GatewayPlan::from_model(&model).expect("model gateway plan");
    let gw_f = GatewayPlan::from_manifest(&manifest).expect("manifest gateway plan");
    let mut bad = 0;
    let mut cmp = |what: &str, a: String, b: String| {
        if a == b {
            println!("  SAME  {what}");
        } else {
            println!("  DIFF  {what}\n        model:    {a}\n        manifest: {b}");
            bad += 1;
        }
    };

    println!("== gateway");
    cmp("summary", gw_m.summary(), gw_f.summary());
    cmp(
        "root_rate_mbps",
        gw_m.root_rate_mbps.to_string(),
        gw_f.root_rate_mbps.to_string(),
    );
    cmp(
        "endpoints",
        format!("{:?}", gw_m.endpoints),
        format!("{:?}", gw_f.endpoints),
    );

    println!("== nodes");
    let dir = std::path::Path::new(ws_dir);
    let n_m = plan_nodes_from_model(&model, &gw_m, dir).expect("model node plans");
    let n_f = plan_nodes(&manifest, &gw_f, dir).expect("manifest node plans");
    cmp("count", n_m.len().to_string(), n_f.len().to_string());
    for (a, b) in n_m.iter().zip(n_f.iter()) {
        cmp(
            &format!("node {}", a.node_id),
            format!(
                "{} ep={} peer={} vlan={} backend={:?} netns={}",
                a.node_id,
                a.endpoint,
                a.peer,
                a.vlan,
                a.backend,
                a.runtime_name()
            ),
            format!(
                "{} ep={} peer={} vlan={} backend={:?} netns={}",
                b.node_id,
                b.endpoint,
                b.peer,
                b.vlan,
                b.backend,
                b.runtime_name()
            ),
        );
    }

    println!("== traffic");
    let t_m = TrafficPlan::from_model(&model, &gw_m).expect("model traffic plan");
    let t_f = TrafficPlan::from_manifest(&manifest, &gw_f).expect("manifest traffic plan");
    cmp(
        "count",
        t_m.profiles.len().to_string(),
        t_f.profiles.len().to_string(),
    );
    for (a, b) in t_m.profiles.iter().zip(t_f.profiles.iter()) {
        cmp(
            &format!("profile {}", a.id),
            format!("{a:?}"),
            format!("{b:?}"),
        );
    }

    println!("\n{}", if bad == 0 { "IDENTICAL" } else { "DIVERGENT" });
    std::process::exit(if bad == 0 { 0 } else { 1 });
}
