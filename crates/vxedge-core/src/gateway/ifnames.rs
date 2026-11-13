//! Host interface names for a device's slice attachments, derived from the device id and the
//! run's instance slot ([`Instance`], `ARCHITECTURE.md` §6.0.5).
//!
//! # Why this module exists
//!
//! Endpoint names used to be **positional** — `vz0`, `vz1`, … assigned in traversal order — and
//! campaign scripts address those names directly (`ip addr replace … dev vzp0`, `ifname=vz3`).
//! That made traversal order load-bearing in the worst possible way: adding a device, or merely
//! feeding the planner a differently-ordered view of the *same* topology, renumbered the rest.
//! exp-002 declares its arms `cont, microvm, libvirt` and sorts to `cont, libvirt, microvm`, so
//! `vz1`/`vz2` swapped between the two schema paths — the campaign would have measured libvirt
//! and reported microvm, passing every test and every validation on the way.
//!
//! Names here are a **pure function of the attachment** (device id, port). Nothing else about
//! the experiment can move them: not declaration order, not sorting, not how many other devices
//! exist, not whether one is added or removed. Traversal order stops being load-bearing because
//! nothing is derived from it any more.
//!
//! # The length budget
//!
//! Linux caps an interface name at `IFNAMSIZ - 1` = 15 bytes, and a veth pair needs *two* names
//! from the same stem (`vz<s>-` for the access port, `vp<s>-` for its peer, where `<s>` is the
//! run's slot), so the stem gets 11 characters.
//! Real device ids are longer than that (`node-adas-probe-01` is 18), so shortening has to be
//! part of the scheme rather than an error — the ids in the frozen manifests are the ones the
//! results are recorded against, and renaming them to fit a kernel constant would be the tail
//! wagging the dog.
//!
//! A shortened stem keeps a readable head and a hash of the **whole** id, so it stays a pure
//! function of that id alone. The mapping is never left implicit: `GatewayPlan` carries the
//! device id on every endpoint, `NodePlan` joins on it, and `vxedge zone list` prints each
//! device's `port=` and `peer=` — so "which interface is this arm" is always answerable without
//! recomputing a hash by hand.

use crate::instance::Instance;

/// `IFNAMSIZ` is 16 bytes *including* the NUL terminator.
pub const IFNAME_MAX: usize = 15;

/// Bridge access port: the veth host side, or the tap itself. `vz<s>-`.
pub const ACCESS_LEAD: &str = "vz";
/// Veth peer, the end that moves into the device's namespace. `vp<s>-`.
const PEER_LEAD: &str = "vp";
/// Ingress-impairment IFB, the device the access port's *inbound* traffic is redirected onto.
/// `vi<s>-`.
const IFB_LEAD: &str = "vi";

/// The prefix a two-letter lead makes with a slot: `vz` and slot `0` make `vz0-`.
fn prefix(lead: &str, instance: Instance) -> String {
    format!("{lead}{instance}-")
}

/// Every prefix here is a two-letter lead, the slot and a dash, so the three names of an
/// attachment share one budget: the stem gets what the 15 bytes leave.
const PREFIX_LEN: usize = 4;
const STEM_MAX: usize = IFNAME_MAX - PREFIX_LEN;

/// The port name elided from a stem.
///
/// Every attachment in every manifest to date is `eth0`, and spelling it would spend 5 of the 11
/// stem characters saying what is true of practically all of them. It is elided rather than
/// dropped: a device attached on `eth1` as well keeps both attachments distinguishable, and the
/// rule is still a pure function of (device, port) — no rule of the form "…only when the device
/// has more than one" is used, because that would make a device's own interface name depend on
/// the rest of its link list.
const PRIMARY_PORT: &str = "eth0";

/// FNV-1a, 32-bit. Explicit rather than `DefaultHasher` because `DefaultHasher`'s output is
/// documented as unstable across releases, and an interface name that changes when the toolchain
/// moves would be a reproducibility bug in a testbed whose whole claim is reproducibility.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Keep `[a-z0-9-]`, fold everything else to `-`. Interface names have no business carrying a
/// `:`, a `/` or a space, and ids are lowercased so two ids differing only in case cannot
/// produce one name silently — they collide, and the planner reports it.
fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Shortens `key` to fit `budget` bytes, keeping a readable head plus a hash of the whole
/// original key — shared by every prefix in this module (`stem`, `wireguard_tunnel`) so the
/// shortening rule can only drift in one place, not silently between them.
fn shorten_to_budget(key: String, budget: usize) -> String {
    if key.len() <= budget {
        return key;
    }
    // ASCII by construction after `sanitize`, so slicing on a byte index is safe.
    let head = key[..budget - 5].trim_end_matches('-');
    format!("{head}-{:04x}", fnv1a(key.as_bytes()) as u16)
}

