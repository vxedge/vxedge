//! FiveGPlan: desired-state compilation of a manifest's `mode: s-nssai`
//! slices. Pure data — no privileges, no I/O — mirroring `GatewayPlan`.
//!
//! Addressing is deterministic per slice ordering: every run of the same
//! manifest produces byte-identical configs, which is what makes rendered
//! configs diffable and archives reproducible.
//!
//! Two address families, found necessary live (2026-07-17): the RAN (gNB +
//! UEs) cannot share a network namespace with the CN (AMF + per-slice
//! SMF/UPF) — confirmed by isolating the two and finding the UE's own
//! `uesimtun<i>` and the UPF's `vxtun<i>` device install *identical*
//! connected routes for the same UE subnet when co-located, an ambiguity
//! that never occurs in a real deployment (the UE is a physically separate
//! device). So: **CN-internal** SBI/PFCP traffic (NRF/AUSF/UDM/UDR/PCF/BSF,
//! SMF↔UPF) stays on loopback, unchanged; **RAN-facing** traffic (AMF's
//! NGAP, each UPF's GTP-U) moves to the transit subnet between the CN pod
//! and a separate RAN pod (`TRANSIT_SUBNET`) — the only addresses gNB
//! actually needs to dial.

/// Loopback addressing gives each slice its own SMF/UPF pair; the scheme
/// below caps out well before octet overflow, and >16 slices on one host is
/// outside any planned experiment.
const MAX_SNSSAI_SLICES: usize = 16;

/// CN pod ↔ RAN pod transit link (N2/N3 in 5G vocabulary), a dedicated
/// range distinct from any UE subnet (`10.<46+i>.0.0/16`) or Open5GS's own
/// documented sample UE subnet (`10.45.0.0/16`, never used by vxedge but
/// worth staying clear of for anyone cross-referencing upstream docs).
pub const TRANSIT_SUBNET: &str = "10.99.0.0/24";
pub const CN_TRANSIT_ADDR: &str = "10.99.0.1";
pub const RAN_TRANSIT_ADDR: &str = "10.99.0.2";

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
    /// SMF SBI/PFCP address (`127.0.1.<10+i>`, CN-internal).
    pub smf_addr: String,
    /// UPF PFCP address (`127.0.2.<10+i>`, CN-internal — talks to SMF only).
    pub upf_pfcp_addr: String,
    /// UPF GTP-U address (`10.99.0.<10+i>`, on the CN↔RAN transit subnet —
    /// the address gNB dials for this slice's N3 tunnel).
    pub upf_gtpu_addr: String,
    /// UPF tun device (`vxtun<i>`): one per slice — the default `ogstun`
    /// can only serve one UPF instance (found live: EBUSY on the second).
    pub tun_dev: String,
    /// UE SUPI (`imsi-99970000000000<1+i>`), registered against the lab PLMN.
    pub supi: String,
    /// This UE's own address (`10.99.0.<50+i>`) on the RAN pod's internal
    /// gNB-access bridge — reachable from gNB (`ran_transit_addr`, the
    /// bridge's own address) so `nr-ue`'s outbound socket to
    /// `gnbSearchList` has a source address and a route. Unrelated to
    /// `ue_subnet` (the UE's *own* PDU-session data IP, assigned only once
    /// registration succeeds) — this is purely the transport the UE's
    /// control-plane process itself runs over, before any PDU session
    /// exists. Offset well clear of `upf_gtpu_addr`'s `10.99.0.<10+i>`
    /// range (max index 15 → `.25`) even though they live on different
    /// pods' interfaces and would never literally collide at the kernel
    /// level — same subnet, so IP-level uniqueness still matters for
    /// routing consistency across it.
    pub ue_ran_link_addr: String,
    /// Downlink HTB shaping for this slice, opt-in via the manifest's own
    /// `Slice.qos` block — `None` (not `Some` with a 1mbit-floor default
    /// the way `gateway::plan::SlicePlan` defaults an *absent* vlan `qos`)
    /// when the slice doesn't declare one, so every manifest that predates
    /// this field (including every earlier live-verified s-nssai scenario)
    /// keeps its exact prior behavior: unclassified, unconstrained traffic
    /// up to the shared root rate. Only a slice that explicitly opts in
    /// gets a dedicated class. Found necessary 2026-07-18: Open5GS's own
    /// subscriber-level AMBR field is recorded but not enforced by its
    /// software UPF in this deployment (empirically confirmed live — a
    /// patched 20mbit AMBR had zero effect on measured throughput), so
    /// real per-slice isolation needs a kernel-level HTB class instead,
    /// mirroring `gateway::provision::apply_slices`'s already-proven
    /// mechanism rather than relying on 5G-core QoS signaling that isn't
    /// actually active here.
    pub qos: Option<SliceSessionQos>,
}

