//! A shared shape for "how is this thing doing right now" — the domain-level analogue of
//! vxAPI's `ResourceState`, kept deliberately separate from it: this crate never depends on the
//! generated API crate (`vxedge-api`), so nothing in the domain model can accidentally couple
//! itself to the wire protocol. `vxedged` is the only place that maps [`ObservedState`] onto
//! `vxedge_api::v1::ResourceState` — see its own `status.rs`.
//!
//! Before this module existed, every RPC handler that needed to turn a domain observation
//! (zones' `NodeState`, `GatewayObservation`, ...) into a status hand-rolled its own
//! `if state == X { Up } else { Down }` conversion and its own "is everything up" rollup —
//! `docs/TASKS.md` #16-adjacent work found this scattered, inconsistent, and in one place
//! (`ExperimentSvc::status`'s own hand-rolled `all_up` check) subtly wrong: it reported
//! `Degraded` even when *every* child was down, rather than the more honest `Down`. [`Observable`]
//! and [`aggregate`] exist so that logic is written once, correctly, and reused.

/// Where a resource stands, independent of any wire encoding. Every backend's own observation
/// type ([`crate::zones::backend::NodeState`], and any future one) implements [`Observable`] to
/// produce this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedState {
    Up,
    Down,
    Staging,
    Degraded,
    Error,
}

/// Anything in the domain model that can report its own current state. Implemented by each
/// backend's own observation type, so `vxedged`'s RPC handlers can build a `ResourceStatus`
/// (or roll several up via [`aggregate`]) the same way regardless of which backend produced the
/// observation, instead of a bespoke conversion per call site.
///
/// Deliberately narrow — just `state()`, not also a `detail()` — because a useful detail string
/// is usually richer than what a bare observation alone carries (`NodeState` doesn't know its
/// own node's zone/backend/port; the caller building a `ResourceStatus` does). Forcing every
/// implementor to also produce a detail string would either be uninformative ("up"/"down") or
/// push implementors to duplicate context the caller already has. `state()` is the part every
/// existing call site actually got wrong or repeated; that's what this replaces.
///
/// **Not yet implemented by `GatewayObservation`.** Its per-slice/impairment/ingress-filter/
/// tunnel sub-observations make its own status construction richer than one `ObservedState`,
/// and it's the code every frozen H1/H3/H4 number was measured through — retrofitting it is a
/// real, separate piece of work, not folded into this pass to avoid risking published numbers
/// for a stylistic win.
pub trait Observable {
    fn state(&self) -> ObservedState;
}

/// Rolls up a set of children's states into one parent state — the Composite pattern: a
/// composite's own state is *derived* from its children, never independently declared, so a
/// parent can't silently disagree with what's actually running underneath it.
///
/// - No children at all → [`ObservedState::Down`] — nothing declared, nothing provisioned.
/// - Every child the same state → that state (an experiment where every zone is genuinely `Up`
///   reports `Up`, not a synthetic "mostly fine").
/// - Any child [`ObservedState::Error`] → `Error` — a hard failure anywhere is a hard failure
///   for the whole.
/// - Else any child [`ObservedState::Degraded`] → `Degraded`.
/// - Else a mix of anything else (`Up` alongside `Down`/`Staging`, or `Down` alongside
///   `Staging`) → `Degraded`: reporting `Up` while part of the declared topology isn't would
///   let a campaign start measuring an incomplete run — the exact class of bug this replaces a
///   hand-rolled `all_up` check for.
pub fn aggregate(children: &[ObservedState]) -> ObservedState {
    let Some(first) = children.first() else {
        return ObservedState::Down;
    };
    if children.iter().all(|s| s == first) {
        return *first;
    }
    if children.contains(&ObservedState::Error) {
        return ObservedState::Error;
    }
    if children.contains(&ObservedState::Degraded) {
        return ObservedState::Degraded;
    }
    ObservedState::Degraded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_children_is_down() {
        assert_eq!(aggregate(&[]), ObservedState::Down);
    }

    #[test]
    fn uniform_children_keep_their_state() {
        for s in [
            ObservedState::Up,
            ObservedState::Down,
            ObservedState::Staging,
            ObservedState::Degraded,
            ObservedState::Error,
        ] {
            assert_eq!(aggregate(&[s, s, s]), s, "{s:?} did not stay uniform");
        }
    }

    #[test]
    fn any_error_wins_over_everything_else() {
        assert_eq!(
            aggregate(&[ObservedState::Up, ObservedState::Up, ObservedState::Error]),
            ObservedState::Error
        );
        assert_eq!(
            aggregate(&[ObservedState::Degraded, ObservedState::Error]),
            ObservedState::Error
        );
    }

    #[test]
    fn any_degraded_wins_over_up_down_staging() {
        assert_eq!(
            aggregate(&[
                ObservedState::Up,
                ObservedState::Degraded,
                ObservedState::Down
            ]),
            ObservedState::Degraded
        );
    }

    /// The specific case `ExperimentSvc::status`'s old hand-rolled check got right by luck and
    /// the new one gets right by construction: some up, some down is an *incomplete* topology,
    /// not a synthetic "up" or a synthetic "down" — degraded is the honest answer.
    #[test]
    fn a_mix_of_up_and_down_is_degraded_not_up_or_down() {
        assert_eq!(
            aggregate(&[ObservedState::Up, ObservedState::Down]),
            ObservedState::Degraded
        );
    }

    #[test]
    fn all_down_is_down_not_degraded() {
        // The bug this module exists to fix: reporting "degraded" when literally nothing is up
        // is less honest than "down" -- nothing partially worked, nothing worked at all.
        assert_eq!(
            aggregate(&[ObservedState::Down, ObservedState::Down]),
            ObservedState::Down
        );
    }
}