/// The shared stem of an attachment's two interface names.
///
/// Deterministic, and depends on nothing but its arguments — that is the whole property this
/// module exists to provide.
pub fn stem(device: &str, port: Option<&str>) -> String {
    let mut key = sanitize(device);
    match port {
        Some(p) if !p.eq_ignore_ascii_case(PRIMARY_PORT) => {
            key.push('-');
            key.push_str(&sanitize(p));
        }
        _ => {}
    }
    if key.is_empty() {
        // Unreachable from a validated workspace (ids are non-empty map keys), but a stem is
        // about to become a kernel object name, so it is never allowed to be empty.
        key.push_str("dev");
    }
    shorten_to_budget(key, STEM_MAX)
}

/// Gateway-terminated WireGuard tunnel interface (`docs/ROADMAP.md` #16, "WireGuard link
/// setup"), derived from the tunnel's own id — the same pure-function contract as
/// [`access_port`]: nothing but the id itself (declaration order, tunnel count) can move it.
/// Its own prefix and budget, not `stem`'s: a tunnel has no `(device, port)` pair, only one id,
/// and no peer name to share a budget with the way `vz<s>-`/`vp<s>-` do.
const WIREGUARD_LEAD: &str = "vxwg";
/// `vxwg<s>-`: the lead, the slot and a dash.
const TUNNEL_STEM_MAX: usize = IFNAME_MAX - 6;

pub fn wireguard_tunnel(instance: Instance, id: &str) -> String {
    let key = sanitize(id);
    let key = if key.is_empty() {
        "tun".to_string()
    } else {
        key
    };
    format!(
        "{}{}",
        prefix(WIREGUARD_LEAD, instance),
        shorten_to_budget(key, TUNNEL_STEM_MAX)
    )
}

/// Gateway-side VXLAN tunnel interface (`docs/ROADMAP.md` #16, VXLAN provisioning), derived from
/// the tunnel's own id — same pure-function contract, own prefix and budget, for the same
/// reason [`wireguard_tunnel`]'s own doc comment gives.
const VXLAN_LEAD: &str = "vxln";

pub fn vxlan_tunnel(instance: Instance, id: &str) -> String {
    let key = sanitize(id);
    let key = if key.is_empty() {
        "tun".to_string()
    } else {
        key
    };
    format!(
        "{}{}",
        prefix(VXLAN_LEAD, instance),
        shorten_to_budget(key, TUNNEL_STEM_MAX)
    )
}

/// The bridge access port for an attachment (veth host side, or the tap itself).
pub fn access_port(instance: Instance, device: &str, port: Option<&str>) -> String {
    format!("{}{}", prefix(ACCESS_LEAD, instance), stem(device, port))
}

/// The veth peer for an attachment — the end that moves into the device's namespace. Taps have
/// no peer: the hypervisor opens the access port directly, and the plan sets `peer == name`.
pub fn veth_peer(instance: Instance, device: &str, port: Option<&str>) -> String {
    format!("{}{}", prefix(PEER_LEAD, instance), stem(device, port))
}

/// The IFB device carrying an access port's *ingress* impairment.
///
/// A qdisc only shapes egress, so an access port's own netem impairs bridge→device and nothing
/// else. The device→bridge direction is redirected onto this IFB, where a second, identical
/// netem gives the segment the same characteristics both ways — which is what a length of
/// 100BASE-T1 actually has.
///
/// Taken from the access port rather than re-derived from `(device, port)` on purpose: the two
/// names must share a stem exactly, and reading it off the name the planner already committed to
/// makes that structural instead of a rule two call sites have to keep agreeing on. A name that
/// did not come from [`access_port`] keeps its own text and would overrun `IFNAMSIZ`, which the
/// kernel refuses loudly — but no such name reaches here: the plan carries only what
/// [`access_port`] produced.
/// The slot stays the access port's, so the IFB is in the same run as its port.
pub fn ingress_ifb(access_port: &str) -> String {
    let rest = access_port.strip_prefix(ACCESS_LEAD).unwrap_or(access_port);
    format!("{IFB_LEAD}{rest}")
}

