//! Host interface names for a device's slice attachments, derived from the device id.
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
//! from the same stem (`vz-` for the access port, `vzp-` for its peer), so the stem gets 11
//! characters.
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

/// `IFNAMSIZ` is 16 bytes *including* the NUL terminator.
pub const IFNAME_MAX: usize = 15;

/// Bridge access port: the veth host side, or the tap itself.
const ACCESS_PREFIX: &str = "vz-";
/// Veth peer, the end that moves into the device's namespace.
const PEER_PREFIX: &str = "vzp-";
/// Ingress-impairment IFB, the device the access port's *inbound* traffic is redirected onto.
/// Four bytes like [`PEER_PREFIX`], so it costs the stem budget nothing extra.
const IFB_PREFIX: &str = "vxi-";

/// Longest stem that fits under *both* prefixes. The peer prefix is the longer one, so it sets
/// the budget for the pair — a stem that fitted only the access port would produce a veth whose
/// two ends could not both be named.
const STEM_MAX: usize = IFNAME_MAX - PEER_PREFIX.len();

/// Characters of the id kept verbatim when a stem must be shortened, leaving `-` + 4 hex digits.
const HEAD: usize = STEM_MAX - 5;

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
    if key.len() <= STEM_MAX {
        return key;
    }
    // ASCII by construction after `sanitize`, so slicing on a byte index is safe.
    let head = key[..HEAD].trim_end_matches('-');
    format!("{head}-{:04x}", fnv1a(key.as_bytes()) as u16)
}

/// The bridge access port for an attachment (veth host side, or the tap itself).
pub fn access_port(device: &str, port: Option<&str>) -> String {
    format!("{ACCESS_PREFIX}{}", stem(device, port))
}

/// The veth peer for an attachment — the end that moves into the device's namespace. Taps have
/// no peer: the hypervisor opens the access port directly, and the plan sets `peer == name`.
pub fn veth_peer(device: &str, port: Option<&str>) -> String {
    format!("{PEER_PREFIX}{}", stem(device, port))
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
pub fn ingress_ifb(access_port: &str) -> String {
    let stem = access_port
        .strip_prefix(ACCESS_PREFIX)
        .unwrap_or(access_port);
    format!("{IFB_PREFIX}{stem}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the positional scheme could not have: a name depends on its own device and
    /// nothing else. Adding, removing or reordering devices cannot move it.
    #[test]
    fn a_name_depends_on_its_device_alone() {
        let before = access_port("node-b", Some("eth0"));
        // Whatever else the experiment grows, `node-b` keeps its interface.
        for neighbour in ["node-a", "aaa", "zzz", "node-b-extra"] {
            assert_ne!(access_port(neighbour, Some("eth0")), before);
        }
        assert_eq!(access_port("node-b", Some("eth0")), before);
    }

    #[test]
    fn short_ids_are_spelled_out() {
        assert_eq!(access_port("node-a", Some("eth0")), "vz-node-a");
        assert_eq!(veth_peer("node-a", Some("eth0")), "vzp-node-a");
        assert_eq!(access_port("zonefl", None), "vz-zonefl");
        // Exactly at the budget.
        assert_eq!(access_port("node-cont-1", Some("eth0")), "vz-node-cont-1");
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
            let name = access_port(id, Some("eth0"));
            let peer = veth_peer(id, Some("eth0"));
            let ifb = ingress_ifb(&name);
            assert!(name.len() <= IFNAME_MAX, "{name} is {} bytes", name.len());
            assert!(peer.len() <= IFNAME_MAX, "{peer} is {} bytes", peer.len());
            assert!(ifb.len() <= IFNAME_MAX, "{ifb} is {} bytes", ifb.len());
            assert_eq!(name, access_port(id, Some("eth0")), "not deterministic");
        }
        // Pinned, because these are the names campaign scripts and results metadata carry: a
        // silent change here is a silent change to which interface a run measured.
        assert_eq!(
            access_port("node-adas-probe-01", Some("eth0")),
            "vz-node-a-c06a"
        );
        assert_eq!(
            veth_peer("node-infot-load-01", Some("eth0")),
            "vzp-node-i-2921"
        );
    }

    /// Ids sharing a head are what makes the hash necessary — `node-mvm-mmio-01` and
    /// `node-mvm-pci-01` are two arms of the same experiment.
    #[test]
    fn ids_sharing_a_head_do_not_share_a_name() {
        let a = access_port("node-mvm-mmio-01", Some("eth0"));
        let b = access_port("node-mvm-pci-01", Some("eth0"));
        assert_ne!(a, b, "two arms would have shared one interface");
    }

    /// A second attachment on the same device has to be nameable, and naming it must not rename
    /// the first.
    #[test]
    fn a_non_primary_port_extends_the_stem_without_moving_the_primary_one() {
        assert_eq!(access_port("gw1", Some("eth0")), "vz-gw1");
        assert_eq!(access_port("gw1", None), "vz-gw1");
        assert_eq!(access_port("gw1", Some("eth1")), "vz-gw1-eth1");
        assert_ne!(access_port("gw1", Some("eth1")), access_port("gw1", None));
    }

    /// An attachment's three names are distinct but share a stem, so `vz-`/`vzp-`/`vxi-` read as
    /// one set. Distinctness is not cosmetic: the IFB is created as a real link, and a name
    /// clashing with the port it serves would delete that port on teardown.
    #[test]
    fn the_ingress_ifb_shares_the_stem_but_not_the_name() {
        for id in ["node-a", "node-adas-probe-01"] {
            let port = access_port(id, Some("eth0"));
            let peer = veth_peer(id, Some("eth0"));
            let ifb = ingress_ifb(&port);
            assert_eq!(ifb, format!("vxi-{}", stem(id, Some("eth0"))));
            assert_ne!(ifb, port);
            assert_ne!(ifb, peer);
        }
    }

    #[test]
    fn characters_the_kernel_will_not_take_are_folded_out() {
        let name = access_port("node/a b:c", Some("eth0"));
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{name}"
        );
        assert!(name.len() <= IFNAME_MAX);
    }
}
