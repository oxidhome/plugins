//! Instance configuration, read through `host-config`. Field names match
//! `[config.*]` in `manifest.toml`.

use std::net::Ipv4Addr;

use crate::protocol::discovery::{MAX_SWEEP_HOSTS, SweepTarget};
use oxidhome_sdk::bindings::oxidhome::plugin::types::Value;

/// Resolved configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Run UDP discovery at all.
    pub discovery: bool,
    /// Send the probe to `255.255.255.255`.
    pub broadcast: bool,
    /// Networks (/24 or narrower) and hosts to sweep with paced unicast.
    pub sweep: Vec<SweepTarget>,
    /// Re-discovery cadence; 0 = only at start.
    pub discovery_interval_ms: u64,
    /// State poll cadence for plugs, switches, bulbs and camera controls.
    pub poll_interval_ms: u64,
    /// Kasa account email (camera control and stream auth).
    pub camera_username: Option<String>,
    /// Kasa account password (`type = "secret"`).
    pub camera_password: Option<String>,
    /// Poll the camera's own motion detection once a second.
    pub camera_motion: bool,
    /// Config problems, logged once at init.
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            discovery: true,
            broadcast: true,
            sweep: Vec::new(),
            discovery_interval_ms: 300_000,
            poll_interval_ms: 30_000,
            camera_username: None,
            camera_password: None,
            camera_motion: false,
            warnings: Vec::new(),
        }
    }
}

/// Smallest accepted poll interval: the TS plugin polls every 30 s and the
/// repo guidelines ask for ≥ 30 s defaults; 5 s is the floor for users who
/// tune it.
pub const MIN_POLL_INTERVAL_S: i64 = 5;
/// Smallest re-discovery interval (a sweep takes a few seconds of traffic).
pub const MIN_DISCOVERY_INTERVAL_S: i64 = 60;

impl Config {
    /// Builds the config from a key lookup (`host-config::get-config`).
    pub fn load(mut get: impl FnMut(&str) -> Option<Value>) -> Self {
        let mut c = Config::default();
        let mut bool_of = |key: &str, default: bool, warnings: &mut Vec<String>| match get(key) {
            None => default,
            Some(Value::BoolVal(b)) => b,
            Some(_) => {
                warnings.push(format!("config {key}: expected a bool"));
                default
            }
        };
        let mut w = Vec::new();
        c.discovery = bool_of("discovery", true, &mut w);
        c.broadcast = bool_of("discovery_broadcast", true, &mut w);
        c.camera_motion = bool_of("camera_motion", false, &mut w);
        if let Some(v) = int(get("discovery_interval_s"), "discovery_interval_s", &mut w) {
            c.discovery_interval_ms = if v <= 0 {
                0
            } else {
                secs_to_ms(v.max(MIN_DISCOVERY_INTERVAL_S))
            };
        }
        if let Some(v) = int(get("poll_interval_s"), "poll_interval_s", &mut w) {
            c.poll_interval_ms = secs_to_ms(v.max(MIN_POLL_INTERVAL_S));
        }
        if let Some(s) = string(get("discovery_targets"), "discovery_targets", &mut w) {
            for item in s.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                match SweepTarget::parse(item) {
                    Ok(t) => c.sweep.push(t),
                    Err(e) => w.push(format!("config discovery_targets: {e}")),
                }
            }
        }
        // Distinct hosts, counted only until one past the cap.
        let mut seen = std::collections::HashSet::new();
        for h in c.sweep.iter().flat_map(|t| t.hosts()) {
            if seen.insert(h) && seen.len() > MAX_SWEEP_HOSTS {
                break;
            }
        }
        if seen.len() > MAX_SWEEP_HOSTS
            && let Some(last) = crate::protocol::discovery::sweep_hosts(&c.sweep).last()
        {
            w.push(format!(
                "config discovery_targets: more than {MAX_SWEEP_HOSTS} hosts; each sweep stops at {last}"
            ));
        }
        c.camera_username =
            string(get("camera_username"), "camera_username", &mut w).filter(|s| !s.is_empty());
        c.camera_password =
            string(get("camera_password"), "camera_password", &mut w).filter(|s| !s.is_empty());
        c.warnings = w;
        c
    }

    /// Both camera credentials, when set.
    #[must_use]
    pub fn camera_credentials(&self) -> Option<(&str, &str)> {
        Some((
            self.camera_username.as_deref()?,
            self.camera_password.as_deref()?,
        ))
    }

    /// Whether `ip` is inside a configured sweep target.
    #[must_use]
    pub fn in_sweep(&self, ip: Ipv4Addr) -> bool {
        self.sweep.iter().any(|t| t.hosts().any(|h| h == ip))
    }
}

