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

/// A legacy `<name>.vxp` descriptor is no longer read: the package and experiment verbs refuse
/// it and say how to migrate it.
#[test]
fn a_legacy_workspace_is_told_to_migrate() {
    let base = scratch("legacy");
    let (ok, _, _) = vxedge(&base, &["experiment", "new", "demo"]);
    assert!(ok);
    let root = base.join("demo");
    std::fs::remove_file(root.join("vxp.yaml")).unwrap();
    std::fs::remove_file(root.join("vxp.lock")).unwrap();
    std::fs::write(root.join("demo.vxp"), "id: demo\nversion: 0.1.0\n").unwrap();
    for verb in [&["pkg", "pack"][..], &["experiment", "validate"]] {
        let (ok, out, err) = vxedge(&root, verb);
        assert!(!ok, "{verb:?}: {out}");
        assert!(
            err.contains("legacy `<name>.vxp` descriptor, which this vxedge no longer reads")
                && err.contains("schema: vxp/1"),
            "{verb:?}: {err}"
        );
    }
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
    // A local part the documents reference: committed, so it travels; a build output does not.
    std::fs::create_dir_all(source.join("parts/gw-tcu")).unwrap();
    std::fs::write(source.join("parts/gw-tcu/Containerfile"), "FROM scratch\n").unwrap();
    std::fs::write(source.join("parts/gw-tcu/build.log"), "untracked\n").unwrap();
    git(&repo, &["add", "vehicles/sedan/parts/gw-tcu/Containerfile"]);
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
    assert!(
        out.contains("vendored parts/gw-tcu/Containerfile from acme/catalogue/vehicles/sedan"),
        "{out}"
    );
    assert!(
        !pkg.join("parts/gw-tcu/build.log").exists(),
        "untracked files stay behind"
    );
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

/// `experiment validate` runs the typed-intent checks `Plan` runs, and names each field.
#[test]
fn experiment_validate_reports_typed_intent_problems() {
    let base = scratch("intent");
    let (ok, _, _) = vxedge(&base, &["experiment", "new", "demo"]);
    assert!(ok);
    let root = base.join("demo");
    let main = std::fs::read_to_string(root.join("main.yaml")).unwrap();
    std::fs::write(
        root.join("main.yaml"),
        format!(
            "{main}\nparameters:\n  rate: {{ type: integer, unit: 1/s, default: 0, binds: \"traffic_profiles[nope].pps\" }}\n"
        ),
    )
    .unwrap();
    let (ok, out, err) = vxedge(&root, &["experiment", "validate"]);
    assert!(!ok, "{out}");
    assert!(err.contains("parameters.rate.binds"), "{err}");
    assert!(
        err.contains("`traffic_profiles[nope]`: no entry `nope`"),
        "{err}"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// A stand-in `oras` that keeps pushed layers in a folder by digest and serves them back from
/// `blob fetch`, recording each call's arguments.
fn fake_oras(dir: &Path) {
    std::fs::create_dir_all(dir.join("blobs")).unwrap();
    let script = format!(
        r#"#!/bin/sh
echo "$@" >> "{d}/calls"
case "$1" in
  push)
    layer=$(echo "$7" | sed 's/:application.*//')
    sum=$(sha256sum "$layer" | cut -d' ' -f1)
    cp "$layer" "{d}/blobs/$sum"
    cp config.json "{d}/config.json"
    ;;
  blob)
    out="$4"; hex=$(echo "$5" | sed 's/.*@sha256://')
    [ -f "{d}/blobs/$hex" ] || {{ echo "not found" >&2; exit 1; }}
    cp "{d}/blobs/$hex" "$out"
    ;;
esac
"#,
        d = dir.display()
    );
    let bin = dir.join("oras");
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

fn vxedge_with(dir: &Path, fake: &Path, cache: &Path, args: &[&str]) -> (bool, String, String) {
    let path = format!(
        "{}:{}",
        fake.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_vxedge"))
        .arg("--chdir")
        .arg(dir)
        .args(args)
        .env("PATH", path)
        .env("XDG_CACHE_HOME", cache)
        .output()
        .expect("run vxedge");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A package pushed and pulled back is the same bytes: the layer is the packed file, its digest
/// is the package digest, and the pull checks both before the cache keeps anything.
#[test]
fn a_package_is_pushed_and_pulled_back_by_its_digest() {
    let base = scratch("push");
    let (fake, cache) = (base.join("fake"), base.join("cache"));
    fake_oras(&fake);
    let (ok, _, _) = vxedge(&base, &["pkg", "new", "demo", "--namespace", "acme"]);
    assert!(ok);
    let root = base.join("demo");
    let (_, out, _) = vxedge(&root, &["pkg", "inspect"]);
    let digest = digest_line(&out);

    let (ok, out, err) = vxedge_with(&root, &fake, &cache, &["pkg", "push", "ghcr.io/acme/demo"]);
    assert!(ok, "{out}{err}");
    assert!(out.contains("pushed ghcr.io/acme/demo:0.1.0"), "{out}");
    assert_eq!(
        digest_line(&out),
        digest,
        "the layer digest is the package digest"
    );
    let calls = std::fs::read_to_string(fake.join("calls")).unwrap();
    assert!(calls.contains("--artifact-type application/vnd.vxedge.vxp.v1 --config config.json:application/vnd.vxedge.vxp.config.v1+json demo-0.1.0.vxp:application/vnd.vxedge.vxp.v1.tar"), "{calls}");
    let config = std::fs::read_to_string(fake.join("config.json")).unwrap();
    assert!(
        config.contains(r#""id":"acme/demo""#) && config.contains(r#""version":"0.1.0""#),
        "{config}"
    );

    let reference = format!("ghcr.io/acme/demo@{digest}");
    let into = base.join("restored");
    let (ok, out, err) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "pull", &reference, "--into", into.to_str().unwrap()],
    );
    assert!(ok, "{out}{err}");
    let cached = cache
        .join("vxedge/parts/packages")
        .join(format!("{}.vxp", digest.trim_start_matches("sha256:")));
    assert_eq!(
        vxedge_core::pkg::sha256(&std::fs::read(&cached).unwrap()),
        digest
    );
    assert_eq!(
        std::fs::read(into.join("vxp.yaml")).unwrap(),
        std::fs::read(root.join("vxp.yaml")).unwrap()
    );

    // A tag alone is refused, and so are bytes that do not hash to the digest asked for.
    let (ok, _, err) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "pull", "ghcr.io/acme/demo:0.1.0"],
    );
    assert!(
        !ok && err.contains("does not name a package digest"),
        "{err}"
    );
    let other = format!("sha256:{}", "0".repeat(64));
    std::fs::write(fake.join("blobs").join("0".repeat(64)), b"tampered").unwrap();
    let (ok, _, err) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "pull", &format!("ghcr.io/acme/demo@{other}")],
    );
    assert!(!ok && err.contains("hash to"), "{err}");
    assert!(
        !cache
            .join("vxedge/parts/packages")
            .join(format!("{}.vxp", "0".repeat(64)))
            .exists()
    );
    std::fs::remove_dir_all(&base).ok();
}

