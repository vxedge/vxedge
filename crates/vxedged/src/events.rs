//! In-process event bus: every state transition and audit-worthy action is
//! published here; `EventService.Subscribe` streams it to clients.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;
use vxedge_api::v1::{Event, ResourceKind};

const BUS_CAPACITY: usize = 256;

/// The events this daemon emits.
///
/// A closed set rather than a free `&str`, so the vocabulary is discoverable in one place and a
/// typo is a compile error rather than an event nobody is subscribed to. See the `event-driven`
/// skill for the rules these follow; the two that shape this list:
///
/// - **An event is a domain fact that already happened**, about a named resource. Past tense.
///   Job *progress* is not an event — it belongs to that job's watch channel, and putting it
///   here would turn a shared bus into one operation's log.
/// - **The bus is not the audit log.** It is fire-and-forget and lossy by construction; a slow
///   subscriber skips rather than blocking provisioning. Anything that must survive goes to
///   `audit.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A job changed state. The one pre-existing family, kept because `JobService.Watch` is
    /// per-job and a subscriber watching the whole daemon still wants to see jobs move.
    JobState,
    /// The bridge, uplink and endpoints a `GatewayPlan` describes are on the host.
    GatewayUp,
    /// The gateway's objects have been removed.
    GatewayDown,
    /// One slice's VLAN and HTB class are applied on the trunk.
    SliceApplied,
    /// A device is running under its backend and attached to its port.
    ZoneUp,
    /// A device has been stopped and its runtime state cleaned up.
    ZoneDown,
    /// A compute (hpc-role) device is running under its backend and attached to its port.
    /// Distinct from `ZoneUp` so a subscriber can tell "central compute came up" from "an
    /// ECU came up" without inspecting the device — `ComputeService` and `ZoneService` share
    /// the same lifecycle underneath, but the event vocabulary follows the RPC surface, which
    /// follows the role an OEM engineer actually asked about.
    ComputeUp,
    /// A compute device has been stopped and its runtime state cleaned up.
    ComputeDown,
    /// A service process was spawned and pidfile-tracked.
    ServiceUp,
    /// A service process was stopped.
    ServiceDown,
    /// The signal plane joined its device's container group.
    SignalPlaneUp,
    /// The signal plane left its device's container group.
    SignalPlaneDown,
    /// A run began: the first `up` of a workspace brought its gateway up. Its resource is the run.
    RunUp,
    /// A run ended: its gateway went down.
    RunDown,
    /// A traffic profile started driving load.
    TrafficStarted,
    /// A traffic profile stopped.
    TrafficStopped,
    /// A `network.nos_check` instance was booted (via `NosCheckService.Up`).
    NosCheckUp,
    /// A `network.nos_check` instance was torn down.
    NosCheckDown,
    /// `NosCheckService.Verify` completed a config-parity check (pass or fail — the fact is
    /// "a check ran," not "it passed"; the outcome is in the event's own detail).
    NosCheckVerified,
    // Deliberately absent: a `fiveg.ue-registered` event. It is an obvious next member, but
    // `fiveg::lifecycle` has no point today where registration is *observed* — it is inferred
    // from a successful `ue_up`, which is not the same fact. Adding the variant before the
    // observation exists would be guessing at a shape, which the event-driven skill rules out.
}

impl EventKind {
    /// The wire string. Dotted, `<resource>.<fact>`, so a subscriber can prefix-match a family
    /// without parsing.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::JobState => "job.state",
            EventKind::GatewayUp => "gateway.up",
            EventKind::GatewayDown => "gateway.down",
            EventKind::SliceApplied => "gateway.slice-applied",
            EventKind::ZoneUp => "zone.up",
            EventKind::ZoneDown => "zone.down",
            EventKind::ComputeUp => "compute.up",
            EventKind::ComputeDown => "compute.down",
            EventKind::ServiceUp => "service.up",
            EventKind::ServiceDown => "service.down",
            EventKind::SignalPlaneUp => "signal-plane.up",
            EventKind::SignalPlaneDown => "signal-plane.down",
            EventKind::RunUp => "run.up",
            EventKind::RunDown => "run.down",
            EventKind::TrafficStarted => "traffic.started",
            EventKind::TrafficStopped => "traffic.stopped",
            EventKind::NosCheckUp => "nos-check.up",
            EventKind::NosCheckDown => "nos-check.down",
            EventKind::NosCheckVerified => "nos-check.verified",
        }
    }

    /// What kind of thing this event is about. Never `Unspecified`: an event whose resource kind
    /// is unset cannot be filtered, which is what made the original four unusable — a subscriber
    /// could not ask "everything about `node-cont-01`" and get an answer.
    pub fn resource_kind(self) -> ResourceKind {
        match self {
            EventKind::JobState => ResourceKind::Unspecified,
            EventKind::GatewayUp | EventKind::GatewayDown | EventKind::SliceApplied => {
                ResourceKind::Gateway
            }
            EventKind::ZoneUp | EventKind::ZoneDown => ResourceKind::Zone,
            EventKind::ComputeUp | EventKind::ComputeDown => ResourceKind::Compute,
            EventKind::ServiceUp | EventKind::ServiceDown => ResourceKind::Service,
            EventKind::SignalPlaneUp | EventKind::SignalPlaneDown => ResourceKind::SignalPlane,
            // A run is a vehicle's unless said otherwise: `emit_run` names the kind it really is.
            EventKind::RunUp | EventKind::RunDown => ResourceKind::Vehicle,
            EventKind::TrafficStarted | EventKind::TrafficStopped => ResourceKind::Traffic,
            EventKind::NosCheckUp | EventKind::NosCheckDown | EventKind::NosCheckVerified => {
                ResourceKind::NosCheck
            }
        }
    }
}

