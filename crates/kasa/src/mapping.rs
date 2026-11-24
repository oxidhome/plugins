//! How Kasa devices map onto `OxidHome` devices and capabilities. The
//! README's "Device mapping" table documents the same thing.
//!
//! Standard capabilities carry what they can (`switch`, `dimmer`,
//! `color-light`, `sensor`, `video-stream`, `audio-stream`); Kasa-specific
//! surfaces are namespaced extensions under [`EXT_NAMESPACE`] (M2).

use serde_json::json;

use crate::protocol::camera::OnOffFeature;
use crate::protocol::discovery::KasaClass;
use oxidhome_sdk::bindings::oxidhome::plugin::capabilities::{
    CapabilitySpec, ColorLightSpec, ExtensionSpec, SensorSpec, Unit,
};
use oxidhome_sdk::bindings::oxidhome::plugin::devices::DeviceInfo;
use oxidhome_sdk::bindings::oxidhome::plugin::types::{KeyValue, Value};

/// Namespace of this plugin's extension capabilities.
pub const EXT_NAMESPACE: &str = "dev.oxidhome.kasa";
/// Manufacturer string on every device.
pub const MANUFACTURER: &str = "TP-Link Kasa";
/// Color temperature range the TS plugin assumes for every bulb (the
/// firmware doesn't report it).
pub const BULB_KELVIN_RANGE: (u32, u32) = (2700, 6500);

/// Plugin-defined capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ext {
    /// The device's status LED. State `on`; command `set { on }`.
    StatusLed,
    /// Energy meter readout. State `voltage_v`, `current_a`, `power_w`,
    /// `total_wh`, `today_wh`, `month_wh`; command `refresh`.
    EnergyMeter,
    /// The camera's own motion detection. State `detected`.
    Motion,
    /// Pan/tilt. State `max_x`, `max_y`, `presets` (JSON); commands
    /// `move { direction, speed? }`, `goto-preset { index }`.
    PanTilt,
    /// Camera maintenance. Commands `reboot`, `snapshot`.
    CameraControl,
}

impl Ext {
    /// Extension name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Ext::StatusLed => "status-led",
            Ext::EnergyMeter => "energy-meter",
            Ext::Motion => "motion",
            Ext::PanTilt => "pan-tilt",
            Ext::CameraControl => "camera-control",
        }
    }

    /// The WIT spec.
    #[must_use]
    pub fn spec(self) -> CapabilitySpec {
        CapabilitySpec::Extension(ExtensionSpec {
            namespace: EXT_NAMESPACE.into(),
            name: self.name().into(),
            version: 1,
        })
    }

    /// The capability key used for state events, commands and the
    /// manifest: `extension(<namespace>/<name>@1)`.
    #[must_use]
    pub fn key(self) -> String {
        format!("extension({EXT_NAMESPACE}/{}@1)", self.name())
    }

    /// Parses a capability key.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        [
            Ext::StatusLed,
            Ext::EnergyMeter,
            Ext::Motion,
            Ext::PanTilt,
            Ext::CameraControl,
        ]
        .into_iter()
        .find(|e| e.key() == key)
    }
}

/// Standard capability keys.
pub mod cap {
    pub const SWITCH: &str = "switch";
    pub const DIMMER: &str = "dimmer";
    pub const COLOR_LIGHT: &str = "color-light";
    pub const SENSOR: &str = "sensor";
}

/// What one registered `OxidHome` device is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// Plug or wall switch: one relay.
    Relay,
    /// Dimmer plug or switch: relay + brightness.
    Dimmer,
    /// Smart bulb.
    Bulb { color: bool, temp: bool },
    /// The strip itself (its LED); outlets are separate devices.
    Strip,
    /// One outlet of a strip.
    Outlet { child_id: String },
    /// Live power draw of a plug with an energy meter, as a `sensor`.
    Power,
    /// A camera.
    Camera,
    /// A camera's spotlight, siren or privacy switch.
    CameraChild(OnOffFeature),
}

/// Per-device feature flags that add capabilities. Independent toggles,
/// one per optional capability, so plain bools are the honest shape.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Features {
    pub led: bool,
    pub emeter: bool,
    pub motion: bool,
    pub ptz: bool,
}

/// The role of the main device of a class.
#[must_use]
pub fn main_role(class: KasaClass, color: bool, temp: bool) -> Role {
    match class {
        KasaClass::Plug | KasaClass::Switch => Role::Relay,
        KasaClass::Dimmer => Role::Dimmer,
        KasaClass::Bulb => Role::Bulb { color, temp },
        KasaClass::Strip => Role::Strip,
        KasaClass::Camera => Role::Camera,
    }
}

