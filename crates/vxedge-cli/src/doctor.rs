//! `vxedge doctor` — host preflight per docs/TESTBED-SETUP.md tiers. Local
//! only (no daemon connection): its whole job is checking whether the host
//! can even run one. Presence checks shell out and parse loosely; the Rust,
//! protoc and buf checks compare numeric versions against the minimums that
//! docs/TESTBED-SETUP.md documents. The Rust minimum is read from
//! `rust-toolchain.toml` at build time (see build.rs), so it follows the pin.

use serde::Serialize;
use std::process::Command;

/// Minimum Rust, from the workspace pin (`rust-toolchain.toml`) via build.rs.
const MIN_RUSTC: &str = env!("VXEDGE_MIN_RUSTC");
/// Floors for the other build tools (docs/TESTBED-SETUP.md, Tier 0).
const MIN_PROTOC: &str = "3.21";
const MIN_BUF: &str = "1.71";

/// A parsed `major.minor.patch` version; `pre` marks a pre-release (`-nightly`, `-beta.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Version {
    nums: (u32, u32, u32),
    pre: bool,
}

/// Parses `1.99`, `1.99.1` or `1.99.1-nightly`; missing components are 0.
fn parse_token(token: &str) -> Option<Version> {
    let (core, pre) = match token.split_once(['-', '+']) {
        Some((core, _)) => (core, true),
        None => (token, false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        nums: (major, minor, patch),
        pre,
    })
}

/// The first version-looking word of a tool's `--version` line
/// (`rustc 1.99.1 (...)`, `libprotoc 3.21.12`, `1.71.0`).
fn parse_version(line: &str) -> Option<Version> {
    line.split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(parse_token)
}

/// True when `line` reports a version at or above `min`. A pre-release of exactly
/// the minimum (`1.99.1-nightly`) is below the release.
fn meets_floor(line: &str, min: &str) -> bool {
    match (parse_version(line), parse_token(min)) {
        (Some(have), Some(min)) => have.nums > min.nums || (have.nums == min.nums && !have.pre),
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Ok,
    Warn,
    Missing,
}

#[derive(Debug, Serialize)]
struct Check {
    tier: u8,
    name: &'static str,
    required: bool,
    status: CheckStatus,
    detail: String,
    fix_hint: Option<String>,
}

fn cmd_version(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&out.stderr).into_owned()
    } else {
        text.into_owned()
    };
    text.lines().next().map(|l| l.trim().to_string())
}

fn present(program: &str, args: &[&str]) -> bool {
    cmd_version(program, args).is_some()
}