/// Stand-ins for `cosign` and `gh`: a bundle holds the digest of the bytes it signs, and `gh`
/// vouches for a file only if its digest is in `attested`.
fn fake_signers(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let cosign = r#"#!/bin/sh
cmd=$1; shift; key=""; bundle=""; file=""
while [ $# -gt 0 ]; do case "$1" in
  --key) key=$2; shift 2;; --bundle) bundle=$2; shift 2;; --*) shift;; *) file=$1; shift;; esac; done
sum=$(sha256sum "$file" | cut -d' ' -f1)
case "$cmd" in
  sign-blob) echo "$sum $(cat "$key")" > "$bundle";;
  verify-blob) [ "$(cat "$bundle" 2>/dev/null)" = "$sum $(cat "$key")" ] || { echo "invalid signature" >&2; exit 1; };;
esac
"#;
    let gh = format!(
        r#"#!/bin/sh
sum=$(sha256sum "$3" | cut -d' ' -f1)
grep -q "$sum" "{}/attested" 2>/dev/null || {{ echo "no attestation found" >&2; exit 1; }}
"#,
        dir.display()
    );
    for (name, body) in [("cosign", cosign.to_string()), ("gh", gh)] {
        let bin = dir.join(name);
        std::fs::write(&bin, body).unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
    }
}

