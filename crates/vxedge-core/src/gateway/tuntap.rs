//! Persistent tap device creation — the other half of `docs/TASKS.md` #16 the
//! [`super::netlink`] module doc explicitly deferred: `ip tuntap add ... mode tap` is not an
//! `RTM_NEWLINK` operation at all. Tun/tap devices are created via `ioctl(TUNSETIFF)` on
//! `/dev/net/tun`, a genuinely different kernel interface than the rest of the rtnetlink port,
//! and made to outlive the creating process's file descriptor via a second ioctl
//! (`TUNSETPERSIST`) — the same two calls `ip tuntap add` itself makes internally.
//!
//! **Deliberately not hand-rolled.** `rust-patterns` (this repo's own house rule) treats
//! `unsafe` as effectively forbidden — none exists in the workspace — and requires FFI to go
//! through a vetted binding crate rather than a hand-defined `struct ifreq` + raw `ioctl(2)`
//! call, which is exactly the shape this operation would otherwise take. `tun` (crates.io,
//! `meh/rust-tun`) is that vetted crate here: 2.35M downloads total, ~486K in the trailing 90
//! days, last published 2026-07-21 — checked before adding it as a dependency, the same
//! diligence [`super::netlink`]'s own doc comment applied to `rtnetlink` vs `nlink`. Its
//! `Device::persist()` calls `TUNSETPERSIST` on the open fd directly; the fd is then dropped
//! (closing it) with the device already detached from this process's lifetime, exactly the
//! "create it, something else opens it later" contract `qemu-microvm`/`libvirt` need — QEMU's
//! own `-netdev tap,ifname=...` attaches to an existing persistent device by name, it does not
//! expect to be the one that created it.
//!
//! **Ownership, not just persistence.** The kernel refuses `TUNSETIFF` on an *existing*
//! persistent device to any opener that is neither `CAP_NET_ADMIN`-capable nor the uid
//! recorded by a prior `TUNSETOWNER` (`net/tun.c`'s own `tun_attach`) -- an unset owner
//! means "root only," not "anyone." `Device::persist()` alone would leave qemu-microvm
//! needing root just to attach to a tap this same (unprivileged, CAP_NET_ADMIN-carrying)
//! process just created -- live-found 2026-08-06 chasing a "verification failed -- nodes
//! not up" report that turned out to be a root-owned pidfile an unprivileged `vxedged`
//! could no longer read back. `create_and_persist` below calls `TUNSETOWNER` with this
//! process's own uid so the later, deliberately *unprivileged* qemu launch
//! (`zones::backend::microvm_up`) can attach without `sudo` -- see that function's own
//! comment for why running qemu privileged is the wrong fix, not a shortcut to this one.
//!
//! Netns handling mirrors [`super::netlink`]'s own `run_async`/`enter_netns` shape rather than
//! reusing it directly: `enter_netns` there is private, small, and the module's own doc comment
//! states a preference for a few duplicated lines over widening a proven function's callers —
//! same call here, since tap creation needs no netlink connection or async runtime at all (the
//! `tun` crate is synchronous), so `run_async`'s shape wouldn't fit unchanged anyway.

use super::netlink;

#[derive(Debug, thiserror::Error)]
pub enum TunTapError {
    #[error("netns `{netns}`: {detail}")]
    Namespace { netns: String, detail: String },
    #[error("tap device `{name}`: create failed: {source}")]
    Create { name: String, source: tun::Error },
    #[error("tap device `{name}`: persist failed: {source}")]
    Persist { name: String, source: tun::Error },
    #[error("tap device `{name}`: set-owner failed: {source}")]
    Owner { name: String, source: tun::Error },
    #[error("tap creation thread for `{name}` panicked: {detail}")]
    ThreadPanic { name: String, detail: String },
}

fn enter_netns(name: &str) -> Result<(), TunTapError> {
    let path = format!("/var/run/netns/{name}");
    let file = std::fs::File::open(&path).map_err(|source| TunTapError::Namespace {
        netns: name.to_string(),
        detail: source.to_string(),
    })?;
    nix::sched::setns(&file, nix::sched::CloneFlags::CLONE_NEWNET).map_err(|errno| {
        TunTapError::Namespace {
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

fn create_and_persist(name: &str) -> Result<(), TunTapError> {
    let mut config = tun::Configuration::default();
    config.tun_name(name).layer(tun::Layer::L2);
    let mut dev = tun::create(&config).map_err(|source| TunTapError::Create {
        name: name.to_string(),
        source,
    })?;
    dev.persist().map_err(|source| TunTapError::Persist {
        name: name.to_string(),
        source,
    })?;
    // Owner uid, not just persistence — see the module doc's "Ownership, not just persistence"
    // section: without this, only root (or a `CAP_NET_ADMIN` holder) can attach to the
    // now-persistent device at all, which would force the unprivileged qemu-microvm launch this
    // exists for right back onto `sudo`.
    let uid = nix::unistd::getuid().as_raw() as i32;
    dev.user(uid).map_err(|source| TunTapError::Owner {
        name: name.to_string(),
        source,
    })?;
    // `dev` drops here, closing the fd this process opened it with — `persist()` above is what
    // makes that safe: the device is already marked to outlive any single fd by the time this
    // happens, so closing ours does not tear it down.
    Ok(())
}

/// Idempotent: tolerates an already-existing tap device, the same contract every `ensure_*` in
/// [`super::netlink`] carries. `netlink::link_exists` is a plain kernel link-existence check —
/// it works for a tap device exactly as it does for a veth or bridge, since by the time this
/// function could observe it, the device is just another netdevice; the tun/tap-specific ioctls
/// are only needed to bring it into existence in the first place.
///
/// Runs the actual creation on a dedicated thread that enters `netns` first when given — a tap
/// device, like everything else in [`super::netlink`], must land in the same namespace the rest
/// of that endpoint's gateway state does, not always the host's default one (the CI netns
/// integration suite provisions a whole disposable gateway inside one).
pub fn ensure_tap(netns: Option<&str>, name: &str) -> Result<(), TunTapError> {
    if netlink::link_exists(netns, name) {
        return Ok(());
    }
    let netns = netns.map(str::to_string);
    let name = name.to_string();
    let handle = std::thread::spawn({
        let name = name.clone();
        move || -> Result<(), TunTapError> {
            if let Some(ns) = &netns {
                enter_netns(ns)?;
            }
            create_and_persist(&name)
        }
    });
    handle.join().unwrap_or_else(|e| {
        Err(TunTapError::ThreadPanic {
            name,
            detail: panic_message(&*e),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // No unprivileged tests here: both `link_exists` and tap creation itself need real kernel
    // access (creation additionally needs CAP_NET_ADMIN). Covered by the privileged
    // `gateway_netns` suite instead (`docs/TASKS.md` #16's own verification note), the same
    // reason `netlink.rs`'s own primitives carry no unit tests of their own.

    #[test]
    fn error_messages_name_the_device() {
        // Cheap coverage that doesn't need a kernel: the error variants carry the device name
        // through to their Display output, which is what a caller's own error-wrapping
        // (`provision::nl`) relies on to say which device failed, not just that one did.
        let err = TunTapError::ThreadPanic {
            name: "vxtap-test".into(),
            detail: "boom".into(),
        };
        assert!(err.to_string().contains("vxtap-test"));
    }
}
