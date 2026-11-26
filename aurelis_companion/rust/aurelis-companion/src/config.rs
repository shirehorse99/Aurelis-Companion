//! Settings, from the environment and (for the Home Assistant add-on) `/data/options.json`.
//! Environment wins over the options file.

use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub data_dir: PathBuf,
    pub picture_dir: Option<PathBuf>,
    pub port: u16,
    pub disk_budget_mb: Option<u64>,
    pub picture_store_enabled: bool,
    /// Whether this Companion runs AI jobs a television hands over (default on).
    pub ai_runner: bool,
    /// Whether this Companion relays televisions' Betfair calls to Betfair's two hosts (default on).
    pub betfair_relay: bool,
    /// Whether this Companion accepts scheduled DVR captures (default on).
    pub dvr_enabled: bool,
    /// Whether this Companion relays live streams for multi-room fan-out (default on).
    pub stream_relay: bool,
    /// Whether this Companion maintains rolling timeshift buffers for live rewind (default on).
    pub timeshift_enabled: bool,
    /// Minutes of rolling timeshift buffer to retain per active channel (default 60).
    pub timeshift_minutes: u64,
    /// Whether this Companion supports on-the-fly stream transcoding/remuxing (default on).
    pub transcode_enabled: bool,
    /// Whether this Companion accepts offline catch-up and episode staging requests (default on).
    pub stage_enabled: bool,
    /// Whether this Companion manages the household tuner lease pool (default on).
    pub tuner_enabled: bool,
    /// Whether Home Assistant integration (media_player bridge and notifications) is enabled (default on).
    pub homeassistant_enabled: bool,
    /// Home Assistant Core API URL (e.g. http://supervisor/core/api or http://192.168.1.60:8123/api).
    pub hass_url: Option<String>,
    /// Home Assistant Long-Lived Token or SUPERVISOR_TOKEN.
    pub hass_token: Option<String>,
}

pub const DEFAULT_PORT: u16 = 8765;

