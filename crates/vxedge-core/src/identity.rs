//! **The workspace identity** (`ARCHITECTURE.md` §13): one SHA-256 over every file an experiment
//! is made of, so an audit record, a run and a results archive can say which workspace they came
//! from and agree on the answer.
//!
//! Two algorithms exist and every recorded identity names the one that computed it:
//!
//! - `vxws/1`: every file under the root, recursively, except `results/`. Kept so identities
//!   already recorded in run records and published archives stay verifiable.
//! - `vxws/2` (current): as `vxws/1`, and also skips version-control metadata (`.git`, file or
//!   directory, at any depth), editor droppings ([`DEFAULT_EXCLUDES`]) and whatever the
//!   workspace's root `.vxpignore` names, in `.gitignore` syntax. A workspace that is its own git
//!   repository, or that an editor has touched, keeps its identity. The preimage starts with
//!   `vxws/2\0`, so a `vxws/2` value can never equal a `vxws/1` value by accident: identities
//!   computed under different algorithms are not comparable, and now cannot look it.
//!
//! In both, each file contributes its relative path (`/`-separated), NUL, its bytes, NUL, in
//! sorted path order. `results/` and `.git` are excluded before `.vxpignore` is consulted, so a
//! `!results/` line cannot make outputs into inputs. `.vxpignore` itself is hashed: it decides
//! what the other inputs are.
//!
//! The matcher is built from the workspace alone. It reads no global gitignore, no `.ignore`
//! file and no environment, because an identity that depended on the machine computing it
//! would not identify anything.
//!
//! `vxtelemetry` implements the same rule without depending on this crate, and both sides pin
//! the same test vectors: two implementations of one rule stay honest by agreeing on answers.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sha2::{Digest, Sha256};

/// The file at the workspace root that lists further exclusions, in `.gitignore` syntax.
pub const IGNORE_FILE: &str = ".vxpignore";

/// What `vxws/2` always excludes besides `results/` and `.git`: editor swap, backup and lock
/// files, and Finder metadata. Byte-identical in `vxtelemetry`.
pub const DEFAULT_EXCLUDES: &[&str] = &["*.swp", "*.swo", "*~", ".#*", "#*#", ".DS_Store"];

/// Which rule computed a workspace identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdentityAlgorithm {
    Vxws1,
    Vxws2,
}

impl IdentityAlgorithm {
    /// The algorithm every new identity is computed with.
    pub const CURRENT: Self = Self::Vxws2;

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vxws1 => "vxws/1",
            Self::Vxws2 => "vxws/2",
        }
    }
}

impl fmt::Display for IdentityAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IdentityAlgorithm {
    type Err = IdentityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "vxws/1" => Ok(Self::Vxws1),
            "vxws/2" => Ok(Self::Vxws2),
            other => Err(IdentityError::UnknownAlgorithm(other.to_string())),
        }
    }
}

/// A workspace identity and the algorithm that computed it. Two identities are comparable only
/// when their algorithms are equal; derived equality compares both fields, so they never match
/// across algorithms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub algorithm: IdentityAlgorithm,
    /// Lower-case hex SHA-256.
    pub value: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("reading `{}`: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("`{}`: {message}", .path.display())]
    IgnoreFile { path: PathBuf, message: String },
    #[error("unknown workspace identity algorithm `{0}`; this build knows vxws/1 and vxws/2")]
    UnknownAlgorithm(String),
}

/// The identity of the workspace at `root` under `algorithm`.
pub fn compute(
    root: &Path,
    algorithm: IdentityAlgorithm,
) -> Result<WorkspaceIdentity, IdentityError> {
    let rules = match algorithm {
        IdentityAlgorithm::Vxws1 => None,
        IdentityAlgorithm::Vxws2 => Some(rules(root)?),
    };
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    walk(root, root, rules.as_ref(), &mut files)?;
    files.sort();

    let mut h = Sha256::new();
    if algorithm == IdentityAlgorithm::Vxws2 {
        h.update(algorithm.as_str().as_bytes());
        h.update([0u8]);
    }
    for (rel, path) in files {
        let bytes = std::fs::read(&path).map_err(|source| IdentityError::Io {
            path: path.clone(),
            source,
        })?;
        h.update(rel.as_bytes());
        h.update([0u8]);
        h.update(&bytes);
        h.update([0u8]);
    }
    // sha2 0.11 returns a `hybrid_array::Array`, which has no `LowerHex`; spell the hex out.
    let digest = h.finalize();
    Ok(WorkspaceIdentity {
        algorithm,
        value: digest.iter().map(|b| format!("{b:02x}")).collect(),
    })
}

