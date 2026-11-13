//! `qemu-mcu`: microcontroller firmware on an emulated board (ARCHITECTURE.md §9).
//!
//! The node's bundle is `firmware.elf` and a `meta.json` naming the board. QEMU runs it under TCG
//! (there is no KVM for M-profile cores), with the board's own NIC on the node's tap. An MCU has
//! no kernel command line, so the same `vx.ip=<cidr> vx.node=<id>` words reach the firmware as
//! its semihosting command line, which it reads with `SYS_GET_CMDLINE`. The console is the
//! board's first UART, written to `<runtime>/<node>.serial.log` as a `qemu-microvm` guest's is.
//!
//! Status and `down` are `qemu-microvm`'s: the same pidfile, and the same `vx.node=` marker on
//! QEMU's command line, here inside `-semihosting-config`.

use std::path::{Path, PathBuf};

use super::backend::ZoneBackendError;
use super::plan::NodePlan;

/// The emulator a `qemu-mcu` node runs.
pub const QEMU_MCU_BIN: &str = "qemu-system-arm";

/// The firmware file in a `qemu-mcu` bundle.
pub const FIRMWARE: &str = "firmware.elf";

/// A board `qemu-mcu` boots: its QEMU machine, CPU and NIC model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    /// ARM MPS2 with the AN385 image: a Cortex-M3 and a LAN9118 (SMSC LAN9220 family) NIC.
    Mps2An385,
}

impl Board {
    const ALL: [Board; 1] = [Board::Mps2An385];

    pub fn machine(self) -> &'static str {
        match self {
            Board::Mps2An385 => "mps2-an385",
        }
    }

    fn cpu(self) -> &'static str {
        match self {
            Board::Mps2An385 => "cortex-m3",
        }
    }

    fn nic_model(self) -> &'static str {
        match self {
            Board::Mps2An385 => "lan9118",
        }
    }

    /// The instruction set `meta.json`'s `arch` must name, when it names one.
    fn arch(self) -> &'static str {
        match self {
            Board::Mps2An385 => "armv7m",
        }
    }
}

/// The bundle's board, from its `meta.json` (`machine`, or the `org.vxedge.machine` a published
/// vxparts bundle carries). Required: a firmware image is built for one board.
pub(super) fn bundle_board(np: &NodePlan, dir: &Path) -> Result<Board, ZoneBackendError> {
    let refuse = |detail: String| ZoneBackendError::Runtime {
        node: np.node_id.clone(),
        detail,
    };
    let supported = || Board::ALL.map(|b| format!("`{}`", b.machine())).join(", ");
    let text = std::fs::read_to_string(dir.join("meta.json")).map_err(|e| {
        refuse(format!(
            "a qemu-mcu bundle needs a meta.json naming its board ({}); reading {}: {e}",
            supported(),
            dir.join("meta.json").display()
        ))
    })?;
    let meta: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| refuse(format!("the bundle's meta.json is not JSON: {e}")))?;
    let get = |keys: [&str; 2]| {
        keys.iter()
            .find_map(|k| meta.get(*k).and_then(|v| v.as_str()))
    };
    let machine = get(["machine", "org.vxedge.machine"]).ok_or_else(|| {
        refuse(format!(
            "the bundle's meta.json names no `machine`; qemu-mcu boots {}",
            supported()
        ))
    })?;
    let board = Board::ALL
        .into_iter()
        .find(|b| b.machine() == machine)
        .ok_or_else(|| {
            refuse(format!(
                "the bundle's meta.json names machine `{machine}`; qemu-mcu boots {}",
                supported()
            ))
        })?;
    if let Some(arch) = get(["arch", "org.vxedge.arch"]).filter(|a| *a != board.arch()) {
        return Err(refuse(format!(
            "the bundle's meta.json names arch `{arch}`, but `{}` is `{}`",
            board.machine(),
            board.arch()
        )));
    }
    Ok(board)
}

