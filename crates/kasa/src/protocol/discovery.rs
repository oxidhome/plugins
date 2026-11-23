//! UDP/9999 discovery, shared by both device families.
//!
//! Send the encrypted `{"system":{"get_sysinfo":{}}}` (no length prefix)
//! to port 9999; every Kasa device replies from port 9999 with one datagram
//! holding its encrypted system info. The TS plugin broadcasts, then sends a
//! **paced** unicast probe (3 ms apart) to every host of the local /24,
//! because newer camera firmware (KC420WS) ignores broadcast and a
//! back-to-back burst of 254 datagrams gets dropped. It listens ~2.5 s.

use std::net::Ipv4Addr;

use serde_json::Value;

use super::cipher;
use super::iot::SysInfo;
use super::json::get_str;

/// Discovery port.
pub const DISCOVERY_PORT: u16 = 9999;
/// How long the TS plugin listens after the sweep starts.
pub const DISCOVERY_WINDOW_MS: u64 = 2_500;
/// Gap between paced unicast probes.
pub const UNICAST_PACING_MS: u64 = 3;
/// Largest number of unicast targets one sweep sends to (four /24s; at
/// 3 ms pacing that is ~3 s of probes spread over ~16 ticks).
pub const MAX_SWEEP_HOSTS: usize = 1024;

/// The plaintext probe.
pub const PROBE_JSON: &str = r#"{"system":{"get_sysinfo":{}}}"#;

/// The encrypted probe datagram.
#[must_use]
pub fn probe() -> Vec<u8> {
    cipher::encrypt(PROBE_JSON.as_bytes())
}

/// The device family, from the discovery reply or sysinfo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KasaClass {
    Camera,
    Bulb,
    Plug,
    Switch,
    /// A plug or switch with brightness (HS220, KS230, ...).
    Dimmer,
    /// A multi-outlet strip (HS300, KP303, ...). The TS plugin skips these.
    Strip,
}

impl KasaClass {
    /// Stable storage/metadata name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            KasaClass::Camera => "camera",
            KasaClass::Bulb => "bulb",
            KasaClass::Plug => "plug",
            KasaClass::Switch => "switch",
            KasaClass::Dimmer => "dimmer",
            KasaClass::Strip => "strip",
        }
    }

    /// Inverse of [`KasaClass::as_str`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "camera" => KasaClass::Camera,
            "bulb" => KasaClass::Bulb,
            "plug" => KasaClass::Plug,
            "switch" => KasaClass::Switch,
            "dimmer" => KasaClass::Dimmer,
            "strip" => KasaClass::Strip,
            _ => return None,
        })
    }
}

/// Classifies a device by its sysinfo, as `classifyKasa` + `isDimmer` in
/// the TS plugin, plus strips. `None` for families not modeled (hubs, ...).
#[must_use]
pub fn classify(s: &SysInfo) -> Option<KasaClass> {
    match s.kind.as_deref()? {
        "IOT.IPCAMERA" => Some(KasaClass::Camera),
        "IOT.SMARTBULB" => Some(KasaClass::Bulb),
        "IOT.SMARTPLUGSWITCH" => {
            if !s.children.is_empty() {
                return Some(KasaClass::Strip);
            }
            if is_dimmer(s) {
                return Some(KasaClass::Dimmer);
            }
            let dev_name = s
                .dev_name
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if dev_name.contains("switch") || dev_name.contains("dimmer") {
                Some(KasaClass::Switch)
            } else {
                Some(KasaClass::Plug)
            }
        }
        _ => None,
    }
}

/// A plug or switch is a dimmer when it reports `brightness`, `feature`
/// contains `DIM`, or `dev_name` contains "dimmer". The last check is for
/// the KS230 3-way dimmer, which reports neither of the first two while idle.
#[must_use]
pub fn is_dimmer(s: &SysInfo) -> bool {
    s.brightness.is_some()
        || s.feature.as_deref().is_some_and(|f| f.contains("DIM"))
        || s.dev_name
            .as_deref()
            .is_some_and(|d| d.to_ascii_lowercase().contains("dimmer"))
}

