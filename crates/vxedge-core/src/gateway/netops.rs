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
        let mut cmd = match &self.netns {
            Some(ns) => {
                let mut c = Command::new("ip");
                c.args(["netns", "exec", ns, program]);
                c
            }
            None => Command::new(program),
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
        match self.run(program, args) {
            Ok(_) => Ok(()),
            Err(NetOpsError::Command { stderr, .. }) if stderr.contains("File exists") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Run, treating "does not exist" as success (idempotent deletes).
    pub fn run_tolerate_missing(&self, program: &str, args: &[&str]) -> Result<(), NetOpsError> {
        match self.run(program, args) {
            Ok(_) => Ok(()),
            Err(NetOpsError::Command { stderr, .. })
                if stderr.contains("Cannot find device")
                    || stderr.contains("does not exist")
                    || stderr.contains("No such device") =>
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
