//! Boots a `nos_check` image under QEMU/KVM and talks to it over a serial console — the
//! process-lifecycle half of `docs/ROADMAP.md` #16/#31, mirroring the pidfile-managed pattern
//! `zones::backend`'s `qemu-microvm` already proves (spawn, `-pidfile`, `-daemonize`,
//! kill-with-grace teardown, `/proc/<pid>` existence for status) and `fiveg::lifecycle`'s own
//! naming convention.
//!
//! Deliberately does **not** include "load a rendered config and verify parity" business logic
//! in this module — that is a separate, larger piece built on top of the generic console driver
//! here ([`Console`]), not folded into it. What this module owns: can the image boot, and can
//! something on the other end of the serial console be talked to at all. It was proven
//! end-to-end by hand first (a Python/`pexpect` script, `vxparts/parts/gw/nos/os/sonic-vs/
//! verify-config-parity.py`) before being ported to this native driver, so the sequence itself
//! (boot → serial console → login prompt → `admin`/`YourPaSsWoRd`) is already known-correct;
//! what's new here is running it without a Python/`pexpect` dependency the daemon would
//! otherwise need at runtime.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum NosCheckError {
    #[error("nos_check `{name}`: failed to start QEMU: {detail}")]
    Boot { name: String, detail: String },
    #[error("nos_check `{name}`: could not connect to its console socket: {source}")]
    ConsoleConnect {
        name: String,
        source: std::io::Error,
    },
    #[error("nos_check `{name}`: console error: {source}")]
    ConsoleIo {
        name: String,
        source: std::io::Error,
    },
    #[error(
        "nos_check `{name}`: console never produced `{needle}` within {timeout:?} (saw: {tail})"
    )]
    ExpectTimeout {
        name: String,
        needle: String,
        timeout: Duration,
        tail: String,
    },
}

/// Everything needed to boot one `nos_check` instance. Deliberately not `GatewayPlan`-shaped:
/// `nos_check` is not a zone-attached guest (see `model::NosCheck`'s own doc comment) and
/// carries none of a `NodePlan`'s endpoint/backend fields.
#[derive(Debug, Clone)]
pub struct NosCheckLifecyclePlan {
    pub name: String,
    /// The already-fetched (`vxparts` `build.sh`) disk image path.
    pub image: PathBuf,
    pub memory_mb: u32,
    pub vcpus: u32,
}

impl NosCheckLifecyclePlan {
    fn pidfile(&self, runtime_dir: &Path) -> PathBuf {
        runtime_dir.join(format!("nos-check-{}.pid", self.name))
    }

    fn console_sock(&self, runtime_dir: &Path) -> PathBuf {
        runtime_dir.join(format!("nos-check-{}.console.sock", self.name))
    }
}

fn read_pid(pidfile: &Path) -> Option<i32> {
    std::fs::read_to_string(pidfile).ok()?.trim().parse().ok()
}

pub fn is_running(plan: &NosCheckLifecyclePlan, runtime_dir: &Path) -> bool {
    match read_pid(&plan.pidfile(runtime_dir)) {
        Some(pid) => Path::new(&format!("/proc/{pid}")).exists(),
        None => false,
    }
}

/// Observed state of one `nos_check` instance — just process presence today
/// (`is_running`/`crate::status::ObservedState::{Down,Up}`). What this deliberately does not
/// yet have: a `Staging`/console-verified distinction (booted but not yet confirmed reachable
/// over the console, vs. confirmed logged in and responsive) — that needs the "log in, load
/// config, verify" business logic this module's own doc comment already tracks as a separate,
/// not-yet-built follow-on. Adding a richer state here ahead of that logic would be a status
/// this module can't yet actually confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NosCheckObservation {
    pub present: bool,
}

impl crate::status::Observable for NosCheckObservation {
    fn state(&self) -> crate::status::ObservedState {
        if self.present {
            crate::status::ObservedState::Up
        } else {
            crate::status::ObservedState::Down
        }
    }
}

pub fn nos_check_status(plan: &NosCheckLifecyclePlan, runtime_dir: &Path) -> NosCheckObservation {
    NosCheckObservation {
        present: is_running(plan, runtime_dir),
    }
}

