//! **Packages** (`ARCHITECTURE.md` §6.0.7): the lock, the packed form and the package digest of a
//! `vxp/1` package. Everything here is local and offline: the `pkg` verbs are built on it, and the
//! daemon computes a run's package digest with [`package_digest`] without writing a file.
//!
//! The packed form is a plain ustar archive whose bytes are a pure function of the source: the
//! workspace identity's inputs, in its sorted order, each a `0644` regular file owned by uid and
//! gid 0 with mtime 0, no extension headers, and two zero blocks at the end. The writer and the
//! reader are hand-written because a general tar library writes GNU or PAX headers for long
//! names, stamps mtimes and pads to a blocking factor, and each of those would move the digest.
//! [`read_packed`] accepts exactly what [`pack_entries`] writes and nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::identity::{self, IdentityError};
use crate::model::PartKind;
use crate::package::{DESCRIPTOR_FILE, LOCK_FILE, PackageDoc};

/// The lock's own schema.
pub const LOCK_SCHEMA: &str = "vxp-lock/1";

const BLOCK: usize = 512;
const NAME_LEN: usize = 100;
const PREFIX_LEN: usize = 155;
/// The largest size an 11-digit octal field holds.
const MAX_SIZE: u64 = 0o77_777_777_777;

#[derive(Debug, thiserror::Error)]
pub enum PkgError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Discover(#[from] crate::package::DiscoverError),
    #[error("reading `{}`: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "`{path}` is a {what}. A package holds regular files only: replace it with the file it \
         stands for, or exclude it in `.vxpignore`."
    )]
    NotARegularFile { path: String, what: &'static str },
    #[error(
        "`{}` is not valid UTF-8. Package paths are UTF-8: rename it, or exclude it in \
         `.vxpignore`.",
        .path.display()
    )]
    NonUtf8Path { path: PathBuf },
    #[error(
        "`{path}` is too long for a packed package: ustar holds a name of up to {NAME_LEN} bytes \
         after a `/`-split prefix of up to {PREFIX_LEN}. Shorten it."
    )]
    PathTooLong { path: String },
    #[error("`{path}` is {size} bytes, more than a packed entry can hold")]
    TooLarge { path: String, size: u64 },
    #[error("the workspace does not build: {0}")]
    Workspace(String),
    #[error("`{LOCK_FILE}`: {0}")]
    Lock(String),
    #[error("cannot vendor: {0}")]
    Vendor(String),
    #[error("not a packed vxp/1 package{}: {reason}", .entry.as_ref().map(|e| format!(" (entry `{e}`)")).unwrap_or_default())]
    Packed {
        entry: Option<String>,
        reason: String,
    },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> PkgError + '_ {
    move |source| PkgError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// `sha256:` and the lower-case hex SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// The package's inputs as `(path, bytes)`, in packed order, refusing what a package cannot hold:
/// a symbolic link anywhere on the path, a hard link, a device, socket or FIFO, a non-UTF-8 path.
pub fn entries(root: &Path) -> Result<Vec<(String, Vec<u8>)>, PkgError> {
    let mut checked: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out = Vec::new();
    for (_, file) in identity::inputs(root)? {
        let rel = file.strip_prefix(root).expect("input under root");
        let Some(rel_str) = rel.to_str() else {
            return Err(PkgError::NonUtf8Path { path: file });
        };
        let rel_str = rel_str.replace('\\', "/");
        // A path ustar cannot hold is refused here, so `pkg validate` and `pkg lock` say so
        // before `pkg pack` would.
        if split_name(&rel_str).is_none() {
            return Err(PkgError::PathTooLong { path: rel_str });
        }
        // Every directory on the way, then the file: a symlinked directory makes its files look
        // regular, and the identity walk follows it.
        let mut at = root.to_path_buf();
        for component in rel.components() {
            at.push(component);
            if !checked.insert(at.clone()) {
                continue;
            }
            let meta = std::fs::symlink_metadata(&at).map_err(io(&at))?;
            let ft = meta.file_type();
            let what = if ft.is_symlink() {
                Some("symbolic link")
            } else if at == file && !ft.is_file() {
                Some("device, socket or FIFO")
            } else if at == file && hard_linked(&meta) {
                Some("hard link")
            } else {
                None
            };
            if let Some(what) = what {
                let path = at
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .replace('\\', "/");
                return Err(PkgError::NotARegularFile { path, what });
            }
        }
        let bytes = std::fs::read(&file).map_err(io(&file))?;
        out.push((rel_str, bytes));
    }
    Ok(out)
}

#[cfg(unix)]
fn hard_linked(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1
}

#[cfg(not(unix))]
fn hard_linked(_: &std::fs::Metadata) -> bool {
    false
}

/// Split `path` into ustar's `(prefix, name)`: the whole path as the name when it fits, otherwise
/// the **shortest** prefix ending before a `/` with which both fit (GNU tar and Python's `tarfile`
/// choose the same split).
fn split_name(path: &str) -> Option<(&str, &str)> {
    if path.len() <= NAME_LEN {
        return Some(("", path));
    }
    path.match_indices('/')
        .map(|(i, _)| (&path[..i], &path[i + 1..]))
        .find(|(prefix, name)| {
            prefix.len() <= PREFIX_LEN && name.len() <= NAME_LEN && !name.is_empty()
        })
}

fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(text.as_bytes());
    field[digits] = 0;
}

fn header(path: &str, size: u64) -> Result<[u8; BLOCK], PkgError> {
    let (prefix, name) = split_name(path).ok_or_else(|| PkgError::PathTooLong {
        path: path.to_string(),
    })?;
    if size > MAX_SIZE {
        return Err(PkgError::TooLarge {
            path: path.to_string(),
            size,
        });
    }
    let mut h = [0u8; BLOCK];
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], 0o644);
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0);
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    // devmajor and devminor stay empty, as Python's `tarfile` leaves them for a regular file.
    h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    // The checksum is taken with its own field read as spaces, and written as six octal digits,
    // NUL, space.
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..154].copy_from_slice(format!("{sum:06o}").as_bytes());
    h[154] = 0;
    h[155] = b' ';
    Ok(h)
}

/// The packed bytes of `entries`, which must be sorted by path with no duplicates. Pure.
pub fn pack_entries(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>, PkgError> {
    let mut out = Vec::new();
    let mut previous: Option<&str> = None;
    for (path, bytes) in entries {
        if previous.is_some_and(|p| p >= path.as_str()) {
            return Err(PkgError::Packed {
                entry: Some(path.clone()),
                reason: "entries must be in sorted path order, each once".to_string(),
            });
        }
        previous = Some(path);
        check_path(path)?;
        out.extend_from_slice(&header(path, bytes.len() as u64)?);
        out.extend_from_slice(bytes);
        out.resize(out.len().next_multiple_of(BLOCK), 0);
    }
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

fn check_path(path: &str) -> Result<(), PkgError> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..");
    if bad {
        return Err(PkgError::Packed {
            entry: Some(path.to_string()),
            reason: "paths are relative and `/`-separated, with no empty, `.` or `..` component"
                .to_string(),
        });
    }
    Ok(())
}

/// The packed bytes of the package at `root`.
pub fn pack(root: &Path) -> Result<Vec<u8>, PkgError> {
    crate::package::discover(root)?;
    pack_entries(&entries(root)?)
}

/// The package digest of the source folder at `root`: the digest its packed file would have,
/// computed without writing one.
pub fn package_digest(root: &Path) -> Result<String, PkgError> {
    Ok(sha256(&pack(root)?))
}

fn descriptor(root: &Path) -> Result<PackageDoc, PkgError> {
    Ok(crate::package::discover(root)?.package)
}

fn field_str<'a>(h: &'a [u8], entry: &str) -> Result<&'a str, PkgError> {
    let end = h.iter().position(|&b| b == 0).unwrap_or(h.len());
    std::str::from_utf8(&h[..end]).map_err(|_| PkgError::Packed {
        entry: Some(entry.to_string()),
        reason: "a header field is not UTF-8".to_string(),
    })
}

