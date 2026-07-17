//! Deterministic config rendering from a [`FiveGPlan`].
//!
//! Targets Open5GS 2.8.x (`amf.yaml`/`nssf.yaml`/per-slice `smf`/`upf` —
//! validated against live 2.8.0 daemons, 2026-07-14) and UERANSIM 3.2.x
//! (`gnb.yaml`, per-slice `ue.yaml` — pending local build). Every interpolated value
//! is validated by the plan (ids restricted to `[a-zA-Z0-9_-]`, sd to hex,
//! addresses generated), so plain string templates are injection-safe here.
//! Each rendered file must parse as YAML — unit-tested below — and the full
//! set is byte-stable for a given plan (reproducibility rule, §13).

use std::path::Path;

use super::plan::{FiveGPlan, SliceSession};

/// Fixed Milenage test key/OPc used for every rendered UE subscriber
/// (documented as test-only in `render_ue`). Shared with `lifecycle`'s
/// `open5gs-dbctl` calls so the two never drift apart — one constant pair,
/// not two copies that happen to match today.
pub const TEST_UE_KEY: &str = "465B5CE8B199B49FAA5F0A2EE238A6BC";
pub const TEST_UE_OPC: &str = "E8ED289DEBA952E4283B54E88E6183CA";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    /// File name relative to the runtime config dir (e.g. `open5gs/amf.yaml`).
    pub name: String,
    pub contents: String,
}

/// Render and write the full config set under `dir` (created if absent),
/// mirroring each [`ConfigFile::name`]'s relative path. Shared by the
/// `render_fiveg` dev example and the `lifecycle` module — one write path.
pub fn write_all(plan: &FiveGPlan, dir: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut written = Vec::new();
    for f in render_all(plan) {
        let path = dir.join(&f.name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &f.contents)?;
        written.push(path);
    }
    Ok(written)
}

fn snssai_yaml(indent: &str, sst: u8, sd: Option<&str>) -> String {
    match sd {
        Some(sd) => format!("{indent}- sst: {sst}\n{indent}  sd: \"{sd}\""),
        None => format!("{indent}- sst: {sst}"),
    }
}

