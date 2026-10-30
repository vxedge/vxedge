//! Pure desired-state and decision logic for container groups — no I/O, no daemon, no
//! privileges, so all of it unit-tests anywhere (`rust-patterns`: plans are pure data).
//! [`super::client`] is the thin impure edge that executes what this module decides.
//!
//! A **group** is the shape bollard has no word for: Podman's "pod", made explicit. One infra
//! container owns a network namespace; every member joins it with
//! `NetworkMode: container:<infra_id>`. Membership is recorded in labels, never inferred from
//! names: `vx5g` is a prefix of `vx5g-ran`, so a name-prefix filter for the CN group would also
//! match every RAN container, and `down` on one would tear down the other.

use std::collections::BTreeMap;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// Label carrying the group a container belongs to. Exact-match filtered (see module doc).
pub const LABEL_GROUP: &str = "io.vxedge.group";
/// Label carrying the member suffix (`infra` for the netns owner).
pub const LABEL_MEMBER: &str = "io.vxedge.member";
/// Label carrying [`spec_hash`] of what the container was created from, so `up` can tell a
/// container that matches the manifest from one left behind by an older spec.
pub const LABEL_SPEC: &str = "io.vxedge.spec-sha256";

/// Member suffix reserved for the infra container.
pub const INFRA_MEMBER: &str = "infra";

/// Network mode of a group's infra container: `pasta`, which is what `podman pod create` gives a
/// rootless pod's infra container, so a group's namespace starts out like the pod it replaces
/// (loopback plus pasta's host-facing interface, whose default route the UE-netns `0.0.0.0` bug
/// in `open5gs-ueransim-patterns` is about). Left unset, the Docker-compatible API defaults to
/// `bridge` instead: a different namespace layout, joined to a rootless netns whose shared pasta
/// process Podman must signal on teardown, which Ubuntu's AppArmor `pasta` profile refuses when
/// the request comes through the API service (found live 2026-10-28: every `group_down` failed
/// with "rootless netns: kill network process: permission denied").
pub const INFRA_NETWORK_MODE: &str = "pasta";

/// The `NetworkMode` a group container is created with: [`INFRA_NETWORK_MODE`] for the infra
/// container (`netns_owner` is `None`), `container:<infra id>` for a member.
pub fn network_mode(netns_owner: Option<&str>) -> String {
    match netns_owner {
        Some(id) => format!("container:{id}"),
        None => INFRA_NETWORK_MODE.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid container name `{name}`: {reason}")]
pub struct NameError {
    pub name: String,
    pub reason: &'static str,
}

/// Podman's own rule for container names (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`), checked here so a bad
/// name fails at planning with the name in the message instead of as an API 500 mid-`up`.
fn check_name(name: &str) -> Result<(), NameError> {
    let mut chars = name.chars();
    match chars.next() {
        None => {
            return Err(NameError {
                name: name.to_string(),
                reason: "empty",
            });
        }
        Some(c) if !c.is_ascii_alphanumeric() => {
            return Err(NameError {
                name: name.to_string(),
                reason: "must start with a letter or digit",
            });
        }
        Some(_) => {}
    }
    if chars.any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))) {
        return Err(NameError {
            name: name.to_string(),
            reason: "only letters, digits, `_`, `.` and `-` are allowed",
        });
    }
    Ok(())
}

/// A host path mounted into a container. Typed rather than a `-v` string so read-only and
/// SELinux relabelling are visible in the spec (and in its hash), not buried in flag text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Bind {
    pub host: PathBuf,
    pub container: String,
    pub read_only: bool,
    /// `:Z` — private SELinux relabel. Harmless on hosts without SELinux.
    pub relabel: bool,
}

impl Bind {
    /// The Docker-API `Binds` entry (`host:container[:opts]`).
    pub fn to_bind_string(&self) -> String {
        let mut opts: Vec<&str> = Vec::new();
        if self.read_only {
            opts.push("ro");
        }
        if self.relabel {
            opts.push("Z");
        }
        let base = format!("{}:{}", self.host.display(), self.container);
        if opts.is_empty() {
            base
        } else {
            format!("{base}:{}", opts.join(","))
        }
    }
}

/// One container to run. Every field is typed: there is no "extra flags" escape hatch, because
/// a flag string is exactly the CLI text this module exists to stop producing.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct ContainerSpec {
    /// Member suffix; the container is named `<group>-<name>` (the `podman pod` naming the 5G
    /// core already uses, so migrating it does not rename anything an operator looks for).
    pub name: String,
    pub image: String,
    /// Command + args. Empty means the image's own default command.
    pub cmd: Vec<String>,
    /// Ordered, so the spec hash is deterministic for a given manifest.
    pub env: BTreeMap<String, String>,
    pub binds: Vec<Bind>,
    pub cap_add: Vec<String>,
    /// Host device paths passed through at the same path (`/dev/net/tun`).
    pub devices: Vec<String>,
}

