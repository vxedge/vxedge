//! Native Rust netlink primitives for `provision.rs`'s kernel-bridge path — slice 1 of the
//! rtnetlink port `netops.rs`'s own doc comment names as a follow-up.
//!
//! Deliberately **not** a full replacement for [`super::netops::Shell`]: `rtnetlink`/
//! `netlink-packet-route` (rust-netlink org, an 8-year, tens-of-millions-of-downloads track
//! record) cover links, addresses, bridge VLAN filtering and netns lifecycle well, confirmed
//! by reading the actual v0.21.0 source before committing to this — but their `tc` support
//! covers only `fq_codel`/`ingress` qdiscs, nothing for HTB, netem, or filter actions. The
//! only crate that claims that coverage is 7 months old with ~1.4K downloads: too immature to
//! bet the mechanism behind H1's headline number on. So `tc`/`ovs-vsctl` stay on
//! `Shell` — this module covers `apply_links`, `apply_slices`'s trunk-VLAN half,
//! `apply_endpoints`'s link+VLAN halves, `teardown`'s link deletes, `observe`'s link/VLAN
//! queries, VXLAN link creation, zones' container netns lifecycle (netns create/delete/
//! exists plus moving/renaming a link across namespaces, `docs/ROADMAP.md` #16), and (added
//! 2026-08-04) WireGuard tunnel **device creation only** — `nft`/`wg` genuinely stay on
//! `Shell` even after this: `nft` has its own JSON-based CLI protocol with no netlink crate
//! coverage here, and WireGuard peer/key configuration is a *different kernel protocol
//! family* (generic netlink's `wireguard` family, not rtnetlink), which `LinkWireguard`
//! deliberately does not attempt to cover — see [`ensure_wireguard`]'s own doc comment. Also
//! added 2026-08-04: the fiveg host-attachment path (`apply_fiveg_uplink`/
//! `apply_fiveg_ue_routes` in `provision.rs`) — bridge-self VLAN membership
//! ([`bridge_vlan_add_self`]), an 802.1q VLAN subinterface ([`ensure_vlan_subif`]), resolving a
//! link's current bridge master ([`link_master`]), and route creation ([`add_route`]). `tc`
//! stays on `Shell` there too (`apply_fiveg_slice_qos`'s own HTB tree). `docs/ROADMAP.md` #16
//! tracks the still-deferred remainder (OVS's own link calls, tap devices — confirmed not even
//! netlink-portable at all) as separate, smaller follow-ons — not silently implied done by this
//! module.
//!
//! **Netns creation/deletion is not hand-rolled here.** `rtnetlink::NetworkNamespace::add`/
//! `del` already do exactly what `ip netns add`/`del` do — confirmed by reading the actual
//! source: a `fork()`-based child process (the calling thread's own namespace is never
//! touched), `unshare(CLONE_NEWNET)`, the same `mount --make-shared /run/netns` + fallback
//! strategy iproute2 itself uses (the upstream code comment says so directly), then a
//! bind-mount of `/proc/self/ns/net` onto `/run/netns/<name>`. Using the already-vetted crate
//! for this is a direct extension of the same trust decision made for the rest of this
//! module, not a new one — the alternative (hand-rolling `unshare`/`mount` ourselves) is
//! exactly the kind of low-level systems programming a mistake in is hardest to notice.
//!
//! **A genuine side benefit, not just fewer shell-outs**: `Shell::escalate`'s sudo-wrapping
//! exists because `setcap cap_net_admin+ep` on the `vxedged` binary does *not* propagate to the
//! `ip`/`bridge`/`tc` child processes it spawns (capabilities don't cross fork+exec without
//! ambient-set handling neither `setcap` nor `Shell` performs — found live 2026-07-17). That
//! problem doesn't exist here: these are `vxedged`'s own direct syscalls, no child process
//! involved, so `setcap cap_net_admin+ep` on the daemon binary works correctly for everything
//! in this module. [`NetlinkError::Permission`] says so.
//!
//! Every function takes the same `netns: Option<&str>` `Shell` does, and preserves it exactly:
//! `rtnetlink::new_connection()` opens a socket in the *calling thread's* current network
//! namespace, so netns-scoped calls run on a dedicated `std::thread` that `setns(2)`s into
//! `/var/run/netns/<name>` before ever touching netlink — never shared across namespace
//! boundaries, which is what keeps the netns integration suite's isolation real.

