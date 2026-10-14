//! Experiment-workspace model and validator (Track D, `docs/ROADMAP.md`).
//!
//! A workspace is a folder of YAML files. Each declares its `kind` and `id`; the loader
//! indexes on that pair and every cross-reference is by id, never by path — so files can be
//! renamed or reorganised freely.
//!
//! §6.0 is **the only authored schema, and the only runnable one** (`docs/ARCHITECTURE.md`
//! §6.0.6, 2026-07-31) — the earlier §6.1/§6.2 manifest format and its `crate::manifest`
//! module are deleted, not merely superseded; every workspace parses through this module.
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
    #[error(
        "`{}` is a §6.{} manifest (it declares `{key}:` and no `kind:`), which stopped being a \
         provisioning input on 2026-07-31 and was deleted. Point this at the experiment's §6.0 \
         workspace directory instead; if this file is a frozen manifest kept as provenance, \
         read it at the `v0.1.0` tag, where the schema it was written to still exists.",
        .path.display(),
        if .key == "vehicle" { "2" } else { "1" }
    )]
    LegacyManifest { path: PathBuf, key: String },
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
        "vehicle `{vehicle}`: link `{link}` endpoint `{endpoint}` has `ip: {ip}` — expected an \
         address with a prefix length, e.g. `10.101.0.11/24`"
    )]
    MalformedAddress {
        vehicle: String,
        link: String,
        endpoint: String,
        ip: String,
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
    #[error(
        "vehicle `{vehicle}`: link `{link}` has an `impairments:` block with nothing in it — set \
         at least one of `rate_mbps`, `delay_us`, `jitter_us`, `loss_pct`, or drop the block"
    )]
    ImpairmentEmpty { vehicle: String, link: String },
    #[error(
        "vehicle `{vehicle}`: link `{link}` sets `impairments.jitter_us` without \
         `impairments.delay_us` — jitter is variation *of* a delay, so there is nothing for it \
         to vary. Add a `delay_us`, or drop the jitter."
    )]
    ImpairmentJitterWithoutDelay { vehicle: String, link: String },
    #[error(
        "vehicle `{vehicle}`: link `{link}` sets `impairments.{field}: {value}` — expected \
         {expected}"
    )]
    ImpairmentOutOfRange {
        vehicle: String,
        link: String,
        field: &'static str,
        value: String,
        expected: &'static str,
    },
    #[error(
        "vehicle `{vehicle}`: link `{link}` declares `impairments:` but binds no `slice:` — a \
         link that does not cross vxgateway has no access port to install netem on, so the \
         impairment would be silently ignored. Bind the link to a slice, or drop the block."
    )]
    ImpairmentWithoutSlice { vehicle: String, link: String },
    #[error(
        "vehicle `{vehicle}`: link `{link}` has a `harness:` block with nothing in it — set at \
         least one of `medium`, `length_m`, `weight_g`, or drop the block"
    )]
    HarnessEmpty { vehicle: String, link: String },
    #[error(
        "vehicle `{vehicle}`: link `{link}` sets `harness.{field}: {value}` — a harness segment \
         cannot have a negative {field}"
    )]
    HarnessNegative {
        vehicle: String,
        link: String,
        field: &'static str,
        value: String,
    },
}

/// Traffic endpoints that name no component. `internet` is the host-side reflector on the far
/// side of the gateway; `carla` is the simulator. Kept as a closed list so a typo'd component id
/// is an error at authoring time rather than a profile that silently measures nothing.
const SYMBOLIC_ENDPOINTS: &str = "internet, carla";

fn is_symbolic_endpoint(ep: &str) -> bool {
    SYMBOLIC_ENDPOINTS.split(", ").any(|s| s == ep)
}

/// Accepts either a scalar or a sequence, so `target_ids: fn-hpc1` and `target_ids: [fn-a, fn-b]` both
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

/// Every top-level field a `kind: experiment` document may declare — needed in full so
/// `#[serde(deny_unknown_fields)]` can catch a typo'd key without also rejecting every field
/// this struct doesn't itself interpret. `services`/`telemetry`/`results`/`method`/
/// `traffic_profiles` are carried as opaque `Value`s deliberately: `model::build` already
/// parses and validates each of them from the same raw document (`Workspace::experiments_raw`)
/// with its own named errors (e.g. `ServiceDef`'s own `deny_unknown_fields`, `narrow()`'s range
/// checks) — retyping them here would be a second copy of that validation to keep in sync, for
/// no additional protection. `kind`/`world` are accepted-but-unused here for the same reason
/// `rxswin` is on `BaselineDoc`: `kind` is dispatched by `Header` before this struct is ever
/// parsed (it must still be a *declared* field, or `deny_unknown_fields` would reject every
/// real document's own `kind: experiment` line); `world` is documented in `ARCHITECTURE.md`
/// §6.0.3 but nothing in this codebase reads it yet (§30's audit found it, `docs/ROADMAP.md`
/// tracks it as a documented-but-unimplemented gap, not a validation bug — there is nothing to
/// misdeclare in a field nothing consumes).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentDoc {
    pub kind: String,
    pub id: String,
    /// A human-readable label, distinct from `id`. Not in `ARCHITECTURE.md`'s own §6.0.3
    /// schema at all — found live while checking this change against `vxmodels`' real
    /// renderer (`vxmodel render --experiment ...`, which emits it from a SysML model's own
    /// `name` attribute) rather than only against this repo's own hand-authored fixtures.
    /// Without this field, `deny_unknown_fields` would have broken every real `vxmodel`
    /// render — and, per `vxmodels/.github/workflows/ci.yml`, that repo's own CI.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub vehicles: Vec<String>,
    #[serde(default)]
    pub baseline: Option<String>,
    #[serde(default)]
    pub world: Option<String>,
    #[serde(default)]
    pub network: Option<NetworkDoc>,
    #[serde(default)]
    pub services: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub telemetry: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub results: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub method: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub traffic_profiles: Option<serde_yaml_ng::Value>,
}

