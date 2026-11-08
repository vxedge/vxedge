//! **Trust** (`ARCHITECTURE.md` §6.0.8): signing a packed package and recording who verified it.
//!
//! A signature covers the package digest, never a folder: a folder cannot hold its own signature,
//! because the signature would be an input to the digest it signs. So a successful verification
//! is written beside the package, as a record in the part cache keyed by the package digest. `up`
//! computes the package digest of every `vxp/1` run and can look the record up by it.
//!
//! Two methods, shelled out like `oras` (decision X-11):
//! - a GitHub artifact attestation, checked with `gh attestation verify` against the repository
//!   (and optionally the workflow) that made it, for published packages;
//! - a key-pair signature in Sigstore bundle format, made and checked with `cosign` offline, for
//!   air-gapped labs. No transparency log is involved: the lab's public key is the trust root.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::pkg::{self, PkgError};

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error(transparent)]
    Package(#[from] PkgError),
    #[error("cannot run `{tool}`: {source} (is it installed and on PATH?)")]
    Spawn {
        tool: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("`{tool}` refused `{}`: {detail}", .file.display())]
    Refused {
        tool: &'static str,
        file: PathBuf,
        detail: String,
    },
    #[error("{op} `{}`: {source}", .path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

fn io<'a>(op: &'static str, path: &'a Path) -> impl FnOnce(std::io::Error) -> TrustError + 'a {
    move |source| TrustError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

/// How a package was verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    GithubAttestation,
    SigstoreKey,
}

/// Who signed a package, as a trust policy names an allowed signer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Signer {
    /// The repository whose workflow attested the package, and that workflow when it was checked.
    Github {
        repo: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
    },
    /// A public key, named by the SHA-256 of its file, so a policy and a record agree on it
    /// without either holding the key.
    Key { fingerprint: String },
}

impl std::fmt::Display for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Github {
                repo,
                workflow: Some(w),
            } => write!(f, "github:{repo} ({w})"),
            Self::Github { repo, .. } => write!(f, "github:{repo}"),
            Self::Key { fingerprint } => write!(f, "key:{fingerprint}"),
        }
    }
}

/// A successful verification of one package digest, kept in the part cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationRecord {
    pub package_digest: String,
    pub method: Method,
    pub signer: Signer,
    pub verified_at_unix_ms: i64,
}

/// The signer a public key file stands for.
pub fn key_signer(public_key: &Path) -> Result<Signer, TrustError> {
    let bytes = std::fs::read(public_key).map_err(io("reading", public_key))?;
    Ok(Signer::Key {
        fingerprint: pkg::sha256(&bytes),
    })
}

/// Where the record of a package digest's verification lives: beside the pulled package.
pub fn record_path(cache_dir: &Path, digest: &str) -> Result<PathBuf, TrustError> {
    let packed = crate::distribution::package_cache_path(cache_dir, digest).map_err(|_| {
        TrustError::Package(PkgError::Lock(format!(
            "`{digest}` is not a package digest"
        )))
    })?;
    Ok(packed.with_extension("verified.json"))
}

/// The verification record for `digest`, if the package was verified on this host.
pub fn read_record(cache_dir: &Path, digest: &str) -> Option<VerificationRecord> {
    let path = record_path(cache_dir, digest).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text)
        .ok()
        .filter(|r: &VerificationRecord| r.package_digest == digest)
}

fn write_record(cache_dir: &Path, record: &VerificationRecord) -> Result<PathBuf, TrustError> {
    let path = record_path(cache_dir, &record.package_digest)?;
    let dir = path.parent().expect("a record path has a parent");
    std::fs::create_dir_all(dir).map_err(io("creating", dir))?;
    let json = serde_json::to_vec_pretty(record).expect("a record always serializes");
    std::fs::write(&path, json).map_err(io("writing", &path))?;
    Ok(path)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The bundle written beside a packed file: `<file>.sigstore.json`.
pub fn bundle_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".sigstore.json");
    PathBuf::from(name)
}

