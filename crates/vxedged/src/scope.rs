//! Which run a request acts on, and where that run keeps its files (`ARCHITECTURE.md` §5.2,
//! §6.0.5).
//!
//! Several runs share one host, each in its own instance slot. A run's files live in its own
//! directory under the runtime directory, `runs/<s>/`, so its record, its manifest, its 5G and
//! gPTP config, its checks and its measurement and traffic pidfiles are never another run's.
//! Daemon-wide files (the socket, the audit log, jobs) stay at the root.
//!
//! The record on disk is the truth about which runs are up, as it was for the single run before:
//! it survives a daemon restart, and it counts only while its bridge exists.

use std::path::{Path, PathBuf};

use tonic::Status;
use vxedge_api::v1::RunRef;
use vxedge_core::instance::Instance;

use crate::errors::R;
use crate::services::ActiveRecord;

/// The directory of the run in slot `instance`.
pub(crate) fn run_dir(root: &Path, instance: Instance) -> PathBuf {
    root.join("runs").join(instance.to_string())
}

/// The run a request acts on: its slot, and its directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunScope {
    pub instance: Instance,
    pub dir: PathBuf,
}

impl RunScope {
    pub(crate) fn new(root: &Path, instance: Instance) -> Self {
        Self {
            instance,
            dir: run_dir(root, instance),
        }
    }
}

/// Whether a host link exists. A seam so the rules below are tested without a host.
pub(crate) fn link_up(bridge: &str) -> bool {
    Path::new("/sys/class/net").join(bridge).exists()
}

/// Every run with a record whose bridge exists, lowest slot first.
pub(crate) fn recorded(root: &Path) -> Vec<(Instance, ActiveRecord)> {
    recorded_with(root, link_up)
}

pub(crate) fn recorded_with(
    root: &Path,
    bridge_up: impl Fn(&str) -> bool,
) -> Vec<(Instance, ActiveRecord)> {
    Instance::all()
        .filter_map(|i| {
            let record = crate::services::read_active_record(&run_dir(root, i))?;
            bridge_up(&record.bridge).then_some((i, record))
        })
        .collect()
}

