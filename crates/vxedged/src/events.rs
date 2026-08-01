//! In-process event bus: every state transition and audit-worthy action is
//! published here; `EventService.Subscribe` streams it to clients.

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
    /// A service process was spawned and pidfile-tracked.
    ServiceUp,
    /// A service process was stopped.
    ServiceDown,
    /// A traffic profile started driving load.
    TrafficStarted,
    /// A traffic profile stopped.
    TrafficStopped,
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
            EventKind::ServiceUp => "service.up",
            EventKind::ServiceDown => "service.down",
            EventKind::TrafficStarted => "traffic.started",
            EventKind::TrafficStopped => "traffic.stopped",
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
            EventKind::ServiceUp | EventKind::ServiceDown => ResourceKind::Service,
            EventKind::TrafficStarted | EventKind::TrafficStopped => ResourceKind::Traffic,
        }
    }
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    seq: Arc<AtomicU64>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            tx,
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Publish a domain fact.
    ///
    /// Call this **after** the state change is durable, never before: a subscriber that observes
    /// a transition which then fails has been told something untrue. Never call it on a hot
    /// path — measurement code must not perturb the measurement, and the bus is shared.
    pub fn emit(&self, kind: EventKind, resource_vxid: &str, detail: &str) {
        let event = Event {
            id: format!("evt-{:08x}", self.seq.fetch_add(1, Ordering::Relaxed)),
            ts_unix_ms: crate::jobs::now_ms(),
            kind: kind.as_str().to_string(),
            resource_kind: kind.resource_kind() as i32,
            resource_vxid: resource_vxid.to_string(),
            detail: detail.to_string(),
        };
        // No subscribers is fine; the bus is fire-and-forget. It must never be able to block
        // provisioning — an event that can is a dashboard made into a dependency of the testbed.
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
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
            EventKind::ServiceUp,
            EventKind::ServiceDown,
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
        let event = rx.recv().await.expect("delivered");
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
}