fn run(tool: &'static str, file: &Path, cmd: &mut Command) -> Result<(), TrustError> {
    let out = cmd
        .output()
        .map_err(|source| TrustError::Spawn { tool, source })?;
    if out.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(TrustError::Refused {
        tool,
        file: file.to_path_buf(),
        detail: if detail.is_empty() {
            format!("exit status {}", out.status)
        } else {
            detail
        },
    })
}

/// The packed file's digest, after checking it is a canonical packed package: a signature over a
/// file `pkg unpack` would refuse is worth nothing.
fn digest_of(file: &Path) -> Result<String, TrustError> {
    let bytes = std::fs::read(file).map_err(io("reading", file))?;
    pkg::read_packed(&bytes)?;
    Ok(pkg::sha256(&bytes))
}

/// Sign the packed file with a cosign private key, writing `<file>.sigstore.json` beside it, with
/// no transparency log. Returns the bundle path.
pub fn sign_with_key(file: &Path, private_key: &Path) -> Result<PathBuf, TrustError> {
    digest_of(file)?;
    let bundle = bundle_path(file);
    run(
        "cosign",
        file,
        Command::new("cosign")
            .arg("sign-blob")
            .arg("--key")
            .arg(private_key)
            .arg("--bundle")
            .arg(&bundle)
            // cosign 3: offline signing needs both, or it asks for a signing config.
            .args(["--use-signing-config=false", "--tlog-upload=false", "--yes"])
            .arg(file),
    )?;
    Ok(bundle)
}

/// Verify the packed file's `<file>.sigstore.json` against a cosign public key, offline, and
/// record it in `cache_dir`.
pub fn verify_with_key(
    file: &Path,
    public_key: &Path,
    cache_dir: &Path,
) -> Result<VerificationRecord, TrustError> {
    let digest = digest_of(file)?;
    let bundle = bundle_path(file);
    run(
        "cosign",
        file,
        Command::new("cosign")
            .arg("verify-blob")
            .arg("--key")
            .arg(public_key)
            .arg("--bundle")
            .arg(&bundle)
            .arg("--insecure-ignore-tlog=true")
            .arg(file),
    )?;
    let record = VerificationRecord {
        package_digest: digest,
        method: Method::SigstoreKey,
        signer: key_signer(public_key)?,
        verified_at_unix_ms: now_ms(),
    };
    write_record(cache_dir, &record)?;
    Ok(record)
}

/// Verify the packed file's GitHub artifact attestation against the repository that made it, and
/// the workflow when given, and record it in `cache_dir`.
pub fn verify_github(
    file: &Path,
    repo: &str,
    workflow: Option<&str>,
    cache_dir: &Path,
) -> Result<VerificationRecord, TrustError> {
    let digest = digest_of(file)?;
    let mut cmd = Command::new("gh");
    cmd.args(["attestation", "verify"])
        .arg(file)
        .args(["--repo", repo]);
    if let Some(w) = workflow {
        cmd.args(["--signer-workflow", w]);
    }
    run("gh", file, &mut cmd)?;
    let record = VerificationRecord {
        package_digest: digest,
        method: Method::GithubAttestation,
        signer: Signer::Github {
            repo: repo.to_string(),
            workflow: workflow.map(str::to_string),
        },
        verified_at_unix_ms: now_ms(),
    };
    write_record(cache_dir, &record)?;
    Ok(record)
}

/// One signer a host trusts, as its trust policy names it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AllowedSigner {
    /// A repository whose workflow attests packages. With `workflow`, only that workflow counts.
    Github {
        repo: String,
        #[serde(default)]
        workflow: Option<String>,
    },
    /// A public key, by the fingerprint `pkg verify` prints (`sha256:…` of the key file), or by
    /// the key file itself, read when the policy loads.
    Key {
        #[serde(default)]
        fingerprint: Option<String>,
        #[serde(default)]
        public_key: Option<PathBuf>,
    },
}

/// A host's trust policy (`ARCHITECTURE.md` §6.0.8). Without one, any package runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustPolicy {
    /// Refuse a package whose digest has no verification record from an allowed signer.
    #[serde(default)]
    pub require_signed: bool,
    #[serde(default)]
    pub signers: Vec<AllowedSigner>,
}