/// The entries of a packed package, refusing anything [`pack_entries`] would not have written
/// byte for byte: another entry type, mode, owner or mtime, an extension header, unsorted or
/// repeated paths, an unsafe path, or bytes after the end.
pub fn read_packed(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, PkgError> {
    let packed = |entry: Option<&str>, reason: &str| PkgError::Packed {
        entry: entry.map(str::to_string),
        reason: reason.to_string(),
    };
    if !bytes.len().is_multiple_of(BLOCK) {
        return Err(packed(
            None,
            "its length is not a whole number of 512-byte blocks",
        ));
    }
    let mut entries = Vec::new();
    let mut at = 0;
    loop {
        let Some(h) = bytes.get(at..at + BLOCK) else {
            return Err(packed(None, "it ends without its two zero blocks"));
        };
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let name = field_str(&h[..100], "?")?;
        let prefix = field_str(&h[345..500], name)?;
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if &h[257..263] != b"ustar\0" || &h[263..265] != b"00" {
            return Err(packed(Some(&path), "it is not a POSIX ustar header"));
        }
        if h[156] != b'0' {
            let what = match h[156] {
                b'x' | b'g' => "a PAX extension header",
                b'L' | b'K' => "a GNU long-name header",
                b'1' => "a hard link",
                b'2' => "a symbolic link",
                b'5' => "a directory entry",
                _ => "not a regular file",
            };
            return Err(packed(Some(&path), what));
        }
        check_path(&path)?;
        let size = u64::from_str_radix(field_str(&h[124..135], &path)?, 8)
            .map_err(|_| packed(Some(&path), "its size field is not octal"))?;
        let size = usize::try_from(size).map_err(|_| packed(Some(&path), "it is too large"))?;
        let start = at + BLOCK;
        let Some(data) = bytes.get(start..start + size) else {
            return Err(packed(Some(&path), "the archive ends inside it"));
        };
        // Everything else in the header must be what the writer writes: mode 0644, uid and gid 0,
        // empty user and group names, mtime 0, a correct checksum, nothing in the unused fields.
        if header(&path, size as u64)?[..] != h[..] {
            return Err(packed(
                Some(&path),
                "its header is not the canonical one (mode 0644, uid and gid 0, no user or group \
                 name, mtime 0, a correct checksum)",
            ));
        }
        entries.push((path, data.to_vec()));
        at = (start + size).next_multiple_of(BLOCK);
    }
    // Re-packing what was read must give these bytes back: that covers order, duplicates, padding
    // and anything after the two zero blocks.
    if pack_entries(&entries)? != bytes {
        return Err(packed(
            None,
            "it is not in canonical form (sorted entries, zero padding, exactly two zero blocks at \
             the end)",
        ));
    }
    Ok(entries)
}

/// Write the package `bytes` as a source folder at `dest`, which must not exist or be empty. The
/// result is checked by packing it again: an entry its own `.vxpignore` would exclude, or one
/// under `results/` or `.git`, makes a folder that does not pack to these bytes, and is refused.
pub fn unpack(bytes: &[u8], dest: &Path) -> Result<(), PkgError> {
    let entries = read_packed(bytes)?;
    if dest.exists() {
        let mut listing = std::fs::read_dir(dest).map_err(io(dest))?;
        if listing.next().is_some() {
            return Err(PkgError::Io {
                path: dest.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "the target folder is not empty; unpack into a new one",
                ),
            });
        }
    }
    for (path, data) in &entries {
        let file = dest.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        std::fs::write(&file, data).map_err(io(&file))?;
    }
    let repacked = pack(dest).and_then(|b| {
        (b == bytes).then_some(()).ok_or_else(|| PkgError::Packed {
            entry: None,
            reason: "its entries do not make a source folder that packs back to the same bytes \
                     (an entry its `.vxpignore` excludes, or one under `results/` or `.git`)"
                .to_string(),
        })
    });
    if let Err(e) = repacked {
        let _ = std::fs::remove_dir_all(dest);
        return Err(e);
    }
    Ok(())
}

