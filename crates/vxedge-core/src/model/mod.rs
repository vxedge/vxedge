//! The vxedge domain model — the in-code single source of truth the planners consume.
//!
//! This replaces `ExperimentManifest` (§6.1) as the planners' input. §6.1 is being rejected, not
//! demoted: it has no interaction layer, so a workspace's messaging topology can be modelled and
//! validated but never executed, and no amount of lowering fixes a concept the target schema
//! does not have.
//!
//! # The ontology, and what it fixes
//!
//! §6.0's `composition: {zones, compute, gateway}` groups devices by what they are, and calls one
//! group "zones" — but **a zone is a place, not a device**. That is the same conflation §6.0 was
//! created to fix in `ZoneRole`, surviving one level down. Here they are separate:
//!
//! - [`Zone`] — a *region* of the entity. Front-left, cabin, nacelle-2, an ATA zone. A named
//!   region rather than an enum, which is what lets a non-road entity have zones at all.
//! - [`Device`] — a physical or virtual unit *located in* a zone, carrying a [`DeviceRole`].
//! - [`Function`] — the unit of behaviour, deployed *onto* a device. AUTOSAR calls this a
//!   software component; the separation of function from the ECU it runs on is the whole point
//!   of the Virtual Function Bus, and it is what makes a [`Channel`] expressible.
//! - [`Part`] — the loadable that implements a function (vxparts, addressed by VXPN).
//!
//! # Link versus channel
//!
//! The distinction is load-bearing and has two strong precedents.
//!
//! - A [`Link`] is **structural connectivity**: a path bytes can physically take. A wire, a VLAN
//!   trunk, a CAN bus, a 5G bearer. `ARCHITECTURE.md` §7 states it directly — the manifest's
//!   links section *is* the wiring harness. That matters beyond bookkeeping: zonal E/E is
//!   justified by harness reduction, so a model that cannot express harness cannot express the
//!   main claimed benefit of the architecture under study.
//! - A [`Channel`] is a **logical information flow** between functions: a DDS topic, a SOME/IP
//!   service, a CAN frame id. It rides one or more links via `carried_by`.
//!
//! AUTOSAR's VFB is deliberately deployment-independent, with a separate step mapping it onto
//! communication clusters — channel : link :: VFB : comm-cluster. ARINC 664's virtual links are
//! the same idea in avionics, and are the reason this generalises off-road.
//!
//! Binding the two is what makes latency **attributable per topic per slice** rather than only
//! per link, which is the measurement H1 and H2 actually need.
//!
//! # Deliberately not serde types
//!
//! Nothing here derives `Deserialize`. §6.0's YAML is shaped for a human editing a file;
//! the model is shaped for the planners. Keeping them separate means each can change for its own
//! reasons — the mistake §6.1 made was being both at once.

use std::collections::BTreeMap;

pub mod build;

pub use build::{ModelError, build};

