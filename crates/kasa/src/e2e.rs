//! End-to-end tests of [`Kasa`] over the fake host and network.

use std::net::Ipv4Addr;

use serde_json::json;

use crate::fake::{FakeCam, FakeHost, FakeIot, FakeNet};
use crate::kasa::{Kasa, STORAGE_KEY};
use crate::mapping::{Ext, cap};
use crate::platform::NetError;
use oxidhome_sdk::bindings::oxidhome::plugin::devices::{Command, CommandResult};
use oxidhome_sdk::bindings::oxidhome::plugin::logging::Level;
use oxidhome_sdk::bindings::oxidhome::plugin::types::{Error, KeyValue, Value};

const PLUG: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 10);
const BULB: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 11);
const STRIP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 12);
const DIMMER: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 13);
const CAM: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);

fn plug() -> FakeIot {
    let mut d = FakeIot::new(json!({
        "type": "IOT.SMARTPLUGSWITCH", "model": "KP115(US)", "alias": "Kettle",
        "deviceId": "PLUG1", "mac": "AA:BB", "sw_ver": "1.0.1", "dev_name": "Smart Wi-Fi Plug",
        "feature": "TIM:ENE", "relay_state": 0, "led_off": 0
    }));
    d.realtime = Some(
        json!({ "voltage_mv": 120_000, "current_ma": 500, "power_mw": 60_000, "total_wh": 1_000 }),
    );
    d
}

fn bulb() -> FakeIot {
    FakeIot::new(json!({
        "type": "IOT.SMARTBULB", "model": "KL130(US)", "alias": "Lamp", "deviceId": "BULB1",
        "is_color": 1, "is_variable_color_temp": 1, "is_dimmable": 1,
        "light_state": { "on_off": 1, "brightness": 40, "hue": 0, "saturation": 0, "color_temp": 2700 }
    }))
}

fn strip() -> FakeIot {
    FakeIot::new(json!({
        "type": "IOT.SMARTPLUGSWITCH", "model": "HS300(US)", "alias": "Desk", "deviceId": "STRIP",
        "led_off": 1, "feature": "TIM:ENE",
        "children": [ { "id": "00", "alias": "Monitor", "state": 1 }, { "id": "01", "alias": "", "state": 0 } ]
    }))
}

fn dimmer() -> FakeIot {
    FakeIot::new(json!({
        "type": "IOT.SMARTPLUGSWITCH", "model": "HS220(US)", "alias": "Hall", "deviceId": "DIM1",
        "dev_name": "Smart Wi-Fi Dimmer", "relay_state": 1, "brightness": 30, "led_off": 0
    }))
}

fn camera_discovery() -> FakeIot {
    // A camera answers the UDP probe; its TCP 9999 is not used.
    FakeIot::new(json!({
        "system": { "deviceId": "CAM1", "alias": "Yard", "model": "KC420WS(US)",
            "mic_type": "IOT.IPCAMERA", "mic_mac": "CC:DD", "sw_ver": "2.3" }
    }))
}

fn kv(k: &str, v: Value) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: v,
    }
}

fn cmd(id: &str, capability: &str, action: &str, args: Vec<KeyValue>) -> Command {
    Command {
        id: id.into(),
        capability: capability.into(),
        action: action.into(),
        args,
    }
}

fn creds() -> Vec<(&'static str, Value)> {
    vec![
        ("camera_username", Value::StringVal("me@example.com".into())),
        ("camera_password", Value::StringVal("hunter2".into())),
        (
            "discovery_targets",
            Value::StringVal("192.168.1.0/24".into()),
        ),
    ]
}

/// A plugin with the given devices, after one full discovery.
fn discovered(cfg: &[(&str, Value)], devices: Vec<(Ipv4Addr, FakeIot)>) -> Kasa<FakeHost, FakeNet> {
    let mut net = FakeNet {
        now_ms: 1_000,
        unix_ms: 1_700_000_000_000,
        ..FakeNet::default()
    };
    net.iot.extend(devices);
    let mut k = Kasa::new(FakeHost::with_config(cfg), net);
    k.init();
    for _ in 0..8 {
        k.tick();
        k.net.advance(500);
    }
    k
}

fn id(k: &Kasa<FakeHost, FakeNet>, local: &str) -> String {
    k.device_id_of(local)
        .unwrap_or_else(|| panic!("{local} not registered: {:?}", k.host.devices))
}

fn bool_field(r: &CommandResult, key: &str) -> Option<bool> {
    match r {
        CommandResult::OkWithState(f) => f.iter().find_map(|kv| match &kv.value {
            Value::BoolVal(b) if kv.key == key => Some(*b),
            _ => None,
        }),
        _ => None,
    }
}

#[test]
fn discovery_registers_every_family_with_its_capabilities() {
    let k = discovered(
        &[],
        vec![
            (PLUG, plug()),
            (BULB, bulb()),
            (STRIP, strip()),
            (DIMMER, dimmer()),
        ],
    );
    let caps = |local: &str| -> Vec<String> {
        k.host
            .device(local)
            .unwrap()
            .1
            .capabilities
            .iter()
            .map(|c| {
                let d = format!("{c:?}");
                let d = d.strip_prefix("CapabilitySpec::").unwrap_or(&d);
                d.split('(').next().unwrap().to_string()
            })
            .collect()
    };
    assert_eq!(caps("PLUG1"), ["Switch", "Extension", "Extension"]);
    assert_eq!(caps("PLUG1/power"), ["Sensor"]);
    assert_eq!(caps("BULB1"), ["Switch", "Dimmer", "ColorLight"]);
    assert_eq!(caps("DIM1"), ["Switch", "Dimmer", "Extension"]);
    assert_eq!(caps("STRIP"), ["Extension"]);
    assert_eq!(caps("STRIP/outlet-00"), ["Switch"]);
    assert_eq!(k.host.device("STRIP/outlet-00").unwrap().1.name, "Monitor");
    assert_eq!(k.host.device("STRIP/outlet-01").unwrap().1.name, "Desk");
    assert_eq!(k.host.devices.len(), 7);
    // Broadcast went out once; no sweep configured.
    assert!(k.net.udp_sent.iter().all(|a| a.ip().is_broadcast()));
    assert!(!k.net.udp_open, "socket closed after the window");

    // State from the discovery reply was published.
    let plug = id(&k, "PLUG1");
    let sw = k.host.events_for(&plug, cap::SWITCH);
    assert_eq!(sw.len(), 1);
    assert!(matches!(sw[0].get("state"), Some(Value::BoolVal(false))));
    let led = k.host.events_for(&plug, &Ext::StatusLed.key());
    assert!(matches!(led[0].get("on"), Some(Value::BoolVal(true))));
    let monitor = id(&k, "STRIP/outlet-00");
    assert!(matches!(
        k.host.events_for(&monitor, cap::SWITCH)[0].get("state"),
        Some(Value::BoolVal(true))
    ));
    let bulb = id(&k, "BULB1");
    let color = k.host.events_for(&bulb, cap::COLOR_LIGHT);
    assert!(matches!(
        color[0].get("color_temp_kelvin"),
        Some(Value::IntVal(2700))
    ));
    let dim = id(&k, "DIM1");
    assert!(matches!(
        k.host.events_for(&dim, cap::DIMMER)[0].get("level"),
        Some(Value::FloatVal(l)) if (l - 0.3).abs() < 1e-9
    ));
}

