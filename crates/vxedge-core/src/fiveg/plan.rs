//! FiveGPlan: desired-state compilation of a manifest's `mode: s-nssai`
//! slices. Pure data — no privileges, no I/O — mirroring `GatewayPlan`.
//!
//! Addressing is deterministic per slice ordering (loopback-only, single
//! host): every run of the same manifest produces byte-identical configs,
//! which is what makes rendered configs diffable and archives reproducible.

use crate::manifest::{ExperimentManifest, SliceMode};

/// Loopback addressing gives each slice its own SMF/UPF pair; the scheme
/// below caps out well before octet overflow, and >16 slices on one host is
/// outside any planned experiment.
const MAX_SNSSAI_SLICES: usize = 16;

/// Lab PLMN (the Open5GS documentation/test PLMN — not a real operator).
pub const PLMN_MCC: &str = "999";
pub const PLMN_MNC: &str = "70";
pub const TAC: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum FiveGPlanError {
    #[error(
        "no `network.slices` with mode `s-nssai` — nothing to plan (vlan slices are the gateway's job)"
    )]
    NoSnssaiSlices,
    #[error("slice `{0}` has mode `s-nssai` but no `s_nssai` block")]
    MissingSnssai(String),
    #[error(
        "slice `{slice}` has invalid s_nssai.sd `{sd}` — must be exactly 6 hex digits (3-byte slice differentiator)"
    )]
    InvalidSd { slice: String, sd: String },
    #[error("slices `{a}` and `{b}` declare the same S-NSSAI (sst+sd must be unique per slice)")]
    DuplicateSnssai { a: String, b: String },
    #[error(
        "slice id `{0}` contains characters outside [a-zA-Z0-9_-] — it is interpolated into config files and DNNs"
    )]
    InvalidSliceId(String),
    #[error("too many s-nssai slices: {0} (max {MAX_SNSSAI_SLICES})")]
    TooManySlices(usize),
}

/// One slice's PDU session universe: the S-NSSAI, its dedicated SMF/UPF
/// pair, the UE that subscribes to it, and the UE IP pool it hands out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceSession {
    pub slice_id: String,
    pub sst: u8,
    /// Normalized lowercase 6-hex-digit slice differentiator, if declared.
    pub sd: Option<String>,
    /// Data network name for the session; the slice id doubles as the DNN,
    /// which keeps manifest vocabulary and 5G vocabulary 1:1.
    pub dnn: String,
    /// UE pool subnet the slice's UPF serves (`10.<46+i>.0.0/16`).
    pub ue_subnet: String,
    /// UPF tunnel gateway address inside `ue_subnet` (`10.<46+i>.0.1`).
    pub ue_gateway: String,
    /// SMF SBI/PFCP address (`127.0.1.<10+i>`).
    pub smf_addr: String,
    /// UPF PFCP address (`127.0.2.<10+i>`).
    pub upf_pfcp_addr: String,
    /// UPF GTP-U address (`127.0.2.<110+i>`).
    pub upf_gtpu_addr: String,
    /// UE SUPI (`imsi-99970000000000<1+i>`), registered against the lab PLMN.
    pub supi: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiveGPlan {
    pub mcc: String,
    pub mnc: String,
    pub tac: u32,
    /// AMF NGAP + SBI address (Open5GS upstream loopback convention).
    pub amf_addr: String,
    /// NRF SBI address.
    pub nrf_addr: String,
    /// NSSF SBI address.
    pub nssf_addr: String,
    /// UERANSIM gNB link/NGAP/GTP address.
    pub gnb_addr: String,
    pub sessions: Vec<SliceSession>,
}

fn valid_slice_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn normalize_sd(slice: &str, sd: &str) -> Result<String, FiveGPlanError> {
    let norm = sd.to_ascii_lowercase();
    if norm.len() == 6 && norm.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(norm)
    } else {
        Err(FiveGPlanError::InvalidSd {
            slice: slice.to_string(),
            sd: sd.to_string(),
        })
    }
}

