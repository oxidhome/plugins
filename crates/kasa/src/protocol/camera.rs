//! Camera control models on top of [`super::linkie`]: status LED,
//! spotlight, siren, camera on/off (privacy), pan/tilt, motion state,
//! reboot, and the snapshot endpoint.

use serde_json::{Value, json};

use super::http::{HttpRequest, HttpResponse, Method};
use super::json::{get_i64, get_str};
use super::linkie::{LinkieError, LinkieRequest, MODULE_PREFIX, ok_result, result};

/// Stream port: live stream, snapshot, audio-only stream.
pub const STREAM_PORT: u16 = 19443;
/// Talk (speaker) port.
pub const TALK_PORT: u16 = 18443;
/// The one live stream a camera serves (a second client stalls in TLS).
pub const STREAM_PATH: &str = "/https/stream/mixed";
/// Still image path, 640x360, works alongside the live stream.
pub const SNAPSHOT_PATH: &str = "/https/stream/snapshot.jpg";
/// Snapshot timeout in the TS plugin.
pub const SNAPSHOT_TIMEOUT_MS: u64 = 10_000;
/// A real snapshot is ~60–110 KB; anything far larger isn't one.
pub const SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// While-motion hold: `is_moving` drops ~2 s after the last alarm, so a
/// person standing still flickers it.
pub const MOTION_HOLD_MS: u64 = 10_000;
/// Motion poll cadence.
pub const MOTION_POLL_MS: u64 = 1_000;
/// Failed motion polls back off up to this.
pub const MOTION_POLL_MAX_BACKOFF_MS: u64 = 30_000;
/// Motion poll cadence while the camera is off.
pub const MOTION_POLL_PRIVACY_MS: u64 = 5_000;

/// The on/off features that share one shape: read returns
/// `{"value":"on"|"off"}`, write takes `{"value":"on"|"off"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnOffFeature {
    /// Status LED on the camera body.
    Led,
    /// "Spotlight" in the app (KC420WS); `dayNight.force_lamp_state`.
    Spotlight,
    /// Siren. The camera stops it by itself after the duration set in the
    /// app (default 30 s), so a read shortly after may say `off`.
    Siren,
    /// The camera's own on/off. `off` is "camera off" / privacy: video stops
    /// for every client, the stream answers 503, other calls may return
    /// `-40919`.
    CameraEnabled,
}

impl OnOffFeature {
    /// All features, in probe order.
    pub const ALL: [OnOffFeature; 4] = [
        OnOffFeature::Led,
        OnOffFeature::Spotlight,
        OnOffFeature::Siren,
        OnOffFeature::CameraEnabled,
    ];

    /// `(module, read method, write method)`, module relative to the prefix.
    #[must_use]
    pub fn methods(self) -> (&'static str, &'static str, &'static str) {
        match self {
            OnOffFeature::Led => ("led", "get_status", "set_status"),
            OnOffFeature::Spotlight => ("dayNight", "get_force_lamp_state", "set_force_lamp_state"),
            OnOffFeature::Siren => ("siren", "get_state", "set_state"),
            OnOffFeature::CameraEnabled => ("switch", "get_is_enable", "set_is_enable"),
        }
    }