/// `vxp.lock` (`ARCHITECTURE.md` §6.0.7). Field order is declaration order and `files` is sorted,
/// so writing the same lock always gives the same bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub schema: String,
    pub package: LockedPackage,
    /// The vxedge version that wrote this lock. Not compared by [`Lock::drift`].
    pub locked_with: String,
    /// Every identity input except `vxp.lock`, by path, with its `sha256:` digest.
    pub files: BTreeMap<String, String>,
    /// Every published bundle or WebAssembly part a document references, as the document pins it.
    #[serde(default)]
    pub parts: Vec<LockedRef>,
    /// Every published container image a document references, as the document pins it.
    #[serde(default)]
    pub images: Vec<LockedRef>,
    /// Documents copied from elsewhere by `pkg vendor`, kept across re-locks.
    #[serde(default)]
    pub vendored: Vec<Vendored>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPackage {
    pub id: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedRef {
    #[serde(rename = "ref")]
    pub reference: String,
    /// Absent for an image a document names by tag: the lock records what the document pins and
    /// cannot pin more offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vendored {
    pub file: String,
    pub source: String,
    pub digest: String,
}

impl Lock {
    /// The lock as `vxp.lock` holds it.
    pub fn to_yaml(&self) -> String {
        serde_yaml_ng::to_string(self).expect("a lock always serializes")
    }

    pub fn from_yaml(text: &str) -> Result<Self, PkgError> {
        let lock: Self =
            serde_yaml_ng::from_str(text).map_err(|e| PkgError::Lock(e.to_string()))?;
        if lock.schema != LOCK_SCHEMA {
            return Err(PkgError::Lock(format!(
                "`schema: {}` is not a lock this vxedge reads (it reads {LOCK_SCHEMA})",
                lock.schema
            )));
        }
        Ok(lock)
    }

    /// Where `self`, the written lock, and `current`, the package as it is now, disagree: one line
    /// each, naming the file, part or image. Empty when the lock holds.
    pub fn drift(&self, current: &Lock) -> Vec<String> {
        let mut out = Vec::new();
        if self.package != current.package {
            out.push(format!(
                "the lock is for {} {}, the descriptor says {} {}",
                self.package.id, self.package.version, current.package.id, current.package.version
            ));
        }
        for (path, digest) in &current.files {
            match self.files.get(path) {
                None => out.push(format!("`{path}` was added")),
                Some(d) if d != digest => out.push(format!("`{path}` changed")),
                Some(_) => {}
            }
        }
        for path in self.files.keys() {
            if !current.files.contains_key(path) {
                out.push(format!("`{path}` was removed"));
            }
        }
        for (what, locked, now) in [
            ("part", &self.parts, &current.parts),
            ("image", &self.images, &current.images),
        ] {
            for r in now.iter().filter(|r| !locked.contains(r)) {
                out.push(format!(
                    "{what} `{}` is not in the lock as referenced",
                    r.reference
                ));
            }
            for r in locked.iter().filter(|r| !now.contains(r)) {
                out.push(format!(
                    "{what} `{}` is locked but no longer referenced",
                    r.reference
                ));
            }
        }
        out
    }
}

/// The lock the package at `root` has now, written by vxedge `locked_with`. `vendored` is carried
/// over from the existing lock, which only `pkg vendor` writes.
pub fn lock(root: &Path, locked_with: &str) -> Result<Lock, PkgError> {
    let descriptor = descriptor(root)?;
    let files = entries(root)?
        .into_iter()
        .filter(|(path, _)| path != LOCK_FILE)
        .map(|(path, bytes)| (path, sha256(&bytes)))
        .collect();

    let ws =
        crate::workspace::Workspace::load(root).map_err(|e| PkgError::Workspace(e.to_string()))?;
    // One model per baseline: a parameter may bind `baseline` to run another per arm, and the
    // lock pins what every arm runs, so `pkg pull` and an offline copy can run any of them.
    let models = if ws.baselines.len() > 1 {
        ws.baselines
            .iter()
            .map(|b| {
                let mut one = ws.clone();
                if let Some(exp) = one.experiments.first_mut() {
                    exp.baseline = Some(b.id.clone());
                }
                crate::model::build(&one)
            })
            .collect::<Result<Vec<_>, _>>()
    } else {
        crate::model::build(&ws).map(|m| vec![m])
    }
    .map_err(|e| PkgError::Workspace(e.to_string()))?;
    let mut parts = BTreeSet::new();
    let mut images = BTreeSet::new();
    for model in &models {
        let published = model
            .entities
            .iter()
            .flat_map(|e| e.functions.values())
            .filter_map(|f| f.part.as_ref())
            .filter(|p| !p.local);
        for p in published {
            let r = LockedRef {
                reference: p.reference.clone(),
                digest: p.digest.clone(),
            };
            match p.kind {
                PartKind::Container => images.insert(r),
                PartKind::Bundle | PartKind::Wasm | PartKind::Attach => parts.insert(r),
            };
        }
        // A published artifact store is fetched into the part cache like a guest bundle, so it is a
        // part; a local one is content, already in `files`.
        for def in model.services.values() {
            if let Some(crate::services::ArtifactStore::Published { reference, digest }) =
                crate::services::ota_store(def)
            {
                parts.insert(LockedRef {
                    reference,
                    digest: Some(digest),
                });
            }
        }
        if let Some(nos) = &model.nos_check
            && let Some(reference) = &nos.part_ref
        {
            parts.insert(LockedRef {
                reference: reference.clone(),
                digest: nos.digest.clone(),
            });
        }
        // The 5G core and RAN are not placements: vxedged starts its own digest-pinned images.
        let fiveg = crate::fiveg::fiveg_images(model)
            .into_iter()
            .map(str::to_string);
        for url in model.placements().map(|p| p.image.url.clone()).chain(fiveg) {
            let url = &url;
            if url.starts_with("localhost/") || url.starts_with("local/") {
                continue;
            }
            images.insert(match url.split_once('@') {
                Some((reference, digest)) => LockedRef {
                    reference: reference.to_string(),
                    digest: Some(digest.to_string()),
                },
                None => LockedRef {
                    reference: url.clone(),
                    digest: None,
                },
            });
        }
    }

    let vendored = match std::fs::read_to_string(root.join(LOCK_FILE)) {
        Ok(text) => Lock::from_yaml(&text)
            .map(|l| l.vendored)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    Ok(Lock {
        schema: LOCK_SCHEMA.to_string(),
        package: LockedPackage {
            id: descriptor.id.as_str().to_string(),
            version: descriptor.version.to_string(),
        },
        locked_with: locked_with.to_string(),
        files,
        parts: parts.into_iter().collect(),
        images: images.into_iter().collect(),
        vendored,
    })
}

/// Write `lock` to `root`'s `vxp.lock`.
pub fn write_lock(root: &Path, lock: &Lock) -> Result<(), PkgError> {
    let path = root.join(LOCK_FILE);
    std::fs::write(&path, lock.to_yaml()).map_err(io(&path))
}

/// The lock `root` holds, if it has one.
pub fn read_lock(root: &Path) -> Result<Option<Lock>, PkgError> {
    let path = root.join(LOCK_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => Lock::from_yaml(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(PkgError::Io { path, source }),
    }
}

/// What `pkg validate` and `pkg lock --check` report about the lock: a missing lock, or each way
/// the written one disagrees with the package. Empty when it holds.
pub fn lock_problems(root: &Path) -> Result<Vec<String>, PkgError> {
    drift_from_written(root, &lock(root, "")?)
}

/// How the lock written at `root` differs from `current`; a missing lock is a difference.
fn drift_from_written(root: &Path, current: &Lock) -> Result<Vec<String>, PkgError> {
    Ok(match read_lock(root)? {
        None => vec![format!("there is no `{LOCK_FILE}`; run `vxedge pkg lock`")],
        Some(written) => written.drift(current),
    })
}

/// The packed file name: `<name>-<version>.vxp`.
pub fn packed_file_name(root: &Path) -> Result<String, PkgError> {
    let d = descriptor(root)?;
    Ok(format!("{}-{}.vxp", d.id.name(), d.version))
}

/// The descriptor and lock a packed package carries, read without unpacking it.
pub fn packed_descriptor_and_lock(bytes: &[u8]) -> Result<(String, Option<String>), PkgError> {
    let entries = read_packed(bytes)?;
    let text = |name: &str| {
        entries
            .iter()
            .find(|(p, _)| p == name)
            .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
    };
    let descriptor = text(DESCRIPTOR_FILE).ok_or_else(|| PkgError::Packed {
        entry: None,
        reason: format!("it holds no `{DESCRIPTOR_FILE}`"),
    })?;
    Ok((descriptor, text(LOCK_FILE)))
}

/// What a run of a `vxp/1` package records about it (`ARCHITECTURE.md` §13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPackage {
    pub id: String,
    pub version: String,
    /// The package digest of what runs: [`package_digest`] of the folder at `up`.
    pub digest: String,
    /// Whether `up` checked the lock. `false` only for `up --unlocked`.
    pub locked: bool,
}

/// Why `up` refuses a `vxp/1` package before anything runs.
#[derive(Debug, thiserror::Error)]
pub enum RunRefusal {
    #[error(
        "the package does not match its `{LOCK_FILE}`: {}. Run `vxedge pkg lock` to record the \
         change, or `up --unlocked` while authoring; an unlocked run is recorded as unlocked.",
        .problems.join("; ")
    )]
    LockMismatch { problems: Vec<String> },
    #[error(transparent)]
    Package(#[from] PkgError),
}

/// The package a run from `root` records, after the checks `up` makes on it. `None` without a
/// readable `vxp.yaml`, which the descriptor check reports. Unless
/// `unlocked`, a lock that does not match is refused, naming each difference. Locked or not, a
/// package that cannot be packed is refused with `pkg pack`'s error, since its run could not
/// record a digest.
pub fn run_package(root: &Path, unlocked: bool) -> Result<Option<RunPackage>, RunRefusal> {
    if crate::package::discover(root).is_err() {
        // Reported by the descriptor check, which runs first.
        return Ok(None);
    }
    let digest = package_digest(root)?;
    let current = lock(root, "")?;
    if !unlocked {
        let problems = drift_from_written(root, &current)?;
        if !problems.is_empty() {
            return Err(RunRefusal::LockMismatch { problems });
        }
    }
    Ok(Some(RunPackage {
        id: current.package.id,
        version: current.package.version,
        digest,
        locked: !unlocked,
    }))
}

/// What [`vendor`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct VendorReport {
    /// The entries written to the lock's `vendored`, one per copied document.
    pub copied: Vec<Vendored>,
    /// What the source holds that was not copied, with why: `(name, reason)`.
    pub left_behind: Vec<(String, &'static str)>,
    /// Set when the package no longer builds after the copy, so the lock could not be rebuilt in
    /// full: the `vendored` entries and `files` are written, and the documents need fixing before
    /// `pkg lock`.
    pub relock_error: Option<String>,
    /// Local references the package makes and does not hold: a `./` bundle or artifact
    /// directory, or a `localhost/` image built per host. `up` refuses without them. Empty when
    /// the package does not build.
    pub local_refs: Vec<String>,
}

/// Why something in a vendoring source is not copied.
fn left_behind_reason(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        "directory: build output or content, not a document"
    } else if name.ends_with(".sh") {
        "host recipe: a script may reach outside its folder"
    } else if name == DESCRIPTOR_FILE || name == LOCK_FILE || name.ends_with(".vxp") {
        "package descriptor or lock"
    } else {
        "not a document"
    }
}

/// Copy the vxedge documents of `source` into the package at `root`, record each in the lock's
/// `vendored` under `source_id`, and re-lock (`ARCHITECTURE.md` §6.0.7, decision X-5). Documents
/// only: scripts, directories and descriptors stay behind and are listed. A destination that
/// exists and was not vendored from `source_id` is refused unless `replace`.
///
/// The `vendored` entries are written even when the package stops building, which is the usual
/// first step (the scaffold's `main.yaml` names the vehicle the copy replaced); the report says
/// so and the full re-lock waits for `pkg lock`.
/// A relative path inside the folder, `/`-separated, or `None` for one that would leave it.
fn inside(path: &str) -> Option<String> {
    let rel = path
        .strip_prefix("./")
        .unwrap_or(path)
        .trim_end_matches('/');
    let safe = !rel.is_empty()
        && !rel.starts_with('/')
        && rel
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..");
    safe.then(|| rel.to_string())
}