/// A signed package verifies, and the verification is recorded by its package digest; a package
/// changed after signing, or one nobody attested, does not verify and records nothing.
#[test]
fn a_package_is_signed_verified_and_recorded_by_its_digest() {
    let base = scratch("sign");
    let (fake, cache) = (base.join("fake"), base.join("cache"));
    fake_signers(&fake);
    let (ok, _, _) = vxedge(&base, &["pkg", "new", "demo", "--namespace", "acme"]);
    assert!(ok);
    let (ok, out, _) = vxedge(&base.join("demo"), &["pkg", "pack"]);
    assert!(ok);
    let digest = digest_line(&out);
    let file = base.join("demo-0.1.0.vxp");
    let file_s = file.to_str().unwrap();
    std::fs::write(base.join("lab.key"), "private-1").unwrap();
    std::fs::write(base.join("lab.pub"), "private-1").unwrap();

    let (ok, out, err) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "sign", file_s, "--key", "lab.key"],
    );
    assert!(ok, "{out}{err}");
    assert!(base.join("demo-0.1.0.vxp.sigstore.json").is_file());
    let (ok, out, err) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "verify", file_s, "--key", "lab.pub"],
    );
    assert!(ok, "{out}{err}");
    let fingerprint = vxedge_core::pkg::sha256(b"private-1");
    assert!(out.contains(&format!("signer key:{fingerprint}")), "{out}");
    let record = cache.join("vxedge/parts/packages").join(format!(
        "{}.verified.json",
        digest.trim_start_matches("sha256:")
    ));
    let text = std::fs::read_to_string(&record).unwrap();
    assert!(
        text.contains(&digest) && text.contains("sigstore-key"),
        "{text}"
    );

    // Another key does not verify it.
    std::fs::write(base.join("other.pub"), "private-2").unwrap();
    let (ok, _, err) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "verify", file_s, "--key", "other.pub"],
    );
    assert!(!ok && err.contains("cosign"), "{err}");

    // GitHub: only an attested digest verifies.
    let (ok, _, err) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "verify", file_s, "--repo", "acme/experiments"],
    );
    assert!(!ok && err.contains("no attestation"), "{err}");
    std::fs::write(fake.join("attested"), digest.trim_start_matches("sha256:")).unwrap();
    let (ok, out, err) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "verify", file_s, "--repo", "acme/experiments"],
    );
    assert!(ok, "{out}{err}");
    assert!(out.contains("signer github:acme/experiments"), "{out}");

    // A file that is not a canonical packed package is refused before any tool runs.
    std::fs::write(base.join("junk.vxp"), b"not a package").unwrap();
    let (ok, _, err) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "sign", "junk.vxp", "--key", "lab.key"],
    );
    assert!(!ok && err.contains("packed"), "{err}");
    std::fs::remove_dir_all(&base).ok();
}

/// Stand-ins for the offline tools: `oras copy` records into a layout folder, `skopeo` records
/// which image it stored, and `podman image exists` answers from that record.
fn fake_offline_tools(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let d = dir.display();
    // The pause image's real index, as `oras manifest fetch` returns it (its sha256 is the pinned
    // digest, which `pack --offline` checks).
    let index = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pause-index.json");
    let index = index.display();
    let scripts = [
        (
            "oras",
            format!(
                r#"#!/bin/sh
echo "oras $@" >> "{d}/calls"
[ "$1" = manifest ] && {{ cat "{index}"; exit 0; }}
[ "$1" = copy ] && {{ layout=$(echo "$4" | cut -d: -f1); mkdir -p "$layout"; echo "$3" >> "$layout/copied"; }}
[ "$1" = pull ] && {{ mkdir -p "$5"; echo part > "$5/meta.json"; }}
exit 0
"#
            ),
        ),
        (
            "skopeo",
            format!(
                "#!/bin/sh\necho \"skopeo $@\" >> \"{d}/calls\"\necho \"$4\" | sed 's/^containers-storage://' >> \"{d}/images\"\n"
            ),
        ),
        (
            "podman",
            format!(
                r#"#!/bin/sh
case "$1" in
  image) grep -qx "$3" "{d}/images" 2>/dev/null;;
  unshare) shift; exec "$@";;
esac
"#
            ),
        ),
    ];
    for (name, body) in scripts {
        let bin = dir.join(name);
        std::fs::write(&bin, body).unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
    }
}