/// Suffix of a camera child's local id (the TS plugin's nativeId suffixes).
#[must_use]
pub fn child_suffix(f: OnOffFeature) -> &'static str {
    match f {
        OnOffFeature::Spotlight => "spotlight",
        OnOffFeature::Siren => "siren",
        OnOffFeature::CameraEnabled => "privacy",
        OnOffFeature::Led => "led",
    }
}

/// Local id of a device of `kasa_id`.
#[must_use]
pub fn local_id(kasa_id: &str, role: &Role) -> String {
    match role {
        Role::Outlet { child_id } => format!("{kasa_id}/outlet-{child_id}"),
        Role::Power => format!("{kasa_id}/power"),
        Role::CameraChild(f) => format!("{kasa_id}/{}", child_suffix(*f)),
        _ => kasa_id.to_string(),
    }
}

/// Display-name suffix of a child device.
#[must_use]
pub fn name_suffix(role: &Role) -> Option<&'static str> {
    match role {
        Role::Power => Some("Power"),
        Role::CameraChild(OnOffFeature::Spotlight) => Some("Spotlight"),
        Role::CameraChild(OnOffFeature::Siren) => Some("Siren"),
        Role::CameraChild(OnOffFeature::CameraEnabled) => Some("Privacy Mode"),
        _ => None,
    }
}

/// The capability list of a device.
#[must_use]
pub fn capabilities(role: &Role, f: Features) -> Vec<CapabilitySpec> {
    let mut caps = Vec::new();
    match role {
        Role::Relay | Role::Outlet { .. } | Role::CameraChild(_) => {
            caps.push(CapabilitySpec::Switch);
        }
        Role::Dimmer => {
            caps.push(CapabilitySpec::Switch);
            caps.push(CapabilitySpec::Dimmer);
        }
        Role::Bulb { color, temp } => {
            caps.push(CapabilitySpec::Switch);
            caps.push(CapabilitySpec::Dimmer);
            if *color || *temp {
                caps.push(CapabilitySpec::ColorLight(ColorLightSpec {
                    supports_hsv: *color,
                    supports_color_temp: *temp,
                    color_temp_min_kelvin: temp.then_some(BULB_KELVIN_RANGE.0),
                    color_temp_max_kelvin: temp.then_some(BULB_KELVIN_RANGE.1),
                }));
            }
        }
        Role::Strip => {}
        Role::Power => caps.push(CapabilitySpec::Sensor(SensorSpec {
            unit: Unit::Watt,
            min: Some(0.0),
            max: None,
        })),
        Role::Camera => {
            caps.push(CapabilitySpec::VideoStream);
            caps.push(CapabilitySpec::AudioStream);
            caps.push(Ext::CameraControl.spec());
            if f.motion {
                caps.push(Ext::Motion.spec());
            }
            if f.ptz {
                caps.push(Ext::PanTilt.spec());
            }
        }
    }
    let led_capable = matches!(
        role,
        Role::Relay | Role::Dimmer | Role::Strip | Role::Camera
    );
    if f.led && led_capable {
        caps.push(Ext::StatusLed.spec());
    }
    if f.emeter && matches!(role, Role::Relay | Role::Dimmer) {
        caps.push(Ext::EnergyMeter.spec());
    }
    caps
}

/// Identity of a physical device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub kasa_id: String,
    pub name: String,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub mac: Option<String>,
    pub address: String,
    pub class: Option<KasaClass>,
}

/// Builds the `device-info` for one role of a physical device.
#[must_use]
pub fn device_info(
    id: &Identity,
    role: &Role,
    f: Features,
    outlet_name: Option<&str>,
) -> DeviceInfo {
    let name = match (role, outlet_name) {
        (Role::Outlet { .. }, Some(n)) if !n.is_empty() => n.to_string(),
        _ => match name_suffix(role) {
            Some(s) => format!("{} {s}", id.name),
            None => id.name.clone(),
        },
    };
    let mut metadata = vec![
        kv_str("kasa_id", &id.kasa_id),
        kv_str("address", &id.address),
    ];
    if let Some(c) = id.class {
        metadata.push(kv_str("kasa_class", c.as_str()));
    }
    if let Some(m) = &id.mac {
        metadata.push(kv_str("mac", m));
    }
    match role {
        Role::Outlet { child_id } => metadata.push(kv_str("child_id", child_id)),
        Role::CameraChild(feat) => metadata.push(kv_str("camera_child", child_suffix(*feat))),
        _ => {}
    }
    DeviceInfo {
        local_id: local_id(&id.kasa_id, role),
        name,
        manufacturer: Some(MANUFACTURER.into()),
        model: id.model.clone(),
        firmware: id.firmware.clone(),
        capabilities: capabilities(role, f),
        initial_state: Vec::new(),
        metadata,
    }
}