/// A validated group: a name every member is prefixed with, and the infra container that owns
/// the shared network namespace.
///
/// The infra image is the caller's choice, not a default here: a reproducible testbed pins it
/// by digest (§13), and this module has no business inventing which digest that is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSpec {
    name: String,
    infra: ContainerSpec,
}

impl GroupSpec {
    /// `infra.name` is ignored and forced to [`INFRA_MEMBER`]; its `cmd` must keep the
    /// container alive (a pause binary, or `sleep infinity`), since the namespace dies with it.
    pub fn new(name: impl Into<String>, infra: ContainerSpec) -> Result<Self, NameError> {
        let name = name.into();
        check_name(&name)?;
        Ok(Self {
            infra: ContainerSpec {
                name: INFRA_MEMBER.to_string(),
                ..infra
            },
            name,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn infra(&self) -> &ContainerSpec {
        &self.infra
    }

    pub fn infra_container_name(&self) -> String {
        member_name(&self.name, INFRA_MEMBER)
    }

    /// Full container name for a member, refusing the reserved infra suffix and anything Podman
    /// would reject.
    pub fn member_container_name(&self, spec: &ContainerSpec) -> Result<String, NameError> {
        if spec.name == INFRA_MEMBER {
            return Err(NameError {
                name: spec.name.clone(),
                reason: "`infra` is reserved for the group's network-namespace owner",
            });
        }
        let full = member_name(&self.name, &spec.name);
        check_name(&full)?;
        Ok(full)
    }

    /// Labels for a container in this group. `netns_owner` is the infra container id for a
    /// member, `None` for the infra container itself; it is part of the hash, because a member
    /// joined to a since-replaced infra container sits in a namespace nothing else shares.
    pub fn labels(
        &self,
        spec: &ContainerSpec,
        netns_owner: Option<&str>,
    ) -> BTreeMap<String, String> {
        BTreeMap::from([
            (LABEL_GROUP.to_string(), self.name.clone()),
            (LABEL_MEMBER.to_string(), spec.name.clone()),
            (LABEL_SPEC.to_string(), spec_hash(spec, netns_owner)),
        ])
    }
}

pub fn member_name(group: &str, member: &str) -> String {
    format!("{group}-{member}")
}

/// Stable hash of everything a container was created from, its [`network_mode`] included. Two
/// specs that would create different containers hash differently; the same spec hashes the same
/// on every run.
pub fn spec_hash(spec: &ContainerSpec, netns_owner: Option<&str>) -> String {
    #[derive(serde::Serialize)]
    struct Hashed<'a> {
        spec: &'a ContainerSpec,
        network_mode: String,
    }
    let bytes = serde_json::to_vec(&Hashed {
        spec,
        network_mode: network_mode(netns_owner),
    })
    .expect("invariant: ContainerSpec is plain strings, maps and bools, always serializable");
    let digest = Sha256::digest(&bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// One container as the daemon reports it — the observed half of the reconciler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedContainer {
    pub id: String,
    pub name: String,
    /// [`LABEL_MEMBER`]; `None` for a container carrying the group label but not this one,
    /// which only happens if something other than this module created it.
    pub member: Option<String>,
    pub spec_hash: Option<String>,
    /// Daemon state word (`running`, `created`, `exited`, …), kept verbatim for status output.
    pub state: String,
}

impl ObservedContainer {
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }
}

/// Observed state of one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStatus {
    pub group: String,
    pub infra: Option<ObservedContainer>,
    /// Every non-infra container labelled with this group, sorted by name.
    pub members: Vec<ObservedContainer>,
}

impl GroupStatus {
    pub fn from_observed(group: &str, observed: Vec<ObservedContainer>) -> Self {
        let mut infra = None;
        let mut members = Vec::new();
        for c in observed {
            if c.member.as_deref() == Some(INFRA_MEMBER) {
                infra = Some(c);
            } else {
                members.push(c);
            }
        }
        members.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            group: group.to_string(),
            infra,
            members,
        }
    }

    /// Anything of this group exists, in any state — `podman pod exists`' meaning, which is
    /// what `up`'s idempotency check and `down`'s fast path ask.
    pub fn exists(&self) -> bool {
        self.infra.is_some() || !self.members.is_empty()
    }