/// The local paths the documents at `paths` reference: every `ref` or `artifacts` string that
/// starts with `./` (parts and artifact stores), and a road's `network` and a scenario's
/// `storyboard` (their OpenDRIVE and OpenSCENARIO files), each as a `/`-separated path inside
/// the folder. A path that would leave the folder is ignored: a document cannot vendor what is
/// outside it.
fn referenced_local_paths(paths: &[PathBuf]) -> Vec<String> {
    fn walk(v: &serde_yaml_ng::Value, out: &mut BTreeSet<String>) {
        match v {
            serde_yaml_ng::Value::Mapping(m) => {
                for (k, v) in m {
                    if let (Some("ref" | "artifacts"), Some(s)) = (k.as_str(), v.as_str())
                        && let Some(rel) = s.strip_prefix("./")
                    {
                        let rel = rel.trim_end_matches('/');
                        let safe = !rel.is_empty()
                            && rel
                                .split('/')
                                .all(|c| !c.is_empty() && c != "." && c != "..");
                        if safe {
                            out.insert(rel.to_string());
                        }
                    }
                    walk(v, out);
                }
            }
            serde_yaml_ng::Value::Sequence(items) => items.iter().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for doc in serde_yaml_ng::Deserializer::from_str(&text) {
            if let Ok(v) = serde_yaml_ng::Value::deserialize(doc) {
                walk(&v, &mut out);
                let file = match v.get("kind").and_then(|k| k.as_str()) {
                    Some("road") => v.get("network"),
                    Some("scenario") => v.get("storyboard"),
                    _ => None,
                };
                if let Some(rel) = file.and_then(|f| f.as_str()).and_then(inside) {
                    out.insert(rel);
                }
            }
        }
    }
    out.into_iter().collect()
}

pub fn vendor(
    root: &Path,
    source: &Path,
    source_id: &str,
    replace: bool,
    locked_with: &str,
    tracked: Option<&[String]>,
) -> Result<VendorReport, PkgError> {
    let descriptor = descriptor(root)?;
    let canonical = |p: &Path| p.canonicalize().map_err(io(p));
    let (root_c, source_c) = (canonical(root)?, canonical(source)?);
    if source_c.starts_with(&root_c) {
        return Err(PkgError::Vendor(format!(
            "`{}` is inside the package; vendor from a folder outside it",
            source.display()
        )));
    }
    if source_id.trim().is_empty() || source_id.contains(char::is_whitespace) {
        return Err(PkgError::Vendor(format!(
            "`{source_id}` is not a source name; use `<owner>/<repo>/<path>`"
        )));
    }
    // The source must read as a workspace's documents, as `vehicle up` reads a vehicle folder.
    crate::workspace::Workspace::load(source)
        .map_err(|e| PkgError::Vendor(format!("`{}`: {e}", source.display())))?;
    let documents = crate::workspace::document_paths(source).map_err(io(source))?;
    if documents.is_empty() {
        return Err(PkgError::Vendor(format!(
            "`{}` holds no vxedge documents (`*.yaml`)",
            source.display()
        )));
    }
    let names: Vec<String> = documents
        .iter()
        .map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
                .ok_or_else(|| PkgError::NonUtf8Path { path: p.clone() })
        })
        .collect::<Result<Vec<String>, _>>()?;
    // Local parts the documents reference travel with them when they are content: files the
    // source's git checkout tracks. Untracked ones are build outputs and stay behind.
    let referenced = referenced_local_paths(&documents);
    let part_files: Vec<String> = tracked
        .unwrap_or_default()
        .iter()
        .filter(|t| {
            referenced
                .iter()
                .any(|r| *t == r || t.starts_with(&format!("{r}/")))
        })
        .filter(|t| source.join(t).is_file())
        .cloned()
        .collect();
    let names: Vec<String> = names
        .into_iter()
        .chain(part_files.iter().cloned())
        .collect();
    let documents: Vec<PathBuf> = documents
        .into_iter()
        .chain(part_files.iter().map(|t| source.join(t)))
        .collect();

    let written = read_lock(root)?;
    let previous = written
        .as_ref()
        .map(|l| l.vendored.clone())
        .unwrap_or_default();
    let ours = |file: &str| {
        previous
            .iter()
            .any(|v| v.file == file && v.source == source_id)
    };
    let conflicts: Vec<&str> = names
        .iter()
        .filter(|n| root.join(n).exists() && !ours(n))
        .map(String::as_str)
        .collect();
    if !conflicts.is_empty() && !replace {
        return Err(PkgError::Vendor(format!(
            "the package already has {}, not vendored from `{source_id}`. Pass `--replace` to \
             overwrite them with the copies.",
            conflicts
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let mut copied = Vec::new();
    for (path, name) in documents.iter().zip(&names) {
        let bytes = std::fs::read(path).map_err(io(path))?;
        let dest = root.join(name);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(io(dir))?;
        }
        std::fs::write(&dest, &bytes).map_err(io(&dest))?;
        copied.push(Vendored {
            file: name.clone(),
            source: source_id.to_string(),
            digest: sha256(&bytes),
        });
    }
    let mut vendored: Vec<Vendored> = previous
        .into_iter()
        .filter(|v| !names.contains(&v.file))
        .chain(copied.iter().cloned())
        .collect();
    vendored.sort_by(|a, b| a.file.cmp(&b.file));

    // First the lock that needs no model: `files` and `vendored` current, the rest as written.
    let files = entries(root)?
        .into_iter()
        .filter(|(path, _)| path != LOCK_FILE)
        .map(|(path, bytes)| (path, sha256(&bytes)))
        .collect();
    let interim = Lock {
        schema: LOCK_SCHEMA.to_string(),
        package: LockedPackage {
            id: descriptor.id.as_str().to_string(),
            version: descriptor.version.to_string(),
        },
        locked_with: locked_with.to_string(),
        files,
        parts: written
            .as_ref()
            .map(|l| l.parts.clone())
            .unwrap_or_default(),
        images: written
            .as_ref()
            .map(|l| l.images.clone())
            .unwrap_or_default(),
        vendored,
    };
    write_lock(root, &interim)?;

    let mut left_behind = Vec::new();
    let mut listing: Vec<_> = std::fs::read_dir(source)
        .map_err(io(source))?
        .filter_map(Result::ok)
        .collect();
    listing.sort_by_key(|e| e.file_name());
    for entry in listing {
        let name = entry.file_name().to_string_lossy().into_owned();
        if names.contains(&name)
            || part_files
                .iter()
                .any(|f| f.starts_with(&format!("{name}/")))
        {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let reason = if referenced
            .iter()
            .any(|r| r == &name || r.starts_with(&format!("{name}/")))
        {
            if tracked.is_some() {
                "a local part the documents reference, not tracked in git: a build output"
            } else {
                "a local part the documents reference; with no git checkout to tell content \
                 from build output, it is not copied"
            }
        } else {
            left_behind_reason(&name, is_dir)
        };
        left_behind.push((name.clone(), reason));
    }

    let (relock_error, local_refs) = match lock(root, locked_with) {
        Ok(full) => {
            write_lock(root, &full)?;
            (None, local_refs(root))
        }
        Err(e) => (
            Some(format!(
                "{e}. The copies are in place and recorded in `{LOCK_FILE}`; fix the documents \
                 that still refer to what they replaced (often `main.yaml`), then run \
                 `vxedge pkg lock`."
            )),
            Vec::new(),
        ),
    };
    Ok(VendorReport {
        copied,
        left_behind,
        relock_error,
        local_refs,
    })
}

/// Local references the package at `root` makes and does not hold, as `up`'s preflight would find
/// them: `./` guest bundles, `config.artifacts` directories, and `localhost/` images, which are
/// built per host and so always listed. Empty when the package does not build.
pub fn local_refs(root: &Path) -> Vec<String> {
    let Ok(ws) = crate::workspace::Workspace::load(root) else {
        return Vec::new();
    };
    let Ok(model) = crate::model::build(&ws) else {
        return Vec::new();
    };
    let mut refs = BTreeSet::new();
    for path in crate::zones::missing_local_bundles(&model, root) {
        let shown = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        refs.insert(format!("./{shown} (a guest bundle, not in the package)"));
    }
    for def in model.services.values() {
        if let Some(store) = crate::services::ota_artifacts(def)
            && !root.join(&store).is_dir()
        {
            let shown = format!("./{}", store.trim_start_matches("./"));
            refs.insert(format!(
                "{shown} (the artifact store of service `{}`, not in the package)",
                def.id
            ));
        }
    }
    let images = model
        .entities
        .iter()
        .flat_map(|e| e.functions.values())
        .filter_map(|f| f.part.as_ref())
        .filter(|p| p.kind == PartKind::Container)
        .map(|p| p.reference.clone())
        .chain(model.placements().map(|p| p.image.url.clone()));
    for image in images.filter(|i| i.starts_with("localhost/")) {
        refs.insert(format!("{image} (built per host)"));
    }
    refs.into_iter().collect()
}

/// What [`vendor_check`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VendorCheck {
    /// Each way the package's vendored copies disagree with what was recorded or with their
    /// sources. Empty when they agree.
    pub problems: Vec<String>,
    /// Vendored files whose source could not be found here, so drift from it was not checked.
    pub unchecked: Vec<String>,
}

/// Check every vendored copy: against the digest recorded when it was copied (a local edit), and,
/// when `resolve` finds the source folder, against the source as it is now (drift). Source lookup
/// is the caller's, since it depends on the checkout around the package.
pub fn vendor_check(
    root: &Path,
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<VendorCheck, PkgError> {
    let mut out = VendorCheck::default();
    let Some(lock) = read_lock(root)? else {
        return Ok(out);
    };
    for v in &lock.vendored {
        let here = root.join(&v.file);
        match std::fs::read(&here) {
            Ok(bytes) if sha256(&bytes) != v.digest => out.problems.push(format!(
                "`{}` was edited in the package since it was vendored from `{}`",
                v.file, v.source
            )),
            Ok(_) => {}
            Err(_) => out.problems.push(format!(
                "`{}` was vendored from `{}` and is no longer in the package",
                v.file, v.source
            )),
        }
        match resolve(&v.source) {
            None => out
                .unchecked
                .push(format!("`{}` (source `{}` is not here)", v.file, v.source)),
            Some(dir) => match std::fs::read(dir.join(&v.file)) {
                Ok(bytes) if sha256(&bytes) != v.digest => out.problems.push(format!(
                    "`{}` changed at its source `{}` since it was vendored; run `vxedge pkg \
                     vendor` again to take the change",
                    v.file, v.source
                )),
                Ok(_) => {}
                Err(_) => out.problems.push(format!(
                    "`{}` is gone from its source `{}`",
                    v.file, v.source
                )),
            },
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("vxpkg-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (n, c) in files {
            let p = root.join(n);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        }
        root
    }

    const DESCRIPTOR: &str =
        "schema: vxp/1\nid: vxedge/id-test\nversion: 1.0.0\nentry: main.yaml\n";
    const MAIN: &str = "kind: experiment\nid: id-test\n";

    /// The packed-bytes **shared test vector**: the `vxws/2` fixture with a `vxp/1` descriptor and
    /// a literal lock. A packed file's digest is a public identity, so this value is frozen like
    /// the workspace identity's; `vxtelemetry` asserts the same value over the same fixture.
    /// Python's `tarfile` (ustar format, mode 0644, mtime 0, no user or group names) writes the
    /// same bytes for these entries, up to its padding to a 10 KiB record.
    #[test]
    fn packed_bytes_shared_vector() {
        let root = workspace(
            "vector",
            &[
                ("vxp.yaml", DESCRIPTOR),
                ("vxp.lock", "schema: vxp-lock/1\n"),
                ("main.yaml", MAIN),
                ("parts/local-thing", "payload\n"),
                ("scripts/run.sh", "#!/usr/bin/env bash\n"),
                (".vxpignore", "scratch/\n"),
                ("scratch/junk", "local build output\n"),
                (".git/HEAD", "ref: refs/heads/main\n"),
                ("main.yaml~", "backup\n"),
                ("results/rep1.zip", "not an input"),
            ],
        );
        let bytes = pack(&root).unwrap();
        // Six 512-byte headers, six data blocks, two zero blocks.
        assert_eq!(bytes.len(), (6 + 6 + 2) * BLOCK);
        let digest = sha256(&bytes);
        assert_eq!(
            digest, "sha256:1888e766b259786d5d065a4a7c85f3d3ddc6bb589a30cb257156b85e9155a5c6",
            "the packed form changed; vxtelemetry must move with it"
        );
        assert_eq!(package_digest(&root).unwrap(), digest);
        fs::remove_dir_all(&root).ok();
    }

    /// The second **shared test vector**, for the header edge cases the first does not reach: a
    /// path over 100 bytes that splits at the shortest prefix (not the longest), a name of exactly
    /// 100 bytes with no terminating NUL, an empty file (no data block) and a file of exactly one
    /// block (no padding). `vxtelemetry` pins the same value, and Python's `tarfile` writes the same
    /// bytes, up to its 10 KiB record padding.
    #[test]
    fn packed_bytes_split_vector() {
        let long = format!(
            "parts/{}/{}/{}/file.txt",
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(40)
        );
        let exact = format!("parts/{}", "d".repeat(94));
        let block = "x".repeat(BLOCK);
        let root = workspace(
            "split-vector",
            &[
                (
                    "vxp.yaml",
                    "schema: vxp/1\nid: vxedge/split-test\nversion: 1.0.0\nentry: main.yaml\n",
                ),
                ("vxp.lock", "schema: vxp-lock/1\n"),
                ("main.yaml", "kind: experiment\nid: split-test\n"),
                (&long, "split at the shortest prefix\n"),
                (&exact, "a name of exactly 100 bytes\n"),
                ("parts/empty", ""),
                ("parts/block", &block),
            ],
        );
        assert_eq!((long.len(), exact.len()), (137, 100));
        let bytes = pack(&root).unwrap();
        // Seven headers, six data blocks (the empty file has none), two zero blocks.
        assert_eq!(bytes.len(), (7 + 6 + 2) * BLOCK);
        let at = |path: &str| {
            let (_, header) = bytes
                .chunks(BLOCK)
                .enumerate()
                .find(|(_, h)| h[257..263] == *b"ustar\0" && h[..100].starts_with(path.as_bytes()))
                .unwrap();
            header.to_vec()
        };
        let split = at("bbbb");
        assert!(split[345..].starts_with(format!("parts/{}\0", "a".repeat(40)).as_bytes()));
        assert_eq!(at(&exact)[..100], *exact.as_bytes());
        assert_eq!(
            sha256(&bytes),
            "sha256:c1e0906cd96c16c02c3bdc814982767c582573d4b3707cc56f20048c6d1c76b3",
            "the packed form changed; vxtelemetry must move with it"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// The bytes are what a standard tool reads: GNU tar lists the entries in order.
    #[test]
    fn tar_reads_the_packed_form() {
        let root = workspace(
            "gnutar",
            &[
                ("vxp.yaml", DESCRIPTOR),
                ("main.yaml", MAIN),
                ("a/b/c.txt", "x"),
            ],
        );
        let file = root.with_extension("vxp");
        fs::write(&file, pack(&root).unwrap()).unwrap();
        let Ok(out) = std::process::Command::new("tar")
            .arg("-tvf")
            .arg(&file)
            .output()
        else {
            return; // no tar on this host
        };
        let listing = String::from_utf8_lossy(&out.stdout);
        let names: Vec<&str> = listing
            .lines()
            .filter_map(|l| l.split_whitespace().last())
            .collect();
        assert_eq!(names, ["a/b/c.txt", "main.yaml", "vxp.yaml"], "{listing}");
        assert!(
            listing.lines().all(|l| l.starts_with("-rw-r--r-- 0/0")),
            "{listing}"
        );
        fs::remove_file(&file).ok();
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn modes_and_order_of_creation_do_not_move_the_digest() {
        let a = workspace(
            "order-a",
            &[("vxp.yaml", DESCRIPTOR), ("main.yaml", MAIN), ("z.sh", "z")],
        );
        let b = workspace(
            "order-b",
            &[("z.sh", "z"), ("main.yaml", MAIN), ("vxp.yaml", DESCRIPTOR)],
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(b.join("z.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(package_digest(&a).unwrap(), package_digest(&b).unwrap());
        fs::remove_dir_all(&a).ok();
        fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn a_long_path_splits_at_the_shortest_prefix_and_too_long_is_refused() {
        let dir = "d".repeat(60);
        let long = format!("{dir}/{dir}/f.txt");
        assert_eq!(split_name(&long), Some((dir.as_str(), &long[61..])));
        assert_eq!(split_name("short"), Some(("", "short")));
        let name_too_long = format!("a/{}", "n".repeat(101));
        assert_eq!(split_name(&name_too_long), None);
        assert!(matches!(
            pack_entries(&[(name_too_long.clone(), vec![])]),
            Err(PkgError::PathTooLong { .. })
        ));
        // A split path reads back whole.
        let bytes = pack_entries(&[(long.clone(), b"x".to_vec())]).unwrap();
        assert_eq!(read_packed(&bytes).unwrap(), vec![(long, b"x".to_vec())]);
    }

    /// A path too long to pack is refused when the inputs are read, so validation reports it.
    #[test]
    fn an_unpackable_path_is_refused_with_the_inputs() {
        let long = "n".repeat(101);
        let root = workspace(
            "longname",
            &[
                ("vxp.yaml", DESCRIPTOR),
                ("main.yaml", MAIN),
                (long.as_str(), "x"),
            ],
        );
        match entries(&root) {
            Err(PkgError::PathTooLong { path }) => assert_eq!(path, long),
            other => panic!("expected PathTooLong, got {other:?}"),
        }
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn links_and_special_files_are_refused_by_name() {
        let root = workspace(
            "links",
            &[
                ("vxp.yaml", DESCRIPTOR),
                ("main.yaml", MAIN),
                ("real/x", "x"),
            ],
        );
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        match pack(&root) {
            Err(PkgError::NotARegularFile { path, what }) => {
                assert_eq!(path, "alias");
                assert_eq!(what, "symbolic link");
            }
            other => panic!("expected a refused symlink, got {other:?}"),
        }
        fs::remove_file(root.join("alias")).unwrap();
        fs::hard_link(root.join("real/x"), root.join("y")).unwrap();
        assert!(matches!(
            pack(&root),
            Err(PkgError::NotARegularFile {
                what: "hard link",
                ..
            })
        ));
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let root = workspace("utf8", &[("vxp.yaml", DESCRIPTOR), ("main.yaml", MAIN)]);
        let odd = root.join(std::ffi::OsStr::from_bytes(b"caf\xe9.txt"));
        if fs::write(&odd, "x").is_err() {
            return; // a filesystem that refuses the name already
        }
        assert!(matches!(pack(&root), Err(PkgError::NonUtf8Path { .. })));
        fs::remove_dir_all(&root).ok();
    }

    /// A legacy `<name>.vxp` descriptor is no longer read, so it has no packed form.
    #[test]
    fn a_legacy_descriptor_has_no_packed_form() {
        let root = workspace(
            "legacy",
            &[("id-test.vxp", "id: id-test\n"), ("main.yaml", MAIN)],
        );
        assert!(matches!(
            pack(&root),
            Err(PkgError::Discover(
                crate::package::DiscoverError::LegacyDescriptor { .. }
            ))
        ));
        fs::remove_dir_all(&root).ok();
    }

    fn canonical() -> Vec<u8> {
        pack_entries(&[
            ("a.txt".to_string(), b"alpha".to_vec()),
            ("b/c.txt".to_string(), b"gamma".to_vec()),
        ])
        .unwrap()
    }

    #[test]
    fn the_canonical_form_reads_back() {
        let entries = read_packed(&canonical()).unwrap();
        assert_eq!(entries[0], ("a.txt".to_string(), b"alpha".to_vec()));
        assert_eq!(entries[1].0, "b/c.txt");
    }

    /// Each way a tar file can differ from the canonical form is refused, naming the entry.
    #[test]
    fn a_non_canonical_archive_is_refused() {
        let reseal = |b: &mut [u8], at: usize| {
            let h = &mut b[at..at + BLOCK];
            h[148..156].copy_from_slice(b"        ");
            let sum: u32 = h.iter().map(|&x| u32::from(x)).sum();
            h[148..154].copy_from_slice(format!("{sum:06o}").as_bytes());
        };
        let refused = |b: &[u8], needle: &str| match read_packed(b) {
            Err(e @ PkgError::Packed { .. }) => {
                let msg = e.to_string();
                assert!(msg.contains(needle), "expected `{needle}` in: {msg}");
            }
            other => panic!("expected a refusal containing `{needle}`, got {other:?}"),
        };

        let mut mode = canonical();
        mode[100..107].copy_from_slice(b"0000755");
        reseal(&mut mode, 0);
        refused(&mode, "a.txt");

        let mut mtime = canonical();
        mtime[136..147].copy_from_slice(b"14000000000");
        reseal(&mut mtime, 0);
        refused(&mtime, "canonical");

        let mut owner = canonical();
        owner[265..269].copy_from_slice(b"root");
        reseal(&mut owner, 0);
        refused(&owner, "canonical");

        let mut symlink = canonical();
        symlink[156] = b'2';
        reseal(&mut symlink, 0);
        refused(&symlink, "symbolic link");

        let mut pax = canonical();
        pax[156] = b'x';
        reseal(&mut pax, 0);
        refused(&pax, "PAX");

        let mut checksum = canonical();
        checksum[150] = b'7';
        refused(&checksum, "checksum");

        let mut trailing = canonical();
        trailing.extend_from_slice(&[0u8; BLOCK]);
        refused(&trailing, "canonical form");

        let unsorted = [
            pack_entries(&[("b.txt".to_string(), vec![])]).unwrap(),
            pack_entries(&[("a.txt".to_string(), vec![])]).unwrap(),
        ];
        let mut swapped = unsorted[0][..BLOCK].to_vec();
        swapped.extend_from_slice(&unsorted[1]);
        refused(&swapped, "sorted");

        let mut dotdot = canonical();
        dotdot[..5].copy_from_slice(b"../a.");
        reseal(&mut dotdot, 0);
        refused(&dotdot, "`..`");

        refused(&canonical()[..BLOCK * 4], "two zero blocks");
    }

    #[test]
    fn unpack_restores_a_folder_that_packs_to_the_same_bytes() {
        let root = workspace(
            "unpack-src",
            &[
                ("vxp.yaml", DESCRIPTOR),
                ("main.yaml", MAIN),
                ("parts/p.bin", "p"),
            ],
        );
        let bytes = pack(&root).unwrap();
        let dest = root.with_file_name(format!("vxpkg-{}-unpacked", std::process::id()));
        let _ = fs::remove_dir_all(&dest);
        unpack(&bytes, &dest).unwrap();
        assert_eq!(pack(&dest).unwrap(), bytes);
        assert_eq!(
            identity::compute(&dest, identity::IdentityAlgorithm::Vxws2).unwrap(),
            identity::compute(&root, identity::IdentityAlgorithm::Vxws2).unwrap()
        );
        // Not over an existing folder's contents.
        assert!(unpack(&bytes, &dest).is_err());
        fs::remove_dir_all(&dest).ok();
        fs::remove_dir_all(&root).ok();
    }

    /// An archive whose entries the folder's own rules would exclude cannot round-trip, so it is
    /// refused and nothing is left behind.
    #[test]
    fn unpack_refuses_entries_the_identity_would_exclude() {
        let bytes = pack_entries(&[
            ("main.yaml".to_string(), MAIN.as_bytes().to_vec()),
            ("results/out.zip".to_string(), b"x".to_vec()),
            ("vxp.yaml".to_string(), DESCRIPTOR.as_bytes().to_vec()),
        ])
        .unwrap();
        let dest = std::env::temp_dir().join(format!("vxpkg-{}-sneaky", std::process::id()));
        let _ = fs::remove_dir_all(&dest);
        assert!(matches!(
            unpack(&bytes, &dest),
            Err(PkgError::Packed { .. })
        ));
        assert!(!dest.exists());
    }

    const PINNED: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    /// A scaffolded workspace that builds, with a `vxp/1` descriptor and one published,
    /// digest-pinned container part beside its local ones.
    fn locked_workspace(name: &str) -> PathBuf {
        let parent = std::env::temp_dir().join(format!("vxpkg-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir_all(&parent).unwrap();
        let root =
            crate::scaffold::WorkspacePlan::plan("id-test", crate::scaffold::Template::Minimal)
                .unwrap()
                .write(&parent)
                .unwrap();
        fs::write(root.join("vxp.yaml"), DESCRIPTOR).unwrap();
        let parts = fs::read_to_string(root.join("parts.yaml"))
            .unwrap()
            .replace(
                "  - ref: local/vxprobe:host-process\n",
                &format!("  - ref: ghcr.io/vxedge/parts/vxprobe\n    digest: \"{PINNED}\"\n"),
            );
        fs::write(root.join("parts.yaml"), parts).unwrap();
        fs::create_dir_all(root.join("parts")).unwrap();
        fs::write(root.join("parts/p.bin"), "p").unwrap();
        root
    }

    /// A workspace with two baselines locks what both run: a parameter may bind `baseline` to run
    /// either per arm, so a pull or an offline copy needs every arm's references.
    #[test]
    fn a_lock_pins_every_baselines_references() {
        let root = locked_workspace("two-baselines");
        let parts = fs::read_to_string(root.join("parts.yaml")).unwrap();
        fs::write(
            root.join("parts-alt.yaml"),
            parts.replace("id: bl-local", "id: bl-alt").replace(
                "ghcr.io/vxedge/parts/vxprobe",
                "ghcr.io/vxedge/parts/agent/load/app/alt",
            ),
        )
        .unwrap();
        let locked = lock(&root, "0.8.0").unwrap();
        for reference in [
            "ghcr.io/vxedge/parts/vxprobe",
            "ghcr.io/vxedge/parts/agent/load/app/alt",
        ] {
            assert!(
                locked.images.iter().any(|r| r.reference == reference),
                "{reference}: {:?}",
                locked.images
            );
        }
        fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    /// An `s-nssai` slice starts the 5G core and RAN, which are no placement: the lock pins their
    /// images by digest all the same, and drops them when no slice is `s-nssai`.
    #[test]
    fn an_snssai_slice_locks_the_5g_images() {
        let root = locked_workspace("fiveg");
        let plain = lock(&root, "0.8.0").unwrap();
        let main = fs::read_to_string(root.join("main.yaml")).unwrap().replace(
            "mode: vlan, vlan: 101,",
            "mode: s-nssai, s_nssai: { sst: 1, sd: '000001' },",
        );
        fs::write(root.join("main.yaml"), main).unwrap();
        let fiveg = lock(&root, "0.8.0").unwrap();
        for image in [
            crate::fiveg::FIVEG_CORE_IMAGE,
            crate::fiveg::FIVEG_RAN_IMAGE,
            crate::fiveg::FIVEG_MONGO_IMAGE,
        ] {
            let (reference, digest) = image.split_once('@').unwrap();
            let locked = LockedRef {
                reference: reference.into(),
                digest: Some(digest.into()),
            };
            assert!(fiveg.images.contains(&locked), "{:?}", fiveg.images);
            assert!(!plain.images.contains(&locked), "{:?}", plain.images);
        }
        fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    /// A published artifact store is a part like any other: the lock lists it by ref and digest,
    /// so a pull or an offline copy knows to fetch it. A local store is content, in `files`.
    #[test]
    fn a_published_artifact_store_is_locked_as_a_part() {
        let root = locked_workspace("store");
        let store = format!("ghcr.io/vxedge/parts/svc/ota/config/gate-store@{PINNED}");
        let vehicle = fs::read_to_string(root.join("vehicle.yaml")).unwrap();
        fs::write(
            root.join("vehicle.yaml"),
            format!(
                "{vehicle}services:\n  ota:\n    id: ota-01\n    type: ota\n    image: {{ url: \
                 ghcr.io/vxedge/parts/svc/ota/app/vxota@{PINNED}, kind: container }}\n    on: hpc1\n    \
                 config:\n      artifacts: {store}\n"
            ),
        )
        .unwrap();
        let locked = lock(&root, "0.8.0").unwrap();
        assert!(
            locked.parts.contains(&LockedRef {
                reference: "ghcr.io/vxedge/parts/svc/ota/config/gate-store".into(),
                digest: Some(PINNED.into()),
            }),
            "{:?}",
            locked.parts
        );
        fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn a_lock_lists_every_input_but_itself_and_rewrites_identical_bytes() {
        let root = locked_workspace("lock");
        let first = lock(&root, "0.7.0").unwrap();
        assert_eq!(first.schema, LOCK_SCHEMA);
        assert_eq!(first.package.id, "vxedge/id-test");
        assert!(first.files.contains_key("vxp.yaml"));
        assert!(first.files.contains_key("main.yaml"));
        assert!(!first.files.keys().any(|f| f.starts_with("results/")));
        assert_eq!(first.files["parts/p.bin"], sha256(b"p"));
        // The published part, as the document pins it; local parts are not locked by reference.
        assert_eq!(
            first.images,
            [LockedRef {
                reference: "ghcr.io/vxedge/parts/vxprobe".to_string(),
                digest: Some(PINNED.to_string()),
            }]
        );
        assert!(first.parts.is_empty());
        write_lock(&root, &first).unwrap();
        let text = fs::read_to_string(root.join(LOCK_FILE)).unwrap();
        // Re-locking an unchanged package writes the same bytes, and the lock never lists itself.
        let second = lock(&root, "0.7.0").unwrap();
        assert_eq!(second.to_yaml(), text);
        assert!(!second.files.contains_key(LOCK_FILE));
        assert_eq!(Lock::from_yaml(&text).unwrap(), first);
        assert!(lock_problems(&root).unwrap().is_empty());
        fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn lock_drift_names_each_file_and_a_missing_lock_is_a_problem() {
        let root = locked_workspace("drift");
        assert_eq!(lock_problems(&root).unwrap().len(), 1, "no lock yet");
        write_lock(&root, &lock(&root, "0.7.0").unwrap()).unwrap();
        fs::write(root.join("parts/p.bin"), "changed").unwrap();
        fs::write(root.join("extra.txt"), "new").unwrap();
        let problems = lock_problems(&root).unwrap();
        assert!(
            problems.contains(&"`parts/p.bin` changed".to_string()),
            "{problems:?}"
        );
        assert!(
            problems.contains(&"`extra.txt` was added".to_string()),
            "{problems:?}"
        );
        fs::remove_file(root.join("extra.txt")).unwrap();
        fs::remove_file(root.join("parts/p.bin")).unwrap();
        assert_eq!(lock_problems(&root).unwrap(), ["`parts/p.bin` was removed"]);
        fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn a_lock_with_an_unknown_schema_or_key_is_refused() {
        assert!(Lock::from_yaml("schema: vxp-lock/9\npackage: {id: a/b, version: 1.0.0}\nlocked_with: x\nfiles: {}\n").is_err());
        assert!(Lock::from_yaml("schema: vxp-lock/1\npackage: {id: a/b, version: 1.0.0}\nlocked_with: x\nfiles: {}\nextra: 1\n").is_err());
    }

    #[test]
    fn the_packed_file_is_named_for_its_package() {
        let root = locked_workspace("name");
        assert_eq!(packed_file_name(&root).unwrap(), "id-test-1.0.0.vxp");
        let (descriptor, lock) = packed_descriptor_and_lock(&pack(&root).unwrap()).unwrap();
        assert_eq!(descriptor, DESCRIPTOR);
        assert_eq!(lock, None);
        fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    /// What `up` runs is what the lock says, and a run records its package digest.
    #[test]
    fn a_run_records_the_package_and_refuses_a_lock_that_does_not_match() {
        let root = locked_workspace("run");
        // No lock yet: a mismatch, unless the run is unlocked.
        match run_package(&root, false) {
            Err(RunRefusal::LockMismatch { problems }) => {
                assert!(problems[0].contains("no `vxp.lock`"), "{problems:?}")
            }
            other => panic!("{other:?}"),
        }
        let unlocked = run_package(&root, true).unwrap().unwrap();
        assert!(!unlocked.locked);

        write_lock(&root, &lock(&root, "0.7.0").unwrap()).unwrap();
        let run = run_package(&root, false).unwrap().unwrap();
        assert_eq!(
            run,
            RunPackage {
                id: "vxedge/id-test".into(),
                version: "1.0.0".into(),
                digest: package_digest(&root).unwrap(),
                locked: true,
            }
        );

        fs::write(root.join("parts/p.bin"), "edited").unwrap();
        match run_package(&root, false) {
            Err(RunRefusal::LockMismatch { problems }) => {
                assert_eq!(problems, ["`parts/p.bin` changed"])
            }
            other => panic!("{other:?}"),
        }
        let unlocked = run_package(&root, true).unwrap().unwrap();
        assert_ne!(unlocked.digest, run.digest, "the digest is of what runs");
        fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    /// A run of a `vxp/1` package always records a digest, so one that cannot be packed does not
    /// run, locked or not.
    #[cfg(unix)]
    #[test]
    fn a_package_that_cannot_be_packed_does_not_run() {
        let root = locked_workspace("run-unpackable");
        write_lock(&root, &lock(&root, "0.7.0").unwrap()).unwrap();
        std::os::unix::fs::symlink("p.bin", root.join("parts/link")).unwrap();
        for unlocked in [false, true] {
            match run_package(&root, unlocked) {
                Err(RunRefusal::Package(PkgError::NotARegularFile { path, .. })) => {
                    assert_eq!(path, "parts/link")
                }
                other => panic!("{other:?}"),
            }
        }
        fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    /// A source folder like a reference vehicle: documents, a script and a build output.
    fn vehicle_source(name: &str, vehicle_id: &str) -> PathBuf {
        let scratch = locked_workspace(&format!("{name}-src"));
        let src = scratch.parent().unwrap().join("vehicles").join(name);
        fs::create_dir_all(src.join("bundle")).unwrap();
        let vehicle = fs::read_to_string(scratch.join("vehicle.yaml"))
            .unwrap()
            .replace("id: car1", &format!("id: {vehicle_id}"));
        fs::write(src.join("vehicle.yaml"), vehicle).unwrap();
        fs::copy(scratch.join("parts.yaml"), src.join("parts.yaml")).unwrap();
        fs::write(src.join("build.sh"), "#!/bin/sh\n../other/build.sh\n").unwrap();
        fs::write(src.join("README.md"), "a vehicle\n").unwrap();
        src
    }

    #[test]
    fn vendoring_copies_documents_records_them_and_says_what_to_fix() {
        let root = locked_workspace("vendor");
        write_lock(&root, &lock(&root, "0.7.0").unwrap()).unwrap();
        let src = vehicle_source("vendor", "car2");
        let id = "vxedge/vxperiments/vehicles/car2";

        // The scaffold's own documents are in the way.
        let refused = vendor(&root, &src, id, false, "0.7.0", None)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("`parts.yaml`, `vehicle.yaml`") && refused.contains("--replace"),
            "{refused}"
        );

        let report = vendor(&root, &src, id, true, "0.7.0", None).unwrap();
        assert_eq!(
            report
                .copied
                .iter()
                .map(|v| v.file.as_str())
                .collect::<Vec<_>>(),
            ["parts.yaml", "vehicle.yaml"]
        );
        assert_eq!(
            fs::read(root.join("vehicle.yaml")).unwrap(),
            fs::read(src.join("vehicle.yaml")).unwrap()
        );
        let behind: Vec<&str> = report.left_behind.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(behind, ["README.md", "build.sh", "bundle"]);
        assert!(report.left_behind[1].1.starts_with("host recipe"));
        // `main.yaml` still names `car1`, so the package does not build, and the copies are
        // recorded anyway.
        let fix = report
            .relock_error
            .expect("main.yaml names the replaced vehicle");
        assert!(
            fix.contains("main.yaml") && fix.contains("pkg lock"),
            "{fix}"
        );
        let written = read_lock(&root).unwrap().unwrap();
        assert_eq!(written.vendored, report.copied);
        assert_eq!(written.vendored[1].source, id);

        // Fixed, a plain re-lock keeps the provenance and the lock holds.
        let main = fs::read_to_string(root.join("main.yaml"))
            .unwrap()
            .replace("[car1]", "[car2]");
        fs::write(root.join("main.yaml"), main).unwrap();
        write_lock(&root, &lock(&root, "0.7.0").unwrap()).unwrap();
        assert_eq!(read_lock(&root).unwrap().unwrap().vendored, report.copied);
        assert_eq!(lock_problems(&root).unwrap(), Vec::<String>::new());

        // Vendoring the same source again needs no `--replace`, and builds now.
        let again = vendor(&root, &src, id, false, "0.7.0", None).unwrap();
        assert_eq!(again.relock_error, None);
        fs::remove_dir_all(root.parent().unwrap()).ok();
        fs::remove_dir_all(src.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn vendor_check_catches_a_local_edit_and_drift_at_the_source() {
        let root = locked_workspace("vcheck");
        let src = vehicle_source("vcheck", "car1");
        let id = "vxedge/vxperiments/vehicles/car1";
        vendor(&root, &src, id, true, "0.7.0", None).unwrap();
        let at_src = |want: &str| -> Option<PathBuf> { (want == id).then(|| src.clone()) };
        assert_eq!(
            vendor_check(&root, &at_src).unwrap(),
            VendorCheck::default()
        );

        let nowhere = |_: &str| -> Option<PathBuf> { None };
        assert_eq!(vendor_check(&root, &nowhere).unwrap().unchecked.len(), 2);

        fs::write(
            root.join("parts.yaml"),
            format!(
                "{}# local\n",
                fs::read_to_string(root.join("parts.yaml")).unwrap()
            ),
        )
        .unwrap();
        fs::write(
            src.join("vehicle.yaml"),
            format!(
                "{}# upstream\n",
                fs::read_to_string(src.join("vehicle.yaml")).unwrap()
            ),
        )
        .unwrap();
        let found = vendor_check(&root, &at_src).unwrap().problems;
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found[0].contains("`parts.yaml` was edited in the package"),
            "{found:?}"
        );
        assert!(
            found[1].contains("`vehicle.yaml` changed at its source"),
            "{found:?}"
        );
        fs::remove_dir_all(root.parent().unwrap()).ok();
        fs::remove_dir_all(src.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn a_source_inside_the_package_or_without_documents_is_refused() {
        let root = locked_workspace("vrefuse");
        fs::create_dir_all(root.join("parts/vehicle")).unwrap();
        let inside = vendor(
            &root,
            &root.join("parts/vehicle"),
            "a/b/c",
            true,
            "0.7.0",
            None,
        )
        .unwrap_err();
        assert!(
            inside.to_string().contains("inside the package"),
            "{inside}"
        );
        let empty = root.parent().unwrap().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let none = vendor(&root, &empty, "a/b/c", true, "0.7.0", None).unwrap_err();
        assert!(none.to_string().contains("no vxedge documents"), "{none}");
        fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    /// A part folder the documents reference travels with them when git tracks it, as a vehicle's
    /// committed adapter does, and a build output the documents reference stays behind.
    #[test]
    fn vendoring_a_scenario_copies_its_road_and_storyboard() {
        let root = locked_workspace("vscen");
        let src = vehicle_source("vscen", "car1");
        fs::create_dir_all(src.join("assets")).unwrap();
        fs::write(src.join("assets/grid.xodr"), "<OpenDRIVE/>").unwrap();
        fs::write(src.join("assets/stop.xosc"), "<OpenSCENARIO/>").unwrap();
        fs::write(src.join("assets/notes.txt"), "untracked").unwrap();
        fs::write(
            src.join("scenario.yaml"),
            "kind: road\nid: grid\nnetwork: assets/grid.xodr\n---\nkind: scenario\nid: stop\n\
             road: grid\nstoryboard: ./assets/stop.xosc\nsimulator: esmini\n",
        )
        .unwrap();
        let tracked: Vec<String> = [
            "assets/grid.xodr",
            "assets/stop.xosc",
            "parts.yaml",
            "scenario.yaml",
            "vehicle.yaml",
        ]
        .map(String::from)
        .to_vec();
        let id = "vxedge/vxperiments/scenarios/stop";
        let report = vendor(&root, &src, id, true, "0.7.0", Some(&tracked)).unwrap();
        let files: Vec<&str> = report.copied.iter().map(|v| v.file.as_str()).collect();
        assert!(files.contains(&"assets/grid.xodr"), "{files:?}");
        assert!(files.contains(&"assets/stop.xosc"), "{files:?}");
        assert!(!root.join("assets/notes.txt").exists());
        assert_eq!(
            fs::read(root.join("assets/stop.xosc")).unwrap(),
            b"<OpenSCENARIO/>"
        );
    }

    #[test]
    fn vendoring_copies_the_part_folders_git_tracks() {
        let root = locked_workspace("vparts");
        let src = vehicle_source("vparts", "car1");
        fs::create_dir_all(src.join("aaos")).unwrap();
        fs::write(src.join("aaos/attach.yaml"), "interface: eth0\n").unwrap();
        fs::write(src.join("aaos/start.sh"), "#!/bin/sh\n").unwrap();
        fs::write(src.join("aaos/scratch.log"), "untracked\n").unwrap();
        fs::write(src.join("bundle/vmlinuz"), "kernel").unwrap();
        let parts = fs::read_to_string(src.join("parts.yaml")).unwrap();
        fs::write(
            src.join("parts.yaml"),
            format!(
                "{parts}  - {{ ref: ./aaos, kind: attach, target_ids: [adas] }}\n  - {{ ref: ./bundle, kind: bundle, target_ids: [adas] }}\n  - {{ ref: ./../outside, kind: bundle, target_ids: [adas] }}\n"
            ),
        )
        .unwrap();
        let tracked: Vec<String> = [
            "aaos/attach.yaml",
            "aaos/start.sh",
            "parts.yaml",
            "vehicle.yaml",
        ]
        .map(String::from)
        .to_vec();
        let id = "vxedge/vxperiments/vehicles/car1";
        let report = vendor(&root, &src, id, true, "0.7.0", Some(&tracked)).unwrap();
        let files: Vec<&str> = report.copied.iter().map(|v| v.file.as_str()).collect();
        assert_eq!(
            files,
            [
                "parts.yaml",
                "vehicle.yaml",
                "aaos/attach.yaml",
                "aaos/start.sh"
            ]
        );
        assert_eq!(
            fs::read(root.join("aaos/attach.yaml")).unwrap(),
            b"interface: eth0\n"
        );
        assert!(
            !root.join("aaos/scratch.log").exists(),
            "untracked files stay behind"
        );
        assert!(!root.join("bundle").exists());
        let bundle = report
            .left_behind
            .iter()
            .find(|(n, _)| n == "bundle")
            .unwrap();
        assert!(bundle.1.contains("build output"), "{bundle:?}");
        assert!(!report.left_behind.iter().any(|(n, _)| n == "aaos"));

        let at_src = |want: &str| -> Option<PathBuf> { (want == id).then(|| src.clone()) };
        assert_eq!(
            vendor_check(&root, &at_src).unwrap(),
            VendorCheck::default()
        );
        fs::write(src.join("aaos/start.sh"), "#!/bin/sh\necho changed\n").unwrap();
        let found = vendor_check(&root, &at_src).unwrap().problems;
        assert!(
            found[0].contains("`aaos/start.sh` changed at its source"),
            "{found:?}"
        );

        // Without a git checkout nothing tells content from build output, so no folder is copied.
        let root2 = locked_workspace("vparts-nogit");
        let report = vendor(&root2, &src, id, true, "0.7.0", None).unwrap();
        assert!(!root2.join("aaos").exists());
        let aaos = report
            .left_behind
            .iter()
            .find(|(n, _)| n == "aaos")
            .unwrap();
        assert!(aaos.1.contains("no git checkout"), "{aaos:?}");
        fs::remove_dir_all(root.parent().unwrap()).ok();
        fs::remove_dir_all(root2.parent().unwrap()).ok();
        fs::remove_dir_all(src.parent().unwrap().parent().unwrap()).ok();
    }
}