#[test]
fn sweep_is_paced_and_finds_unicast_only_devices() {
    let cfg = [
        ("discovery_broadcast", Value::BoolVal(false)),
        (
            "discovery_targets",
            Value::StringVal("192.168.1.0/24".into()),
        ),
    ];
    let k = discovered(&cfg, vec![(PLUG, plug())]);
    assert_eq!(k.net.udp_sent.len(), 254);
    assert!(k.net.udp_sent.iter().all(|a| !a.ip().is_broadcast()));
    assert_eq!(
        k.net.slept_ms,
        3 * (254 - 4),
        "3 ms between probes within a tick"
    );
    assert!(k.device_id_of("PLUG1").is_some());
}

/// Before slice 8.3 the host refuses broadcast: the sweep still finds
/// devices, and without one the plugin says what to configure, once.
#[test]
fn refused_broadcast_falls_back_to_the_sweep() {
    let run = |cfg: &[(&str, Value)]| {
        let mut net = FakeNet {
            now_ms: 1_000,
            refuse_broadcast: true,
            ..FakeNet::default()
        };
        net.iot.insert(PLUG, plug());
        let mut k = Kasa::new(FakeHost::with_config(cfg), net);
        k.init();
        for _ in 0..8 {
            k.tick();
            k.net.advance(500);
        }
        k
    };
    let warnings = |k: &Kasa<FakeHost, FakeNet>| {
        k.host
            .logs
            .iter()
            .filter(|(l, m)| matches!(l, Level::Warn) && m.contains("discovery_targets"))
            .count()
    };

    let swept = run(&[("discovery_targets", Value::StringVal("192.168.1.10".into()))]);
    assert!(swept.device_id_of("PLUG1").is_some());
    assert_eq!(warnings(&swept), 0);

    let mut alone = run(&[("discovery_interval_s", Value::IntVal(60))]);
    assert!(alone.device_id_of("PLUG1").is_none());
    assert_eq!(warnings(&alone), 1);
    assert!(!alone.net.udp_open, "no listening window with nothing sent");
    // The next run doesn't repeat the warning.
    alone.net.advance(60_000);
    for _ in 0..8 {
        alone.tick();
        alone.net.advance(500);
    }
    assert_eq!(warnings(&alone), 1);

    // Broadcast off and no targets: the same warning.
    let off = run(&[("discovery_broadcast", Value::BoolVal(false))]);
    assert_eq!(warnings(&off), 1);
    assert_eq!(off.net.udp_sent, []);
}

/// Without the broadcast only hosts a probe went out to were asked, so a
/// reply from anyone else (outside the targets, not probed yet, or whose
/// probe failed) is ignored rather than adopted.
#[test]
fn unsolicited_replies_are_ignored_without_broadcast() {
    let evil = |id: &str| {
        let reply = json!({ "system": { "get_sysinfo": {
            "type": "IOT.SMARTPLUGSWITCH", "model": "HS103(US)", "alias": "Intruder",
            "deviceId": id, "mac": "EE:EE", "relay_state": 0 } } });
        crate::protocol::cipher::encrypt(reply.to_string().as_bytes())
    };
    let mut net = FakeNet {
        now_ms: 1_000,
        refuse_broadcast: true,
        refuse_to: vec![Ipv4Addr::new(192, 168, 1, 2)],
        ..FakeNet::default()
    };
    net.iot.insert(PLUG, plug());
    // Outside the targets.
    net.udp_inbox
        .push_back((Ipv4Addr::new(192, 168, 2, 1), evil("EVIL1")));
    // A target in a later batch (64 probes per tick), not probed yet.
    net.udp_inbox
        .push_back((Ipv4Addr::new(192, 168, 1, 200), evil("EVIL2")));
    let cfg = [(
        "discovery_targets",
        Value::StringVal("192.168.1.0/24".into()),
    )];
    let mut k = Kasa::new(FakeHost::with_config(&cfg), net);
    k.init();
    k.tick();
    // A target whose probe the host refused.
    k.net
        .udp_inbox
        .push_back((Ipv4Addr::new(192, 168, 1, 2), evil("EVIL3")));
    for _ in 0..8 {
        k.tick();
        k.net.advance(500);
    }
    assert!(k.device_id_of("PLUG1").is_some());
    for id in ["EVIL1", "EVIL2", "EVIL3"] {
        assert!(k.device_id_of(id).is_none(), "{id} adopted");
    }
}

#[test]
fn devices_persist_and_come_back_without_discovery() {
    let k = discovered(&[], vec![(PLUG, plug()), (STRIP, strip())]);
    assert!(k.host.storage.contains_key(STORAGE_KEY));
    let mut host = FakeHost::with_config(&[("discovery", Value::BoolVal(false))]);
    host.storage = k.host.storage.clone();
    let mut net = FakeNet::default();
    net.iot.insert(PLUG, plug());
    let mut k2 = Kasa::new(host, net);
    k2.init();
    let mut locals: Vec<String> = k2
        .host
        .devices
        .iter()
        .map(|(_, d)| d.local_id.clone())
        .collect();
    locals.sort();
    assert_eq!(
        locals,
        [
            "PLUG1",
            "PLUG1/power",
            "STRIP",
            "STRIP/outlet-00",
            "STRIP/outlet-01"
        ]
    );
    let plug = k2.host.device("PLUG1").unwrap();
    assert_eq!(plug.1.name, "Kettle");
    assert_eq!(plug.1.capabilities.len(), 3);
    k2.tick();
    assert!(k2.net.udp_sent.is_empty(), "discovery disabled");
}

