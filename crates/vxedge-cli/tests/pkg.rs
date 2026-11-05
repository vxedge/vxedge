//! The `pkg` verbs end to end, as an author uses them: scaffold, validate, pack, change, re-lock,
//! unpack. All local: no daemon, no network.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("vxedge-pkg-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Run `vxedge --chdir <dir> <args>`: (success, stdout, stderr).
fn vxedge(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_vxedge"))
        .arg("--chdir")
        .arg(dir)
        .args(args)
        .output()
        .expect("run vxedge");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn digest_line(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("digest "))
        .unwrap_or_else(|| panic!("no digest line in:\n{stdout}"))
        .to_string()
}

#[test]
fn an_author_scaffolds_validates_packs_relocks_and_unpacks() {
    let base = scratch("flow");

    let (ok, out, err) = vxedge(&base, &["pkg", "new", "demo", "--namespace", "acme"]);
    assert!(ok, "pkg new: {out}{err}");
    let root = base.join("demo");
    let descriptor = std::fs::read_to_string(root.join("vxp.yaml")).unwrap();
    assert!(descriptor.contains("id: acme/demo"), "{descriptor}");
    assert!(
        root.join("vxp.lock").is_file(),
        "pkg new writes the first lock"
    );

    let (ok, out, err) = vxedge(&root, &["pkg", "validate"]);
    assert!(ok, "a fresh package validates: {out}{err}");

    let (ok, out, err) = vxedge(&root, &["pkg", "pack"]);
    assert!(ok, "pkg pack: {out}{err}");
    let packed = base.join("demo-0.1.0.vxp");
    assert!(
        packed.is_file(),
        "packed beside the source folder, not inside it"
    );
    let digest = digest_line(&out);
    assert!(digest.starts_with("sha256:"), "{digest}");

    // The same digest from the file and from the source, without packing again.
    let (ok, out, _) = vxedge(&base, &["pkg", "inspect", packed.to_str().unwrap()]);
    assert!(ok);
    assert_eq!(digest_line(&out), digest);
    assert!(out.contains("acme/demo"), "{out}");
    let (ok, out, _) = vxedge(&root, &["pkg", "inspect"]);
    assert!(ok);
    assert_eq!(digest_line(&out), digest);

    // A change the lock does not cover: validate names it, pack refuses, lock fixes it.
    std::fs::write(root.join("assets/trace.csv"), "t,v\n").unwrap();
    let (ok, out, err) = vxedge(&root, &["pkg", "validate"]);
    assert!(!ok);
    assert!(
        format!("{out}{err}").contains("`assets/trace.csv` was added"),
        "{out}{err}"
    );
    let (ok, _, err) = vxedge(&root, &["pkg", "pack", "--out", base.to_str().unwrap()]);
    assert!(!ok, "a stale lock is not packed");
    assert!(err.contains("pkg lock"), "{err}");
    let (ok, _, err) = vxedge(&root, &["pkg", "lock", "--check"]);
    assert!(!ok, "--check reports drift without writing: {err}");
    let (ok, out, err) = vxedge(&root, &["pkg", "lock"]);
    assert!(ok, "{out}{err}");
    assert!(vxedge(&root, &["pkg", "lock", "--check"]).0);
    std::fs::remove_file(&packed).unwrap();
    let (ok, out, _) = vxedge(&root, &["pkg", "pack"]);
    assert!(ok);
    let relocked = digest_line(&out);
    assert_ne!(relocked, digest);

    // Unpacked, it is the same package.
    let into = base.join("restored");
    let (ok, out, err) = vxedge(
        &base,
        &[
            "pkg",
            "unpack",
            packed.to_str().unwrap(),
            "--into",
            into.to_str().unwrap(),
        ],
    );
    assert!(ok, "pkg unpack: {out}{err}");
    let (ok, out, _) = vxedge(&into, &["pkg", "inspect"]);
    assert!(ok);
    assert_eq!(digest_line(&out), relocked);
    assert!(vxedge(&into, &["pkg", "validate"]).0);

    std::fs::remove_dir_all(&base).ok();
}

/// `experiment new` scaffolds the same `vxp/1` package `pkg new` does, from every template:
/// writing the lock builds the model, so a template that does not build fails here.
#[test]
fn experiment_new_scaffolds_a_locked_vxp1_package() {
    for template in ["minimal", "slicing"] {
        let base = scratch(&format!("expnew-{template}"));
        let (ok, out, err) = vxedge(
            &base,
            &["experiment", "new", "demo", "--template", template],
        );
        assert!(ok, "{template}: {out}{err}");
        let root = base.join("demo");
        assert!(!root.join("demo.vxp").exists(), "no legacy descriptor");
        let descriptor = std::fs::read_to_string(root.join("vxp.yaml")).unwrap();
        assert!(descriptor.starts_with("schema: vxp/1\n"), "{descriptor}");
        assert!(descriptor.contains("id: local/demo"), "{descriptor}");
        assert!(vxedge(&root, &["pkg", "validate"]).0, "{template}");
        assert!(vxedge(&root, &["experiment", "validate"]).0, "{template}");
        std::fs::remove_dir_all(&base).ok();
    }
}

/// A legacy descriptor still runs, but the package verbs say how to migrate it.
#[test]
fn a_legacy_workspace_is_told_to_migrate() {
    let base = scratch("legacy");
    let (ok, _, _) = vxedge(&base, &["experiment", "new", "demo"]);
    assert!(ok);
    let root = base.join("demo");
    std::fs::remove_file(root.join("vxp.yaml")).unwrap();
    std::fs::remove_file(root.join("vxp.lock")).unwrap();
    std::fs::write(root.join("demo.vxp"), "id: demo\nversion: 0.1.0\n").unwrap();
    let (ok, _, err) = vxedge(&root, &["pkg", "pack"]);
    assert!(!ok);
    assert!(err.contains("legacy descriptor"), "{err}");
    std::fs::remove_dir_all(&base).ok();
}
