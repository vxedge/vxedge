//! Thin wrapper over iproute2 (`ip`, `bridge`, `tc`) with JSON observation.
//!
//! Deliberate engineering call (recorded in TASKS): provisioning shells out to
//! iproute2 for now — argument-vector execs of validated, generated tokens
//! (no shell interpolation) — with `-json` for reads. A pure-rtnetlink port is
//! a follow-up; the plan/reconcile logic above this layer won't change.

use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum NetOpsError {
    #[error(
        "insufficient privileges for `{cmdline}` — vxedged needs CAP_NET_ADMIN \
         (run under sudo, or `setcap cap_net_admin+ep` on the binary)"
    )]
    Permission { cmdline: String },
    #[error("`{cmdline}` failed: {stderr}")]
    Command { cmdline: String, stderr: String },
    #[error("cannot execute `{program}`: {source} (is iproute2 installed?)")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("cannot parse `{cmdline}` output: {detail}")]
    Parse { cmdline: String, detail: String },
}

/// Executes iproute2 commands, optionally inside a named network namespace
/// (`ip netns exec <ns> ...`) — that is what makes the integration tests and
/// future multi-gateway setups isolated.
#[derive(Debug, Clone, Default)]
pub struct Shell {
    pub netns: Option<String>,
}

impl Shell {
    pub fn run(&self, program: &str, args: &[&str]) -> Result<String, NetOpsError> {
        self.run_inner(program, args, true)
    }

    /// Run in the host context, ignoring the netns wrapper. Required for
    /// `ip netns add/del/exec/list`: named-netns registry entries are
    /// per-mount-namespace, and creating one from inside `ip netns exec`
    /// leaves a bind mount that does not propagate (caught in CI: the
    /// subsequent veth move fails with EINVAL on an empty registry file).
    pub fn run_host(&self, program: &str, args: &[&str]) -> Result<String, NetOpsError> {
        self.run_inner(program, args, false)
    }

    fn run_inner(
        &self,
        program: &str,
        args: &[&str],
        wrapped: bool,
    ) -> Result<String, NetOpsError> {
        let mut cmd = match (&self.netns, wrapped) {
            (Some(ns), true) => {
                let mut c = Command::new("ip");
                c.args(["netns", "exec", ns, program]);
                c
            }
            _ => Command::new(program),
        };
        cmd.args(args);
        let cmdline = format!("{program} {}", args.join(" "));
        let out = cmd.output().map_err(|source| NetOpsError::Spawn {
            program: program.to_string(),
            source,
        })?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
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
            Err(NetOpsError::Command { stderr, .. }) if stderr.contains("File exists") => Ok(()),
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
            Err(NetOpsError::Command { stderr, .. })
                if stderr.contains("Cannot find device")
                    || stderr.contains("does not exist")
                    || stderr.contains("No such device")
                    || stderr.contains("No such file") =>
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