impl FiveGPlan {
    pub fn from_manifest(manifest: &ExperimentManifest) -> Result<Self, FiveGPlanError> {
        let slices: Vec<_> = manifest
            .network
            .as_ref()
            .map(|n| n.slices.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter(|s| s.mode == SliceMode::Snssai)
            .collect();

        if slices.is_empty() {
            return Err(FiveGPlanError::NoSnssaiSlices);
        }
        if slices.len() > MAX_SNSSAI_SLICES {
            return Err(FiveGPlanError::TooManySlices(slices.len()));
        }

        let mut sessions: Vec<SliceSession> = Vec::with_capacity(slices.len());
        for (i, s) in slices.iter().enumerate() {
            if !valid_slice_id(&s.id) {
                return Err(FiveGPlanError::InvalidSliceId(s.id.clone()));
            }
            let snssai = s
                .s_nssai
                .as_ref()
                .ok_or_else(|| FiveGPlanError::MissingSnssai(s.id.clone()))?;
            let sd = snssai
                .sd
                .as_deref()
                .map(|sd| normalize_sd(&s.id, sd))
                .transpose()?;

            if let Some(prev) = sessions.iter().find(|p| p.sst == snssai.sst && p.sd == sd) {
                return Err(FiveGPlanError::DuplicateSnssai {
                    a: prev.slice_id.clone(),
                    b: s.id.clone(),
                });
            }

            sessions.push(SliceSession {
                slice_id: s.id.clone(),
                sst: snssai.sst,
                sd,
                dnn: s.id.clone(),
                ue_subnet: format!("10.{}.0.0/16", 46 + i),
                ue_gateway: format!("10.{}.0.1", 46 + i),
                smf_addr: format!("127.0.1.{}", 10 + i),
                upf_pfcp_addr: format!("127.0.2.{}", 10 + i),
                upf_gtpu_addr: format!("127.0.2.{}", 110 + i),
                // 15-digit IMSI: MCC(3) + MNC(2) + 10-digit MSIN.
                supi: format!("imsi-{}{}{:010}", PLMN_MCC, PLMN_MNC, 1 + i),
            });
        }

        Ok(FiveGPlan {
            mcc: PLMN_MCC.to_string(),
            mnc: PLMN_MNC.to_string(),
            tac: TAC,
            amf_addr: "127.0.0.5".to_string(),
            nrf_addr: "127.0.0.10".to_string(),
            nssf_addr: "127.0.0.14".to_string(),
            gnb_addr: "127.0.0.100".to_string(),
            sessions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ExperimentManifest;

    fn manifest(slices_yaml: &str) -> ExperimentManifest {
        let yaml = format!(
            r#"
experiment: {{ id: t, name: t, duration_s: 1, results_prefix: r }}
network:
  slices:
{slices_yaml}
"#
        );
        ExperimentManifest::from_yaml(&yaml).expect("test manifest parses")
    }

    #[test]
    fn plans_sessions_with_deterministic_addressing() {
        let m = manifest(
            "    - { id: adas, mode: s-nssai, s_nssai: { sst: 1, sd: '0A0B0C' } }\n\
             \x20   - { id: infotainment, mode: s-nssai, s_nssai: { sst: 2 } }\n",
        );
        let plan = FiveGPlan::from_manifest(&m).expect("plans");
        assert_eq!(plan.sessions.len(), 2);
        let adas = &plan.sessions[0];
        assert_eq!(adas.sst, 1);
        assert_eq!(
            adas.sd.as_deref(),
            Some("0a0b0c"),
            "sd normalizes lowercase"
        );
        assert_eq!(adas.dnn, "adas");
        assert_eq!(adas.ue_subnet, "10.46.0.0/16");
        assert_eq!(adas.smf_addr, "127.0.1.10");
        assert_eq!(adas.upf_gtpu_addr, "127.0.2.110");
        assert_eq!(adas.supi, "imsi-999700000000001");
        let info = &plan.sessions[1];
        assert_eq!(info.sd, None);
        assert_eq!(info.ue_subnet, "10.47.0.0/16");
        assert_eq!(info.supi, "imsi-999700000000002");
        // Same manifest, same plan — the reproducibility contract.
        assert_eq!(plan, FiveGPlan::from_manifest(&m).expect("plans again"));
    }

    #[test]
    fn vlan_only_manifest_is_no_snssai() {
        let m = manifest("    - { id: adas, mode: vlan, vlan: 101 }\n");
        assert!(matches!(
            FiveGPlan::from_manifest(&m),
            Err(FiveGPlanError::NoSnssaiSlices)
        ));
    }

    #[test]
    fn mixed_manifest_plans_only_snssai_slices() {
        let m = manifest(
            "    - { id: adas, mode: vlan, vlan: 101 }\n\
             \x20   - { id: ota, mode: s-nssai, s_nssai: { sst: 3 } }\n",
        );
        let plan = FiveGPlan::from_manifest(&m).expect("plans");
        assert_eq!(plan.sessions.len(), 1);
        assert_eq!(plan.sessions[0].slice_id, "ota");
    }

    #[test]
    fn rejects_bad_sd() {
        let m = manifest("    - { id: adas, mode: s-nssai, s_nssai: { sst: 1, sd: 'xyz' } }\n");
        assert!(matches!(
            FiveGPlan::from_manifest(&m),
            Err(FiveGPlanError::InvalidSd { .. })
        ));
    }

    #[test]
    fn rejects_duplicate_snssai() {
        let m = manifest(
            "    - { id: a, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }\n\
             \x20   - { id: b, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }\n",
        );
        assert!(matches!(
            FiveGPlan::from_manifest(&m),
            Err(FiveGPlanError::DuplicateSnssai { .. })
        ));
    }
}