/// The slot an access port name carries: `Some(Some(slot))` for `vz<s>-…`, `Some(None)` for a
/// `vz` port that carries no slot (a name from before slots), and `None` for any other link.
pub fn access_port_instance(name: &str) -> Option<Option<Instance>> {
    let rest = name.strip_prefix(ACCESS_LEAD)?;
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some('-'), _) => Some(None),
        (Some(c), Some('-')) => Instance::from_char(c).map(Some),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const I0: Instance = Instance::FIRST;

    /// The property the positional scheme could not have: a name depends on its own device and
    /// nothing else. Adding, removing or reordering devices cannot move it.
    #[test]
    fn a_name_depends_on_its_device_alone() {
        let before = access_port(I0, "node-b", Some("eth0"));
        // Whatever else the experiment grows, `node-b` keeps its interface.
        for neighbour in ["node-a", "aaa", "zzz", "node-b-extra"] {
            assert_ne!(access_port(I0, neighbour, Some("eth0")), before);
        }
        assert_eq!(access_port(I0, "node-b", Some("eth0")), before);
    }

    #[test]
    fn short_ids_are_spelled_out() {
        assert_eq!(access_port(I0, "node-a", Some("eth0")), "vz0-node-a");
        assert_eq!(veth_peer(I0, "node-a", Some("eth0")), "vp0-node-a");
        assert_eq!(access_port(I0, "zonefl", None), "vz0-zonefl");
        // Exactly at the budget.
        assert_eq!(
            access_port(I0, "node-cont-1", Some("eth0")),
            "vz0-node-cont-1"
        );
    }

    /// The frozen manifests' ids are all longer than the budget, so this path is the common one,
    /// not the exception.
    #[test]
    fn long_ids_shorten_deterministically_and_stay_within_ifnamsiz() {
        for id in [
            "node-adas-probe-01",
            "node-infot-load-01",
            "node-cont-01",
            "node-microvm-01",
            "node-libvirt-01",
            "node-mvm-mmio-01",
            "node-q35-vhost-01",
        ] {
            let name = access_port(I0, id, Some("eth0"));
            let peer = veth_peer(I0, id, Some("eth0"));
            let ifb = ingress_ifb(&name);
            assert!(name.len() <= IFNAME_MAX, "{name} is {} bytes", name.len());
            assert!(peer.len() <= IFNAME_MAX, "{peer} is {} bytes", peer.len());
            assert!(ifb.len() <= IFNAME_MAX, "{ifb} is {} bytes", ifb.len());
            assert_eq!(name, access_port(I0, id, Some("eth0")), "not deterministic");
        }
        // Pinned, because these are the names campaign scripts and results metadata carry: a
        // silent change here is a silent change to which interface a run measured.
        assert_eq!(
            access_port(I0, "node-adas-probe-01", Some("eth0")),
            "vz0-node-a-c06a"
        );
        assert_eq!(
            veth_peer(I0, "node-infot-load-01", Some("eth0")),
            "vp0-node-i-2921"
        );
    }

    /// Ids sharing a head are what makes the hash necessary — `node-mvm-mmio-01` and
    /// `node-mvm-pci-01` are two arms of the same experiment.
    #[test]
    fn ids_sharing_a_head_do_not_share_a_name() {
        let a = access_port(I0, "node-mvm-mmio-01", Some("eth0"));
        let b = access_port(I0, "node-mvm-pci-01", Some("eth0"));
        assert_ne!(a, b, "two arms would have shared one interface");
    }

    /// A second attachment on the same device has to be nameable, and naming it must not rename
    /// the first.
    #[test]
    fn a_non_primary_port_extends_the_stem_without_moving_the_primary_one() {
        assert_eq!(access_port(I0, "gw1", Some("eth0")), "vz0-gw1");
        assert_eq!(access_port(I0, "gw1", None), "vz0-gw1");
        assert_eq!(access_port(I0, "gw1", Some("eth1")), "vz0-gw1-eth1");
        assert_ne!(
            access_port(I0, "gw1", Some("eth1")),
            access_port(I0, "gw1", None)
        );
    }

    /// An attachment's three names are distinct but share a stem, so `vz<s>-`/`vp<s>-`/`vi<s>-` read as
    /// one set. Distinctness is not cosmetic: the IFB is created as a real link, and a name
    /// clashing with the port it serves would delete that port on teardown.
    #[test]
    fn the_ingress_ifb_shares_the_stem_but_not_the_name() {
        for id in ["node-a", "node-adas-probe-01"] {
            let port = access_port(I0, id, Some("eth0"));
            let peer = veth_peer(I0, id, Some("eth0"));
            let ifb = ingress_ifb(&port);
            assert_eq!(ifb, format!("vi0-{}", stem(id, Some("eth0"))));
            assert_ne!(ifb, port);
            assert_ne!(ifb, peer);
        }
    }

    #[test]
    fn characters_the_kernel_will_not_take_are_folded_out() {
        let name = access_port(I0, "node/a b:c", Some("eth0"));
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{name}"
        );
        assert!(name.len() <= IFNAME_MAX);
    }

    #[test]
    fn wireguard_tunnel_names_are_deterministic_and_fit_ifnamsiz() {
        // A tunnel's stem is 9 characters: the slot takes the byte the old prefix left free.
        assert_eq!(wireguard_tunnel(I0, "ota-upl"), "vxwg0-ota-upl");
        assert_eq!(wireguard_tunnel(I0, "ota-uplink"), "vxwg0-ota-3cb1");
        // Same pure-function contract as `access_port`: depends on its own id alone.
        let before = wireguard_tunnel(I0, "tun-b");
        for neighbour in ["tun-a", "aaa", "zzz"] {
            assert_ne!(wireguard_tunnel(I0, neighbour), before);
        }
        assert_eq!(wireguard_tunnel(I0, "tun-b"), before);
        // Long ids shorten deterministically, same scheme as `stem`.
        let long = wireguard_tunnel(I0, "ota-uplink-to-central-compute-site-2");
        assert!(long.len() <= IFNAME_MAX, "{long} is {} bytes", long.len());
        assert_eq!(
            long,
            wireguard_tunnel(I0, "ota-uplink-to-central-compute-site-2")
        );
    }

    #[test]
    fn vxlan_tunnel_names_are_deterministic_and_fit_ifnamsiz() {
        assert_eq!(vxlan_tunnel(I0, "site-b"), "vxln0-site-b");
        let before = vxlan_tunnel(I0, "tun-b");
        for neighbour in ["tun-a", "aaa", "zzz"] {
            assert_ne!(vxlan_tunnel(I0, neighbour), before);
        }
        assert_eq!(vxlan_tunnel(I0, "tun-b"), before);
        let long = vxlan_tunnel(I0, "ota-uplink-to-central-compute-site-2");
        assert!(long.len() <= IFNAME_MAX, "{long} is {} bytes", long.len());
        assert_eq!(
            long,
            vxlan_tunnel(I0, "ota-uplink-to-central-compute-site-2")
        );
        // Distinct prefixes keep the two tunnel kinds' names from colliding even on the same id.
        assert_ne!(
            vxlan_tunnel(I0, "ota-uplink"),
            wireguard_tunnel(I0, "ota-uplink")
        );
    }

    /// Two runs of one vehicle share a host, so the slot is part of every name: the same device
    /// in two slots has two names, and the stem is the same in both.
    #[test]
    fn the_slot_separates_two_runs_of_one_device() {
        let i1 = Instance::parse("1").unwrap();
        assert_eq!(
            access_port(I0, "node-adas-probe-01", None),
            "vz0-node-a-c06a"
        );
        assert_eq!(
            access_port(i1, "node-adas-probe-01", None),
            "vz1-node-a-c06a"
        );
        assert_eq!(veth_peer(i1, "node-adas-probe-01", None), "vp1-node-a-c06a");
        assert_eq!(
            ingress_ifb(&access_port(i1, "node-adas-probe-01", None)),
            "vi1-node-a-c06a"
        );
        assert_ne!(wireguard_tunnel(I0, "t"), wireguard_tunnel(i1, "t"));
        assert_ne!(vxlan_tunnel(I0, "t"), vxlan_tunnel(i1, "t"));
    }

    #[test]
    fn an_access_port_name_says_its_slot() {
        let z = Instance::parse("z").unwrap();
        assert_eq!(access_port_instance("vz0-node-a"), Some(Some(I0)));
        assert_eq!(
            access_port_instance(&access_port(z, "x", None)),
            Some(Some(z))
        );
        assert_eq!(access_port_instance("vz-node-a"), Some(None));
        for other in [
            "vxbr0",
            "vp0-node-a",
            "vi0-node-a",
            "vzA-node",
            "eth0",
            "vz",
        ] {
            assert_eq!(access_port_instance(other), None, "{other}");
        }
    }
}