/// The executable `program` resolves to on PATH, without running it. sudoers rules name a full
/// path, so the sudo check asks about this one.
fn find_on_path(program: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .chain(["/usr/sbin", "/sbin"].map(std::path::PathBuf::from))
        .map(|dir| dir.join(program))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Whether `sudo -n` would run `program` now, without asking for a password: a NOPASSWD rule,
/// or a credential still cached for this terminal. `sudo -l <cmd>` only lists; it runs nothing.
fn sudo_runs_without_password(program: &std::path::Path) -> bool {
    Command::new("sudo")
        .arg("-n")
        .arg("-l")
        .arg(program)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn check(
    tier: u8,
    name: &'static str,
    required: bool,
    ok: bool,
    detail: impl Into<String>,
    fix_hint: Option<&str>,
) -> Check {
    Check {
        tier,
        name,
        required,
        status: if ok {
            CheckStatus::Ok
        } else if required {
            CheckStatus::Missing
        } else {
            CheckStatus::Warn
        },
        detail: detail.into(),
        fix_hint: fix_hint.map(str::to_string),
    }
}

/// `network.shaping`'s gate schedule renders as the kernel's `taprio` qdisc
/// (`vxedge_core::package::kernel_has_taprio`, the probe `Plan` and `up` use). Optional: only an
/// experiment with a gate schedule needs it. Pure, given what the probe found.
fn taprio_check(loaded: bool, available: bool) -> Check {
    check(
        2,
        "taprio (TSN gate schedule)",
        false,
        available,
        if loaded {
            "sch_taprio loaded"
        } else if available {
            "sch_taprio built in or installed for this kernel; `tc` loads it on first use"
        } else {
            "no sch_taprio for this kernel — only needed for `network.shaping`"
        },
        Some(
            "sudo modprobe sch_taprio (a kernel built without CONFIG_NET_SCH_TAPRIO has none; \
             only `network.shaping` needs it)",
        ),
    )
}

fn tier0() -> Vec<Check> {
    let mut c = Vec::new();

    let kernel = cmd_version("uname", &["-r"]).unwrap_or_default();
    let kernel_ok = kernel
        .split(['.', '-'])
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|major| major >= 6)
        .unwrap_or(false);
    c.push(check(
        0,
        "kernel >= 6.8",
        true,
        kernel_ok,
        kernel,
        Some(
            "any modern systemd Linux with kernel >= 6.8 works — see docs/TESTBED-SETUP.md Tier 0",
        ),
    ));

    let rustc = cmd_version("rustc", &["--version"]).unwrap_or_default();
    c.push(check(
        0,
        concat!("Rust >= ", env!("VXEDGE_MIN_RUSTC")),
        true,
        meets_floor(&rustc, MIN_RUSTC),
        if rustc.is_empty() {
            "not found".to_string()
        } else {
            rustc
        },
        Some(&format!(
            "rustup installs the pinned toolchain from rust-toolchain.toml on the first cargo \
             command in the repo (or: rustup toolchain install {MIN_RUSTC})"
        )),
    ));

    let protoc = cmd_version("protoc", &["--version"]).unwrap_or_default();
    c.push(check(
        0,
        "protoc >= 3.21",
        true,
        meets_floor(&protoc, MIN_PROTOC),
        if protoc.is_empty() {
            "not found".to_string()
        } else {
            protoc
        },
        Some("apt install protobuf-compiler (or see https://protobuf.dev/installation)"),
    ));

    let buf = cmd_version("buf", &["--version"]).unwrap_or_default();
    c.push(check(
        0,
        "buf >= 1.71",
        false,
        meets_floor(&buf, MIN_BUF),
        if buf.is_empty() {
            "not found — only needed for proto lint/breaking-check".to_string()
        } else {
            buf
        },
        Some("https://buf.build/docs/installation"),
    ));

    let ip = cmd_version("ip", &["-V"]).unwrap_or_default();
    c.push(check(
        0,
        "iproute2 >= 6.1",
        true,
        !ip.is_empty(),
        if ip.is_empty() {
            "not found".to_string()
        } else {
            ip
        },
        Some("apt install iproute2"),
    ));

    c
}

fn tier1() -> Vec<Check> {
    let mut c = Vec::new();

    let is_root = effective_uid() == 0;
    c.push(check(
        1,
        "CAP_NET_ADMIN",
        true,
        is_root,
        if is_root {
            "running as root".to_string()
        } else {
            "not root — vxedged needs CAP_NET_ADMIN".to_string()
        },
        Some("run vxedged under sudo, or: sudo setcap cap_net_admin+ep target/debug/vxedged"),
    ));

    c.push(check(
        1,
        "nftables",
        true,
        present("nft", &["--version"]),
        cmd_version("nft", &["--version"]).unwrap_or_else(|| "not found".to_string()),
        Some("apt install nftables"),
    ));

    let preempt_rt = cmd_version("uname", &["-v"])
        .map(|v| v.to_lowercase().contains("preempt_rt"))
        .unwrap_or(false);
    c.push(check(
        1,
        "PREEMPT_RT kernel",
        false,
        preempt_rt,
        if preempt_rt {
            "PREEMPT_RT active".to_string()
        } else {
            "generic kernel — fine for first numbers, record as a determinism caveat (§12)"
                .to_string()
        },
        Some("Ubuntu Pro real-time kernel is the upgrade path when it matters"),
    ));

    c
}

fn tier2() -> Vec<Check> {
    let mut c = Vec::new();

    let kvm = std::path::Path::new("/dev/kvm").exists();
    c.push(check(
        2,
        "KVM (/dev/kvm)",
        true,
        kvm,
        if kvm {
            "present".to_string()
        } else {
            "missing".to_string()
        },
        Some("check virtualization is enabled in BIOS/UEFI and the kvm module is loaded"),
    ));

    c.push(check(
        2,
        "QEMU >= 8.x",
        true,
        present("qemu-system-x86_64", &["--version"]),
        cmd_version("qemu-system-x86_64", &["--version"])
            .unwrap_or_else(|| "not found".to_string()),
        Some("apt install qemu-system-x86"),
    ));

    // Optional: only `backend: cloud-hypervisor` needs it, and QEMU serves every other VM.
    c.push(check(
        2,
        "Cloud Hypervisor (tested: v54.0)",
        false,
        present("cloud-hypervisor", &["--version"]),
        cmd_version("cloud-hypervisor", &["--version"]).unwrap_or_else(|| "not found".to_string()),
        Some(
            "needed only for `backend: cloud-hypervisor`: install a release from \
             https://github.com/cloud-hypervisor/cloud-hypervisor/releases",
        ),
    ));

    // Optional: only `backend: qemu-mcu` needs it.
    c.push(check(
        2,
        "QEMU Arm (qemu-system-arm)",
        false,
        present("qemu-system-arm", &["--version"]),
        cmd_version("qemu-system-arm", &["--version"]).unwrap_or_else(|| "not found".to_string()),
        Some("needed only for `backend: qemu-mcu`: apt install qemu-system-arm"),
    ));

    c.push(check(
        2,
        "libvirt >= 10.x",
        true,
        present("virsh", &["--version"]),
        cmd_version("virsh", &["--version"]).unwrap_or_else(|| "not found".to_string()),
        Some("apt install libvirt-daemon-system libvirt-clients"),
    ));

    // vxedged runs `virsh` as its own user, never through sudo (zones::backend::virsh), so
    // what counts is whether *this* user reaches `qemu:///system`. As root that proves nothing.
    let libvirt_as_user = effective_uid() != 0
        && Command::new("virsh")
            .args(["-c", "qemu:///system", "uri"])
            .output()
            .is_ok_and(|o| o.status.success());
    c.push(check(
        2,
        "libvirt reachable without sudo",
        true,
        libvirt_as_user,
        if effective_uid() == 0 {
            "not checked as root: run doctor as the user vxedged runs as".to_string()
        } else if libvirt_as_user {
            "qemu:///system answers this user".to_string()
        } else {
            "qemu:///system refuses this user".to_string()
        },
        Some("sudo usermod -aG libvirt $USER, then start a new login session"),
    ));

    c.push(check(
        2,
        "Open vSwitch (optional variant)",
        false,
        present("ovs-vsctl", &["--version"]),
        cmd_version("ovs-vsctl", &["--version"]).unwrap_or_else(|| "not found".to_string()),
        Some("apt install openvswitch-switch — only needed for the OVS gateway data-plane variant"),
    ));

    c.push(check(
        2,
        "WireGuard tools",
        false,
        present("wg", &["--version"]),
        cmd_version("wg", &["--version"])
            .unwrap_or_else(|| "not found — needed for telemetry/OTA transport links".to_string()),
        Some("apt install wireguard-tools"),
    ));

    // The 5G core and RAN run as Podman pods from published images that vxedged pulls on first
    // use (`mode: s-nssai` slices only), so what the host needs is what those pods need.
    let uid = effective_uid();
    let socket = vxedge_core::containers::user_socket_path(
        uid,
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
    );
    let socket_ok = socket.as_ref().is_ok_and(|p| {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(p).is_ok_and(|m| m.file_type().is_socket())
    });
    c.push(check(
        2,
        "Podman API socket (5G pods, `mode: s-nssai`)",
        false,
        socket_ok,
        match &socket {
            Err(_) => "not checked as root: run doctor as the user vxedged runs as".to_string(),
            Ok(p) if socket_ok => format!("{} is listening", p.display()),
            Ok(p) => format!("{} not found", p.display()),
        },
        Some(
            "as the user vxedged runs as: systemctl --user enable --now podman.socket; \
             loginctl enable-linger $USER",
        ),
    ));

    let pasta = cmd_version("pasta", &["--version"]);
    c.push(check(
        2,
        "passt (pasta networking for the 5G pods)",
        false,
        pasta.is_some(),
        pasta.unwrap_or_else(|| "pasta not found".to_string()),
        Some("apt install passt"),
    ));

    // UERANSIM's NGAP uses the host kernel's SCTP, which is not namespaced.
    let sctp = std::path::Path::new("/sys/module/sctp").exists();
    c.push(check(
        2,
        "SCTP kernel module (5G NGAP)",
        false,
        sctp,
        if sctp {
            "loaded".to_string()
        } else {
            "not loaded".to_string()
        },
        Some(
            "sudo modprobe sctp (once per boot; to load it at boot: \
             echo sctp | sudo tee /etc/modules-load.d/sctp.conf)",
        ),
    ));

    c.push(taprio_check(
        std::path::Path::new("/sys/module/sch_taprio").exists(),
        vxedge_core::package::kernel_has_taprio(),
    ));

    // The 5G transit link and bridge attachment run `sudo ip` and `sudo nsenter` (join a pod's
    // network namespace by PID), and must not stop at a password prompt.
    let missing: Vec<String> = ["ip", "nsenter"]
        .into_iter()
        .filter(|prog| !find_on_path(prog).is_some_and(|p| sudo_runs_without_password(&p)))
        .map(str::to_string)
        .collect();
    c.push(check(
        2,
        "sudo without a password for ip and nsenter (5G pods)",
        false,
        missing.is_empty(),
        if missing.is_empty() {
            "sudo -n allows both (a NOPASSWD rule, or a credential cached right now)".to_string()
        } else {
            format!("sudo -n refuses: {}", missing.join(", "))
        },
        Some(
            "add /usr/bin/nsenter (and /usr/sbin/ip) to the deploying user's NOPASSWD sudoers \
             entry (docs/TESTBED-SETUP.md, Tier 1 and Tier 2)",
        ),
    ));

    let gpu = present("nvidia-smi", &["--query-gpu=name", "--format=csv,noheader"]);
    c.push(check(
        2,
        "NVIDIA GPU + driver (for CARLA/inference)",
        false,
        gpu,
        cmd_version(
            "nvidia-smi",
            &["--query-gpu=name,driver_version", "--format=csv,noheader"],
        )
        .unwrap_or_else(|| "not found — only needed for CARLA + inference workloads".to_string()),
        None,
    ));

    let container_rt = present("docker", &["--version"]) || present("podman", &["--version"]);
    c.push(check(
        2,
        "Container runtime (Docker or Podman)",
        false,
        container_rt,
        if container_rt {
            "present".to_string()
        } else {
            "neither docker nor podman found".to_string()
        },
        Some("needed for digest-pinned service/compute images"),
    ));

    c
}

fn tier3() -> Vec<Check> {
    vec![check(
        3,
        "Tier 3 (Kuksa, CARLA assets, AAOS, Qt, Node)",
        false,
        false,
        "phase 3 not started — nothing to check yet".to_string(),
        None,
    )]
}

/// Effective UID without a `libc` dependency for one syscall — read from /proc instead.
fn effective_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2)?.parse::<u32>().ok())
        })
        .unwrap_or(u32::MAX)
}