/// A software-defined mobility entity. A car today; the class axis is what keeps an eVTOL,
/// vessel or aircraft expressible without re-deriving the model.
#[derive(Debug, Clone)]
pub struct Entity {
    pub id: String,
    pub class: EntityClass,
    /// Free-form within a class (`car`, `truck`, `quadrotor`, …). Not an enum: the taxonomy
    /// lives in `vxparts/schema/taxonomy.yaml`, and duplicating it here would create a second
    /// place to edit that can silently disagree.
    pub subtype: Option<String>,
    pub zones: BTreeMap<String, Zone>,
    pub devices: BTreeMap<String, Device>,
    pub functions: BTreeMap<String, Function>,
    pub links: Vec<Link>,
    pub channels: Vec<Channel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityClass {
    Road,
    Air,
    Marine,
    Rail,
    /// Anything the taxonomy grows later. Kept open so a new class is a data change rather than
    /// a code change — the same reason `subtype` is a string.
    Other(String),
}

/// A region of the entity. Deliberately *not* an enum of `front-left`/`rear-right`: those are
/// road-vehicle conventions, and hard-coding them is exactly what would block the mobility
/// entities this model is meant to reach.
#[derive(Debug, Clone)]
pub struct Zone {
    pub id: String,
    pub name: Option<String>,
    /// Optional pointer into an external zoning standard (an ATA zone number, a compartment
    /// designation). Carried, never interpreted.
    pub reference: Option<String>,
}

/// A physical or virtual unit located in a zone.
#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub role: DeviceRole,
    /// Which [`Zone`] it sits in. Optional because a gateway or central computer is often
    /// modelled as belonging to the entity rather than to any one region.
    pub zone: Option<String>,
    pub resources: Option<Resources>,
    /// How the device is realised in the testbed — container, microvm, full VM. This is H3's
    /// experimental variable, and it belongs on the device rather than on the part precisely so
    /// an arm can vary it while holding the software identical.
    pub backend: Option<Backend>,
    /// Signal bindings for sensors and actuators (`ARCHITECTURE.md` §6.2): a VSS path, and a
    /// DBC for CAN-attached devices.
    pub signals: Vec<SignalBinding>,
    /// CPUs this device's runtime is pinned to. A determinism control, not an allocation —
    /// which is why it sits here rather than in [`Resources`].
    pub pinned_cpus: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRole {
    /// Aggregates a zone's sensors/actuators onto the backbone — the role that makes the
    /// architecture zonal rather than domain-oriented.
    ZoneController,
    /// Central high-performance compute (the AD/cockpit/motion computer).
    Hpc,
    /// Uplink into vxgateway; the vehicle's edge.
    Gateway,
    /// A conventional single-function ECU.
    Ecu,
    Sensor,
    Actuator,
    /// A pure forwarding element (backbone switch).
    Switch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Container,
    QemuMicrovm,
    Libvirt,
}

/// What a device is *allocated*. Distinct from a part's `requires`, which is what the software
/// *needs* — validating one against the other is what catches an under-provisioned host before
/// a run rather than during one.
#[derive(Debug, Clone, Default)]
pub struct Resources {
    pub cpu: Option<u32>,
    pub memory_gb: Option<u32>,
    pub gpu: Option<String>,
}

/// A sensor's or actuator's signal identity.
#[derive(Debug, Clone)]
pub struct SignalBinding {
    pub id: String,
    /// COVESA VSS path, the signal plane's vocabulary (`Vehicle.Camera.FL`).
    pub vss: Option<String>,
    /// CAN frame definitions for restbus simulation.
    pub dbc: Option<String>,
}

/// The unit of behaviour, deployed onto a device.
///
/// Separating this from [`Device`] is what makes [`Channel`] endpoints meaningful: a topic flows
/// between *functions*, and which devices those happen to sit on is a deployment decision that a
/// study should be able to vary without rewriting the interaction topology.
#[derive(Debug, Clone)]
pub struct Function {
    pub id: String,
    pub deployed_on: String,
    pub part: Option<Part>,
    /// Automotive safety integrity level, when declared. Carried so freedom-from-interference
    /// experiments (exp-004) have somewhere to state the property under test.
    pub asil: Option<Asil>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asil {
    Qm,
    A,
    B,
    C,
    D,
}

/// A loadable, addressed either locally or by a digest-pinned registry reference.
#[derive(Debug, Clone)]
pub struct Part {
    pub reference: String,
    pub digest: Option<String>,
    /// `local/…`, a relative path, or an absolute path — none of which name a registry artifact,
    /// so none of which can be digest-pinned.
    pub local: bool,
}

/// Structural connectivity: the harness.
#[derive(Debug, Clone)]
pub struct Link {
    pub id: String,
    pub endpoints: Vec<Endpoint>,
    pub kind: LinkKind,
    /// The network slice this link's traffic belongs to, when it crosses vxgateway.
    pub slice: Option<String>,
    pub harness: Option<Harness>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkKind {
    Ethernet,
    VlanTrunk,
    Can,
    Vxlan,
    Wireguard,
    Direct,
}

/// Physical characteristics of a harness segment.
///
/// Not decoration: zonal architectures are justified by harness reduction, and §7 already makes
/// harness topology manifest data so it can be an experimental variable. Without these fields
/// the model cannot express the dependent variable of the zonal-versus-domain comparison.
#[derive(Debug, Clone, Default)]
pub struct Harness {
    /// `100BASE-T1`, `1000BASE-T1`, `CAN-FD`, …
    pub medium: Option<String>,
    pub length_m: Option<f64>,
    pub weight_g: Option<f64>,
}

/// One end of a link or channel: a device (or function) and an optional port.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Endpoint {
    pub node: String,
    pub port: Option<String>,
    /// Address (CIDR) the zone backend configures on this attachment. Only meaningful on a
    /// link; a channel endpoint leaves it unset. §6.0's `endpoints: [a:eth0, b:eth0]` string
    /// form cannot express it yet — the §6.1 adapter populates it, and the schema change that
    /// finishes 5a gives authors an object form that can.
    pub ip_cidr: Option<String>,
}

/// A logical information flow between functions.
#[derive(Debug, Clone)]
pub struct Channel {
    pub id: String,
    pub from: Endpoint,
    pub to: Vec<Endpoint>,
    pub protocol: Protocol,
    /// Per-channel QoS, distinct from the *slice's* QoS. A DDS topic has its own deadline and
    /// reliability, and without this there is no way to separate "this topic missed its
    /// deadline" from "the slice was congested" — which is half of what H1 and H2 measure.
    pub qos: Option<ChannelQos>,
    /// The links this channel rides. Validating that the endpoints are actually reachable over
    /// them is what stops per-topic-per-slice attribution resting on an unchecked assertion.
    pub carried_by: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Dds,
    SomeIp,
    Zenoh,
    Can,
    Udp,
    Tcp,
}

#[derive(Debug, Clone, Default)]
pub struct ChannelQos {
    pub deadline_ms: Option<u32>,
    pub reliability: Option<Reliability>,
    pub rate_hz: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reliability {
    BestEffort,
    Reliable,
}

/// A network partition under test.
#[derive(Debug, Clone)]
pub struct Slice {
    pub id: String,
    pub mode: SliceMode,
    pub vlan: Option<u16>,
    pub s_nssai: Option<SNssai>,
    pub qos: Option<SliceQos>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceMode {
    Vlan,
    SNssai,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SNssai {
    pub sst: u8,
    pub sd: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SliceQos {
    pub min_rate_mbps: Option<u32>,
    pub max_rate_mbps: Option<u32>,
    pub priority: Option<u8>,
}

/// The whole experiment: what is under test, on which entity, with which software.
#[derive(Debug, Clone)]
pub struct Experiment {
    pub id: String,
    pub entities: Vec<Entity>,
    pub slices: Vec<Slice>,
    /// Aggregate rate of the shared uplink bottleneck, in bits per second.
    ///
    /// This is where slice contention is actually measured, so dropping it silently would change
    /// every number an experiment produces while everything still validated and ran — exp-001
    /// declares 500 mbit and would have planned at the 1000 mbit default.
    pub uplink_bandwidth_bps: Option<u64>,
    pub traffic: Vec<TrafficProfile>,
    pub method: Method,
    /// Everything §6.0 carries that the model does not yet interpret — telemetry, results,
    /// services. Kept as raw YAML rather than dropped, so the daemon can still act on it while
    /// the model grows to cover it. Each block that graduates to a typed field leaves here.
    pub passthrough: BTreeMap<String, serde_yaml_ng::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Method {
    pub duration_s: Option<u32>,
    pub repetitions: Option<u32>,
    pub warmup_s: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct TrafficProfile {
    pub id: String,
    /// A device id, or a symbolic endpoint (`internet`, `carla`) that names no device.
    pub source: TrafficEndpoint,
    pub target: TrafficEndpoint,
    pub pps: Option<u32>,
    pub bandwidth_mbps: Option<u32>,
    pub payload_bytes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrafficEndpoint {
    Device(String),
    /// The host-side reflector beyond the gateway.
    Internet,
    Simulator(String),
}

impl Entity {
    /// Devices in a stable order, so anything derived from the traversal (gateway endpoint
    /// numbering, and therefore interface names campaign scripts address directly) is
    /// reproducible across runs.
    pub fn devices_in_order(&self) -> impl Iterator<Item = &Device> {
        self.devices.values()
    }

    pub fn device(&self, id: &str) -> Option<&Device> {
        self.devices.get(id)
    }

    /// Every link touching `device`, with the port it attaches on.
    pub fn links_for<'a>(
        &'a self,
        device: &'a str,
    ) -> impl Iterator<Item = (&'a Link, &'a Endpoint)> {
        self.links.iter().flat_map(move |l| {
            l.endpoints
                .iter()
                .filter(move |e| e.node == device)
                .map(move |e| (l, e))
        })
    }

    /// Which device a function runs on.
    pub fn host_of(&self, function: &str) -> Option<&Device> {
        self.functions
            .get(function)
            .and_then(|f| self.devices.get(&f.deployed_on))
    }
}

/// A violated model invariant.
///
/// The model introduces id spaces §6.0 did not have — zones, devices and functions are now
/// distinct kinds, where §6.0 had one flat "component" namespace. New namespaces mean new ways
/// to collide, and every reference between them is a new way to dangle. Checking that here
/// rather than trusting the builder means a hand-constructed model (a future adapter, a test
/// fixture, an alternative frontend) cannot skip it.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvariantError {
    #[error(
        "entity `{entity}`: `{id}` is both a zone and a device — a reference to it would be ambiguous"
    )]
    ZoneDeviceCollision { entity: String, id: String },
    #[error(
        "device id `{id}` is used by more than one entity; device ids are unique across the experiment because parts and traffic profiles address them flatly"
    )]
    DuplicateDevice { id: String },
    #[error(
        "entity `{entity}`: device `{device}` is located in zone `{zone}`, which the entity does not declare"
    )]
    UnknownZone {
        entity: String,
        device: String,
        zone: String,
    },
    #[error(
        "entity `{entity}`: function `{function}` is deployed on `{device}`, which is not a device of this entity"
    )]
    UnknownDeployment {
        entity: String,
        function: String,
        device: String,
    },
    #[error("entity `{entity}`: link `{link}` has endpoint `{endpoint}`, which is not a device")]
    UnknownLinkEndpoint {
        entity: String,
        link: String,
        endpoint: String,
    },
    #[error(
        "entity `{entity}`: channel `{channel}` has endpoint `{endpoint}`, which is not a function — channels connect functions, not devices"
    )]
    UnknownChannelEndpoint {
        entity: String,
        channel: String,
        endpoint: String,
    },
    #[error(
        "entity `{entity}`: channel `{channel}` says it is carried by link `{link}`, which does not exist"
    )]
    UnknownCarriedByLink {
        entity: String,
        channel: String,
        link: String,
    },
    #[error("entity `{entity}`: link id `{id}` is declared more than once")]
    DuplicateLink { entity: String, id: String },
    #[error("entity `{entity}`: channel id `{id}` is declared more than once")]
    DuplicateChannel { entity: String, id: String },
    #[error("slice id `{id}` is declared more than once")]
    DuplicateSlice { id: String },
    #[error("traffic profile `{profile}` names device `{device}`, which does not exist")]
    UnknownTrafficDevice { profile: String, device: String },
    #[error("link `{link}` binds slice `{slice}`, which the experiment does not define")]
    UndefinedSlice { link: String, slice: String },
}