impl TrustPolicy {
    /// Read a policy file, resolving each `public_key` to its fingerprint. A policy that requires
    /// signatures and allows no signer is refused: it would refuse everything.
    pub fn load(path: &Path) -> Result<Self, String> {
        let shown = path.display();
        let text = std::fs::read_to_string(path).map_err(|e| format!("`{shown}`: {e}"))?;
        let mut policy: Self =
            serde_yaml_ng::from_str(&text).map_err(|e| format!("`{shown}`: {e}"))?;
        for signer in &mut policy.signers {
            if let AllowedSigner::Key {
                fingerprint,
                public_key,
            } = signer
            {
                match (fingerprint.as_ref(), public_key.as_ref()) {
                    (None, Some(key)) => {
                        let key = path.parent().unwrap_or(Path::new(".")).join(key);
                        let Signer::Key { fingerprint: f } =
                            key_signer(&key).map_err(|e| format!("`{shown}`: {e}"))?
                        else {
                            unreachable!("key_signer returns a key signer")
                        };
                        *fingerprint = Some(f);
                    }
                    (Some(_), None) => {}
                    _ => {
                        return Err(format!(
                            "`{shown}`: a key signer names exactly one of `fingerprint` and \
                             `public_key`"
                        ));
                    }
                }
            }
        }
        if policy.require_signed && policy.signers.is_empty() {
            return Err(format!(
                "`{shown}` requires signed packages and allows no signer, so it would refuse \
                 every package; list the signers this host trusts"
            ));
        }
        Ok(policy)
    }

    /// Whether `signer` is one this policy allows.
    pub fn allows(&self, signer: &Signer) -> bool {
        self.signers.iter().any(|allowed| match (allowed, signer) {
            (
                AllowedSigner::Github { repo, workflow },
                Signer::Github {
                    repo: r,
                    workflow: w,
                },
            ) => repo == r && workflow.as_ref().is_none_or(|wf| w.as_ref() == Some(wf)),
            (AllowedSigner::Key { fingerprint, .. }, Signer::Key { fingerprint: f }) => {
                fingerprint.as_ref() == Some(f)
            }
            _ => false,
        })
    }