    /// The infra container and at least one member are running, and no member is not.
    pub fn all_running(&self) -> bool {
        self.infra
            .as_ref()
            .is_some_and(ObservedContainer::is_running)
            && !self.members.is_empty()
            && self.members.iter().all(ObservedContainer::is_running)
    }

    pub fn member(&self, container_name: &str) -> Option<&ObservedContainer> {
        self.members.iter().find(|c| c.name == container_name)
    }
}

/// What `up` does to the infra container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfraAction {
    /// Running and created from this spec: leave it, and its namespace, alone.
    Keep,
    /// Absent, stale, or stopped: remove whatever of the group exists, then create afresh.
    ///
    /// A stopped infra container is not restarted in place. Restarting it creates a new
    /// network namespace, and every member already joined to it would keep the old one — a
    /// group that `status` reports as up while its members share nothing.
    Rebuild,
}

pub fn infra_action(group: &GroupSpec, observed: Option<&ObservedContainer>) -> InfraAction {
    match observed {
        Some(c)
            if c.is_running()
                && c.spec_hash.as_deref() == Some(spec_hash(group.infra(), None).as_str()) =>
        {
            InfraAction::Keep
        }
        _ => InfraAction::Rebuild,
    }
}

/// What `up` does to one member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberAction {
    Keep,
    /// Created from this spec but not running (exited, or created and never started).
    Start,
    Create,
    /// Exists but from a different spec or a different infra container: remove, then create.
    Recreate,
}

