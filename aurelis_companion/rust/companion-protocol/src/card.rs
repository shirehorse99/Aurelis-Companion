//! `GET /v1/card` — unsealed, no secrets.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    pub protocol: Vec<u32>,
    pub version: String,
    pub host: Host,
    pub state: String,
    pub busy_reason: Option<String>,
    pub paired: bool,
    pub household_fingerprint: Option<String>,
    pub capabilities: Capabilities,
    pub declined: BTreeMap<String, String>,
    pub disk: Disk,
    pub assessed_at_ms: i64,
    /// When the master captured the household bundle this Companion holds; null when none is held.
    /// Absent from an older Companion's card, which reads as none.
    #[serde(default)]
    pub household_bundle_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    pub arch: String,
    pub ram_mb: u64,
    pub free_ram_mb: u64,
    pub disk_free_mb: u64,
    pub on_sd_card: bool,
    pub bench_score: u64,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub ai_store: bool,
    pub guide_store: bool,
    pub picture_store: bool,
    pub ai_runner: bool,
    pub heavy_pass: bool,
    #[serde(default)]
    pub dvr: bool,
    #[serde(default)]
    pub stream_relay: bool,
    #[serde(default)]
    pub timeshift: bool,
    #[serde(default)]
    pub transcode: bool,
    #[serde(default)]
    pub stage: bool,
    #[serde(default)]
    pub tuner: bool,
    #[serde(default)]
    pub homeassistant: bool,
    #[serde(default)]
    pub notifications: bool,
    #[serde(default)]
    pub lineup: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    pub budget_mb: u64,
    pub used_mb: UsedMb,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsedMb {
    pub ai: u64,
    pub guide: u64,
    pub picture: u64,
    #[serde(default)]
    pub dvr: u64,
    #[serde(default)]
    pub stage: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"{"protocol":[1],"version":"0.1.0","host":{"arch":"amd64","ramMb":3870,"freeRamMb":1410,"diskFreeMb":402000,"onSdCard":false,"benchScore":812,"label":"Intel NUC (HA OS)"},
 "state":"available","busyReason":null,"paired":true,"householdFingerprint":"a1b2c3",
 "capabilities":{"aiStore":true,"guideStore":true,"pictureStore":false,"aiRunner":false,"heavyPass":false,"dvr":false,"streamRelay":false,"timeshift":false,"transcode":false,"stage":false,"tuner":false,"homeassistant":false,"notifications":false,"lineup":false},
 "declined":{"pictureStore":"turned off until the household enables it","aiRunner":"stage 2","heavyPass":"stage 3"},
 "disk":{"budgetMb":20480,"usedMb":{"ai":3,"guide":180,"picture":0,"dvr":0,"stage":0}},"assessedAtMs":0,"householdBundleAtMs":null}"#;

    #[test]
    fn example_card_round_trips() {
        let card: Card = serde_json::from_str(EXAMPLE).expect("example deserializes");
        assert_eq!(card.host.ram_mb, 3870);
        assert_eq!(card.busy_reason, None);
        assert!(!card.capabilities.dvr);
        assert!(!card.capabilities.stream_relay);
        assert!(!card.capabilities.timeshift);
        assert!(!card.capabilities.transcode);
        assert!(!card.capabilities.stage);
        assert!(!card.capabilities.tuner);
        assert!(!card.capabilities.homeassistant);
        assert!(!card.capabilities.notifications);
        assert!(!card.capabilities.lineup);
        assert_eq!(card.disk.used_mb.dvr, 0);
        assert_eq!(card.disk.used_mb.stage, 0);
        let again = serde_json::to_value(&card).unwrap();
        let original: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
        assert_eq!(again, original);
    }

    #[test]
    fn older_card_without_dvr_deserializes() {
        let older = r#"{"protocol":[1],"version":"0.1.0","host":{"arch":"amd64","ramMb":3870,"freeRamMb":1410,"diskFreeMb":402000,"onSdCard":false,"benchScore":812,"label":"Intel NUC (HA OS)"},
 "state":"available","busyReason":null,"paired":true,"householdFingerprint":"a1b2c3",
 "capabilities":{"aiStore":true,"guideStore":true,"pictureStore":false,"aiRunner":false,"heavyPass":false},
 "declined":{"pictureStore":"turned off until the household enables it","aiRunner":"stage 2","heavyPass":"stage 3"},
 "disk":{"budgetMb":20480,"usedMb":{"ai":3,"guide":180,"picture":0}},"assessedAtMs":0,"householdBundleAtMs":null}"#;
        let card: Card = serde_json::from_str(older).expect("older card deserializes");
        assert!(!card.capabilities.dvr);
        assert!(!card.capabilities.stream_relay);
        assert!(!card.capabilities.timeshift);
        assert!(!card.capabilities.transcode);
        assert!(!card.capabilities.stage);
        assert!(!card.capabilities.tuner);
        assert!(!card.capabilities.homeassistant);
        assert!(!card.capabilities.notifications);
        assert!(!card.capabilities.lineup);
        assert_eq!(card.disk.used_mb.dvr, 0);
        assert_eq!(card.disk.used_mb.stage, 0);
    }
}