/// `tunnels`/`nos_check`/`data_plane`/`defaults` are opaque `Value`s for the same reason
/// `ExperimentDoc`'s own blocks are: `model::build` already validates each with its own named
/// errors, so retyping them here would duplicate that validation rather than add to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDoc {
    #[serde(default)]
    pub slices: Vec<SliceDoc>,
    #[serde(default)]
    pub tunnels: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub nos_check: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub data_plane: Option<String>,
    #[serde(default)]
    pub defaults: Option<serde_yaml_ng::Value>,
}

/// Every field a slice entry may declare — closes §30's last remaining gap (this struct used
/// to type only `id`, so a typo'd slice field was silently ignored). `mode`/`vlan`/`qos`/
/// `s_nssai` stay opaque (`Value`/loosely-typed) rather than fully structured on purpose:
/// `model::build::slices_of` already re-parses and range-checks each of them from the same raw
/// document via `narrow()`, which produces a named `ModelError::OutOfRange` naming the exact
/// field and its max — typing `vlan` as a strict `u16` here would make an out-of-range value
/// fail at *workspace load* with a generic serde overflow message instead, a strictly worse
/// error for the same mistake. This struct's job is catching an unknown field name, not
/// re-validating a value shape `slices_of` already owns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SliceDoc {
    pub id: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub vlan: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub qos: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub s_nssai: Option<serde_yaml_ng::Value>,
}