use futures_util::TryStreamExt;
use rtnetlink::packet_route::AddressFamily;
use rtnetlink::packet_route::link::{
    AfSpecBridge, BridgeVlanInfoFlags, LinkAttribute, LinkExtentMask,
};
use rtnetlink::{
    LinkBridge, LinkBridgeVlan, LinkUnspec, LinkVeth, LinkVlan, LinkVxlan, LinkWireguard,
    NetworkNamespace, RouteMessageBuilder,
};
use std::os::fd::AsRawFd;

// Netlink error codes are the kernel's own `-errno`, carried in `ErrorMessage::code`. Named
// here rather than pulling in `libc` for three constants.
const EPERM: i32 = -1;
const ENOENT: i32 = -2;
const EEXIST: i32 = -17;
const ENODEV: i32 = -19;

#[derive(Debug, thiserror::Error)]
pub enum NetlinkError {
    #[error("netns `{netns}`: {detail}")]
    Namespace { netns: String, detail: String },
    #[error("cannot open a netlink connection: {0}")]
    Connect(std::io::Error),
    #[error(
        "insufficient privileges for netlink operation `{op}` — vxedged needs CAP_NET_ADMIN \
         for its own process (run as root, or `setcap cap_net_admin+ep` on the vxedged binary \
         — unlike the iproute2 shell-out this replaces, setcap DOES work here: no child \
         process is involved, so capability propagation across fork+exec isn't a concern)"
    )]
    Permission { op: String },
    #[error("netlink operation `{op}` failed: {source}")]
    Operation {
        op: String,
        source: rtnetlink::Error,
    },
    #[error("netlink worker thread panicked during `{op}`: {detail}")]
    ThreadPanic { op: String, detail: String },
    #[error("link `{name}` not found")]
    LinkNotFound { name: String },
    #[error("`{cidr}` is not a valid CIDR address (expected `<ip>/<prefix>`)")]
    MalformedCidr { cidr: String },
    #[error("netlink route operation `{op}` rejected the route: {detail}")]
    InvalidRoute { op: String, detail: String },
}

fn is_eexist(e: &rtnetlink::Error) -> bool {
    matches!(e, rtnetlink::Error::NetlinkError(m) if m.code.map(|c| c.get()) == Some(EEXIST))
}

fn is_missing(e: &rtnetlink::Error) -> bool {
    matches!(
        e,
        rtnetlink::Error::NetlinkError(m)
            if matches!(m.code.map(|c| c.get()), Some(ENOENT) | Some(ENODEV))
    )
}

fn is_permission(e: &rtnetlink::Error) -> bool {
    matches!(e, rtnetlink::Error::NetlinkError(m) if m.code.map(|c| c.get()) == Some(EPERM))
}

fn op_err(op: &str, source: rtnetlink::Error) -> NetlinkError {
    if is_permission(&source) {
        NetlinkError::Permission { op: op.to_string() }
    } else {
        NetlinkError::Operation {
            op: op.to_string(),
            source,
        }
    }
}

fn enter_netns(name: &str) -> Result<(), NetlinkError> {
    let path = format!("/var/run/netns/{name}");
    let file = std::fs::File::open(&path).map_err(|source| NetlinkError::Namespace {
        netns: name.to_string(),
        detail: source.to_string(),
    })?;
    nix::sched::setns(&file, nix::sched::CloneFlags::CLONE_NEWNET).map_err(|errno| {
        NetlinkError::Namespace {
            netns: name.to_string(),
            detail: errno.to_string(),
        }
    })
}