/// Whether two workspace paths are one folder: two copies of one package in two folders are two
/// runs, and one folder named two ways is one.
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn list(runs: &[(Instance, ActiveRecord)]) -> String {
    runs.iter()
        .map(|(i, r)| format!("`{}` (slot {i}, `{}`)", r.run_id(), r.id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The run a verb acts on (`ARCHITECTURE.md` §5.2): the run it names; else the run of the
/// workspace it names; else the only run. With nothing up and no run named, the first slot, so a
/// verb that finds nothing reports it as it did before there were slots.
pub(crate) fn resolve(
    root: &Path,
    run: Option<&RunRef>,
    workspace: Option<&Path>,
) -> Result<RunScope, Status> {
    resolve_in(root, &recorded(root), run, workspace)
}

pub(crate) fn resolve_in(
    root: &Path,
    runs: &[(Instance, ActiveRecord)],
    run: Option<&RunRef>,
    workspace: Option<&Path>,
) -> Result<RunScope, Status> {
    if let Some(wanted) = run.filter(|r| !r.run_id.is_empty()) {
        return match runs.iter().find(|(_, r)| r.run_id() == wanted.run_id) {
            Some((i, _)) => Ok(RunScope::new(root, *i)),
            None if runs.is_empty() => Err(crate::errors::precondition(
                R::NoActiveRun,
                format!("run `{}` is not active: nothing is", wanted.run_id),
            )),
            None => Err(crate::errors::precondition(
                R::RunMismatch,
                format!(
                    "run `{}` is not active; these are: {}",
                    wanted.run_id,
                    list(runs)
                ),
            )),
        };
    }
    let matching: Vec<_> = match workspace {
        Some(dir) => runs
            .iter()
            .filter(|(_, r)| same_dir(&r.dir, dir))
            .cloned()
            .collect(),
        None => runs.to_vec(),
    };
    match matching.as_slice() {
        // Nothing up: the first slot, where a verb finds nothing, as before there were slots.
        [] if runs.is_empty() => Ok(RunScope::new(root, Instance::FIRST)),
        // Never another run's slot: a workspace with no run has nothing to act on here.
        [] => Err(crate::errors::precondition(
            R::NoActiveRun,
            format!(
                "`{}` has no run on this host; these do: {}",
                workspace
                    .map(|w| w.display().to_string())
                    .unwrap_or_default(),
                list(runs)
            ),
        )),
        [(i, _)] => Ok(RunScope::new(root, *i)),
        many => Err(crate::errors::precondition(
            R::RunAmbiguous,
            format!(
                "{} runs match, so name one with its run id: {}",
                many.len(),
                list(many)
            ),
        )),
    }
}

/// The run of the workspace in `workspace`, if it has exactly one; for status verbs, which report
/// an absent run rather than refuse.
pub(crate) fn run_of(root: &Path, workspace: &Path) -> Option<RunScope> {
    let runs = recorded(root);
    let matching: Vec<_> = runs
        .iter()
        .filter(|(_, r)| same_dir(&r.dir, workspace))
        .collect();
    match matching.as_slice() {
        [(i, _)] => Some(RunScope::new(root, *i)),
        _ => None,
    }
}

/// Which workspace last ran in a slot, kept after its run is down so a later `Collect` finds the
/// ingest that run filled.
fn last_file(root: &Path, instance: Instance) -> PathBuf {
    run_dir(root, instance).join("workspace.json")
}

pub(crate) fn remember_workspace(root: &Path, instance: Instance, workspace: &Path) {
    let _ = std::fs::write(
        last_file(root, instance),
        serde_json::json!({ "dir": workspace }).to_string(),
    );
}

/// The slot the workspace in `workspace` last ran in, the latest when several did.
pub(crate) fn last_slot_of(root: &Path, workspace: &Path) -> Option<Instance> {
    Instance::all()
        .filter_map(|i| {
            let file = last_file(root, i);
            let text = std::fs::read_to_string(&file).ok()?;
            let v: serde_json::Value = serde_json::from_str(&text).ok()?;
            let dir = PathBuf::from(v["dir"].as_str()?);
            same_dir(&dir, workspace).then(|| {
                let at = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
                (at, i)
            })
        })
        .max()
        .map(|(_, i)| i)
}

/// A campaign's hold on its slot between trials, in that slot's directory so it outlives a
/// daemon restart. A campaign downs and ups its vehicle for every trial, and another workspace's
/// `up` in that gap must not take the slot.
fn reservation_file(root: &Path, instance: Instance) -> PathBuf {
    run_dir(root, instance).join("reserved.json")
}

/// The campaign that holds slot `instance`, if one does.
pub(crate) fn reserved_by(root: &Path, instance: Instance) -> Option<String> {
    let text = std::fs::read_to_string(reservation_file(root, instance)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["campaign"].as_str().map(str::to_owned)
}

/// `resumable`: the campaign has a request id, so a retry after a restart resumes it in this
/// slot. A hold that cannot be resumed is released when the daemon starts.
pub(crate) fn reserve(
    root: &Path,
    instance: Instance,
    campaign: &str,
    resumable: bool,
) -> std::io::Result<()> {
    std::fs::create_dir_all(run_dir(root, instance))?;
    std::fs::write(
        reservation_file(root, instance),
        serde_json::json!({ "campaign": campaign, "resumable": resumable }).to_string(),
    )
}

/// Whether the hold on slot `instance` can be resumed by a retry.
pub(crate) fn resumable(root: &Path, instance: Instance) -> bool {
    std::fs::read_to_string(reservation_file(root, instance))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["resumable"].as_bool())
        .unwrap_or(false)
}

/// Ends a campaign's hold. A hold that is already gone is fine: release runs on every exit path.
pub(crate) fn release(root: &Path, instance: Instance, campaign: &str) {
    if reserved_by(root, instance).as_deref() == Some(campaign) {
        let _ = std::fs::remove_file(reservation_file(root, instance));
    }
}

/// Who is asking for a slot: a campaign may use the slot it holds.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Asker<'a> {
    pub campaign: Option<&'a str>,
    /// The id of the model being brought up: an inline manifest has no folder, so its own run is
    /// the one with no folder and this id.
    pub id: Option<&'a str>,
}

/// The slot an `up` of the workspace in `workspace` takes (`ARCHITECTURE.md` §6.0.5): its own
/// run's, when it has one and asks for no other; else the one it asks for, unless another run
/// holds it; else the lowest slot no run holds and no campaign reserves.
pub(crate) fn choose(
    root: &Path,
    workspace: Option<&Path>,
    requested: Option<Instance>,
    asker: Asker<'_>,
) -> Result<Instance, Status> {
    choose_in(
        &recorded(root),
        |i| reserved_by(root, i),
        workspace,
        requested,
        asker,
    )
}

pub(crate) fn choose_in(
    runs: &[(Instance, ActiveRecord)],
    reserved: impl Fn(Instance) -> Option<String>,
    workspace: Option<&Path>,
    requested: Option<Instance>,
    asker: Asker<'_>,
) -> Result<Instance, Status> {
    let own = |i: Instance| {
        runs.iter()
            .find(|(s, _)| *s == i)
            .is_some_and(|(_, r)| match workspace {
                Some(w) => same_dir(&r.dir, w),
                None => r.dir == Path::new(".") && asker.id.is_some_and(|id| id == r.id),
            })
    };
    let held_by = |i: Instance| runs.iter().find(|(s, _)| *s == i).map(|(_, r)| r);
    let reserved_for_other =
        |i: Instance| reserved(i).filter(|c| asker.campaign != Some(c.as_str()));
    if let Some(i) = requested {
        if let Some(r) = held_by(i).filter(|_| !own(i)) {
            return Err(crate::errors::precondition(
                R::HeldByRun,
                format!(
                    "slot {i} is held by run `{}` of `{}`",
                    r.run_id(),
                    r.dir.display()
                ),
            ));
        }
        if let Some(c) = reserved_for_other(i) {
            return Err(crate::errors::precondition(
                R::HeldByRun,
                format!("slot {i} is held by campaign `{c}` between its trials"),
            ));
        }
        return Ok(i);
    }
    if let Some(campaign) = asker.campaign
        && let Some(i) = Instance::all().find(|i| reserved(*i).as_deref() == Some(campaign))
    {
        return Ok(i);
    }
    let mine: Vec<Instance> = runs.iter().map(|(i, _)| *i).filter(|i| own(*i)).collect();
    match mine.as_slice() {
        [i] => return Ok(*i),
        [] => {}
        _ => {
            return Err(crate::errors::precondition(
                R::RunAmbiguous,
                format!(
                    "this workspace runs in slots {}, so name one with `instance`",
                    mine.iter()
                        .map(Instance::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
    }
    Instance::all()
        .find(|i| held_by(*i).is_none() && reserved_for_other(*i).is_none())
        .ok_or_else(|| {
            crate::errors::precondition(
                R::HeldByRun,
                "every instance slot on this host is held by a run or a campaign",
            )
        })
}

/// The run a composite verb (`experiment`, `vehicle`) stages: its slot and directory, and its run
/// id once its gateway is up, which every later stage names.
#[derive(Debug, Clone)]
pub(crate) struct RunCtx {
    pub scope: RunScope,
    pub run: Option<RunRef>,
    /// The bare vehicle a `vehicle up` brings up, so its gateway stage records the run as that
    /// vehicle's. `None` for an experiment.
    pub vehicle: Option<String>,
}

impl RunCtx {
    pub(crate) fn new(scope: RunScope) -> Self {
        let mut ctx = Self {
            scope,
            run: None,
            vehicle: None,
        };
        ctx.refresh();
        ctx
    }

    /// A run that is the bare vehicle `id`'s.
    pub(crate) fn of_vehicle(mut self, id: &str) -> Self {
        self.vehicle = Some(id.to_string());
        self
    }

    /// Read the run id from the run's record: after its gateway comes up, or for a run already up.
    pub(crate) fn refresh(&mut self) {
        self.run = crate::services::read_active_record(&self.scope.dir).map(|r| RunRef {
            run_id: r.run_id().to_string(),
        });
    }
}

/// The slot an internal request acts on: every stage a composite verb (an `experiment`, a
/// `vehicle`, a campaign's trial) sends names its run's slot, so it never falls back to "the only
/// run", which with another run up is that other run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Slot(pub Instance);

/// `req`, marked as acting on slot `instance`.
pub(crate) fn with_slot<T>(mut req: tonic::Request<T>, instance: Instance) -> tonic::Request<T> {
    req.extensions_mut().insert(Slot(instance));
    req
}

/// The bare vehicle a gateway `up` brings up a run for, on the stage request `vehicle up` sends,
/// so the run's record says it is that vehicle's: its manifest is the synthesized experiment, which
/// alone does not say so.
#[derive(Debug, Clone)]
pub(crate) struct VehicleRun(pub String);

/// The run a request acts on: the slot an internal request names, else [`resolve`]'s rule.
pub(crate) fn of<T>(
    req: &tonic::Request<T>,
    root: &Path,
    run: Option<&RunRef>,
    workspace: Option<&Path>,
) -> Result<RunScope, Status> {
    match req.extensions().get::<Slot>() {
        Some(Slot(i)) => Ok(RunScope::new(root, *i)),
        None => resolve(root, run, workspace),
    }
}

/// Refuse a lifecycle verb from outside a campaign on the slot the campaign holds: its trials
/// down and up its run, and another caller's `down` or `zone up` there would cut a trial short.
/// The campaign's own requests, and the stages of a composite verb, name their slot or campaign.
pub(crate) fn guard<T>(
    req: &tonic::Request<T>,
    root: &Path,
    scope: &RunScope,
) -> Result<(), Status> {
    Caller::of(req).check(root, scope)
}

/// Who sent a lifecycle request, for [`guard`]: a stage of a composite verb, or a campaign.
#[derive(Debug, Clone, Default)]
pub(crate) struct Caller {
    internal: bool,
    campaign: Option<String>,
}

impl Caller {
    pub(crate) fn of<T>(req: &tonic::Request<T>) -> Self {
        Self {
            internal: req.extensions().get::<Slot>().is_some(),
            campaign: req.extensions().get::<Campaign>().map(|c| c.0.clone()),
        }
    }

    pub(crate) fn check(&self, root: &Path, scope: &RunScope) -> Result<(), Status> {
        if self.internal {
            return Ok(());
        }
        match reserved_by(root, scope.instance) {
            Some(c) if Some(&c) != self.campaign.as_ref() => Err(crate::errors::precondition(
                R::HeldByRun,
                format!(
                    "slot {} is held by campaign `{c}` between its trials; wait for it to finish",
                    scope.instance
                ),
            )),
            _ => Ok(()),
        }
    }
}

/// A campaign's id on a request it makes, so the slot it reserves is open to it.
#[derive(Debug, Clone)]
pub(crate) struct Campaign(pub String);

/// What a lifecycle verb acts on: a run it may start, in the slot asked for if any, or a run
/// that is already up, named or found by its workspace.
#[derive(Debug, Clone)]
pub(crate) enum Target {
    Start(Option<Instance>),
    Existing(Option<RunRef>),
    /// The slot an internal request names.
    At(Instance),
}

impl Target {
    /// The run this target is, for a request whose workspace is in `workspace`.
    pub(crate) fn scope(
        &self,
        root: &Path,
        workspace: Option<&Path>,
        id: Option<&str>,
    ) -> Result<RunScope, Status> {
        match self {
            Target::Start(requested) => {
                choose(root, workspace, *requested, Asker { campaign: None, id })
                    .map(|i| RunScope::new(root, i))
            }
            Target::Existing(run) => resolve(root, run.as_ref(), workspace),
            Target::At(i) => Ok(RunScope::new(root, *i)),
        }
    }
}

/// What an `up` or `apply` starts: the slot an internal request names (its parent chose it, a
/// campaign's trial in the slot its campaign holds), else the slot `instance` asks for, if any.
pub(crate) fn start_of<T>(req: &tonic::Request<T>, instance: &str) -> Result<Target, Status> {
    match req.extensions().get::<Slot>() {
        Some(Slot(i)) => Ok(Target::At(*i)),
        None => requested(instance).map(Target::Start),
    }
}

/// The slot a request names, if it names one.
pub(crate) fn requested(instance: &str) -> Result<Option<Instance>, Status> {
    if instance.is_empty() {
        return Ok(None);
    }
    Instance::parse(instance)
        .map(Some)
        .map_err(|e| crate::errors::invalid(R::InvalidRequest, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, dir: &str, run_id: &str) -> ActiveRecord {
        ActiveRecord {
            id: id.into(),
            dir: dir.into(),
            bridge: "vxbr0".into(),
            sha256: None,
            identity_algorithm: None,
            run_id: Some(run_id.into()),
            started_unix_ms: None,
            package: None,
            kernel: None,
            shaping: None,
            time_sync: None,
            vehicle: None,
        }
    }

    fn slot(c: &str) -> Instance {
        Instance::parse(c).unwrap()
    }

    fn reason(s: &Status) -> String {
        use tonic_types::StatusExt;
        s.get_error_details()
            .error_info()
            .map(|i| i.reason.clone())
            .unwrap_or_default()
    }

    fn runs() -> Vec<(Instance, ActiveRecord)> {
        vec![
            (slot("0"), record("car", "/ws/a", "run-a")),
            (slot("1"), record("car", "/ws/b", "run-b")),
        ]
    }

    const ROOT: &str = "/run/vx";

    #[test]
    fn a_named_run_is_that_run() {
        let s = resolve_in(
            Path::new(ROOT),
            &runs(),
            Some(&RunRef {
                run_id: "run-b".into(),
            }),
            None,
        )
        .unwrap();
        assert_eq!(s, RunScope::new(Path::new(ROOT), slot("1")));
        assert_eq!(s.dir, Path::new("/run/vx/runs/1"));
    }

    #[test]
    fn a_named_run_that_is_not_up_is_refused() {
        let e = resolve_in(
            Path::new(ROOT),
            &runs(),
            Some(&RunRef {
                run_id: "run-z".into(),
            }),
            None,
        )
        .unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_RUN_MISMATCH");
        let e = resolve_in(
            Path::new(ROOT),
            &[],
            Some(&RunRef {
                run_id: "run-z".into(),
            }),
            None,
        )
        .unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_NO_ACTIVE_RUN");
    }

    #[test]
    fn no_run_named_is_the_workspaces_run_or_the_only_one() {
        let root = Path::new(ROOT);
        assert_eq!(
            resolve_in(root, &runs(), None, Some(Path::new("/ws/b")))
                .unwrap()
                .instance,
            slot("1")
        );
        let only = &runs()[..1];
        assert_eq!(
            resolve_in(root, only, None, None).unwrap().instance,
            slot("0")
        );
        assert_eq!(
            resolve_in(root, &[], None, None).unwrap().instance,
            Instance::FIRST
        );
    }

    /// The danger the rule exists for: a `down` of a workspace with no run must not land on
    /// another run's slot.
    #[test]
    fn a_workspace_with_no_run_never_resolves_to_another_runs_slot() {
        let e = resolve_in(Path::new(ROOT), &runs(), None, Some(Path::new("/ws/c"))).unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_NO_ACTIVE_RUN");
        assert!(e.message().contains("run-a"), "{}", e.message());
    }

    #[test]
    fn two_runs_and_no_run_named_is_ambiguous_and_never_chosen_silently() {
        let e = resolve_in(Path::new(ROOT), &runs(), None, None).unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_RUN_AMBIGUOUS");
        assert!(
            e.message().contains("run-a") && e.message().contains("run-b"),
            "{}",
            e.message()
        );
        let twice = vec![
            (slot("0"), record("car", "/ws/a", "run-a")),
            (slot("1"), record("car", "/ws/a", "run-a2")),
        ];
        let e = resolve_in(Path::new(ROOT), &twice, None, Some(Path::new("/ws/a"))).unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_RUN_AMBIGUOUS");
    }

    #[test]
    fn up_keeps_its_runs_slot_or_takes_the_lowest_free_one() {
        let none = |_| None;
        let ask = Asker::default();
        assert_eq!(
            choose_in(&runs(), none, Some(Path::new("/ws/b")), None, ask).unwrap(),
            slot("1")
        );
        assert_eq!(
            choose_in(&runs(), none, Some(Path::new("/ws/c")), None, ask).unwrap(),
            slot("2")
        );
        assert_eq!(
            choose_in(&[], none, None, None, ask).unwrap(),
            Instance::FIRST
        );
    }

    #[test]
    fn a_slot_another_run_holds_is_refused() {
        let e = choose_in(
            &runs(),
            |_| None,
            Some(Path::new("/ws/c")),
            Some(slot("0")),
            Asker::default(),
        )
        .unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_HELD_BY_RUN");
        assert!(e.message().contains("run-a"), "{}", e.message());
        // Its own slot, asked for again, is a repeated `up`.
        assert_eq!(
            choose_in(
                &runs(),
                |_| None,
                Some(Path::new("/ws/a")),
                Some(slot("0")),
                Asker::default()
            )
            .unwrap(),
            slot("0")
        );
        // The same workspace in a second slot is a second run of it.
        assert_eq!(
            choose_in(
                &runs(),
                |_| None,
                Some(Path::new("/ws/a")),
                Some(slot("5")),
                Asker::default()
            )
            .unwrap(),
            slot("5")
        );
    }

    #[test]
    fn a_campaigns_reserved_slot_is_skipped_by_others_and_kept_for_it() {
        let reserved = |i: Instance| (i == slot("2")).then(|| "camp-1".to_string());
        let other = Asker::default();
        assert_eq!(
            choose_in(&runs(), reserved, Some(Path::new("/ws/c")), None, other).unwrap(),
            slot("3"),
            "the gap between two trials is not a free slot"
        );
        let e = choose_in(
            &runs(),
            reserved,
            Some(Path::new("/ws/c")),
            Some(slot("2")),
            other,
        )
        .unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_HELD_BY_RUN");
        let campaign = Asker {
            campaign: Some("camp-1"),
            id: None,
        };
        assert_eq!(
            choose_in(&runs(), reserved, Some(Path::new("/ws/c")), None, campaign).unwrap(),
            slot("2")
        );
    }

    #[test]
    fn a_reservation_is_kept_on_disk_and_released_only_by_its_campaign() {
        let root = std::env::temp_dir().join(format!("vxscope-{}", std::process::id()));
        let i = slot("4");
        reserve(&root, i, "camp-1", true).unwrap();
        assert!(resumable(&root, i));
        assert_eq!(reserved_by(&root, i).as_deref(), Some("camp-1"));
        release(&root, i, "camp-2");
        assert_eq!(reserved_by(&root, i).as_deref(), Some("camp-1"));
        release(&root, i, "camp-1");
        assert_eq!(reserved_by(&root, i), None);
        release(&root, i, "camp-1");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An inline manifest has no folder: its own run is the one with none and the same id, so a
    /// repeated `up` keeps its slot instead of taking another.
    #[test]
    fn a_repeated_inline_up_keeps_its_slot() {
        let runs = vec![(slot("3"), record("car", ".", "run-i"))];
        let ask = Asker {
            campaign: None,
            id: Some("car"),
        };
        assert_eq!(
            choose_in(&runs, |_| None, None, None, ask).unwrap(),
            slot("3")
        );
        let other = Asker {
            campaign: None,
            id: Some("bus"),
        };
        assert_eq!(
            choose_in(&runs, |_| None, None, None, other).unwrap(),
            Instance::FIRST
        );
    }

    #[test]
    fn a_slot_named_in_a_request_must_be_one() {
        assert_eq!(requested("").unwrap(), None);
        assert_eq!(requested("7").unwrap(), Some(slot("7")));
        assert_eq!(
            reason(&requested("A").unwrap_err()),
            "ERROR_REASON_INVALID_REQUEST"
        );
    }

    #[test]
    fn records_count_only_while_their_bridge_exists() {
        let root = std::env::temp_dir().join(format!("vxscope-rec-{}", std::process::id()));
        for (s, bridge) in [("0", "vxbr0"), ("3", "vxbr3")] {
            let d = run_dir(&root, slot(s));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("active.json"),
                serde_json::json!({"id": "car", "dir": "/ws/a", "bridge": bridge, "run_id": format!("run-{s}")})
                    .to_string(),
            )
            .unwrap();
        }
        let found = recorded_with(&root, |b| b == "vxbr3");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, slot("3"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A campaign's trial asks its stages for the slot its campaign holds: the stage starts
    /// there, while an outside `up` naming that slot is refused.
    #[test]
    fn a_stage_starts_in_the_slot_its_parent_chose_even_when_a_campaign_holds_it() {
        let root = std::env::temp_dir().join(format!("vxscope-stage-{}", std::process::id()));
        reserve(&root, Instance::FIRST, "c-1", true).unwrap();
        let stage = with_slot(tonic::Request::new(()), Instance::FIRST);
        let target = start_of(&stage, "").unwrap();
        assert_eq!(
            target.scope(&root, None, None).unwrap().instance,
            Instance::FIRST
        );
        let outside = start_of(&tonic::Request::new(()), "0").unwrap();
        assert_eq!(
            reason(&outside.scope(&root, None, None).unwrap_err()),
            "ERROR_REASON_HELD_BY_RUN"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
