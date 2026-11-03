//! The `serde` feature yields the canonical Protobuf JSON mapping: agents branch on enum
//! names, never on ordinals.
#![cfg(feature = "serde")]

use vxedge_api::v1::{ErrorReason, Job, JobState, ResourceState, ResourceStatus};

#[test]
fn enums_serialize_by_name_and_fields_in_lower_camel_case() {
    let job = Job {
        id: "job-1-1".into(),
        state: JobState::Failed as i32,
        progress_percent: 40,
        ..Default::default()
    };
    let v = serde_json::to_value(&job).unwrap();
    assert_eq!(v["state"], "JOB_STATE_FAILED");
    assert_eq!(v["progressPercent"], 40);

    let s = ResourceStatus {
        vxid: "z1".into(),
        state: ResourceState::Unprovisioned as i32,
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(&s).unwrap()["state"],
        "RESOURCE_STATE_UNPROVISIONED"
    );
    assert_eq!(
        serde_json::to_value(ErrorReason::Busy).unwrap(),
        "ERROR_REASON_BUSY"
    );
}

#[test]
fn default_values_are_emitted_not_omitted() {
    let v = serde_json::to_value(vxedge_api::v1::RunListResponse::default()).unwrap();
    assert_eq!(v["runs"], serde_json::json!([]));
    let v = serde_json::to_value(Job::default()).unwrap();
    assert_eq!(v["state"], "JOB_STATE_UNSPECIFIED");
    assert_eq!(v["error"], "");
}

#[test]
fn json_round_trips() {
    let job = Job {
        id: "job-1-2".into(),
        state: JobState::Running as i32,
        ..Default::default()
    };
    let back: Job = serde_json::from_value(serde_json::to_value(&job).unwrap()).unwrap();
    assert_eq!(back, job);
}