/// Recent events kept for `Subscribe { after_id }`. Older ones are gone, and a cursor pointing at
/// one is told so.
const HISTORY: usize = 1024;

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<(u64, Event)>,
    seq: Arc<AtomicU64>,
    boot: Arc<str>,
    history: Arc<std::sync::Mutex<VecDeque<(u64, Event)>>>,
    history_cap: usize,
    runtime_dir: Option<Arc<PathBuf>>,
}

/// Whether `kind` is one a subscriber asked for: an exact name, or a family such as `zone`, which
/// matches `zone.up` and `zone.down`.
pub fn wanted(kinds: &[String], kind: &str) -> bool {
    kinds.is_empty()
        || kinds.iter().any(|k| {
            let family = k.trim_end_matches('.');
            kind == k
                || kind
                    .strip_prefix(family)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
}

impl EventBus {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::build(BUS_CAPACITY, HISTORY, None)
    }

    #[cfg(test)]
    pub fn sized(capacity: usize, history: usize) -> Self {
        Self::build(capacity, history, None)
    }

    /// A bus that stamps each event with the run active in `runtime_dir`.
    pub fn with_runtime_dir(runtime_dir: PathBuf) -> Self {
        Self::build(BUS_CAPACITY, HISTORY, Some(runtime_dir))
    }

    fn build(capacity: usize, history_cap: usize, runtime_dir: Option<PathBuf>) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            seq: Arc::new(AtomicU64::new(0)),
            boot: crate::jobs::boot_id().into(),
            history: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            history_cap,
            runtime_dir: runtime_dir.map(Arc::new),
        }
    }

    /// Publish a domain fact.
    ///
    /// Call this **after** the state change is durable, never before: a subscriber that observes
    /// a transition which then fails has been told something untrue. Never call it on a hot
    /// path — measurement code must not perturb the measurement, and the bus is shared.
    pub fn emit(&self, kind: EventKind, resource_vxid: &str, detail: &str) {
        let run_id = self
            .runtime_dir
            .as_deref()
            .and_then(|d| crate::services::active_run(d))
            .map(|r| r.run_id().to_string())
            .unwrap_or_default();
        self.publish(kind, kind.resource_kind(), resource_vxid, detail, run_id);
    }

    /// A run began or ended. The run is the resource, and `resource_kind` says whether it is a
    /// vehicle's or an experiment's.
    pub fn emit_run(
        &self,
        kind: EventKind,
        resource_kind: ResourceKind,
        run_id: &str,
        detail: &str,
    ) {
        self.publish(kind, resource_kind, run_id, detail, run_id.to_string());
    }

    fn publish(
        &self,
        kind: EventKind,
        resource_kind: ResourceKind,
        resource_vxid: &str,
        detail: &str,
        run_id: String,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let event = Event {
            id: format!("evt-{}-{seq:08x}", self.boot),
            ts_unix_ms: crate::jobs::now_ms(),
            kind: kind.as_str().to_string(),
            resource_kind: resource_kind as i32,
            resource_vxid: resource_vxid.to_string(),
            detail: detail.to_string(),
            run_id,
        };
        {
            let mut history = self.history.lock().expect("event history poisoned");
            history.push_back((seq, event.clone()));
            while history.len() > self.history_cap {
                history.pop_front();
            }
        }
        // No subscribers is fine; the bus is fire-and-forget. It must never be able to block
        // provisioning — an event that can is a dashboard made into a dependency of the testbed.
        let _ = self.tx.send((seq, event));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<(u64, Event)> {
        self.tx.subscribe()
    }

    /// The events after `id`, and `id`'s own sequence number, if the history still holds it.
    pub fn after(&self, id: &str) -> Option<(u64, Vec<Event>)> {
        let history = self.history.lock().expect("event history poisoned");
        let at = history.iter().position(|(_, e)| e.id == id)?;
        let floor = history[at].0;
        Some((
            floor,
            history
                .iter()
                .skip(at + 1)
                .map(|(_, e)| e.clone())
                .collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every event must be filterable. The four the bus started with all carried
    /// `ResourceKind::Unspecified`, so a subscriber could not ask "everything about
    /// `node-cont-01`" and get an answer — which is why the bus was plumbed but useless.
    #[test]
    fn every_domain_event_names_the_kind_of_thing_it_is_about() {
        for kind in [
            EventKind::GatewayUp,
            EventKind::GatewayDown,
            EventKind::SliceApplied,
            EventKind::ZoneUp,
            EventKind::ZoneDown,
            EventKind::ComputeUp,
            EventKind::ComputeDown,
            EventKind::ServiceUp,
            EventKind::ServiceDown,
            EventKind::SignalPlaneUp,
            EventKind::SignalPlaneDown,
            EventKind::TrafficStarted,
            EventKind::TrafficStopped,
        ] {
            assert_ne!(
                kind.resource_kind(),
                ResourceKind::Unspecified,
                "{} is unfilterable",
                kind.as_str()
            );
        }
        // Job state is the exception and stays one: it is about a job, not a resource, and
        // `ResourceKind` has no member for that.
        assert_eq!(
            EventKind::JobState.resource_kind(),
            ResourceKind::Unspecified
        );
    }

    /// `<resource>.<fact>`, so a subscriber can prefix-match a family without parsing.
    #[test]
    fn wire_names_are_dotted_and_past_tense() {
        for kind in [
            EventKind::JobState,
            EventKind::GatewayUp,
            EventKind::SliceApplied,
            EventKind::ZoneUp,
            EventKind::ServiceUp,
            EventKind::TrafficStarted,
        ] {
            let name = kind.as_str();
            assert!(name.contains('.'), "{name} is not <resource>.<fact>");
            assert_eq!(name, name.to_lowercase(), "{name} is not lowercase");
        }
    }

    #[tokio::test]
    async fn a_subscriber_receives_a_domain_fact_with_its_resource() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.emit(
            EventKind::ZoneUp,
            "node-cont-01",
            "Container on vz-node-c-32cc",
        );
        let (_, event) = rx.recv().await.expect("delivered");
        assert_eq!(event.kind, "zone.up");
        assert_eq!(event.resource_vxid, "node-cont-01");
        assert_eq!(event.resource_kind, ResourceKind::Zone as i32);
    }

    /// The bus is fire-and-forget: emitting with nobody listening must not fail or block, or an
    /// event becomes something that can stop provisioning.
    #[test]
    fn emitting_with_no_subscribers_is_a_no_op() {
        EventBus::new().emit(EventKind::GatewayDown, "vxgateway", "removed");
    }

    #[test]
    fn a_family_matches_its_events_and_only_them() {
        let kinds = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(wanted(&kinds(&[]), "zone.up"));
        assert!(wanted(&kinds(&["zone"]), "zone.up"));
        assert!(wanted(&kinds(&["zone."]), "zone.down"));
        assert!(wanted(&kinds(&["zone.up"]), "zone.up"));
        assert!(!wanted(&kinds(&["zone"]), "zones.up"));
        assert!(!wanted(&kinds(&["zone.up"]), "zone.down"));
        assert!(wanted(&kinds(&["signal-plane"]), "signal-plane.up"));
    }

    #[test]
    fn history_resumes_after_an_id_and_forgets_the_oldest() {
        let bus = EventBus::sized(16, 3);
        for i in 0..5 {
            bus.emit(EventKind::ZoneUp, &format!("n{i}"), "");
        }
        let ids: Vec<String> = bus
            .history
            .lock()
            .unwrap()
            .iter()
            .map(|(_, e)| e.id.clone())
            .collect();
        assert_eq!(ids.len(), 3, "bounded");
        let (_, after) = bus.after(&ids[0]).unwrap();
        let after: Vec<&str> = after.iter().map(|e| e.resource_vxid.as_str()).collect();
        assert_eq!(after, ["n3", "n4"]);
        let first_ever = ids[0].replace("00000002", "00000000");
        assert!(bus.after(&first_ever).is_none(), "aged out");
    }

    #[test]
    fn ids_carry_this_daemons_prefix() {
        let a = EventBus::new();
        let b = EventBus::new();
        a.emit(EventKind::ZoneUp, "n", "");
        b.emit(EventKind::ZoneUp, "n", "");
        let id = |bus: &EventBus| bus.history.lock().unwrap()[0].1.id.clone();
        assert_ne!(id(&a), id(&b), "two daemons never share an id");
    }
}