    fn full_module(self) -> String {
        format!("{MODULE_PREFIX}{}", self.methods().0)
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// A read of one feature.
#[must_use]
pub fn get_state(f: OnOffFeature) -> LinkieRequest {
    let (m, get, _) = f.methods();
    LinkieRequest::new().camera(m, get, json!({}))
}

/// One request that reads every on/off feature plus the PTZ capability.
/// Several modules can go in one request and a module the camera lacks
/// answers `-10008` without failing the others. (The TS plugin probes them
/// one request at a time.)
#[must_use]
pub fn probe_all() -> LinkieRequest {
    let mut r = LinkieRequest::new();
    for f in OnOffFeature::ALL {
        let (m, get, _) = f.methods();
        r = r.camera(m, get, json!({}));
    }
    r.camera("ptz", "get_capability", json!({}))
}

/// A write of one feature.
#[must_use]
pub fn set_state(f: OnOffFeature, on: bool) -> LinkieRequest {
    let (m, _, set) = f.methods();
    LinkieRequest::new().camera(m, set, json!({ "value": on_off(on) }))
}

/// Parses a feature read. `None` means the camera lacks the feature (module
/// missing or an error) — a successful read doubles as feature detection.
/// Like the TS `getState`, the value decides, not `err_code`.
#[must_use]
pub fn parse_state(reply: &Value, f: OnOffFeature) -> Option<bool> {
    let (_, get, _) = f.methods();
    match get_str(result(reply, &f.full_module(), get).ok()?, "value")? {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// Checks a feature write succeeded (`err_code == 0`).
///
/// # Errors
///
/// [`LinkieError`].
pub fn check_set(reply: &Value, f: OnOffFeature) -> Result<(), LinkieError> {
    let (_, _, set) = f.methods();
    ok_result(reply, &f.full_module(), set).map(|_| ())
}

/// `system.set_reboot` under the camera prefix. (`system.reboot` doesn't
/// exist: `-10008`.) The camera drops off ~20 s later and is back after
/// ~30 s; it's the fix for a dead stream server on 19443.
#[must_use]
pub fn reboot() -> LinkieRequest {
    LinkieRequest::new().camera("system", "set_reboot", json!({}))
}

/// Checks a reboot was accepted.
///
/// # Errors
///
/// [`LinkieError`].
pub fn check_reboot(reply: &Value) -> Result<(), LinkieError> {
    ok_result(reply, &format!("{MODULE_PREFIX}system"), "set_reboot").map(|_| ())
}

/// Top-level `system.get_sysinfo` (not under the prefix): identity plus
/// `led_status` and `camera_switch` in one reply.
#[must_use]
pub fn get_sysinfo() -> LinkieRequest {
    LinkieRequest::new().raw("system", "get_sysinfo", json!({}))
}

/// Pan/tilt range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtzCapability {
    pub max_x: i64,
    pub max_y: i64,
}

/// Parses `ptz.get_capability`. Fixed-lens models answer `-10008`, so
/// `None` doubles as "no pan/tilt".
#[must_use]
pub fn parse_ptz_capability(reply: &Value) -> Option<PtzCapability> {
    let v = ok_result(reply, &format!("{MODULE_PREFIX}ptz"), "get_capability").ok()?;
    Some(PtzCapability {
        max_x: get_i64(v, "max_x")?,
        max_y: get_i64(v, "max_y")?,
    })
}

/// `ptz.get_all_preset`.
#[must_use]
pub fn get_presets() -> LinkieRequest {
    LinkieRequest::new().camera("ptz", "get_all_preset", json!({}))
}

/// A saved viewpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtzPreset {
    /// 1-based, as `set_run_to_preset` takes it.
    pub index: i64,
    pub name: String,
}

/// Parses `ptz.get_all_preset`. The entry shape isn't documented: read
/// `index` and `name` when present, else use the 1-based position.
#[must_use]
pub fn parse_presets(reply: &Value) -> Option<Vec<PtzPreset>> {
    let v = ok_result(reply, &format!("{MODULE_PREFIX}ptz"), "get_all_preset").ok()?;
    let Some(Value::Array(list)) = v.get("preset_attr") else {
        return None;
    };
    // Bounded: the list is published in one state field, which the host
    // caps.
    let presets = list
        .iter()
        .take(MAX_PRESETS)
        .enumerate()
        .map(|(i, p)| {
            let index = p
                .get("index")
                .and_then(Value::as_i64)
                .unwrap_or_else(|| i64::try_from(i + 1).unwrap_or(i64::MAX));
            let name = get_str(p, "name")
                .filter(|n| !n.trim().is_empty())
                .map_or_else(
                    || format!("Viewpoint {index}"),
                    |n| n.chars().take(MAX_PRESET_NAME).collect(),
                );
            PtzPreset { index, name }
        })
        .collect();
    Some(presets)
}

/// Most presets kept per camera (the app allows far fewer).
pub const MAX_PRESETS: usize = 32;
/// Longest preset name kept, in characters.
pub const MAX_PRESET_NAME: usize = 64;

/// `set_move` directions: lowercase, `top`/`bottom` (not up/down, which
/// answer `-41203`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtzDirection {
    Left,
    Right,
    Top,
    Bottom,
}

impl PtzDirection {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PtzDirection::Left => "left",
            PtzDirection::Right => "right",
            PtzDirection::Top => "top",
            PtzDirection::Bottom => "bottom",
        }
    }

    /// Parses a direction; also accepts `up`/`down` and maps them.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "left" => PtzDirection::Left,
            "right" => PtzDirection::Right,
            "top" | "up" => PtzDirection::Top,
            "bottom" | "down" => PtzDirection::Bottom,
            _ => return None,
        })
    }
}