fn kv_str(k: &str, v: &str) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Value::StringVal(v.into()),
    }
}

fn kv(k: &str, value: Value) -> KeyValue {
    KeyValue {
        key: k.into(),
        value,
    }
}

/// `switch` state.
#[must_use]
pub fn switch_fields(on: bool) -> Vec<KeyValue> {
    vec![kv("state", Value::BoolVal(on))]
}

/// `dimmer` state from Kasa brightness 0–100.
#[must_use]
pub fn dimmer_fields(brightness: u8) -> Vec<KeyValue> {
    vec![kv(
        "level",
        Value::FloatVal(f64::from(brightness.min(100)) / 100.0),
    )]
}

/// `color-light` state. `color_temp_kelvin` is present for bulbs with a
/// white channel: 0 while in color (HSV) mode, so a switch from white to
/// color replaces the old Kelvin value in the host's merged state.
#[must_use]
pub fn color_fields(
    hue: u16,
    saturation: u8,
    brightness: u8,
    color_temp: Option<u32>,
) -> Vec<KeyValue> {
    let mut v = vec![
        kv("hue", Value::FloatVal(f64::from(hue.min(360)) / 360.0)),
        kv(
            "saturation",
            Value::FloatVal(f64::from(saturation.min(100)) / 100.0),
        ),
        kv(
            "value",
            Value::FloatVal(f64::from(brightness.min(100)) / 100.0),
        ),
    ];
    if let Some(k) = color_temp {
        v.push(kv("color_temp_kelvin", Value::IntVal(i64::from(k))));
    }
    v
}

/// `sensor` state in watts.
#[must_use]
pub fn power_fields(watts: f64) -> Vec<KeyValue> {
    vec![
        kv("value", Value::FloatVal(watts)),
        kv("unit", Value::StringVal("watt".into())),
    ]
}

/// `status-led` state.
#[must_use]
pub fn led_fields(on: bool) -> Vec<KeyValue> {
    vec![kv("on", Value::BoolVal(on))]
}

/// `motion` state.
#[must_use]
pub fn motion_fields(detected: bool) -> Vec<KeyValue> {
    vec![kv("detected", Value::BoolVal(detected))]
}

/// `energy-meter` state.
#[must_use]
pub fn energy_fields(
    reading: Option<&crate::protocol::iot::EmeterReading>,
    history: Option<&crate::protocol::iot::EnergyHistory>,
) -> Vec<KeyValue> {
    let mut v = Vec::new();
    if let Some(r) = reading {
        v.push(kv("voltage_v", Value::FloatVal(r.voltage_v)));
        v.push(kv("current_a", Value::FloatVal(r.current_a)));
        v.push(kv("power_w", Value::FloatVal(r.power_w)));
        v.push(kv("total_wh", Value::FloatVal(r.total_wh)));
    }
    if let Some(h) = history {
        v.push(kv("today_wh", Value::FloatVal(h.today_wh)));
        v.push(kv("month_wh", Value::FloatVal(h.month_wh)));
        let days: serde_json::Map<String, serde_json::Value> = h
            .days
            .iter()
            .map(|(d, wh)| (d.clone(), json!(wh)))
            .collect();
        let months: serde_json::Map<String, serde_json::Value> = h
            .months
            .iter()
            .map(|(m, wh)| (m.clone(), json!(wh)))
            .collect();
        v.push(kv(
            "history",
            Value::JsonVal(json!({ "days_wh": days, "months_wh": months }).to_string()),
        ));
    }
    v
}

/// `pan-tilt` state.
#[must_use]
pub fn pan_tilt_fields(
    cap: crate::protocol::camera::PtzCapability,
    presets: &[crate::protocol::camera::PtzPreset],
) -> Vec<KeyValue> {
    let list: Vec<serde_json::Value> = presets
        .iter()
        .map(|p| json!({ "index": p.index, "name": p.name }))
        .collect();
    vec![
        kv("max_x", Value::IntVal(cap.max_x)),
        kv("max_y", Value::IntVal(cap.max_y)),
        kv(
            "presets",
            Value::JsonVal(serde_json::Value::Array(list).to_string()),
        ),
    ]
}

