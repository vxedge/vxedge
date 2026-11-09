//! **Slice queue counters** (`ARCHITECTURE.md` §12): each slice's HTB class on the uplink, the
//! shared bottleneck, as `tc -s -j class show` reports it.

use super::netops::{NetOpsError, Shell};

/// One HTB class's counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassStats {
    /// `1:10`, as `tc` prints a class id.
    pub handle: String,
    pub bytes: u64,
    pub packets: u64,
    pub drops: u64,
    pub overlimits: u64,
    /// Bytes queued now.
    pub backlog: u64,
    /// Packets queued now.
    pub qlen: u64,
}

/// Every class on `dev`, read through the daemon's own shell (so inside `VXEDGE_NETNS` when set).
pub fn class_stats(sh: &Shell, dev: &str) -> Result<Vec<ClassStats>, NetOpsError> {
    let out = sh.run("tc", &["-s", "-j", "class", "show", "dev", dev])?;
    Ok(parse(&out))
}

/// `tc -s -j class show` output. A field `tc` leaves out reads as 0; output that is not JSON
/// reads as no classes.
pub fn parse(json: &str) -> Vec<ClassStats> {
    let Ok(serde_json::Value::Array(classes)) = serde_json::from_str(json.trim()) else {
        return Vec::new();
    };
    classes
        .iter()
        .filter_map(|c| {
            let handle = c["handle"].as_str()?.to_string();
            let stat = |k: &str| c["stats"][k].as_u64().unwrap_or(0);
            Some(ClassStats {
                handle,
                bytes: stat("bytes"),
                packets: stat("packets"),
                drops: stat("drops"),
                overlimits: stat("overlimits"),
                backlog: stat("backlog"),
                qlen: stat("qlen"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn htb_classes_parse_from_tc_json() {
        let json = r#"[{"class":"htb","handle":"1:1","root":true,"rate":62500000,
            "stats":{"bytes":9000,"packets":6,"drops":0,"overlimits":0,"requeues":0,"backlog":0,"qlen":0}},
            {"class":"htb","handle":"1:10","parent":"1:1","prio":1,
            "stats":{"bytes":1500,"packets":1,"drops":2,"overlimits":3,"requeues":0,"backlog":3000,"qlen":2}}]"#;
        let c = parse(json);
        assert_eq!(c.len(), 2);
        assert_eq!(
            c[1],
            ClassStats {
                handle: "1:10".into(),
                bytes: 1500,
                packets: 1,
                drops: 2,
                overlimits: 3,
                backlog: 3000,
                qlen: 2
            }
        );
        assert!(parse("").is_empty());
        assert!(parse("Cannot find device").is_empty());
    }
}