/// `pack --offline` carries what the lock pins beside the package, and vxedge's own infra image,
/// and refuses a tag-only ref;
/// `pull --layout` loads parts and images, and a second load finds them already there.
#[test]
fn an_offline_layout_carries_the_lock_and_loads_without_a_registry() {
    let base = scratch("offline");
    let (fake, cache) = (base.join("fake"), base.join("cache"));
    fake_offline_tools(&fake);
    let (ok, _, _) = vxedge(&base, &["pkg", "new", "demo"]);
    assert!(ok);
    let root = base.join("demo");
    let parts = std::fs::read_to_string(root.join("parts.yaml")).unwrap();
    let digest = format!("sha256:{}", "1".repeat(64));
    let pinned = parts.replace(
        "  - ref: local/vxprobe:host-process\n",
        &format!("  - ref: ghcr.io/vxedge/parts/vxprobe\n    digest: \"{digest}\"\n"),
    );
    assert_ne!(pinned, parts, "the scaffold names the local probe");
    std::fs::write(root.join("parts.yaml"), &pinned).unwrap();
    assert!(vxedge(&root, &["pkg", "lock"]).0);

    let (ok, out, err) = vxedge_with(&root, &fake, &cache, &["pkg", "pack", "--offline"]);
    assert!(ok, "{out}{err}");
    let layout = base.join("demo-0.1.0.parts");
    // The pinned probe, and the infra image vxedge runs every container node with.
    assert!(out.contains("(2 parts and images)"), "{out}");
    let index = std::fs::read_to_string(layout.join("vxedge-parts.json")).unwrap();
    assert!(
        index.contains(r#""kind": "image""#) && index.contains(&digest),
        "{index}"
    );
    assert!(
        index.contains(r#""ref": "registry.k8s.io/pause""#),
        "{index}"
    );
    // Its Linux platforms travel and its Windows ones do not, and the index blob is in the layout
    // under the pinned digest.
    let calls = std::fs::read_to_string(fake.join("calls")).unwrap();
    let pause_copies: Vec<&str> = calls
        .lines()
        .filter(|l| l.starts_with("oras copy") && l.contains("registry.k8s.io/pause@"))
        .collect();
    assert_eq!(pause_copies.len(), 5, "{calls}");
    assert!(!calls.contains("abce690161824d5950c0064ed836f397c6316a833d1abca92b79017d19949482"));
    let (_, pause_digest) = vxedge_core::containers::spec::PAUSE_IMAGE
        .split_once('@')
        .unwrap();
    let blob = layout
        .join("blobs/sha256")
        .join(pause_digest.trim_start_matches("sha256:"));
    assert_eq!(
        vxedge_core::pkg::sha256(&std::fs::read(&blob).unwrap()),
        pause_digest
    );
    let layout_index = std::fs::read_to_string(layout.join("index.json")).unwrap();
    assert!(
        layout_index.contains(&pause_digest.replace(':', "-")),
        "{layout_index}"
    );
    let calls = std::fs::read_to_string(fake.join("calls")).unwrap();
    assert!(
        calls.contains(&format!(
            "oras copy --to-oci-layout ghcr.io/vxedge/parts/vxprobe@{digest} {}:sha256-{}",
            layout.display(),
            "1".repeat(64)
        )),
        "{calls}"
    );

    let (ok, out, err) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "pull", "--layout", layout.to_str().unwrap()],
    );
    assert!(ok, "{out}{err}");
    assert!(
        out.contains(&format!(
            "loaded: image ghcr.io/vxedge/parts/vxprobe@{digest}"
        )),
        "{out}"
    );
    let calls = std::fs::read_to_string(fake.join("calls")).unwrap();
    assert!(
        calls.contains("skopeo copy --preserve-digests oci:"),
        "{calls}"
    );
    let (ok, out, _) = vxedge_with(
        &base,
        &fake,
        &cache,
        &["pkg", "pull", "--layout", layout.to_str().unwrap()],
    );
    assert!(ok && out.contains("already here: image"), "{out}");

    // A reference pinned by tag only cannot travel.
    std::fs::write(
        root.join("parts.yaml"),
        parts.replace(
            "  - ref: local/vxprobe:host-process\n",
            "  - ref: ghcr.io/vxedge/parts/vxprobe:0.1.1\n",
        ),
    )
    .unwrap();
    assert!(vxedge(&root, &["pkg", "lock"]).0);
    std::fs::create_dir_all(base.join("again")).unwrap();
    let (ok, _, err) = vxedge_with(
        &root,
        &fake,
        &cache,
        &[
            "pkg",
            "pack",
            "--offline",
            "--out",
            base.join("again").to_str().unwrap(),
        ],
    );
    assert!(!ok && err.contains("pinned by tag only"), "{err}");
    assert!(
        !base.join("again/demo-0.1.0.vxp").exists(),
        "nothing is written when the layout cannot be made"
    );
    std::fs::remove_dir_all(&base).ok();
}