#[test]
fn polling_publishes_changes_only_and_reads_energy() {
    let mut k = discovered(&[], vec![(PLUG, plug())]);
    let plug = id(&k, "PLUG1");
    let power = id(&k, "PLUG1/power");
    k.net.advance(30_000);
    k.tick();
    let energy = k.host.events_for(&plug, &Ext::EnergyMeter.key());
    assert_eq!(energy.len(), 1);
    assert!(
        matches!(energy[0].get("power_w"), Some(Value::FloatVal(p)) if (p - 60.0).abs() < 1e-9)
    );
    assert!(
        matches!(energy[0].get("today_wh"), Some(Value::FloatVal(w)) if (w - 120.0).abs() < 1e-9)
    );
    assert!(
        matches!(energy[0].get("month_wh"), Some(Value::FloatVal(w)) if (w - 200.0).abs() < 1e-9)
    );
    assert_eq!(k.host.events_for(&power, cap::SENSOR).len(), 1);
    // Nothing changed: the next poll publishes nothing new.
    let before = k.host.events.len();
    k.net.advance(30_000);
    k.tick();
    assert_eq!(k.host.events.len(), before);
    // The relay flips on the device (the app): the poll publishes it.
    k.net.iot.get_mut(&PLUG).unwrap().sysinfo["relay_state"] = json!(1);
    k.net.advance(30_000);
    k.tick();
    let sw = k.host.events_for(&plug, cap::SWITCH);
    assert!(matches!(
        sw.last().unwrap().get("state"),
        Some(Value::BoolVal(true))
    ));
}

#[test]
fn unreachable_device_logs_once_and_recovers() {
    let mut k = discovered(&[], vec![(PLUG, plug())]);
    k.net.iot.get_mut(&PLUG).unwrap().fail = Some(NetError::Sent("timeout".into()));
    for _ in 0..3 {
        k.net.advance(30_000);
        k.tick();
    }
    let warns = k
        .host
        .logs
        .iter()
        .filter(|(_, m)| m.contains("state poll failed"))
        .count();
    assert_eq!(warns, 1);
    k.net.iot.get_mut(&PLUG).unwrap().fail = None;
    k.net.advance(30_000);
    k.tick();
    assert!(
        k.host
            .logs
            .iter()
            .any(|(_, m)| m.contains("reachable again"))
    );
}

#[test]
fn relay_commands_have_h5_outcomes() {
    let mut k = discovered(&[], vec![(PLUG, plug())]);
    let plug = id(&k, "PLUG1");
    let r = k.execute_command(
        &plug,
        &cmd(
            "c1",
            cap::SWITCH,
            "set",
            vec![kv("state", Value::BoolVal(true))],
        ),
    );
    assert_eq!(bool_field(&r, "state"), Some(true));
    assert_eq!(k.net.iot[&PLUG].sysinfo["relay_state"], json!(1));
    // The confirmed change is an event (subscribers see it); the poll that
    // follows doesn't publish it again.
    let events = k.host.events_for(&plug, cap::SWITCH);
    assert!(matches!(
        events.last().and_then(|e| e.get("state")),
        Some(Value::BoolVal(true))
    ));
    let n = events.len();
    k.net.advance(30_000);
    k.tick();
    assert_eq!(k.host.events_for(&plug, cap::SWITCH).len(), n);

    let r = k.execute_command(&plug, &cmd("c2", cap::SWITCH, "toggle", vec![]));
    assert_eq!(bool_field(&r, "state"), Some(false));

    // Bad arguments → invalid-argument, nothing sent.
    let sent = k.net.tcp_count;
    let r = k.execute_command(&plug, &cmd("c3", cap::SWITCH, "set", vec![]));
    assert!(matches!(r, CommandResult::Err(Error::InvalidArgument(_))));
    let r = k.execute_command(
        &plug,
        &cmd(
            "c4",
            cap::DIMMER,
            "set",
            vec![kv("level", Value::FloatVal(0.5))],
        ),
    );
    assert!(matches!(r, CommandResult::Err(Error::InvalidArgument(_))));
    assert_eq!(k.net.tcp_count, sent);

    // Connect failure → err (nothing reached the device).
    k.net.iot.get_mut(&PLUG).unwrap().fail = Some(NetError::NotSent("refused".into()));
    let r = k.execute_command(
        &plug,
        &cmd(
            "c5",
            cap::SWITCH,
            "set",
            vec![kv("state", Value::BoolVal(true))],
        ),
    );
    assert!(matches!(r, CommandResult::Err(Error::Unavailable(_))));
    // Timeout after sending → unknown.
    k.net.iot.get_mut(&PLUG).unwrap().fail = Some(NetError::Sent("timeout".into()));
    let r = k.execute_command(
        &plug,
        &cmd(
            "c6",
            cap::SWITCH,
            "set",
            vec![kv("state", Value::BoolVal(true))],
        ),
    );
    assert!(matches!(r, CommandResult::Unknown(_)));
    // Device refuses → err.
    k.net.iot.get_mut(&PLUG).unwrap().fail = None;
    k.net.iot.get_mut(&PLUG).unwrap().err_code = -1;
    let r = k.execute_command(
        &plug,
        &cmd(
            "c7",
            cap::SWITCH,
            "set",
            vec![kv("state", Value::BoolVal(true))],
        ),
    );
    assert!(matches!(r, CommandResult::Err(Error::Unavailable(m)) if m.contains("-1")));

    let r = k.execute_command("nope", &cmd("c8", cap::SWITCH, "toggle", vec![]));
    assert!(matches!(r, CommandResult::Err(Error::NotFound(_))));
    let power = id(&k, "PLUG1/power");
    let r = k.execute_command(&power, &cmd("c9", cap::SWITCH, "toggle", vec![]));
    assert!(matches!(r, CommandResult::Err(Error::InvalidArgument(_))));
}

#[test]
fn led_energy_and_outlet_commands() {
    let mut k = discovered(&[], vec![(PLUG, plug()), (STRIP, strip())]);
    let plug = id(&k, "PLUG1");
    let r = k.execute_command(
        &plug,
        &cmd(
            "l",
            &Ext::StatusLed.key(),
            "set",
            vec![kv("on", Value::BoolVal(false))],
        ),
    );
    assert_eq!(bool_field(&r, "on"), Some(false));
    assert_eq!(k.net.iot[&PLUG].sysinfo["led_off"], json!(1));

    let r = k.execute_command(&plug, &cmd("e", &Ext::EnergyMeter.key(), "refresh", vec![]));
    assert!(matches!(&r, CommandResult::OkWithState(f) if f.iter().any(|kv| kv.key == "history")));

    let outlet = id(&k, "STRIP/outlet-01");
    let r = k.execute_command(&outlet, &cmd("o", cap::SWITCH, "toggle", vec![]));
    assert_eq!(bool_field(&r, "state"), Some(true));
    let last = k.net.iot[&STRIP].requests.last().unwrap().clone();
    assert_eq!(
        last["context"]["child_ids"],
        json!(["01"]),
        "reported id sent verbatim"
    );
    assert_eq!(k.net.iot[&STRIP].sysinfo["children"][1]["state"], json!(1));
    assert_eq!(
        k.net.iot[&STRIP].sysinfo["children"][0]["state"],
        json!(1),
        "other outlet untouched"
    );
}

