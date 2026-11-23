//! The legacy Kasa "smart home" protocol (plugs, wall switches, dimmers,
//! bulbs, multi-outlet strips) on TCP/9999.
//!
//! Wire format: a 4-byte **big-endian** length of the JSON, then the JSON
//! encrypted with the [`super::cipher`]. The length counts the
//! plaintext bytes, which equals the ciphertext length. One request per
//! connection; the TS plugin opens a new connection per command with a 5 s
//! timeout, and never sends concurrent requests to one device (parallel
//! connections occasionally race and lose a reply).
//!
//! Requests are `{"<module>":{"<method>":{<arguments>}}}`; replies mirror
//! the request with each method's result in place of its arguments, and
//! every result carries `err_code` (`0` = success).

use serde_json::{Map, Value, json};

use super::cipher;
use super::json::{as_bool01, get_f64, get_i64, get_str, get_u32};

/// TCP port of the smart-home protocol.
pub const IOT_PORT: u16 = 9999;
/// Per-call timeout the TS plugin uses.
pub const IOT_TIMEOUT_MS: u64 = 5_000;
/// Largest reply frame accepted. Real replies (bulb state, strips with six
/// outlets) are a few KB; the cap keeps a hostile peer from making the
/// plugin buffer an arbitrary amount.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// Errors from the smart-home codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IotError {
    /// A reply frame announced more than [`MAX_FRAME_BYTES`].
    FrameTooLarge(usize),
    /// The decrypted reply is not JSON.
    BadJson(String),
    /// The reply lacks the module/method that was asked for.
    MissingResult { module: String, method: String },
    /// The device answered with a non-zero `err_code`.
    Device {
        module: String,
        method: String,
        code: i64,
        msg: Option<String>,
    },
}

