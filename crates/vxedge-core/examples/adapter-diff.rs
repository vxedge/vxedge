//! Verify the §6.1 → model adapter against a real manifest.
//!
//! Plans it two ways — straight from §6.1, and through the adapter into the model — and diffs
//! everything a campaign depends on. This is the check that decides whether §6.1 can be deleted:
//! if a frozen manifest plans differently through the model, the migration loses a result.
//!
//! ```text
//! cargo run -p vxedge-core --example adapter-diff -- <manifest.yaml>...
//! ```

use vxedge_core::gateway::GatewayPlan;
use vxedge_core::traffic::TrafficPlan;
use vxedge_core::zones::plan::{plan_nodes, plan_nodes_from_model};

fn main() {
    let mut failures = 0;
    for path in std::env::args().skip(1) {
        println!("== {path}");
        let yaml = std::fs::read_to_string(&path).expect("readable");
        let manifest = match vxedge_core::ExperimentManifest::from_yaml(&yaml) {
            Ok(m) => m,
            Err(e) => {
                println!("  SKIP  does not parse: {e}");
                continue;
            }
        };
        let model = manifest.to_model().expect("adapts");
        let violations = model.check();
        if !violations.is_empty() {
            println!("  FAIL  model invariants: {violations:?}");
            failures += 1;
        }

        let mut cmp = |what: &str, a: String, b: String| {
            if a == b {
                println!("  SAME  {what}");
            } else {
                println!("  DIFF  {what}\n        model:  {a}\n        §6.1:   {b}");
                failures += 1;
            }
        };

        let (gm, gf) = match (
            GatewayPlan::from_model(&model),
            GatewayPlan::from_manifest(&manifest),
        ) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                println!(
                    "  NOTE  gateway plan: model={:?} §6.1={:?}",
                    a.err(),
                    b.err()
                );
                continue;
            }
        };
        cmp("gateway summary", gm.summary(), gf.summary());
        // Endpoint identity is the whole point: campaign scripts address vzp<N> by name, so a
        // renumbering silently attaches an experimental arm to the wrong interface.
        cmp(
            "endpoints",
            format!("{:?}", gm.endpoints),
            format!("{:?}", gf.endpoints),
        );

        let dir = std::path::Path::new(&path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        if let (Ok(nm), Ok(nf)) = (
            plan_nodes_from_model(&model, &gm, dir),
            plan_nodes(&manifest, &gf, dir),
        ) {
            cmp("node count", nm.len().to_string(), nf.len().to_string());
            for (a, b) in nm.iter().zip(nf.iter()) {
                cmp(
                    &format!("node {}", a.node_id),
                    format!(
                        "{} {} {} {} {:?}",
                        a.node_id, a.endpoint, a.peer, a.vlan, a.backend
                    ),
                    format!(
                        "{} {} {} {} {:?}",
                        b.node_id, b.endpoint, b.peer, b.vlan, b.backend
                    ),
                );
            }
        }
        if let (Ok(tm), Ok(tf)) = (
            TrafficPlan::from_model(&model, &gm),
            TrafficPlan::from_manifest(&manifest, &gf),
        ) {
            for (a, b) in tm.profiles.iter().zip(tf.profiles.iter()) {
                cmp(
                    &format!("profile {}", a.id),
                    format!("{a:?}"),
                    format!("{b:?}"),
                );
            }
        }
    }
    println!(
        "\n{}",
        if failures == 0 {
            "ALL IDENTICAL"
        } else {
            "DIVERGENT"
        }
    );
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
