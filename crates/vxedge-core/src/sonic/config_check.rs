//! The SONiC-specific business logic built on top of [`super::lifecycle::Console`]'s generic
//! serial driver: log in, repurpose whatever ports a rendered config's own `VLAN_MEMBER`
//! entries name, load the config, and read every declared field back to confirm it matches
//! exactly. This is the native Rust port of the sequence `vxparts/parts/gw/nos/os/sonic-vs/
//! verify-config-parity.py` already proved correct by hand — same steps, same reasoning,
//! ported so `vxedged` needs no Python dependency to run it itself.
//!
//! Kept out of [`super::lifecycle`] deliberately: that module's own doc comment promises it
//! "knows nothing about SONiC's own CLI, login flow, or config format" — this module is
//! exactly the caller that concern was left to.

use std::time::Duration;

use super::lifecycle::{Console, NosCheckError};
use super::render::ConfigDb;

const PROMPT: &str = "admin@sonic:~$";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// One declared field that didn't come back as declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigParityMismatch {
    /// The full CONFIG_DB key, e.g. `VLAN|Vlan101` or `VLAN_MEMBER|Vlan101|Ethernet0`.
    pub key: String,
    pub field: String,
    pub declared: String,
    /// What was actually read back — empty if the key/field wasn't found at all, not just if
    /// its value differed.
    pub live: String,
}

/// The result of one config-parity run: how many fields were declared, and which (if any)
/// didn't come back matching.
#[derive(Debug, Clone, Default)]
pub struct ConfigParityReport {
    pub checked: usize,
    pub mismatches: Vec<ConfigParityMismatch>,
}

impl ConfigParityReport {
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Logs into an already-booted, already-connected, not-yet-authenticated console with SONiC-VS's
/// own default credentials. Its own function (not folded into [`verify_config_parity`]) so a
/// caller that only needs to confirm the image boots and accepts a login — `tests/
/// nos_check_lifecycle.rs`'s own privileged test — can reuse the exact same sequence instead of
/// keeping a second copy of it.
pub fn login(console: &mut Console, boot_timeout: Duration) -> Result<(), NosCheckError> {
    console.expect("login:", boot_timeout)?;
    console.send_line("admin")?;
    console.expect("assword:", DEFAULT_TIMEOUT)?;
    console.send_line("YourPaSsWoRd")?;
    console.expect(PROMPT, DEFAULT_TIMEOUT)?;
    Ok(())
}

/// Loads `config` via SONiC's own `config load` on an already-logged-in console (see [`login`]),
/// and reads every declared `VLAN`/`VLAN_MEMBER` field back to confirm it matches exactly.
pub fn verify_config_parity(
    console: &mut Console,
    config: &ConfigDb,
) -> Result<ConfigParityReport, NosCheckError> {
    // SONiC-VS's default config pre-assigns every front-panel port as a routed L3 interface
    // (a default T1 leaf-fabric topology) — a port can't be both a routed interface and a VLAN
    // member simultaneously, so whichever ports this config's own VLAN_MEMBER entries name get
    // repurposed first, the same step a real operator would take. Derived from the config
    // itself, not hardcoded, so this works for whatever ports a given plan actually declares.
    let mut ports: Vec<&str> = config
        .vlan_member
        .keys()
        .filter_map(|k| k.split_once('|').map(|(_, port)| port))
        .collect();
    ports.sort_unstable();
    ports.dedup();
    for port in &ports {
        console.send_line(&format!(
            "for ip in $(redis-cli -n 4 --scan --pattern 'INTERFACE|{port}|*' | cut -d'|' -f3); \
             do sudo config interface ip remove {port} \"$ip\"; done"
        ))?;
        console.expect(PROMPT, DEFAULT_TIMEOUT)?;
    }

    let json = serde_json::to_string_pretty(config).map_err(|e| NosCheckError::ConsoleIo {
        name: "config_check".to_string(),
        source: std::io::Error::other(e.to_string()),
    })?;
    console.send_line("cat > /tmp/vxedge-nos-check.json << 'VXEOF'")?;
    for line in json.lines() {
        console.send_line(line)?;
    }
    console.send_line("VXEOF")?;
    console.expect(PROMPT, DEFAULT_TIMEOUT)?;

    console.send_line("sudo config load /tmp/vxedge-nos-check.json -y")?;
    console.expect(PROMPT, Duration::from_secs(30))?;

    let mut report = ConfigParityReport::default();
    for (vlan_id, decl) in &config.vlan {
        report.checked += 1;
        console.send_line(&format!("redis-cli -n 4 hget 'VLAN|{vlan_id}' vlanid"))?;
        let transcript = console.expect(PROMPT, DEFAULT_TIMEOUT)?;
        let live = extract_quoted_reply(&transcript).unwrap_or_default();
        if live != decl.vlanid {
            report.mismatches.push(ConfigParityMismatch {
                key: format!("VLAN|{vlan_id}"),
                field: "vlanid".into(),
                declared: decl.vlanid.clone(),
                live,
            });
        }
    }
    for (member_key, decl) in &config.vlan_member {
        report.checked += 1;
        console.send_line(&format!(
            "redis-cli -n 4 hget 'VLAN_MEMBER|{member_key}' tagging_mode"
        ))?;
        let transcript = console.expect(PROMPT, DEFAULT_TIMEOUT)?;
        let live = extract_quoted_reply(&transcript).unwrap_or_default();
        if live != decl.tagging_mode {
            report.mismatches.push(ConfigParityMismatch {
                key: format!("VLAN_MEMBER|{member_key}"),
                field: "tagging_mode".into(),
                declared: decl.tagging_mode.to_string(),
                live,
            });
        }
    }
    Ok(report)
}

/// Pulls the first double-quoted substring out of a console transcript — `redis-cli`'s own
/// reply format (`"101"`), distinguishable from the command line that produced it because every
/// command this module sends quotes its own redis keys with *single* quotes, never double.
fn extract_quoted_reply(transcript: &str) -> Option<String> {
    let start = transcript.find('"')?;
    let rest = &transcript[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_quoted_reply_ignoring_single_quoted_command_text() {
        let transcript = "redis-cli -n 4 hget 'VLAN|Vlan101' vlanid\r\n\
             redis-cli -n 4 hget 'VLAN|Vlan101' vlanid\r\n\
             \"101\"\r\n\
             admin@sonic:~$ ";
        assert_eq!(extract_quoted_reply(transcript).as_deref(), Some("101"));
    }

    #[test]
    fn no_quoted_reply_is_none() {
        assert_eq!(extract_quoted_reply("admin@sonic:~$ "), None);
    }
}
