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

/// The committed package every host must pack to the same digest (X1.4's two-host proof). Each
/// CI run is a fresh checkout on another machine, with its own umask, mtimes and file order, so
/// this test passing there and on a developer's host is the proof. The fixture is packed from its
/// committed lock, never re-locked: `locked_with` names the vxedge that wrote it, and the lock
/// check ignores that field, so a later vxedge still accepts the lock and packs the same bytes.
/// If this digest moves, every published package's digest moved with it.
const TWO_HOST_DIGEST: &str =
    "sha256:f95ef0a7c0dd7bb9a0b8dd25ae64048072290d3474d7124e14daff93f86b8c3b";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

#[test]
fn the_committed_package_packs_to_the_same_digest_on_every_host() {
    let committed = fixture("two-host");
    let (ok, out, err) = vxedge(&committed, &["pkg", "lock", "--check"]);
    assert!(ok, "the committed lock matches the checkout: {out}{err}");
    let (ok, out, err) = vxedge(&committed, &["pkg", "inspect"]);
    assert!(ok, "{out}{err}");
    assert_eq!(
        digest_line(&out),
        TWO_HOST_DIGEST,
        "from the checkout as is"
    );

    // A copy that differs in everything the packed form must not see: modes, mtimes, creation
    // order, version control, editor backups and results.
    let base = scratch("two-host");
    let copy = base.join("two-host");
    copy_tree(&committed, &copy);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let set = |p: &str, mode: u32| {
            std::fs::set_permissions(copy.join(p), std::fs::Permissions::from_mode(mode)).unwrap()
        };
        set("scripts/run.sh", 0o600);
        set("main.yaml", 0o755);
    }
    std::fs::create_dir_all(copy.join(".git")).unwrap();
    std::fs::write(copy.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(copy.join("main.yaml~"), "editor backup\n").unwrap();
    std::fs::write(copy.join("results/rep1.zip"), "not an input").unwrap();

    let (ok, out, err) = vxedge(&copy, &["pkg", "pack"]);
    assert!(ok, "pkg pack: {out}{err}");
    assert_eq!(digest_line(&out), TWO_HOST_DIGEST, "from a perturbed copy");
    let bytes = std::fs::read(base.join("two-host-0.1.0.vxp")).unwrap();
    assert_eq!(
        vxedge_core::pkg::sha256(&bytes),
        TWO_HOST_DIGEST,
        "the digest is the SHA-256 of the file, as `sha256sum` reads it"
    );
    std::fs::remove_dir_all(&base).ok();
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs")
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// A catalogue repository as `vxperiments` is: a reference vehicle under `vehicles/` and a package
/// under `experiments/`, in one checkout. `pkg vendor` names the source from git, and `--check`
/// finds it again in the same checkout.
#[test]
fn a_package_vendors_a_vehicle_and_flags_drift_from_it() {
    let repo = scratch("vendor");
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:acme/catalogue.git",
        ],
    );
    let (ok, out, err) = vxedge(&repo, &["pkg", "new", "vehicle-src", "--path", "."]);
    assert!(ok, "{out}{err}");
    let source = repo.join("vehicles/sedan");
    std::fs::create_dir_all(&source).unwrap();
    for f in ["vehicle.yaml", "parts.yaml"] {
        std::fs::copy(repo.join("vehicle-src").join(f), source.join(f)).unwrap();
    }
    std::fs::write(source.join("build.sh"), "#!/bin/sh\n").unwrap();
    std::fs::remove_dir_all(repo.join("vehicle-src")).unwrap();
    std::fs::create_dir_all(repo.join("experiments")).unwrap();
    let (ok, _, _) = vxedge(&repo.join("experiments"), &["pkg", "new", "demo"]);
    assert!(ok);
    let pkg = repo.join("experiments/demo");

    let (ok, _, err) = vxedge(&pkg, &["pkg", "vendor", "../../vehicles/sedan"]);
    assert!(!ok && err.contains("--replace"), "{err}");
    let (ok, out, err) = vxedge(
        &pkg,
        &["pkg", "vendor", "../../vehicles/sedan", "--replace"],
    );
    assert!(ok, "{out}{err}");
    assert!(
        out.contains("vendored vehicle.yaml from acme/catalogue/vehicles/sedan"),
        "{out}"
    );
    assert!(out.contains("not vendored: build.sh (host recipe"), "{out}");
    let lock = std::fs::read_to_string(pkg.join("vxp.lock")).unwrap();
    assert!(
        lock.contains("source: acme/catalogue/vehicles/sedan"),
        "{lock}"
    );
    assert!(
        vxedge(&pkg, &["pkg", "lock", "--check"]).0,
        "the copies are the vehicle `demo` names"
    );

    let (ok, out, err) = vxedge(&pkg, &["pkg", "vendor", "--check"]);
    assert!(ok, "{out}{err}");
    assert!(
        !out.contains("not checked"),
        "the source is in this checkout: {out}"
    );

    let vehicle = std::fs::read_to_string(source.join("vehicle.yaml")).unwrap();
    std::fs::write(
        source.join("vehicle.yaml"),
        format!("{vehicle}# changed upstream\n"),
    )
    .unwrap();
    let (ok, out, err) = vxedge(&pkg, &["pkg", "vendor", "--check"]);
    assert!(!ok, "{out}{err}");
    assert!(
        format!("{out}{err}").contains("`vehicle.yaml` changed at its source"),
        "{out}{err}"
    );

    // Outside a checkout the source cannot be named, and the error says how to name it.
    let loose = scratch("vendor-loose");
    let (ok, _, err) = vxedge(&pkg, &["pkg", "vendor", loose.to_str().unwrap()]);
    assert!(!ok && err.contains("--as"), "{err}");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&loose).ok();
}
