//! Thin wrapper over iproute2 (`ip`, `bridge`, `tc`) with JSON observation.
//!
//! Deliberate engineering call: provisioning shells out to iproute2 —
//! argument-vector execs of validated, generated tokens (no shell
//! interpolation) — with `-json` for reads. `netlink.rs` covers part of the
//! kernel-bridge path natively; the plan/reconcile logic above this layer does
//! not depend on which mechanism runs.

use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum NetOpsError {
    #[error(
        "insufficient privileges for `{cmdline}` — vxedged needs CAP_NET_ADMIN for \
         `ip`/`bridge`/`tc` (cache a sudo credential, or configure NOPASSWD sudo for \
         those commands; `setcap` on the vxedged binary does not work here — capabilities \
         don't propagate to the child processes this shells out to)"
    )]
    Permission { cmdline: String },
    #[error("`{cmdline}` failed: {stderr}")]
    Command { cmdline: String, stderr: String },
    /// `sudo -n` refused for want of a credential or a rule: a host requirement, not a
    /// networking failure.
    #[error(
        "vxedged cannot escalate `{cmdline}`: `sudo` wants a password and a daemon has no \
         terminal. Either run vxedged as root, or grant passwordless sudo for that command \
         (TESTBED-SETUP.md). `setcap` on the vxedged binary does not cover it: capabilities do \
         not propagate across exec to child processes"
    )]
    NoSudo { cmdline: String },
    /// This process cannot enter a named network namespace (`setns(2)` needs `CAP_SYS_ADMIN`).
    #[error(
        "vxedged cannot enter network namespace `{netns}`: it needs CAP_SYS_ADMIN \
         (`sudo setcap cap_net_admin,cap_sys_admin+ep <path-to-vxedged>`, or run it as root)"
    )]
    CannotEnterNetns { netns: String },
    #[error("cannot execute `{program}`: {source} (is iproute2 installed?)")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("cannot parse `{cmdline}` output: {detail}")]
    Parse { cmdline: String, detail: String },
}

/// A device's ingress qdisc slot (`TC_H_INGRESS`), which holds either an `ingress` or a `clsact`
/// qdisc. `tc qdisc del ... parent` this slot removes whichever is there, where a delete by kind
/// fails on the other.
pub(crate) const INGRESS_SLOT: &str = "ffff:fff1";

/// Executes iproute2 commands, optionally inside a named network namespace
/// (`ip netns exec <ns> ...`) — that is what makes the integration tests and
/// future multi-gateway setups isolated.
#[derive(Debug, Clone, Default)]
pub struct Shell {
    pub netns: Option<String>,
    /// Prefix every command with `sudo`. Off by default — preserves exact
    /// prior behavior for tests and doctor checks that already run their
    /// whole process privileged (`gateway_netns.rs`'s own doc comment: "run
    /// under sudo"). `vxedged`'s real deployment turns this on instead of
    /// running the daemon itself as root: found live 2026-07-17 that
    /// `setcap cap_net_admin+ep` on the `vxedged` binary does *not* grant
    /// the capability to `ip`/`bridge`/`tc` — Linux capabilities don't
    /// propagate across `fork`+`exec` to a separate child binary without
    /// explicit ambient-set handling neither `setcap ep` nor this shell-out
    /// code performs. `sudo`-wrapping here mirrors the pattern already
    /// proven live in `fiveg::lifecycle`'s `host_ip`/`nsenter_run`, and
    /// keeps `vxedged` itself an unprivileged process — required for
    /// `fiveg`'s own child `podman` calls to keep resolving the deploying
    /// user's own image storage rather than root's separate one.
    pub escalate: bool,
}

