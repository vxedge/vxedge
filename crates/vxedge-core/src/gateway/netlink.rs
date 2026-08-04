//! Native Rust netlink primitives for `provision.rs`'s kernel-bridge path — slice 1 of the
//! rtnetlink port `netops.rs`'s own doc comment names as a follow-up.
//!
//! Deliberately **not** a full replacement for [`super::netops::Shell`]: `rtnetlink`/
//! `netlink-packet-route` (rust-netlink org, an 8-year, tens-of-millions-of-downloads track
//! record) cover links, addresses and bridge VLAN filtering well, confirmed by reading the
//! actual v0.21.0 source before committing to this — but their `tc` support covers only
//! `fq_codel`/`ingress` qdiscs, nothing for HTB, netem, or filter actions. The only crate that
//! claims that coverage is 7 months old with ~1.4K downloads: too immature to bet the mechanism
//! behind H1's headline number on. So `tc`/`nft`/`wg`/`ovs-vsctl` stay on `Shell` — this module
//! covers exactly what `apply_links`, `apply_slices`'s trunk-VLAN half, `apply_endpoints`'s
//! link+VLAN halves, `teardown`'s link deletes, and `observe`'s link/VLAN queries need, and
//! nothing else. `docs/TASKS.md` #16 tracks the deferred remainder (OVS's own link calls, the
//! fiveg uplink, tunnel device creation, zone netns lifecycle, tap devices) as separate,
//! smaller follow-ons — not silently implied done by this slice.
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
use rtnetlink::{LinkBridge, LinkBridgeVlan, LinkUnspec, LinkVeth};

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
}