/// Idempotent: a second `up` against an already-running instance is a no-op, the same contract
/// every other backend in this codebase carries.
pub fn nos_check_up(plan: &NosCheckLifecyclePlan, runtime_dir: &Path) -> Result<(), NosCheckError> {
    if is_running(plan, runtime_dir) {
        return Ok(());
    }
    let pidfile = plan.pidfile(runtime_dir);
    let sock = plan.console_sock(runtime_dir);
    // A stale socket from a prior, uncleanly-killed instance would make QEMU's own
    // `server,nowait` bind fail with "address already in use".
    let _ = std::fs::remove_file(&sock);
    let kvm = Path::new("/dev/kvm").exists();

    let mem = plan.memory_mb.to_string();
    let smp = plan.vcpus.to_string();
    let drive = format!("file={},if=virtio,format=qcow2", plan.image.display());
    let serial = format!("unix:{},server,nowait", sock.display());
    let pidfile_arg = pidfile.display().to_string();

    let mut args: Vec<&str> = vec![
        "-m",
        &mem,
        "-smp",
        &smp,
        "-drive",
        &drive,
        "-netdev",
        "user,id=mgmt",
        "-device",
        "virtio-net-pci,netdev=mgmt",
        "-display",
        "none",
        "-monitor",
        "none",
        "-serial",
        &serial,
        "-pidfile",
        &pidfile_arg,
        "-daemonize",
    ];
    if kvm {
        args.push("-enable-kvm");
    }
    let out = std::process::Command::new("qemu-system-x86_64")
        .args(&args)
        .output()
        .map_err(|e| NosCheckError::Boot {
            name: plan.name.clone(),
            detail: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(NosCheckError::Boot {
            name: plan.name.clone(),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// TERM, brief grace, then KILL — same reasoning `zones::backend::microvm_down` already
/// documents: fine for a disposable check instance with nothing to flush cleanly.
pub fn nos_check_down(
    plan: &NosCheckLifecyclePlan,
    runtime_dir: &Path,
) -> Result<(), NosCheckError> {
    if let Some(pid) = read_pid(&plan.pidfile(runtime_dir)) {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .output();
        std::thread::sleep(Duration::from_millis(300));
        if Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
    }
    let _ = std::fs::remove_file(plan.pidfile(runtime_dir));
    let _ = std::fs::remove_file(plan.console_sock(runtime_dir));
    Ok(())
}

/// A minimal, generic driver over a QEMU serial console exposed as a unix socket — accumulates
/// raw bytes (not lines: GRUB's own menu screen is a raw terminal control stream with no
/// reliable newlines, found live authoring this) and waits for an expected substring to appear
/// anywhere in what's been read so far.
///
/// Deliberately generic: it knows nothing about SONiC's own CLI, login flow, or config format —
/// that is a caller concern. The actual "log in, clear a port, load a rendered config, read it
/// back" sequence this exists to eventually support is `docs/ROADMAP.md`'s tracked follow-on, not
/// built here.
pub struct Console {
    stream: UnixStream,
    name: String,
}

impl Console {
    /// Connects to an already-listening QEMU console socket, retrying briefly: `-serial
    /// unix:...,server,nowait` creates the socket file as soon as QEMU starts, but accepting a
    /// connection can lag process spawn by a few hundred milliseconds.
    pub fn connect(sock: &Path, name: &str, timeout: Duration) -> Result<Self, NosCheckError> {
        let deadline = Instant::now() + timeout;
        loop {
            match UnixStream::connect(sock) {
                Ok(stream) => {
                    return Ok(Self {
                        stream,
                        name: name.to_string(),
                    });
                }
                Err(e) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                    let _ = e;
                }
                Err(e) => {
                    return Err(NosCheckError::ConsoleConnect {
                        name: name.to_string(),
                        source: e,
                    });
                }
            }
        }
    }

    /// Reads until `needle` appears anywhere in the accumulated output, returning everything
    /// read so far (including the match) — the caller's own transcript, not just a boolean.
    /// Polls on a short read timeout rather than blocking indefinitely, so the *overall*
    /// `timeout` is honored even if the guest never produces another byte.
    pub fn expect(&mut self, needle: &str, timeout: Duration) -> Result<String, NosCheckError> {
        self.stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|e| NosCheckError::ConsoleIo {
                name: self.name.clone(),
                source: e,
            })?;
        let deadline = Instant::now() + timeout;
        let mut transcript = String::new();
        let mut buf = [0u8; 4096];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    transcript.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if transcript.contains(needle) {
                        return Ok(transcript);
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => {
                    return Err(NosCheckError::ConsoleIo {
                        name: self.name.clone(),
                        source: e,
                    });
                }
            }
            if Instant::now() >= deadline {
                let tail_len = transcript.len().min(200);
                return Err(NosCheckError::ExpectTimeout {
                    name: self.name.clone(),
                    needle: needle.to_string(),
                    timeout,
                    tail: transcript[transcript.len() - tail_len..].to_string(),
                });
            }
        }
    }

    /// Sends one line, `\r`-terminated — a real serial console, not a pipe: the guest's own
    /// getty expects carriage return, not bare `\n`.
    pub fn send_line(&mut self, line: &str) -> Result<(), NosCheckError> {
        write!(self.stream, "{line}\r").map_err(|e| NosCheckError::ConsoleIo {
            name: self.name.clone(),
            source: e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{Observable, ObservedState};

    fn plan() -> NosCheckLifecyclePlan {
        NosCheckLifecyclePlan {
            name: format!("unittest{}", std::process::id()),
            image: PathBuf::from("/nonexistent/sonic-vs.img"),
            memory_mb: 4096,
            vcpus: 2,
        }
    }

    #[test]
    fn nothing_running_reports_down_without_touching_qemu() {
        let tmp = std::env::temp_dir().join(format!("vx-nos-check-unit-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let p = plan();
        assert!(!is_running(&p, &tmp));
        let obs = nos_check_status(&p, &tmp);
        assert!(!obs.present);
        assert_eq!(obs.state(), ObservedState::Down);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_pidfile_naming_a_real_process_reports_up() {
        let tmp = std::env::temp_dir().join(format!("vx-nos-check-unit2-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let p = plan();
        // This test's own process is guaranteed to exist for as long as the test runs.
        std::fs::write(p.pidfile(&tmp), std::process::id().to_string()).unwrap();
        let obs = nos_check_status(&p, &tmp);
        assert!(obs.present);
        assert_eq!(obs.state(), ObservedState::Up);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