/// The files the current rule (`vxws/2`) covers, as `(relative path, file)` in the sorted order
/// [`compute`] hashes them: exactly what a package's lock lists and its packed form holds
/// (`ARCHITECTURE.md` §6.0.7).
pub fn inputs(root: &Path) -> Result<Vec<(String, PathBuf)>, IdentityError> {
    let rules = rules(root)?;
    let mut files = Vec::new();
    walk(root, root, Some(&rules), &mut files)?;
    files.sort();
    Ok(files)
}

/// Which of `names`, top-level files in `root`, the current rule would leave out of the
/// identity. Validation uses it to refuse a `.vxpignore` that excludes the descriptor, the lock
/// or a document: the identity would then be blind to what the experiment is made of.
pub fn excluded_top_level<'a>(
    root: &Path,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<String>, IdentityError> {
    let rules = rules(root)?;
    Ok(names
        .into_iter()
        .filter(|n| rules.matched(Path::new(n), false).is_ignore())
        .map(str::to_string)
        .collect())
}

/// The `vxws/2` matcher: [`DEFAULT_EXCLUDES`], then the root `.vxpignore` if there is one.
fn rules(root: &Path) -> Result<Gitignore, IdentityError> {
    let ignore_file = root.join(IGNORE_FILE);
    let bad = |message: String| IdentityError::IgnoreFile {
        path: ignore_file.clone(),
        message,
    };
    let mut b = GitignoreBuilder::new(root);
    for pattern in DEFAULT_EXCLUDES {
        b.add_line(None, pattern)
            .map_err(|e| bad(format!("default exclude `{pattern}`: {e}")))?;
    }
    if ignore_file.is_file()
        && let Some(e) = b.add(&ignore_file)
    {
        return Err(bad(e.to_string()));
    }
    b.build().map_err(|e| bad(e.to_string()))
}