#[test]
fn bulb_and_dimmer_commands() {
    let mut k = discovered(&[], vec![(BULB, bulb()), (DIMMER, dimmer())]);
    let bulb = id(&k, "BULB1");
    let r = k.execute_command(
        &bulb,
        &cmd(
            "b1",
            cap::DIMMER,
            "set",
            vec![kv("level", Value::FloatVal(0.75))],
        ),
    );
    assert!(matches!(r, CommandResult::OkWithState(_)));
    assert_eq!(
        k.net.iot[&BULB].sysinfo["light_state"]["brightness"],
        json!(75)
    );

    let hsv = vec![
        kv("hue", Value::FloatVal(0.5)),
        kv("saturation", Value::FloatVal(1.0)),
    ];
    let r = k.execute_command(&bulb, &cmd("b2", cap::COLOR_LIGHT, "set", hsv));
    assert!(matches!(r, CommandResult::OkWithState(_)));
    let ls = &k.net.iot[&BULB].sysinfo["light_state"];
    assert_eq!(
        (
            ls["hue"].clone(),
            ls["saturation"].clone(),
            ls["color_temp"].clone()
        ),
        (json!(180), json!(100), json!(0))
    );

    let r = k.execute_command(
        &bulb,
        &cmd(
            "b3",
            cap::COLOR_LIGHT,
            "set",
            vec![kv("color_temp_kelvin", Value::IntVal(4000))],
        ),
    );
    assert!(matches!(r, CommandResult::OkWithState(_)));
    assert_eq!(
        k.net.iot[&BULB].sysinfo["light_state"]["color_temp"],
        json!(4000)
    );
    let r = k.execute_command(
        &bulb,
        &cmd(
            "b4",
            cap::COLOR_LIGHT,
            "set",
            vec![kv("color_temp_kelvin", Value::IntVal(9000))],
        ),
    );
    assert!(matches!(r, CommandResult::Err(Error::InvalidArgument(_))));

    let r = k.execute_command(
        &bulb,
        &cmd(
            "b5",
            cap::SWITCH,
            "set",
            vec![kv("state", Value::BoolVal(false))],
        ),
    );
    assert_eq!(bool_field(&r, "state"), Some(false));
    assert_eq!(k.net.iot[&BULB].sysinfo["light_state"]["on_off"], json!(0));

    let dim = id(&k, "DIM1");
    let r = k.execute_command(
        &dim,
        &cmd(
            "d1",
            cap::DIMMER,
            "set",
            vec![kv("level", Value::IntVal(1))],
        ),
    );
    assert!(matches!(r, CommandResult::OkWithState(_)));
    assert_eq!(k.net.iot[&DIMMER].sysinfo["brightness"], json!(100));
    let r = k.execute_command(
        &dim,
        &cmd(
            "d2",
            cap::DIMMER,
            "set",
            vec![kv("level", Value::FloatVal(0.0))],
        ),
    );
    assert!(matches!(r, CommandResult::Err(Error::InvalidArgument(_))));
}

fn with_camera(cfg: &[(&str, Value)], cam: FakeCam) -> Kasa<FakeHost, FakeNet> {
    let mut k = discovered(cfg, vec![(CAM, camera_discovery())]);
    k.net.cams.insert(CAM, cam);
    // The first camera poll was due on adoption; run it now that the
    // camera answers.
    k.net.advance(30_000);
    k.tick();
    k
}

#[test]
fn camera_probe_registers_children_and_publishes_state() {
    let mut cam = FakeCam::basic();
    cam.ptz = true;
    let k = with_camera(&creds(), cam);
    let main = k.host.device("CAM1").unwrap().1.clone();
    let names: Vec<String> = main
        .capabilities
        .iter()
        .map(|c| format!("{c:?}").replace("CapabilitySpec::", ""))
        .collect();
    assert!(
        names[0].starts_with("VideoStream") && names[1].starts_with("AudioStream"),
        "{names:?}"
    );
    assert!(names.iter().any(|n| n.contains("pan-tilt")), "{names:?}");
    assert!(names.iter().any(|n| n.contains("status-led")), "{names:?}");
    assert!(
        !names.iter().any(|n| n.contains("motion")),
        "motion is opt-in"
    );
    assert!(k.device_id_of("CAM1/siren").is_some());
    assert!(k.device_id_of("CAM1/privacy").is_some());
    assert!(
        k.device_id_of("CAM1/spotlight").is_none(),
        "no dayNight module"
    );
    let privacy = id(&k, "CAM1/privacy");
    assert!(matches!(
        k.host.events_for(&privacy, cap::SWITCH)[0].get("state"),
        Some(Value::BoolVal(false))
    ));
    let cam_id = id(&k, "CAM1");
    let pt = k.host.events_for(&cam_id, &Ext::PanTilt.key());
    assert!(matches!(pt[0].get("presets"), Some(Value::JsonVal(s)) if s.contains("Door")));
    // Control auth is md5 hex of the password.
    assert!(k.net.cams[&CAM].auth_seen[0].starts_with("Basic "));
}

#[test]
fn camera_commands_are_accepted_then_completed() {
    let mut k = with_camera(&creds(), FakeCam::basic());
    let siren = id(&k, "CAM1/siren");
    let r = k.execute_command(
        &siren,
        &cmd(
            "s1",
            cap::SIREN,
            "set",
            vec![kv("state", Value::BoolVal(true))],
        ),
    );
    assert!(matches!(r, CommandResult::Accepted));
    k.tick();
    let (cid, res) = k.host.completed.last().unwrap();
    assert_eq!(cid, "s1");
    assert_eq!(bool_field(res, "state"), Some(true));
    assert_eq!(
        k.net.cams[&CAM].values["smartlife.cam.ipcamera.siren"],
        "on"
    );

    // Privacy on = camera off.
    let privacy = id(&k, "CAM1/privacy");
    k.execute_command(&privacy, &cmd("p1", cap::SWITCH, "toggle", vec![]));
    k.tick();
    assert_eq!(
        k.net.cams[&CAM].values["smartlife.cam.ipcamera.switch"],
        "off"
    );
    assert_eq!(
        bool_field(&k.host.completed.last().unwrap().1, "state"),
        Some(true)
    );

    let cam_id = id(&k, "CAM1");
    k.execute_command(
        &cam_id,
        &cmd("r1", &Ext::CameraControl.key(), "reboot", vec![]),
    );
    k.execute_command(
        &cam_id,
        &cmd("snap", &Ext::CameraControl.key(), "snapshot", vec![]),
    );
    k.tick();
    let done: Vec<&(String, CommandResult)> = k.host.completed.iter().rev().take(2).collect();
    assert!(matches!(done[1], (id, CommandResult::Ok) if id == "r1"));
    assert!(matches!(&done[0].1, CommandResult::OkWithState(_)));
    assert_eq!(k.host.blobs[0].0, "snapshot-kasa-cam1-08419f47.jpg");
    assert_eq!(k.host.blobs[0].2, "image/jpeg");

    // Device-side refusal → err; HTTP 401 → permission-denied; timeout → unknown.
    k.net.cams.get_mut(&CAM).unwrap().write_err = Some(-40709);
    k.execute_command(
        &siren,
        &cmd(
            "s2",
            cap::SIREN,
            "set",
            vec![kv("state", Value::BoolVal(false))],
        ),
    );
    k.tick();
    assert!(matches!(
        k.host.completed.last().unwrap().1,
        CommandResult::Err(Error::Unavailable(_))
    ));
    k.net.cams.get_mut(&CAM).unwrap().status = Some(401);
    k.execute_command(
        &siren,
        &cmd(
            "s3",
            cap::SIREN,
            "set",
            vec![kv("state", Value::BoolVal(false))],
        ),
    );
    k.tick();
    assert!(matches!(
        k.host.completed.last().unwrap().1,
        CommandResult::Err(Error::PermissionDenied(_))
    ));
    k.net.cams.get_mut(&CAM).unwrap().fail = Some(NetError::Sent("timeout".into()));
    k.execute_command(
        &siren,
        &cmd(
            "s4",
            cap::SIREN,
            "set",
            vec![kv("state", Value::BoolVal(false))],
        ),
    );
    k.tick();
    assert!(matches!(
        k.host.completed.last().unwrap().1,
        CommandResult::Unknown(_)
    ));

    // Validation happens before acceptance.
    let r = k.execute_command(&cam_id, &cmd("m", &Ext::PanTilt.key(), "move", vec![]));
    assert!(
        matches!(r, CommandResult::Err(Error::InvalidArgument(_))),
        "no PTZ on this camera"
    );
}