fn render_amf(plan: &FiveGPlan) -> String {
    let slice_list = plan
        .sessions
        .iter()
        .map(|s| snssai_yaml("        ", s.sst, s.sd.as_deref()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"# rendered by vxedge (FiveGPlan) — do not hand-edit
amf:
  sbi:
    server:
      - address: {amf}
        port: 7777
    client:
      nrf:
        - uri: http://{nrf}:7777
  ngap:
    server:
      - address: {amf_ngap}
  guami:
    - plmn_id: {{ mcc: {mcc}, mnc: {mnc} }}
      amf_id: {{ region: 2, set: 1 }}
  tai:
    - plmn_id: {{ mcc: {mcc}, mnc: {mnc} }}
      tac: {tac}
  plmn_support:
    - plmn_id: {{ mcc: {mcc}, mnc: {mnc} }}
      s_nssai:
{slice_list}
  security:
    integrity_order: [NIA2, NIA1, NIA0]
    ciphering_order: [NEA0, NEA1, NEA2]
  network_name:
    full: vxedge
  amf_name: vxedge-amf
  time:
    t3512:
      value: 540
"#,
        amf = plan.amf_addr,
        amf_ngap = plan.amf_ngap_addr,
        nrf = plan.nrf_addr,
        mcc = plan.mcc,
        mnc = plan.mnc,
        tac = plan.tac,
    )
}

fn render_nssf(plan: &FiveGPlan) -> String {
    // Each nsi entry's s_nssai is a MAPPING, not a list (live 2.8.0 asserts
    // on the list form: `ogs_yaml_iter_key: node->type == YAML_SCALAR_NODE`).
    let nsi_list = plan
        .sessions
        .iter()
        .map(|s| {
            let sd = match s.sd.as_deref() {
                Some(sd) => format!("\n          sd: \"{sd}\""),
                None => String::new(),
            };
            format!(
                "      - uri: http://{nrf}:7777\n        s_nssai:\n          sst: {sst}{sd}",
                nrf = plan.nrf_addr,
                sst = s.sst,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"# rendered by vxedge (FiveGPlan) — do not hand-edit
nssf:
  sbi:
    server:
      - address: {nssf}
        port: 7777
    client:
      nrf:
        - uri: http://{nrf}:7777
      nsi:
{nsi_list}
"#,
        nssf = plan.nssf_addr,
        nrf = plan.nrf_addr,
    )
}

fn render_smf(plan: &FiveGPlan, s: &SliceSession) -> String {
    // `dnn` nests INSIDE each s_nssai entry (live 2.8.0: sibling `dnn` is an
    // unknown key and the slice then has "No DNN").
    let sd_line = match s.sd.as_deref() {
        Some(sd) => format!("\n        sd: \"{sd}\""),
        None => String::new(),
    };
    format!(
        r#"# rendered by vxedge (FiveGPlan) — slice `{id}` — do not hand-edit
smf:
  info:
    - s_nssai:
      - sst: {sst}{sd_line}
        dnn: [{dnn}]
  sbi:
    server:
      - address: {smf}
        port: 7777
    client:
      nrf:
        - uri: http://{nrf}:7777
  pfcp:
    server:
      - address: {smf}
    client:
      upf:
        - address: {upf_pfcp}
          dnn: [{dnn}]
  gtpc:
    server:
      - address: {smf}
  gtpu:
    server:
      - address: {smf}
  session:
    - subnet: {subnet}
      gateway: {gw}
      dnn: {dnn}
  dns: [1.1.1.1]
"#,
        id = s.slice_id,
        sst = s.sst,
        dnn = s.dnn,
        smf = s.smf_addr,
        nrf = plan.nrf_addr,
        upf_pfcp = s.upf_pfcp_addr,
        subnet = s.ue_subnet,
        gw = s.ue_gateway,
    )
}

fn render_upf(s: &SliceSession) -> String {
    format!(
        r#"# rendered by vxedge (FiveGPlan) — slice `{id}` — do not hand-edit
upf:
  pfcp:
    server:
      - address: {pfcp}
  gtpu:
    server:
      - address: {gtpu}
  session:
    - subnet: {subnet}
      gateway: {gw}
      dnn: {dnn}
      dev: {tun}
"#,
        id = s.slice_id,
        pfcp = s.upf_pfcp_addr,
        gtpu = s.upf_gtpu_addr,
        subnet = s.ue_subnet,
        gw = s.ue_gateway,
        dnn = s.dnn,
        tun = s.tun_dev,
    )
}

/// Simple SBI-only NFs (NRF has no client section; the rest are NRF-direct
/// clients; UDR/PCF additionally bind the pod's MongoDB).
fn render_nrf(plan: &FiveGPlan) -> String {
    format!(
        r#"# rendered by vxedge (FiveGPlan) — do not hand-edit
nrf:
  serving:
    - plmn_id: {{ mcc: {mcc}, mnc: {mnc} }}
  sbi:
    server:
      - address: {nrf}
        port: 7777
"#,
        mcc = plan.mcc,
        mnc = plan.mnc,
        nrf = plan.nrf_addr,
    )
}

fn render_simple_nf(name: &str, addr: &str, nrf: &str, db_uri: Option<&str>) -> String {
    let db = match db_uri {
        Some(uri) => format!("\n\ndb_uri: {uri}"),
        None => String::new(),
    };
    format!(
        r#"# rendered by vxedge (FiveGPlan) — do not hand-edit{db}
{name}:
  sbi:
    server:
      - address: {addr}
        port: 7777
    client:
      nrf:
        - uri: http://{nrf}:7777
"#,
    )
}

fn render_gnb(plan: &FiveGPlan) -> String {
    let slice_list = plan
        .sessions
        .iter()
        .map(|s| snssai_yaml("  ", s.sst, s.sd.as_deref()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"# rendered by vxedge (FiveGPlan) — do not hand-edit
mcc: '{mcc}'
mnc: '{mnc}'
nci: '0x000000010'
idLength: 32
tac: {tac}
linkIp: {gnb}
ngapIp: {gnb}
gtpIp: {gnb}
amfConfigs:
  - address: {amf_ngap}
    port: 38412
slices:
{slice_list}
ignoreStreamIds: true
"#,
        mcc = plan.mcc,
        mnc = plan.mnc,
        tac = plan.tac,
        gnb = plan.ran_transit_addr,
        amf_ngap = plan.amf_ngap_addr,
    )
}

fn render_ue(plan: &FiveGPlan, s: &SliceSession) -> String {
    let session_slice = match s.sd.as_deref() {
        Some(sd) => format!("      sst: {}\n      sd: \"{}\"", s.sst, sd),
        None => format!("      sst: {}", s.sst),
    };
    let nssai_entry = snssai_yaml("    ", s.sst, s.sd.as_deref());
    format!(
        r#"# rendered by vxedge (FiveGPlan) — slice `{id}` — do not hand-edit
supi: '{supi}'
mcc: '{mcc}'
mnc: '{mnc}'
protectionScheme: 0
key: '{key}'
op: '{opc}'
opType: 'OPC'
amf: '8000'
imei: '356938035643803'
imeiSv: '4370816125816151'
gnbSearchList:
  - {gnb}
uacAic:
  mps: false
  mcs: false
uacAcc:
  normalClass: 0
  class11: false
  class12: false
  class13: false
  class14: false
  class15: false
sessions:
  - type: 'IPv4'
    apn: '{dnn}'
    slice:
{session_slice}
configured-nssai:
{nssai}
default-nssai:
{nssai}
integrity:
  IA1: true
  IA2: true
  IA3: true
ciphering:
  EA1: true
  EA2: true
  EA3: true
integrityMaxRate:
  uplink: 'full'
  downlink: 'full'
"#,
        id = s.slice_id,
        supi = s.supi,
        mcc = plan.mcc,
        mnc = plan.mnc,
        key = TEST_UE_KEY,
        opc = TEST_UE_OPC,
        gnb = plan.ran_transit_addr,
        dnn = s.dnn,
        nssai = nssai_entry,
    )
}

/// Render the complete config set for a plan: Open5GS AMF + NSSF, one
/// SMF/UPF pair per slice, the UERANSIM gNB, and one UE per slice.
pub fn render_all(plan: &FiveGPlan) -> Vec<ConfigFile> {
    let mut files = vec![
        ConfigFile {
            name: "open5gs/nrf.yaml".into(),
            contents: render_nrf(plan),
        },
        ConfigFile {
            name: "open5gs/amf.yaml".into(),
            contents: render_amf(plan),
        },
        ConfigFile {
            name: "open5gs/nssf.yaml".into(),
            contents: render_nssf(plan),
        },
        ConfigFile {
            name: "open5gs/ausf.yaml".into(),
            contents: render_simple_nf("ausf", &plan.ausf_addr, &plan.nrf_addr, None),
        },
        ConfigFile {
            name: "open5gs/udm.yaml".into(),
            contents: render_simple_nf("udm", &plan.udm_addr, &plan.nrf_addr, None),
        },
        ConfigFile {
            name: "open5gs/udr.yaml".into(),
            contents: render_simple_nf("udr", &plan.udr_addr, &plan.nrf_addr, Some(&plan.db_uri)),
        },
        ConfigFile {
            name: "open5gs/pcf.yaml".into(),
            contents: render_simple_nf("pcf", &plan.pcf_addr, &plan.nrf_addr, Some(&plan.db_uri)),
        },
        ConfigFile {
            name: "open5gs/bsf.yaml".into(),
            contents: render_simple_nf("bsf", &plan.bsf_addr, &plan.nrf_addr, None),
        },
        ConfigFile {
            name: "ueransim/gnb.yaml".into(),
            contents: render_gnb(plan),
        },
    ];
    for s in &plan.sessions {
        files.push(ConfigFile {
            name: format!("open5gs/smf-{}.yaml", s.slice_id),
            contents: render_smf(plan, s),
        });
        files.push(ConfigFile {
            name: format!("open5gs/upf-{}.yaml", s.slice_id),
            contents: render_upf(s),
        });
        files.push(ConfigFile {
            name: format!("ueransim/ue-{}.yaml", s.slice_id),
            contents: render_ue(plan, s),
        });
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ExperimentManifest;

    fn plan() -> FiveGPlan {
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
    fn renders_complete_set_and_every_file_is_valid_yaml() {
        let files = render_all(&plan());
        // 8 core NFs (nrf/amf/nssf/ausf/udm/udr/pcf/bsf) + gnb + 2 x (smf, upf, ue)
        assert_eq!(files.len(), 15);
        for f in &files {
            let parsed: Result<serde_yaml_ng::Value, _> = serde_yaml_ng::from_str(&f.contents);
            assert!(parsed.is_ok(), "{} is not valid YAML: {:?}", f.name, parsed);
        }
    }

    #[test]
    fn amf_advertises_every_snssai() {
        let files = render_all(&plan());
        let amf = &files
            .iter()
            .find(|f| f.name == "open5gs/amf.yaml")
            .unwrap()
            .contents;
        assert!(amf.contains("sst: 1"), "amf lists adas sst");
        assert!(amf.contains("sd: \"000001\""), "amf lists adas sd");
        assert!(amf.contains("sst: 2"), "amf lists infotainment sst");
        assert!(amf.contains("mcc: 999"), "lab PLMN");
    }

    #[test]
    fn per_slice_files_bind_distinct_addresses_and_dnn() {
        let files = render_all(&plan());
        let smf_adas = &files
            .iter()
            .find(|f| f.name == "open5gs/smf-adas.yaml")
            .unwrap()
            .contents;
        let smf_info = &files
            .iter()
            .find(|f| f.name == "open5gs/smf-infotainment.yaml")
            .unwrap()
            .contents;
        assert!(smf_adas.contains("subnet: 10.46.0.0/16"));
        assert!(smf_info.contains("subnet: 10.47.0.0/16"));
        assert!(smf_adas.contains("dnn: adas"));
        assert!(smf_info.contains("dnn: infotainment"));
        assert!(smf_adas.contains("address: 127.0.1.10"));
        assert!(smf_info.contains("address: 127.0.1.11"));
    }

    #[test]
    fn ue_configs_request_their_slice_only() {
        let files = render_all(&plan());
        let ue = &files
            .iter()
            .find(|f| f.name == "ueransim/ue-adas.yaml")
            .unwrap()
            .contents;
        assert!(ue.contains("supi: 'imsi-999700000000001'"));
        assert!(ue.contains("apn: 'adas'"));
        assert!(ue.contains("sd: \"000001\""));
        let ue2 = &files
            .iter()
            .find(|f| f.name == "ueransim/ue-infotainment.yaml")
            .unwrap()
            .contents;
        assert!(ue2.contains("supi: 'imsi-999700000000002'"));
        assert!(!ue2.contains("sd:"), "no sd requested when slice has none");
    }

    #[test]
    fn rendering_is_deterministic() {
        assert_eq!(render_all(&plan()), render_all(&plan()));
    }
}