fn walk(
    base: &Path,
    dir: &Path,
    rules: Option<&Gitignore>,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), IdentityError> {
    let io = |source| IdentityError::Io {
        path: dir.to_path_buf(),
        source,
    };
    for entry in std::fs::read_dir(dir).map_err(io)? {
        let path = entry.map_err(io)?.path();
        let rel = path
            .strip_prefix(base)
            .expect("walked path under base")
            .to_string_lossy()
            .replace('\\', "/");
        let is_dir = path.is_dir();
        // Outputs are not inputs, under every algorithm and whatever `.vxpignore` says.
        if is_dir && rel == "results" {
            continue;
        }
        if let Some(rules) = rules {
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            // An excluded directory is not entered, so nothing under it can be re-included:
            // the same rule git applies.
            if rules.matched(Path::new(&rel), is_dir).is_ignore() {
                continue;
            }
        }
        if is_dir {
            walk(base, &path, rules, out)?;
        } else {
            out.push((rel, path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("vxid-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (n, c) in files {
            let p = root.join(n);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        }
        root
    }

    fn id(root: &Path, a: IdentityAlgorithm) -> String {
        compute(root, a).unwrap().value
    }

    /// The `vxws/1` **shared test vector**. Frozen: archives and run records carry values
    /// computed this way, and they stay verifiable only while this holds. `vxtelemetry`
    /// asserts the same value over the same fixture.
    #[test]
    fn vxws1_is_frozen() {
        let root = workspace(
            "v1",
            &[
                ("main.yaml", "kind: experiment\nid: id-test\n"),
                ("parts/local-thing", "payload\n"),
                ("scripts/run.sh", "#!/usr/bin/env bash\n"),
            ],
        );
        assert_eq!(
            id(&root, IdentityAlgorithm::Vxws1),
            "f095a9facfabea97828ac5ee48a0da5516bfe036b546f612bcf182856db0efac"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// The `vxws/2` **shared test vector**, over a fixture that exercises every exclusion.
    /// `vxtelemetry` asserts the same value over the same fixture; if it changes, both move
    /// together, deliberately.
    #[test]
    fn vxws2_shared_vector() {
        let root = workspace(
            "v2",
            &[
                ("main.yaml", "kind: experiment\nid: id-test\n"),
                ("parts/local-thing", "payload\n"),
                ("scripts/run.sh", "#!/usr/bin/env bash\n"),
                (".vxpignore", "scratch/\n"),
                ("scratch/junk", "local build output\n"),
                (".git/HEAD", "ref: refs/heads/main\n"),
                ("main.yaml~", "backup\n"),
                ("results/rep1.zip", "not an input"),
            ],
        );
        let v2 = id(&root, IdentityAlgorithm::Vxws2);
        assert_eq!(
            v2, "ddd70ed261fc314c991d00961eda8c4661e5881685f8f7febbd05c151cd39090",
            "the vxws/2 rule changed; vxtelemetry must move with it"
        );

        // The excluded files do not count...
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/other\n").unwrap();
        fs::write(root.join("scratch/junk"), "rebuilt\n").unwrap();
        fs::write(root.join("main.yaml~"), "older backup\n").unwrap();
        assert_eq!(id(&root, IdentityAlgorithm::Vxws2), v2);

        // `inputs` lists exactly what the identity hashes.
        let listed: Vec<String> = inputs(&root).unwrap().into_iter().map(|(r, _)| r).collect();
        assert_eq!(
            listed,
            [
                ".vxpignore",
                "main.yaml",
                "parts/local-thing",
                "scripts/run.sh"
            ]
        );

        // ...the inputs do, and so does the ignore file.
        fs::write(root.join(".vxpignore"), "scratch/\n*.tmp\n").unwrap();
        assert_ne!(id(&root, IdentityAlgorithm::Vxws2), v2);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_algorithms_never_agree_even_with_nothing_to_exclude() {
        let root = workspace("sep", &[("main.yaml", "kind: experiment\nid: e\n")]);
        assert_ne!(
            id(&root, IdentityAlgorithm::Vxws1),
            id(&root, IdentityAlgorithm::Vxws2)
        );
        fs::remove_dir_all(&root).ok();
    }

    /// `.git` is a file, not a directory, in a git worktree or submodule checkout.
    #[test]
    fn a_git_file_is_excluded_too() {
        let root = workspace("gitfile", &[("main.yaml", "kind: experiment\nid: e\n")]);
        let before = id(&root, IdentityAlgorithm::Vxws2);
        fs::write(root.join(".git"), "gitdir: /elsewhere\n").unwrap();
        fs::create_dir_all(root.join("parts/vendored/.git")).unwrap();
        fs::write(root.join("parts/vendored/.git/HEAD"), "x").unwrap();
        assert_eq!(id(&root, IdentityAlgorithm::Vxws2), before);
        fs::remove_dir_all(&root).ok();
    }

    /// Outputs are never inputs: `.vxpignore` cannot re-include `results/`.
    #[test]
    fn vxpignore_cannot_reinclude_results() {
        let root = workspace(
            "reinclude",
            &[
                ("main.yaml", "kind: experiment\nid: e\n"),
                (".vxpignore", "!results/\n"),
            ],
        );
        let before = id(&root, IdentityAlgorithm::Vxws2);
        fs::create_dir_all(root.join("results")).unwrap();
        fs::write(root.join("results/rep1.zip"), "x").unwrap();
        assert_eq!(id(&root, IdentityAlgorithm::Vxws2), before);
        fs::remove_dir_all(&root).ok();
    }

    /// `vxws/1` is what old records hold, so it must still ignore nothing but `results/`.
    #[test]
    fn vxws1_still_counts_everything_but_results() {
        let root = workspace("v1noise", &[("main.yaml", "kind: experiment\nid: e\n")]);
        let before = id(&root, IdentityAlgorithm::Vxws1);
        fs::write(root.join("main.yaml~"), "backup").unwrap();
        assert_ne!(id(&root, IdentityAlgorithm::Vxws1), before);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn algorithm_names_round_trip_and_unknown_ones_are_refused() {
        for a in [IdentityAlgorithm::Vxws1, IdentityAlgorithm::Vxws2] {
            assert_eq!(a.as_str().parse::<IdentityAlgorithm>().unwrap(), a);
        }
        assert!("vxws/9".parse::<IdentityAlgorithm>().is_err());
        assert_eq!(IdentityAlgorithm::CURRENT, IdentityAlgorithm::Vxws2);
    }
}