/// Whether this process's *children* will already have `CAP_NET_ADMIN`.
///
/// The test is **effective uid 0**, not this process's own capability set, and the distinction
/// is the whole point: capabilities do not propagate across `fork`+`exec` to a separate binary
/// without ambient-set handling this shell-out code does not do — the live 2026-07-17 finding
/// recorded on [`Shell::escalate`]. So a `vxedged` carrying `cap_net_admin+ep` still spawns an
/// unprivileged `ip`. Running as uid 0 is different: the child *is* root and inherits
/// everything.
///
/// Read from `/proc/self/status` rather than `geteuid(2)` because `unsafe` is effectively
/// forbidden here and this needs no new dependency. Cached: a process's euid cannot change
/// underneath us.
pub(crate) fn children_are_privileged() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix("Uid:"))
                    // `Uid:  real  effective  saved  fs`
                    .and_then(|v| v.split_whitespace().nth(1).map(str::to_owned))
            })
            .is_some_and(|euid| euid == "0")
    })
}

/// Whether this process can enter a named network namespace: `setns(2)` needs `CAP_SYS_ADMIN`
/// in its effective set, or uid 0. Read from `/proc/self/status`'s `CapEff` like
/// [`children_are_privileged`], and cached for the same reason.
pub fn can_enter_netns() -> bool {
    const CAP_SYS_ADMIN: u32 = 21;
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix("CapEff:"))
                    .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
            })
            .is_some_and(|caps| caps & (1 << CAP_SYS_ADMIN) != 0)
    })
}

/// Whether registering a named namespace has to go through `ip` and sudo. `/run/netns` is
/// root's, and `cap_net_admin,cap_sys_admin` do not reach a directory another user owns, so a
/// `vxedged` under `setcap` cannot register a namespace itself. Everything after that works with
/// its own capabilities.
pub fn escalates_netns_registry(sh: &Shell) -> bool {
    sh.escalate && !children_are_privileged()
}

/// Register the named namespace `name` unless it exists. Registry operations run in the host
/// context, never nested inside another namespace: a bind mount made from inside one does not
/// propagate.
pub fn ensure_named_netns(sh: &Shell, name: &str) -> Result<(), NetOpsError> {
    if super::netlink::netns_exists(name) {
        return Ok(());
    }
    if escalates_netns_registry(sh) {
        sh.run_host("ip", &["netns", "add", name]).map(|_| ())
    } else {
        super::netlink::ensure_netns(name).map_err(|e| NetOpsError::Command {
            cmdline: format!("ip netns add {name}"),
            stderr: e.to_string(),
        })
    }
}

/// Delete the named namespace `name` if it exists. Its interfaces go with it; a process still
/// inside keeps the namespace itself alive, so callers stop those first.
pub fn delete_named_netns(sh: &Shell, name: &str) -> Result<(), NetOpsError> {
    if !super::netlink::netns_exists(name) {
        return Ok(());
    }
    if escalates_netns_registry(sh) {
        sh.run_host("ip", &["netns", "del", name]).map(|_| ())
    } else {
        super::netlink::delete_netns(name).map_err(|e| NetOpsError::Command {
            cmdline: format!("ip netns del {name}"),
            stderr: e.to_string(),
        })
    }
}

impl Shell {
    pub fn run(&self, program: &str, args: &[&str]) -> Result<String, NetOpsError> {
        self.run_inner(program, args, true, None)
    }

    /// Run in the host context, ignoring the netns wrapper. Required for
    /// `ip netns add/del/exec/list`: named-netns registry entries are
    /// per-mount-namespace, and creating one from inside `ip netns exec`
    /// leaves a bind mount that does not propagate (caught in CI: the
    /// subsequent veth move fails with EINVAL on an empty registry file).
    pub fn run_host(&self, program: &str, args: &[&str]) -> Result<String, NetOpsError> {
        self.run_inner(program, args, false, None)
    }

    /// Like [`Self::run`], but pipes `stdin` to the child instead of leaving its stdin
    /// unattached. The one consumer today: `wg set <tunnel> private-key /dev/stdin` — a
    /// generated tunnel private key is passed straight from `wg genkey`'s stdout into this, and
    /// is never written to disk anywhere in this process, matching the "no secrets in manifests
    /// or on disk" rule the rest of the codebase already follows for daemon state.
    pub fn run_with_stdin(
        &self,
        program: &str,
        args: &[&str],
        stdin: &str,
    ) -> Result<String, NetOpsError> {
        self.run_inner(program, args, true, Some(stdin))
    }

