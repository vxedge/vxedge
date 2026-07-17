//! Subscriber provisioning: writes `subscribers` documents directly into
//! Open5GS's MongoDB store, rather than shelling to `open5gs-dbctl`.
//!
//! Why: `dbctl`'s CLI helpers do not just require an SD for
//! `add_ue_with_slice` — its SD-less fallback, `add_ue_with_apn`,
//! **hardcodes `sst: NumberInt(1)` and omits the `sd` field entirely**
//! (confirmed by reading `/usr/local/bin/open5gs-dbctl` inside `vx5g-core`,
//! v2.8.0 tag script). A slice declared with e.g. `sst: 2` and no SD would
//! silently register its subscriber under SST 1 — wrong, not just
//! "default". The document shape below is copied from `dbctl`'s own
//! `insertOne` call (both branches; diffed to confirm they agree on every
//! field except `slice.sst`/`slice.sd`), so this is schema-compatible with
//! what AUSF/UDM already expect — just built with the session's *actual*
//! sst/sd always, sd included only when the slice declares one (a valid,
//! and different, document shape from "sd: empty string").
//!
//! Values are interpolated into a JS string without escaping — safe only
//! because every value crossed [`super::plan::FiveGPlan::from_manifest`]'s
//! validation first (imsi/sst are digits, key/opc/sd are hex, apn is
//! charset-restricted), the same injection-safety argument `render.rs`
//! already makes for its config templates.

use super::plan::{FiveGPlan, SliceSession};
use super::render::{TEST_UE_KEY, TEST_UE_OPC};

/// The `session` field is a **BSON array** of one document (confirmed
/// against `dbctl`'s own script, and the hard way: an earlier version of
/// this function emitted a bare object here via a bracket-eating
/// find/replace, which UDR accepted on insert but failed to convert back to
/// JSON when AMF queried it — `dnn_infos` came back empty and every PDU
/// session request failed `DNN_NOT_SUPPORTED_OR_NOT_SUBSCRIBED`, live-caught
/// 2026-07-17. Building the nesting directly with `format!`, not
/// string-replace, so this class of bug can't recur silently.
fn session_array(apn: &str) -> String {
    format!(
        r#"[{{ "name": "{apn}", "type": NumberInt(3), "qos": {{ "index": NumberInt(9), "arp": {{ "priority_level": NumberInt(8), "pre_emption_capability": NumberInt(1), "pre_emption_vulnerability": NumberInt(2) }} }}, "ambr": {{ "downlink": {{ "value": NumberInt(1000000000), "unit": NumberInt(0) }}, "uplink": {{ "value": NumberInt(1000000000), "unit": NumberInt(0) }} }}, "pcc_rule": [], "_id": new ObjectId() }}]"#
    )
}

fn slice_literal(sst: u8, sd: Option<&str>, apn: &str) -> String {
    let sd_field = match sd {
        Some(sd) => format!("\"sd\": \"{sd}\", "),
        None => String::new(),
    };
    format!(
        r#"{{ "sst": NumberInt({sst}), {sd_field}"default_indicator": true, "session": {session}, "_id": new ObjectId() }}"#,
        session = session_array(apn)
    )
}

/// The mongosh `insertOne` script for one subscriber. Pure string
/// construction — no I/O, unit-tested directly.
pub fn subscriber_insert_script(s: &SliceSession) -> String {
    let imsi = s.supi.trim_start_matches("imsi-");
    let slice = slice_literal(s.sst, s.sd.as_deref(), &s.dnn);
    format!(
        r#"db.subscribers.insertOne({{ "_id": new ObjectId(), "schema_version": NumberInt(1), "imsi": "{imsi}", "msisdn": [], "imeisv": [], "mme_host": [], "mm_realm": [], "purge_flag": [], "slice": [{slice}], "security": {{ "k": "{key}", "op": null, "opc": "{opc}", "amf": "8000" }}, "ambr": {{ "downlink": {{ "value": NumberInt(1000000000), "unit": NumberInt(0) }}, "uplink": {{ "value": NumberInt(1000000000), "unit": NumberInt(0) }} }}, "access_restriction_data": 32, "network_access_mode": 0, "subscriber_status": 0, "operator_determined_barring": 0, "subscribed_rau_tau_timer": 12, "__v": 0 }});"#,
        key = TEST_UE_KEY,
        opc = TEST_UE_OPC,
    )
}