/// Command argument lookups.
pub mod args {
    use oxidhome_sdk::bindings::oxidhome::plugin::types::{KeyValue, Value};

    fn find<'a>(args: &'a [KeyValue], key: &str) -> Option<&'a Value> {
        args.iter()
            .rev()
            .find(|kv| kv.key == key)
            .map(|kv| &kv.value)
    }

    /// A bool argument.
    #[must_use]
    pub fn bool(args: &[KeyValue], key: &str) -> Option<bool> {
        match find(args, key)? {
            Value::BoolVal(b) => Some(*b),
            _ => None,
        }
    }

    /// A number argument (int or float).
    #[must_use]
    pub fn number(args: &[KeyValue], key: &str) -> Option<f64> {
        match find(args, key)? {
            Value::FloatVal(f) if f.is_finite() => Some(*f),
            // Small integers are exact in f64.
            #[allow(clippy::cast_precision_loss)]
            Value::IntVal(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// An integer argument.
    #[must_use]
    pub fn int(args: &[KeyValue], key: &str) -> Option<i64> {
        match find(args, key)? {
            Value::IntVal(i) => Some(*i),
            _ => None,
        }
    }

    /// A string argument.
    #[must_use]
    pub fn string<'a>(args: &'a [KeyValue], key: &str) -> Option<&'a str> {
        match find(args, key)? {
            Value::StringVal(s) => Some(s),
            _ => None,
        }
    }

    /// Whether the key is present at all.
    #[must_use]
    pub fn has(args: &[KeyValue], key: &str) -> bool {
        find(args, key).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(caps: &[CapabilitySpec]) -> Vec<String> {
        caps.iter()
            .map(|c| match c {
                CapabilitySpec::Switch => "switch".to_string(),
                CapabilitySpec::Dimmer => "dimmer".into(),
                CapabilitySpec::ColorLight(s) => {
                    format!(
                        "color-light(hsv={},temp={})",
                        s.supports_hsv, s.supports_color_temp
                    )
                }
                CapabilitySpec::Sensor(_) => "sensor".into(),
                CapabilitySpec::VideoStream => "video-stream".into(),
                CapabilitySpec::AudioStream => "audio-stream".into(),
                CapabilitySpec::Button => "button".into(),
                CapabilitySpec::Extension(e) => format!("ext:{}", e.name),
                // Not produced by this plugin.
                other => format!("unexpected:{other:?}"),
            })
            .collect()
    }

    #[test]
    fn capability_table() {
        let led = Features {
            led: true,
            ..Features::default()
        };
        assert_eq!(
            names(&capabilities(&Role::Relay, led)),
            ["switch", "ext:status-led"]
        );
        let ene = Features {
            led: true,
            emeter: true,
            ..Features::default()
        };
        assert_eq!(
            names(&capabilities(&Role::Relay, ene)),
            ["switch", "ext:status-led", "ext:energy-meter"]
        );
        assert_eq!(
            names(&capabilities(&Role::Dimmer, Features::default())),
            ["switch", "dimmer"]
        );
        assert_eq!(
            names(&capabilities(
                &Role::Bulb {
                    color: true,
                    temp: true
                },
                led
            )),
            ["switch", "dimmer", "color-light(hsv=true,temp=true)"]
        );
        assert_eq!(
            names(&capabilities(
                &Role::Bulb {
                    color: false,
                    temp: false
                },
                Features::default()
            )),
            ["switch", "dimmer"]
        );
        assert_eq!(names(&capabilities(&Role::Strip, led)), ["ext:status-led"]);
        assert_eq!(
            names(&capabilities(
                &Role::Outlet {
                    child_id: "x".into()
                },
                ene
            )),
            ["switch"]
        );
        assert_eq!(names(&capabilities(&Role::Power, ene)), ["sensor"]);
        let cam = Features {
            led: true,
            motion: true,
            ptz: true,
            emeter: false,
        };
        assert_eq!(
            names(&capabilities(&Role::Camera, cam)),
            [
                "video-stream",
                "audio-stream",
                "ext:camera-control",
                "ext:motion",
                "ext:pan-tilt",
                "ext:status-led"
            ]
        );
        assert_eq!(
            names(&capabilities(&Role::CameraChild(OnOffFeature::Siren), cam)),
            ["switch"]
        );
    }

    #[test]
    fn ids_names_and_metadata() {
        let id = Identity {
            kasa_id: "8006".into(),
            name: "Yard".into(),
            class: Some(KasaClass::Camera),
            mac: Some("AA".into()),
            address: "10.0.0.5".into(),
            ..Identity::default()
        };
        let child = device_info(
            &id,
            &Role::CameraChild(OnOffFeature::CameraEnabled),
            Features::default(),
            None,
        );
        assert_eq!(child.local_id, "8006/privacy");
        assert_eq!(child.name, "Yard Privacy Mode");
        assert_eq!(child.manufacturer.as_deref(), Some(MANUFACTURER));
        assert!(child.metadata.iter().any(|kv| kv.key == "camera_child"));
        let outlet = device_info(
            &id,
            &Role::Outlet {
                child_id: "800601".into(),
            },
            Features::default(),
            Some("Lamp"),
        );
        assert_eq!(outlet.local_id, "8006/outlet-800601");
        assert_eq!(outlet.name, "Lamp");
        let unnamed = device_info(
            &id,
            &Role::Outlet {
                child_id: "800601".into(),
            },
            Features::default(),
            Some(""),
        );
        assert_eq!(unnamed.name, "Yard");
        assert_eq!(local_id("8006", &Role::Power), "8006/power");
        assert_eq!(
            name_suffix(&Role::CameraChild(OnOffFeature::Spotlight)),
            Some("Spotlight")
        );
        assert_eq!(child_suffix(OnOffFeature::Led), "led");
        assert_eq!(main_role(KasaClass::Switch, false, false), Role::Relay);
        assert_eq!(main_role(KasaClass::Strip, false, false), Role::Strip);
    }

    #[test]
    fn extension_keys_round_trip() {
        for e in [
            Ext::StatusLed,
            Ext::EnergyMeter,
            Ext::Motion,
            Ext::PanTilt,
            Ext::CameraControl,
        ] {
            assert_eq!(Ext::from_key(&e.key()), Some(e));
        }
        assert_eq!(
            Ext::StatusLed.key(),
            "extension(dev.oxidhome.kasa/status-led@1)"
        );
        assert_eq!(Ext::from_key("extension(com.x/y@1)"), None);
    }

    #[test]
    fn state_fields() {
        let dim = dimmer_fields(55);
        assert!(matches!(dim[0].value, Value::FloatVal(v) if (v - 0.55).abs() < 1e-9));
        let hsv = color_fields(180, 50, 100, Some(0));
        assert!(matches!(hsv[3].value, Value::IntVal(0)), "0 = color mode");
        assert_eq!(color_fields(180, 50, 100, None).len(), 3);
        let warm = color_fields(0, 0, 80, Some(2700));
        assert!(matches!(warm[3].value, Value::IntVal(2700)));
        assert_eq!(power_fields(3.5).len(), 2);
        let reading = crate::protocol::iot::EmeterReading {
            voltage_v: 1.0,
            current_a: 2.0,
            power_w: 3.0,
            total_wh: 4.0,
        };
        assert_eq!(energy_fields(Some(&reading), None).len(), 4);
        assert_eq!(energy_fields(None, None).len(), 0);
        let pt = pan_tilt_fields(
            crate::protocol::camera::PtzCapability { max_x: 1, max_y: 2 },
            &[crate::protocol::camera::PtzPreset {
                index: 1,
                name: "Door".into(),
            }],
        );
        assert!(matches!(&pt[2].value, Value::JsonVal(s) if s == r#"[{"index":1,"name":"Door"}]"#));
    }

    #[test]
    fn arg_lookups() {
        let a = vec![
            KeyValue {
                key: "b".into(),
                value: Value::BoolVal(true),
            },
            KeyValue {
                key: "i".into(),
                value: Value::IntVal(3),
            },
            KeyValue {
                key: "f".into(),
                value: Value::FloatVal(f64::NAN),
            },
            KeyValue {
                key: "s".into(),
                value: Value::StringVal("x".into()),
            },
        ];
        assert_eq!(args::bool(&a, "b"), Some(true));
        assert_eq!(args::bool(&a, "i"), None);
        assert_eq!(args::number(&a, "i"), Some(3.0));
        assert_eq!(args::number(&a, "f"), None);
        assert_eq!(args::int(&a, "i"), Some(3));
        assert_eq!(args::int(&a, "s"), None);
        assert_eq!(args::string(&a, "s"), Some("x"));
        assert!(args::has(&a, "f"));
        assert!(!args::has(&a, "z"));
    }
}
