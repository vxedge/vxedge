//! Deterministic `config_db.json` rendering from a [`SonicPlan`]. Only the `VLAN`/`VLAN_MEMBER`
//! tables: exactly what a slice/VLAN twin needs to claim, no more. SONiC's real schema has
//! dozens of other tables (ACL, QoS, BGP, PORT, INTERFACE, ...) this twin makes no claim about
//! rendering or verifying.

use super::plan::SonicPlan;
use std::collections::BTreeMap;

/// One `VLAN` table row. Field name matches SONiC's own `config_db.json` schema verbatim
/// (`vlanid`, a string even though it's numeric — SONiC's own convention, not this crate's).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VlanEntry {
    pub vlanid: String,
}

/// One `VLAN_MEMBER` table row. The real schema keys this table `"Vlan<id>|<port>"` — a
/// composite key a Rust map key can express directly as a `String`, so nothing is flattened out
/// here the way [`ConfigDb`]'s map keys already carry it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VlanMemberEntry {
    pub tagging_mode: &'static str,
}

/// The subset of SONiC's `config_db.json` this twin renders and claims to be correct.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ConfigDb {
    #[serde(rename = "VLAN")]
    pub vlan: BTreeMap<String, VlanEntry>,
    #[serde(rename = "VLAN_MEMBER")]
    pub vlan_member: BTreeMap<String, VlanMemberEntry>,
}

/// Renders every declared VLAN and its trunk-port membership. `BTreeMap` throughout, not
/// `HashMap`: `config_db.json` output has to be byte-for-byte reproducible across runs, the same
/// reproducibility rule every other rendered artifact in this codebase already follows
/// (`ifnames`'s deterministic naming, `fiveg::render`'s rendered config set).
pub fn render(plan: &SonicPlan) -> ConfigDb {
    let mut db = ConfigDb::default();
    for v in &plan.vlans {
        db.vlan.insert(
            format!("Vlan{}", v.id),
            VlanEntry {
                vlanid: v.id.to_string(),
            },
        );
        db.vlan_member.insert(
            format!("Vlan{}|{}", v.id, plan.trunk_port),
            VlanMemberEntry {
                tagging_mode: "tagged",
            },
        );
    }
    db
}

/// `render`, serialized to the pretty-printed JSON text a real `config_db.json` file is.
pub fn render_json(plan: &SonicPlan) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&render(plan))
}

#[cfg(test)]
mod tests {
    use super::super::plan::SonicVlan;
    use super::*;

    fn plan() -> SonicPlan {
        SonicPlan {
            vlans: vec![SonicVlan { id: 101 }, SonicVlan { id: 103 }],
            trunk_port: "Ethernet0".into(),
        }
    }

    #[test]
    fn renders_one_vlan_row_and_one_tagged_trunk_membership_per_slice() {
        let db = render(&plan());
        assert_eq!(db.vlan.len(), 2);
        assert_eq!(
            db.vlan["Vlan101"],
            VlanEntry {
                vlanid: "101".into()
            }
        );
        assert_eq!(
            db.vlan["Vlan103"],
            VlanEntry {
                vlanid: "103".into()
            }
        );
        assert_eq!(db.vlan_member.len(), 2);
        assert_eq!(
            db.vlan_member["Vlan101|Ethernet0"],
            VlanMemberEntry {
                tagging_mode: "tagged"
            }
        );
        assert_eq!(
            db.vlan_member["Vlan103|Ethernet0"],
            VlanMemberEntry {
                tagging_mode: "tagged"
            }
        );
    }

    #[test]
    fn an_empty_plan_renders_empty_tables_not_an_error() {
        let db = render(&SonicPlan {
            vlans: vec![],
            trunk_port: "Ethernet0".into(),
        });
        assert!(db.vlan.is_empty());
        assert!(db.vlan_member.is_empty());
    }

    #[test]
    fn json_matches_sonic_config_db_schema_shape() {
        let json = render_json(&SonicPlan {
            vlans: vec![SonicVlan { id: 101 }],
            trunk_port: "Ethernet0".into(),
        })
        .unwrap();
        // Pinned, the same way `ifnames`'s rendered names are pinned: this text is what a real
        // `config_db.json` on a booted SONiC-VS is compared against, so a silent format change
        // here is a silent change to what "config parity" means.
        assert_eq!(
            json,
            r#"{
  "VLAN": {
    "Vlan101": {
      "vlanid": "101"
    }
  },
  "VLAN_MEMBER": {
    "Vlan101|Ethernet0": {
      "tagging_mode": "tagged"
    }
  }
}"#
        );
    }
}
