//! Render a GatewayPlan onto the kernel (apply), remove it (teardown), and
//! observe actual state (the reconciler's "observed" side). All operations
//! are idempotent: creates tolerate "exists", deletes tolerate "missing",
//! and tc uses `replace` semantics throughout.

use super::netops::{NetOpsError, Shell};
use super::plan::GatewayPlan;

/// HTB minor of the default (unclassified traffic) class.
const DEFAULT_CLASS: &str = "fffe";

/// Phase 1 of apply: bridge + trunk uplink.
pub fn apply_links(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    if !sh.link_exists(&plan.bridge) {
        sh.run(
            "ip",
            &[
                "link",
                "add",
                &plan.bridge,
                "type",
                "bridge",
                "vlan_filtering",
                "1",
            ],
        )?;
    }
    sh.run("ip", &["link", "set", &plan.bridge, "up"])?;

    if !sh.link_exists(&plan.uplink) {
        sh.run(
            "ip",
            &[
                "link",
                "add",
                &plan.uplink,
                "type",
                "veth",
                "peer",
                "name",
                &plan.uplink_peer,
            ],
        )?;
    }
    sh.run("ip", &["link", "set", &plan.uplink, "master", &plan.bridge])?;
    sh.run("ip", &["link", "set", &plan.uplink, "up"])?;
    sh.run("ip", &["link", "set", &plan.uplink_peer, "up"])?;
    Ok(())
}

/// Phase 2 of apply: per-slice VLANs on the trunk + the HTB queue tree.
pub fn apply_slices(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    let root_rate = format!("{}mbit", plan.root_rate_mbps);
    sh.run(
        "tc",
        &[
            "qdisc",
            "replace",
            "dev",
            &plan.uplink,
            "root",
            "handle",
            "1:",
            "htb",
            "default",
            DEFAULT_CLASS,
        ],
    )?;
    sh.run(
        "tc",
        &[
            "class",
            "replace",
            "dev",
            &plan.uplink,
            "parent",
            "1:",
            "classid",
            "1:1",
            "htb",
            "rate",
            &root_rate,
        ],
    )?;
    // Default class: best-effort leftovers, low floor, can borrow to root.
    sh.run(
        "tc",
        &[
            "class",
            "replace",
            "dev",
            &plan.uplink,
            "parent",
            "1:1",
            "classid",
            &format!("1:{DEFAULT_CLASS}"),
            "htb",
            "rate",
            "1mbit",
            "ceil",
            &root_rate,
            "prio",
            "7",
        ],
    )?;

    for (i, slice) in plan.slices.iter().enumerate() {
        let vid = slice.vlan.to_string();
        // Trunk carries every slice VLAN tagged.
        sh.run_tolerate_exists("bridge", &["vlan", "add", "dev", &plan.uplink, "vid", &vid])?;
        let classid = format!("1:{:x}", slice.class_minor);
        sh.run(
            "tc",
            &[
                "class",
                "replace",
                "dev",
                &plan.uplink,
                "parent",
                "1:1",
                "classid",
                &classid,
                "htb",
                "rate",
                &format!("{}mbit", slice.min_rate_mbps),
                "ceil",
                &format!("{}mbit", slice.max_rate_mbps),
                "prio",
                &slice.priority.to_string(),
            ],
        )?;
        // Map the slice's VLAN onto its class (egress on the trunk).
        sh.run(
            "tc",
            &[
                "filter",
                "replace",
                "dev",
                &plan.uplink,
                "parent",
                "1:",
                "pref",
                &(10 + i).to_string(),
                "protocol",
                "802.1q",
                "flower",
                "vlan_id",
                &vid,
                "classid",
                &classid,
            ],
        )?;
    }
    Ok(())
}