fn panic_message(e: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Runs `f` against a fresh connection, on a dedicated thread that enters `netns` first (if
/// given) — the one piece of genuinely new engineering this port needed, since a shared
/// connection or runtime across namespace boundaries would silently break the isolation the
/// netns integration suite exists to prove.
fn run_async<F, Fut, T>(netns: Option<&str>, op: &'static str, f: F) -> Result<T, NetlinkError>
where
    F: FnOnce(rtnetlink::Handle) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, NetlinkError>>,
    T: Send + 'static,
{
    let netns = netns.map(str::to_string);
    let handle = std::thread::spawn(move || -> Result<T, NetlinkError> {
        if let Some(ns) = &netns {
            enter_netns(ns)?;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a current-thread tokio runtime for a netlink call");
        rt.block_on(async move {
            let (connection, handle, _) =
                rtnetlink::new_connection().map_err(NetlinkError::Connect)?;
            tokio::spawn(connection);
            f(handle).await
        })
    });
    handle.join().unwrap_or_else(|e| {
        Err(NetlinkError::ThreadPanic {
            op: op.to_string(),
            detail: panic_message(&*e),
        })
    })
}

/// The same dedicated-thread-plus-runtime shape [`run_async`] uses, minus the netlink
/// connection it always opens — for [`NetworkNamespace::add`]/[`NetworkNamespace::del`], which
/// don't touch an `rtnetlink::Handle` at all (they `fork()`/`unshare()`/`mount()` directly).
/// Deliberately a separate, small function rather than a refactor of `run_async` itself: that
/// function is already proven correct by the whole privileged netns suite, and duplicating a
/// few lines here is cheaper than risking it.
fn run_blocking<F, Fut, T>(op: &'static str, f: F) -> Result<T, NetlinkError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, NetlinkError>>,
    T: Send + 'static,
{
    let handle = std::thread::spawn(move || -> Result<T, NetlinkError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a current-thread tokio runtime for a netlink call");
        rt.block_on(f())
    });
    handle.join().unwrap_or_else(|e| {
        Err(NetlinkError::ThreadPanic {
            op: op.to_string(),
            detail: panic_message(&*e),
        })
    })
}

async fn link_index(
    handle: &rtnetlink::Handle,
    name: &str,
) -> Result<Option<u32>, rtnetlink::Error> {
    let mut links = handle.link().get().match_name(name.to_string()).execute();
    // `match_name` on a nonexistent link returns `Err(ENODEV)`, not an empty stream — found
    // live by this module's own unit tests, contrary to what upstream's own `set_link_down.rs`
    // example implies (its `else` branch on `try_next()? == None` looks like it can never
    // actually be reached the way it's written). Treat "missing" the same way every other
    // function here already does, rather than letting it surface as a hard connection error.
    match links.try_next().await {
        Ok(v) => Ok(v.map(|msg| msg.header.index)),
        Err(e) if is_missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Mirrors `Shell::link_exists`'s exact semantics: any failure (including "no CAP_NET_ADMIN to
/// even ask") reads as "does not exist," not as an error — matching the existing caller
/// contract this replaces rather than tightening it as a side effect of the port.
pub fn link_exists(netns: Option<&str>, name: &str) -> bool {
    let name = name.to_string();
    run_async(netns, "link_exists", move |handle| async move {
        link_index(&handle, &name)
            .await
            .map(|idx| idx.is_some())
            .map_err(|e| op_err("link_exists", e))
    })
    .unwrap_or(false)
}

/// Resolves `dev`'s current bridge master (`IFLA_MASTER`/`LinkAttribute::Controller`), if any —
/// `ip -json link show <dev>`'s own `master` field, read directly rather than parsed from JSON.
/// Two lookups: the ifindex the attribute carries still needs resolving back to a name via a
/// second query. `None` covers both "`dev` doesn't exist" and "`dev` exists but has no
/// master" — the caller (`provision.rs::host_veth_master`) is the one that turns "no master"
/// into an error, since a bare link with nothing attached is a valid state elsewhere in this
/// module (e.g. before [`set_master`] runs).
pub fn link_master(netns: Option<&str>, dev: &str) -> Result<Option<String>, NetlinkError> {
    let dev = dev.to_string();
    run_async(netns, "link_master", move |handle| async move {
        let mut links = handle.link().get().match_name(dev.clone()).execute();
        let msg = match links.try_next().await {
            Ok(Some(msg)) => msg,
            Ok(None) => return Ok(None),
            Err(e) if is_missing(&e) => return Ok(None),
            Err(e) => return Err(op_err("link_master", e)),
        };
        let Some(master_idx) = msg.attributes.iter().find_map(|a| match a {
            LinkAttribute::Controller(idx) => Some(*idx),
            _ => None,
        }) else {
            return Ok(None);
        };
        let mut masters = handle.link().get().match_index(master_idx).execute();
        let master_msg = match masters.try_next().await {
            Ok(Some(msg)) => msg,
            Ok(None) => return Ok(None),
            Err(e) if is_missing(&e) => return Ok(None),
            Err(e) => return Err(op_err("link_master", e)),
        };
        Ok(master_msg.attributes.iter().find_map(|a| match a {
            LinkAttribute::IfName(name) => Some(name.clone()),
            _ => None,
        }))
    })
}

/// Idempotent: tolerates an already-existing bridge, the same contract
/// `Shell::run_tolerate_exists` carried for `ip link add ... type bridge`.
pub fn ensure_bridge(netns: Option<&str>, name: &str) -> Result<(), NetlinkError> {
    let name = name.to_string();
    run_async(netns, "ensure_bridge", move |handle| async move {
        if link_index(&handle, &name)
            .await
            .map_err(|e| op_err("ensure_bridge", e))?
            .is_some()
        {
            return Ok(());
        }
        handle
            .link()
            .add(LinkBridge::new(&name).vlan_filtering(true).build())
            .execute()
            .await
            .or_else(|e| {
                if is_eexist(&e) {
                    Ok(())
                } else {
                    Err(op_err("ensure_bridge", e))
                }
            })
    })
}

/// Idempotent, same contract as [`ensure_bridge`].
pub fn ensure_veth_pair(netns: Option<&str>, name: &str, peer: &str) -> Result<(), NetlinkError> {
    let name = name.to_string();
    let peer = peer.to_string();
    run_async(netns, "ensure_veth_pair", move |handle| async move {
        if link_index(&handle, &name)
            .await
            .map_err(|e| op_err("ensure_veth_pair", e))?
            .is_some()
        {
            return Ok(());
        }
        handle
            .link()
            .add(LinkVeth::new(&name, &peer).build())
            .execute()
            .await
            .or_else(|e| {
                if is_eexist(&e) {
                    Ok(())
                } else {
                    Err(op_err("ensure_veth_pair", e))
                }
            })
    })
}

/// Idempotent, same contract as [`ensure_bridge`]. Unicast point-to-point VXLAN only — no
/// `group`/multicast support, matching `network.tunnels`' own `kind: vxlan` schema
/// (`docs/ARCHITECTURE.md` §6.0.3: `vni`/`remote`/`dstport`, no group address). `rtnetlink`'s
/// `LinkVxlan::remote`/`remote6` are separate methods (no single `IpAddr`-generic one), hence
/// the branch on `remote`'s own variant.
pub fn ensure_vxlan(
    netns: Option<&str>,
    name: &str,
    vni: u32,
    remote: std::net::IpAddr,
    dstport: u16,
) -> Result<(), NetlinkError> {
    let name = name.to_string();
    run_async(netns, "ensure_vxlan", move |handle| async move {
        if link_index(&handle, &name)
            .await
            .map_err(|e| op_err("ensure_vxlan", e))?
            .is_some()
        {
            return Ok(());
        }
        let builder = LinkVxlan::new(&name, vni).port(dstport);
        let builder = match remote {
            std::net::IpAddr::V4(addr) => builder.remote(addr),
            std::net::IpAddr::V6(addr) => builder.remote6(addr),
        };
        handle
            .link()
            .add(builder.build())
            .execute()
            .await
            .or_else(|e| {
                if is_eexist(&e) {
                    Ok(())
                } else {
                    Err(op_err("ensure_vxlan", e))
                }
            })
    })
}

/// Idempotent, same contract as [`ensure_bridge`]. **Device creation only** — `IFLA_LINKINFO`
/// with `INFO_KIND=wireguard` and no `INFO_DATA`, which is all `ip link add ... type
/// wireguard` itself does. Peer/key/listen-port configuration is a *different* protocol
/// entirely (the kernel's `wireguard` generic-netlink family, not rtnetlink at all — no
/// `IFLA_*` attribute carries a private key or a peer list), so `wg genkey`/`wg set` stay on
/// the `wg` shell-out in `provision.rs` permanently, not as deferred scope.
pub fn ensure_wireguard(netns: Option<&str>, name: &str) -> Result<(), NetlinkError> {
    let name = name.to_string();
    run_async(netns, "ensure_wireguard", move |handle| async move {
        if link_index(&handle, &name)
            .await
            .map_err(|e| op_err("ensure_wireguard", e))?
            .is_some()
        {
            return Ok(());
        }
        handle
            .link()
            .add(LinkWireguard::new(&name).build())
            .execute()
            .await
            .or_else(|e| {
                if is_eexist(&e) {
                    Ok(())
                } else {
                    Err(op_err("ensure_wireguard", e))
                }
            })
    })
}

pub fn set_master(netns: Option<&str>, dev: &str, bridge: &str) -> Result<(), NetlinkError> {
    let dev = dev.to_string();
    let bridge = bridge.to_string();
    run_async(netns, "set_master", move |handle| async move {
        let Some(dev_idx) = link_index(&handle, &dev)
            .await
            .map_err(|e| op_err("set_master", e))?
        else {
            return Err(NetlinkError::LinkNotFound { name: dev });
        };
        let Some(bridge_idx) = link_index(&handle, &bridge)
            .await
            .map_err(|e| op_err("set_master", e))?
        else {
            return Err(NetlinkError::LinkNotFound { name: bridge });
        };
        handle
            .link()
            .set(
                LinkUnspec::new_with_index(dev_idx)
                    .controller(bridge_idx)
                    .build(),
            )
            .execute()
            .await
            .map_err(|e| op_err("set_master", e))
    })
}

pub fn link_up(netns: Option<&str>, name: &str) -> Result<(), NetlinkError> {
    let name = name.to_string();
    run_async(netns, "link_up", move |handle| async move {
        let Some(idx) = link_index(&handle, &name)
            .await
            .map_err(|e| op_err("link_up", e))?
        else {
            return Err(NetlinkError::LinkNotFound { name });
        };
        handle
            .link()
            .set(LinkUnspec::new_with_index(idx).up().build())
            .execute()
            .await
            .map_err(|e| op_err("link_up", e))
    })
}

/// Idempotent: tolerates an already-absent link, the same contract
/// `Shell::run_tolerate_missing` carried for `ip link del`.
pub fn delete_link(netns: Option<&str>, name: &str) -> Result<(), NetlinkError> {
    let name = name.to_string();
    run_async(netns, "delete_link", move |handle| async move {
        let Some(idx) = link_index(&handle, &name)
            .await
            .map_err(|e| op_err("delete_link", e))?
        else {
            return Ok(());
        };
        handle.link().del(idx).execute().await.or_else(|e| {
            if is_missing(&e) {
                Ok(())
            } else {
                Err(op_err("delete_link", e))
            }
        })
    })
}

/// `bridge vlan add dev <dev> vid <vid> [pvid untagged]` — idempotent (tolerates an already-
/// present entry, matching `Shell::run_tolerate_exists`'s contract for the same command).
pub fn bridge_vlan_add(
    netns: Option<&str>,
    dev: &str,
    vid: u16,
    pvid_untagged: bool,
) -> Result<(), NetlinkError> {
    let dev_owned = dev.to_string();
    run_async(netns, "bridge_vlan_add", move |handle| async move {
        let Some(idx) = link_index(&handle, &dev_owned)
            .await
            .map_err(|e| op_err("bridge_vlan_add", e))?
        else {
            return Err(NetlinkError::LinkNotFound { name: dev_owned });
        };
        let flags = if pvid_untagged {
            BridgeVlanInfoFlags::Pvid | BridgeVlanInfoFlags::Untagged
        } else {
            BridgeVlanInfoFlags::empty()
        };
        let msg = LinkBridgeVlan::new(idx).vlan(vid, flags).build();
        handle.link().set(msg).execute().await.or_else(|e| {
            if is_eexist(&e) {
                Ok(())
            } else {
                Err(op_err("bridge_vlan_add", e))
            }
        })
    })
}

/// `bridge vlan del dev <dev> vid <vid>` — idempotent (tolerates an already-absent entry).
pub fn bridge_vlan_del(netns: Option<&str>, dev: &str, vid: u16) -> Result<(), NetlinkError> {
    let dev_owned = dev.to_string();
    run_async(netns, "bridge_vlan_del", move |handle| async move {
        let Some(idx) = link_index(&handle, &dev_owned)
            .await
            .map_err(|e| op_err("bridge_vlan_del", e))?
        else {
            return Ok(());
        };
        let msg = LinkBridgeVlan::new(idx)
            .vlan(vid, BridgeVlanInfoFlags::empty())
            .build();
        handle
            .link()
            .del_with_message(msg)
            .execute()
            .await
            .or_else(|e| {
                if is_missing(&e) {
                    Ok(())
                } else {
                    Err(op_err("bridge_vlan_del", e))
                }
            })
    })
}

/// `bridge vlan add dev <bridge> vid <vid> self` — the bridge device's own internal ("self")
/// VLAN membership, distinct from [`bridge_vlan_add`]'s regular port entry: a `vlan_filtering`
/// bridge's own internal path needs explicit VLAN membership too before the host can address a
/// subinterface on it (`apply_fiveg_uplink`'s own doc comment in `provision.rs` has the full
/// story). A separate function rather than a flag on `bridge_vlan_add` — that one is already
/// proven by every existing caller (slices, endpoints, VXLAN tunnels), and `self` changes which
/// *device* the entry applies to, not just a flag on the same shape. Idempotent, same contract.
pub fn bridge_vlan_add_self(
    netns: Option<&str>,
    bridge: &str,
    vid: u16,
) -> Result<(), NetlinkError> {
    let bridge_owned = bridge.to_string();
    run_async(netns, "bridge_vlan_add_self", move |handle| async move {
        let Some(idx) = link_index(&handle, &bridge_owned)
            .await
            .map_err(|e| op_err("bridge_vlan_add_self", e))?
        else {
            return Err(NetlinkError::LinkNotFound { name: bridge_owned });
        };
        let msg = LinkBridgeVlan::new(idx)
            .vlan(vid, BridgeVlanInfoFlags::empty())
            .bridge_self()
            .build();
        handle.link().set(msg).execute().await.or_else(|e| {
            if is_eexist(&e) {
                Ok(())
            } else {
                Err(op_err("bridge_vlan_add_self", e))
            }
        })
    })
}

/// `ip link add link <parent> name <name> type vlan id <vid>` — an 802.1q VLAN subinterface,
/// structurally different from [`bridge_vlan_add`]'s bridge-filtering membership: this creates
/// a whole new netdev the host can address (`apply_fiveg_uplink`'s `<bridge>.<vid>`
/// convention), not a tag on an existing port. Idempotent, same contract as [`ensure_bridge`].
pub fn ensure_vlan_subif(
    netns: Option<&str>,
    name: &str,
    parent: &str,
    vlan_id: u16,
) -> Result<(), NetlinkError> {
    let name_owned = name.to_string();
    let parent_owned = parent.to_string();
    run_async(netns, "ensure_vlan_subif", move |handle| async move {
        if link_index(&handle, &name_owned)
            .await
            .map_err(|e| op_err("ensure_vlan_subif", e))?
            .is_some()
        {
            return Ok(());
        }
        let Some(parent_idx) = link_index(&handle, &parent_owned)
            .await
            .map_err(|e| op_err("ensure_vlan_subif", e))?
        else {
            return Err(NetlinkError::LinkNotFound { name: parent_owned });
        };
        handle
            .link()
            .add(LinkVlan::new(&name_owned, parent_idx, vlan_id).build())
            .execute()
            .await
            .or_else(|e| {
                if is_eexist(&e) {
                    Ok(())
                } else {
                    Err(op_err("ensure_vlan_subif", e))
                }
            })
    })
}

/// `ip route add <dest-cidr> via <gateway> dev <name>` — idempotent (tolerates an
/// already-present route, matching `Shell::run_tolerate_exists`'s contract for the same
/// command). `dest_cidr` reuses [`parse_cidr`]; `via` is a bare address, not CIDR.
pub fn add_route(
    netns: Option<&str>,
    dest_cidr: &str,
    via: &str,
    dev: &str,
) -> Result<(), NetlinkError> {
    let (dest, prefix) = parse_cidr(dest_cidr)?;
    let via: std::net::IpAddr = via.parse().map_err(|_| NetlinkError::MalformedCidr {
        cidr: via.to_string(),
    })?;
    let dev_owned = dev.to_string();
    run_async(netns, "add_route", move |handle| async move {
        let Some(idx) = link_index(&handle, &dev_owned)
            .await
            .map_err(|e| op_err("add_route", e))?
        else {
            return Err(NetlinkError::LinkNotFound { name: dev_owned });
        };
        let msg = RouteMessageBuilder::<std::net::IpAddr>::new()
            .destination_prefix(dest, prefix)
            .map_err(|e| NetlinkError::InvalidRoute {
                op: "add_route".to_string(),
                detail: e.to_string(),
            })?
            .gateway(via)
            .map_err(|e| NetlinkError::InvalidRoute {
                op: "add_route".to_string(),
                detail: e.to_string(),
            })?
            .output_interface(idx)
            .build();
        handle.route().add(msg).execute().await.or_else(|e| {
            if is_eexist(&e) {
                Ok(())
            } else {
                Err(op_err("add_route", e))
            }
        })
    })
}

/// `bridge -j vlan show dev <dev>`, reduced to the VLAN ids carried — `observe`'s own read
/// side. Dumps every link with the bridge-VLAN extent mask and filters client-side by ifindex,
/// matching the upstream `set_bridge_vlan.rs` example's own documented workaround: combining
/// `set_filter_mask` with a name/index match doesn't work at the kernel level for this dump.
pub fn bridge_vlan_list(netns: Option<&str>, dev: &str) -> Result<Vec<u16>, NetlinkError> {
    let dev_owned = dev.to_string();
    run_async(netns, "bridge_vlan_list", move |handle| async move {
        let Some(idx) = link_index(&handle, &dev_owned)
            .await
            .map_err(|e| op_err("bridge_vlan_list", e))?
        else {
            return Ok(Vec::new());
        };
        let mut stream = handle
            .link()
            .get()
            .set_filter_mask(
                AddressFamily::Bridge,
                vec![LinkExtentMask::BrvlanCompressed],
            )
            .execute();
        let mut vids = Vec::new();
        while let Some(msg) = stream
            .try_next()
            .await
            .map_err(|e| op_err("bridge_vlan_list", e))?
        {
            if msg.header.index != idx {
                continue;
            }
            for attr in &msg.attributes {
                let LinkAttribute::AfSpecBridge(specs) = attr else {
                    continue;
                };
                for spec in specs {
                    if let AfSpecBridge::VlanInfo(info) = spec {
                        vids.push(info.vid);
                    }
                }
            }
        }
        Ok(vids)
    })
}

// ---------------------------------------------------------------------------
// Zone container netns lifecycle (`docs/ROADMAP.md` #16) — the second rtnetlink-port slice.
// ---------------------------------------------------------------------------

fn netns_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(rtnetlink::NETNS_PATH).join(name)
}

/// A plain file-existence check at `rtnetlink::NETNS_PATH` (`/run/netns/<name>`) — no netlink
/// call needed, and strictly more precise than the old `ip netns list` + line-parsing this
/// replaces (an exact filename match, not a prefix-tolerant string search).
pub fn netns_exists(name: &str) -> bool {
    netns_path(name).exists()
}

/// Host-context only, deliberately — never nested inside another netns, the same reason
/// `Shell::run_host` was used for this before the port: a bind mount created from inside `ip
/// netns exec` does not propagate. Idempotent: tolerates an already-existing namespace.
///
/// `NetworkNamespace::add` has no "already exists" tolerance of its own (it opens the netns
/// file with `O_EXCL`, so a second call errors) — checked here instead, before *and* after the
/// attempt, the same "check first, tolerate on the way out too" shape [`ensure_bridge`]/
/// [`ensure_veth_pair`] already use for the same idempotency contract against a real EEXIST
/// errno. `NetworkNamespace`'s own error type carries only a `String`, not a structured errno
/// to match on, which is why this checks existence directly rather than inspecting the error.
pub fn ensure_netns(name: &str) -> Result<(), NetlinkError> {
    if netns_exists(name) {
        return Ok(());
    }
    let owned = name.to_string();
    run_blocking("ensure_netns", move || async move {
        NetworkNamespace::add(owned.clone())
            .await
            .map_err(|e| NetlinkError::Namespace {
                netns: owned,
                detail: e.to_string(),
            })
    })
    .or_else(|e| if netns_exists(name) { Ok(()) } else { Err(e) })
}

/// Idempotent: tolerates an already-absent namespace, the same contract every other `delete_*`
/// in this module carries.
pub fn delete_netns(name: &str) -> Result<(), NetlinkError> {
    if !netns_exists(name) {
        return Ok(());
    }
    let owned = name.to_string();
    run_blocking("delete_netns", move || async move {
        NetworkNamespace::del(owned.clone())
            .await
            .map_err(|e| NetlinkError::Namespace {
                netns: owned,
                detail: e.to_string(),
            })
    })
    .or_else(|e| if netns_exists(name) { Err(e) } else { Ok(()) })
}

/// Moves `name` (found in the caller's own `netns` context, which may itself be a netns) into
/// `target_ns` — `LinkMessageBuilder::setns_by_fd`, opening `/run/netns/<target_ns>` as the fd
/// source. The link stays down across the move: this is called before [`rename_link`]/
/// [`link_up`] in `zones::backend::container_up`'s own sequence, never after, avoiding any
/// kernel rename-while-up edge case for interface types that care.
pub fn move_to_netns(netns: Option<&str>, name: &str, target_ns: &str) -> Result<(), NetlinkError> {
    let name_owned = name.to_string();
    let target_path = netns_path(target_ns);
    let target_ns_owned = target_ns.to_string();
    run_async(netns, "move_to_netns", move |handle| async move {
        let Some(idx) = link_index(&handle, &name_owned)
            .await
            .map_err(|e| op_err("move_to_netns", e))?
        else {
            return Err(NetlinkError::LinkNotFound { name: name_owned });
        };
        let file = std::fs::File::open(&target_path).map_err(|source| NetlinkError::Namespace {
            netns: target_ns_owned,
            detail: source.to_string(),
        })?;
        handle
            .link()
            .set(
                LinkUnspec::new_with_index(idx)
                    .setns_by_fd(file.as_raw_fd())
                    .build(),
            )
            .execute()
            .await
            .map_err(|e| op_err("move_to_netns", e))
    })
}

/// Renames a link, in whatever `netns` context it currently lives in.
pub fn rename_link(
    netns: Option<&str>,
    old_name: &str,
    new_name: &str,
) -> Result<(), NetlinkError> {
    let old_name_owned = old_name.to_string();
    let new_name_owned = new_name.to_string();
    run_async(netns, "rename_link", move |handle| async move {
        let Some(idx) = link_index(&handle, &old_name_owned)
            .await
            .map_err(|e| op_err("rename_link", e))?
        else {
            return Err(NetlinkError::LinkNotFound {
                name: old_name_owned,
            });
        };
        handle
            .link()
            .set(LinkUnspec::new_with_index(idx).name(new_name_owned).build())
            .execute()
            .await
            .map_err(|e| op_err("rename_link", e))
    })
}

fn parse_cidr(cidr: &str) -> Result<(std::net::IpAddr, u8), NetlinkError> {
    let malformed = || NetlinkError::MalformedCidr {
        cidr: cidr.to_string(),
    };
    let (ip, prefix) = cidr.split_once('/').ok_or_else(malformed)?;
    let ip: std::net::IpAddr = ip.parse().map_err(|_| malformed())?;
    let prefix: u8 = prefix.parse().map_err(|_| malformed())?;
    Ok((ip, prefix))
}

/// `ip addr add <cidr> dev <name>` — idempotent (tolerates an already-present address), the
/// one primitive slice 1 deliberately deferred (not needed for the kernel-bridge path it
/// scoped).
pub fn add_address(netns: Option<&str>, name: &str, cidr: &str) -> Result<(), NetlinkError> {
    let (addr, prefix) = parse_cidr(cidr)?;
    let name_owned = name.to_string();
    run_async(netns, "add_address", move |handle| async move {
        let Some(idx) = link_index(&handle, &name_owned)
            .await
            .map_err(|e| op_err("add_address", e))?
        else {
            return Err(NetlinkError::LinkNotFound { name: name_owned });
        };
        handle
            .address()
            .add(idx, addr, prefix)
            .execute()
            .await
            .or_else(|e| {
                if is_eexist(&e) {
                    Ok(())
                } else {
                    Err(op_err("add_address", e))
                }
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonexistent_link_reports_absent_without_erroring() {
        // No root/CAP_NET_ADMIN needed for a read against a name that cannot exist — this is
        // the same "runs unprivileged" property `Shell::link_exists` already has.
        assert!(!link_exists(None, "vx-nonexist"));
    }

    #[test]
    fn deleting_an_absent_link_is_a_no_op() {
        assert!(delete_link(None, "vx-nonexist").is_ok());
    }

    #[test]
    fn a_nonexistent_netns_reports_absent_without_erroring() {
        // Same "runs unprivileged" property as link_exists — a plain file-existence check.
        assert!(!netns_exists("vx-nonexist-ns"));
    }

    #[test]
    fn deleting_an_absent_netns_is_a_no_op() {
        assert!(delete_netns("vx-nonexist-ns").is_ok());
    }

    #[test]
    fn parse_cidr_accepts_v4_and_v6_addresses_with_a_prefix() {
        assert_eq!(
            parse_cidr("192.0.2.7/24").unwrap(),
            ("192.0.2.7".parse().unwrap(), 24)
        );
        assert_eq!(
            parse_cidr("2001:db8::7/64").unwrap(),
            ("2001:db8::7".parse().unwrap(), 64)
        );
    }

    #[test]
    fn parse_cidr_rejects_a_bare_address_with_no_prefix() {
        assert!(matches!(
            parse_cidr("192.0.2.7"),
            Err(NetlinkError::MalformedCidr { .. })
        ));
    }

    #[test]
    fn parse_cidr_rejects_an_unparsable_address() {
        assert!(matches!(
            parse_cidr("not-an-address/24"),
            Err(NetlinkError::MalformedCidr { .. })
        ));
    }

    #[test]
    fn parse_cidr_rejects_a_non_numeric_prefix() {
        assert!(matches!(
            parse_cidr("192.0.2.7/thirty-two"),
            Err(NetlinkError::MalformedCidr { .. })
        ));
    }
}