impl Experiment {
    /// Check every model invariant, reporting all violations rather than the first.
    ///
    /// A fix-one-rerun loop is the wrong shape for a validator an engineer runs while authoring;
    /// the workspace validator already reports everything at once and this matches it.
    pub fn check(&self) -> Vec<InvariantError> {
        use InvariantError as E;
        let mut errors = Vec::new();

        let mut seen_slices: BTreeMap<&str, ()> = BTreeMap::new();
        for s in &self.slices {
            if seen_slices.insert(s.id.as_str(), ()).is_some() {
                errors.push(E::DuplicateSlice { id: s.id.clone() });
            }
        }
        let slice_ids: Vec<&str> = self.slices.iter().map(|s| s.id.as_str()).collect();

        // Device ids stay unique across the *whole experiment*, not merely per entity: parts
        // target them flatly (`target_ids: hpc1`) and traffic profiles name them flatly, so a
        // repeat across two entities would make both references ambiguous. This is the cost of
        // flat ids over structural paths, and it has to be an actual check.
        let mut device_owner: BTreeMap<&str, &str> = BTreeMap::new();

        for entity in &self.entities {
            for (id, device) in &self.devices_of(entity) {
                if device_owner.insert(id, entity.id.as_str()).is_some() {
                    errors.push(E::DuplicateDevice {
                        id: (*id).to_string(),
                    });
                }
                if entity.zones.contains_key(*id) {
                    errors.push(E::ZoneDeviceCollision {
                        entity: entity.id.clone(),
                        id: (*id).to_string(),
                    });
                }
                if let Some(z) = &device.zone
                    && !entity.zones.contains_key(z)
                {
                    errors.push(E::UnknownZone {
                        entity: entity.id.clone(),
                        device: (*id).to_string(),
                        zone: z.clone(),
                    });
                }
            }

            for (id, f) in &entity.functions {
                if !entity.devices.contains_key(&f.deployed_on) {
                    errors.push(E::UnknownDeployment {
                        entity: entity.id.clone(),
                        function: id.clone(),
                        device: f.deployed_on.clone(),
                    });
                }
            }

            let mut link_ids: BTreeMap<&str, ()> = BTreeMap::new();
            for l in &entity.links {
                if link_ids.insert(l.id.as_str(), ()).is_some() {
                    errors.push(E::DuplicateLink {
                        entity: entity.id.clone(),
                        id: l.id.clone(),
                    });
                }
                for ep in &l.endpoints {
                    if !entity.devices.contains_key(&ep.node) {
                        errors.push(E::UnknownLinkEndpoint {
                            entity: entity.id.clone(),
                            link: l.id.clone(),
                            endpoint: ep.node.clone(),
                        });
                    }
                }
                // The cross-file join: an entity names a slice, the experiment must define it.
                if let Some(slice) = &l.slice
                    && !slice_ids.contains(&slice.as_str())
                {
                    errors.push(E::UndefinedSlice {
                        link: l.id.clone(),
                        slice: slice.clone(),
                    });
                }
            }

            let mut channel_ids: BTreeMap<&str, ()> = BTreeMap::new();
            for c in &entity.channels {
                if channel_ids.insert(c.id.as_str(), ()).is_some() {
                    errors.push(E::DuplicateChannel {
                        entity: entity.id.clone(),
                        id: c.id.clone(),
                    });
                }
                // Channels connect *functions*. That is the distinction the model exists to
                // make, so it has to be enforced rather than merely documented.
                for ep in std::iter::once(&c.from).chain(c.to.iter()) {
                    if !entity.functions.contains_key(&ep.node) {
                        errors.push(E::UnknownChannelEndpoint {
                            entity: entity.id.clone(),
                            channel: c.id.clone(),
                            endpoint: ep.node.clone(),
                        });
                    }
                }
                for link in &c.carried_by {
                    if !link_ids.contains_key(link.as_str()) {
                        errors.push(E::UnknownCarriedByLink {
                            entity: entity.id.clone(),
                            channel: c.id.clone(),
                            link: link.clone(),
                        });
                    }
                }
            }
        }

        for t in &self.traffic {
            for ep in [&t.source, &t.target] {
                if let TrafficEndpoint::Device(d) = ep
                    && !device_owner.contains_key(d.as_str())
                {
                    errors.push(E::UnknownTrafficDevice {
                        profile: t.id.clone(),
                        device: d.clone(),
                    });
                }
            }
        }

        errors
    }

    fn devices_of<'a>(&self, entity: &'a Entity) -> Vec<(&'a str, &'a Device)> {
        entity
            .devices
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    }
}
