//! Test doubles: a recording [`Host`] and a [`Net`] with simulated Kasa
//! devices behind it (smart-home devices on TCP/UDP 9999, cameras on
//! LINKIE2 and the stream port).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::rc::Rc;

use serde_json::{Value as Json, json};

use crate::media::PluginPipeConfig;
use crate::platform::{ByteStream, Host, Net, NetError, PipeSink};
use crate::protocol::http::{HttpRequest, HttpResponse};
use crate::protocol::{cipher, iot, linkie};
use oxidhome_sdk::bindings::oxidhome::plugin::devices::{CommandResult, DeviceInfo};
use oxidhome_sdk::bindings::oxidhome::plugin::logging::Level;
use oxidhome_sdk::bindings::oxidhome::plugin::types::{Error, KeyValue, Value};

/// One published state event.
#[derive(Debug, Clone)]
pub struct Published {
    pub device: String,
    pub capability: String,
    pub fields: Vec<KeyValue>,
}

impl Published {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields
            .iter()
            .find(|kv| kv.key == key)
            .map(|kv| &kv.value)
    }
}

/// Packets written into one pipe.
pub type PipeLog = Rc<RefCell<(Vec<Vec<u8>>, bool)>>;

/// A recording host.
#[derive(Default)]
pub struct FakeHost {
    pub config: HashMap<String, Value>,
    pub storage: HashMap<String, Value>,
    pub devices: Vec<(String, DeviceInfo)>,
    pub updates: Vec<(String, DeviceInfo)>,
    pub events: Vec<Published>,
    pub completed: Vec<(String, CommandResult)>,
    pub blobs: Vec<(String, Vec<u8>, String)>,
    pub logs: Vec<(Level, String)>,
    pub pipes: Vec<(PluginPipeConfig, PipeLog)>,
    pub fail_publish: bool,
    pub fail_storage_get: bool,
    pub refuse_register: bool,
    /// Last device id handed out; ids aren't reused after a removal.
    pub seq: usize,
}

impl FakeHost {
    pub fn with_config(pairs: &[(&str, Value)]) -> Self {
        Self {
            config: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            ..Self::default()
        }
    }

    /// The registered info of a local id.
    pub fn device(&self, local_id: &str) -> Option<&(String, DeviceInfo)> {
        self.devices.iter().find(|(_, d)| d.local_id == local_id)
    }

    /// Events for one device and capability.
    pub fn events_for(&self, device: &str, capability: &str) -> Vec<&Published> {
        self.events
            .iter()
            .filter(|e| e.device == device && e.capability == capability)
            .collect()
    }
}

struct FakePipe(PipeLog);

impl PipeSink for FakePipe {
    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.0.borrow_mut().0.push(data.to_vec());
        Ok(())
    }
    fn close(&mut self) {
        self.0.borrow_mut().1 = true;
    }
}

impl Host for FakeHost {
    fn register_device(&mut self, info: DeviceInfo) -> Result<String, Error> {
        if self.refuse_register {
            return Err(Error::PermissionDenied("device quota".into()));
        }
        self.seq += 1;
        let id = format!("dev-{}", self.seq);
        self.devices.push((id.clone(), info));
        Ok(id)
    }
    fn remove_device(&mut self, id: &str) -> Result<(), Error> {
        self.devices.retain(|(i, _)| i != id);
        Ok(())
    }
    fn update_device(&mut self, id: &str, info: DeviceInfo) -> Result<(), Error> {
        self.updates.push((id.to_string(), info.clone()));
        if let Some(d) = self.devices.iter_mut().find(|(i, _)| i == id) {
            d.1 = info;
        }
        Ok(())
    }
    fn publish_state(
        &mut self,
        device: &str,
        capability: &str,
        fields: Vec<KeyValue>,
        _unix_ms: u64,
    ) -> Result<(), Error> {
        if self.fail_publish {
            return Err(Error::Internal("publish failed".into()));
        }
        self.events.push(Published {
            device: device.to_string(),
            capability: capability.to_string(),
            fields,
        });
        Ok(())
    }
    fn complete_command(&mut self, id: &str, result: CommandResult) -> Result<(), Error> {
        self.completed.push((id.to_string(), result));
        Ok(())
    }
    fn config(&mut self, key: &str) -> Option<Value> {
        self.config.get(key).cloned()
    }
    fn storage_get(&mut self, key: &str) -> Result<Option<Value>, Error> {
        if self.fail_storage_get {
            return Err(Error::Unavailable("storage down".into()));
        }
        Ok(self.storage.get(key).cloned())
    }
    fn storage_set(&mut self, key: &str, value: Value) -> Result<(), Error> {
        self.storage.insert(key.to_string(), value);
        Ok(())
    }
    fn storage_delete(&mut self, key: &str) -> Result<(), Error> {
        self.storage.remove(key);
        Ok(())
    }
    fn blob_write(&mut self, name: &str, data: Vec<u8>, mime: &str) -> Result<String, Error> {
        self.blobs.push((name.to_string(), data, mime.to_string()));
        Ok(format!("blob-{}", self.blobs.len()))
    }
    fn log(&mut self, level: Level, message: &str) {
        self.logs.push((level, message.to_string()));
    }
    fn open_pipe(&mut self, config: PluginPipeConfig) -> Result<Box<dyn PipeSink>, Error> {
        let log = PipeLog::default();
        self.pipes.push((config, log.clone()));
        Ok(Box::new(FakePipe(log)))
    }
}