/// One parsed discovery reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Discovered {
    pub address: Ipv4Addr,
    /// `deviceId`, else the MAC: the device's identity.
    pub id: String,
    pub class: Option<KasaClass>,
    pub sysinfo: SysInfo,
}

/// Decrypts and parses one reply datagram. `None` for anything that isn't
/// a recognizable Kasa reply.
#[must_use]
pub fn parse_reply(address: Ipv4Addr, datagram: &[u8]) -> Option<Discovered> {
    let plain = cipher::decrypt(datagram);
    let json: Value = serde_json::from_slice(&plain).ok()?;
    let best = extract_identity(&json)?;
    let sysinfo = SysInfo::parse(best);
    let id = sysinfo
        .device_id
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| sysinfo.mac.clone().filter(|s| !s.is_empty()))?;
    Some(Discovered {
        address,
        id,
        class: classify(&sysinfo),
        sysinfo,
    })
}

/// Cameras answer with a differently shaped object than smart-home devices;
/// their identity sits at another depth with other names. Walk the tree up
/// to depth 4 and take the object with the most identifying fields, as
/// `extractSysInfo` does.
fn extract_identity(root: &Value) -> Option<&Value> {
    fn walk<'a>(node: &'a Value, depth: usize, out: &mut Vec<&'a Value>) {
        if depth > 4 {
            return;
        }
        // Like `Object.values`, arrays are walked too.
        match node {
            Value::Object(map) => {
                if ["model", "alias", "deviceId", "dev_id"]
                    .iter()
                    .any(|k| get_str(node, k).is_some())
                {
                    out.push(node);
                }
                for v in map.values() {
                    walk(v, depth + 1, out);
                }
            }
            Value::Array(list) => {
                for v in list {
                    walk(v, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    let mut candidates = Vec::new();
    walk(root, 0, &mut candidates);
    // Stable: on a tie the first (shallowest, earliest) candidate wins, as
    // with the TS plugin's stable sort.
    let mut best: Option<(&Value, usize)> = None;
    for c in candidates {
        let n = field_count(c);
        if best.is_none_or(|(_, b)| n > b) {
            best = Some((c, n));
        }
    }
    best.map(|(v, _)| v)
}

fn field_count(o: &Value) -> usize {
    let has = |k: &str| o.get(k).is_some_and(truthy);
    [
        has("model"),
        has("alias"),
        has("deviceId") || has("dev_id"),
        has("mac") || has("mic_mac"),
        has("type") || has("mic_type"),
    ]
    .iter()
    .filter(|b| **b)
    .count()
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => true,
    }
}

/// A unicast sweep target: an IPv4 network of /24 or narrower, or one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepTarget {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

impl SweepTarget {
    /// Parses `a.b.c.d` or `a.b.c.d/nn` (24 ≤ nn ≤ 32). Wider networks are
    /// refused: sweeping a /23 or larger would flood the LAN, which is why
    /// the TS plugin skips them.
    ///
    /// # Errors
    ///
    /// A message naming the problem.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (
                a,
                p.parse::<u8>()
                    .map_err(|_| format!("bad prefix length in {s:?}"))?,
            ),
            None => (s, 32),
        };
        let ip: Ipv4Addr = addr
            .parse()
            .map_err(|_| format!("{s:?} is not an IPv4 address or network"))?;
        if !(24..=32).contains(&prefix) {
            return Err(format!(
                "{s:?}: only /24 or narrower networks are swept (a wider sweep floods the LAN)"
            ));
        }
        let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
        Ok(Self {
            network: Ipv4Addr::from(u32::from(ip) & mask),
            prefix,
        })
    }

    /// Host addresses: for /31 and /32 every address, otherwise every
    /// address but the network and broadcast ones.
    pub fn hosts(self) -> impl Iterator<Item = Ipv4Addr> {
        let base = u32::from(self.network);
        let size = 1u32 << (32 - u32::from(self.prefix));
        let (first, last) = if size <= 2 {
            (base, base + size - 1)
        } else {
            (base + 1, base + size - 2)
        };
        (first..=last).map(Ipv4Addr::from)
    }
}