#[test]
fn camera_without_credentials_is_registered_but_unavailable() {
    let k0 = discovered(&[], vec![(CAM, camera_discovery())]);
    let mut k = k0;
    k.net.cams.insert(CAM, FakeCam::basic());
    k.net.advance(30_000);
    k.tick();
    assert!(
        k.net.cams[&CAM].calls.is_empty(),
        "no calls without credentials"
    );
    let cam_id = id(&k, "CAM1");
    let r = k.execute_command(
        &cam_id,
        &cmd("r", &Ext::CameraControl.key(), "reboot", vec![]),
    );
    assert!(
        matches!(r, CommandResult::Err(Error::Unavailable(m)) if m.contains("camera_password"))
    );
}

#[test]
fn ptz_commands() {
    let mut cam = FakeCam::basic();
    cam.ptz = true;
    let mut k = with_camera(&creds(), cam);
    let cam_id = id(&k, "CAM1");
    let mv = vec![
        kv("direction", Value::StringVal("up".into())),
        kv("magnitude", Value::FloatVal(0.45)),
    ];
    assert!(matches!(
        k.execute_command(&cam_id, &cmd("m1", &Ext::PanTilt.key(), "move", mv)),
        CommandResult::Accepted
    ));
    let go = vec![kv("index", Value::IntVal(1))];
    k.execute_command(&cam_id, &cmd("m2", &Ext::PanTilt.key(), "goto-preset", go));
    k.tick();
    let calls = &k.net.cams[&CAM].calls;
    let mv = calls.iter().find(|c| c.1 == "set_move").unwrap();
    assert_eq!(mv.2, json!({ "direction": "top", "speed": 5 }));
    assert!(
        calls
            .iter()
            .any(|c| c.1 == "set_run_to_preset" && c.2 == json!({ "index": 1 }))
    );
    k.net.cams.get_mut(&CAM).unwrap().write_err = Some(-41203);
    let mv = vec![kv("direction", Value::StringVal("left".into()))];
    k.execute_command(&cam_id, &cmd("m3", &Ext::PanTilt.key(), "move", mv));
    k.tick();
    assert!(matches!(
        k.host.completed.last().unwrap().1,
        CommandResult::Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn motion_polling_holds_and_pauses_in_privacy() {
    let mut cfg = creds();
    cfg.push(("camera_motion", Value::BoolVal(true)));
    let mut k = with_camera(&cfg, FakeCam::basic());
    let cam_id = id(&k, "CAM1");
    k.net.cams.get_mut(&CAM).unwrap().moving = true;
    k.net.advance(1_000);
    k.tick();
    let motion = |k: &Kasa<FakeHost, FakeNet>| -> Vec<Option<bool>> {
        k.host
            .events_for(&cam_id, &Ext::Motion.key())
            .iter()
            .map(|e| match e.get("detected") {
                Some(Value::BoolVal(b)) => Some(*b),
                _ => None,
            })
            .collect()
    };
    // The first successful poll publishes the idle state, then motion.
    assert_eq!(motion(&k), [Some(false), Some(true)]);
    k.net.cams.get_mut(&CAM).unwrap().moving = false;
    for _ in 0..5 {
        k.net.advance(1_000);
        k.tick();
    }
    assert_eq!(motion(&k).len(), 2, "held while within 10 s");
    for _ in 0..6 {
        k.net.advance(1_000);
        k.tick();
    }
    assert_eq!(motion(&k).last(), Some(&Some(false)));

    // Camera turned off in the app: motion polls get -40919 and slow down.
    k.net
        .cams
        .get_mut(&CAM)
        .unwrap()
        .values
        .insert("smartlife.cam.ipcamera.switch".into(), "off".into());
    k.net.advance(1_000);
    k.tick();
    let polls = |k: &Kasa<FakeHost, FakeNet>| {
        k.net.cams[&CAM]
            .calls
            .iter()
            .filter(|c| c.1 == "get_detect_event_state")
            .count()
    };
    let n = polls(&k);
    k.net.advance(1_000);
    k.tick();
    assert_eq!(polls(&k), n, "privacy: no poll a second later");
}

#[test]
fn stream_pipeline_pumps_rtp_into_pipes() {
    let mut cam = FakeCam::basic();
    let mut body =
        b"--data-boundary--\r\nContent-Type: video/x-h264\r\nContent-Length: 8\r\n\r\n".to_vec();
    body.extend_from_slice(&[0, 0, 0, 1, 0x65, 1, 2, 3]);
    body.extend_from_slice(
        b"--data-boundary--\r\nContent-Type: audio/g711u\r\nContent-Length: 160\r\n\r\n",
    );
    body.extend_from_slice(&[0xff; 160]);
    cam.stream = vec![body];
    let mut k = with_camera(&creds(), cam);
    let cam_id = id(&k, "CAM1");
    let p = k.setup_pipeline(&cam_id).unwrap();
    assert!(matches!(
        p.activation,
        crate::media::ActivationPolicy::OnDemand
    ));
    assert!(k.streaming("CAM1"));
    // Stream auth: base64(password) as the password.
    assert_eq!(
        k.net.cams[&CAM].auth_seen.last().unwrap(),
        "Basic bWVAZXhhbXBsZS5jb206YUhWdWRHVnlNZz09"
    );
    k.tick();
    assert_eq!(k.host.pipes.len(), 2);
    assert_eq!(
        k.host.pipes[0].1.borrow().0.len(),
        1,
        "one video RTP packet"
    );
    assert_eq!(
        k.host.pipes[1].1.borrow().0.len(),
        1,
        "one audio RTP packet"
    );
    // The fake stream then closes: the session ends and the pipes close.
    k.tick();
    assert!(!k.streaming("CAM1"));
    assert!(k.host.pipes[0].1.borrow().1);
    assert!(k.setup_pipeline(&id(&k, "CAM1/siren")).is_err());
}

#[test]
fn queued_camera_toggles_run_in_order() {
    let mut k = with_camera(&creds(), FakeCam::basic());
    let siren = id(&k, "CAM1/siren");
    // The siren answers on `siren`, not `switch`.
    let r = k.execute_command(&siren, &cmd("t0", cap::SWITCH, "toggle", vec![]));
    assert!(
        matches!(r, CommandResult::Err(Error::InvalidArgument(_))),
        "{r:?}"
    );
    k.execute_command(&siren, &cmd("t1", cap::SIREN, "toggle", vec![]));
    k.execute_command(&siren, &cmd("t2", cap::SIREN, "toggle", vec![]));
    k.tick();
    let n = k.host.completed.len();
    assert_eq!(bool_field(&k.host.completed[n - 2].1, "state"), Some(true));
    assert_eq!(bool_field(&k.host.completed[n - 1].1, "state"), Some(false));
    assert_eq!(
        k.net.cams[&CAM].values["smartlife.cam.ipcamera.siren"],
        "off"
    );
    let events = k.host.events_for(&siren, cap::SIREN);
    assert!(matches!(
        events.last().and_then(|e| e.get("state")),
        Some(Value::BoolVal(false))
    ));
}

#[test]
fn restored_outlets_need_a_read_before_toggle() {
    let k = discovered(&[], vec![(STRIP, strip())]);
    assert!(k.host.storage.contains_key(STORAGE_KEY));
    let mut host = FakeHost::with_config(&[("discovery", Value::BoolVal(false))]);
    host.storage = k.host.storage.clone();
    let mut net = FakeNet::default();
    net.iot.insert(STRIP, strip());
    let mut k = Kasa::new(host, net);
    let before = format!("{:?}", k.host.storage);
    k.init();
    assert_eq!(
        format!("{:?}", k.host.storage),
        before,
        "replay doesn't rewrite storage"
    );
    let outlet = id(&k, "STRIP/outlet-00");
    let r = k.execute_command(&outlet, &cmd("t", cap::SWITCH, "toggle", vec![]));
    assert!(matches!(r, CommandResult::Err(Error::Unavailable(_))));
    k.net.advance(60_000);
    k.tick();
    let r = k.execute_command(&outlet, &cmd("t", cap::SWITCH, "toggle", vec![]));
    assert_eq!(bool_field(&r, "state"), Some(false), "outlet 00 was on");
}

#[test]
fn bulb_cache_follows_confirmed_commands() {
    let mut k = discovered(&[], vec![(BULB, bulb()), (DIMMER, dimmer())]);
    let bulb = id(&k, "BULB1");
    k.execute_command(
        &bulb,
        &cmd(
            "b",
            cap::DIMMER,
            "set",
            vec![kv("level", Value::FloatVal(0.9))],
        ),
    );
    let hsv = vec![
        kv("hue", Value::FloatVal(0.5)),
        kv("saturation", Value::FloatVal(1.0)),
    ];
    let r = k.execute_command(&bulb, &cmd("c", cap::COLOR_LIGHT, "set", hsv));
    assert!(matches!(&r, CommandResult::OkWithState(f)
        if f.iter().any(|kv| kv.key == "color_temp_kelvin" && matches!(kv.value, Value::IntVal(0)))));
    assert_eq!(
        k.net.iot[&BULB].sysinfo["light_state"]["brightness"],
        json!(90),
        "brightness kept"
    );
    let dim = id(&k, "DIM1");
    let r = k.execute_command(
        &dim,
        &cmd(
            "d",
            cap::DIMMER,
            "set",
            vec![kv("level", Value::FloatVal(0.001))],
        ),
    );
    assert!(matches!(&r, CommandResult::OkWithState(f)
        if matches!(f[0].value, Value::FloatVal(l) if (l - 0.01).abs() < 1e-9)));
}

#[test]
fn unconfirmed_writes_are_unknown() {
    let mut k = discovered(&[], vec![(PLUG, plug())]);
    let plug = id(&k, "PLUG1");
    // A reply without err_code doesn't confirm the write.
    k.net.iot.get_mut(&PLUG).unwrap().err_code = i64::MIN;
    let r = k.execute_command(
        &plug,
        &cmd(
            "x",
            cap::SWITCH,
            "set",
            vec![kv("state", Value::BoolVal(true))],
        ),
    );
    assert!(matches!(r, CommandResult::Unknown(_)), "{r:?}");
}

#[test]
fn ambiguous_write_needs_a_read_before_toggle() {
    let mut k = discovered(&[], vec![(PLUG, plug())]);
    let plug = id(&k, "PLUG1");
    // The write lands but its acknowledgement is unusable.
    k.net.iot.get_mut(&PLUG).unwrap().err_code = i64::MIN;
    let on = vec![kv("state", Value::BoolVal(true))];
    let r = k.execute_command(&plug, &cmd("a", cap::SWITCH, "set", on));
    assert!(matches!(r, CommandResult::Unknown(_)), "{r:?}");
    k.net.iot.get_mut(&PLUG).unwrap().err_code = 0;
    let r = k.execute_command(&plug, &cmd("b", cap::SWITCH, "toggle", vec![]));
    assert!(
        matches!(r, CommandResult::Err(Error::Unavailable(_))),
        "{r:?}"
    );
    k.net.advance(30_000);
    k.tick();
    let r = k.execute_command(&plug, &cmd("c", cap::SWITCH, "toggle", vec![]));
    assert_eq!(bool_field(&r, "state"), Some(false));
}

#[test]
fn outlet_rejects_the_strip_led() {
    let mut k = discovered(&[], vec![(STRIP, strip())]);
    let outlet = id(&k, "STRIP/outlet-00");
    let sent = k.net.tcp_count;
    let r = k.execute_command(
        &outlet,
        &cmd(
            "l",
            &Ext::StatusLed.key(),
            "set",
            vec![kv("on", Value::BoolVal(true))],
        ),
    );
    assert!(
        matches!(r, CommandResult::Err(Error::InvalidArgument(_))),
        "{r:?}"
    );
    assert_eq!(k.net.tcp_count, sent);
}

#[test]
fn unreadable_storage_holds_saves_until_read() {
    let k = discovered(&[], vec![(PLUG, plug()), (STRIP, strip())]);
    let stored = format!("{:?}", k.host.storage.get(STORAGE_KEY));
    let mut host = FakeHost::default();
    host.storage = k.host.storage.clone();
    host.fail_storage_get = true;
    let mut net = FakeNet::default();
    // Only the plug is online.
    net.iot.insert(PLUG, plug());
    let mut k2 = Kasa::new(host, net);
    k2.init();
    for _ in 0..8 {
        k2.tick();
        k2.net.advance(500);
    }
    assert_eq!(
        format!("{:?}", k2.host.storage.get(STORAGE_KEY)),
        stored,
        "not overwritten"
    );
    k2.host.fail_storage_get = false;
    k2.tick();
    assert!(
        k2.device_id_of("STRIP").is_some(),
        "restored after the read"
    );
}

#[test]
fn refused_idle_motion_is_retried() {
    let mut cfg = creds();
    cfg.push(("camera_motion", Value::BoolVal(true)));
    let mut k = with_camera(&cfg, FakeCam::basic());
    let cam_id = id(&k, "CAM1");
    k.net.cams.get_mut(&CAM).unwrap().moving = true;
    k.net.advance(1_000);
    k.tick();
    k.net.cams.get_mut(&CAM).unwrap().moving = false;
    k.host.fail_publish = true;
    for _ in 0..12 {
        k.net.advance(1_000);
        k.tick();
    }
    k.host.fail_publish = false;
    k.net.advance(250);
    k.tick();
    let last = k.host.events_for(&cam_id, &Ext::Motion.key());
    assert!(matches!(
        last.last().and_then(|e| e.get("detected")),
        Some(Value::BoolVal(false))
    ));
}

#[test]
fn malformed_ptz_arguments_are_rejected() {
    let mut cam = FakeCam::basic();
    cam.ptz = true;
    let mut k = with_camera(&creds(), cam);
    let cam_id = id(&k, "CAM1");
    for bad in [
        kv("speed", Value::StringVal("fast".into())),
        kv("magnitude", Value::StringVal("big".into())),
    ] {
        let args = vec![kv("direction", Value::StringVal("left".into())), bad];
        let r = k.execute_command(&cam_id, &cmd("p", &Ext::PanTilt.key(), "move", args));
        assert!(
            matches!(r, CommandResult::Err(Error::InvalidArgument(_))),
            "{r:?}"
        );
    }
}

#[test]
fn slow_devices_spread_work_across_ticks() {
    let mut k = with_camera(&creds(), FakeCam::basic());
    let cam_id = id(&k, "CAM1");
    for n in 0..6 {
        k.execute_command(
            &cam_id,
            &cmd(
                &format!("r{n}"),
                &Ext::CameraControl.key(),
                "reboot",
                vec![],
            ),
        );
    }
    // Each camera call now takes a full 5 s timeout.
    k.net.latency_ms = 5_000;
    let before = k.host.completed.len();
    k.tick();
    let first = k.host.completed.len() - before;
    assert!(
        (1..6).contains(&first),
        "budgeted: {first} of 6 in one tick"
    );
    for _ in 0..4 {
        k.tick();
    }
    assert_eq!(
        k.host.completed.len() - before,
        6,
        "the rest follow in order"
    );
    let ids: Vec<&str> = k.host.completed[before..]
        .iter()
        .map(|c| c.0.as_str())
        .collect();
    assert_eq!(ids, ["r0", "r1", "r2", "r3", "r4", "r5"]);
}

#[test]
fn inventory_is_one_small_record_per_device() {
    let mut s = strip();
    s.sysinfo["alias"] = json!("x".repeat(500));
    let mut k = discovered(&[], vec![(PLUG, plug()), (STRIP, s)]);
    let mut keys: Vec<&String> = k.host.storage.keys().collect();
    keys.sort();
    assert_eq!(keys, ["device/PLUG1", "device/STRIP", STORAGE_KEY]);
    let Some(Value::JsonVal(rec)) = k.host.storage.get("device/STRIP") else {
        panic!("no record");
    };
    assert!(rec.len() < 1_024, "names are clipped: {} bytes", rec.len());
    // Unchanged records aren't written again.
    k.host.storage.clear();
    k.net.advance(60_000);
    k.tick();
    assert!(k.host.storage.is_empty(), "{:?}", k.host.storage.keys());
}

#[test]
fn retired_outlets_are_removed_and_refused() {
    let mut k = discovered(&[], vec![(STRIP, strip())]);
    let old = id(&k, "STRIP/outlet-01");
    k.net.iot.get_mut(&STRIP).unwrap().sysinfo["children"] = json!([ { "id": "00", "alias": "Monitor", "state": 1 }, { "id": "02", "alias": "Fan", "state": 0 } ]);
    k.net.advance(60_000);
    k.tick();
    assert!(k.device_id_of("STRIP/outlet-02").is_some());
    assert!(k.device_id_of("STRIP/outlet-01").is_none());
    assert!(
        k.host.devices.iter().all(|(i, _)| i != &old),
        "removed on the host"
    );
    let on = vec![kv("state", Value::BoolVal(true))];
    let r = k.execute_command(&old, &cmd("o", cap::SWITCH, "set", on));
    assert!(matches!(r, CommandResult::Err(_)), "{r:?}");
}

#[test]
fn refused_camera_children_are_registered_on_a_later_probe() {
    let mut net = FakeNet {
        now_ms: 1_000,
        unix_ms: 1_700_000_000_000,
        ..FakeNet::default()
    };
    net.iot.insert(CAM, camera_discovery());
    net.cams.insert(CAM, FakeCam::basic());
    let mut k = Kasa::new(FakeHost::with_config(&creds()), net);
    k.init();
    // The camera itself registers; its children are refused at first.
    k.tick();
    k.host.refuse_register = true;
    for _ in 0..8 {
        k.net.advance(1_000);
        k.tick();
    }
    assert!(k.device_id_of("CAM1").is_some());
    assert!(k.device_id_of("CAM1/siren").is_none());
    k.host.refuse_register = false;
    k.net.advance(120_000);
    k.tick();
    assert!(
        k.device_id_of("CAM1/siren").is_some(),
        "retried on the next probe"
    );
}

#[test]
fn partial_bulb_reply_keeps_color_and_ptz_range_is_checked() {
    let mut k = discovered(&[], vec![(BULB, bulb())]);
    let lamp = id(&k, "BULB1");
    let n = k.host.events_for(&lamp, cap::COLOR_LIGHT).len();
    assert!(n > 0);
    k.net.iot.get_mut(&BULB).unwrap().sysinfo["light_state"] = json!({ "on_off": 0 });
    k.net.advance(60_000);
    k.tick();
    assert_eq!(
        k.host.events_for(&lamp, cap::COLOR_LIGHT).len(),
        n,
        "no invented color"
    );
    let mv = vec![
        kv("direction", Value::StringVal("left".into())),
        kv("magnitude", Value::FloatVal(2.0)),
    ];
    let mut cam = FakeCam::basic();
    cam.ptz = true;
    let mut c = with_camera(&creds(), cam);
    let cam_id = id(&c, "CAM1");
    let r = c.execute_command(&cam_id, &cmd("m", &Ext::PanTilt.key(), "move", mv));
    assert!(
        matches!(r, CommandResult::Err(Error::InvalidArgument(_))),
        "{r:?}"
    );
}

#[test]
fn partial_strip_reply_publishes_no_outlet_state() {
    let k = discovered(&[], vec![(STRIP, strip())]);
    let mut host = FakeHost::with_config(&[("discovery", Value::BoolVal(false))]);
    host.storage = k.host.storage.clone();
    let mut net = FakeNet::default();
    let mut s = strip();
    s.sysinfo.as_object_mut().unwrap().remove("children");
    net.iot.insert(STRIP, s);
    let mut k = Kasa::new(host, net);
    k.init();
    k.net.advance(60_000);
    k.tick();
    let outlet = id(&k, "STRIP/outlet-00");
    assert!(
        k.host.events_for(&outlet, cap::SWITCH).is_empty(),
        "no placeholder state"
    );
}

#[test]
fn reused_address_stops_commands_to_the_old_device() {
    let mut k = discovered(&[], vec![(PLUG, plug())]);
    let old = id(&k, "PLUG1");
    // DHCP hands the plug's address to another plug.
    let mut other = plug();
    other.sysinfo["deviceId"] = json!("PLUG2");
    other.sysinfo["mac"] = json!("CC:DD");
    k.net.iot.insert(PLUG, other);
    k.net.advance(60_000);
    k.tick();
    let sent = k.net.tcp_count;
    let on = vec![kv("state", Value::BoolVal(true))];
    let r = k.execute_command(&old, &cmd("x", cap::SWITCH, "set", on));
    assert!(
        matches!(r, CommandResult::Err(Error::Unavailable(_))),
        "{r:?}"
    );
    assert_eq!(k.net.tcp_count, sent, "nothing sent to the other plug");
    assert_eq!(k.net.iot[&PLUG].sysinfo["relay_state"], json!(0));
}

#[test]
fn energy_history_respects_the_tick_budget() {
    let k = discovered(&[], vec![(PLUG, plug())]);
    let mut host = FakeHost::with_config(&[("discovery", Value::BoolVal(false))]);
    host.storage = k.host.storage.clone();
    let mut net = FakeNet::default();
    net.iot.insert(PLUG, plug());
    let mut k = Kasa::new(host, net);
    k.init();
    k.net.advance(60_000);
    // Every exchange takes 4 s: sysinfo, realtime, time and two history
    // reads would be 20 s.
    k.net.latency_ms = 4_000;
    let start = k.net.now_ms;
    k.tick();
    assert!(
        k.net.now_ms - start <= crate::kasa::TICK_BUDGET_MS,
        "{} ms",
        k.net.now_ms - start
    );
}

#[test]
fn malformed_hsv_value_is_refused() {
    let mut k = discovered(&[], vec![(BULB, bulb())]);
    let lamp = id(&k, "BULB1");
    let sent = k.net.tcp_count;
    let args = vec![
        kv("hue", Value::FloatVal(0.5)),
        kv("saturation", Value::FloatVal(0.5)),
        kv("value", Value::StringVal("bad".into())),
    ];
    let r = k.execute_command(&lamp, &cmd("h", cap::COLOR_LIGHT, "set", args));
    assert!(
        matches!(r, CommandResult::Err(Error::InvalidArgument(_))),
        "{r:?}"
    );
    assert_eq!(k.net.tcp_count, sent);
}

#[test]
fn stale_address_survives_restart_and_mac_identifies_devices() {
    let mut k = discovered(&[], vec![(PLUG, plug())]);
    // The replacement reports no deviceId, only a different MAC.
    let mut other = plug();
    other.sysinfo.as_object_mut().unwrap().remove("deviceId");
    other.sysinfo["mac"] = json!("11:22:33:44:55:66");
    k.net.iot.insert(PLUG, other.clone());
    k.net.advance(60_000);
    k.tick();
    let old = id(&k, "PLUG1");
    let on = vec![kv("state", Value::BoolVal(true))];
    let r = k.execute_command(&old, &cmd("a", cap::SWITCH, "set", on.clone()));
    assert!(
        matches!(r, CommandResult::Err(Error::Unavailable(_))),
        "{r:?}"
    );

    // After a restart (no discovery yet) the address is still refused.
    let mut host = FakeHost::with_config(&[("discovery", Value::BoolVal(false))]);
    host.storage = k.host.storage.clone();
    let mut net = FakeNet::default();
    net.iot.insert(PLUG, other);
    let mut k2 = Kasa::new(host, net);
    k2.init();
    let old = id(&k2, "PLUG1");
    let r = k2.execute_command(&old, &cmd("b", cap::SWITCH, "set", on));
    assert!(
        matches!(r, CommandResult::Err(Error::Unavailable(_))),
        "{r:?}"
    );
    assert_eq!(k2.net.tcp_count, 0);
}

#[test]
fn mac_adopted_device_keeps_its_identity_when_sysinfo_adds_an_id() {
    // Discovery saw only a MAC; the TCP reply adds a deviceId.
    let mut p = plug();
    p.sysinfo.as_object_mut().unwrap().remove("deviceId");
    let mut k = discovered(&[], vec![(PLUG, p)]);
    let dev = id(&k, "AA:BB");
    k.net.iot.get_mut(&PLUG).unwrap().sysinfo["deviceId"] = json!("PLUG1");
    k.net.advance(60_000);
    k.tick();
    let on = vec![kv("state", Value::BoolVal(true))];
    let r = k.execute_command(&dev, &cmd("m", cap::SWITCH, "set", on));
    assert_eq!(bool_field(&r, "state"), Some(true), "{r:?}");
}

#[test]
fn outlet_aliases_are_bounded() {
    let mut s = strip();
    s.sysinfo["children"][0]["alias"] = json!("a".repeat(10_000));
    let k = discovered(&[], vec![(STRIP, s)]);
    let (_, info) = k.host.device("STRIP/outlet-00").unwrap();
    assert!(info.name.len() <= 64, "{}", info.name.len());
}