/// Maps a -1..1 magnitude onto Kasa's 1..10 step size: `ceil(|v| * 10)`,
/// the same mapping the TS plugin (and Reolink plugin) uses.
#[must_use]
pub fn ptz_speed(v: f64) -> u8 {
    if !v.is_finite() {
        return 1;
    }
    let steps = (v.abs() * 10.0).ceil().clamp(1.0, 10.0);
    // Clamped to 1..=10 above, so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let s = steps as u8;
    s
}

/// `ptz.set_move`. `speed` is a step size, not a rate: one discrete step,
/// then the camera stops. 0 or >10 answers `-41203`.
#[must_use]
pub fn ptz_move(direction: PtzDirection, speed: u8) -> LinkieRequest {
    LinkieRequest::new().camera(
        "ptz",
        "set_move",
        json!({ "direction": direction.as_str(), "speed": speed.clamp(1, 10) }),
    )
}

/// `ptz.set_run_to_preset` (1-based index).
#[must_use]
pub fn ptz_goto_preset(index: i64) -> LinkieRequest {
    LinkieRequest::new().camera("ptz", "set_run_to_preset", json!({ "index": index }))
}

/// Checks a PTZ command.
///
/// # Errors
///
/// [`LinkieError`]; `-41203` is a bad argument or an end stop.
pub fn check_ptz(reply: &Value, method: &str) -> Result<(), LinkieError> {
    ok_result(reply, &format!("{MODULE_PREFIX}ptz"), method).map(|_| ())
}

/// `vod.get_detect_event_state`. The firmware wants `timestamp` (Unix
/// seconds, after 2019-05-01) directly in the method object; a missing one
/// is reported as `[start_time] is required` (-40700).
#[must_use]
pub fn get_motion(unix_seconds: u64) -> LinkieRequest {
    LinkieRequest::new().camera(
        "vod",
        "get_detect_event_state",
        json!({ "timestamp": unix_seconds }),
    )
}

/// Parses the motion reply: `is_moving` is true while an alarm fired in
/// the last ~2 s (motion, person, baby cry, doorbell press; not sound).
///
/// # Errors
///
/// [`LinkieError`]. A missing `err_code` or `-40709` happen sporadically
/// and are transient; `-40919` means the camera is off.
pub fn parse_motion(reply: &Value) -> Result<bool, LinkieError> {
    let v = ok_result(
        reply,
        &format!("{MODULE_PREFIX}vod"),
        "get_detect_event_state",
    )?;
    Ok(v.get("is_moving") == Some(&Value::Bool(true)))
}

/// Debounces `is_moving` into a held motion state.
#[derive(Debug, Clone, Default)]
pub struct MotionHold {
    last_true_ms: Option<u64>,
    detected: bool,
}

impl MotionHold {
    /// Feeds one poll result at monotonic `now_ms`; returns the new state
    /// when it changed.
    pub fn observe(&mut self, moving: bool, now_ms: u64) -> Option<bool> {
        if moving {
            self.last_true_ms = Some(now_ms);
        }
        self.settle(now_ms)
    }