    /// The check `up` and `Plan` make: under `require_signed`, the package with `digest` must have
    /// a verification record here from an allowed signer. `None` is a run with no package digest
    /// (a legacy descriptor or an inline manifest), which cannot be verified.
    pub fn check(&self, cache_dir: &Path, digest: Option<&str>) -> Result<(), String> {
        if !self.require_signed {
            return Ok(());
        }
        let Some(digest) = digest else {
            return Err(
                "this host requires signed packages, and only a `vxp/1` package has a \
                        package digest to verify; a legacy descriptor or an inline manifest \
                        cannot run here"
                    .to_string(),
            );
        };
        match read_record(cache_dir, digest) {
            None => Err(format!(
                "this host requires signed packages, and package `{digest}` has not been \
                 verified here. Run `vxedge pkg verify` on its packed file (with `--cache-dir` \
                 set to the daemon's) and try again. Nothing was provisioned."
            )),
            Some(record) if !self.allows(&record.signer) => Err(format!(
                "package `{digest}` was verified as signed by {}, which this host's trust policy \
                 does not allow. Nothing was provisioned.",
                record.signer
            )),
            Some(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:1888e766b259786d5d065a4a7c85f3d3ddc6bb589a30cb257156b85e9155a5c6";

    #[test]
    fn a_record_reads_back_by_its_digest_and_only_by_it() {
        let cache = std::env::temp_dir().join(format!("vxtrust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let record = VerificationRecord {
            package_digest: DIGEST.into(),
            method: Method::SigstoreKey,
            signer: Signer::Key {
                fingerprint: "sha256:ab".into(),
            },
            verified_at_unix_ms: 1,
        };
        let path = write_record(&cache, &record).unwrap();
        assert!(path.ends_with(format!(
            "packages/{}.verified.json",
            DIGEST.trim_start_matches("sha256:")
        )));
        assert_eq!(read_record(&cache, DIGEST), Some(record.clone()));
        // A record copied under another digest's name does not vouch for that digest.
        let other = format!("sha256:{}", "0".repeat(64));
        std::fs::copy(&path, record_path(&cache, &other).unwrap()).unwrap();
        assert_eq!(read_record(&cache, &other), None);
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains(r#""method": "sigstore-key""#), "{json}");
        assert!(json.contains(r#""kind": "key""#), "{json}");
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn a_signer_reads_as_a_policy_names_it() {
        let gh = Signer::Github {
            repo: "vxedge/vxperiments".into(),
            workflow: Some(".github/workflows/release.yml".into()),
        };
        assert_eq!(
            gh.to_string(),
            "github:vxedge/vxperiments (.github/workflows/release.yml)"
        );
        let parsed: Signer =
            serde_json::from_str(r#"{"kind":"github","repo":"vxedge/vxperiments"}"#).unwrap();
        assert_eq!(
            parsed,
            Signer::Github {
                repo: "vxedge/vxperiments".into(),
                workflow: None
            }
        );
        assert_eq!(
            bundle_path(Path::new("/x/demo-0.1.0.vxp")),
            PathBuf::from("/x/demo-0.1.0.vxp.sigstore.json")
        );
    }

    #[test]
    fn a_policy_allows_named_signers_and_requires_a_record() {
        let dir = std::env::temp_dir().join(format!("vxtrust-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lab.pub"), "public").unwrap();
        std::fs::write(
            dir.join("trust.yaml"),
            "require_signed: true\nsigners:\n  - { kind: github, repo: vxedge/vxperiments, \
             workflow: .github/workflows/release.yml }\n  - { kind: key, public_key: lab.pub }\n",
        )
        .unwrap();
        let policy = TrustPolicy::load(&dir.join("trust.yaml")).unwrap();
        let lab = key_signer(&dir.join("lab.pub")).unwrap();
        assert!(policy.allows(&lab));
        assert!(policy.allows(&Signer::Github {
            repo: "vxedge/vxperiments".into(),
            workflow: Some(".github/workflows/release.yml".into())
        }));
        assert!(
            !policy.allows(&Signer::Github {
                repo: "vxedge/vxperiments".into(),
                workflow: None
            }),
            "the policy names a workflow"
        );
        assert!(!policy.allows(&Signer::Key {
            fingerprint: "sha256:other".into()
        }));

        let cache = dir.join("cache");
        let unverified = policy.check(&cache, Some(DIGEST)).unwrap_err();
        assert!(unverified.contains("has not been verified"), "{unverified}");
        assert!(policy.check(&cache, None).unwrap_err().contains("legacy"));
        let record = |signer| VerificationRecord {
            package_digest: DIGEST.into(),
            method: Method::SigstoreKey,
            signer,
            verified_at_unix_ms: 1,
        };
        write_record(
            &cache,
            &record(Signer::Key {
                fingerprint: "sha256:other".into(),
            }),
        )
        .unwrap();
        assert!(
            policy
                .check(&cache, Some(DIGEST))
                .unwrap_err()
                .contains("does not allow")
        );
        write_record(&cache, &record(lab)).unwrap();
        assert_eq!(policy.check(&cache, Some(DIGEST)), Ok(()));
        assert_eq!(
            TrustPolicy::default().check(&cache, None),
            Ok(()),
            "no policy, no check"
        );

        std::fs::write(dir.join("empty.yaml"), "require_signed: true\n").unwrap();
        assert!(
            TrustPolicy::load(&dir.join("empty.yaml"))
                .unwrap_err()
                .contains("allows no signer")
        );
        std::fs::write(
            dir.join("both.yaml"),
            "signers:\n  - { kind: key, fingerprint: x, public_key: lab.pub }\n",
        )
        .unwrap();
        assert!(TrustPolicy::load(&dir.join("both.yaml")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