pub fn member_action(
    spec: &ContainerSpec,
    infra_id: &str,
    observed: Option<&ObservedContainer>,
) -> MemberAction {
    let Some(c) = observed else {
        return MemberAction::Create;
    };
    if c.spec_hash.as_deref() != Some(spec_hash(spec, Some(infra_id)).as_str()) {
        MemberAction::Recreate
    } else if c.is_running() {
        MemberAction::Keep
    } else {
        MemberAction::Start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pause() -> ContainerSpec {
        ContainerSpec {
            image: "example.invalid/pause@sha256:00".into(),
            cmd: vec!["sleep".into(), "infinity".into()],
            ..Default::default()
        }
    }

    fn nrf() -> ContainerSpec {
        ContainerSpec {
            name: "nrf".into(),
            image: "vx5g-core".into(),
            cmd: vec!["open5gs-nrfd".into()],
            ..Default::default()
        }
    }

    fn group() -> GroupSpec {
        GroupSpec::new("vx5g", pause()).unwrap()
    }

    fn observed(member: &str, hash: Option<String>, state: &str) -> ObservedContainer {
        ObservedContainer {
            id: format!("id-{member}"),
            name: member_name("vx5g", member),
            member: Some(member.into()),
            spec_hash: hash,
            state: state.into(),
        }
    }

    #[test]
    fn names_follow_the_pod_convention_the_5g_core_already_uses() {
        let g = group();
        assert_eq!(g.infra_container_name(), "vx5g-infra");
        assert_eq!(g.member_container_name(&nrf()).unwrap(), "vx5g-nrf");
        assert_eq!(
            g.infra().name,
            INFRA_MEMBER,
            "infra name is forced, not trusted"
        );
    }

    #[test]
    fn a_member_cannot_claim_the_infra_suffix() {
        let spec = ContainerSpec {
            name: "infra".into(),
            ..nrf()
        };
        assert!(group().member_container_name(&spec).is_err());
    }

    #[test]
    fn names_podman_would_reject_fail_at_planning() {
        assert!(GroupSpec::new("", pause()).is_err());
        assert!(GroupSpec::new("-vx5g", pause()).is_err());
        assert!(GroupSpec::new("vx5g/ran", pause()).is_err());
        let spaced = ContainerSpec {
            name: "smf adas".into(),
            ..nrf()
        };
        assert!(group().member_container_name(&spaced).is_err());
    }

    #[test]
    fn bind_strings_carry_their_options_in_order() {
        let b = Bind {
            host: "/run/vx/fiveg-config".into(),
            container: "/etc/vx5g".into(),
            read_only: true,
            relabel: true,
        };
        // The exact string `fiveg::lifecycle` passes to `podman run -v` today.
        assert_eq!(b.to_bind_string(), "/run/vx/fiveg-config:/etc/vx5g:ro,Z");
        let plain = Bind {
            read_only: false,
            relabel: false,
            ..b
        };
        assert_eq!(plain.to_bind_string(), "/run/vx/fiveg-config:/etc/vx5g");
    }

    #[test]
    fn spec_hash_is_stable_and_sensitive_to_every_input() {
        let base = spec_hash(&nrf(), Some("infra-1"));
        assert_eq!(base, spec_hash(&nrf(), Some("infra-1")), "deterministic");
        assert_eq!(base.len(), 64);
        let other_image = ContainerSpec {
            image: "vx5g-core:next".into(),
            ..nrf()
        };
        assert_ne!(base, spec_hash(&other_image, Some("infra-1")));
        assert_ne!(
            base,
            spec_hash(&nrf(), Some("infra-2")),
            "netns owner counts"
        );
        let with_env = ContainerSpec {
            env: BTreeMap::from([("A".into(), "1".into())]),
            ..nrf()
        };
        assert_ne!(base, spec_hash(&with_env, Some("infra-1")));
    }

    #[test]
    fn infra_gets_the_pod_network_mode_and_members_join_it() {
        assert_eq!(network_mode(None), "pasta");
        assert_eq!(network_mode(Some("abc")), "container:abc");
    }

    #[test]
    fn labels_record_group_member_and_hash() {
        let l = group().labels(&nrf(), Some("infra-1"));
        assert_eq!(l[LABEL_GROUP], "vx5g");
        assert_eq!(l[LABEL_MEMBER], "nrf");
        assert_eq!(l[LABEL_SPEC], spec_hash(&nrf(), Some("infra-1")));
    }

    #[test]
    fn infra_is_kept_only_when_running_from_the_same_spec() {
        let g = group();
        let good = spec_hash(g.infra(), None);
        assert_eq!(infra_action(&g, None), InfraAction::Rebuild);
        assert_eq!(
            infra_action(&g, Some(&observed("infra", Some(good.clone()), "running"))),
            InfraAction::Keep
        );
        // Stopped infra is rebuilt, never restarted: a restart is a new namespace.
        assert_eq!(
            infra_action(&g, Some(&observed("infra", Some(good), "exited"))),
            InfraAction::Rebuild
        );
        assert_eq!(
            infra_action(
                &g,
                Some(&observed("infra", Some("stale".into()), "running"))
            ),
            InfraAction::Rebuild
        );
    }

    /// The re-run path is where every idempotency bug in this project has lived, so each arm of
    /// the second `up` is pinned here rather than left to the live test.
    #[test]
    fn member_actions_converge_on_the_second_up() {
        let spec = nrf();
        let good = spec_hash(&spec, Some("infra-1"));
        assert_eq!(member_action(&spec, "infra-1", None), MemberAction::Create);
        assert_eq!(
            member_action(
                &spec,
                "infra-1",
                Some(&observed("nrf", Some(good.clone()), "running"))
            ),
            MemberAction::Keep
        );
        assert_eq!(
            member_action(
                &spec,
                "infra-1",
                Some(&observed("nrf", Some(good.clone()), "exited"))
            ),
            MemberAction::Start
        );
        assert_eq!(
            member_action(
                &spec,
                "infra-2",
                Some(&observed("nrf", Some(good), "running"))
            ),
            MemberAction::Recreate,
            "joined to a replaced infra container"
        );
        assert_eq!(
            member_action(&spec, "infra-1", Some(&observed("nrf", None, "running"))),
            MemberAction::Recreate,
            "not created by this module"
        );
    }

    #[test]
    fn group_status_splits_infra_from_members_and_sorts() {
        let s = GroupStatus::from_observed(
            "vx5g",
            vec![
                observed("upf-adas", None, "running"),
                observed("infra", None, "running"),
                observed("amf", None, "running"),
            ],
        );
        assert!(s.exists());
        assert_eq!(s.infra.as_ref().unwrap().name, "vx5g-infra");
        let names: Vec<&str> = s.members.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["vx5g-amf", "vx5g-upf-adas"]);
        assert!(s.all_running());
        assert!(s.member("vx5g-amf").is_some());
    }

    #[test]
    fn all_running_needs_infra_and_at_least_one_running_member() {
        let empty = GroupStatus::from_observed("vx5g", vec![]);
        assert!(!empty.exists());
        assert!(!empty.all_running());
        let infra_only =
            GroupStatus::from_observed("vx5g", vec![observed("infra", None, "running")]);
        assert!(infra_only.exists());
        assert!(
            !infra_only.all_running(),
            "a pod with nothing in it is not up"
        );
        let one_down = GroupStatus::from_observed(
            "vx5g",
            vec![
                observed("infra", None, "running"),
                observed("nrf", None, "exited"),
            ],
        );
        assert!(!one_down.all_running());
    }
}