#[derive(Serialize)]
struct Report {
    target_tier: u8,
    passed: bool,
    checks: Vec<Check>,
}

pub fn run(target_tier: u8, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut checks = tier0();
    checks.extend(tier1());
    checks.extend(tier2());
    checks.extend(tier3());

    let passed = !checks
        .iter()
        .any(|c| c.tier <= target_tier && c.required && c.status == CheckStatus::Missing);

    if json {
        let report = Report {
            target_tier,
            passed,
            checks,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("== vxedge doctor (target tier {target_tier}, per docs/TESTBED-SETUP.md) ==");
        for c in &checks {
            let symbol = match c.status {
                CheckStatus::Ok => "✓",
                CheckStatus::Warn => "○",
                CheckStatus::Missing => {
                    if c.tier <= target_tier && c.required {
                        "✗"
                    } else {
                        "○"
                    }
                }
            };
            let scope = if c.tier > target_tier {
                format!(" (tier {} — beyond target, informational)", c.tier)
            } else if !c.required {
                " (optional)".to_string()
            } else {
                String::new()
            };
            println!("[{symbol}] {}{scope}: {}", c.name, c.detail);
            if c.status != CheckStatus::Ok
                && c.tier <= target_tier
                && c.required
                && let Some(hint) = &c.fix_hint
            {
                println!("    fix: {hint}");
            }
        }
        println!();
        if passed {
            println!("== all required tier <= {target_tier} checks pass ==");
        } else {
            println!("== FAILED: one or more required tier <= {target_tier} checks are missing ==");
        }
    }

    if !passed {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An optional check: without the module it warns and hints `modprobe`, never fails tier 2.
    #[test]
    fn taprio_is_an_optional_tier2_check() {
        let missing = taprio_check(false, false);
        assert_eq!(missing.tier, 2);
        assert_eq!(missing.name, "taprio (TSN gate schedule)");
        assert!(!missing.required);
        assert_eq!(missing.status, CheckStatus::Warn);
        assert!(
            missing
                .fix_hint
                .as_deref()
                .unwrap()
                .starts_with("sudo modprobe sch_taprio")
        );
        assert_eq!(taprio_check(true, true).status, CheckStatus::Ok);
        let installed = taprio_check(false, true);
        assert_eq!(installed.status, CheckStatus::Ok);
        assert!(
            installed.detail.contains("first use"),
            "{}",
            installed.detail
        );
    }

    #[test]
    fn parses_tool_version_lines() {
        let v = |s: &str| parse_version(s).map(|v| (v.nums, v.pre));
        assert_eq!(
            v("rustc 1.99.1 (58cf8f9c3 2026-10-13)"),
            Some(((1, 99, 1), false))
        );
        assert_eq!(
            v("rustc 1.93.1 (01f6ddf75 2026-02-11) (built from a source tarball)"),
            Some(((1, 93, 1), false))
        );
        assert_eq!(
            v("rustc 1.100.0-nightly (abc 2026-11-01)"),
            Some(((1, 100, 0), true))
        );
        assert_eq!(v("libprotoc 3.21.12"), Some(((3, 21, 12), false)));
        assert_eq!(v("libprotoc 36.0"), Some(((36, 0, 0), false)));
        assert_eq!(v("1.71.0"), Some(((1, 71, 0), false)));
        assert_eq!(v(""), None);
        assert_eq!(v("not found"), None);
    }

    #[test]
    fn rust_floor_compares_numbers_not_substrings() {
        assert!(meets_floor("rustc 1.99.1 (x 2026-10-13)", "1.99.1"));
        assert!(meets_floor("rustc 1.99.2 (x 2026-10-20)", "1.99.1"));
        assert!(meets_floor("rustc 1.100.0 (x 2026-11-20)", "1.99.1"));
        assert!(meets_floor("rustc 2.0.0 (x 2030-01-01)", "1.99.1"));
        // The old `contains("1.96")` test got these wrong in both directions.
        assert!(!meets_floor("rustc 1.99.0 (x 2026-09-01)", "1.99.1"));
        assert!(!meets_floor("rustc 1.96.1 (x 2026-06-26)", "1.99.1"));
        assert!(!meets_floor("rustc 1.9.0 (x 2018-01-01)", "1.99.1"));
        assert!(!meets_floor(
            "rustc 1.93.1 (x 2026-02-11) (built from a source tarball)",
            "1.99.1"
        ));
    }

    #[test]
    fn prerelease_of_the_minimum_does_not_count() {
        assert!(!meets_floor(
            "rustc 1.99.1-nightly (x 2026-10-01)",
            "1.99.1"
        ));
        assert!(meets_floor(
            "rustc 1.100.0-nightly (x 2026-11-01)",
            "1.99.1"
        ));
    }

    #[test]
    fn unparseable_or_missing_output_fails() {
        assert!(!meets_floor("", "1.99.1"));
        assert!(!meets_floor("not found", "1.99.1"));
        assert!(!meets_floor("rustc 1.99.1", "garbage"));
    }

    #[test]
    fn protoc_and_buf_floors() {
        assert!(meets_floor("libprotoc 3.21.12", MIN_PROTOC));
        assert!(meets_floor("libprotoc 36.0", MIN_PROTOC));
        assert!(!meets_floor("libprotoc 3.20.3", MIN_PROTOC));
        assert!(meets_floor("1.71.0", MIN_BUF));
        assert!(!meets_floor("1.70.9", MIN_BUF));
    }

    #[test]
    fn rust_minimum_follows_the_toolchain_pin() {
        let pin = include_str!("../../../rust-toolchain.toml");
        let channel = pin
            .lines()
            .find_map(|l| l.trim().strip_prefix("channel"))
            .and_then(|l| l.split('"').nth(1))
            .expect("rust-toolchain.toml has a channel");
        assert_eq!(MIN_RUSTC, channel);
    }
}