impl std::fmt::Display for IotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IotError::FrameTooLarge(n) => {
                write!(f, "reply frame of {n} bytes exceeds {MAX_FRAME_BYTES}")
            }
            IotError::BadJson(e) => write!(f, "reply is not JSON: {e}"),
            IotError::MissingResult { module, method } => {
                write!(f, "reply has no {module}.{method} result")
            }
            IotError::Device {
                module,
                method,
                code,
                msg,
            } => {
                write!(f, "{module}.{method} err_code={code}")?;
                if let Some(m) = msg {
                    write!(f, " ({m})")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for IotError {}

/// Frames a JSON request for TCP/9999: length prefix + ciphertext.
#[must_use]
pub fn encode_frame(json: &str) -> Vec<u8> {
    let bytes = json.as_bytes();
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    cipher::encrypt_in_place(&mut out[4..]);
    out
}

/// Incremental decoder for one reply frame. Large replies (bulb state)
/// arrive split over several TCP segments, so the transport keeps pushing
/// until [`FrameDecoder::finish`] returns a frame.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends received bytes.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The decrypted JSON once the whole frame is in, `Ok(None)` while
    /// more bytes are needed. Bytes past the frame are ignored.
    ///
    /// # Errors
    ///
    /// [`IotError::FrameTooLarge`] when the length prefix is over the cap.
    pub fn finish(&mut self) -> Result<Option<Vec<u8>>, IotError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len > MAX_FRAME_BYTES {
            return Err(IotError::FrameTooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let mut plain = self.buf[4..4 + len].to_vec();
        cipher::decrypt_in_place(&mut plain);
        Ok(Some(plain))
    }

    /// How many more bytes the frame needs, when known.
    #[must_use]
    pub fn missing(&self) -> Option<usize> {
        if self.buf.len() < 4 {
            return Some(4 - self.buf.len());
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        let total = usize::try_from(len).unwrap_or(usize::MAX).saturating_add(4);
        Some(total.saturating_sub(self.buf.len()))
    }
}

/// Decodes one complete reply frame (length prefix included).
///
/// # Errors
///
/// [`IotError::FrameTooLarge`], [`IotError::BadJson`], or `MissingResult`
/// (reused for a frame that is shorter than its prefix says).
pub fn decode_frame(frame: &[u8]) -> Result<Value, IotError> {
    let mut d = FrameDecoder::new();
    d.push(frame);
    match d.finish()? {
        Some(plain) => parse_json(&plain),
        None => Err(IotError::BadJson("truncated frame".into())),
    }
}

/// Parses decrypted reply bytes.
///
/// # Errors
///
/// [`IotError::BadJson`].
pub fn parse_json(plain: &[u8]) -> Result<Value, IotError> {
    serde_json::from_slice(plain).map_err(|e| IotError::BadJson(e.to_string()))
}

/// One smart-home request: modules and methods with their arguments, plus
/// an optional child context for one outlet of a multi-outlet strip.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    body: Map<String, Value>,
}

impl Request {
    /// An empty request.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `module.method(args)`. A method can appear only once per
    /// module; adding it again replaces the arguments.
    #[must_use]
    pub fn call(mut self, module: &str, method: &str, args: Value) -> Self {
        let entry = self
            .body
            .entry(module.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(m) = entry {
            m.insert(method.to_string(), args);
        }
        self
    }

    /// Targets one outlet of a strip (HS300, KP303, KP400, HS107). The
    /// strip relays the request to the child named in
    /// `context.child_ids`. Not used by the Scrypted plugin (which skips
    /// strips); the shape follows python-kasa.
    #[must_use]
    pub fn for_child(mut self, child_id: &str) -> Self {
        self.body
            .insert("context".into(), json!({ "child_ids": [child_id] }));
        self
    }

    /// Serialized JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        Value::Object(self.body.clone()).to_string()
    }

    /// Framed, encrypted bytes for the wire.
    #[must_use]
    pub fn to_frame(&self) -> Vec<u8> {
        encode_frame(&self.to_json())
    }

    /// The `(module, method)` pairs in this request, for result checks.
    #[must_use]
    pub fn methods(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (module, v) in &self.body {
            if module == "context" {
                continue;
            }
            if let Value::Object(m) = v {
                for method in m.keys() {
                    out.push((module.clone(), method.clone()));
                }
            }
        }
        out
    }
}

/// Module names.
pub mod module {
    /// Relay, LED, sysinfo.
    pub const SYSTEM: &str = "system";
    /// Brightness on dimmer plugs and switches.
    pub const DIMMER: &str = "smartlife.iot.dimmer";
    /// Bulb state.
    pub const LIGHTING: &str = "smartlife.iot.smartbulb.lightingservice";
    /// Energy meter on plugs.
    pub const EMETER: &str = "emeter";
    /// Device clock.
    pub const TIME: &str = "time";
}

/// `system.get_sysinfo` — the state read for every device.
#[must_use]
pub fn get_sysinfo() -> Request {
    Request::new().call(module::SYSTEM, "get_sysinfo", json!({}))
}

/// `system.set_relay_state` — on/off for plugs, switches, dimmers and
/// strip outlets (with `child`).
#[must_use]
pub fn set_relay_state(on: bool, child: Option<&str>) -> Request {
    let r = Request::new().call(
        module::SYSTEM,
        "set_relay_state",
        json!({ "state": u8::from(on) }),
    );
    match child {
        Some(c) => r.for_child(c),
        None => r,
    }
}

/// `system.set_led_off` — the device's status LED. `off = true` darkens it.
#[must_use]
pub fn set_led_off(off: bool) -> Request {
    Request::new().call(
        module::SYSTEM,
        "set_led_off",
        json!({ "off": u8::from(off) }),
    )
}

/// `smartlife.iot.dimmer.set_brightness`, `brightness` clamped to 1–100
/// (0 is refused by the firmware; on/off goes through the relay).
#[must_use]
pub fn set_brightness(brightness: u8) -> Request {
    Request::new().call(
        module::DIMMER,
        "set_brightness",
        json!({ "brightness": brightness.clamp(1, 100) }),
    )
}

/// `emeter.get_realtime`.
#[must_use]
pub fn get_emeter_realtime() -> Request {
    Request::new().call(module::EMETER, "get_realtime", json!({}))
}

/// `time.get_time` — the device's local clock, which energy history is
/// keyed by.
#[must_use]
pub fn get_time() -> Request {
    Request::new().call(module::TIME, "get_time", json!({}))
}

/// `emeter.get_daystat` for one month plus `emeter.get_monthstat` for one
/// year, in one request. Both `year` and `month` are required for
/// `get_daystat` (`-3` otherwise).
#[must_use]
pub fn get_energy_stats(day_year: i32, day_month: u32, month_year: i32) -> Request {
    Request::new()
        .call(
            module::EMETER,
            "get_daystat",
            json!({ "year": day_year, "month": day_month }),
        )
        .call(
            module::EMETER,
            "get_monthstat",
            json!({ "year": month_year }),
        )
}

/// A bulb state change. Only the fields set are sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LightChange {
    pub on: Option<bool>,
    /// 0–100.
    pub brightness: Option<u8>,
    /// 0–360.
    pub hue: Option<u16>,
    /// 0–100.
    pub saturation: Option<u8>,
    /// Kelvin; `Some(0)` switches a color bulb to color mode.
    pub color_temp: Option<u32>,
}

impl LightChange {
    /// An HSV change. Sends `color_temp: 0` with it: without that, some
    /// bulbs ignore hue/saturation while in color-temperature mode.
    #[must_use]
    pub fn hsv(hue: u16, saturation: u8, brightness: u8) -> Self {
        Self {
            on: None,
            brightness: Some(brightness.min(100)),
            hue: Some(hue.min(360)),
            saturation: Some(saturation.min(100)),
            color_temp: Some(0),
        }
    }
}

/// `transition_light_state` with `ignore_default: 1`, which merges with
/// the current state instead of resetting the other fields.
#[must_use]
pub fn transition_light_state(change: &LightChange) -> Request {
    let mut args = Map::new();
    args.insert("ignore_default".into(), json!(1));
    if let Some(on) = change.on {
        args.insert("on_off".into(), json!(u8::from(on)));
    }
    if let Some(b) = change.brightness {
        args.insert("brightness".into(), json!(b.min(100)));
    }
    if let Some(h) = change.hue {
        args.insert("hue".into(), json!(h.min(360)));
    }
    if let Some(s) = change.saturation {
        args.insert("saturation".into(), json!(s.min(100)));
    }
    if let Some(t) = change.color_temp {
        args.insert("color_temp".into(), json!(t));
    }
    Request::new().call(
        module::LIGHTING,
        "transition_light_state",
        Value::Object(args),
    )
}

/// The result object of `module.method`, checking its `err_code`. A
/// result without `err_code` counts as success.
///
/// # Errors
///
/// [`IotError::MissingResult`] or [`IotError::Device`].
pub fn method_result<'a>(
    reply: &'a Value,
    module: &str,
    method: &str,
) -> Result<&'a Value, IotError> {
    let r = reply
        .get(module)
        .and_then(|m| m.get(method))
        .ok_or_else(|| IotError::MissingResult {
            module: module.into(),
            method: method.into(),
        })?;
    if let Some(code) = r.get("err_code").and_then(Value::as_i64)
        && code != 0
    {
        return Err(IotError::Device {
            module: module.into(),
            method: method.into(),
            code,
            msg: get_str(r, "err_msg").map(str::to_string),
        });
    }
    Ok(r)
}

