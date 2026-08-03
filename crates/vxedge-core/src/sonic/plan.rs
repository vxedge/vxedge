//! `SonicPlan`: vxgateway's slice/VLAN intent, reduced to exactly what SONiC's config needs to
//! express the same partitioning. Deliberately narrower than
//! [`GatewayPlan`](crate::gateway::GatewayPlan), which also carries HTB queue-tree, endpoint and
//! impairment detail SONiC's config schema has no equivalent for — this twin claims config
//! portability for the slice/VLAN topology, nothing else.

use crate::gateway::GatewayPlan;

/// One `network.slices[].mode: vlan` entry, reduced to what a SONiC `VLAN` table row needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SonicVlan {
    pub id: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SonicPlan {
    /// One entry per VLAN slice. `mode: s-nssai` slices carry no VLAN and are out of scope for
    /// this twin — SONiC's config_db.json schema has no representation for a 5G slice
    /// attachment; that territory is `crate::fiveg`'s, not this one's.
    pub vlans: Vec<SonicVlan>,
    /// The trunk port carrying every VLAN, tagged. Supplied by the caller rather than guessed
    /// here: SONiC-VS's own interface naming for its virtual ports has never been confirmed
    /// against a live boot by this codebase (`docs/TASKS.md` #16 tracks that verification as
    /// the explicit next step), and a wrong hardcoded guess would silently misrender rather
    /// than fail loudly.
    pub trunk_port: String,
}

impl SonicPlan {
    /// Reduces a full `GatewayPlan` to exactly the slice/VLAN intent this twin claims
    /// portability for.
    pub fn from_gateway_plan(plan: &GatewayPlan, trunk_port: impl Into<String>) -> Self {
        Self {
            vlans: plan
                .slices
                .iter()
                .map(|s| SonicVlan { id: s.vlan })
                .collect(),
            trunk_port: trunk_port.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayPlan;

    fn model(yaml: &str) -> crate::model::Experiment {
        crate::model::load_yaml(yaml).expect("valid test workspace")
    }

    const BASE: &str = r#"
kind: experiment
id: exp-sonic
vehicles: [veh]
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
    - { id: infotainment, mode: vlan, vlan: 103 }
---
kind: vehicle
id: veh
devices:
  node-ecu-01: { role: ecu, backend: container }
links:
  - { id: l-adas, endpoints: [node-ecu-01:eth0], slice: adas }
"#;

    #[test]
    fn reduces_a_gateway_plan_to_its_vlan_ids_and_the_given_trunk_port() {
        let plan = GatewayPlan::from_model(&model(BASE)).unwrap();
        let sonic = SonicPlan::from_gateway_plan(&plan, "Ethernet0");
        assert_eq!(
            sonic.vlans,
            vec![SonicVlan { id: 101 }, SonicVlan { id: 103 }]
        );
        assert_eq!(sonic.trunk_port, "Ethernet0");
    }

    #[test]
    fn an_snssai_only_plan_has_no_vlans_to_twin() {
        let ws = r#"
kind: experiment
id: exp-sonic-snssai
vehicles: [veh]
network:
  slices:
    - { id: adas5g, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }
---
kind: vehicle
id: veh
devices: {}
"#;
        let plan = GatewayPlan::from_model(&model(ws)).unwrap();
        let sonic = SonicPlan::from_gateway_plan(&plan, "Ethernet0");
        assert!(
            sonic.vlans.is_empty(),
            "s-nssai slices have no VLAN for this twin to render"
        );
    }
}