impl Config {
    /// Reads the real environment and `<data dir>/options.json` when it exists.
    pub fn load() -> Result<Config, String> {
        let env = |k: &str| std::env::var(k).ok();
        // The same reading as `from_sources`, so the options file is looked for where the config says.
        let data_dir = env("AURELIS_DATA_DIR").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "/data".to_string());
        let options_path = Path::new(&data_dir).join("options.json");
        let options = match std::fs::read_to_string(&options_path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("cannot read {}: {e}", options_path.display())),
        };
        Config::from_sources(&env, options.as_deref())
    }

    /// Pure: `env` looks a variable up, `options_json` is the options file's text if there is one.
    pub fn from_sources(env: &dyn Fn(&str) -> Option<String>, options_json: Option<&str>) -> Result<Config, String> {
        let options: Value = match options_json {
            Some(text) => serde_json::from_str(text).map_err(|e| format!("options.json is not valid JSON: {e}"))?,
            None => Value::Null,
        };
        if !options.is_null() && !options.is_object() {
            return Err("options.json must hold a JSON object".to_string());
        }
        // An absent or null option is "not set"; any other wrong type is refused with its name.
        let opt = |k: &str| options.get(k).filter(|v| !v.is_null());
        let opt_str = |k: &str| -> Result<Option<String>, String> {
            match opt(k) {
                None => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
                Some(other) => Err(format!("options.json '{k}' must be text, found {other}")),
            }
        };
        let opt_u64 = |k: &str| -> Result<Option<u64>, String> {
            match opt(k) {
                None => Ok(None),
                Some(v) => v.as_u64().map(Some).ok_or_else(|| format!("options.json '{k}' must be a whole number, found {v}")),
            }
        };
        let env_str = |k: &str| env(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

        let data_dir = PathBuf::from(env_str("AURELIS_DATA_DIR").unwrap_or_else(|| "/data".to_string()));
        let port = match env_str("AURELIS_PORT") {
            Some(p) => p.parse::<u16>().map_err(|_| format!("AURELIS_PORT '{p}' is not a port number"))?,
            None => match opt_u64("port")? {
                Some(p) => u16::try_from(p).map_err(|_| format!("options.json 'port' {p} is not a port number"))?,
                None => DEFAULT_PORT,
            },
        };
        let picture_dir = match env_str("AURELIS_PICTURE_DIR") {
            Some(d) => Some(d),
            None => opt_str("picture_dir")?,
        }
        .map(PathBuf::from);
        let disk_budget_mb = match env_str("AURELIS_DISK_BUDGET_MB") {
            Some(b) => Some(b.parse::<u64>().map_err(|_| format!("AURELIS_DISK_BUDGET_MB '{b}' is not a number"))?),
            None => opt_u64("disk_budget_mb")?,
        };
        // 0 means "automatic", the same as not set (the Home Assistant add-on ships `disk_budget_mb: 0`).
        let disk_budget_mb = disk_budget_mb.filter(|b| *b > 0);
        // One reader for every on/off switch: the environment wins, then the options file, then the default.
        let switch = |env_name: &str, option: &str, default: bool| -> Result<bool, String> {
            match env_str(env_name) {
                Some(v) if v.eq_ignore_ascii_case("on") => Ok(true),
                Some(v) if v.eq_ignore_ascii_case("off") => Ok(false),
                Some(v) => Err(format!("{env_name} '{v}' must be 'on' or 'off'")),
                None => match opt(option) {
                    None => Ok(default),
                    Some(Value::Bool(b)) => Ok(*b),
                    Some(Value::String(s)) if s.eq_ignore_ascii_case("on") => Ok(true),
                    Some(Value::String(s)) if s.eq_ignore_ascii_case("off") => Ok(false),
                    Some(other) => Err(format!("options.json '{option}' must be true, false, 'on' or 'off', found {other}")),
                },
            }
        };
        let picture_store_enabled = switch("AURELIS_PICTURES", "pictures", false)?;
        let ai_runner = switch("AURELIS_AI_RUNNER", "ai_runner", true)?;
        let betfair_relay = switch("AURELIS_BETFAIR_RELAY", "betfair_relay", true)?;
        let dvr_enabled = switch("AURELIS_DVR", "dvr", true)?;
        let stream_relay = switch("AURELIS_STREAM_RELAY", "stream_relay", true)?;
        let timeshift_enabled = switch("AURELIS_TIMESHIFT", "timeshift", true)?;
        let timeshift_minutes = match env_str("AURELIS_TIMESHIFT_MINUTES") {
            Some(v) => v.parse::<u64>().map_err(|_| format!("AURELIS_TIMESHIFT_MINUTES '{v}' is not a number"))?,
            None => match opt_u64("timeshift_minutes")? {
                Some(m) if m > 0 => m,
                _ => 60,
            },
        };
        let transcode_enabled = switch("AURELIS_TRANSCODE", "transcode", true)?;
        let stage_enabled = switch("AURELIS_STAGE", "stage", true)?;
        let tuner_enabled = switch("AURELIS_TUNER", "tuner", true)?;
        let homeassistant_enabled = switch("AURELIS_HOMEASSISTANT", "homeassistant", true)?;
        let hass_url = env_str("HASS_URL").or_else(|| opt("hass_url").and_then(|v| v.as_str().map(String::from)))
            .or_else(|| if env_str("SUPERVISOR_TOKEN").is_some() { Some("http://supervisor/core/api".to_string()) } else { None });
        let hass_token = env_str("HASS_TOKEN").or_else(|| env_str("SUPERVISOR_TOKEN"))
            .or_else(|| opt("hass_token").and_then(|v| v.as_str().map(String::from)));
        Ok(Config { data_dir, picture_dir, port, disk_budget_mb, picture_store_enabled, ai_runner, betfair_relay, dvr_enabled, stream_relay, timeshift_enabled, timeshift_minutes, transcode_enabled, stage_enabled, tuner_enabled, homeassistant_enabled, hass_url, hass_token })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn defaults() {
        let c = Config::from_sources(&env_of(&[]), None).unwrap();
        assert_eq!(c.data_dir, PathBuf::from("/data"));
        assert_eq!(c.port, 8765);
        assert!(!c.picture_store_enabled && c.picture_dir.is_none() && c.disk_budget_mb.is_none());
        assert!(c.dvr_enabled);
    }

    #[test]
    fn env_overrides_options() {
        let opts = r#"{"port": 9000, "pictures": true, "picture_dir": "/media/pics", "disk_budget_mb": 5000}"#;
        let c = Config::from_sources(&env_of(&[("AURELIS_PORT", "9100")]), Some(opts)).unwrap();
        assert_eq!(c.port, 9100);
        assert!(c.picture_store_enabled);
        assert_eq!(c.picture_dir, Some(PathBuf::from("/media/pics")));
        assert_eq!(c.disk_budget_mb, Some(5000));
    }

    #[test]
    fn bad_values_are_refused_with_a_reason() {
        assert!(Config::from_sources(&env_of(&[("AURELIS_PORT", "x")]), None).unwrap_err().contains("AURELIS_PORT"));
        assert!(Config::from_sources(&env_of(&[]), Some("{")).unwrap_err().contains("options.json"));
    }

    #[test]
    fn wrongly_typed_options_are_refused_by_name() {
        for (json, name) in [
            (r#"{"port":"9000"}"#, "'port'"),
            (r#"{"port":70000}"#, "'port'"),
            (r#"{"disk_budget_mb":"5000"}"#, "'disk_budget_mb'"),
            (r#"{"pictures":1}"#, "'pictures'"),
            (r#"{"picture_dir":5}"#, "'picture_dir'"),
            ("[1]", "object"),
        ] {
            let e = Config::from_sources(&env_of(&[]), Some(json)).unwrap_err();
            assert!(e.contains(name), "{json} -> {e}");
        }
    }

    #[test]
    fn a_budget_of_zero_means_automatic_in_both_places() {
        assert_eq!(Config::from_sources(&env_of(&[]), Some(r#"{"disk_budget_mb":0}"#)).unwrap().disk_budget_mb, None);
        assert_eq!(Config::from_sources(&env_of(&[("AURELIS_DISK_BUDGET_MB", "0")]), None).unwrap().disk_budget_mb, None);
        // The environment's 0 wins over the file's number: both then mean "automatic".
        assert_eq!(Config::from_sources(&env_of(&[("AURELIS_DISK_BUDGET_MB", "0")]), Some(r#"{"disk_budget_mb":5000}"#)).unwrap().disk_budget_mb, None);
    }

    #[test]
    fn pictures_env_must_be_on_or_off_and_null_options_are_unset() {
        assert!(Config::from_sources(&env_of(&[("AURELIS_PICTURES", "yes")]), None).unwrap_err().contains("AURELIS_PICTURES"));
        assert!(!Config::from_sources(&env_of(&[("AURELIS_PICTURES", "off")]), Some(r#"{"pictures":true}"#)).unwrap().picture_store_enabled);
        let c = Config::from_sources(&env_of(&[]), Some(r#"{"port":null,"picture_dir":null,"pictures":"on"}"#)).unwrap();
        assert_eq!((c.port, c.picture_dir, c.picture_store_enabled), (8765, None, true));
    }

    #[test]
    fn ai_runner_defaults_on_and_follows_env_then_options() {
        assert!(Config::from_sources(&env_of(&[]), None).unwrap().ai_runner);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"ai_runner":false}"#)).unwrap().ai_runner);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"ai_runner":"off"}"#)).unwrap().ai_runner);
        assert!(Config::from_sources(&env_of(&[("AURELIS_AI_RUNNER", "on")]), Some(r#"{"ai_runner":false}"#)).unwrap().ai_runner);
        assert!(Config::from_sources(&env_of(&[("AURELIS_AI_RUNNER", "maybe")]), None).unwrap_err().contains("AURELIS_AI_RUNNER"));
        assert!(Config::from_sources(&env_of(&[]), Some(r#"{"ai_runner":3}"#)).unwrap_err().contains("'ai_runner'"));
    }

    #[test]
    fn betfair_relay_defaults_on_and_follows_env_then_options() {
        assert!(Config::from_sources(&env_of(&[]), None).unwrap().betfair_relay);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"betfair_relay":false}"#)).unwrap().betfair_relay);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"betfair_relay":"off"}"#)).unwrap().betfair_relay);
        assert!(Config::from_sources(&env_of(&[("AURELIS_BETFAIR_RELAY", "on")]), Some(r#"{"betfair_relay":false}"#)).unwrap().betfair_relay);
        assert!(!Config::from_sources(&env_of(&[("AURELIS_BETFAIR_RELAY", "OFF")]), None).unwrap().betfair_relay);
        assert!(Config::from_sources(&env_of(&[("AURELIS_BETFAIR_RELAY", "maybe")]), None).unwrap_err().contains("AURELIS_BETFAIR_RELAY"));
        assert!(Config::from_sources(&env_of(&[]), Some(r#"{"betfair_relay":3}"#)).unwrap_err().contains("'betfair_relay'"));
    }

    #[test]
    fn dvr_defaults_on_and_follows_env_then_options() {
        assert!(Config::from_sources(&env_of(&[]), None).unwrap().dvr_enabled);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"dvr":false}"#)).unwrap().dvr_enabled);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"dvr":"off"}"#)).unwrap().dvr_enabled);
        assert!(Config::from_sources(&env_of(&[("AURELIS_DVR", "on")]), Some(r#"{"dvr":false}"#)).unwrap().dvr_enabled);
        assert!(!Config::from_sources(&env_of(&[("AURELIS_DVR", "OFF")]), None).unwrap().dvr_enabled);
        assert!(Config::from_sources(&env_of(&[("AURELIS_DVR", "maybe")]), None).unwrap_err().contains("AURELIS_DVR"));
        assert!(Config::from_sources(&env_of(&[]), Some(r#"{"dvr":3}"#)).unwrap_err().contains("'dvr'"));
    }

    #[test]
    fn stream_relay_defaults_on_and_follows_env_then_options() {
        assert!(Config::from_sources(&env_of(&[]), None).unwrap().stream_relay);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"stream_relay":false}"#)).unwrap().stream_relay);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"stream_relay":"off"}"#)).unwrap().stream_relay);
        assert!(Config::from_sources(&env_of(&[("AURELIS_STREAM_RELAY", "on")]), Some(r#"{"stream_relay":false}"#)).unwrap().stream_relay);
        assert!(!Config::from_sources(&env_of(&[("AURELIS_STREAM_RELAY", "OFF")]), None).unwrap().stream_relay);
        assert!(Config::from_sources(&env_of(&[("AURELIS_STREAM_RELAY", "maybe")]), None).unwrap_err().contains("AURELIS_STREAM_RELAY"));
        assert!(Config::from_sources(&env_of(&[]), Some(r#"{"stream_relay":3}"#)).unwrap_err().contains("'stream_relay'"));
    }

    #[test]
    fn transcode_defaults_on_and_follows_env_then_options() {
        assert!(Config::from_sources(&env_of(&[]), None).unwrap().transcode_enabled);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"transcode":false}"#)).unwrap().transcode_enabled);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"transcode":"off"}"#)).unwrap().transcode_enabled);
        assert!(Config::from_sources(&env_of(&[("AURELIS_TRANSCODE", "on")]), Some(r#"{"transcode":false}"#)).unwrap().transcode_enabled);
        assert!(!Config::from_sources(&env_of(&[("AURELIS_TRANSCODE", "OFF")]), None).unwrap().transcode_enabled);
        assert!(Config::from_sources(&env_of(&[("AURELIS_TRANSCODE", "maybe")]), None).unwrap_err().contains("AURELIS_TRANSCODE"));
        assert!(Config::from_sources(&env_of(&[]), Some(r#"{"transcode":3}"#)).unwrap_err().contains("'transcode'"));
    }

    #[test]
    fn stage_defaults_on_and_follows_env_then_options() {
        assert!(Config::from_sources(&env_of(&[]), None).unwrap().stage_enabled);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"stage":false}"#)).unwrap().stage_enabled);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"stage":"off"}"#)).unwrap().stage_enabled);
        assert!(Config::from_sources(&env_of(&[("AURELIS_STAGE", "on")]), Some(r#"{"stage":false}"#)).unwrap().stage_enabled);
        assert!(!Config::from_sources(&env_of(&[("AURELIS_STAGE", "OFF")]), None).unwrap().stage_enabled);
        assert!(Config::from_sources(&env_of(&[("AURELIS_STAGE", "maybe")]), None).unwrap_err().contains("AURELIS_STAGE"));
        assert!(Config::from_sources(&env_of(&[]), Some(r#"{"stage":3}"#)).unwrap_err().contains("'stage'"));
    }

    #[test]
    fn tuner_defaults_on_and_follows_env_then_options() {
        assert!(Config::from_sources(&env_of(&[]), None).unwrap().tuner_enabled);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"tuner":false}"#)).unwrap().tuner_enabled);
        assert!(!Config::from_sources(&env_of(&[]), Some(r#"{"tuner":"off"}"#)).unwrap().tuner_enabled);
        assert!(Config::from_sources(&env_of(&[("AURELIS_TUNER", "on")]), Some(r#"{"tuner":false}"#)).unwrap().tuner_enabled);
        assert!(!Config::from_sources(&env_of(&[("AURELIS_TUNER", "OFF")]), None).unwrap().tuner_enabled);
        assert!(Config::from_sources(&env_of(&[("AURELIS_TUNER", "maybe")]), None).unwrap_err().contains("AURELIS_TUNER"));
        assert!(Config::from_sources(&env_of(&[]), Some(r#"{"tuner":3}"#)).unwrap_err().contains("'tuner'"));
    }

    #[test]
    fn homeassistant_defaults_on_and_resolves_supervisor_token() {
        let c = Config::from_sources(&env_of(&[]), None).unwrap();
        assert!(c.homeassistant_enabled);
        assert_eq!(c.hass_url, None);
        assert_eq!(c.hass_token, None);

        let c_sup = Config::from_sources(&env_of(&[("SUPERVISOR_TOKEN", "token123")]), None).unwrap();
        assert!(c_sup.homeassistant_enabled);
        assert_eq!(c_sup.hass_url.as_deref(), Some("http://supervisor/core/api"));
        assert_eq!(c_sup.hass_token.as_deref(), Some("token123"));

        let c_custom = Config::from_sources(&env_of(&[("HASS_URL", "http://192.168.1.60:8123/api"), ("HASS_TOKEN", "secret")]), None).unwrap();
        assert_eq!(c_custom.hass_url.as_deref(), Some("http://192.168.1.60:8123/api"));
        assert_eq!(c_custom.hass_token.as_deref(), Some("secret"));
    }

    #[test]
    fn the_add_on_defaults_are_read_as_unset_not_as_errors() {
        // Exactly what companion/ha-addon/config.yaml ships as `options`.
        let c = Config::from_sources(&env_of(&[]), Some(r#"{"pictures": false, "picture_dir": "", "disk_budget_mb": 0, "ai_runner": true, "betfair_relay": true}"#)).unwrap();
        assert!(!c.picture_store_enabled && c.ai_runner && c.betfair_relay);
        assert_eq!(c.picture_dir, None);
        assert_eq!(c.disk_budget_mb, None);
    }
}