/// Checks every method of `request` (a write) succeeded in `reply`.
///
/// # Errors
///
/// The first failing method's error.
pub fn check_reply(request: &Request, reply: &Value) -> Result<(), IotError> {
    for (module, method) in request.methods() {
        let r = method_result(reply, &module, &method)?;
        // A write is confirmed only by an explicit `err_code: 0`; reads
        // tolerate its absence (as the TS parsers do), writes don't.
        if r.get("err_code").and_then(Value::as_i64) != Some(0) {
            return Err(IotError::MissingResult { module, method });
        }
    }
    Ok(())
}

/// One outlet of a multi-outlet strip, from `sysinfo.children`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildOutlet {
    /// The id to put into `context.child_ids`, exactly as the strip
    /// reports it (python-kasa passes it through unchanged).
    pub id: String,
    pub alias: String,
    pub on: bool,
}

/// Current light state of a bulb.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LightState {
    pub on: bool,
    pub brightness: Option<u8>,
    pub hue: Option<u16>,
    pub saturation: Option<u8>,
    /// Kelvin; 0 while in color mode.
    pub color_temp: Option<u32>,
}

impl LightState {
    /// Parses `light_state`. While the bulb is **off** it carries only
    /// `on_off: 0`, and the remembered values sit under `dft_on_state`.
    #[must_use]
    pub fn parse(ls: &Value) -> Option<Self> {
        let on = as_bool01(ls.get("on_off")?)?;
        let active = if on {
            ls
        } else {
            ls.get("dft_on_state").unwrap_or(&Value::Null)
        };
        Some(Self {
            on,
            brightness: get_u32(active, "brightness").and_then(|v| u8::try_from(v.min(100)).ok()),
            hue: get_u32(active, "hue").and_then(|v| u16::try_from(v.min(360)).ok()),
            saturation: get_u32(active, "saturation").and_then(|v| u8::try_from(v.min(100)).ok()),
            color_temp: get_u32(active, "color_temp"),
        })
    }
}

/// The parts of `get_sysinfo` the plugin uses. Every field is optional:
/// the shape varies per family and firmware, and a malformed reply must not
/// take the plugin down.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SysInfo {
    pub device_id: Option<String>,
    pub alias: Option<String>,
    pub model: Option<String>,
    pub mac: Option<String>,
    /// `type` (smart home) or `mic_type` (cameras).
    pub kind: Option<String>,
    pub sw_ver: Option<String>,
    pub hw_ver: Option<String>,
    pub dev_name: Option<String>,
    pub feature: Option<String>,
    pub relay_on: Option<bool>,
    pub brightness: Option<u8>,
    pub led_off: Option<bool>,
    pub is_color: bool,
    pub is_variable_color_temp: bool,
    pub is_dimmable: bool,
    pub light: Option<LightState>,
    pub children: Vec<ChildOutlet>,
}