    /// Re-evaluates the hold without a new poll (e.g. on a tick).
    pub fn settle(&mut self, now_ms: u64) -> Option<bool> {
        let held = self
            .last_true_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < MOTION_HOLD_MS);
        if held == self.detected {
            None
        } else {
            self.detected = held;
            Some(held)
        }
    }

    /// Clears motion (camera off, polling stopped); returns `Some(false)`
    /// when it was on.
    pub fn clear(&mut self) -> Option<bool> {
        self.last_true_ms = None;
        if self.detected {
            self.detected = false;
            Some(false)
        } else {
            None
        }
    }

    /// Current held state.
    #[must_use]
    pub fn detected(&self) -> bool {
        self.detected
    }
}

/// Exponential backoff for failed motion polls.
#[must_use]
pub fn next_motion_backoff(current_ms: u64) -> u64 {
    (current_ms.saturating_mul(2)).clamp(MOTION_POLL_MS, MOTION_POLL_MAX_BACKOFF_MS)
}

/// `GET /https/stream/snapshot.jpg` with stream credentials.
#[must_use]
pub fn snapshot_request(host: &str, port: u16, stream_auth: &str) -> HttpRequest {
    HttpRequest {
        method: Method::Get,
        host: host.into(),
        port,
        path: SNAPSHOT_PATH.into(),
        headers: vec![
            ("Authorization".into(), stream_auth.into()),
            ("Accept".into(), "image/*".into()),
        ],
        body: Vec::new(),
    }
}

