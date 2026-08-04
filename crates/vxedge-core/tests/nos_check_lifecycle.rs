//! Privileged integration test: boots a real SONiC-VS instance under QEMU/KVM using
//! `sonic::lifecycle`'s native Rust driver, and proves it can be talked to over its serial
//! console — no Python/`pexpect` dependency at runtime, only at proof-of-concept time
//! (`vxparts/parts/gw/nos/os/sonic-vs/verify-config-parity.py` proved the sequence itself is
//! correct first; this proves the native port of it is too).
//!
//! Ignored by default (needs KVM and the real image, not vendored in this repo). Run with:
//!   SONIC_VS_IMAGE=/path/to/sonic-vs.img cargo test -p vxedge-core --test \
//!     nos_check_lifecycle -- --ignored

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::Duration;
use vxedge_core::sonic::lifecycle::{Console, NosCheckLifecyclePlan, nos_check_down, nos_check_up};

/// Deletes the disposable boot copy and kills the QEMU instance even on panic. Never touches
/// the original image path a caller supplied — see this crate's own boot-mutation warning
/// (`vxparts/parts/gw/nos/os/sonic-vs/build.sh`): QEMU opens a qcow2 read-write by default, and
/// SONiC's first-boot sequence writes to the disk.
struct Cleanup {
    plan: NosCheckLifecyclePlan,
    runtime_dir: PathBuf,
    copy: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = nos_check_down(&self.plan, &self.runtime_dir);
        let _ = std::fs::remove_file(&self.copy);
        // The whole runtime dir, not just the copy: a panic before the test's own final
        // `remove_dir_all` (found live — the first, failing run of this test left an empty
        // `/tmp/vx-nos-check-test-<pid>` behind) must not leak it either.
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

#[test]
#[ignore = "needs KVM and the real SONiC-VS image; run with SONIC_VS_IMAGE=<path> (see module docs)"]
fn boots_and_logs_in_over_the_console() {
    let source_image = std::env::var("SONIC_VS_IMAGE")
        .expect("set SONIC_VS_IMAGE=/path/to/sonic-vs.img (see module docs)");
    let source_image = Path::new(&source_image);
    assert!(source_image.is_file(), "{source_image:?} does not exist");

    let runtime_dir =
        std::env::temp_dir().join(format!("vx-nos-check-test-{}", std::process::id()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    let copy = runtime_dir.join("sonic-vs.img");
    std::fs::copy(source_image, &copy).expect("copy to a disposable boot target");

    let plan = NosCheckLifecyclePlan {
        name: format!("test{}", std::process::id()),
        image: copy.clone(),
        memory_mb: 4096,
        vcpus: 2,
    };
    let guard = Cleanup {
        plan: plan.clone(),
        runtime_dir: runtime_dir.clone(),
        copy: copy.clone(),
    };

    nos_check_up(&plan, &runtime_dir).expect("boot");
    // Idempotency: the same contract every other backend in this codebase carries.
    nos_check_up(&plan, &runtime_dir).expect("second up is a no-op");

    let sock = runtime_dir.join(format!("nos-check-{}.console.sock", plan.name));
    let mut console =
        Console::connect(&sock, &plan.name, Duration::from_secs(10)).expect("connect");

    console
        .expect("login:", Duration::from_secs(150))
        .expect("boot to a login prompt");
    console.send_line("admin").unwrap();
    console
        .expect("assword:", Duration::from_secs(20))
        .expect("password prompt");
    console.send_line("YourPaSsWoRd").unwrap();
    console
        .expect("admin@sonic:~$", Duration::from_secs(20))
        .expect("shell prompt after login");

    console.send_line("show version").unwrap();
    // `expect` returns as soon as its needle appears, not once the whole command's output has
    // arrived -- "SONiC Software Version" is near the *start* of `show version`'s output,
    // "ASIC: vs" further down. Wait for the prompt to return instead, so the accumulated
    // transcript actually contains the full output by the time it's checked.
    let transcript = console
        .expect("admin@sonic:~$", Duration::from_secs(30))
        .expect("show version output, up to the next prompt");
    assert!(
        transcript.contains("SONiC Software Version") && transcript.contains("ASIC: vs"),
        "unexpected show version output: {transcript}"
    );

    // `guard`'s own `Drop` handles teardown -- including on an early panic, which is the case
    // that matters (see its own doc comment).
    drop(guard);
}