/// Unlike `ExperimentDoc`, every field this document declares is typed all the way down
/// (`build_entity` reads only from `&VehicleDoc`, never a parallel raw `Value` — confirmed
/// before adding `deny_unknown_fields` here, since that duplication is exactly what would have
/// made this unsafe). `kind` is accepted-but-unused for the same reason `ExperimentDoc`'s own
/// is: `Header` already dispatches on it, but it must still be a declared field or every real
/// vehicle document's own `kind: vehicle` line would be rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VehicleDoc {
    pub kind: String,
    pub id: String,
    /// A human-readable label, distinct from `id`. Same finding as `ExperimentDoc::name` —
    /// not in `ARCHITECTURE.md`'s §6.0.1 schema, but real, live-produced data
    /// (`vxmodel render --vehicle ...` emits it from a SysML model's own `name`), found by
    /// checking against `vxmodels`' actual renderer output rather than only this repo's own
    /// fixtures.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    /// Regions of the entity. Named, not enumerated: `front-left`/`rear-right` are road-vehicle
    /// conventions and another mobility entity has its own.
    #[serde(default)]
    pub zones: BTreeMap<String, ZoneDoc>,
    /// Units located in zones. Replaces `composition: {zones, compute, gateway}`, which grouped
    /// devices by what they are and called one group "zones" — but a zone is a *place*. `role`
    /// is a field now, so a sensor or actuator needs no fourth group.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceDoc>,
    /// Units of behaviour, each deployed onto a device. Software attaches here, not to a device,
    /// so one device can host several and a study can redeploy without rewriting `channels`.
    #[serde(default)]
    pub functions: BTreeMap<String, FunctionDoc>,
    #[serde(default)]
    pub links: Vec<LinkDoc>,
    #[serde(default)]
    pub channels: Vec<ChannelDoc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneDoc {
    #[serde(default)]
    pub name: Option<String>,
    /// Pointer into an external zoning standard (an ATA zone number, a compartment
    /// designation). Carried, never interpreted.
    #[serde(default)]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceDoc {
    /// `zone-controller` | `hpc` | `gateway` | `ecu` | `sensor` | `actuator` | `switch`.
    pub role: String,
    #[serde(default)]
    pub zone: Option<String>,
    /// `container` | `qemu-microvm` | `libvirt`. Required for anything attached to a slice: the
    /// planner has to know whether to build a veth or a tap, and guessing is what produced a
    /// workspace that validated and then could not boot.
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub resources: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub realtime: Option<serde_yaml_ng::Value>,
    /// COVESA VSS path, for a sensor or actuator.
    #[serde(default)]
    pub vss: Option<String>,
    #[serde(default)]
    pub dbc: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionDoc {
    pub deployed_on: String,
    #[serde(default)]
    pub asil: Option<String>,
}

impl VehicleDoc {
    /// Device ids in map order — which is traversal order, and therefore what positional
    /// endpoint names (`vz0`, `vz1`, …) derive from.
    pub fn device_ids(&self) -> Vec<&str> {
        self.devices.keys().map(String::as_str).collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkDoc {
    pub id: String,
    #[serde(default)]
    pub endpoints: Vec<EndpointDoc>,
    #[serde(default)]
    pub slice: Option<String>,
    /// PHY characteristics to emulate on this segment (`ARCHITECTURE.md` §7, "Harness
    /// modeling"). Absent means an unimpaired virtual wire, which is what every frozen
    /// experiment declares and therefore what they keep measuring.
    #[serde(default)]
    pub impairments: Option<ImpairmentDoc>,
    /// The physical harness segment this link stands for — the declarative half of harness
    /// modeling, alongside [`ImpairmentDoc`]'s emulated half. §7 states the claim this exists to
    /// back: zonal E/E is justified by harness reduction, so a model that cannot say how much
    /// wire a topology needs cannot express the architecture's own headline benefit. Carried and
    /// reported, never rendered onto the kernel — nothing about a length in meters or a weight
    /// in grams is a `tc` argument.
    #[serde(default)]
    pub harness: Option<HarnessDoc>,
}

/// A harness segment's physical properties, as an author states them.
///
/// Every field is descriptive rather than executable: this feeds a harness-weight/length report
/// (a zonal-versus-domain study's dependent variable), not the reconciler. [`ImpairmentDoc`] on
/// the same link is the half that *does* reach the kernel, for the segment's timing behaviour.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HarnessDoc {
    /// `100BASE-T1`, `1000BASE-T1`, `CAN-FD`, … Free-form, matching [`crate::model::Harness`]'s
    /// own comment: the taxonomy of automotive harness media is larger than this workspace needs
    /// to enumerate, and a closed enum here would reject a real medium the day someone names one.
    #[serde(default)]
    pub medium: Option<String>,
    #[serde(default)]
    pub length_m: Option<f64>,
    #[serde(default)]
    pub weight_g: Option<f64>,
}

impl HarnessDoc {
    pub fn is_empty(&self) -> bool {
        self.medium.is_none() && self.length_m.is_none() && self.weight_g.is_none()
    }
}

/// What a harness segment does to the packets crossing it: the manifest's expression of
/// 100BASE-T1 versus 1000BASE-T1 versus a clean virtual wire.
///
/// This is the field that makes harness topology an experimental *variable* rather than a
/// drawing — §7's claim that star-versus-daisy-chain is iterable in minutes needs the segments
/// to differ in something a measurement can see.
///
/// # Semantics, stated because the alternatives are all defensible
///
/// Every value describes **one traversal of the segment, in one direction**. `delay_us: 250` is
/// a one-way delay, so a round trip over the link accrues 500 µs; `loss_pct` is the chance a
/// packet crossing in either direction is dropped. Both directions are impaired identically,
/// because a length of twisted pair does not know which way the electrons are going.
///
/// # The jitter caveat
///
/// `jitter_us` is netem's, and netem applies jitter per packet independently — so a packet drawn
/// a short delay **can overtake** one drawn a long delay. That reordering is real, not an
/// artifact to correct for, but it is not what a BASE-T1 segment does either, and an experiment
/// measuring a reordering-sensitive protocol has to account for it. Delay and loss carry no such
/// caveat.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImpairmentDoc {
    /// Segment rate cap in mbit/s — 100 for 100BASE-T1, 1000 for 1000BASE-T1.
    ///
    /// Distinct from a slice's `qos.max_rate_mbps`, and they compose: the slice's HTB tree caps
    /// what a *slice* may draw from the shared uplink, this caps what one *wire* can carry. A
    /// zonal-versus-domain comparison varies this while holding the slice QoS fixed.
    #[serde(default)]
    pub rate_mbps: Option<u32>,
    /// One-way propagation + PHY delay, microseconds.
    #[serde(default)]
    pub delay_us: Option<u32>,
    /// Delay variation, microseconds. Requires `delay_us` — netem has nothing to vary otherwise.
    #[serde(default)]
    pub jitter_us: Option<u32>,
    /// Packet loss, percent (`0.01` is one in ten thousand).
    #[serde(default)]
    pub loss_pct: Option<f64>,
}

impl ImpairmentDoc {
    /// True when the block was written but says nothing — worth reporting, since the author
    /// plainly meant to impair something.
    pub fn is_empty(&self) -> bool {
        self.rate_mbps.is_none()
            && self.delay_us.is_none()
            && self.jitter_us.is_none()
            && self.loss_pct.is_none()
    }
}

/// One end of a link, in either of two forms:
///
/// ```yaml
/// endpoints: [gw1:eth0, zonefl:eth0]                        # bare: no address needed
/// endpoints: [{ node: node-cont-01, port: eth0, ip: 10.101.0.11/24 }]
/// ```
///
/// The bare form is kept because it is the right one whenever nothing has to be reachable at a
/// known address, which is most attachments — exp-001 declares no address at all and lets the
/// campaign assign them. The object form exists for the case that has no other expression:
/// exp-002's three arms share one VLAN and are told apart *only* by address, so the probe
/// reaches each arm by its `10.101.0.1{1,2,3}`.
///
/// An address is a property of the **attachment**, not of the device — a device on two slices
/// has two — so it lives on the endpoint rather than on `devices:`. And it lives in the
/// *vehicle* rather than the experiment because it describes a structural fact ("this ECU
/// answers here"), which is what keeps one vehicle usable by many baselines.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum EndpointDoc {
    /// `component` or `component:port`.
    Bare(String),
    Attached(AttachedEndpoint),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttachedEndpoint {
    pub node: String,
    #[serde(default)]
    pub port: Option<String>,
    /// Host address in CIDR form (`10.101.0.11/24`), configured on this attachment by the zone
    /// backend. Validated for shape at load time — an address that only fails at `ip addr add`
    /// fails halfway through provisioning, with half a topology up.
    #[serde(default)]
    pub ip: Option<String>,
}

impl EndpointDoc {
    /// The component id this end names.
    pub fn node(&self) -> &str {
        match self {
            EndpointDoc::Bare(s) => component_of(s),
            EndpointDoc::Attached(a) => &a.node,
        }
    }

    /// The port, when declared.
    pub fn port(&self) -> Option<&str> {
        match self {
            EndpointDoc::Bare(s) => s.split_once(':').map(|(_, p)| p),
            EndpointDoc::Attached(a) => a.port.as_deref(),
        }
    }

    pub fn ip(&self) -> Option<&str> {
        match self {
            EndpointDoc::Bare(_) => None,
            EndpointDoc::Attached(a) => a.ip.as_deref(),
        }
    }

    /// `component:port`, the spelling §6.1 lowering and error messages both want.
    pub fn qualified(&self) -> String {
        match self.port() {
            Some(p) => format!("{}:{p}", self.node()),
            None => self.node().to_string(),
        }
    }
}

impl std::fmt::Display for EndpointDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.qualified())
    }
}