/// One `mongosh --eval <script> <db_uri>` argv per session — the exact
/// invocation shape `dbctl` itself used, so it drops into the same
/// `podman run --rm --pod <pod> <core_image> <args>` call site `lifecycle`
/// already makes for every other one-shot command.
pub fn subscriber_insert_commands(plan: &FiveGPlan) -> Vec<Vec<String>> {
    plan.sessions
        .iter()
        .map(|s| {
            vec![
                "mongosh".to_string(),
                "--eval".to_string(),
                subscriber_insert_script(s),
                plan.db_uri.clone(),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ExperimentManifest;

    fn plan_two_slices() -> FiveGPlan {
        let yaml = r#"
experiment: { id: t, name: t, duration_s: 1, results_prefix: r }
network:
  slices:
    - { id: adas, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }
    - { id: infotainment, mode: s-nssai, s_nssai: { sst: 2 } }
"#;
        let m = ExperimentManifest::from_yaml(yaml).expect("test manifest parses");
        FiveGPlan::from_manifest(&m).expect("plans")
    }

    #[test]
    fn script_with_sd_includes_the_slices_own_sst_and_sd() {
        let plan = plan_two_slices();
        let script = subscriber_insert_script(&plan.sessions[0]);
        assert!(script.contains("\"imsi\": \"999700000000001\""));
        assert!(script.contains("\"sst\": NumberInt(1)"));
        assert!(script.contains("\"sd\": \"000001\""));
        assert!(script.contains("\"name\": \"adas\""));
        assert!(script.contains(&format!("\"k\": \"{TEST_UE_KEY}\"")));
        assert!(script.contains(&format!("\"opc\": \"{TEST_UE_OPC}\"")));
    }

    /// Regression guard for the live-caught bug (2026-07-17): `session`
    /// must be a BSON **array** containing the session document, not a bare
    /// document — Open5GS's UDR silently accepts the wrong shape on insert
    /// but fails converting it back to JSON on query, breaking every PDU
    /// session with `DNN_NOT_SUPPORTED_OR_NOT_SUBSCRIBED`. `"session": [{`
    /// is the array-open-then-object-open sequence; the bug produced
    /// `"session": {` (no `[`) instead.
    #[test]
    fn session_field_is_an_array_not_a_bare_object() {
        let plan = plan_two_slices();
        let script = subscriber_insert_script(&plan.sessions[0]);
        assert!(
            script.contains("\"session\": [{"),
            "session must open with '[{{' (array containing one doc), got: {script}"
        );
        // Balanced: the array must also close before the slice doc closes.
        assert!(script.contains("}]"), "session array must close with '}}]'");
    }

    #[test]
    fn script_without_sd_still_uses_the_slices_actual_sst_not_1() {
        // The bug this module fixes: dbctl's SD-less fallback hardcoded
        // sst=1 regardless of the manifest. This slice is sst=2, no sd.
        let plan = plan_two_slices();
        let script = subscriber_insert_script(&plan.sessions[1]);
        assert!(script.contains("\"sst\": NumberInt(2)"));
        assert!(
            !script.contains("\"sst\": NumberInt(1)"),
            "must not silently fall back to sst 1"
        );
    }

    #[test]
    fn script_without_sd_omits_the_sd_field_rather_than_writing_a_blank_one() {
        let plan = plan_two_slices();
        let script = subscriber_insert_script(&plan.sessions[1]);
        assert!(!script.contains("\"sd\":"));
    }

    #[test]
    fn commands_carry_the_plans_db_uri() {
        let plan = plan_two_slices();
        let cmds = subscriber_insert_commands(&plan);
        assert_eq!(cmds.len(), 2);
        for cmd in &cmds {
            assert_eq!(cmd[0], "mongosh");
            assert_eq!(cmd[1], "--eval");
            assert_eq!(cmd[3], plan.db_uri);
        }
    }

    #[test]
    fn every_session_gets_a_distinct_imsi_in_its_script() {
        let plan = plan_two_slices();
        let s0 = subscriber_insert_script(&plan.sessions[0]);
        let s1 = subscriber_insert_script(&plan.sessions[1]);
        assert!(s0.contains("999700000000001"));
        assert!(s1.contains("999700000000002"));
    }
}