impl SysInfo {
    /// Parses a sysinfo object (the value of `system.get_sysinfo`, or the
    /// identity object discovery picked out of a camera reply).
    #[must_use]
    pub fn parse(o: &Value) -> Self {
        let device_id = get_str(o, "deviceId")
            .or_else(|| get_str(o, "dev_id"))
            .map(str::to_string);
        let children = match o.get("children") {
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(|c| {
                    Some(ChildOutlet {
                        id: get_str(c, "id")?.to_string(),
                        alias: get_str(c, "alias").unwrap_or_default().to_string(),
                        on: c.get("state").and_then(as_bool01).unwrap_or(false),
                    })
                })
                .collect(),
            _ => Vec::new(),
        };
        Self {
            device_id,
            alias: get_str(o, "alias").map(str::to_string),
            model: get_str(o, "model")
                .or_else(|| get_str(o, "hwId"))
                .or_else(|| get_str(o, "hw_id"))
                .map(str::to_string),
            mac: get_str(o, "mic_mac")
                .or_else(|| get_str(o, "mac"))
                .map(str::to_string),
            kind: get_str(o, "type")
                .or_else(|| get_str(o, "mic_type"))
                .or_else(|| get_str(o, "deviceType"))
                .map(str::to_string),
            sw_ver: get_str(o, "sw_ver").map(str::to_string),
            hw_ver: get_str(o, "hw_ver").map(str::to_string),
            dev_name: get_str(o, "dev_name").map(str::to_string),
            feature: get_str(o, "feature").map(str::to_string),
            relay_on: o.get("relay_state").and_then(as_bool01),
            brightness: get_u32(o, "brightness").and_then(|b| u8::try_from(b.min(100)).ok()),
            led_off: o.get("led_off").and_then(as_bool01),
            is_color: get_i64(o, "is_color") == Some(1),
            is_variable_color_temp: get_i64(o, "is_variable_color_temp") == Some(1),
            is_dimmable: get_i64(o, "is_dimmable") == Some(1),
            light: o.get("light_state").and_then(LightState::parse),
            children,
        }
    }

    /// `feature` contains `ENE`: the device has an energy meter.
    #[must_use]
    pub fn has_emeter(&self) -> bool {
        self.feature
            .as_deref()
            .is_some_and(|f| f.to_ascii_uppercase().contains("ENE"))
    }
}

/// Extracts and parses `system.get_sysinfo` from a reply.
///
/// # Errors
///
/// As [`method_result`].
pub fn parse_sysinfo_reply(reply: &Value) -> Result<SysInfo, IotError> {
    method_result(reply, module::SYSTEM, "get_sysinfo").map(SysInfo::parse)
}

/// An energy reading normalized to V / A / W / Wh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeterReading {
    pub voltage_v: f64,
    pub current_a: f64,
    pub power_w: f64,
    pub total_wh: f64,
}

/// Parses `emeter.get_realtime`, which comes in a milli-unit schema
/// (`voltage_mv`, `current_ma`, `power_mw`, `total_wh`) or a base-unit one
/// (`voltage` V, `current` A, `power` W, `total` kWh) depending on the
/// device. Checks which keys are present rather than trusting the model.
///
/// # Errors
///
/// [`IotError`] from the result lookup; `Ok(None)` when neither schema
/// matches.
pub fn parse_emeter_reply(reply: &Value) -> Result<Option<EmeterReading>, IotError> {
    let e = method_result(reply, module::EMETER, "get_realtime")?;
    if let Some(mv) = get_f64(e, "voltage_mv") {
        return Ok(Some(EmeterReading {
            voltage_v: mv / 1000.0,
            current_a: get_f64(e, "current_ma").unwrap_or(0.0) / 1000.0,
            power_w: get_f64(e, "power_mw").unwrap_or(0.0) / 1000.0,
            total_wh: get_f64(e, "total_wh").unwrap_or(0.0),
        }));
    }
    if let Some(v) = get_f64(e, "voltage") {
        return Ok(Some(EmeterReading {
            voltage_v: v,
            current_a: get_f64(e, "current").unwrap_or(0.0),
            power_w: get_f64(e, "power").unwrap_or(0.0),
            total_wh: get_f64(e, "total").unwrap_or(0.0) * 1000.0,
        }));
    }
    Ok(None)
}

