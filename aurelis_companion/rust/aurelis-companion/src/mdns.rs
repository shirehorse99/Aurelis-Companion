//! The LAN advert: `_aurelis-companion._tcp` on the Companion's port, so a television finds it
//! without an address being typed. TXT `v=1` (protocol version) and `fp=` (the paired household's
//! fingerprint, empty while unpaired). The advert is re-announced when the pairing state changes.
//!
//! If multicast is not available (Docker without host networking, a locked-down VLAN) the advert
//! cannot start; that is one printed line and the Companion carries on serving by address.

use mdns_sd::{ServiceDaemon, ServiceInfo};

pub const SERVICE_TYPE: &str = "_aurelis-companion._tcp.local.";
/// The name a person sees in a service browser. mdns-sd adds a suffix itself on a name clash.
pub const INSTANCE_NAME: &str = "Aurelis Companion";

/// The TXT records, in order. `fp` is present but empty when unpaired, so a browser can tell
/// "unpaired" from "an old Companion that does not say".
pub fn txt_records(fingerprint: Option<&str>) -> Vec<(String, String)> {
    vec![
        ("v".to_string(), crate::state::PROTOCOL_VERSION.to_string()),
        ("fp".to_string(), fingerprint.unwrap_or("").to_string()),
    ]
}

fn host_name() -> String {
    let name = sysinfo::System::host_name().unwrap_or_default();
    let name = name.trim().trim_end_matches('.').trim_end_matches(".local");
    let clean: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    format!("{}.local.", if clean.is_empty() { "aurelis-companion" } else { &clean })
}

pub struct Advert {
    daemon: ServiceDaemon,
    port: u16,
    host: String,
}

impl Advert {
    /// Starts advertising. The error says why, in words, and is meant to be printed once.
    pub fn start(port: u16, fingerprint: Option<&str>) -> Result<Advert, String> {
        let daemon = ServiceDaemon::new().map_err(|e| format!("the mDNS responder could not start: {e}"))?;
        let advert = Advert { daemon, port, host: host_name() };
        advert.announce(fingerprint)?;
        Ok(advert)
    }

    fn announce(&self, fingerprint: Option<&str>) -> Result<(), String> {
        let txt = txt_records(fingerprint);
        let props: Vec<(&str, &str)> = txt.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        // No address given: `enable_addr_auto` keeps it to the machine's current interface addresses.
        let info = ServiceInfo::new(SERVICE_TYPE, INSTANCE_NAME, &self.host, "", self.port, &props[..])
            .map_err(|e| format!("the mDNS record could not be built: {e}"))?
            .enable_addr_auto();
        self.daemon.register(info).map_err(|e| format!("the mDNS record could not be announced: {e}"))
    }

    /// The pairing state changed: announce the new TXT records.
    pub fn update(&self, fingerprint: Option<&str>) -> Result<(), String> {
        self.announce(fingerprint)
    }

    pub fn stop(&self) {
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_records_carry_version_and_fingerprint() {
        assert_eq!(txt_records(Some("a1b2c3")), vec![("v".to_string(), "1".to_string()), ("fp".to_string(), "a1b2c3".to_string())]);
        assert_eq!(txt_records(None), vec![("v".to_string(), "1".to_string()), ("fp".to_string(), String::new())]);
    }

    #[test]
    fn the_service_type_and_name_are_the_agreed_ones() {
        assert_eq!(SERVICE_TYPE, "_aurelis-companion._tcp.local.");
        assert_eq!(INSTANCE_NAME, "Aurelis Companion");
    }

    #[test]
    fn host_name_is_a_valid_local_name() {
        let h = host_name();
        assert!(h.ends_with(".local.") && h.len() > ".local.".len(), "{h}");
    }
}