    fn run_inner(
        &self,
        program: &str,
        args: &[&str],
        wrapped: bool,
        stdin: Option<&str>,
    ) -> Result<String, NetOpsError> {
        let mut full_args: Vec<String> = Vec::new();
        let exe = match (&self.netns, wrapped) {
            (Some(ns), true) => {
                full_args.extend(["netns", "exec", ns, program].iter().map(|s| s.to_string()));
                "ip"
            }
            _ => program,
        };
        full_args.extend(args.iter().map(|s| s.to_string()));

        // Escalation is a *need*, not a setting. Already running as uid 0 (the documented
        // campaign path — `sudo ./scripts/run-exp-001.sh`) makes `sudo` a pointless subprocess
        // per netlink call, and an outright failure on a host that has no `sudo` installed at
        // all, which is the normal case in a container or CI image.
        let escalate = self.escalate && !children_are_privileged();
        let mut cmd = if escalate {
            let mut c = Command::new("sudo");
            // Never prompt. A daemon has no terminal, so an interactive `sudo` would block
            // until it times out and then report a TTY error that says nothing about the real
            // fix. `-n` fails immediately and lets the branch below explain it.
            c.arg("-n");
            c.arg(exe);
            c
        } else {
            Command::new(exe)
        };
        cmd.args(&full_args);
        let cmdline = format!(
            "{}{exe} {}",
            if escalate { "sudo " } else { "" },
            full_args.join(" ")
        );
        let out = match stdin {
            None => cmd.output().map_err(|source| NetOpsError::Spawn {
                program: exe.to_string(),
                source,
            })?,
            Some(data) => {
                use std::io::Write;
                use std::process::Stdio;
                cmd.stdin(Stdio::piped());
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());
                let mut child = cmd.spawn().map_err(|source| NetOpsError::Spawn {
                    program: exe.to_string(),
                    source,
                })?;
                child
                    .stdin
                    .take()
                    .expect("stdin piped above")
                    .write_all(data.as_bytes())
                    .map_err(|source| NetOpsError::Spawn {
                        program: exe.to_string(),
                        source,
                    })?;
                child
                    .wait_with_output()
                    .map_err(|source| NetOpsError::Spawn {
                        program: exe.to_string(),
                        source,
                    })?
            }
        };
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // `sudo -n` refusing for want of credentials is not a networking failure, and reporting
        // it as one sends people debugging their manifest. Name both fixes instead.
        if escalate && (stderr.contains("password is required") || stderr.contains("askpass")) {
            return Err(NetOpsError::NoSudo { cmdline });
        }
        if stderr.contains("Operation not permitted") || stderr.contains("Permission denied") {
            Err(NetOpsError::Permission { cmdline })
        } else {
            Err(NetOpsError::Command { cmdline, stderr })
        }
    }

    /// Run, treating "already exists" as success (idempotent creates).
    pub fn run_tolerate_exists(&self, program: &str, args: &[&str]) -> Result<(), NetOpsError> {
        Self::tolerate_exists(self.run(program, args))
    }

    /// Host-context variant of [`Self::run_tolerate_exists`].
    pub fn run_host_tolerate_exists(
        &self,
        program: &str,
        args: &[&str],
    ) -> Result<(), NetOpsError> {
        Self::tolerate_exists(self.run_host(program, args))
    }

    fn tolerate_exists(res: Result<String, NetOpsError>) -> Result<(), NetOpsError> {
        match res {
            Ok(_) => Ok(()),
            // iproute2 spells EEXIST differently per subsystem: links say
            // "File exists", addresses say "Address already assigned".
            Err(NetOpsError::Command { stderr, .. })
                if stderr.contains("File exists") || stderr.contains("already assigned") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Run, treating "does not exist" as success (idempotent deletes).
    pub fn run_tolerate_missing(&self, program: &str, args: &[&str]) -> Result<(), NetOpsError> {
        Self::tolerate_missing(self.run(program, args))
    }

    /// Host-context variant of [`Self::run_tolerate_missing`].
    pub fn run_host_tolerate_missing(
        &self,
        program: &str,
        args: &[&str],
    ) -> Result<(), NetOpsError> {
        Self::tolerate_missing(self.run_host(program, args))
    }

    fn tolerate_missing(res: Result<String, NetOpsError>) -> Result<(), NetOpsError> {
        match res {
            Ok(_) => Ok(()),
            // `tc qdisc del dev <x> parent ffff:fff1` on a device whose ingress-slot qdisc is
            // already gone: the kernel finds the placeholder left in the slot, whose handle is 0,
            // and answers "Cannot delete qdisc with handle of zero" ("Invalid handle" on older
            // kernels). Only that command reads it as absent: elsewhere the same text is a real
            // error.
            Err(NetOpsError::Command { cmdline, stderr })
                if (stderr.contains("handle of zero") || stderr.contains("Invalid handle"))
                    && cmdline.contains("tc qdisc del ")
                    && cmdline.ends_with(&format!(" parent {INGRESS_SLOT}")) =>
            {
                Ok(())
            }
            Err(NetOpsError::Command { stderr, .. })
                if stderr.contains("Cannot find device")
                    || stderr.contains("does not exist")
                    || stderr.contains("No such device")
                    || stderr.contains("No such file")
                    // `tc qdisc del dev <x> parent ffff:fff1` on a device with no ingress-slot
                    // qdisc yet
                    // (gateway::provision::apply_impairment's own first-apply case) — a third
                    // spelling of "already absent," alongside links' and addresses' own, that
                    // only a live kernel surfaces: no fixture reproduces exact iproute2 stderr
                    // text.
                    || stderr.contains("Cannot find specified qdisc") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn link_exists(&self, name: &str) -> bool {
        self.run("ip", &["-j", "link", "show", "dev", name]).is_ok()
    }

    pub fn json(&self, program: &str, args: &[&str]) -> Result<serde_json::Value, NetOpsError> {
        let cmdline = format!("{program} {}", args.join(" "));
        let out = self.run(program, args)?;
        serde_json::from_str(&out).map_err(|e| NetOpsError::Parse {
            cmdline,
            detail: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(cmdline: &str, stderr: &str) -> Result<String, NetOpsError> {
        Err(NetOpsError::Command {
            cmdline: cmdline.into(),
            stderr: stderr.into(),
        })
    }

    #[test]
    fn an_ingress_slot_already_empty_reads_as_absent() {
        for stderr in [
            "Error: Cannot delete qdisc with handle of zero.",
            "Error: Invalid handle.",
            "Error: Cannot find specified qdisc on specified device.",
        ] {
            assert!(
                Shell::tolerate_missing(failed("tc qdisc del dev vxoep1 parent ffff:fff1", stderr))
                    .is_ok(),
                "{stderr}"
            );
        }
        assert!(
            Shell::tolerate_missing(failed(
                "sudo ip netns exec vxh0 tc qdisc del dev vxup0p parent ffff:fff1",
                "Error: Cannot delete qdisc with handle of zero."
            ))
            .is_ok(),
            "escalated or in a namespace, it is the same command"
        );
    }

    #[test]
    fn invalid_handle_elsewhere_is_still_an_error() {
        assert!(
            Shell::tolerate_missing(failed(
                "tc qdisc del dev vxup0 root handle 1:",
                "Error: Invalid handle."
            ))
            .is_err()
        );
        assert!(
            Shell::tolerate_missing(failed(
                "tc qdisc del dev vxup0 root",
                "Error: Cannot delete qdisc with handle of zero."
            ))
            .is_err()
        );
        assert!(
            Shell::tolerate_missing(failed(
                "tc filter add dev vxoep1 ingress protocol all prio 1",
                "Error: Invalid handle."
            ))
            .is_err()
        );
    }
}