/// A calendar date in the device's own time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeviceDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

/// Parses `time.get_time`.
///
/// # Errors
///
/// [`IotError`] from the result lookup; `Ok(None)` without a usable date.
pub fn parse_time_reply(reply: &Value) -> Result<Option<DeviceDate>, IotError> {
    let t = method_result(reply, module::TIME, "get_time")?;
    let (Some(year), Some(month), Some(day)) = (
        get_i64(t, "year").and_then(|y| i32::try_from(y).ok()),
        get_u32(t, "month"),
        get_u32(t, "mday"),
    ) else {
        return Ok(None);
    };
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Ok(None);
    }
    Ok(Some(DeviceDate { year, month, day }))
}

/// One daily or monthly energy total.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnergyStat {
    pub year: i32,
    pub month: u32,
    /// `None` for a monthly total.
    pub day: Option<u32>,
    pub wh: f64,
}

/// Parses the `get_daystat` and `get_monthstat` results of one reply. A
/// failed or missing method yields an empty list, as in the TS plugin.
#[must_use]
pub fn parse_energy_stats(reply: &Value) -> (Vec<EnergyStat>, Vec<EnergyStat>) {
    let list = |method: &str, key: &str| -> Vec<EnergyStat> {
        let Ok(r) = method_result(reply, module::EMETER, method) else {
            return Vec::new();
        };
        let Some(Value::Array(entries)) = r.get(key) else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|e| {
                // Devices on the base-unit schema report `energy` in kWh.
                let wh =
                    get_f64(e, "energy_wh").or_else(|| get_f64(e, "energy").map(|k| k * 1000.0))?;
                Some(EnergyStat {
                    year: get_i64(e, "year").and_then(|y| i32::try_from(y).ok())?,
                    month: get_u32(e, "month")?,
                    day: get_u32(e, "day"),
                    wh,
                })
            })
            .collect()
    };
    (
        list("get_daystat", "day_list"),
        list("get_monthstat", "month_list"),
    )
}

/// Days in a Gregorian month.
#[must_use]
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

impl DeviceDate {
    /// The day before.
    #[must_use]
    pub fn prev_day(self) -> Self {
        if self.day > 1 {
            return Self {
                day: self.day - 1,
                ..self
            };
        }
        let (year, month) = prev_month(self.year, self.month);
        Self {
            year,
            month,
            day: days_in_month(year, month),
        }
    }
}