/// Phase 3 of apply: zone endpoints as access ports (pvid untagged).
pub fn apply_endpoints(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    for ep in &plan.endpoints {
        if !sh.link_exists(&ep.name) {
            sh.run(
                "ip",
                &[
                    "link", "add", &ep.name, "type", "veth", "peer", "name", &ep.peer,
                ],
            )?;
        }
        sh.run("ip", &["link", "set", &ep.name, "master", &plan.bridge])?;
        let vid = ep.vlan.to_string();
        // Access port: node-side traffic is untagged, tagged as the slice
        // VLAN on ingress to the bridge. Drop the default VLAN 1 membership.
        sh.run_tolerate_exists(
            "bridge",
            &[
                "vlan", "add", "dev", &ep.name, "vid", &vid, "pvid", "untagged",
            ],
        )?;
        sh.run_tolerate_missing("bridge", &["vlan", "del", "dev", &ep.name, "vid", "1"])?;
        sh.run("ip", &["link", "set", &ep.name, "up"])?;
        sh.run("ip", &["link", "set", &ep.peer, "up"])?;
    }
    Ok(())
}

/// Full apply, in dependency order.
pub fn apply(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    apply_links(sh, plan)?;
    apply_slices(sh, plan)?;
    apply_endpoints(sh, plan)?;
    Ok(())
}

/// Remove everything the plan created. Deleting a veth removes its peer;
/// deleting the bridge detaches remaining ports. Tolerates absence.
pub fn teardown(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    for ep in &plan.endpoints {
        sh.run_tolerate_missing("ip", &["link", "del", &ep.name])?;
    }
    sh.run_tolerate_missing("ip", &["link", "del", &plan.uplink])?;
    sh.run_tolerate_missing("ip", &["link", "del", &plan.bridge])?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct SliceObservation {
    pub id: String,
    pub vlan: u16,
    /// HTB class for this slice exists on the uplink.
    pub class_present: bool,
    /// VLAN is carried (tagged) on the trunk uplink.
    pub trunk_vlan_present: bool,
}

impl SliceObservation {
    pub fn healthy(&self) -> bool {
        self.class_present && self.trunk_vlan_present
    }
}

#[derive(Debug, Clone)]
pub struct GatewayObservation {
    pub bridge_present: bool,
    pub uplink_present: bool,
    pub slices: Vec<SliceObservation>,
    pub endpoints_present: usize,
    pub endpoints_expected: usize,
}

impl GatewayObservation {
    pub fn fully_up(&self) -> bool {
        self.bridge_present
            && self.uplink_present
            && self.endpoints_present == self.endpoints_expected
            && self.slices.iter().all(|s| s.healthy())
    }
}

/// Observe kernel state against the plan (the reconciler's read side).
pub fn observe(sh: &Shell, plan: &GatewayPlan) -> Result<GatewayObservation, NetOpsError> {
    let bridge_present = sh.link_exists(&plan.bridge);
    let uplink_present = sh.link_exists(&plan.uplink);

    // HTB classes on the uplink. Parsed from text output — `tc -j class show`
    // emits non-JSON on several shipped iproute2 versions (bit us in CI).
    // Lines look like: "class htb 1:10 parent 1:1 prio 1 rate 100Mbit ...".
    let class_handles: Vec<String> = if uplink_present {
        sh.run("tc", &["class", "show", "dev", &plan.uplink])?
            .lines()
            .filter(|l| l.starts_with("class "))
            .filter_map(|l| l.split_whitespace().nth(2))
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };

    // Trunk VLANs: [{"ifname":"vxup0","vlans":[{"vlan":101,...}]}]
    let trunk_vlans: Vec<u16> = if uplink_present {
        let v = sh.json("bridge", &["-j", "vlan", "show", "dev", &plan.uplink])?;
        v.as_array()
            .and_then(|a| a.first())
            .and_then(|e| e.get("vlans"))
            .and_then(|vs| vs.as_array())
            .map(|vs| {
                vs.iter()
                    .filter_map(|v| v.get("vlan").and_then(|x| x.as_u64()))
                    .map(|x| x as u16)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let slices = plan
        .slices
        .iter()
        .map(|s| SliceObservation {
            id: s.id.clone(),
            vlan: s.vlan,
            class_present: class_handles
                .iter()
                .any(|h| h == &format!("1:{:x}", s.class_minor)),
            trunk_vlan_present: trunk_vlans.contains(&s.vlan),
        })
        .collect();

    let endpoints_present = plan
        .endpoints
        .iter()
        .filter(|ep| sh.link_exists(&ep.name))
        .count();

    Ok(GatewayObservation {
        bridge_present,
        uplink_present,
        slices,
        endpoints_present,
        endpoints_expected: plan.endpoints.len(),
    })
}
