//! Experiment-workspace model and validator (Track D, `docs/TASKS.md`).
//!
//! A workspace is a folder of YAML files. Each declares its `kind` and `id`; the loader
//! indexes on that pair and every cross-reference is by id, never by path — so files can be
//! renamed or reorganised freely.
//!
//! **Additive on purpose.** This lives alongside [`crate::manifest`] and changes nothing about
//! it: the ARCHITECTURE.md §6 rewrite that would *replace* the existing schema is a breaking
//! change still awaiting sign-off. Every current experiment keeps parsing through the old path.
//! What this module buys before that sign-off is the thing prose review cannot give — the
//! cross-file joins actually executed, so §6 can be written from something proven.
//!
//! House layering: [`Workspace::load`] does the I/O, [`Workspace::validate`] is pure and
//! returns *all* problems rather than the first, because an engineer fixing a workspace wants
//! the whole list.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("reading `{}`: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("`{}`: {source}", .path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml_ng::Error,
    },
    #[error(
        "workspace validation failed:\n{}",
        .0.iter().map(|e| format!("  - {e}")).collect::<Vec<_>>().join("\n")
    )]
    Invalid(Vec<WorkspaceValidationError>),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceValidationError {
    #[error("no `kind: experiment` file found — a workspace needs exactly one")]
    NoExperiment,
    #[error(
        "{count} `kind: experiment` files found ({ids}) — one experiment per workspace for \
         now; put the other in its own workspace"
    )]
    MultipleExperiments { count: usize, ids: String },
    #[error("duplicate id `{id}` used by both a `{first}` and a `{second}`")]
    DuplicateId {
        id: String,
        first: String,
        second: String,
    },
    #[error("duplicate component id `{id}` — component ids must be unique across the workspace")]
    DuplicateComponentId { id: String },
    #[error("experiment `{experiment}` references unknown vehicle `{vehicle}`")]
    UnknownVehicle { experiment: String, vehicle: String },
    #[error("experiment `{experiment}` references unknown baseline `{baseline}`")]
    UnknownBaseline {
        experiment: String,
        baseline: String,
    },
    #[error(
        "component `{component}` (vehicle `{vehicle}`) has no part in any baseline — it would \
         boot nothing. Add a `parts` entry with `target_ids: {component}`."
    )]
    ComponentWithoutPart { vehicle: String, component: String },
    #[error("baseline `{baseline}`: part `{part}` targets unknown component `{target}`")]
    UnknownTarget {
        baseline: String,
        part: String,
        target: String,
    },
    #[error(
        "vehicle `{vehicle}`: link `{link}` endpoint `{endpoint}` names unknown component `{component}`"
    )]
    UnknownLinkEndpoint {
        vehicle: String,
        link: String,
        endpoint: String,
        component: String,
    },
    #[error(
        "vehicle `{vehicle}`: channel `{channel}` endpoint `{endpoint}` names unknown component `{component}`"
    )]
    UnknownChannelEndpoint {
        vehicle: String,
        channel: String,
        endpoint: String,
        component: String,
    },
    #[error("vehicle `{vehicle}`: channel `{channel}` is carried_by unknown link `{link}`")]
    UnknownCarriedByLink {
        vehicle: String,
        channel: String,
        link: String,
    },
    #[error(
        "vehicle `{vehicle}`: channel `{channel}` cannot reach `{unreachable}` over its \
         carried_by links — either the link list is incomplete or the channel is wired to a \
         component it has no path to"
    )]
    ChannelNotCarried {
        vehicle: String,
        channel: String,
        unreachable: String,
    },
    #[error(
        "vehicle `{vehicle}`: link `{link}` binds slice `{slice}`, which no experiment in this \
         workspace defines under `network.slices` — the vehicle names a slice, the experiment \
         must say what it is"
    )]
    UndefinedSlice {
        vehicle: String,
        link: String,
        slice: String,
    },
    #[error(
        "baseline `{baseline}`: `{part}` is a published ref but has no `digest` — a moving tag \
         is not reproducible for anyone else. Pin it, or use a local path while iterating."
    )]
    PublishedRefWithoutDigest { baseline: String, part: String },
    #[error(
        "baseline `{baseline}`: `{part}` has digest `{digest}` — expected `sha256:` + 64 hex chars"
    )]
    MalformedDigest {
        baseline: String,
        part: String,
        digest: String,
    },
    #[error(
        "experiment `{experiment}`: traffic profile `{profile}` has `{field}: {endpoint}`, which \
         is neither a component in this workspace nor one of the symbolic endpoints \
         ({SYMBOLIC_ENDPOINTS})"
    )]
    UnknownTrafficEndpoint {
        experiment: String,
        profile: String,
        field: &'static str,
        endpoint: String,
    },
}