/// A simulated smart-home device: its `get_sysinfo` object plus meters.
#[derive(Debug, Clone)]
pub struct FakeIot {
    pub sysinfo: Json,
    pub realtime: Option<Json>,
    /// Fail the next exchanges with this error.
    pub fail: Option<NetError>,
    /// Answer every write with this `err_code`.
    pub err_code: i64,
    /// Requests received (decoded JSON).
    pub requests: Vec<Json>,
}

impl FakeIot {
    pub fn new(sysinfo: Json) -> Self {
        Self {
            sysinfo,
            realtime: None,
            fail: None,
            err_code: 0,
            requests: Vec::new(),
        }
    }

    fn handle(&mut self, req: &Json) -> Json {
        self.requests.push(req.clone());
        let child_ids: Vec<String> = req
            .pointer("/context/child_ids")
            .and_then(Json::as_array)
            .map_or_default(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            });
        let mut out = serde_json::Map::new();
        for (module, methods) in req.as_object().into_iter().flatten() {
            if module == "context" {
                continue;
            }
            let mut mo = serde_json::Map::new();
            for (method, args) in methods.as_object().into_iter().flatten() {
                mo.insert(
                    method.clone(),
                    self.method(module, method, args, &child_ids),
                );
            }
            out.insert(module.clone(), Json::Object(mo));
        }
        Json::Object(out)
    }

    fn method(&mut self, module: &str, method: &str, args: &Json, child_ids: &[String]) -> Json {
        let ok = |mut v: Json| {
            v["err_code"] = json!(0);
            v
        };
        let code = self.err_code;
        let write = |me: &mut Self, f: &dyn Fn(&mut Json)| {
            // `i64::MIN` simulates a write that landed but whose
            // acknowledgement came back without `err_code`.
            if code == 0 || code == i64::MIN {
                f(&mut me.sysinfo);
            }
            if code == i64::MIN {
                json!({})
            } else {
                json!({ "err_code": code })
            }
        };
        match (module, method) {
            ("system", "get_sysinfo") => ok(self.sysinfo.clone()),
            ("system", "set_relay_state") => {
                let s = args["state"].clone();
                if child_ids.is_empty() {
                    write(self, &|si| si["relay_state"] = s.clone())
                } else {
                    let ids = child_ids.to_vec();
                    write(self, &|si| {
                        for c in si["children"].as_array_mut().into_iter().flatten() {
                            let id = c["id"].as_str().unwrap_or_default().to_string();
                            if ids.contains(&id) {
                                c["state"] = s.clone();
                            }
                        }
                    })
                }
            }
            ("system", "set_led_off") => {
                let off = args["off"].clone();
                write(self, &|si| si["led_off"] = off.clone())
            }
            (iot::module::DIMMER, "set_brightness") => {
                let b = args["brightness"].clone();
                write(self, &|si| si["brightness"] = b.clone())
            }
            (iot::module::LIGHTING, "transition_light_state") => {
                let a = args.clone();
                write(self, &|si| {
                    for (k, v) in a.as_object().into_iter().flatten() {
                        if k != "ignore_default" && k != "transition_period" {
                            si["light_state"][k] = v.clone();
                        }
                    }
                })
            }
            (iot::module::EMETER, "get_realtime") => match &self.realtime {
                Some(r) => ok(r.clone()),
                None => json!({ "err_code": -1, "err_msg": "module not support" }),
            },
            (iot::module::TIME, "get_time") => ok(json!({ "year": 2026, "month": 3, "mday": 2 })),
            (iot::module::EMETER, "get_daystat") => ok(json!({ "day_list": [
                { "year": 2026, "month": 3, "day": 2, "energy_wh": 120 },
                { "year": 2026, "month": 3, "day": 1, "energy_wh": 80 },
            ]})),
            (iot::module::EMETER, "get_monthstat") => ok(json!({ "month_list": [
                { "year": 2026, "month": 3, "energy_wh": 200 },
            ]})),
            _ => json!({ "err_code": -2, "err_msg": "member not support" }),
        }
    }
}

