//! Render a GatewayPlan onto the kernel (apply), remove it (teardown), and
//! observe actual state (the reconciler's "observed" side). All operations
//! are idempotent: creates tolerate "exists", deletes tolerate "missing",
//! and tc uses `replace` semantics throughout.

use super::netlink;
use super::netops::{NetOpsError, Shell};
use super::plan::GatewayPlan;

/// `netlink::NetlinkError` wrapped as a `NetOpsError`, so the in-scope functions below can keep
/// returning the one error type every other `provision.rs` function does — this is where a
/// backend-specific error crosses into the seam's own vocabulary
/// (`service-boundaries`: "an implementation's internal error is wrapped, never leaked").
fn nl(e: netlink::NetlinkError) -> NetOpsError {
    NetOpsError::Command {
        cmdline: "native netlink call".to_string(),
        stderr: e.to_string(),
    }
}

/// HTB minor of the default (unclassified traffic) class.
const DEFAULT_CLASS: &str = "fffe";

/// Phase 1 of apply: bridge + trunk uplink.
pub fn apply_links(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    let ns = sh.netns.as_deref();
    netlink::ensure_bridge(ns, &plan.bridge).map_err(nl)?;
    netlink::link_up(ns, &plan.bridge).map_err(nl)?;

    netlink::ensure_veth_pair(ns, &plan.uplink, &plan.uplink_peer).map_err(nl)?;
    netlink::set_master(ns, &plan.uplink, &plan.bridge).map_err(nl)?;
    netlink::link_up(ns, &plan.uplink).map_err(nl)?;
    netlink::link_up(ns, &plan.uplink_peer).map_err(nl)?;
    Ok(())
}

/// True when our HTB root qdisc (handle 1:) is already installed on `dev`.
fn htb_root_present(sh: &Shell, dev: &str) -> Result<bool, NetOpsError> {
    let out = sh.run("tc", &["qdisc", "show", "dev", dev])?;
    Ok(out.lines().any(|l| l.starts_with("qdisc htb 1:")))
}