/// Hand-written rather than `#[serde(untagged)]` on the way in: untagged reports every failure
/// as "data did not match any variant", so a typo'd key inside the object form would be
/// diagnosed as a malformed *string*. Dispatching on the YAML node type first means the object
/// form's own error survives — `unknown field `adress`, expected one of node, port, ip`.
impl<'de> Deserialize<'de> for EndpointDoc {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = EndpointDoc;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("`component:port`, or { node, port, ip }")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<EndpointDoc, E> {
                Ok(EndpointDoc::Bare(v.to_string()))
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                m: M,
            ) -> Result<EndpointDoc, M::Error> {
                AttachedEndpoint::deserialize(serde::de::value::MapAccessDeserializer::new(m))
                    .map(EndpointDoc::Attached)
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelDoc {
    pub id: String,
    pub from: String,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub carried_by: Vec<String>,
    /// `dds` | `zenoh` | `someip` | `udp` | `tcp` | `can` (`ARCHITECTURE.md` §6.0.1). Absent
    /// means `udp` — every channel declared before this field existed omits it and keeps
    /// planning the same way.
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub qos: Option<ChannelQosDoc>,
}

/// A channel's own QoS, distinct from the slice's — a DDS topic has its own deadline and
/// reliability, which is half of what H1/H2 attribute per topic rather than only per link.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelQosDoc {
    /// `reliable` | `best-effort`.
    #[serde(default)]
    pub reliability: Option<String>,
    #[serde(default)]
    pub deadline_ms: Option<u32>,
    #[serde(default)]
    pub rate_hz: Option<f64>,
}

/// `kind` is accepted-but-unused, the same reason `ExperimentDoc`'s/`VehicleDoc`'s own are.
/// `rxswin` (`ARCHITECTURE.md` §6.0.2: "UN R156, a property of the vehicle TYPE") was, until
/// §30's audit found it, silently dropped by serde with no `deny_unknown_fields` to catch it —
/// declared here now so it round-trips, though nothing in this codebase reads it yet (`vxedge
/// manifest bom`'s planned RxSWIN roll-up is real, separate, not-yet-built scope, not this
/// field's own gap).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineDoc {
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub rxswin: Option<String>,
    #[serde(default)]
    pub parts: Vec<PartEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartEntry {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub target_ids: Option<OneOrMany>,
    /// `container` | `bundle` — what this loadable *is*, when the reference does not say.
    ///
    /// A part knows its own kind: a `zone/linux/stack/…` vxparts ref is a bootable bundle, a
    /// `local/vxprobe:host-process` is not. Only a bare path is genuinely ambiguous, and that
    /// is what this field is for. Stating it is what lets a baseline pointing a `libvirt` arm
    /// at a container image be *rejected* rather than merely unrepresentable — the check H3
    /// needs most, since it runs one function on three backends from three parts.
    #[serde(default)]
    pub kind: Option<String>,
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
            ws.absorb(&text, &path)?;
        }