/// The month before `(year, month)`.
#[must_use]
pub fn prev_month(year: i32, month: u32) -> (i32, u32) {
    if month <= 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

/// Days of daily history kept.
pub const HISTORY_DAYS: usize = 30;
/// Months of monthly history kept.
pub const HISTORY_MONTHS: usize = 12;

/// Usage history from the plug's own totals, newest first. Days and months
/// the plug has no entry for count as 0, since it leaves those out.
#[derive(Debug, Clone, PartialEq)]
pub struct EnergyHistory {
    pub today_wh: f64,
    pub month_wh: f64,
    /// `(YYYY-MM-DD, Wh)`, [`HISTORY_DAYS`] entries.
    pub days: Vec<(String, f64)>,
    /// `(YYYY-MM, Wh)`, [`HISTORY_MONTHS`] entries.
    pub months: Vec<(String, f64)>,
}

/// The stats requests that cover [`HISTORY_DAYS`] days and
/// [`HISTORY_MONTHS`] months back from `today`: the current month + year,
/// the previous month + year, and, when the day window reaches further
/// (March 1 back to January 31), that month's days too.
#[must_use]
pub fn energy_history_requests(today: DeviceDate) -> Vec<Request> {
    let (py, pm) = prev_month(today.year, today.month);
    let mut reqs = vec![
        get_energy_stats(today.year, today.month, today.year),
        get_energy_stats(py, pm, today.year - 1),
    ];
    let mut first = today;
    for _ in 1..HISTORY_DAYS {
        first = first.prev_day();
    }
    let (ey, em) = prev_month(py, pm);
    if (first.year, first.month) == (ey, em) {
        reqs.push(Request::new().call(
            module::EMETER,
            "get_daystat",
            json!({ "year": ey, "month": em }),
        ));
    }
    reqs
}

/// Builds the history from the stats of both [`energy_history_requests`].
#[must_use]
pub fn energy_history(today: DeviceDate, stats: &[EnergyStat]) -> EnergyHistory {
    let lookup = |year: i32, month: u32, day: Option<u32>| {
        stats
            .iter()
            .rev()
            .find(|s| s.year == year && s.month == month && s.day == day)
            .map_or(0.0, |s| s.wh)
    };
    let mut days = Vec::with_capacity(HISTORY_DAYS);
    let mut d = today;
    for _ in 0..HISTORY_DAYS {
        days.push((
            format!("{:04}-{:02}-{:02}", d.year, d.month, d.day),
            lookup(d.year, d.month, Some(d.day)),
        ));
        d = d.prev_day();
    }
    let mut months = Vec::with_capacity(HISTORY_MONTHS);
    let (mut y, mut m) = (today.year, today.month);
    for _ in 0..HISTORY_MONTHS {
        months.push((format!("{y:04}-{m:02}"), lookup(y, m, None)));
        (y, m) = prev_month(y, m);
    }
    EnergyHistory {
        today_wh: days[0].1,
        month_wh: months[0].1,
        days,
        months,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    #[test]
    fn frames_known_vector() {
        let f = get_sysinfo().to_frame();
        assert_eq!(
            hex(&f),
            "0000001dd0f281f88bff9af7d5ef94b6d1b4c09fec95e68fe187e8caf08bf68bf6"
        );
    }

    #[test]
    fn decoder_waits_for_split_frames() {
        let json = r#"{"system":{"set_relay_state":{"err_code":0}}}"#;
        let frame = encode_frame(json);
        let mut d = FrameDecoder::new();
        d.push(&frame[..2]);
        assert_eq!(d.finish(), Ok(None));
        assert_eq!(d.missing(), Some(2));
        d.push(&frame[2..10]);
        assert_eq!(d.finish(), Ok(None));
        d.push(&frame[10..]);
        assert_eq!(d.finish().unwrap().unwrap(), json.as_bytes());
        assert_eq!(d.missing(), Some(0));
    }

    #[test]
    fn decoder_refuses_oversized_frames() {
        let mut d = FrameDecoder::new();
        d.push(&[0x7f, 0xff, 0xff, 0xff]);
        assert!(matches!(d.finish(), Err(IotError::FrameTooLarge(_))));
    }

    #[test]
    fn decode_frame_rejects_truncated_and_garbage() {
        let frame = encode_frame("{}");
        assert!(decode_frame(&frame[..5]).is_err());
        let garbage = encode_frame("not json");
        assert!(matches!(decode_frame(&garbage), Err(IotError::BadJson(_))));
        assert_eq!(decode_frame(&frame).unwrap(), json!({}));
    }

    #[test]
    fn builds_relay_led_and_dimmer_requests() {
        assert_eq!(
            set_relay_state(true, None).to_json(),
            r#"{"system":{"set_relay_state":{"state":1}}}"#
        );
        assert_eq!(
            set_relay_state(false, None).to_json(),
            r#"{"system":{"set_relay_state":{"state":0}}}"#
        );
        assert_eq!(
            set_led_off(true).to_json(),
            r#"{"system":{"set_led_off":{"off":1}}}"#
        );
        assert_eq!(
            set_brightness(0).to_json(),
            r#"{"smartlife.iot.dimmer":{"set_brightness":{"brightness":1}}}"#
        );
        assert_eq!(
            set_brightness(250).to_json(),
            r#"{"smartlife.iot.dimmer":{"set_brightness":{"brightness":100}}}"#
        );
    }

    #[test]
    fn strip_outlet_requests_carry_child_context() {
        let r = set_relay_state(true, Some("800600AA01"));
        let v: Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(v["context"]["child_ids"][0], "800600AA01");
        assert_eq!(v["system"]["set_relay_state"]["state"], 1);
        assert_eq!(
            r.methods(),
            vec![("system".to_string(), "set_relay_state".to_string())]
        );
    }

    #[test]
    fn energy_stats_request_has_both_methods() {
        let r = get_energy_stats(2026, 11, 2026);
        let v: Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(v["emeter"]["get_daystat"], json!({"year":2026,"month":11}));
        assert_eq!(v["emeter"]["get_monthstat"], json!({"year":2026}));
        assert_eq!(r.methods().len(), 2);
    }

    #[test]
    fn bulb_changes_always_send_ignore_default() {
        let r = transition_light_state(&LightChange {
            on: Some(true),
            ..LightChange::default()
        });
        let v: Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(
            v[module::LIGHTING]["transition_light_state"],
            json!({"ignore_default":1,"on_off":1})
        );
        let hsv = transition_light_state(&LightChange::hsv(400, 120, 80));
        let v: Value = serde_json::from_str(&hsv.to_json()).unwrap();
        assert_eq!(
            v[module::LIGHTING]["transition_light_state"],
            json!({"ignore_default":1,"brightness":80,"hue":360,"saturation":100,"color_temp":0})
        );
    }

    #[test]
    fn method_result_checks_err_code() {
        let ok = json!({"system":{"set_relay_state":{"err_code":0}}});
        assert!(method_result(&ok, "system", "set_relay_state").is_ok());
        let no_code = json!({"system":{"set_relay_state":{}}});
        assert!(method_result(&no_code, "system", "set_relay_state").is_ok());
        let write = set_relay_state(true, None);
        assert!(check_reply(&write, &ok).is_ok());
        assert!(matches!(
            check_reply(&write, &no_code),
            Err(IotError::MissingResult { .. })
        ));
        let null = json!({"system":{"set_relay_state":null}});
        assert!(check_reply(&write, &null).is_err());
        let bad = json!({"smartlife.iot.dimmer":{"set_brightness":{"err_code":-3,"err_msg":"invalid argument"}}});
        let e = method_result(&bad, module::DIMMER, "set_brightness").unwrap_err();
        assert_eq!(
            e.to_string(),
            "smartlife.iot.dimmer.set_brightness err_code=-3 (invalid argument)"
        );
        assert!(matches!(
            method_result(&ok, "system", "get_sysinfo"),
            Err(IotError::MissingResult { .. })
        ));
        assert!(check_reply(&set_relay_state(true, None), &ok).is_ok());
        assert!(check_reply(&set_led_off(true), &ok).is_err());
    }

    #[test]
    fn parses_plug_sysinfo() {
        let reply = json!({"system":{"get_sysinfo":{
            "alias":"Garage Lights","model":"EP25(US)","deviceId":"8006AB",
            "mac":"54:AF:97:2D:AA:24","type":"IOT.SMARTPLUGSWITCH","hw_ver":"1.0",
            "sw_ver":"1.0.2","relay_state":1,"led_off":0,"feature":"TIM:ENE",
            "dev_name":"Smart Wi-Fi Plug Mini","err_code":0}}});
        let s = parse_sysinfo_reply(&reply).unwrap();
        assert_eq!(s.device_id.as_deref(), Some("8006AB"));
        assert_eq!(s.alias.as_deref(), Some("Garage Lights"));
        assert_eq!(s.kind.as_deref(), Some("IOT.SMARTPLUGSWITCH"));
        assert_eq!(s.relay_on, Some(true));
        assert_eq!(s.led_off, Some(false));
        assert!(s.has_emeter());
        assert_eq!(s.children, []);
    }

    #[test]
    fn parses_strip_children_keeping_reported_ids() {
        let s = SysInfo::parse(&json!({
        "deviceId":"8006F0","type":"IOT.SMARTPLUGSWITCH",
        "children":[
            {"id":"00","state":1,"alias":"Lamp"},
            {"id":"8006F001","state":0,"alias":"Fan"},
            {"state":1}
        ]}));
        assert_eq!(
            s.children,
            vec![
                ChildOutlet {
                    id: "00".into(),
                    alias: "Lamp".into(),
                    on: true
                },
                ChildOutlet {
                    id: "8006F001".into(),
                    alias: "Fan".into(),
                    on: false
                },
            ]
        );
    }

    #[test]
    fn parses_bulb_light_state_on_and_off() {
        let on = SysInfo::parse(&json!({"type":"IOT.SMARTBULB","is_color":1,
            "is_variable_color_temp":1,"is_dimmable":1,
            "light_state":{"on_off":1,"brightness":80,"hue":120,"saturation":50,"color_temp":0}}));
        assert!(on.is_color && on.is_variable_color_temp && on.is_dimmable);
        assert_eq!(
            on.light,
            Some(LightState {
                on: true,
                brightness: Some(80),
                hue: Some(120),
                saturation: Some(50),
                color_temp: Some(0)
            })
        );
        let off = SysInfo::parse(&json!({"light_state":{"on_off":0,
            "dft_on_state":{"brightness":30,"hue":0,"saturation":0,"color_temp":2700}}}));
        assert_eq!(
            off.light,
            Some(LightState {
                on: false,
                brightness: Some(30),
                hue: Some(0),
                saturation: Some(0),
                color_temp: Some(2700)
            })
        );
        // A bare off state with nothing remembered still parses.
        let bare = SysInfo::parse(&json!({"light_state":{"on_off":0}}));
        assert_eq!(bare.light.unwrap().brightness, None);
    }

    #[test]
    fn malformed_sysinfo_does_not_panic() {
        let s =
            SysInfo::parse(&json!({"alias":5,"relay_state":"on","children":"x","brightness":-4}));
        assert_eq!(s, SysInfo::default());
    }

    #[test]
    fn parses_both_emeter_schemas() {
        let milli = json!({"emeter":{"get_realtime":{"voltage_mv":120_500,"current_ma":250,
            "power_mw":30000,"total_wh":1234,"err_code":0}}});
        assert_eq!(
            parse_emeter_reply(&milli).unwrap(),
            Some(EmeterReading {
                voltage_v: 120.5,
                current_a: 0.25,
                power_w: 30.0,
                total_wh: 1234.0
            })
        );
        let base = json!({"emeter":{"get_realtime":{"voltage":230.0,"current":0.5,
            "power":115.0,"total":2.5,"err_code":0}}});
        assert_eq!(
            parse_emeter_reply(&base).unwrap(),
            Some(EmeterReading {
                voltage_v: 230.0,
                current_a: 0.5,
                power_w: 115.0,
                total_wh: 2500.0
            })
        );
        let neither = json!({"emeter":{"get_realtime":{"err_code":0}}});
        assert_eq!(parse_emeter_reply(&neither).unwrap(), None);
        let err = json!({"emeter":{"get_realtime":{"err_code":-1}}});
        assert!(parse_emeter_reply(&err).is_err());
    }

    #[test]
    fn parses_time_and_rejects_bad_dates() {
        let t = json!({"time":{"get_time":{"year":2026,"month":3,"mday":1,"hour":0,"min":0,"sec":0,"err_code":0}}});
        assert_eq!(
            parse_time_reply(&t).unwrap(),
            Some(DeviceDate {
                year: 2026,
                month: 3,
                day: 1
            })
        );
        let bad = json!({"time":{"get_time":{"year":2026,"month":2,"mday":30,"err_code":0}}});
        assert_eq!(parse_time_reply(&bad).unwrap(), None);
    }

    #[test]
    fn date_arithmetic_handles_month_and_leap_boundaries() {
        let d = DeviceDate {
            year: 2024,
            month: 3,
            day: 1,
        };
        assert_eq!(
            d.prev_day(),
            DeviceDate {
                year: 2024,
                month: 2,
                day: 29
            }
        );
        let jan = DeviceDate {
            year: 2026,
            month: 1,
            day: 1,
        };
        assert_eq!(
            jan.prev_day(),
            DeviceDate {
                year: 2025,
                month: 12,
                day: 31
            }
        );
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(prev_month(2026, 1), (2025, 12));
    }

    #[test]
    fn builds_energy_history_with_gaps_as_zero() {
        let today = DeviceDate {
            year: 2026,
            month: 1,
            day: 2,
        };
        let reqs = energy_history_requests(today);
        assert_eq!(reqs.len(), 2);
        let mar1 = DeviceDate {
            year: 2026,
            month: 3,
            day: 1,
        };
        let three = energy_history_requests(mar1);
        assert_eq!(three.len(), 3, "March 1 reaches back to January 31");
        let jan: Value = serde_json::from_str(&three[2].to_json()).unwrap();
        assert_eq!(jan["emeter"]["get_daystat"], json!({"year":2026,"month":1}));
        let v: Value = serde_json::from_str(&reqs[1].to_json()).unwrap();
        assert_eq!(v["emeter"]["get_daystat"], json!({"year":2025,"month":12}));
        assert_eq!(v["emeter"]["get_monthstat"], json!({"year":2025}));

        let reply_now = json!({"emeter":{
            "get_daystat":{"day_list":[{"year":2026,"month":1,"day":2,"energy_wh":21}],"err_code":0},
            "get_monthstat":{"month_list":[{"year":2026,"month":1,"energy_wh":118}],"err_code":0}}});
        let reply_prev = json!({"emeter":{
            "get_daystat":{"day_list":[{"year":2025,"month":12,"day":31,"energy":0.004}],"err_code":0},
            "get_monthstat":{"month_list":[{"year":2025,"month":7,"energy_wh":1052}],"err_code":-3}}});
        let mut stats = Vec::new();
        for r in [&reply_now, &reply_prev] {
            let (d, m) = parse_energy_stats(r);
            stats.extend(d);
            stats.extend(m);
        }
        let h = energy_history(today, &stats);
        assert_eq!(h.today_wh, 21.0);
        assert_eq!(h.month_wh, 118.0);
        assert_eq!(h.days.len(), HISTORY_DAYS);
        assert_eq!(h.days[1], ("2026-01-01".into(), 0.0));
        assert_eq!(h.days[2], ("2025-12-31".into(), 4.0));
        assert_eq!(h.months.len(), HISTORY_MONTHS);
        // The failed get_monthstat of the previous year contributes nothing.
        assert_eq!(h.months[6], ("2025-07".into(), 0.0));
        assert_eq!(h.months[11].0, "2025-02");
    }
}