/// The HTB queue tree's root and default classes — fabric-agnostic (both `KernelBridge` and
/// `Ovs` shape it identically on the uplink's own kernel qdisc, which exists and behaves the
/// same regardless of which switching fabric the interface is enslaved to). Shared so the two
/// backends cannot drift on the one piece of the tree that isn't backend-specific.
fn ensure_htb_root_and_default(
    sh: &Shell,
    dev: &str,
    root_rate_mbps: u64,
) -> Result<(), NetOpsError> {
    let root_rate = format!("{root_rate_mbps}mbit");
    // HTB root qdiscs don't support in-place change (`replace` fails on an
    // existing one), so create only when absent; classes/filters below use
    // replace semantics and converge on every apply.
    if !htb_root_present(sh, dev)? {
        sh.run(
            "tc",
            &[
                "qdisc",
                "add",
                "dev",
                dev,
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
            "class", "replace", "dev", dev, "parent", "1:", "classid", "1:1", "htb", "rate",
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
            dev,
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
    Ok(())
}

/// One slice's HTB class + the flower filter mapping its VLAN tag onto that class — also
/// fabric-agnostic: `tc flower vlan_id` matches on the 802.1Q tag already present on the wire,
/// which both backends produce identically (a `KernelBridge` trunk port and an OVS trunk port
/// emit the same tagged frames), and the class itself lives on the uplink's own qdisc, not on
/// anything backend-specific.
fn apply_slice_htb_class(
    sh: &Shell,
    dev: &str,
    slice: &super::plan::SlicePlan,
    pref_offset: usize,
) -> Result<(), NetOpsError> {
    let vid = slice.vlan.to_string();
    let classid = format!("1:{:x}", slice.class_minor);
    sh.run(
        "tc",
        &[
            "class",
            "replace",
            "dev",
            dev,
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
    // Flower `replace` without an explicit handle EEXISTs on an identical filter — which is
    // already the converged state, so tolerate it. (Changing a vlan→class mapping in place is
    // reconciler follow-up.)
    sh.run_tolerate_exists(
        "tc",
        &[
            "filter",
            "replace",
            "dev",
            dev,
            "parent",
            "1:",
            "pref",
            &(10 + pref_offset).to_string(),
            "protocol",
            "802.1q",
            "flower",
            "vlan_id",
            &vid,
            "classid",
            &classid,
        ],
    )?;
    Ok(())
}

/// Phase 2 of apply: per-slice VLANs on the trunk + the HTB queue tree.
pub fn apply_slices(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    ensure_htb_root_and_default(sh, &plan.uplink, plan.root_rate_mbps)?;
    for (i, slice) in plan.slices.iter().enumerate() {
        // Trunk carries every slice VLAN tagged.
        netlink::bridge_vlan_add(sh.netns.as_deref(), &plan.uplink, slice.vlan, false)
            .map_err(nl)?;
        apply_slice_htb_class(sh, &plan.uplink, slice, i)?;
    }
    Ok(())
}

/// Phase 3 of apply: zone endpoints as access ports (pvid untagged).
/// Veth for container nodes (peer moves into the node's netns at `zone up`);
/// tap for VM nodes (libvirt/qemu opens it directly).
/// Creates an endpoint's own device (veth pair or tap) if it doesn't exist yet — fabric-agnostic,
/// since a device attachment is the same kernel object whether its access port ends up enslaved
/// to a Linux bridge or added as an OVS port.
fn ensure_endpoint_device(sh: &Shell, ep: &super::plan::EndpointPlan) -> Result<(), NetOpsError> {
    match ep.kind {
        super::plan::EndpointKind::Veth => {
            netlink::ensure_veth_pair(sh.netns.as_deref(), &ep.name, &ep.peer).map_err(nl)
        }
        // `ip tuntap add ... mode tap` is not an `RTM_NEWLINK` operation at all — tun/tap
        // devices are created via `ioctl(TUNSETIFF)` on `/dev/net/tun`, a genuinely different
        // kernel interface than the rest of this port. Deferred (`docs/TASKS.md` #16): not on
        // H1's own path, which is container-backed (`Veth`), not VM-backed.
        super::plan::EndpointKind::Tap => {
            if !sh.link_exists(&ep.name) {
                sh.run("ip", &["tuntap", "add", "dev", &ep.name, "mode", "tap"])?;
            }
            Ok(())
        }
    }
}

pub fn apply_endpoints(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    let ns = sh.netns.as_deref();
    for ep in &plan.endpoints {
        ensure_endpoint_device(sh, ep)?;
        netlink::set_master(ns, &ep.name, &plan.bridge).map_err(nl)?;
        // Access port: node-side traffic is untagged, tagged as the slice
        // VLAN on ingress to the bridge. Drop the default VLAN 1 membership.
        netlink::bridge_vlan_add(ns, &ep.name, ep.vlan, true).map_err(nl)?;
        netlink::bridge_vlan_del(ns, &ep.name, 1).map_err(nl)?;
        netlink::link_up(ns, &ep.name).map_err(nl)?;
        // Bringing the peer up is only meaningful while the peer is still on this side. A
        // running container zone has already taken it into its own netns and renamed it `eth0`
        // (`zones::backend`), so its *absence here* is the converged state, not a failure.
        //
        // Without this guard `up` is not idempotent, which the hard rule forbids: the first
        // `experiment up` provisions the gateway and then starts the zones, and the second one
        // dies on `ip link set vzp0 up: Cannot find device`. Latent until `experiment up` made
        // gateway-then-zones a single command anyone would naturally re-run — found on the
        // first real provisioning run, 2026-07-30.
        if ep.kind == super::plan::EndpointKind::Veth && netlink::link_exists(ns, &ep.peer) {
            netlink::link_up(ns, &ep.peer).map_err(nl)?;
        }
        if let Some(imp) = &ep.impairment {
            apply_impairment(sh, &ep.name, imp)?;
        }
        apply_ingress_filter(sh, ep)?;
    }
    Ok(())
}

/// The netem arguments for one impairment, as an argument vector.
///
/// Built as tokens rather than a command string — every value here came out of a validated
/// manifest, but a provisioning layer that formats shell text is one manifest away from being a
/// command-injection surface, and this codebase does not have one.
///
/// `delay` is emitted with its jitter as a second positional argument, which is netem's syntax;
/// jitter alone is unrepresentable, and is refused in the validator rather than silently dropped
/// here.
fn netem_args(params: &crate::model::Impairment) -> Vec<String> {
    let mut args = vec!["netem".to_string()];
    if let Some(delay_us) = params.delay_us {
        args.push("delay".to_string());
        args.push(format!("{delay_us}us"));
        if let Some(jitter_us) = params.jitter_us {
            args.push(format!("{jitter_us}us"));
        }
    }
    if let Some(ppm) = params.loss_ppm {
        args.push("loss".to_string());
        // Back to the percentage tc speaks, from the ppm the plan compares on. Four decimals is
        // the scale's full resolution, so this round-trips whatever the author wrote.
        args.push(format!("{}.{:04}%", ppm / 10_000, ppm % 10_000));
    }
    if let Some(rate_mbps) = params.rate_mbps {
        args.push("rate".to_string());
        args.push(format!("{rate_mbps}mbit"));
    }
    args
}

/// Render one link's impairment onto its elected access port, in both directions.
///
/// # Why an IFB, and not simply two qdiscs
///
/// A qdisc shapes egress only. On the access port that is bridge→device, which is half a wire.
/// The other half — device→bridge — *ingresses* here, and the port's own root qdisc never sees
/// it. It cannot be shaped from the far end either: for a container that end has already moved
/// into the device's namespace, and for a VM it belongs to the hypervisor, so reaching it would
/// couple gateway provisioning to both zone backends.
///
/// Redirecting ingress onto an IFB turns it into egress on a device this module owns, where the
/// same netem applies. Everything stays on the host, and veth and tap attachments are treated
/// identically.
///
/// # Idempotence
///
/// The two netem qdiscs use `replace`, which converges. The ingress filter cannot: `tc filter
/// add` appends, so re-running would stack redirects and impair inbound traffic once per
/// `up`, and there is no version-portable way to replace an unhandled `u32` filter in place.
/// Reading the existing filter back instead is what this file already avoids for `tc` —
/// `observe`'s own comment records `tc -j class show` emitting non-JSON on shipped iproute2
/// versions. So the ingress qdisc is deleted and rebuilt, which takes its filters with it and
/// lands on exactly one redirect every time. That is convergent rather than untouched: a re-apply
/// leaves a sub-millisecond window where inbound traffic crosses unimpaired. `up` is a
/// provisioning operation and traffic is started separately (`vxedge traffic up`), so no
/// measurement is running when it happens.
fn apply_impairment(
    sh: &Shell,
    access_port: &str,
    imp: &super::plan::EndpointImpairment,
) -> Result<(), NetOpsError> {
    let netem = netem_args(&imp.params);
    let netem: Vec<&str> = netem.iter().map(String::as_str).collect();

    // Egress: bridge → device.
    let mut egress = vec!["qdisc", "replace", "dev", access_port, "root"];
    egress.extend_from_slice(&netem);
    sh.run("tc", &egress)?;

    // Ingress: device → bridge, by way of the IFB.
    if !sh.link_exists(&imp.ifb) {
        sh.run("ip", &["link", "add", &imp.ifb, "type", "ifb"])?;
    }
    sh.run("ip", &["link", "set", &imp.ifb, "up"])?;
    let mut ifb_egress = vec!["qdisc", "replace", "dev", imp.ifb.as_str(), "root"];
    ifb_egress.extend_from_slice(&netem);
    sh.run("tc", &ifb_egress)?;

    sh.run_tolerate_missing("tc", &["qdisc", "del", "dev", access_port, "ingress"])?;
    sh.run("tc", &["qdisc", "add", "dev", access_port, "ingress"])?;
    sh.run(
        "tc",
        &[
            "filter",
            "add",
            "dev",
            access_port,
            "parent",
            "ffff:",
            "protocol",
            "all",
            "prio",
            "1",
            "u32",
            "match",
            "u32",
            "0",
            "0",
            "action",
            "mirred",
            "egress",
            "redirect",
            "dev",
            &imp.ifb,
        ],
    )?;
    Ok(())
}

/// The nftables `netdev` table name for one access port's anti-spoof filter. One table per
/// port, not one shared table for all of them, so tearing down a single endpoint never risks
/// touching another's rule — the same reasoning `ifnames::ingress_ifb` derives a per-port IFB
/// name rather than sharing one.
fn ingress_filter_table(access_port: &str) -> String {
    format!("vxaf-{access_port}")
}

/// Per-access-port ingress anti-spoof filter (`docs/TASKS.md` #16, "nftables per VLAN/slice").
///
/// **What this is not**: a VLAN-filtering bridge already makes cross-slice traffic structurally
/// impossible — a frame tagged for one VLAN cannot reach a port that isn't a member of it, with
/// or without a firewall. Rules blocking "slice A talking to slice B" here would be dead weight,
/// enforcing something the bridge already guarantees.
///
/// **What this is**: nothing stops a compromised device on the *same* slice from spoofing a
/// neighbor's address to borrow its trust — that gap is real regardless of VLAN isolation, and
/// it is what this renders. Only fires for endpoints with a pinned `expected_source`
/// (`model::Endpoint::ip_cidr`); an attachment with no pinned address renders nothing, the same
/// as an unimpaired link renders no netem.
///
/// The `netdev` family's `ingress` hook, not `bridge`, deliberately: it fires on raw packets
/// arriving at a specific interface before any bridging decision, so it works identically
/// whether that interface ends up enslaved to a Linux bridge or added as an OVS port — no
/// `br_netfilter` dependency, and no fabric-specific variant needed (called from both
/// `apply_endpoints` and `ovs_apply_endpoints`, exactly like `apply_impairment`).
///
/// Idempotent by construction rather than by tolerating a specific error: `nft add table/chain`
/// is already a no-op if either exists (nft's own distinction between `add` and `create`), and
/// the chain is flushed before its one rule is (re-)added — `nft add rule` itself is not
/// idempotent (it appends every time, the same lesson `apply_impairment`'s own doc comment
/// already recorded for `tc filter add`), so re-applying without the flush would stack a second
/// identical drop rule per `up`.
fn apply_ingress_filter(sh: &Shell, ep: &super::plan::EndpointPlan) -> Result<(), NetOpsError> {
    let Some(cidr) = &ep.expected_source else {
        return Ok(());
    };
    let host = cidr.split('/').next().unwrap_or(cidr);
    let table = ingress_filter_table(&ep.name);
    sh.run("nft", &["add", "table", "netdev", &table])?;
    sh.run(
        "nft",
        &[
            "add", "chain", "netdev", &table, "ingress", "{", "type", "filter", "hook", "ingress",
            "device", &ep.name, "priority", "0", ";", "policy", "accept", ";", "}",
        ],
    )?;
    sh.run("nft", &["flush", "chain", "netdev", &table, "ingress"])?;
    sh.run(
        "nft",
        &[
            "add", "rule", "netdev", &table, "ingress", "ip", "saddr", "!=", host, "counter",
            "drop",
        ],
    )?;
    Ok(())
}

fn teardown_ingress_filter(sh: &Shell, ep: &super::plan::EndpointPlan) -> Result<(), NetOpsError> {
    if ep.expected_source.is_none() {
        return Ok(());
    }
    let table = ingress_filter_table(&ep.name);
    sh.run_tolerate_missing("nft", &["delete", "table", "netdev", &table])
}

/// One gateway-terminated tunnel (`docs/TASKS.md` #16), applied or torn down identically
/// regardless of `network.data_plane` for both kinds: a WireGuard interface is a routed L3
/// endpoint, never enslaved to the bridge/OVS switch itself; a VXLAN interface *does* enslave
/// to the bridge, but as a trunk-like port the same way the uplink itself is — neither depends
/// on which switching fabric is otherwise in use. Called once, from the `DataPlane` trait's
/// shared default `apply`, and from both backends' `teardown`/`observe`.
fn apply_tunnel(
    sh: &Shell,
    plan: &GatewayPlan,
    t: &super::plan::TunnelPlan,
) -> Result<(), NetOpsError> {
    match t.kind {
        crate::model::TunnelKind::Wireguard => apply_wireguard_tunnel(sh, t),
        crate::model::TunnelKind::Vxlan => apply_vxlan_tunnel(sh, plan, t),
    }
}

/// Idempotency splits in two, deliberately: interface *creation* (and the one-time key
/// generation that goes with it) happens only when `netlink::link_exists` says the tunnel does
/// not exist yet; peer/listen-port configuration below that point is `wg set`'s own
/// declarative, safe-to-repeat form, run on every `apply` regardless. Folding key generation
/// into the declarative half would regenerate this side's identity on every reconcile pass,
/// silently invalidating whatever the peer already trusts — the same class of bug
/// `apply_impairment`'s own doc comment warns about for `tc filter add`, just on the
/// "recreates instead of no-ops" side rather than the "duplicates" side.
///
/// Device creation goes through `netlink::ensure_wireguard` (`docs/TASKS.md` #16, delivered
/// 2026-08-04) — everything after it (`wg genkey`/`wg set`, the address, bringing the link up)
/// stays on `Shell`: `wg`'s own configuration is a generic-netlink protocol `rtnetlink` doesn't
/// implement, not a deferred gap.
fn apply_wireguard_tunnel(sh: &Shell, t: &super::plan::TunnelPlan) -> Result<(), NetOpsError> {
    let ns = sh.netns.as_deref();
    if !netlink::link_exists(ns, &t.name) {
        netlink::ensure_wireguard(ns, &t.name).map_err(nl)?;
        // The private key never touches disk: piped straight from `wg genkey`'s stdout into
        // `wg set`'s stdin. `/dev/stdin`, not the documented `-` shorthand: found live
        // 2026-08-03 that Ubuntu noble's `wireguard-tools` 1.0.20210914 rejects `-` for
        // `private-key` with a literal `fopen: No such file or directory` (it does not special-
        // case that argument the way the man page describes for some other options) — `wg` has
        // no trouble opening `/dev/stdin` as a real path, since it resolves to this process's
        // own piped stdin via `/proc/self/fd/0` either way. Never stored anywhere by this
        // process — `wg show <name> public-key` (used by `observe_tunnels`) is the only way to
        // read it back, which is also how an operator hands this side's identity to the peer,
        // out of band. See `model::Tunnel`'s doc comment for why the manifest never declares a
        // local key.
        let privkey = sh.run("wg", &["genkey"])?;
        sh.run_with_stdin(
            "wg",
            &["set", &t.name, "private-key", "/dev/stdin"],
            privkey.trim(),
        )?;
    }
    let listen_port = t
        .listen_port
        .expect("plan invariant: wireguard tunnels always carry listen_port")
        .to_string();
    let peer_public_key = t
        .peer_public_key
        .as_deref()
        .expect("plan invariant: wireguard tunnels always carry peer_public_key");
    let allowed_ips = t.peer_allowed_ips.join(",");
    let mut args: Vec<&str> = vec![
        "set",
        &t.name,
        "listen-port",
        &listen_port,
        "peer",
        peer_public_key,
    ];
    if !allowed_ips.is_empty() {
        args.push("allowed-ips");
        args.push(&allowed_ips);
    }
    if let Some(endpoint) = &t.peer_endpoint {
        args.push("endpoint");
        args.push(endpoint);
    }
    sh.run("wg", &args)?;

    let address = t
        .address
        .as_deref()
        .expect("plan invariant: wireguard tunnels always carry address");
    sh.run_tolerate_exists("ip", &["addr", "add", address, "dev", &t.name])?;
    sh.run("ip", &["link", "set", &t.name, "up"])?;
    Ok(())
}

/// A VXLAN tunnel joins the bridge as a trunk-like port carrying whatever local slice VLANs it
/// declared (`plan.rs`'s own `vxlan_vlans`, already resolved from `slices` at plan time) —
/// structurally the same shape `apply_slices` already gives the uplink's own trunk membership,
/// not the access-port (`pvid untagged`) shape `apply_endpoints` gives a zone device. Goes
/// straight through `netlink.rs`: VXLAN link creation was never on the iproute2 shell-out to
/// begin with (confirmed supported by the same `rtnetlink` crate this session's own rtnetlink
/// port already vetted and adopted), so this lands already-modernized.
fn apply_vxlan_tunnel(
    sh: &Shell,
    plan: &GatewayPlan,
    t: &super::plan::TunnelPlan,
) -> Result<(), NetOpsError> {
    let ns = sh.netns.as_deref();
    netlink::ensure_vxlan(
        ns,
        &t.name,
        t.vni
            .expect("plan invariant: vxlan tunnels always carry vni"),
        t.remote
            .expect("plan invariant: vxlan tunnels always carry remote"),
        t.dstport
            .expect("plan invariant: vxlan tunnels always carry dstport (defaulted at plan time)"),
    )
    .map_err(nl)?;
    netlink::set_master(ns, &t.name, &plan.bridge).map_err(nl)?;
    for vlan in &t.vxlan_vlans {
        netlink::bridge_vlan_add(ns, &t.name, *vlan, false).map_err(nl)?;
    }
    netlink::link_up(ns, &t.name).map_err(nl)?;
    Ok(())
}

fn apply_tunnels(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    for t in &plan.tunnels {
        apply_tunnel(sh, plan, t)?;
    }
    Ok(())
}

/// Both kinds now tear down through `netlink::delete_link` — link deletion never cared what
/// created the link, so once `apply_wireguard_tunnel` moved its own creation onto
/// `netlink.rs` (`docs/TASKS.md` #16, delivered 2026-08-04) there was no remaining reason to
/// keep WireGuard's delete on the `ip` shell-out; the earlier doc comment here called that a
/// "separate migration decision outside this change's scope" — this change is that migration.
fn teardown_tunnel(sh: &Shell, t: &super::plan::TunnelPlan) -> Result<(), NetOpsError> {
    netlink::delete_link(sh.netns.as_deref(), &t.name).map_err(nl)
}

fn teardown_tunnels(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    for t in &plan.tunnels {
        teardown_tunnel(sh, t)?;
    }
    Ok(())
}

/// One tunnel's observed state (`GatewayObservation.tunnels`).
#[derive(Debug, Clone)]
pub struct TunnelObservation {
    pub id: String,
    pub name: String,
    pub kind: crate::model::TunnelKind,
    pub present: bool,
    /// This side's current public key, read live off the kernel — the only place it is ever
    /// recorded, since `apply_tunnel` never writes it to disk. Empty when the interface is
    /// absent, and always empty for a `Vxlan`-kind tunnel (no key material at all — an
    /// unencrypted L2 extension, the same trust model as a physical inter-switch link).
    pub public_key: String,
    /// Whether the peer has completed a handshake recently. **Not** a `fully_up` condition:
    /// WireGuard is connectionless, so a correctly configured but idle tunnel legitimately has
    /// no handshake yet — this is informational (`status`), the same way an unimpaired link's
    /// tc counters are informational rather than gating. Always `false` for a `Vxlan`-kind
    /// tunnel — VXLAN has no handshake concept at all, not merely an unestablished one.
    pub handshake_established: bool,
}

fn observe_tunnels(sh: &Shell, plan: &GatewayPlan) -> Vec<TunnelObservation> {
    plan.tunnels
        .iter()
        .map(|t| match t.kind {
            crate::model::TunnelKind::Wireguard => observe_wireguard_tunnel(sh, t),
            crate::model::TunnelKind::Vxlan => observe_vxlan_tunnel(sh, t),
        })
        .collect()
}

fn observe_wireguard_tunnel(sh: &Shell, t: &super::plan::TunnelPlan) -> TunnelObservation {
    let present = netlink::link_exists(sh.netns.as_deref(), &t.name);
    let public_key = sh
        .run("wg", &["show", &t.name, "public-key"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let handshake_established = sh
        .run("wg", &["show", &t.name, "latest-handshakes"])
        .map(|s| {
            // `<pubkey>\t<unix-seconds>`; 0 means "never."
            s.split_whitespace()
                .next_back()
                .and_then(|secs| secs.parse::<u64>().ok())
                .is_some_and(|secs| secs > 0)
        })
        .unwrap_or(false);
    TunnelObservation {
        id: t.id.clone(),
        name: t.name.clone(),
        kind: t.kind,
        present,
        public_key,
        handshake_established,
    }
}

/// "Present" means both the link exists *and* carries every declared VLAN — a VXLAN interface
/// up but missing one of the slices it was supposed to extend is not meaningfully "up" any more
/// than a missing interface is. No separate `link_exists` check needed:
/// `netlink::bridge_vlan_list` already returns an empty list for a missing link, which can
/// never contain the non-empty `vxlan_vlans` every successfully-planned vxlan tunnel carries
/// (`tunnels_of`'s own `MissingVxlanTunnelField` guarantees at least one declared slice).
///
/// A subset check, not exact-set equality — found live: a `vlan_filtering` bridge auto-assigns
/// every new port an untagged default membership in VLAN 1 alongside whatever's explicitly
/// added, the same behavior `observe`'s own uplink trunk check already tolerates via
/// `trunk_vlans.contains(&s.vlan)`. This tunnel is a trunk-like port, the same shape as the
/// uplink, so it inherits the same default-VLAN-1 membership and needs the same tolerant check.
fn observe_vxlan_tunnel(sh: &Shell, t: &super::plan::TunnelPlan) -> TunnelObservation {
    let ns = sh.netns.as_deref();
    let actual = netlink::bridge_vlan_list(ns, &t.name).unwrap_or_default();
    let present = t.vxlan_vlans.iter().all(|vlan| actual.contains(vlan));
    TunnelObservation {
        id: t.id.clone(),
        name: t.name.clone(),
        kind: t.kind,
        present,
        public_key: String::new(),
        handshake_established: false,
    }
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
///
/// Ported to `gateway::netlink` 2026-08-04 (`docs/TASKS.md` #16, rtnetlink port slice 4) —
/// every step here (bridge-VLAN membership, the bridge-self membership, the VLAN subinterface,
/// its address, bringing it up) is netlink-portable; only [`apply_fiveg_slice_qos`]'s HTB tree
/// on this same interface stays on `Shell`.
pub fn apply_fiveg_uplink(sh: &Shell, host_veth: &str) -> Result<String, NetOpsError> {
    let ns = sh.netns.as_deref();
    let vid = crate::fiveg::FIVEG_UPLINK_VLAN;
    netlink::bridge_vlan_add(ns, host_veth, vid, true).map_err(nl)?;
    netlink::bridge_vlan_del(ns, host_veth, 1).map_err(nl)?;

    let bridge = host_veth_master(sh, host_veth)?;
    netlink::bridge_vlan_add_self(ns, &bridge, vid).map_err(nl)?;
    let host_iface = format!("{bridge}.{vid}");
    netlink::ensure_vlan_subif(ns, &host_iface, &bridge, vid).map_err(nl)?;
    let host_addr = format!("{}/24", crate::fiveg::BRIDGE_ATTACH_HOST_ADDR);
    netlink::add_address(ns, &host_iface, &host_addr).map_err(nl)?;
    netlink::link_up(ns, &host_iface).map_err(nl)?;
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
    let ns = sh.netns.as_deref();
    for subnet in ue_subnets {
        netlink::add_route(ns, subnet, crate::fiveg::BRIDGE_ATTACH_CN_ADDR, host_iface)
            .map_err(nl)?;
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
    netlink::link_master(sh.netns.as_deref(), host_veth)
        .map_err(nl)?
        .ok_or_else(|| NetOpsError::Parse {
            cmdline: format!("netlink link_master({host_veth})"),
            detail: "no bridge master — is this veth actually attached to a bridge?".to_string(),
        })
}

/// Remove the host-side VLAN subinterface [`apply_fiveg_uplink`] created
/// (`<bridge>.<FIVEG_UPLINK_VLAN>`). Idempotent — a missing interface is
/// not an error. Doesn't need `host_veth`: the interface name is fully
/// determined by `bridge` + the fixed uplink VLAN id.
pub fn teardown_fiveg_uplink(sh: &Shell, bridge: &str) -> Result<(), NetOpsError> {
    let vid = crate::fiveg::FIVEG_UPLINK_VLAN;
    netlink::delete_link(sh.netns.as_deref(), &format!("{bridge}.{vid}")).map_err(nl)
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

    /// WireGuard tunnels the gateway terminates (`docs/TASKS.md` #16). Given a default
    /// implementation here, not per-backend: a tunnel is a routed L3 endpoint independent of
    /// which fabric backs the VLAN slices, so `KernelBridge` and `Ovs` share one implementation
    /// rather than each redeclaring it — see `apply_tunnels`'s own doc comment.
    fn apply_tunnels(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        apply_tunnels(sh, plan)
    }

    /// The four stages in dependency order.
    ///
    /// They are separate trait methods rather than one opaque `apply` because the daemon
    /// reports progress between them, and a seam that forces a caller to reach underneath it to
    /// keep a feature is not a seam. The order is part of the contract, not a caller's choice.
    fn apply(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        self.apply_links(sh, plan)?;
        self.apply_slices(sh, plan)?;
        self.apply_endpoints(sh, plan)?;
        self.apply_tunnels(sh, plan)?;
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

// ---------------------------------------------------------------------------
// OVS/OVSDB data plane
// ---------------------------------------------------------------------------

/// Bridge + trunk uplink, via `ovs-vsctl` instead of `ip link add ... type bridge`. The veth
/// pair itself is still plain `ip` — OVS attaches to netdevices, it doesn't create them — and
/// `--may-exist` is `ovs-vsctl`'s own idempotent-create idiom, the direct parallel to
/// `link_exists` guards elsewhere in this file.
fn ovs_apply_links(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    sh.run("ovs-vsctl", &["--may-exist", "add-br", &plan.bridge])?;
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
    sh.run(
        "ovs-vsctl",
        &["--may-exist", "add-port", &plan.bridge, &plan.uplink],
    )?;
    sh.run("ip", &["link", "set", &plan.uplink, "up"])?;
    sh.run("ip", &["link", "set", &plan.uplink_peer, "up"])?;
    Ok(())
}

/// Per-slice VLANs on the trunk (OVS's own `trunks` port column) + the same HTB queue tree the
/// kernel bridge builds — `ensure_htb_root_and_default`/`apply_slice_htb_class` don't know or
/// care which switching fabric the uplink is attached to, because they act on the uplink's own
/// kernel qdisc, which exists either way.
fn ovs_apply_slices(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    ensure_htb_root_and_default(sh, &plan.uplink, plan.root_rate_mbps)?;
    // A `set`, not an `add`: OVS's trunk list is one column holding the whole set, so the
    // converged value is assigned outright on every apply — idempotent by construction, unlike
    // the Linux bridge's per-vid `bridge vlan add`, which needs `tolerate_exists` because it's
    // additive. An empty plan (no vlan slices — e.g. s-nssai-only) sets `trunks=[]`, correctly
    // carrying nothing tagged.
    let trunks = plan
        .slices
        .iter()
        .map(|s| s.vlan.to_string())
        .collect::<Vec<_>>()
        .join(",");
    sh.run(
        "ovs-vsctl",
        &["set", "port", &plan.uplink, &format!("trunks=[{trunks}]")],
    )?;
    for (i, slice) in plan.slices.iter().enumerate() {
        apply_slice_htb_class(sh, &plan.uplink, slice, i)?;
    }
    Ok(())
}

/// One access port per device attachment. `add-port`'s trailing `tag=<vid>` sets the column at
/// creation, but `--may-exist` skips column assignment on an *existing* port — so `tag` is set
/// again, unconditionally, as a converging step of its own, the same reason `apply_slices`
/// above uses `set` rather than trusting `add-port`'s one-shot value to stay correct forever.
fn ovs_apply_endpoints(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    for ep in &plan.endpoints {
        ensure_endpoint_device(sh, ep)?;
        sh.run(
            "ovs-vsctl",
            &["--may-exist", "add-port", &plan.bridge, &ep.name],
        )?;
        sh.run(
            "ovs-vsctl",
            &["set", "port", &ep.name, &format!("tag={}", ep.vlan)],
        )?;
        sh.run("ip", &["link", "set", &ep.name, "up"])?;
        // Same idempotency guard as the kernel bridge's own apply_endpoints: a running
        // container zone has already moved the peer into its own netns, so its absence here is
        // the converged state, not a failure.
        if ep.kind == super::plan::EndpointKind::Veth && sh.link_exists(&ep.peer) {
            sh.run("ip", &["link", "set", &ep.peer, "up"])?;
        }
        if let Some(imp) = &ep.impairment {
            apply_impairment(sh, &ep.name, imp)?;
        }
        apply_ingress_filter(sh, ep)?;
    }
    Ok(())
}

/// Removes everything an OVS-backed plan created. `--if-exists` is `ovs-vsctl`'s own
/// tolerate-missing idiom (confirmed idempotent on a second call against an already-removed
/// port/bridge), so unlike [`teardown`] this needs no `run_tolerate_missing` wrapper around the
/// `ovs-vsctl` calls themselves — only around the underlying `ip link del`, which still errors
/// on an absent device the normal way.
fn ovs_teardown(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    for ep in &plan.endpoints {
        if let Some(imp) = &ep.impairment {
            sh.run_tolerate_missing("ip", &["link", "del", &imp.ifb])?;
        }
        teardown_ingress_filter(sh, ep)?;
        sh.run(
            "ovs-vsctl",
            &["--if-exists", "del-port", &plan.bridge, &ep.name],
        )?;
        sh.run_tolerate_missing("ip", &["link", "del", &ep.name])?;
    }
    teardown_tunnels(sh, plan)?;
    sh.run(
        "ovs-vsctl",
        &["--if-exists", "del-port", &plan.bridge, &plan.uplink],
    )?;
    sh.run_tolerate_missing("ip", &["link", "del", &plan.uplink])?;
    sh.run("ovs-vsctl", &["--if-exists", "del-br", &plan.bridge])?;
    Ok(())
}

/// Observe OVS state against the plan — the same shape [`observe`] reports, read through
/// `ovs-vsctl` instead of `ip`/`bridge`. `br-exists` exits 2 for an absent bridge (checked here
/// the same way `link_exists` checks `ip -j link show`'s exit code, not its output). The trunk
/// list happens to already be valid JSON in OVSDB's own text notation (`[101, 103]`, `[]`), so
/// [`Shell::json`] parses it directly with no `--format=json` needed — confirmed live rather
/// than assumed, since guessing wrong here would silently under- or over-report trunk state.
fn ovs_observe(sh: &Shell, plan: &GatewayPlan) -> Result<GatewayObservation, NetOpsError> {
    let bridge_present = sh.run("ovs-vsctl", &["br-exists", &plan.bridge]).is_ok();
    let uplink_present = bridge_present && sh.link_exists(&plan.uplink);

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

    let trunk_vlans: Vec<u16> = if uplink_present {
        sh.json("ovs-vsctl", &["get", "port", &plan.uplink, "trunks"])?
            .as_array()
            .map(|vs| {
                vs.iter()
                    .filter_map(|v| v.as_u64())
                    .map(|v| v as u16)
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

    let bridge_ports: Vec<String> = if bridge_present {
        sh.run("ovs-vsctl", &["list-ports", &plan.bridge])?
            .lines()
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    let endpoints_present = plan
        .endpoints
        .iter()
        .filter(|ep| bridge_ports.iter().any(|p| p == &ep.name) && sh.link_exists(&ep.name))
        .count();

    let mut impairments = Vec::new();
    for ep in &plan.endpoints {
        let Some(imp) = &ep.impairment else { continue };
        let egress_present = sh.link_exists(&ep.name)
            && sh
                .run("tc", &["qdisc", "show", "dev", &ep.name])?
                .lines()
                .any(|l| l.starts_with("qdisc netem "));
        let ingress_present = sh.link_exists(&imp.ifb)
            && sh
                .run("tc", &["qdisc", "show", "dev", &imp.ifb])?
                .lines()
                .any(|l| l.starts_with("qdisc netem "));
        impairments.push(ImpairmentObservation {
            link: imp.link.clone(),
            access_port: ep.name.clone(),
            egress_present,
            ingress_present,
        });
    }

    let mut ingress_filters = Vec::new();
    for ep in &plan.endpoints {
        if ep.expected_source.is_none() {
            continue;
        }
        let table = ingress_filter_table(&ep.name);
        let present = sh.run("nft", &["list", "table", "netdev", &table]).is_ok();
        ingress_filters.push(IngressFilterObservation {
            node: ep.node.clone(),
            access_port: ep.name.clone(),
            present,
        });
    }

    Ok(GatewayObservation {
        bridge_present,
        uplink_present,
        slices,
        endpoints_present,
        endpoints_expected: plan.endpoints.len(),
        impairments,
        ingress_filters,
        tunnels: observe_tunnels(sh, plan),
    })
}

/// The OVS/OVSDB data plane — a second implementation behind the same [`GatewayPlan`], selected
/// by `network.data_plane: ovs`. The kernel bridge remains the default and the one every frozen
/// H1/H3/H4 number was measured on; this exists so a campaign can compare mechanisms without
/// the experiment itself changing (`network-provisioning-patterns` skill). Reuses the tc-based
/// HTB tree, per-slice class/filter and impairment/netem code unchanged — none of it depends on
/// which switching fabric the interface underneath it belongs to.
pub struct Ovs;

impl DataPlane for Ovs {
    fn name(&self) -> &'static str {
        "ovs"
    }

    fn apply_links(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        ovs_apply_links(sh, plan)
    }

    fn apply_slices(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        ovs_apply_slices(sh, plan)
    }

    fn apply_endpoints(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        ovs_apply_endpoints(sh, plan)
    }

    fn teardown(&self, sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
        ovs_teardown(sh, plan)
    }

    fn observe(&self, sh: &Shell, plan: &GatewayPlan) -> Result<GatewayObservation, NetOpsError> {
        ovs_observe(sh, plan)
    }
}

/// The data plane a plan selects (`GatewayPlan::data_plane`, from `network.data_plane` — absent
/// means [`crate::model::DataPlaneKind::KernelBridge`], so every experiment that predates this
/// field keeps selecting the exact mechanism it always has).
pub fn data_plane(kind: crate::model::DataPlaneKind) -> &'static dyn DataPlane {
    match kind {
        crate::model::DataPlaneKind::KernelBridge => &KernelBridge,
        crate::model::DataPlaneKind::Ovs => &Ovs,
    }
}

#[cfg(test)]
mod seam_tests {
    use super::*;
    use crate::model::Impairment;

    /// The seam must not have changed what provisioning does — it dispatches to the same
    /// functions the netns integration test already exercises. This pins the wiring; the live
    /// behaviour is that test's job.
    #[test]
    fn kernel_bridge_is_the_default_selection() {
        assert_eq!(
            data_plane(crate::model::DataPlaneKind::KernelBridge).name(),
            "kernel-bridge"
        );
    }

    #[test]
    fn ovs_is_reachable_through_the_same_selector() {
        assert_eq!(data_plane(crate::model::DataPlaneKind::Ovs).name(), "ovs");
    }

    /// The exact tokens handed to `tc`. Worth pinning as a unit test because the netns test can
    /// only report that netem installed, not that it installed the numbers the manifest asked
    /// for — a units slip (`250` read as milliseconds rather than microseconds) would pass every
    /// live check and misstate every result by a factor of a thousand.
    #[test]
    fn netem_renders_the_declared_units() {
        assert_eq!(
            netem_args(&Impairment {
                rate_mbps: Some(100),
                delay_us: Some(250),
                jitter_us: Some(40),
                loss_ppm: Some(100),
            }),
            [
                "netem", "delay", "250us", "40us", "loss", "0.0100%", "rate", "100mbit"
            ]
        );
    }

    /// Absent fields are absent from the command, not defaulted: netem's own defaults are "do
    /// nothing", and a rate cap nobody asked for would silently bound every measurement.
    #[test]
    fn an_unset_field_contributes_no_argument() {
        assert_eq!(
            netem_args(&Impairment {
                rate_mbps: Some(1000),
                ..Default::default()
            }),
            ["netem", "rate", "1000mbit"]
        );
        assert_eq!(
            netem_args(&Impairment {
                delay_us: Some(5),
                ..Default::default()
            }),
            ["netem", "delay", "5us"]
        );
    }

    /// The ppm scale's endpoints, in the percentages `tc` is given. Loss is the one value that
    /// changes representation between the manifest and the plan, so both ends of its range are
    /// checked rather than assumed.
    #[test]
    fn loss_round_trips_across_the_ppm_scale() {
        let rendered = |ppm| {
            netem_args(&Impairment {
                loss_ppm: Some(ppm),
                ..Default::default()
            })[2]
                .clone()
        };
        assert_eq!(rendered(1), "0.0001%");
        assert_eq!(rendered(100), "0.0100%");
        assert_eq!(rendered(10_000), "1.0000%");
        assert_eq!(rendered(1_000_000), "100.0000%");
    }
}

/// Full apply, in dependency order.
pub fn apply(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    apply_links(sh, plan)?;
    apply_slices(sh, plan)?;
    apply_endpoints(sh, plan)?;
    apply_tunnels(sh, plan)?;
    Ok(())
}

/// Remove everything the plan created. Deleting a veth removes its peer;
/// deleting the bridge detaches remaining ports. Tolerates absence.
pub fn teardown(sh: &Shell, plan: &GatewayPlan) -> Result<(), NetOpsError> {
    let ns = sh.netns.as_deref();
    for ep in &plan.endpoints {
        // The IFB is a link of its own, so deleting the access port does not take it — only its
        // own qdiscs and the ingress redirect pointing at it. Left behind, it would survive
        // `down` and be silently reused by the next `up`, which is how a stale netem outlives
        // the manifest that asked for it.
        if let Some(imp) = &ep.impairment {
            netlink::delete_link(ns, &imp.ifb).map_err(nl)?;
        }
        teardown_ingress_filter(sh, ep)?;
        netlink::delete_link(ns, &ep.name).map_err(nl)?;
    }
    teardown_tunnels(sh, plan)?;
    netlink::delete_link(ns, &plan.uplink).map_err(nl)?;
    netlink::delete_link(ns, &plan.bridge).map_err(nl)?;
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

/// One link's impairment, as it is actually installed.
#[derive(Debug, Clone)]
pub struct ImpairmentObservation {
    pub link: String,
    pub access_port: String,
    /// netem is the root qdisc on the access port — the bridge→device direction.
    pub egress_present: bool,
    /// The IFB exists and carries its own netem — the device→bridge direction.
    pub ingress_present: bool,
}

impl ImpairmentObservation {
    /// Both directions, because one of them alone is an asymmetric wire nobody asked for and
    /// nothing in the results would reveal.
    pub fn healthy(&self) -> bool {
        self.egress_present && self.ingress_present
    }
}

/// One endpoint's ingress anti-spoof filter, as it is actually installed (`docs/TASKS.md` #16).
#[derive(Debug, Clone)]
pub struct IngressFilterObservation {
    pub node: String,
    pub access_port: String,
    /// The `netdev` table (and therefore its chain and drop rule — a single `nft delete table`
    /// removes all three) exists on the kernel.
    pub present: bool,
}

#[derive(Debug, Clone)]
pub struct GatewayObservation {
    pub bridge_present: bool,
    pub uplink_present: bool,
    pub slices: Vec<SliceObservation>,
    pub endpoints_present: usize,
    pub endpoints_expected: usize,
    pub impairments: Vec<ImpairmentObservation>,
    pub ingress_filters: Vec<IngressFilterObservation>,
    pub tunnels: Vec<TunnelObservation>,
}

impl GatewayObservation {
    pub fn fully_up(&self) -> bool {
        self.bridge_present
            && self.uplink_present
            && self.endpoints_present == self.endpoints_expected
            && self.slices.iter().all(|s| s.healthy())
            // An impairment that failed to install turns a 100BASE-T1 run into a clean-wire run
            // that still produces a full set of plausible numbers. `Up` verifies observed against
            // planned and fails when they differ, and this squarely counts as a difference.
            && self.impairments.iter().all(|i| i.healthy())
            // A declared anti-spoof filter that silently failed to install is worse than a
            // missing impairment: it's a security control reporting healthy while doing
            // nothing. Same treatment, for the stronger reason.
            && self.ingress_filters.iter().all(|f| f.present)
            // A declared tunnel must exist and be configured — but *not* have a handshake
            // (`TunnelObservation::handshake_established` is deliberately excluded here, see its
            // own doc comment): a correctly configured, idle WireGuard link is fully up.
            && self.tunnels.iter().all(|t| t.present)
    }
}

/// Observe kernel state against the plan (the reconciler's read side).
pub fn observe(sh: &Shell, plan: &GatewayPlan) -> Result<GatewayObservation, NetOpsError> {
    let ns = sh.netns.as_deref();
    let bridge_present = netlink::link_exists(ns, &plan.bridge);
    let uplink_present = netlink::link_exists(ns, &plan.uplink);

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

    let trunk_vlans: Vec<u16> = if uplink_present {
        netlink::bridge_vlan_list(ns, &plan.uplink).map_err(nl)?
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
        .filter(|ep| netlink::link_exists(ns, &ep.name))
        .count();

    let mut impairments = Vec::new();
    for ep in &plan.endpoints {
        let Some(imp) = &ep.impairment else { continue };
        // Text, not `-j`: this file's own note on `tc -j class show` emitting non-JSON on
        // shipped iproute2 versions applies to qdiscs too, and `htb_root_present` above already
        // reads qdiscs this way. What is checked is that netem is *there*, not that its
        // parameters match — a full parameter diff belongs with the converging reconciler.
        let egress_present = netlink::link_exists(ns, &ep.name)
            && sh
                .run("tc", &["qdisc", "show", "dev", &ep.name])?
                .lines()
                .any(|l| l.starts_with("qdisc netem "));
        let ingress_present = netlink::link_exists(ns, &imp.ifb)
            && sh
                .run("tc", &["qdisc", "show", "dev", &imp.ifb])?
                .lines()
                .any(|l| l.starts_with("qdisc netem "));
        impairments.push(ImpairmentObservation {
            link: imp.link.clone(),
            access_port: ep.name.clone(),
            egress_present,
            ingress_present,
        });
    }

    let mut ingress_filters = Vec::new();
    for ep in &plan.endpoints {
        if ep.expected_source.is_none() {
            continue;
        }
        let table = ingress_filter_table(&ep.name);
        let present = sh.run("nft", &["list", "table", "netdev", &table]).is_ok();
        ingress_filters.push(IngressFilterObservation {
            node: ep.node.clone(),
            access_port: ep.name.clone(),
            present,
        });
    }

    Ok(GatewayObservation {
        bridge_present,
        uplink_present,
        slices,
        endpoints_present,
        endpoints_expected: plan.endpoints.len(),
        impairments,
        ingress_filters,
        tunnels: observe_tunnels(sh, plan),
    })
}