/// A simulated camera.
#[derive(Debug, Clone, Default)]
pub struct FakeCam {
    /// On/off values by full module name.
    pub values: HashMap<String, String>,
    pub ptz: bool,
    pub moving: bool,
    /// Answer every call with this HTTP status instead.
    pub status: Option<u16>,
    /// Override the `err_code` of writes.
    pub write_err: Option<i64>,
    pub fail: Option<NetError>,
    /// `(module, method, args)` of every call.
    pub calls: Vec<(String, String, Json)>,
    /// The live stream body, served in chunks.
    pub stream: Vec<Vec<u8>>,
    pub jpeg: Vec<u8>,
    pub auth_seen: Vec<String>,
}

impl FakeCam {
    /// A camera with LED, siren and camera-enable (no spotlight).
    pub fn basic() -> Self {
        let mut values = HashMap::new();
        for (m, v) in [("led", "on"), ("siren", "off"), ("switch", "on")] {
            values.insert(format!("{}{m}", linkie::MODULE_PREFIX), v.to_string());
        }
        Self {
            values,
            jpeg: vec![0xff, 0xd8, 0xff, 0xe0, 1, 2, 3],
            ..Self::default()
        }
    }

    fn handle(&mut self, req: &Json) -> Json {
        let mut out = serde_json::Map::new();
        for (module, methods) in req.as_object().into_iter().flatten() {
            if module == "context" {
                continue;
            }
            let mut mo = serde_json::Map::new();
            for (method, args) in methods.as_object().into_iter().flatten() {
                self.calls
                    .push((module.clone(), method.clone(), args.clone()));
                mo.insert(method.clone(), self.method(module, method, args));
            }
            out.insert(module.clone(), Json::Object(mo));
        }
        Json::Object(out)
    }

    fn method(&mut self, module: &str, method: &str, args: &Json) -> Json {
        let short = module.strip_prefix(linkie::MODULE_PREFIX).unwrap_or(module);
        let off = self
            .values
            .get(&format!("{}switch", linkie::MODULE_PREFIX))
            .map(String::as_str)
            == Some("off");
        match (short, method) {
            ("ptz", "get_capability") if self.ptz => {
                json!({ "err_code": 0, "max_x": 360, "max_y": 120 })
            }
            ("ptz", "get_all_preset") if self.ptz => {
                json!({ "err_code": 0, "preset_attr": [{ "index": 1, "name": "Door" }] })
            }
            ("ptz", _) if self.ptz => json!({ "err_code": self.write_err.unwrap_or(0) }),
            ("vod", "get_detect_event_state") if off => {
                json!({ "err_code": linkie::err::CAMERA_OFF })
            }
            ("vod", "get_detect_event_state") => json!({ "err_code": 0, "is_moving": self.moving }),
            ("system", "set_reboot") => json!({ "err_code": self.write_err.unwrap_or(0) }),
            _ => match self.values.get_mut(module) {
                Some(v) if method.starts_with("get_") => json!({ "err_code": 0, "value": v }),
                Some(v) if method.starts_with("set_") => {
                    let code = self.write_err.unwrap_or(0);
                    if code == 0 {
                        *v = args["value"].as_str().unwrap_or_default().to_string();
                    }
                    json!({ "err_code": code })
                }
                _ => json!({ "err_code": linkie::err::UNSUPPORTED }),
            },
        }
    }
}

struct FakeStream(VecDeque<Vec<u8>>);

impl ByteStream for FakeStream {
    fn read_available(&mut self, _max: usize) -> Result<Vec<u8>, NetError> {
        self.0
            .pop_front()
            .ok_or_else(|| NetError::Sent("stream closed".into()))
    }
}

/// The simulated network.
#[derive(Default)]
pub struct FakeNet {
    pub iot: HashMap<Ipv4Addr, FakeIot>,
    pub cams: HashMap<Ipv4Addr, FakeCam>,
    /// Discovery replies only to unicast (simulates a host without broadcast).
    pub udp_open: bool,
    /// Refuse broadcast sends, as the host does before slice 8.3.
    pub refuse_broadcast: bool,
    pub udp_sent: Vec<SocketAddrV4>,
    pub udp_inbox: VecDeque<(Ipv4Addr, Vec<u8>)>,
    pub now_ms: u64,
    pub unix_ms: u64,
    pub slept_ms: u64,
    pub tcp_count: usize,
    /// Clock advance per TCP or HTTPS call (a slow or timing-out device).
    pub latency_ms: u64,
}

impl FakeNet {
    pub fn advance(&mut self, ms: u64) {
        self.now_ms += ms;
        self.unix_ms += ms;
    }