fn secs_to_ms(s: i64) -> u64 {
    u64::try_from(s).unwrap_or(0).saturating_mul(1000)
}

fn int(v: Option<Value>, key: &str, w: &mut Vec<String>) -> Option<i64> {
    if let Value::IntVal(i) = v? {
        Some(i)
    } else {
        w.push(format!("config {key}: expected an int"));
        None
    }
}

fn string(v: Option<Value>, key: &str, w: &mut Vec<String>) -> Option<String> {
    if let Value::StringVal(s) = v? {
        Some(s)
    } else {
        w.push(format!("config {key}: expected a string"));
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(pairs: &[(&str, Value)]) -> Config {
        let map: HashMap<String, Value> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        Config::load(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let c = load(&[]);
        assert!(c.discovery && c.broadcast && !c.camera_motion);
        assert_eq!(c.poll_interval_ms, 30_000);
        assert_eq!(c.discovery_interval_ms, 300_000);
        assert_eq!(c.camera_credentials(), None);
        assert_eq!(c.warnings, Vec::<String>::new());
    }

    #[test]
    fn parses_and_clamps() {
        let c = load(&[
            ("poll_interval_s", Value::IntVal(1)),
            ("discovery_interval_s", Value::IntVal(0)),
            (
                "discovery_targets",
                Value::StringVal("192.168.1.0/24, 10.0.0.7,bad,10.0.0.0/16".into()),
            ),
            ("camera_username", Value::StringVal("me@example.com".into())),
            ("camera_password", Value::StringVal("pw".into())),
            ("camera_motion", Value::BoolVal(true)),
            ("discovery", Value::StringVal("yes".into())),
        ]);
        assert_eq!(c.poll_interval_ms, 5_000);
        assert_eq!(c.discovery_interval_ms, 0);
        assert_eq!(c.sweep.len(), 2);
        assert!(c.in_sweep(Ipv4Addr::new(192, 168, 1, 9)));
        assert!(c.in_sweep(Ipv4Addr::new(10, 0, 0, 7)));
        assert!(!c.in_sweep(Ipv4Addr::new(10, 0, 0, 8)));
        assert_eq!(c.camera_credentials(), Some(("me@example.com", "pw")));
        assert!(c.camera_motion);
        assert!(c.discovery, "a mistyped value keeps the default");
        assert_eq!(c.warnings.len(), 3, "{:?}", c.warnings);
        let short = load(&[("discovery_interval_s", Value::IntVal(10))]);
        assert_eq!(short.discovery_interval_ms, 60_000);
        let bad = load(&[
            ("poll_interval_s", Value::StringVal("x".into())),
            ("camera_username", Value::IntVal(1)),
            ("camera_password", Value::StringVal(String::new())),
        ]);
        assert_eq!(bad.warnings.len(), 2);
        assert_eq!(bad.camera_password, None);
        // Five /24s exceed one sweep's host cap: said once, at load.
        let wide = load(&[(
            "discovery_targets",
            Value::StringVal("10.0.1.0/24,10.0.2.0/24,10.0.3.0/24,10.0.4.0/24,10.0.5.0/24".into()),
        )]);
        assert_eq!(wide.warnings.len(), 1, "{:?}", wide.warnings);
        assert!(wide.warnings[0].contains("10.0.5.8"), "{:?}", wide.warnings);
        let four = load(&[(
            "discovery_targets",
            Value::StringVal("10.0.1.0/24,10.0.2.0/24,10.0.3.0/24,10.0.4.0/24".into()),
        )]);
        assert_eq!(four.warnings, Vec::<String>::new());
    }
}