/// Why a snapshot failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// HTTP 503: the camera is turned off.
    CameraOff,
    /// Another non-200 status.
    Http(u16),
    /// Over [`SNAPSHOT_MAX_BYTES`].
    TooLarge(usize),
    /// No JPEG start marker (the camera labels it `image/jpg`, so the body
    /// is checked rather than the header).
    NotJpeg,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::CameraOff => f.write_str("camera is turned off (HTTP 503)"),
            SnapshotError::Http(s) => write!(f, "snapshot HTTP {s}"),
            SnapshotError::TooLarge(n) => write!(f, "snapshot of {n} bytes is too large"),
            SnapshotError::NotJpeg => f.write_str("snapshot is not a JPEG"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Validates a snapshot response and returns the JPEG.
///
/// # Errors
///
/// [`SnapshotError`].
pub fn parse_snapshot(resp: HttpResponse) -> Result<Vec<u8>, SnapshotError> {
    match resp.status {
        200 => {}
        503 => return Err(SnapshotError::CameraOff),
        s => return Err(SnapshotError::Http(s)),
    }
    if resp.body.len() > SNAPSHOT_MAX_BYTES {
        return Err(SnapshotError::TooLarge(resp.body.len()));
    }
    if !resp.body.starts_with(&[0xff, 0xd8]) {
        return Err(SnapshotError::NotJpeg);
    }
    Ok(resp.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::linkie::err;

    fn reply_for(f: OnOffFeature, value: &str) -> Value {
        let (m, get, _) = f.methods();
        json!({ format!("{MODULE_PREFIX}{m}"): { get: { "value": value, "err_code": 0 } } })
    }

    #[test]
    fn feature_reads_and_writes() {
        assert_eq!(
            set_state(OnOffFeature::Spotlight, true).to_json(),
            r#"{"smartlife.cam.ipcamera.dayNight":{"set_force_lamp_state":{"value":"on"}}}"#
        );
        assert_eq!(
            set_state(OnOffFeature::CameraEnabled, false).to_json(),
            r#"{"smartlife.cam.ipcamera.switch":{"set_is_enable":{"value":"off"}}}"#
        );
        assert_eq!(
            get_state(OnOffFeature::Led).to_json(),
            r#"{"smartlife.cam.ipcamera.led":{"get_status":{}}}"#
        );
        for f in OnOffFeature::ALL {
            assert_eq!(parse_state(&reply_for(f, "on"), f), Some(true));
            assert_eq!(parse_state(&reply_for(f, "off"), f), Some(false));
            assert_eq!(parse_state(&reply_for(f, "auto"), f), None);
            assert_eq!(parse_state(&json!({}), f), None);
        }
        let unsupported =
            json!({"smartlife.cam.ipcamera.siren":{"get_state":{"err_code":err::UNSUPPORTED}}});
        assert_eq!(parse_state(&unsupported, OnOffFeature::Siren), None);
    }

    #[test]
    fn probe_batches_one_method_per_module() {
        let r = probe_all();
        assert_eq!(r.methods().len(), 5);
        let set_ok = json!({"smartlife.cam.ipcamera.siren":{"set_state":{"err_code":0}}});
        assert!(check_set(&set_ok, OnOffFeature::Siren).is_ok());
        let set_off =
            json!({"smartlife.cam.ipcamera.siren":{"set_state":{"err_code":err::CAMERA_OFF}}});
        assert_eq!(
            check_set(&set_off, OnOffFeature::Siren).unwrap_err().code(),
            Some(err::CAMERA_OFF)
        );
    }

    #[test]
    fn reboot_and_sysinfo_requests() {
        assert_eq!(
            reboot().to_json(),
            r#"{"smartlife.cam.ipcamera.system":{"set_reboot":{}}}"#
        );
        assert!(
            check_reboot(&json!({"smartlife.cam.ipcamera.system":{"set_reboot":{"err_code":0}}}))
                .is_ok()
        );
        assert!(check_reboot(&json!({})).is_err());
        assert_eq!(get_sysinfo().to_json(), r#"{"system":{"get_sysinfo":{}}}"#);
    }

    #[test]
    fn ptz_models() {
        let cap = json!({"smartlife.cam.ipcamera.ptz":{"get_capability":{"err_code":0,"max_x":360,"max_y":120}}});
        assert_eq!(
            parse_ptz_capability(&cap),
            Some(PtzCapability {
                max_x: 360,
                max_y: 120
            })
        );
        let fixed = json!({"smartlife.cam.ipcamera.ptz":{"get_capability":{"err_code":-10008}}});
        assert_eq!(parse_ptz_capability(&fixed), None);
        let presets = json!({"smartlife.cam.ipcamera.ptz":{"get_all_preset":{"err_code":0,
            "preset_attr":[{"index":3,"name":"Door"},{"name":"  "},{}]}}});
        assert_eq!(
            parse_presets(&presets).unwrap(),
            vec![
                PtzPreset {
                    index: 3,
                    name: "Door".into()
                },
                PtzPreset {
                    index: 2,
                    name: "Viewpoint 2".into()
                },
                PtzPreset {
                    index: 3,
                    name: "Viewpoint 3".into()
                },
            ]
        );
        assert_eq!(
            parse_presets(&fixed),
            None,
            "a failed read is not an empty list"
        );
        let many: Vec<Value> = (0..100)
            .map(|i| json!({"index": i, "name": "n".repeat(500)}))
            .collect();
        let big = json!({"smartlife.cam.ipcamera.ptz":{"get_all_preset":{"err_code":0,"preset_attr":many}}});
        let p = parse_presets(&big).unwrap();
        assert_eq!(p.len(), MAX_PRESETS);
        assert_eq!(p[0].name.chars().count(), MAX_PRESET_NAME);
        assert_eq!(
            ptz_move(PtzDirection::Top, 0).to_json(),
            r#"{"smartlife.cam.ipcamera.ptz":{"set_move":{"direction":"top","speed":1}}}"#
        );
        assert_eq!(
            ptz_goto_preset(2).to_json(),
            r#"{"smartlife.cam.ipcamera.ptz":{"set_run_to_preset":{"index":2}}}"#
        );
        assert_eq!(PtzDirection::parse("up"), Some(PtzDirection::Top));
        assert_eq!(PtzDirection::parse("down"), Some(PtzDirection::Bottom));
        assert_eq!(PtzDirection::parse("left").unwrap().as_str(), "left");
        assert_eq!(PtzDirection::parse("right").unwrap().as_str(), "right");
        assert_eq!(PtzDirection::parse("north"), None);
        let end_stop = json!({"smartlife.cam.ipcamera.ptz":{"set_move":{"err_code":-41203}}});
        assert_eq!(
            check_ptz(&end_stop, "set_move").unwrap_err().code(),
            Some(err::PTZ_ARGUMENT)
        );
    }

    #[test]
    fn ptz_speed_mapping() {
        assert_eq!(ptz_speed(0.0), 1);
        assert_eq!(ptz_speed(0.05), 1);
        assert_eq!(ptz_speed(0.11), 2);
        assert_eq!(ptz_speed(-0.5), 5);
        assert_eq!(ptz_speed(1.0), 10);
        assert_eq!(ptz_speed(7.0), 10);
        assert_eq!(ptz_speed(f64::NAN), 1);
    }

    #[test]
    fn motion_reply_and_hold() {
        assert_eq!(
            get_motion(1_794_375_629).to_json(),
            r#"{"smartlife.cam.ipcamera.vod":{"get_detect_event_state":{"timestamp":1794375629}}}"#
        );
        let moving = json!({"smartlife.cam.ipcamera.vod":{"get_detect_event_state":{"err_code":0,"is_moving":true}}});
        assert_eq!(parse_motion(&moving), Ok(true));
        let still = json!({"smartlife.cam.ipcamera.vod":{"get_detect_event_state":{"err_code":0,"is_moving":false}}});
        assert_eq!(parse_motion(&still), Ok(false));
        let flaky =
            json!({"smartlife.cam.ipcamera.vod":{"get_detect_event_state":{"is_moving":true}}});
        assert!(parse_motion(&flaky).is_err());

        let mut h = MotionHold::default();
        assert_eq!(h.observe(false, 0), None);
        assert_eq!(h.observe(true, 1_000), Some(true));
        assert_eq!(h.observe(false, 5_000), None);
        assert!(h.detected());
        assert_eq!(h.observe(true, 8_000), None);
        assert_eq!(h.settle(17_999), None);
        assert_eq!(h.settle(18_000), Some(false));
        assert_eq!(h.observe(true, 20_000), Some(true));
        assert_eq!(h.clear(), Some(false));
        assert_eq!(h.clear(), None);
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        assert_eq!(next_motion_backoff(MOTION_POLL_MS), 2_000);
        assert_eq!(next_motion_backoff(20_000), MOTION_POLL_MAX_BACKOFF_MS);
        assert_eq!(next_motion_backoff(0), MOTION_POLL_MS);
    }

    #[test]
    fn snapshot_validation() {
        let req = snapshot_request("10.0.0.5", STREAM_PORT, "Basic s");
        assert_eq!(req.path, SNAPSHOT_PATH);
        assert_eq!(req.header("accept"), Some("image/*"));
        let ok = |status, body: &[u8]| HttpResponse {
            status,
            headers: vec![],
            body: body.to_vec(),
        };
        assert_eq!(
            parse_snapshot(ok(200, &[0xff, 0xd8, 1])),
            Ok(vec![0xff, 0xd8, 1])
        );
        assert_eq!(parse_snapshot(ok(503, &[])), Err(SnapshotError::CameraOff));
        assert_eq!(parse_snapshot(ok(401, &[])), Err(SnapshotError::Http(401)));
        assert_eq!(
            parse_snapshot(ok(200, b"<html>")),
            Err(SnapshotError::NotJpeg)
        );
        let big = vec![0xff; SNAPSHOT_MAX_BYTES + 1];
        assert_eq!(
            parse_snapshot(ok(200, &big)),
            Err(SnapshotError::TooLarge(SNAPSHOT_MAX_BYTES + 1))
        );
        assert_eq!(
            SnapshotError::CameraOff.to_string(),
            "camera is turned off (HTTP 503)"
        );
    }
}