    fn discovery_reply(&mut self, ip: Ipv4Addr) {
        if let Some(d) = self.iot.get(&ip) {
            let reply = json!({ "system": { "get_sysinfo": d.sysinfo.clone() } });
            self.udp_inbox
                .push_back((ip, cipher::encrypt(reply.to_string().as_bytes())));
        }
    }
}

impl Net for FakeNet {
    fn tcp_exchange(
        &mut self,
        addr: SocketAddrV4,
        frame: &[u8],
        _timeout_ms: u64,
    ) -> Result<Vec<u8>, NetError> {
        self.tcp_count += 1;
        self.now_ms += self.latency_ms;
        let Some(d) = self.iot.get_mut(addr.ip()) else {
            return Err(NetError::NotSent("connection refused".into()));
        };
        if let Some(e) = d.fail.clone() {
            return Err(e);
        }
        assert_eq!(addr.port(), iot::IOT_PORT);
        let req = iot::decode_frame(frame).expect("plugin sent a valid frame");
        let reply = d.handle(&req);
        Ok(iot::encode_frame(&reply.to_string()))
    }
    fn udp_open(&mut self, _broadcast: bool) -> Result<(), NetError> {
        self.udp_open = true;
        Ok(())
    }
    fn udp_send(&mut self, to: SocketAddrV4, data: &[u8]) -> Result<(), NetError> {
        assert!(self.udp_open, "send on a closed socket");
        assert_eq!(to.port(), 9999);
        assert_eq!(cipher::decrypt(data), br#"{"system":{"get_sysinfo":{}}}"#);
        if self.refuse_broadcast && to.ip().is_broadcast() {
            return Err(NetError::NotSent("permission denied".into()));
        }
        self.udp_sent.push(to);
        if to.ip().is_broadcast() {
            let ips: Vec<Ipv4Addr> = self.iot.keys().copied().collect();
            for ip in ips {
                self.discovery_reply(ip);
            }
        } else {
            self.discovery_reply(*to.ip());
        }
        Ok(())
    }
    fn udp_try_recv(&mut self) -> Option<(Ipv4Addr, Vec<u8>)> {
        self.udp_inbox.pop_front()
    }
    fn udp_close(&mut self) {
        self.udp_open = false;
    }
    fn https(&mut self, req: &HttpRequest, _timeout_ms: u64) -> Result<HttpResponse, NetError> {
        self.now_ms += self.latency_ms;
        let ip: Ipv4Addr = req
            .host
            .parse()
            .map_err(|_| NetError::NotSent("bad host".into()))?;
        let Some(cam) = self.cams.get_mut(&ip) else {
            return Err(NetError::NotSent("connection refused".into()));
        };
        if let Some(e) = cam.fail.clone() {
            return Err(e);
        }
        cam.auth_seen
            .push(req.header("Authorization").unwrap_or_default().to_string());
        if let Some(s) = cam.status {
            return Ok(HttpResponse {
                status: s,
                headers: Vec::new(),
                body: Vec::new(),
            });
        }
        if req.path == crate::protocol::camera::SNAPSHOT_PATH {
            return Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "image/jpeg".into())],
                body: cam.jpeg.clone(),
            });
        }
        assert_eq!(req.port, linkie::LINKIE_PORT);
        assert!(
            req.header("User-Agent")
                .is_some_and(|u| u.starts_with("Kasa/"))
        );
        let json = linkie::decode_form_body(&req.body).expect("valid LINKIE2 body");
        let reply = cam.handle(&json);
        Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: linkie::encode_reply(&reply),
        })
    }
    fn https_stream(
        &mut self,
        req: &HttpRequest,
        _timeout_ms: u64,
    ) -> Result<(HttpResponse, Box<dyn ByteStream>), NetError> {
        let ip: Ipv4Addr = req
            .host
            .parse()
            .map_err(|_| NetError::NotSent("bad host".into()))?;
        let cam = self
            .cams
            .get_mut(&ip)
            .ok_or_else(|| NetError::NotSent("refused".into()))?;
        cam.auth_seen
            .push(req.header("Authorization").unwrap_or_default().to_string());
        Ok((
            HttpResponse {
                status: 200,
                headers: vec![(
                    "Content-Type".into(),
                    "multipart/x-mixed-replace;boundary=data-boundary--".into(),
                )],
                body: Vec::new(),
            },
            Box::new(FakeStream(cam.stream.iter().cloned().collect())),
        ))
    }
    fn monotonic_ms(&self) -> u64 {
        self.now_ms
    }
    fn unix_ms(&self) -> u64 {
        self.unix_ms
    }
    fn sleep_ms(&mut self, ms: u64) {
        self.slept_ms += ms;
    }
    fn random(&mut self, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = u8::try_from(i % 251).unwrap_or(0);
        }
    }
}