/// The deduplicated unicast sweep list for a set of targets, capped at
/// [`MAX_SWEEP_HOSTS`] per sweep.
#[must_use]
pub fn sweep_hosts(targets: &[SweepTarget]) -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for t in targets {
        for h in t.hosts() {
            if out.len() >= MAX_SWEEP_HOSTS {
                return out;
            }
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn datagram(v: &Value) -> Vec<u8> {
        cipher::encrypt(v.to_string().as_bytes())
    }

    const IP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);

    #[test]
    fn probe_is_the_known_vector() {
        assert_eq!(cipher::decrypt(&probe()), PROBE_JSON.as_bytes());
        assert_eq!(probe().len(), 29);
    }

    #[test]
    fn parses_plug_reply() {
        let d = parse_reply(
            IP,
            &datagram(&json!({"system":{"get_sysinfo":{"alias":"Garage","model":"EP25(US)",
                "deviceId":"8006AA","mac":"54:AF:97:2D:AA:24","type":"IOT.SMARTPLUGSWITCH",
                "relay_state":0,"feature":"TIM:ENE","dev_name":"Smart Wi-Fi Plug Mini","err_code":0}}})),
        )
        .unwrap();
        assert_eq!(d.id, "8006AA");
        assert_eq!(d.class, Some(KasaClass::Plug));
        assert_eq!(d.address, IP);
    }

    #[test]
    fn parses_camera_reply_with_mic_fields_at_depth() {
        // Cameras nest identity deeper and use mic_mac / mic_type.
        let v = json!({"system":{"get_sysinfo":{"err_code":0,"system":{
            "model":"KC420WS(US)","alias":"Yard","dev_id":"80DEAD",
            "mic_mac":"503DD1D278D4","mic_type":"IOT.IPCAMERA","sw_ver":"2.3.26"}}}});
        let d = parse_reply(IP, &datagram(&v)).unwrap();
        assert_eq!(d.id, "80DEAD");
        assert_eq!(d.class, Some(KasaClass::Camera));
        assert_eq!(d.sysinfo.mac.as_deref(), Some("503DD1D278D4"));
        assert_eq!(d.sysinfo.sw_ver.as_deref(), Some("2.3.26"));
    }

    #[test]
    fn walks_arrays_and_skips_empty_ids() {
        let v = json!({"result":[{"model":"KP115","alias":"A","deviceId":"","mac":"AA:BB",
            "type":"IOT.SMARTPLUGSWITCH"}]});
        let d = parse_reply(IP, &datagram(&v)).unwrap();
        assert_eq!(d.id, "AA:BB");
        assert_eq!(d.class, Some(KasaClass::Plug));
    }

    #[test]
    fn prefers_the_richest_candidate() {
        let v = json!({"a":{"alias":"decoy"},"b":{"c":{"alias":"Real","model":"KL130","deviceId":"X1","type":"IOT.SMARTBULB"}}});
        let d = parse_reply(IP, &datagram(&v)).unwrap();
        assert_eq!(d.sysinfo.alias.as_deref(), Some("Real"));
        assert_eq!(d.class, Some(KasaClass::Bulb));
    }

    #[test]
    fn falls_back_to_mac_identity_and_rejects_junk() {
        let v = json!({"system":{"get_sysinfo":{"alias":"x","mac":"AA:BB"}}});
        assert_eq!(parse_reply(IP, &datagram(&v)).unwrap().id, "AA:BB");
        let anon = json!({"system":{"get_sysinfo":{"alias":"x"}}});
        assert!(parse_reply(IP, &datagram(&anon)).is_none());
        assert!(parse_reply(IP, b"\x00\x01garbage").is_none());
        assert!(parse_reply(IP, &[]).is_none());
        // Too deep to be found.
        let deep = json!({"a":{"b":{"c":{"d":{"e":{"alias":"x","deviceId":"y"}}}}}});
        assert!(parse_reply(IP, &datagram(&deep)).is_none());
    }

    fn info(v: &Value) -> SysInfo {
        SysInfo::parse(v)
    }

    #[test]
    fn classifies_like_the_ts_plugin() {
        let plug = info(&json!({"type":"IOT.SMARTPLUGSWITCH","dev_name":"Smart Wi-Fi Plug"}));
        assert_eq!(classify(&plug), Some(KasaClass::Plug));
        let sw = info(&json!({"type":"IOT.SMARTPLUGSWITCH","dev_name":"Smart Wi-Fi Light Switch"}));
        assert_eq!(classify(&sw), Some(KasaClass::Switch));
        let ks230 =
            info(&json!({"type":"IOT.SMARTPLUGSWITCH","dev_name":"Wi-Fi Smart 3-Way Dimmer"}));
        assert_eq!(classify(&ks230), Some(KasaClass::Dimmer));
        let hs220 = info(&json!({"type":"IOT.SMARTPLUGSWITCH","brightness":40,"dev_name":"x"}));
        assert_eq!(classify(&hs220), Some(KasaClass::Dimmer));
        let dim_feature = info(&json!({"type":"IOT.SMARTPLUGSWITCH","feature":"TIM:DIM"}));
        assert_eq!(classify(&dim_feature), Some(KasaClass::Dimmer));
        let strip =
            info(&json!({"type":"IOT.SMARTPLUGSWITCH","deviceId":"P","children":[{"id":"00"}]}));
        assert_eq!(classify(&strip), Some(KasaClass::Strip));
        assert_eq!(
            classify(&info(&json!({"type":"IOT.SMARTBULB"}))),
            Some(KasaClass::Bulb)
        );
        assert_eq!(classify(&info(&json!({"type":"IOT.HUB"}))), None);
        assert_eq!(classify(&info(&json!({}))), None);
        for c in [
            KasaClass::Camera,
            KasaClass::Bulb,
            KasaClass::Plug,
            KasaClass::Switch,
            KasaClass::Dimmer,
            KasaClass::Strip,
        ] {
            assert_eq!(KasaClass::parse(c.as_str()), Some(c));
        }
        assert_eq!(KasaClass::parse("hub"), None);
    }

    #[test]
    fn sweep_targets_are_capped_at_a_slash_24() {
        let t = SweepTarget::parse("192.168.1.77/24").unwrap();
        assert_eq!(t.network, Ipv4Addr::new(192, 168, 1, 0));
        let hosts: Vec<_> = t.hosts().collect();
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(192, 168, 1, 254));
        assert!(SweepTarget::parse("10.0.0.0/23").is_err());
        assert!(SweepTarget::parse("10.0.0.0/33").is_err());
        assert!(SweepTarget::parse("10.0.0.0/x").is_err());
        assert!(SweepTarget::parse("host.local").is_err());
        let one = SweepTarget::parse(" 10.0.0.9 ").unwrap();
        assert_eq!(
            one.hosts().collect::<Vec<_>>(),
            vec![Ipv4Addr::new(10, 0, 0, 9)]
        );
        let pair = SweepTarget::parse("10.0.0.8/31").unwrap();
        assert_eq!(pair.hosts().count(), 2);
    }

    #[test]
    fn sweep_list_dedups_and_caps() {
        let a = SweepTarget::parse("10.0.0.0/24").unwrap();
        let b = SweepTarget::parse("10.0.0.5").unwrap();
        let c = SweepTarget::parse("10.0.1.0/24").unwrap();
        let list = sweep_hosts(&[b, a, c]);
        assert_eq!(list.len(), 254 + 254);
        let many: Vec<SweepTarget> = (0..6)
            .map(|i| SweepTarget::parse(&format!("10.0.{i}.0/24")).unwrap())
            .collect();
        assert_eq!(sweep_hosts(&many).len(), MAX_SWEEP_HOSTS);
        assert_eq!(list[0], Ipv4Addr::new(10, 0, 0, 5));
        assert_eq!(
            list.iter()
                .filter(|h| **h == Ipv4Addr::new(10, 0, 0, 5))
                .count(),
            1
        );
    }
}
