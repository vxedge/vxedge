//! Diff the plans a §6.0 workspace and a §6.1 manifest produce.
//!
//! The migration check: a workspace has replaced a frozen manifest only if it provisions the
//! *same* topology. Endpoint names are the sharp edge — campaign scripts address `vzp0`/`vzp1`
//! directly, so a renumbering would keep validating, keep running, and quietly measure the wrong
//! interface.
//!
//! Deleted with §6.1 itself (docs/TASKS.md row 17); until then it is how each migration is
//! signed off.
//!
//! ```text
//! cargo run -p vxedge-core --example plan-diff -- <workspace-dir> <manifest.yaml>
//! ```

use vxedge_core::gateway::GatewayPlan;
use vxedge_core::traffic::TrafficPlan;
use vxedge_core::zones::plan_nodes_from_model;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [ws_dir, manifest_path] = args.as_slice() else {
        eprintln!("usage: plan-diff <workspace-dir> <manifest.yaml>");
        std::process::exit(2);
    };

    let ws = vxedge_core::Workspace::load_validated(std::path::Path::new(ws_dir))
        .unwrap_or_else(|e| panic!("workspace: {e}"));
    let a = vxedge_core::model::build(&ws).unwrap_or_else(|e| panic!("model: {e}"));
    let violations = a.check();
    assert!(violations.is_empty(), "model invariants: {violations:?}");

    let yaml = std::fs::read_to_string(manifest_path).expect("manifest readable");
    let b = vxedge_core::ExperimentManifest::from_yaml(&yaml)
        .expect("manifest parses")
        .to_model()
        .expect("manifest adapts");

    let mut bad = 0;
    let mut cmp = |what: &str, x: String, y: String| {
        if x == y {
            println!("  SAME  {what}");
        } else {
            println!("  DIFF  {what}\n        workspace: {x}\n        frozen:    {y}");
            bad += 1;
        }
    };

    let (ga, gb) = (
        GatewayPlan::from_model(&a).expect("workspace gateway plan"),
        GatewayPlan::from_model(&b).expect("frozen gateway plan"),
    );
    cmp("gateway summary", ga.summary(), gb.summary());
    cmp(
        "endpoints",
        format!("{:?}", ga.endpoints),
        format!("{:?}", gb.endpoints),
    );

    let dir = std::path::Path::new(ws_dir);
    let (na, nb) = (
        plan_nodes_from_model(&a, &ga, dir).expect("workspace node plans"),
        plan_nodes_from_model(&b, &gb, dir).expect("frozen node plans"),
    );
    cmp("node count", na.len().to_string(), nb.len().to_string());
    for (x, y) in na.iter().zip(nb.iter()) {
        cmp(
            &format!("node {}", x.node_id),
            format!(
                "{} {} {} {} {:?} {}",
                x.node_id,
                x.endpoint,
                x.peer,
                x.vlan,
                x.backend,
                x.runtime_name()
            ),
            format!(
                "{} {} {} {} {:?} {}",
                y.node_id,
                y.endpoint,
                y.peer,
                y.vlan,
                y.backend,
                y.runtime_name()
            ),
        );
    }

    if let (Ok(ta), Ok(tb)) = (
        TrafficPlan::from_model(&a, &ga),
        TrafficPlan::from_model(&b, &gb),
    ) {
        cmp(
            "profile count",
            ta.profiles.len().to_string(),
            tb.profiles.len().to_string(),
        );
        for (x, y) in ta.profiles.iter().zip(tb.profiles.iter()) {
            cmp(
                &format!("profile {}", x.id),
                format!("{x:?}"),
                format!("{y:?}"),
            );
        }
    }

    println!("\n{}", if bad == 0 { "IDENTICAL" } else { "DIVERGENT" });
    std::process::exit(if bad == 0 { 0 } else { 1 });
}