        Ok(ws)
    }

    /// Every `*.yaml` in `root` (non-recursive), sorted by path and joined with `---`: the
    /// one-document form of a workspace directory. Same shape `vxedged` reads for a path
    /// manifest, so a caller holding only a directory can hand the result to text-based entry
    /// points such as [`crate::model::synthesize_vehicle_workspace`].
    pub fn read_text(root: &Path) -> Result<String, WorkspaceError> {
        let io = |path: &Path, source| WorkspaceError::Io {
            path: path.to_path_buf(),
            source,
        };
        let mut paths: Vec<PathBuf> = std::fs::read_dir(root)
            .map_err(|e| io(root, e))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
            .collect();
        paths.sort();
        let mut out = String::new();
        for path in paths {
            let text = std::fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            if !out.is_empty() {
                out.push_str("---\n");
            }
            out.push_str(&text);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// True for a workspace that holds a vehicle but no experiment: the shape `vehicle up`
    /// accepts by synthesizing an experiment around it.
    pub fn is_bare_vehicle(&self) -> bool {
        self.experiments.is_empty() && !self.vehicles.is_empty()
    }

    /// Read a workspace from YAML text rather than from a directory.
    ///
    /// One file can hold the whole workspace as `---`-separated documents. That works because
    /// every reference is by id and never by path, so the file boundary carries no meaning —
    /// the loader simply never reached it before. Separate files stay the scaffold's default
    /// because they teach the model, but an engineer who wants one file, or a caller with no
    /// directory to point at (an inline manifest over vxAPI), now has one.
    pub fn from_yaml(text: &str) -> Result<Self, WorkspaceError> {
        let mut ws = Workspace::default();
        ws.absorb(text, Path::new("<inline>"))?;
        Ok(ws)
    }

    /// Route every YAML document in `text` by its `kind`.
    ///
    /// Multi-document on purpose — see [`Workspace::from_yaml`]. A file holding one document is
    /// the same case with one iteration, so there is a single path here rather than two that
    /// can drift.
    fn absorb(&mut self, text: &str, path: &Path) -> Result<(), WorkspaceError> {
        use serde::Deserialize;

        let parse_err = |source| WorkspaceError::Parse {
            path: path.to_path_buf(),
            source,
        };
        for document in serde_yaml_ng::Deserializer::from_str(text) {
            let value = serde_yaml_ng::Value::deserialize(document).map_err(parse_err)?;
            // An empty document (a trailing `---`, or a file of only comments) is not an error;
            // it simply declares nothing.
            if value.is_null() {
                continue;
            }
            // A §6.1/§6.2 manifest reaches here whenever something still points at the old
            // file — a campaign script, a stale note. "missing field `kind`" is true and
            // useless; say what the document is and where to go instead.
            if value.get("kind").is_none() {
                for key in ["experiment", "vehicle"] {
                    if value.get(key).is_some() {
                        return Err(WorkspaceError::LegacyManifest {
                            path: path.to_path_buf(),
                            key: key.to_string(),
                        });
                    }
                }
            }
            let header: Header = serde_yaml_ng::from_value(value.clone()).map_err(parse_err)?;
            fn doc<T: serde::de::DeserializeOwned>(
                value: serde_yaml_ng::Value,
                path: &Path,
            ) -> Result<T, WorkspaceError> {
                serde_yaml_ng::from_value(value).map_err(|source| WorkspaceError::Parse {
                    path: path.to_path_buf(),
                    source,
                })
            }
            match header.kind {
                Kind::Experiment => {
                    self.experiments.push(doc(value.clone(), path)?);
                    self.experiments_raw.push(value);
                }
                Kind::Vehicle => self.vehicles.push(doc(value, path)?),
                Kind::Baseline => self.baselines.push(doc(value, path)?),
                Kind::Road => self.roads.push(header.id),
                Kind::Component => self.components.push(header.id),
            }
        }
        Ok(())
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
        // Parts target *functions*: software runs as a function, and one device may host several.
        let mut function_owner: BTreeMap<String, String> = BTreeMap::new();
        for v in &self.vehicles {
            for id in v.functions.keys() {
                function_owner.insert(id.clone(), v.id.clone());
            }
        }
        let mut component_owner: BTreeMap<String, String> = BTreeMap::new();
        for v in &self.vehicles {
            for id in v.device_ids() {
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
            let own: BTreeSet<&str> = v.device_ids().into_iter().collect();
            let link_ids: BTreeSet<&str> = v.links.iter().map(|l| l.id.as_str()).collect();

            for l in &v.links {
                for ep in &l.endpoints {
                    let c = ep.node();
                    if !own.contains(c) {
                        errors.push(E::UnknownLinkEndpoint {
                            vehicle: v.id.clone(),
                            link: l.id.clone(),
                            endpoint: ep.to_string(),
                            component: c.to_string(),
                        });
                    }
                    if let Some(ip) = ep.ip()
                        && !is_cidr(ip)
                    {
                        errors.push(E::MalformedAddress {
                            vehicle: v.id.clone(),
                            link: l.id.clone(),
                            endpoint: ep.to_string(),
                            ip: ip.to_string(),
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

                if let Some(imp) = &l.impairments {
                    check_impairments(&v.id, &l.id, imp, l.slice.is_some(), &mut errors);
                }
                // Unlike impairments, harness carries no slice-binding rule: it feeds a
                // length/weight report rather than the reconciler, so it is meaningful on any
                // link, slice-bound or not — a direct point-to-point harness segment has a
                // length whether or not it happens to cross vxgateway.
                if let Some(h) = &l.harness {
                    check_harness(&v.id, &l.id, h, &mut errors);
                }
            }

            let own_functions: BTreeSet<&str> = v.functions.keys().map(String::as_str).collect();
            for ch in &v.channels {
                let mut endpoints = vec![&ch.from];
                endpoints.extend(ch.to.iter());
                for ep in &endpoints {
                    let c = component_of(ep);
                    // Channels connect functions, not devices — the distinction the model exists
                    // to make, enforced where the author can act on it.
                    if !own_functions.contains(c) {
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
                    let host = |f: &str| v.functions.get(f).map(|x| x.deployed_on.as_str());
                    let mut reachable: BTreeSet<&str> = BTreeSet::new();
                    if let Some(h) = host(component_of(&ch.from)) {
                        reachable.insert(h);
                    }
                    // Small graphs; iterate to a fixed point rather than build a real DSU.
                    loop {
                        let before = reachable.len();
                        for l in v.links.iter().filter(|l| ch.carried_by.contains(&l.id)) {
                            let comps: Vec<&str> = l.endpoints.iter().map(|e| e.node()).collect();
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
                        let Some(c) = host(component_of(ep)) else {
                            continue;
                        };
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
                for c in v.functions.keys().map(String::as_str) {
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
                        if !function_owner.contains_key(t) {
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

    /// **The workspace identity**: SHA-256 over every file the experiment is made of.
    ///
    /// One rule, stated in `ARCHITECTURE.md` §13, because two answers to "which workspace was
    /// this" is one too many — an audit record and its results archive have to agree. The rule:
    ///
    /// - Every file under the workspace root, recursively, **except `results/`**. Outputs are
    ///   not inputs, and including them would change the identity after the first run.
    /// - The relative path is hashed alongside the bytes, so renaming a file is a change.
    /// - Entries in sorted path order, so the answer does not depend on directory order.
    ///
    /// Recursive on purpose, unlike [`Workspace::load`]: a sandbox's `parts/`, `assets/`,
    /// `scripts/` and `tools/` are *what ran*, even though they are not workspace documents. A
    /// local part that changes is a different experiment, and an identity that ignored it would
    /// say two different runs were the same.
    ///
    /// `vxtelemetry` implements the same rule without depending on this crate, and both sides
    /// pin the same test vector — which is how two implementations of one rule stay honest.
    pub fn content_sha256(root: &Path) -> Result<String, WorkspaceError> {
        use sha2::{Digest, Sha256};

        let mut files: Vec<(String, PathBuf)> = Vec::new();
        collect_files(root, root, &mut files)?;
        files.sort();

        let mut h = Sha256::new();
        for (rel, path) in files {
            let bytes = std::fs::read(&path).map_err(|source| WorkspaceError::Io {
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
        Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
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

/// `10.101.0.11/24` or `fd00::1/64`. The prefix length is required: a bare address leaves the
/// zone backend guessing at the mask, and guessing produced an attachment that came up on the
/// wrong subnet once already.
fn is_cidr(raw: &str) -> bool {
    let Some((addr, prefix)) = raw.split_once('/') else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => prefix <= 32,
        Ok(std::net::IpAddr::V6(_)) => prefix <= 128,
        Err(_) => false,
    }
}

/// Everything wrong with one `impairments:` block, reported against the file the author wrote.
///
/// Three of the four checks are things `tc` would reject anyway, caught here so the message
/// names a manifest field instead of a netem argument. The fourth — an impairment on a link that
/// binds no slice — is the one `tc` would never complain about, because nothing would ever be
/// asked of it: no slice means no access port, and the block would sit in the file looking
/// effective. That is the failure this function exists for.
fn check_impairments(
    vehicle: &str,
    link: &str,
    imp: &ImpairmentDoc,
    has_slice: bool,
    errors: &mut Vec<WorkspaceValidationError>,
) {
    use WorkspaceValidationError as E;
    if imp.is_empty() {
        errors.push(E::ImpairmentEmpty {
            vehicle: vehicle.to_string(),
            link: link.to_string(),
        });
        return;
    }
    if !has_slice {
        errors.push(E::ImpairmentWithoutSlice {
            vehicle: vehicle.to_string(),
            link: link.to_string(),
        });
    }
    if imp.jitter_us.is_some() && imp.delay_us.is_none() {
        errors.push(E::ImpairmentJitterWithoutDelay {
            vehicle: vehicle.to_string(),
            link: link.to_string(),
        });
    }
    if imp.rate_mbps == Some(0) {
        errors.push(E::ImpairmentOutOfRange {
            vehicle: vehicle.to_string(),
            link: link.to_string(),
            field: "rate_mbps",
            value: "0".to_string(),
            expected: "at least 1 — a segment that carries nothing is a disconnected link, not \
                       an impaired one",
        });
    }
    // NaN fails every comparison, so the range test is written to reject it rather than to
    // accept it by falling through.
    if let Some(loss) = imp.loss_pct
        && !(0.0..=100.0).contains(&loss)
    {
        errors.push(E::ImpairmentOutOfRange {
            vehicle: vehicle.to_string(),
            link: link.to_string(),
            field: "loss_pct",
            value: loss.to_string(),
            expected: "a percentage between 0 and 100",
        });
    }
}

/// Everything wrong with one `harness:` block. Simpler than [`check_impairments`]: nothing here
/// composes with anything else in the file, so there is no cross-field rule to enforce, only
/// "did the author write something" and "is a physical quantity actually physical."
fn check_harness(
    vehicle: &str,
    link: &str,
    h: &HarnessDoc,
    errors: &mut Vec<WorkspaceValidationError>,
) {
    use WorkspaceValidationError as E;
    if h.is_empty() {
        errors.push(E::HarnessEmpty {
            vehicle: vehicle.to_string(),
            link: link.to_string(),
        });
        return;
    }
    for (field, value) in [("length_m", h.length_m), ("weight_g", h.weight_g)] {
        // Spelled as `nan() || < 0.0` rather than `!(v >= 0.0)`: both reject NaN identically, but
        // clippy reads a negated partial-order comparison as a sign the two operands might not
        // be comparable at all, which is not the concern here.
        if value.is_some_and(|v| v.is_nan() || v < 0.0) {
            errors.push(E::HarnessNegative {
                vehicle: vehicle.to_string(),
                link: link.to_string(),
                field,
                value: value.unwrap().to_string(),
            });
        }
    }
}

/// Every file under `dir`, relative to `base`, skipping `results/` — see
/// [`Workspace::content_sha256`] for why outputs are excluded.
fn collect_files(
    base: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), WorkspaceError> {
    let entries = std::fs::read_dir(dir).map_err(|source| WorkspaceError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let path = entry
            .map_err(|source| WorkspaceError::Io {
                path: dir.to_path_buf(),
                source,
            })?
            .path();
        let rel = path
            .strip_prefix(base)
            .expect("walked path under base")
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            if rel == "results" {
                continue;
            }
            collect_files(base, &path, out)?;
        } else {
            out.push((rel, path));
        }
    }
    Ok(())
}

pub(crate) fn is_sha256(d: &str) -> bool {
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
zones:
  front-left: {}
devices:
  zonefl: { role: zone-controller, zone: front-left, backend: container }
  hpc1: { role: hpc, backend: container }
  gw1: { role: gateway, backend: container }
functions:
  fn-zonefl: { deployed_on: zonefl }
  fn-hpc1: { deployed_on: hpc1 }
  fn-gw1: { deployed_on: gw1 }
links:
  - { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas }
  - { id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0] }
channels:
  - id: obj
    from: fn-hpc1
    to: [fn-zonefl]
    carried_by: [trunk-hpc, trunk-fl]
"#;

    const GOOD_BASELINE: &str = r#"
kind: baseline
id: bl1
parts:
  - ref: ./images/local-thing
    target_ids: [fn-hpc1, fn-gw1]
  - ref: ghcr.io/vxedge/parts/zone/linux/stack/probe-bundle:0.1.0
    digest: sha256:33c9c8d8625bd5c6d1f4d3d0c561fdaa930e057fcd1fb24fb3badb402f25429d
    target_ids: [fn-zonefl]
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

    /// Addressing and impairment are orthogonal — a pinned attachment address (the object
    /// endpoint form exp-002 uses to tell its three arms apart on one VLAN) and a harness
    /// impairment (an L2 property of the wire itself) compose freely on the same link, because a
    /// real PHY segment doesn't know or care what is addressed on top of it. No new field is
    /// needed to combine them; this pins that the two independent parts of the schema actually
    /// do combine rather than merely both existing.
    #[test]
    fn a_pinned_address_and_an_impairment_compose_on_one_link() {
        let vehicle = GOOD_VEHICLE.replace(
            "- { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas }",
            "- id: trunk-fl\n    endpoints:\n      - gw1:eth0\n      - { node: zonefl, port: eth0, ip: 10.101.0.11/24 }\n    slice: adas\n    impairments: { rate_mbps: 100, delay_us: 250 }",
        );
        let root = workspace(
            "imp-and-ip",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", &vehicle),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        assert_eq!(Workspace::load(&root).unwrap().validate(), vec![]);
        fs::remove_dir_all(&root).ok();
    }

    /// A well-formed impairment adds no findings — the block is usable, not merely parseable.
    #[test]
    fn a_well_formed_impairment_validates() {
        let vehicle = GOOD_VEHICLE.replace(
            "- { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas }",
            "- { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas, \
             impairments: { rate_mbps: 100, delay_us: 250, jitter_us: 40, loss_pct: 0.01 } }",
        );
        let root = workspace(
            "imp-ok",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", &vehicle),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        assert_eq!(Workspace::load(&root).unwrap().validate(), vec![]);
        fs::remove_dir_all(&root).ok();
    }

    /// The impairment mistake that fails *silently*: `trunk-hpc` binds no slice, so it never
    /// reaches vxgateway and its netem would never be installed — with nothing at any later
    /// stage to say so. Every other check here is something `tc` would also reject; this one is
    /// the reason the validator looks at impairments at all.
    #[test]
    fn an_impairment_on_a_link_that_binds_no_slice_is_caught() {
        let vehicle = GOOD_VEHICLE.replace(
            "- { id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0] }",
            "- { id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0], impairments: { delay_us: 250 } }",
        );
        let root = workspace(
            "imp-noslice",
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
                WorkspaceValidationError::ImpairmentWithoutSlice { link, .. } if link == "trunk-hpc"
            )),
            "{errs:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// Malformed blocks, reported together rather than one per run — the same all-at-once
    /// contract every other check here holds to, and what an author editing a file needs.
    #[test]
    fn malformed_impairments_are_all_reported_at_once() {
        use WorkspaceValidationError as E;
        let vehicle = GOOD_VEHICLE
            .replace(
                "- { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas }",
                "- { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas, \
                 impairments: { jitter_us: 40, rate_mbps: 0, loss_pct: 150.0 } }",
            )
            .replace(
                "- { id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0] }",
                "- { id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0], slice: adas, \
                 impairments: {} }",
            );
        let root = workspace(
            "imp-bad",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", &vehicle),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        let errs = Workspace::load(&root).unwrap().validate();
        assert!(
            errs.iter().any(
                |e| matches!(e, E::ImpairmentJitterWithoutDelay { link, .. } if link == "trunk-fl")
            ),
            "{errs:?}"
        );
        assert!(
            errs.iter().any(
                |e| matches!(e, E::ImpairmentOutOfRange { field: "rate_mbps", link, .. } if link == "trunk-fl")
            ),
            "{errs:?}"
        );
        assert!(
            errs.iter().any(
                |e| matches!(e, E::ImpairmentOutOfRange { field: "loss_pct", link, .. } if link == "trunk-fl")
            ),
            "{errs:?}"
        );
        assert!(
            errs.iter()
                .any(|e| matches!(e, E::ImpairmentEmpty { link, .. } if link == "trunk-hpc")),
            "{errs:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// The declarative half of harness modeling: length/weight/medium, carried for a
    /// zonal-versus-domain report, independent of whether the link is impaired or even
    /// slice-bound.
    #[test]
    fn a_harness_block_validates_and_composes_with_impairments() {
        let vehicle = GOOD_VEHICLE
            .replace(
                "- { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas }",
                "- { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas, \
                 impairments: { rate_mbps: 100, delay_us: 250 }, \
                 harness: { medium: 100BASE-T1, length_m: 2.4, weight_g: 85.0 } }",
            )
            .replace(
                "- { id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0] }",
                "- { id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0], \
                 harness: { length_m: 5.0 } }",
            );
        let root = workspace(
            "harness-ok",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", &vehicle),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        assert_eq!(Workspace::load(&root).unwrap().validate(), vec![]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_empty_or_negative_harness_block_is_caught() {
        use WorkspaceValidationError as E;
        let vehicle = GOOD_VEHICLE
            .replace(
                "- { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas }",
                "- { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas, \
                 harness: { length_m: -1.0 } }",
            )
            .replace(
                "- { id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0] }",
                "- { id: trunk-hpc, endpoints: [gw1:eth2, hpc1:eth0], harness: {} }",
            );
        let root = workspace(
            "harness-bad",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", &vehicle),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        let errs = Workspace::load(&root).unwrap().validate();
        assert!(
            errs.iter().any(
                |e| matches!(e, E::HarnessNegative { field: "length_m", link, .. } if link == "trunk-fl")
            ),
            "{errs:?}"
        );
        assert!(
            errs.iter()
                .any(|e| matches!(e, E::HarnessEmpty { link, .. } if link == "trunk-hpc")),
            "{errs:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dangling_target_ids_and_endpoints_are_caught() {
        let baseline = GOOD_BASELINE.replace(
            "target_ids: [fn-hpc1, fn-gw1]",
            "target_ids: [fn-hpc99, fn-gw1]",
        );
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
            WorkspaceValidationError::UnknownTarget { target, .. } if target == "fn-hpc99"
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

    /// exp-002's shape: three arms on one VLAN, told apart only by address. The bare form stays
    /// valid alongside it — most attachments need no fixed address, and every existing workspace
    /// uses it.
    #[test]
    fn a_link_endpoint_can_carry_an_address_without_the_bare_form_going_away() {
        let vehicle = GOOD_VEHICLE.replace(
            "  - { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas }",
            "  - id: trunk-fl\n    endpoints:\n      - gw1:eth0\n      - { node: zonefl, port: eth0, ip: 10.101.0.11/24 }\n    slice: adas",
        );
        let root = workspace(
            "addressed",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", &vehicle),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        let ws = Workspace::load(&root).expect("parses");
        assert_eq!(ws.validate(), vec![]);
        let eps = &ws.vehicles[0].links[0].endpoints;
        assert_eq!(eps[0].node(), "gw1");
        assert_eq!(eps[0].ip(), None, "the bare form carries no address");
        assert_eq!(eps[1].node(), "zonefl");
        assert_eq!(eps[1].port(), Some("eth0"));
        assert_eq!(eps[1].ip(), Some("10.101.0.11/24"));
        fs::remove_dir_all(&root).ok();
    }

    /// An address that only fails at `ip addr add` fails halfway through provisioning, with half
    /// a topology up. It is a shape error, so it is caught where shape errors are caught.
    #[test]
    fn an_address_without_a_prefix_length_is_caught() {
        let vehicle = GOOD_VEHICLE.replace(
            "endpoints: [gw1:eth0, zonefl:eth0]",
            "endpoints: [gw1:eth0, { node: zonefl, ip: 10.101.0.11 }]",
        );
        let root = workspace(
            "badaddr",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", &vehicle),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        let errs = Workspace::load(&root).unwrap().validate();
        assert!(
            errs.iter()
                .any(|e| matches!(e, WorkspaceValidationError::MalformedAddress { ip, .. } if ip == "10.101.0.11")),
            "{errs:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// The reason the endpoint form is deserialized by hand: `#[serde(untagged)]` reports every
    /// failure as "data did not match any variant", so a misspelled key would be diagnosed as a
    /// malformed *string* and send the author looking at the wrong thing.
    #[test]
    fn a_misspelled_key_in_the_object_form_names_the_key() {
        let err = serde_yaml_ng::from_str::<LinkDoc>(
            "id: l\nendpoints: [{ node: zonefl, adress: 10.101.0.11/24 }]\n",
        )
        .expect_err("unknown field is rejected");
        let msg = err.to_string();
        assert!(msg.contains("adress"), "{msg}");
        assert!(msg.contains("node"), "{msg}");
    }

    #[test]
    fn target_ids_accepts_a_scalar_or_a_list() {
        let one: PartEntry = serde_yaml_ng::from_str("ref: ./x\ntarget_ids: fn-hpc1").unwrap();
        let many: PartEntry =
            serde_yaml_ng::from_str("ref: ./x\ntarget_ids: [fn-a, fn-b]").unwrap();
        assert_eq!(one.target_ids.unwrap().as_slice(), vec!["fn-hpc1"]);
        assert_eq!(many.target_ids.unwrap().as_slice(), vec!["fn-a", "fn-b"]);
    }

    /// Every reference is by id and never by path, so the file boundary carries no meaning —
    /// a workspace collapsed into one `---`-separated file has to load, and validate, exactly
    /// as the folder does.
    #[test]
    fn a_whole_workspace_can_live_in_one_file() {
        let one_file = format!("{GOOD_EXPERIMENT}---{GOOD_VEHICLE}---{GOOD_BASELINE}");
        let inline = Workspace::from_yaml(&one_file).expect("parses");
        assert_eq!(inline.validate(), vec![], "{:?}", inline.validate());

        let root = workspace(
            "onefile",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", GOOD_VEHICLE),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        let folder = Workspace::load(&root).unwrap();
        // The same workspace either way: same documents, same ids, same verdict.
        assert_eq!(inline.experiments.len(), folder.experiments.len());
        assert_eq!(inline.vehicles[0].id, folder.vehicles[0].id);
        assert_eq!(
            inline.baselines[0].parts.len(),
            folder.baselines[0].parts.len()
        );
        assert_eq!(inline.experiments_raw, folder.experiments_raw);
        fs::remove_dir_all(&root).ok();
    }

    /// A directory whose files each hold several documents is the same case — nothing about the
    /// two forms may be exclusive.
    #[test]
    fn documents_can_be_grouped_across_files_however_the_author_likes() {
        let root = workspace(
            "grouped",
            &[
                (
                    "everything.yaml",
                    &format!("{GOOD_EXPERIMENT}---{GOOD_VEHICLE}"),
                ),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        assert_eq!(Workspace::load(&root).unwrap().validate(), vec![]);
        fs::remove_dir_all(&root).ok();
    }

    /// The sandbox layout, locked in. A workspace owns `parts/`, `assets/`, `tools/`,
    /// `scripts/` and `results/`, and none of them are workspace *documents* — a `part.yaml`
    /// inside `parts/` describes a loadable, not an experiment, and absorbing it would make a
    /// sandbox invalid the moment it carried its own parts.
    #[test]
    fn sandbox_subdirectories_are_content_not_documents() {
        let root = workspace(
            "sandbox",
            &[
                ("main.yaml", GOOD_EXPERIMENT),
                ("car1.yaml", GOOD_VEHICLE),
                ("parts.yaml", GOOD_BASELINE),
            ],
        );
        for (dir, name, body) in [
            (
                "parts",
                "part.yaml",
                "kind: experiment
id: a-loadable
",
            ),
            (
                "assets",
                "scenario.yaml",
                "kind: vehicle
id: not-a-vehicle
",
            ),
            (
                "tools",
                "config.yaml",
                "kind: baseline
id: not-a-baseline
",
            ),
            (
                "scripts",
                "helper.yaml",
                "kind: experiment
id: stray
",
            ),
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
            fs::write(root.join(dir).join(name), body).unwrap();
        }
        let ws = Workspace::load(&root).unwrap();
        assert_eq!(
            ws.experiments.len(),
            1,
            "a subdirectory joined the workspace"
        );
        assert_eq!(ws.vehicles.len(), 1);
        assert_eq!(ws.validate(), vec![], "{:?}", ws.validate());
        fs::remove_dir_all(&root).ok();
    }

    /// The workspace identity rule (`ARCHITECTURE.md` §13), pinned as a **shared test vector**:
    /// `vxtelemetry` implements the same rule without depending on this crate, and asserts the
    /// same hash over the same fixture. Two implementations of one rule stay honest by agreeing
    /// on an answer, not by sharing code.
    ///
    /// If this value changes, every archive's provenance changes with it — so it changes only
    /// when the rule is deliberately revised, and both sides move together.
    #[test]
    fn the_workspace_identity_is_a_pinned_shared_rule() {
        let root = workspace(
            "identity",
            &[("main.yaml", "kind: experiment\nid: id-test\n")],
        );
        fs::create_dir_all(root.join("parts")).unwrap();
        fs::write(root.join("parts/local-thing"), "payload\n").unwrap();
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::write(root.join("scripts/run.sh"), "#!/usr/bin/env bash\n").unwrap();

        let before = Workspace::content_sha256(&root).unwrap();
        assert_eq!(
            before, "f095a9facfabea97828ac5ee48a0da5516bfe036b546f612bcf182856db0efac",
            "the identity rule changed — vxtelemetry must move with it"
        );

        // A local part is part of what ran: changing one is a different experiment.
        fs::write(root.join("parts/local-thing"), "different\n").unwrap();
        assert_ne!(Workspace::content_sha256(&root).unwrap(), before);
        fs::write(root.join("parts/local-thing"), "payload\n").unwrap();

        // Results are outputs, not inputs — an archive appearing must not restate the identity
        // of the run that produced it.
        fs::create_dir_all(root.join("results")).unwrap();
        fs::write(root.join("results/rep1.zip"), "not an input").unwrap();
        assert_eq!(Workspace::content_sha256(&root).unwrap(), before);

        // Renaming a file is a change: the path is hashed alongside the bytes.
        fs::rename(root.join("parts/local-thing"), root.join("parts/renamed")).unwrap();
        assert_ne!(Workspace::content_sha256(&root).unwrap(), before);
        fs::remove_dir_all(&root).ok();
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
