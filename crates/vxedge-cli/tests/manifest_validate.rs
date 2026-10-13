//! `vxedge manifest validate` must accept exactly what `vehicle up` accepts: a bare vehicle
//! workspace (`kind: vehicle` + `kind: baseline`, no `kind: experiment`).

use std::path::PathBuf;
use std::process::Command;

const VEHICLE: &str = "\
kind: vehicle
id: veh-a
zones:
  front-left: {}
devices:
  node-a: { role: ecu, zone: front-left, backend: container }
functions:
  fn-a: { deployed_on: node-a }
links:
  - id: link-a
    endpoints: [{ node: node-a, port: eth0, ip: 10.101.0.11/24 }]
    slice: adas
";
const BASELINE: &str = "\
kind: baseline
id: bl-a
parts:
  - ref: local/vxprobe:host-process
    target_ids: fn-a
";

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("vxedge-validate-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn validate(path: &std::path::Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_vxedge"))
        .args(["manifest", "validate", "--manifest"])
        .arg(path)
        .output()
        .expect("run vxedge");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn a_bare_vehicle_directory_validates() {
    let d = scratch("dir");
    std::fs::write(d.join("vehicle.yaml"), VEHICLE).unwrap();
    std::fs::write(d.join("parts.yaml"), BASELINE).unwrap();
    let (ok, text) = validate(&d);
    assert!(ok, "{text}");
    assert!(text.contains("bare vehicle"), "{text}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_bare_vehicle_single_file_validates() {
    let d = scratch("file");
    let f = d.join("ws.yaml");
    std::fs::write(&f, format!("{VEHICLE}---\n{BASELINE}")).unwrap();
    let (ok, text) = validate(&f);
    assert!(ok, "{text}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_bare_vehicle_without_a_baseline_is_still_rejected() {
    let d = scratch("nobaseline");
    std::fs::write(d.join("vehicle.yaml"), VEHICLE).unwrap();
    let (ok, text) = validate(&d);
    assert!(!ok, "{text}");
    assert!(text.contains("baseline"), "{text}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_workspace_with_neither_experiment_nor_vehicle_is_still_rejected() {
    let d = scratch("empty");
    std::fs::write(d.join("parts.yaml"), BASELINE).unwrap();
    let (ok, text) = validate(&d);
    assert!(!ok, "{text}");
    assert!(text.contains("kind: experiment"), "{text}");
    let _ = std::fs::remove_dir_all(&d);
}