/// The bundle's firmware, which must exist.
pub(super) fn firmware_path(np: &NodePlan, dir: &Path) -> Result<PathBuf, ZoneBackendError> {
    let firmware = dir.join(FIRMWARE);
    if firmware.is_file() {
        Ok(firmware)
    } else {
        Err(ZoneBackendError::Runtime {
            node: np.node_id.clone(),
            detail: format!(
                "a qemu-mcu bundle is `{FIRMWARE}` and `meta.json`; {} is missing",
                firmware.display()
            ),
        })
    }
}

/// A value inside a QEMU option list, where a comma separates options and `,,` is a literal one.
fn opt_escape(value: &str) -> String {
    value.replace(',', ",,")
}

/// The firmware's command line as semihosting arguments: one `arg=` per word.
pub(super) fn semihosting_config(np: &NodePlan) -> String {
    let ip = np.ip_cidr.as_deref().unwrap_or("");
    let mut config = String::from("enable=on,target=native");
    for word in [
        format!("vx.ip={ip}"),
        format!("vx.node={}", np.node_id),
        format!("{}{}", super::foreign::SLOT_MARKER, np.instance),
    ] {
        config.push_str(",arg=");
        config.push_str(&opt_escape(&word));
    }
    config
}

/// The QEMU command line of a `qemu-mcu` node. Pure, so it is tested.
pub(super) fn argv(
    np: &NodePlan,
    runtime_dir: &Path,
    firmware: &Path,
    board: Board,
) -> Vec<String> {
    let name = np.runtime_name();
    let pidfile = runtime_dir.join(format!("{name}.pid"));
    let serial = format!(
        "file:{}",
        runtime_dir.join(format!("{name}.serial.log")).display()
    );
    let nic = format!(
        "tap,ifname={},script=no,downscript=no,model={},mac={}",
        opt_escape(&np.endpoint),
        board.nic_model(),
        np.guest_mac()
    );
    let mut args: Vec<String> = Vec::new();
    if !np.pinned_cpus.is_empty() {
        // As for qemu-microvm: taskset keeps the whole QEMU thread group on the cell CPUs.
        args.extend([
            "taskset".into(),
            "-c".into(),
            np.pinned_cpus
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ]);
    }
    args.push(QEMU_MCU_BIN.into());
    args.extend(
        [
            "-M",
            board.machine(),
            "-cpu",
            board.cpu(),
            // No default devices: QEMU refuses `-nographic` with `-daemonize` while a default
            // serial, parallel or monitor would go to stdio. The board's UART and NIC stay.
            "-nodefaults",
            "-no-user-config",
            "-nographic",
            "-monitor",
            "none",
            "-serial",
            &serial,
            "-kernel",
            &firmware.display().to_string(),
            "-semihosting-config",
            &semihosting_config(np),
            "-nic",
            &nic,
            "-pidfile",
            &pidfile.display().to_string(),
            "-daemonize",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Backend;

    fn node(dir: &Path) -> NodePlan {
        NodePlan {
            instance: crate::instance::Instance::FIRST,
            zone_id: "front-left".into(),
            node_id: "ecu-m3".into(),
            role: crate::model::DeviceRole::Ecu,
            backend: Backend::QemuMcu,
            endpoint: "vz-ecu-m3".into(),
            peer: "vz-ecu-m3".into(),
            kind: crate::gateway::plan::EndpointKind::Tap,
            vlan: 101,
            slice: Some("adas".into()),
            ip_cidr: Some("10.20.1.21/24".into()),
            extra_cidrs: Vec::new(),
            bundle: Some(dir.to_path_buf()),
            module: None,
            remote_bundle: None,
            image: None,
            run: Default::default(),
            pinned_cpus: Vec::new(),
            qemu: Default::default(),
        }
    }

    fn arg_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let i = argv.iter().position(|a| a == flag).expect(flag);
        &argv[i + 1]
    }

    fn bundle(meta: Option<&str>) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("vxedge-mcu-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(meta) = meta {
            std::fs::write(dir.join("meta.json"), meta).unwrap();
        }
        dir
    }

    #[test]
    fn the_firmware_gets_its_address_and_node_through_semihosting() {
        let np = node(Path::new("/b"));
        let argv = argv(
            &np,
            Path::new("/run/vx"),
            Path::new("/b/firmware.elf"),
            Board::Mps2An385,
        );
        assert_eq!(argv[0], "qemu-system-arm");
        assert_eq!(arg_after(&argv, "-M"), "mps2-an385");
        assert_eq!(arg_after(&argv, "-cpu"), "cortex-m3");
        assert_eq!(arg_after(&argv, "-kernel"), "/b/firmware.elf");
        assert_eq!(
            arg_after(&argv, "-semihosting-config"),
            "enable=on,target=native,arg=vx.ip=10.20.1.21/24,arg=vx.node=ecu-m3,arg=vx.slot=0"
        );
        assert_eq!(
            arg_after(&argv, "-nic"),
            format!(
                "tap,ifname=vz-ecu-m3,script=no,downscript=no,model=lan9118,mac={}",
                np.guest_mac()
            )
        );
        assert_eq!(
            arg_after(&argv, "-serial"),
            format!("file:/run/vx/{}.serial.log", np.runtime_name())
        );
        assert!(
            !argv.iter().any(|a| a == "-enable-kvm"),
            "an MCU runs under TCG"
        );
        assert!(
            !argv.iter().any(|a| a == "-append"),
            "an MCU has no kernel command line"
        );
        assert!(
            argv.iter().any(|a| a == "-nodefaults"),
            "QEMU refuses -nographic with -daemonize while default devices are on"
        );
    }

    #[test]
    fn a_pinned_mcu_node_runs_under_taskset() {
        let np = NodePlan {
            pinned_cpus: vec![6, 7],
            ..node(Path::new("/b"))
        };
        let argv = argv(
            &np,
            Path::new("/r"),
            Path::new("/b/firmware.elf"),
            Board::Mps2An385,
        );
        assert_eq!(argv[..4], ["taskset", "-c", "6,7", "qemu-system-arm"]);
    }

    #[test]
    fn a_comma_in_a_value_is_escaped_for_qemu() {
        assert_eq!(opt_escape("a,b"), "a,,b");
    }

    #[test]
    fn a_bundle_names_a_board_qemu_mcu_boots() {
        let ok = bundle(Some(r#"{"machine":"mps2-an385","arch":"armv7m"}"#));
        assert_eq!(bundle_board(&node(&ok), &ok).unwrap(), Board::Mps2An385);
        let published = bundle(Some(r#"{"org.vxedge.machine":"mps2-an385"}"#));
        assert_eq!(
            bundle_board(&node(&published), &published).unwrap(),
            Board::Mps2An385
        );
        for (meta, says) in [
            (None, "needs a meta.json"),
            (Some(r#"{"arch":"armv7m"}"#), "names no `machine`"),
            (
                Some(r#"{"machine":"lm3s6965evb"}"#),
                "machine `lm3s6965evb`",
            ),
            (
                Some(r#"{"machine":"mps2-an385","arch":"x86_64"}"#),
                "arch `x86_64`",
            ),
        ] {
            let dir = bundle(meta);
            let e = bundle_board(&node(&dir), &dir).unwrap_err().to_string();
            assert!(e.contains(says), "{e}");
            assert!(
                e.contains("mps2-an385"),
                "the refusal names what is supported: {e}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
        let _ = std::fs::remove_dir_all(&ok);
        let _ = std::fs::remove_dir_all(&published);
    }

    #[test]
    fn a_bundle_without_its_firmware_is_refused() {
        let dir = bundle(Some(r#"{"machine":"mps2-an385"}"#));
        let e = firmware_path(&node(&dir), &dir).unwrap_err().to_string();
        assert!(e.contains("firmware.elf"), "{e}");
        std::fs::write(dir.join(FIRMWARE), b"elf").unwrap();
        assert_eq!(
            firmware_path(&node(&dir), &dir).unwrap(),
            dir.join(FIRMWARE)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