/// Traffic endpoints that name no component. `internet` is the host-side reflector on the far
/// side of the gateway; `carla` is the simulator. Kept as a closed list so a typo'd component id
/// is an error at authoring time rather than a profile that silently measures nothing.
const SYMBOLIC_ENDPOINTS: &str = "internet, carla";

fn is_symbolic_endpoint(ep: &str) -> bool {
    SYMBOLIC_ENDPOINTS.split(", ").any(|s| s == ep)
}

/// Accepts either a scalar or a sequence, so `target_ids: hpc1` and `target_ids: [a, b]` both
/// work — the schema should not make a single target more verbose than it needs to be.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub fn as_slice(&self) -> Vec<&str> {
        match self {
            OneOrMany::One(s) => vec![s.as_str()],
            OneOrMany::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Experiment,
    Vehicle,
    Road,
    Baseline,
    Component,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Experiment => "experiment",
            Kind::Vehicle => "vehicle",
            Kind::Road => "road",
            Kind::Baseline => "baseline",
            Kind::Component => "component",
        }
    }
}

/// Just enough of a document to route it. Read first so an unknown or malformed `kind`
/// produces one clear error instead of a confusing failure against the wrong struct.
#[derive(Debug, Deserialize)]
struct Header {
    kind: Kind,
    id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentDoc {
    pub id: String,
    #[serde(default)]
    pub vehicles: Vec<String>,
    #[serde(default)]
    pub baseline: Option<String>,
    #[serde(default)]
    pub network: Option<NetworkDoc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkDoc {
    #[serde(default)]
    pub slices: Vec<SliceDoc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceDoc {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VehicleDoc {
    pub id: String,
    #[serde(default)]
    pub composition: Composition,
    #[serde(default)]
    pub links: Vec<LinkDoc>,
    #[serde(default)]
    pub channels: Vec<ChannelDoc>,
}

/// Fixed groups rather than one `components:` map: it reads better and validates more sharply,
/// and widening later is additive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Composition {
    #[serde(default)]
    pub zones: BTreeMap<String, serde_yaml_ng::Value>,
    #[serde(default)]
    pub compute: BTreeMap<String, serde_yaml_ng::Value>,
    #[serde(default)]
    pub gateway: BTreeMap<String, serde_yaml_ng::Value>,
}

impl Composition {
    /// Every component id declared in this vehicle, in declaration groups.
    pub fn ids(&self) -> Vec<&str> {
        self.zones
            .keys()
            .chain(self.compute.keys())
            .chain(self.gateway.keys())
            .map(String::as_str)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkDoc {
    pub id: String,
    #[serde(default)]
    pub endpoints: Vec<String>,
    #[serde(default)]
    pub slice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDoc {
    pub id: String,
    pub from: String,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub carried_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineDoc {
    pub id: String,
    #[serde(default)]
    pub parts: Vec<PartEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartEntry {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub target_ids: Option<OneOrMany>,
}

impl PartEntry {
    /// A ref is local when it names something on this machine rather than in a registry.
    /// Everything else must be digest-pinned — see
    /// [`WorkspaceValidationError::PublishedRefWithoutDigest`].
    ///
    /// Three forms count as local: a relative path, an absolute path, and the **`local/`
    /// namespace** the existing manifests use for a node that runs as a host process
    /// (`local/vxprobe:host-process` in exp-001). The last one was found by migrating exp-001:
    /// a container-backed node ignores `image.url` entirely, so `local/…` names no artifact at
    /// all — demanding a digest for it would mean inventing one for a thing that does not
    /// exist, which is worse than not checking.
    pub fn is_local(&self) -> bool {
        self.reference.starts_with('.')
            || self.reference.starts_with('/')
            || self.reference.starts_with("local/")
    }
}

/// A loaded workspace. Loading proves the files parse; it does not prove they agree —
/// [`Workspace::validate`] does that.
#[derive(Debug, Clone, Default)]
pub struct Workspace {
    pub root: PathBuf,
    pub experiments: Vec<ExperimentDoc>,
    /// Raw YAML of each experiment document, in the same order as `experiments`.
    /// `network`, `traffic_profiles`, `telemetry` and `results` keep their §6.1 shape
    /// (ARCHITECTURE.md §6.0.3), so lowering passes them through untouched rather than
    /// modelling them twice and risking the two definitions drifting.
    pub experiments_raw: Vec<serde_yaml_ng::Value>,
    pub vehicles: Vec<VehicleDoc>,
    pub baselines: Vec<BaselineDoc>,
    pub roads: Vec<String>,
    pub components: Vec<String>,
}

/// `component:port` → the component half. A bare id has no port and is returned as-is.
fn component_of(endpoint: &str) -> &str {
    endpoint.split(':').next().unwrap_or(endpoint)
}

impl Workspace {
    /// Read every `*.yaml` in `root` (non-recursive) and route it by `kind`. The only I/O here.
    ///
    /// Non-recursive on purpose: a workspace is a flat folder, and recursing would silently
    /// absorb `results/` archives or a vendored directory into the experiment.
    pub fn load(root: &Path) -> Result<Self, WorkspaceError> {
        let mut ws = Workspace {
            root: root.to_path_buf(),
            ..Default::default()
        };

        let entries = std::fs::read_dir(root).map_err(|source| WorkspaceError::Io {
            path: root.to_path_buf(),
            source,
        })?;

        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| WorkspaceError::Io {
                path: root.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
                paths.push(path);
            }
        }
        // Sorted so error output is stable run to run — an unstable error list is miserable
        // to diff in CI.
        paths.sort();

        for path in paths {
            let text = std::fs::read_to_string(&path).map_err(|source| WorkspaceError::Io {
                path: path.clone(),
                source,
            })?;
            let header: Header =
                serde_yaml_ng::from_str(&text).map_err(|source| WorkspaceError::Parse {
                    path: path.clone(),
                    source,
                })?;
            let parse_err = |source| WorkspaceError::Parse {
                path: path.clone(),
                source,
            };
            match header.kind {
                Kind::Experiment => {
                    ws.experiments
                        .push(serde_yaml_ng::from_str(&text).map_err(parse_err)?);
                    ws.experiments_raw
                        .push(serde_yaml_ng::from_str(&text).map_err(parse_err)?);
                }
                Kind::Vehicle => ws
                    .vehicles
                    .push(serde_yaml_ng::from_str(&text).map_err(parse_err)?),
                Kind::Baseline => ws
                    .baselines
                    .push(serde_yaml_ng::from_str(&text).map_err(parse_err)?),
                Kind::Road => ws.roads.push(header.id),
                Kind::Component => ws.components.push(header.id),
            }
        }

        Ok(ws)
    }

    /// Every cross-file check, returning all problems rather than the first.
    pub fn validate(&self) -> Vec<WorkspaceValidationError> {
        use WorkspaceValidationError as E;
        let mut errors = Vec::new();

        match self.experiments.len() {
            0 => errors.push(E::NoExperiment),
            1 => {}
            n => errors.push(E::MultipleExperiments {
                count: n,
                ids: self
                    .experiments
                    .iter()
                    .map(|e| e.id.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        }

        // Document ids are unique across kinds: the loader indexes on (kind, id), but two
        // documents sharing an id make every human reference ambiguous.
        let mut doc_ids: BTreeMap<String, Kind> = BTreeMap::new();
        let mut note = |id: &str, kind: Kind, errors: &mut Vec<E>| {
            if let Some(first) = doc_ids.get(id) {
                errors.push(E::DuplicateId {
                    id: id.to_string(),
                    first: first.as_str().to_string(),
                    second: kind.as_str().to_string(),
                });
            } else {
                doc_ids.insert(id.to_string(), kind);
            }
        };
        for d in &self.experiments {
            note(&d.id, Kind::Experiment, &mut errors);
        }
        for d in &self.vehicles {
            note(&d.id, Kind::Vehicle, &mut errors);
        }
        for d in &self.baselines {
            note(&d.id, Kind::Baseline, &mut errors);
        }
        for id in &self.roads {
            note(id, Kind::Road, &mut errors);
        }

        // Component ids must be unique workspace-wide: `target_ids` is flat, so an id repeated
        // across two vehicles would make a part's destination ambiguous. This is the cost of
        // choosing flat ids over structural paths, and it has to be an actual check.
        let mut component_owner: BTreeMap<String, String> = BTreeMap::new();
        for v in &self.vehicles {
            for id in v.composition.ids() {
                if component_owner
                    .insert(id.to_string(), v.id.clone())
                    .is_some()
                {
                    errors.push(E::DuplicateComponentId { id: id.to_string() });
                }
            }
        }

        let vehicle_ids: BTreeSet<&str> = self.vehicles.iter().map(|v| v.id.as_str()).collect();
        let baseline_ids: BTreeSet<&str> = self.baselines.iter().map(|b| b.id.as_str()).collect();

        // Slices defined by any experiment in the workspace.
        let defined_slices: BTreeSet<&str> = self
            .experiments
            .iter()
            .filter_map(|e| e.network.as_ref())
            .flat_map(|n| n.slices.iter().map(|s| s.id.as_str()))
            .collect();

        for e in &self.experiments {
            for v in &e.vehicles {
                if !vehicle_ids.contains(v.as_str()) {
                    errors.push(E::UnknownVehicle {
                        experiment: e.id.clone(),
                        vehicle: v.clone(),
                    });
                }
            }
            if let Some(b) = &e.baseline
                && !baseline_ids.contains(b.as_str())
            {
                errors.push(E::UnknownBaseline {
                    experiment: e.id.clone(),
                    baseline: b.clone(),
                });
            }
        }

        for v in &self.vehicles {
            let own: BTreeSet<&str> = v.composition.ids().into_iter().collect();
            let link_ids: BTreeSet<&str> = v.links.iter().map(|l| l.id.as_str()).collect();

            for l in &v.links {
                for ep in &l.endpoints {
                    let c = component_of(ep);
                    if !own.contains(c) {
                        errors.push(E::UnknownLinkEndpoint {
                            vehicle: v.id.clone(),
                            link: l.id.clone(),
                            endpoint: ep.clone(),
                            component: c.to_string(),
                        });
                    }
                }
                // The cross-file join: the vehicle names a slice, an experiment must define it.
                if let Some(slice) = &l.slice
                    && !defined_slices.contains(slice.as_str())
                {
                    errors.push(E::UndefinedSlice {
                        vehicle: v.id.clone(),
                        link: l.id.clone(),
                        slice: slice.clone(),
                    });
                }
            }

            for ch in &v.channels {
                let mut endpoints = vec![&ch.from];
                endpoints.extend(ch.to.iter());
                for ep in &endpoints {
                    let c = component_of(ep);
                    if !own.contains(c) {
                        errors.push(E::UnknownChannelEndpoint {
                            vehicle: v.id.clone(),
                            channel: ch.id.clone(),
                            endpoint: (*ep).clone(),
                            component: c.to_string(),
                        });
                    }
                }

                for l in &ch.carried_by {
                    if !link_ids.contains(l.as_str()) {
                        errors.push(E::UnknownCarriedByLink {
                            vehicle: v.id.clone(),
                            channel: ch.id.clone(),
                            link: l.clone(),
                        });
                    }
                }

                // Reachability: a channel's endpoints must be connected by the links it says
                // carry it. This is the check that makes `carried_by` mean something — without
                // it, a channel could claim any link list and per-topic-per-slice attribution
                // would be built on a lie.
                if !ch.carried_by.is_empty() {
                    let mut reachable: BTreeSet<&str> = BTreeSet::new();
                    reachable.insert(component_of(&ch.from));
                    // Small graphs; iterate to a fixed point rather than build a real DSU.
                    loop {
                        let before = reachable.len();
                        for l in v.links.iter().filter(|l| ch.carried_by.contains(&l.id)) {
                            let comps: Vec<&str> =
                                l.endpoints.iter().map(|e| component_of(e)).collect();
                            if comps.iter().any(|c| reachable.contains(c)) {
                                for c in comps {
                                    reachable.insert(c);
                                }
                            }
                        }
                        if reachable.len() == before {
                            break;
                        }
                    }
                    for ep in &ch.to {
                        let c = component_of(ep);
                        if own.contains(c) && !reachable.contains(c) {
                            errors.push(E::ChannelNotCarried {
                                vehicle: v.id.clone(),
                                channel: ch.id.clone(),
                                unreachable: c.to_string(),
                            });
                        }
                    }
                }
            }
        }

        // Every component must be given software by some baseline. Found by the lowering
        // (2026-07-30) rather than by review: a gateway with no part validated cleanly and
        // then could not be lowered to anything runnable, because "boots nothing" is not a
        // shape error — it is a completeness one.
        if !self.baselines.is_empty() {
            let targeted: BTreeSet<&str> = self
                .baselines
                .iter()
                .flat_map(|b| b.parts.iter())
                .filter_map(|p| p.target_ids.as_ref())
                .flat_map(|t| t.as_slice())
                .collect();
            for v in &self.vehicles {
                for c in v.composition.ids() {
                    if !targeted.contains(c) {
                        errors.push(E::ComponentWithoutPart {
                            vehicle: v.id.clone(),
                            component: c.to_string(),
                        });
                    }
                }
            }
        }

        // Traffic endpoints. `traffic_profiles` keeps its §6.1 shape, so it is read from the raw
        // document rather than modelled twice — but its *references* are §6.0 component ids, and
        // an unresolvable one has to be caught here: §6.1 only rejects a profile when *both*
        // sides fail to resolve, so a single typo would otherwise run and measure the wrong thing.
        for (e, raw) in self.experiments.iter().zip(self.experiments_raw.iter()) {
            let Some(profiles) = raw.get("traffic_profiles").and_then(|v| v.as_mapping()) else {
                continue;
            };
            for (name, profile) in profiles {
                let profile_id = name.as_str().unwrap_or("<unnamed>").to_string();
                for field in ["source", "target"] {
                    let Some(ep) = profile.get(field).and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if !component_owner.contains_key(ep) && !is_symbolic_endpoint(ep) {
                        errors.push(E::UnknownTrafficEndpoint {
                            experiment: e.id.clone(),
                            profile: profile_id.clone(),
                            field,
                            endpoint: ep.to_string(),
                        });
                    }
                }
            }
        }

        for b in &self.baselines {
            for p in &b.parts {
                if let Some(targets) = &p.target_ids {
                    for t in targets.as_slice() {
                        if !component_owner.contains_key(t) {
                            errors.push(E::UnknownTarget {
                                baseline: b.id.clone(),
                                part: p.reference.clone(),
                                target: t.to_string(),
                            });
                        }
                    }
                }
                match (&p.digest, p.is_local()) {
                    (None, false) => errors.push(E::PublishedRefWithoutDigest {
                        baseline: b.id.clone(),
                        part: p.reference.clone(),
                    }),
                    (Some(d), _) if !is_sha256(d) => errors.push(E::MalformedDigest {
                        baseline: b.id.clone(),
                        part: p.reference.clone(),
                        digest: d.clone(),
                    }),
                    _ => {}
                }
            }
        }

        errors
    }

    /// Load and validate in one step, the shape a CLI wants.
    pub fn load_validated(root: &Path) -> Result<Self, WorkspaceError> {
        let ws = Workspace::load(root)?;
        let errors = ws.validate();
        if errors.is_empty() {
            Ok(ws)
        } else {
            Err(WorkspaceError::Invalid(errors))
        }
    }
}

fn is_sha256(d: &str) -> bool {
    d.strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Writes a throwaway workspace. Tests own hand-written YAML rather than reusing the
    /// scaffold's output: validating only generated files would be grading our own homework,
    /// and every check below exists for a mistake a human makes by hand.
    fn workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("vxws-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for (n, c) in files {
            fs::write(root.join(n), c).unwrap();
        }
        root
    }

    const GOOD_EXPERIMENT: &str = r#"
kind: experiment
id: exp1
vehicles: [car1]
baseline: bl1
network:
  slices:
    - id: adas
"#;

    const GOOD_VEHICLE: &str = r#"
kind: vehicle
id: car1
composition:
  zones:
    zonefl: { class: zone, subtype: linux }
  compute:
    hpc1: { class: compute, subtype: ad }
  gateway:
    gw1: { class: gw, subtype: tcu }
links:
  - { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas }
  - { id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0] }
channels:
  - id: obj
    from: hpc1:perception
    to: [zonefl:actuation]
    carried_by: [trunk-hpc, trunk-fl]
"#;

    const GOOD_BASELINE: &str = r#"
kind: baseline
id: bl1
parts:
  - ref: ./images/local-thing
    target_ids: [hpc1, gw1]
  - ref: ghcr.io/vxedge/parts/zone/linux/stack/probe-bundle:0.1.0
    digest: sha256:33c9c8d8625bd5c6d1f4d3d0c561fdaa930e057fcd1fb24fb3badb402f25429d
    target_ids: [zonefl]
"#;

    #[test]
    fn a_consistent_workspace_validates() {
        let root = workspace(
            "good",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", GOOD_VEHICLE),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        let ws = Workspace::load(&root).expect("parses");
        assert_eq!(ws.validate(), vec![], "expected a clean workspace");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn files_can_be_renamed_freely() {
        // The whole point of id-based references: names carry no meaning.
        let root = workspace(
            "renamed",
            &[
                ("zzz.yaml", GOOD_EXPERIMENT),
                ("aaa.yaml", GOOD_VEHICLE),
                ("mmm.yaml", GOOD_BASELINE),
            ],
        );
        assert_eq!(Workspace::load(&root).unwrap().validate(), vec![]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_slice_named_by_the_vehicle_but_undefined_is_caught() {
        // The cross-file join, and the reference most likely to be wrong in practice.
        let exp = GOOD_EXPERIMENT.replace("- id: adas", "- id: infotainment");
        let root = workspace(
            "slice",
            &[
                ("main.yaml", &exp),
                ("car1.yaml", GOOD_VEHICLE),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        let errs = Workspace::load(&root).unwrap().validate();
        assert!(
            errs.iter()
                .any(|e| matches!(e, WorkspaceValidationError::UndefinedSlice { slice, .. } if slice == "adas")),
            "{errs:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dangling_target_ids_and_endpoints_are_caught() {
        let baseline = GOOD_BASELINE.replace("target_ids: [hpc1, gw1]", "target_ids: [hpc99, gw1]");
        let vehicle = GOOD_VEHICLE.replace("gw1:eth2, hpc1:eth0", "gw1:eth2, ghost:eth0");
        let root = workspace(
            "dangling",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", &vehicle),
                ("parts.yaml", &baseline),
            ],
        );
        let errs = Workspace::load(&root).unwrap().validate();
        assert!(errs.iter().any(|e| matches!(
            e,
            WorkspaceValidationError::UnknownTarget { target, .. } if target == "hpc99"
        )));
        assert!(errs.iter().any(|e| matches!(
            e,
            WorkspaceValidationError::UnknownLinkEndpoint { component, .. } if component == "ghost"
        )));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_channel_over_links_that_do_not_connect_is_caught() {
        // Without this, `carried_by` could claim any links and per-topic-per-slice latency
        // attribution would rest on an unchecked assertion.
        let vehicle = GOOD_VEHICLE.replace(
            "carried_by: [trunk-hpc, trunk-fl]",
            "carried_by: [trunk-hpc]",
        );
        let root = workspace(
            "unreachable",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", &vehicle),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        let errs = Workspace::load(&root).unwrap().validate();
        assert!(
            errs.iter().any(|e| matches!(
                e,
                WorkspaceValidationError::ChannelNotCarried { unreachable, .. } if unreachable == "zonefl"
            )),
            "{errs:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn published_refs_must_be_pinned_and_local_ones_need_not_be() {
        let baseline = GOOD_BASELINE.replace(
            "    digest: sha256:33c9c8d8625bd5c6d1f4d3d0c561fdaa930e057fcd1fb24fb3badb402f25429d\n",
            "",
        );
        let root = workspace(
            "pin",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", GOOD_VEHICLE),
                ("parts.yaml", &baseline),
            ],
        );
        let errs = Workspace::load(&root).unwrap().validate();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(matches!(
            errs[0],
            WorkspaceValidationError::PublishedRefWithoutDigest { .. }
        ));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_malformed_digest_is_caught() {
        let baseline = GOOD_BASELINE.replace(
            "sha256:33c9c8d8625bd5c6d1f4d3d0c561fdaa930e057fcd1fb24fb3badb402f25429d",
            "sha256:deadbeef",
        );
        let root = workspace(
            "digest",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", GOOD_VEHICLE),
                ("parts.yaml", &baseline),
            ],
        );
        let errs = Workspace::load(&root).unwrap().validate();
        assert!(matches!(
            errs[0],
            WorkspaceValidationError::MalformedDigest { .. }
        ));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn workspace_needs_exactly_one_experiment() {
        let root = workspace("noexp", &[("car1.yaml", GOOD_VEHICLE)]);
        assert!(
            Workspace::load(&root)
                .unwrap()
                .validate()
                .contains(&WorkspaceValidationError::NoExperiment)
        );
        fs::remove_dir_all(&root).ok();

        let second = GOOD_EXPERIMENT.replace("id: exp1", "id: exp2");
        let root = workspace(
            "twoexp",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("arm-b.yaml", &second),
                ("car1.yaml", GOOD_VEHICLE),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        let errs = Workspace::load(&root).unwrap().validate();
        assert!(
            errs.iter()
                .any(|e| matches!(e, WorkspaceValidationError::MultipleExperiments { .. }))
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn target_ids_accepts_a_scalar_or_a_list() {
        let one: PartEntry = serde_yaml_ng::from_str("ref: ./x\ntarget_ids: hpc1").unwrap();
        let many: PartEntry = serde_yaml_ng::from_str("ref: ./x\ntarget_ids: [a, b]").unwrap();
        assert_eq!(one.target_ids.unwrap().as_slice(), vec!["hpc1"]);
        assert_eq!(many.target_ids.unwrap().as_slice(), vec!["a", "b"]);
    }

    #[test]
    fn results_directory_is_not_absorbed_into_the_workspace() {
        // Non-recursive load: a results archive containing YAML must not join the experiment.
        let root = workspace(
            "nested",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", GOOD_VEHICLE),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        fs::create_dir_all(root.join("results")).unwrap();
        fs::write(
            root.join("results/old-run.yaml"),
            "kind: experiment\nid: stale\n",
        )
        .unwrap();
        let ws = Workspace::load(&root).unwrap();
        assert_eq!(ws.experiments.len(), 1, "results/ must be ignored");
        fs::remove_dir_all(&root).ok();
    }
}

#[cfg(test)]
mod scaffold_roundtrip {
    use super::*;
    use crate::scaffold::{Template, WorkspacePlan};

    /// The loop that makes the schema real: what `experiment new` writes must be what the
    /// loader accepts. Generating and validating in isolation proves nothing about the join.
    #[test]
    fn every_generated_template_validates() {
        for template in [Template::Minimal, Template::Slicing] {
            let tmp = std::env::temp_dir().join(format!(
                "vxrt-{}-{}",
                std::process::id(),
                template.as_str()
            ));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp).unwrap();

            let plan = WorkspacePlan::plan("demo", template).unwrap();
            let root = plan.write(&tmp).unwrap();

            let ws = Workspace::load(&root)
                .unwrap_or_else(|e| panic!("{} template does not load: {e}", template.as_str()));
            let errors = ws.validate();
            assert!(
                errors.is_empty(),
                "{} template does not validate: {errors:#?}",
                template.as_str()
            );
            std::fs::remove_dir_all(&tmp).ok();
        }
    }
}
