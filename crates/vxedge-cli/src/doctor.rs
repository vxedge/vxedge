//! `vxedge doctor` — host preflight per docs/TESTBED-SETUP.md tiers. Local
//! only (no daemon connection): its whole job is checking whether the host
//! can even run one. Presence/version checks are best-effort (shell out,
//! parse loosely) — precise semver enforcement isn't worth the complexity
//! for a human-facing report; TESTBED-SETUP.md documents the exact minimums.

use serde::Serialize;
use std::process::Command;

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
        "Rust toolchain",
        true,
        rustc.contains("1.96"),
        if rustc.is_empty() {
            "not found".to_string()
        } else {
            rustc
        },
        Some("rustup handles this automatically from rust-toolchain.toml — just run cargo build"),
    ));

    let protoc = cmd_version("protoc", &["--version"]).unwrap_or_default();
    c.push(check(
        0,
        "protoc >= 3.21",
        true,
        !protoc.is_empty(),
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
        !buf.is_empty(),
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

    c.push(check(
        2,
        "libvirt >= 10.x",
        true,
        present("virsh", &["--version"]),
        cmd_version("virsh", &["--version"]).unwrap_or_else(|| "not found".to_string()),
        Some("apt install libvirt-daemon-system libvirt-clients"),
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

    c.push(check(
        2,
        "Open5GS + UERANSIM (H2, S-NSSAI slicing)",
        false,
        false,
        "not yet packaged by this project — phase-2 driver work, not a host-setup gap".to_string(),
        None,
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