/// One session's downlink HTB parameters — same shape and defaulting
/// convention as `gateway::plan::SlicePlan`'s vlan-side fields (`rate` =
/// `min_rate_mbps`, `ceil` = `max_rate_mbps`), applied only when present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceSessionQos {
    pub min_rate_mbps: u64,
    pub max_rate_mbps: u64,
    pub priority: u8,
    /// tc classid minor number, distinct range from `SlicePlan`'s
    /// `0x10 + i` purely for readability in `tc class show` output side by
    /// side — the two trees live on different interfaces, so there is no
    /// actual collision risk either way.
    pub class_minor: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiveGPlan {
    pub mcc: String,
    pub mnc: String,
    pub tac: u32,
    /// AMF SBI address (CN-internal, loopback).
    pub amf_addr: String,
    /// AMF NGAP address — on the transit subnet; the address gNB dials for
    /// N2 (`CN_TRANSIT_ADDR`).
    pub amf_ngap_addr: String,
    /// NRF SBI address.
    pub nrf_addr: String,
    /// NSSF SBI address.
    pub nssf_addr: String,
    /// AUSF / UDM / UDR / PCF / BSF SBI addresses (registration + session
    /// policy path; upstream loopback convention).
    pub ausf_addr: String,
    pub udm_addr: String,
    pub udr_addr: String,
    pub pcf_addr: String,
    pub bsf_addr: String,
    /// Subscriber/policy database (MongoDB inside the CN pod).
    pub db_uri: String,
    /// RAN pod's transit address — gNB's link/NGAP/GTP-U bind address
    /// (`RAN_TRANSIT_ADDR`), and what every UE's `gnbSearchList` dials.
    /// Lives on a bridge internal to the RAN pod's own netns
    /// (`fiveg::lifecycle::ran_bridge_name`), not directly on any single
    /// veth — each UE now runs in its own pod/netns
    /// (`fiveg::lifecycle::ue_up`), reachable via its own veth into that
    /// bridge, so this one address stays reachable from all of them plus
    /// the CN pod, without any two UEs sharing a network namespace (found
    /// live 2026-07-17: UERANSIM hardcodes routing table `1000` for its
    /// per-UE default route, so co-located UEs collide on it).
    pub ran_transit_addr: String,
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

/// An `s-nssai` slice as the allocator needs it, independent of which schema it came from.
struct SnssaiInput {
    id: String,
    sst: Option<u8>,
    sd: Option<String>,
    /// `(min_rate_mbps, max_rate_mbps, priority)`.
    qos: Option<(u64, u64, u8)>,
}

impl FiveGPlan {
    /// Plan from the domain model (task 5b).
    ///
    /// Delegates to the same allocator as the §6.1 path. Every address in a `FiveGPlan` — UE
    /// subnets, SMF/UPF addresses, TUN devices, SUPIs — is derived positionally from the slice
    /// list, so two allocators would be two chances to drift and a drifted SUPI or subnet fails
    /// as a registration timeout with nothing pointing at the cause.
    pub fn from_model(exp: &crate::model::Experiment) -> Result<Self, FiveGPlanError> {
        let slices: Vec<SnssaiInput> = exp
            .slices
            .iter()
            .filter(|s| s.mode == crate::model::SliceMode::SNssai)
            .map(|s| SnssaiInput {
                id: s.id.clone(),
                sst: s.s_nssai.as_ref().map(|n| n.sst),
                sd: s.s_nssai.as_ref().and_then(|n| n.sd.clone()),
                qos: s.qos.as_ref().map(|q| {
                    (
                        u64::from(q.min_rate_mbps.unwrap_or(0)),
                        u64::from(q.max_rate_mbps.unwrap_or(0)),
                        q.priority.unwrap_or(4),
                    )
                }),
            })
            .collect();
        Self::allocate(&slices)
    }

    /// The one allocator both schema paths use.
    fn allocate(slices: &[SnssaiInput]) -> Result<Self, FiveGPlanError> {
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
            let sst = s
                .sst
                .ok_or_else(|| FiveGPlanError::MissingSnssai(s.id.clone()))?;
            let sd =
                s.sd.as_deref()
                    .map(|sd| normalize_sd(&s.id, sd))
                    .transpose()?;

            if let Some(prev) = sessions.iter().find(|p| p.sst == sst && p.sd == sd) {
                return Err(FiveGPlanError::DuplicateSnssai {
                    a: prev.slice_id.clone(),
                    b: s.id.clone(),
                });
            }

            sessions.push(SliceSession {
                slice_id: s.id.clone(),
                sst,
                sd,
                dnn: s.id.clone(),
                ue_subnet: format!("10.{}.0.0/16", 46 + i),
                ue_gateway: format!("10.{}.0.1", 46 + i),
                smf_addr: format!("127.0.1.{}", 10 + i),
                upf_pfcp_addr: format!("127.0.2.{}", 10 + i),
                upf_gtpu_addr: format!("10.99.0.{}", 10 + i),
                tun_dev: format!("vxtun{i}"),
                // 15-digit IMSI: MCC(3) + MNC(2) + 10-digit MSIN.
                supi: format!("imsi-{}{}{:010}", PLMN_MCC, PLMN_MNC, 1 + i),
                ue_ran_link_addr: format!("10.99.0.{}", 50 + i),
                qos: s.qos.map(|(min, max, prio)| SliceSessionQos {
                    min_rate_mbps: min.max(1),
                    max_rate_mbps: max.max(1),
                    priority: prio,
                    class_minor: 0x20 + i as u16,
                }),
            });
        }

        Ok(FiveGPlan {
            mcc: PLMN_MCC.to_string(),
            mnc: PLMN_MNC.to_string(),
            tac: TAC,
            amf_addr: "127.0.0.5".to_string(),
            amf_ngap_addr: CN_TRANSIT_ADDR.to_string(),
            nrf_addr: "127.0.0.10".to_string(),
            nssf_addr: "127.0.0.14".to_string(),
            ausf_addr: "127.0.0.11".to_string(),
            udm_addr: "127.0.0.12".to_string(),
            udr_addr: "127.0.0.20".to_string(),
            pcf_addr: "127.0.0.13".to_string(),
            bsf_addr: "127.0.0.15".to_string(),
            db_uri: "mongodb://127.0.0.1/open5gs".to_string(),
            ran_transit_addr: RAN_TRANSIT_ADDR.to_string(),
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
        let plan = FiveGPlan::from_model(&m.to_model().expect("adapts")).expect("plans");
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
        assert_eq!(adas.upf_gtpu_addr, "10.99.0.10");
        assert_eq!(adas.ue_ran_link_addr, "10.99.0.50");
        assert_eq!(adas.supi, "imsi-999700000000001");
        let info = &plan.sessions[1];
        assert_eq!(info.sd, None);
        assert_eq!(info.ue_subnet, "10.47.0.0/16");
        assert_eq!(info.upf_gtpu_addr, "10.99.0.11");
        assert_eq!(info.ue_ran_link_addr, "10.99.0.51");
        assert_eq!(info.supi, "imsi-999700000000002");
        assert_eq!(plan.amf_ngap_addr, CN_TRANSIT_ADDR);
        assert_eq!(plan.ran_transit_addr, RAN_TRANSIT_ADDR);
        // Same manifest, same plan — the reproducibility contract.
        assert_eq!(
            plan,
            FiveGPlan::from_model(&m.to_model().expect("adapts")).expect("plans again")
        );
    }

    #[test]
    fn session_qos_is_none_when_the_manifest_declares_no_qos_block() {
        // The compatibility-preserving default: every manifest that predates
        // this field (including every earlier live-verified s-nssai
        // scenario) keeps behaving exactly as before — unclassified,
        // unconstrained traffic — rather than silently getting capped.
        let m = manifest("    - { id: adas, mode: s-nssai, s_nssai: { sst: 1 } }\n");
        let plan = FiveGPlan::from_model(&m.to_model().expect("adapts")).expect("plans");
        assert_eq!(plan.sessions[0].qos, None);
    }

    #[test]
    fn session_qos_is_opt_in_and_mirrors_slice_plans_own_defaulting() {
        let m = manifest(
            "    - { id: adas, mode: s-nssai, s_nssai: { sst: 1 }, \
             qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }\n\
             \x20   - { id: infotainment, mode: s-nssai, s_nssai: { sst: 2 }, \
             qos: { max_rate_mbps: 400 } }\n",
        );
        let plan = FiveGPlan::from_model(&m.to_model().expect("adapts")).expect("plans");
        let adas_qos = plan.sessions[0].qos.expect("adas declared qos");
        assert_eq!(adas_qos.min_rate_mbps, 100);
        assert_eq!(adas_qos.max_rate_mbps, 200);
        assert_eq!(adas_qos.priority, 1);
        assert_eq!(adas_qos.class_minor, 0x20);

        // Partial qos (only max_rate_mbps set) falls back to SlicePlan's own
        // defaults for the rest, same convention, not a different one.
        let info_qos = plan.sessions[1].qos.expect("infotainment declared qos");
        assert_eq!(info_qos.min_rate_mbps, 1, "defaults to 1mbit floor");
        assert_eq!(info_qos.max_rate_mbps, 400);
        assert_eq!(info_qos.priority, 4, "defaults to priority 4");
        assert_eq!(info_qos.class_minor, 0x21);
    }

    #[test]
    fn ue_ran_link_addr_never_collides_with_upf_gtpu_addr_even_at_max_slices() {
        let slices: String = (0..MAX_SNSSAI_SLICES)
            .map(|i| {
                format!(
                    "    - {{ id: s{i}, mode: s-nssai, s_nssai: {{ sst: 1, sd: '{i:06x}' }} }}\n"
                )
            })
            .collect();
        let m = manifest(&slices);
        let plan = FiveGPlan::from_model(&m.to_model().expect("adapts"))
            .expect("plans at the max slice count");
        let gtpu: std::collections::HashSet<_> =
            plan.sessions.iter().map(|s| &s.upf_gtpu_addr).collect();
        let link: std::collections::HashSet<_> =
            plan.sessions.iter().map(|s| &s.ue_ran_link_addr).collect();
        assert!(
            gtpu.is_disjoint(&link),
            "upf_gtpu_addr and ue_ran_link_addr ranges must never overlap on the shared transit subnet"
        );
    }

    #[test]
    fn vlan_only_manifest_is_no_snssai() {
        let m = manifest("    - { id: adas, mode: vlan, vlan: 101 }\n");
        assert!(matches!(
            FiveGPlan::from_model(&m.to_model().expect("adapts")),
            Err(FiveGPlanError::NoSnssaiSlices)
        ));
    }

    #[test]
    fn mixed_manifest_plans_only_snssai_slices() {
        let m = manifest(
            "    - { id: adas, mode: vlan, vlan: 101 }\n\
             \x20   - { id: ota, mode: s-nssai, s_nssai: { sst: 3 } }\n",
        );
        let plan = FiveGPlan::from_model(&m.to_model().expect("adapts")).expect("plans");
        assert_eq!(plan.sessions.len(), 1);
        assert_eq!(plan.sessions[0].slice_id, "ota");
    }

    #[test]
    fn rejects_bad_sd() {
        let m = manifest("    - { id: adas, mode: s-nssai, s_nssai: { sst: 1, sd: 'xyz' } }\n");
        assert!(matches!(
            FiveGPlan::from_model(&m.to_model().expect("adapts")),
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
            FiveGPlan::from_model(&m.to_model().expect("adapts")),
            Err(FiveGPlanError::DuplicateSnssai { .. })
        ));
    }
}
