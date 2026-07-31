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

/// True when our HTB root qdisc (handle 1:) is already installed on `dev`.
fn htb_root_present(sh: &Shell, dev: &str) -> Result<bool, NetOpsError> {
    let out = sh.run("tc", &["qdisc", "show", "dev", dev])?;
    Ok(out.lines().any(|l| l.starts_with("qdisc htb 1:")))
}

/// Phase 2 of apply: per-slice VLANs on the trunk + the HTB queue tree.
pub fn apply_slices(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    let root_rate = format!("{}mbit", plan.root_rate_mbps);
    // HTB root qdiscs don't support in-place change (`replace` fails on an
    // existing one), so create only when absent; classes/filters below use
    // replace semantics and converge on every apply.
    if !htb_root_present(sh, &plan.uplink)? {
        sh.run(
            "tc",
            &[
                "qdisc",
                "add",
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
    }
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
        // Flower `replace` without an explicit handle EEXISTs on an identical
        // filter — which is already the converged state, so tolerate it.
        // (Changing a vlan→class mapping in place is reconciler follow-up.)
        sh.run_tolerate_exists(
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
/// Veth for container nodes (peer moves into the node's netns at `zone up`);
/// tap for VM nodes (libvirt/qemu opens it directly).
pub fn apply_endpoints(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    for ep in &plan.endpoints {
        if !sh.link_exists(&ep.name) {
            match ep.kind {
                super::plan::EndpointKind::Veth => sh.run(
                    "ip",
                    &[
                        "link", "add", &ep.name, "type", "veth", "peer", "name", &ep.peer,
                    ],
                )?,
                super::plan::EndpointKind::Tap => {
                    sh.run("ip", &["tuntap", "add", "dev", &ep.name, "mode", "tap"])?
                }
            };
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
        // Bringing the peer up is only meaningful while the peer is still on this side. A
        // running container zone has already taken it into its own netns and renamed it `eth0`
        // (`zones::backend`), so its *absence here* is the converged state, not a failure.
        //
        // Without this guard `up` is not idempotent, which the hard rule forbids: the first
        // `experiment up` provisions the gateway and then starts the zones, and the second one
        // dies on `ip link set vzp0 up: Cannot find device`. Latent until `experiment up` made
        // gateway-then-zones a single command anyone would naturally re-run — found on the
        // first real provisioning run, 2026-07-30.
        if ep.kind == super::plan::EndpointKind::Veth && sh.link_exists(&ep.peer) {
            sh.run("ip", &["link", "set", &ep.peer, "up"])?;
        }
    }
    Ok(())
}

/// VLAN membership for the 5G core's uplink port on a `vlan_filtering 1`
/// bridge (`vxbr0`): every port needs an explicit membership or it drops
/// all traffic. `host_veth` must already be attached to the bridge and up
/// (`fiveg::lifecycle::bridge_up` does that part — this only adds the
/// membership that primitive deliberately leaves undecided, same convention
/// [`apply_endpoints`] uses for zone nodes, but on
/// `fiveg::lifecycle::FIVEG_UPLINK_VLAN` — the S-NSSAI slices attaching via
/// this port carry no manifest `vlan:` of their own).
///
/// Also gives the **host's own** network stack an address in
/// `fiveg::lifecycle::BRIDGE_ATTACH_SUBNET` — found necessary live
/// 2026-07-17: a host-generated `vxprobe send` targeting a UE's
/// PDU-session address got 100% loss even with the CN pod's port
/// correctly VLAN-tagged, because nothing gave the *host itself* an
/// address reachable through that VLAN. A `vlan_filtering 1` bridge's own
/// internal ("self") path needs its own explicit VLAN membership, same as
/// any other port — `bridge vlan add dev <bridge> vid <n> self` — plus a
/// `<bridge>.<vid>` subinterface to actually address it, the s-nssai
/// equivalent of the vlan path's `<uplink_peer>.<vlan>` convention
/// (`traffic::lifecycle::ensure_endpoint`'s `VlanUplinkFarSide` case).
///
/// Returns the created host-side interface name, for [`apply_fiveg_ue_routes`]
/// to route each UE subnet through — an address in `BRIDGE_ATTACH_SUBNET`
/// alone isn't enough for the host to reach a UE's PDU-session subnet, an
/// *entirely different* subnet reachable only via the CN pod's own routing;
/// found live the same day, one call deeper than this bug (see that
/// function's own doc comment).
pub fn apply_fiveg_uplink(sh: &Shell, host_veth: &str) -> Result<String, NetOpsError> {
    let vid = crate::fiveg::FIVEG_UPLINK_VLAN.to_string();
    sh.run_tolerate_exists(
        "bridge",
        &[
            "vlan", "add", "dev", host_veth, "vid", &vid, "pvid", "untagged",
        ],
    )?;
    sh.run_tolerate_missing("bridge", &["vlan", "del", "dev", host_veth, "vid", "1"])?;

    let bridge = host_veth_master(sh, host_veth)?;
    sh.run_tolerate_exists(
        "bridge",
        &["vlan", "add", "dev", &bridge, "vid", &vid, "self"],
    )?;
    let host_iface = format!("{bridge}.{vid}");
    if !sh.link_exists(&host_iface) {
        sh.run(
            "ip",
            &[
                "link",
                "add",
                "link",
                &bridge,
                "name",
                &host_iface,
                "type",
                "vlan",
                "id",
                &vid,
            ],
        )?;
    }
    let host_addr = format!("{}/24", crate::fiveg::BRIDGE_ATTACH_HOST_ADDR);
    sh.run_tolerate_exists("ip", &["addr", "add", &host_addr, "dev", &host_iface])?;
    sh.run("ip", &["link", "set", &host_iface, "up"])?;
    Ok(host_iface)
}

/// Route each s-nssai slice's UE subnet (`FiveGPlan::sessions[].ue_subnet`,
/// e.g. `10.46.0.0/16`) through the CN pod
/// (`fiveg::lifecycle::BRIDGE_ATTACH_CN_ADDR`), via the host-side VLAN
/// subinterface [`apply_fiveg_uplink`] created and returned.
///
/// Found live 2026-07-17, immediately after fixing `apply_fiveg_uplink`'s
/// own host-reachability gap: giving the host a *local* address in
/// `BRIDGE_ATTACH_SUBNET` (`10.98.0.0/24`) still wasn't enough on its
/// own — a UE's PDU-session subnet is a *completely different* subnet,
/// reachable only through the CN pod's own internal routing
/// (`ip_forward` between its bridge-attach interface and each UPF's tun
/// device, already enabled by `bridge_up`). Without an explicit route,
/// the host's kernel has no way to know that subnet exists at all, let
/// alone that `BRIDGE_ATTACH_CN_ADDR` is the way to it — exactly the
/// `sudo ip route add <ue_subnet> via 10.98.0.1 dev <bridge>` step every
/// earlier manual verification script added by hand and this now
/// replaces with real, tested code.
pub fn apply_fiveg_ue_routes(
    sh: &Shell,
    host_iface: &str,
    ue_subnets: &[String],
) -> Result<(), NetOpsError> {
    for subnet in ue_subnets {
        sh.run_tolerate_exists(
            "ip",
            &[
                "route",
                "add",
                subnet,
                "via",
                crate::fiveg::BRIDGE_ATTACH_CN_ADDR,
                "dev",
                host_iface,
            ],
        )?;
    }
    Ok(())
}

/// One s-nssai slice's downlink HTB parameters, decoupled from
/// `fiveg::SliceSession` on purpose — `gateway` doesn't depend on `fiveg`'s
/// types, matching [`apply_fiveg_ue_routes`]'s own plain-`String` (not
/// `SliceSession`) convention. The caller (`vxedged`'s `services.rs`, which
/// already sees both modules) maps `FiveGPlan::sessions` into this shape.
#[derive(Debug, Clone)]
pub struct FiveGSliceQos {
    pub ue_subnet: String,
    pub min_rate_mbps: u64,
    pub max_rate_mbps: u64,
    pub priority: u8,
    pub class_minor: u16,
}

/// Downlink HTB shaping for s-nssai slices, on the *host-side* VLAN
/// subinterface [`apply_fiveg_uplink`] created (`host_iface`) — the direct
/// analog of [`apply_slices`]'s tree on `plan.uplink`: both `adas-probe`
/// and `infotainment-load` traffic profiles are `source: internet`
/// (host-originated), so this one interface is exactly where every
/// s-nssai slice's downlink traffic converges before
/// [`apply_fiveg_ue_routes`] fans it out toward its own UE subnet — the
/// same "one shared bottleneck link, classful HTB, per-slice rate/ceil/
/// priority" shape [`apply_slices`] already proves live, just classified
/// by destination subnet (`tc ... u32 match ip dst <ue_subnet>`) instead
/// of by VLAN tag (s-nssai traffic isn't VLAN-tagged at this point).
///
/// Found necessary 2026-07-18, not before: Open5GS's own subscriber-level
/// AMBR field is recorded by its SMF/UDR but not enforced by its software
/// UPF in this deployment — confirmed live by patching a subscriber's
/// session AMBR down to 20mbit and measuring the UE's own `uesimtun0`
/// still receiving the full offered ~600mbit unconstrained (`docs/
/// STATUS.md` has the full account). So real per-slice isolation for the
/// snssai arm needs a kernel-level guarantee, not 5G-core QoS signaling
/// that isn't actually active here.
///
/// Opt-in per slice (`slices` only contains sessions whose manifest
/// declared a `qos` block — see `fiveg::SliceSession::qos`'s own doc
/// comment for why an unconditional 1mbit-floor default, the way
/// [`apply_slices`] defaults an *absent* vlan `qos`, would have silently
/// broken every s-nssai manifest that predates this field). No teardown
/// function needed: [`teardown_fiveg_uplink`] deletes `host_iface`
/// outright, and the kernel frees the whole attached qdisc/class/filter
/// tree with it — same reasoning [`teardown`] already relies on for
/// `apply_slices`'s own tree.
pub fn apply_fiveg_slice_qos(
    sh: &Shell,
    host_iface: &str,
    root_rate_mbps: u64,
    slices: &[FiveGSliceQos],
) -> Result<(), NetOpsError> {
    let root_rate = format!("{root_rate_mbps}mbit");
    if !htb_root_present(sh, host_iface)? {
        sh.run(
            "tc",
            &[
                "qdisc",
                "add",
                "dev",
                host_iface,
                "root",
                "handle",
                "1:",
                "htb",
                "default",
                DEFAULT_CLASS,
            ],
        )?;
    }
    sh.run(
        "tc",
        &[
            "class", "replace", "dev", host_iface, "parent", "1:", "classid", "1:1", "htb", "rate",
            &root_rate,
        ],
    )?;
    // Default class: any downlink traffic not matching a declared slice's
    // subnet (shouldn't normally occur — apply_fiveg_ue_routes only routes
    // declared UE subnets through this interface at all — but the same
    // low-floor/high-ceil/lowest-priority safety net apply_slices gives
    // its own unclassified traffic).
    sh.run(
        "tc",
        &[
            "class",
            "replace",
            "dev",
            host_iface,
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

    for slice in slices {
        let classid = format!("1:{:x}", slice.class_minor);
        sh.run(
            "tc",
            &[
                "class",
                "replace",
                "dev",
                host_iface,
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
        // Classify by destination subnet (s-nssai traffic isn't
        // VLAN-tagged here, unlike apply_slices's flower/vlan_id match).
        sh.run_tolerate_exists(
            "tc",
            &[
                "filter",
                "replace",
                "dev",
                host_iface,
                "parent",
                "1:",
                "pref",
                &format!("{}", slice.class_minor),
                "protocol",
                "ip",
                "u32",
                "match",
                "ip",
                "dst",
                &slice.ue_subnet,
                "flowid",
                &classid,
            ],
        )?;
    }
    Ok(())
}

/// Look up which bridge `host_veth` is currently enslaved to, so this stays
/// correct regardless of the caller's own bridge name (`GatewayPlan.bridge`
/// is always `vxbr0` today, but this function shouldn't assume it) —
/// avoids threading a second parameter through every call site just to
/// duplicate what `ip -json link show` already reports.
fn host_veth_master(sh: &Shell, host_veth: &str) -> Result<String, NetOpsError> {
    let out = sh.json("ip", &["-json", "link", "show", host_veth])?;
    out.as_array()
        .and_then(|a| a.first())
        .and_then(|l| l.get("master"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| NetOpsError::Parse {
            cmdline: format!("ip -json link show {host_veth}"),
            detail: "no `master` field — is this veth actually attached to a bridge?".to_string(),
        })
}

/// Remove the host-side VLAN subinterface [`apply_fiveg_uplink`] created
/// (`<bridge>.<FIVEG_UPLINK_VLAN>`). Idempotent — a missing interface is
/// not an error. Doesn't need `host_veth`: the interface name is fully
/// determined by `bridge` + the fixed uplink VLAN id.
pub fn teardown_fiveg_uplink(sh: &Shell, bridge: &str) -> Result<(), NetOpsError> {
    let vid = crate::fiveg::FIVEG_UPLINK_VLAN;
    sh.run_tolerate_missing("ip", &["link", "del", &format!("{bridge}.{vid}")])
}

/// One data-plane implementation: how a [`GatewayPlan`] is rendered onto a host, removed, and
/// read back.
///
/// The plan is the contract. Everything above this seam — slice ids, VLAN ids, HTB class
/// minors, interface names ([`crate::gateway::ifnames`]) — is decided in planning, so two
/// implementations of this trait provision *the same topology* by different mechanisms. That is
/// the property the phase-2 OVS variant needs: a campaign must be able to swap the data plane
/// without any experiment changing, or the comparison measures two topologies rather than two
/// mechanisms.
///
/// What an implementation owes:
///
/// - **Idempotence, both ways.** `apply` on a live topology is a no-op; `teardown` on an absent
///   one likewise. Both are re-run constantly — `vxedge up` after a `zone down` is the normal
///   path, not an edge case.
/// - **`observe` never mutates**, and reports what is *there*, not what was asked for. It is the
///   reconciler's read side and the honest half of `status`.
/// - **No naming decisions.** Names come from the plan. An implementation that derives an
///   interface name has forked a rule that must have exactly one owner.
///
/// **Known limit**: the 5G attachment helpers (`apply_fiveg_uplink`, `apply_fiveg_ue_routes`,
/// `apply_fiveg_slice_qos`, `teardown_fiveg_uplink`) are still bridge-specific free functions
/// below this seam. They join the trait when a second implementation needs them — inventing
/// their shape now, with no OVS 5G path to check it against, would be guessing at an interface
/// rather than extracting one.
pub trait DataPlane {
    /// The mechanism's name, as it would be written in a manifest or a results caveat.
    fn name(&self) -> &'static str;

    /// Bridge and trunk uplink. Everything else attaches to what this creates.
    fn apply_links(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError>;

    /// Per-slice VLANs on the trunk and the HTB queue tree on the uplink — the shared
    /// bottleneck where slice contention is actually measured.
    fn apply_slices(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError>;

    /// One access port per device attachment, veth or tap per the plan.
    fn apply_endpoints(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError>;

    fn teardown(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError>;

    fn observe(&self, sh: &Shell, plan: &GatewayPlan) -> Result<GatewayObservation, NetOpsError>;

    /// The three stages in dependency order.
    ///
    /// They are separate trait methods rather than one opaque `apply` because the daemon
    /// reports progress between them, and a seam that forces a caller to reach underneath it to
    /// keep a feature is not a seam. The order is part of the contract, not a caller's choice.
    fn apply(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        self.apply_links(sh, plan)?;
        self.apply_slices(sh, plan)?;
        self.apply_endpoints(sh, plan)?;
        Ok(())
    }
}

/// The Linux kernel bridge + VLAN filtering + tc HTB data plane — vxgateway's backend since
/// phase 1, and the one every recorded number was measured on.
pub struct KernelBridge;

impl DataPlane for KernelBridge {
    fn name(&self) -> &'static str {
        "kernel-bridge"
    }

    fn apply_links(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        apply_links(sh, plan)
    }

    fn apply_slices(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        apply_slices(sh, plan)
    }

    fn apply_endpoints(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        apply_endpoints(sh, plan)
    }

    fn teardown(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        teardown(sh, plan)
    }

    fn observe(&self, sh: &Shell, plan: &GatewayPlan) -> Result<GatewayObservation, NetOpsError> {
        observe(sh, plan)
    }
}

/// The active data plane.
///
/// One implementation today, so this takes no argument: a selector with one option is a knob
/// nobody can turn, and guessing whether it belongs on the manifest, the daemon or the plan is
/// the kind of decision that is cheap once a second mechanism exists and expensive to unpick if
/// guessed early. The OVS variant brings its selector with it.
pub fn data_plane() -> &'static dyn DataPlane {
    &KernelBridge
}

#[cfg(test)]
mod seam_tests {
    use super::*;

    /// The seam must not have changed what provisioning does — it dispatches to the same
    /// functions the netns integration test already exercises. This pins the wiring; the live
    /// behaviour is that test's job.
    #[test]
    fn the_active_data_plane_is_the_kernel_bridge() {
        assert_eq!(data_plane().name(), "kernel-bridge");
    }
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
