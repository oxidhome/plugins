//! The plugin logic: discovery, registration, polling on `tick`, command
//! execution with H5 outcomes, and camera streams. Generic over [`Host`]
//! and [`Net`] so it runs natively against fakes.
//!
//! Command outcomes (ADR-0003):
//!
//! - Smart-home devices answer within one short TCP exchange, so their
//!   commands run inline: success is `ok-with-state`; a failure before
//!   anything was sent is `err`; a request that went out without a usable
//!   reply is `unknown`.
//! - Camera calls go over HTTPS to a camera that must be called serially
//!   and can take 2 × 5 s (stale kept-alive socket, then one retry). They
//!   answer `accepted` and run on the next `tick`, which reports the
//!   result with `complete-command`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};

use serde_json::{Value as Json, json};

use crate::config::Config;
use crate::mapping::{
    self, Ext, Features, Identity, Role, args, cap, color_fields, device_info, dimmer_fields,
    energy_fields, led_fields, main_role, motion_fields, pan_tilt_fields, power_fields,
    switch_fields,
};
use crate::media::{self, MediaPipeline, StreamSession};
use crate::platform::{Host, Net, NetError};
use crate::protocol::camera::{self, OnOffFeature, PtzCapability, PtzDirection, PtzPreset};
use crate::protocol::discovery::{self, Discovered, KasaClass};
use crate::protocol::http::{control_auth, stream_auth};
use crate::protocol::iot::{
    self, ChildOutlet, EmeterReading, EnergyHistory, IotError, LightChange, Request, SysInfo,
};
use crate::protocol::linkie::{self, LinkieError, LinkieRequest, err};
use crate::protocol::multipart;
use oxidhome_sdk::bindings::oxidhome::plugin::devices::{Command, CommandResult};
use oxidhome_sdk::bindings::oxidhome::plugin::logging::Level;
use oxidhome_sdk::bindings::oxidhome::plugin::types::{Error, KeyValue, Value};

/// Storage key of the adopted-device index (a JSON array of ids); each
/// device's record sits under [`record_key`].
pub const STORAGE_KEY: &str = "devices";

/// Longest name kept in a stored record, so one record stays small.
const MAX_STORED_NAME: usize = 64;

/// Storage key of one adopted device's record.
#[must_use]
pub fn record_key(kasa_id: &str) -> String {
    format!("device/{kasa_id}")
}
/// Smart-home polls per tick, so one tick stays short when many devices
/// are due together. With the manifest's 1 s tick that is 16 polls a
/// second, enough for [`MAX_DEVICES`] at the 30 s default interval.
pub const MAX_IOT_POLLS_PER_TICK: usize = 16;
/// Network time one tick may start new work in. The host's watchdog kills
/// a tick at 30 s; work begun before the budget runs out can still take
/// one full timeout (10 s for a snapshot), and a camera poll makes up to
/// three 5 s calls, so the budget leaves room for that.
pub const TICK_BUDGET_MS: u64 = 12_000;
/// Unicast probes sent per tick (3 ms apart, ~200 ms of a tick).
pub const PROBES_PER_TICK: usize = 64;
/// Most devices one instance adopts. Discovery replies are unauthenticated
/// UDP, so this bounds what a flood of fake identities can allocate.
pub const MAX_DEVICES: usize = 256;
/// Most outlets registered per strip (HS300 has 6).
pub const MAX_OUTLETS: usize = 16;
/// Longest accepted device id or outlet id.
pub const MAX_ID_LEN: usize = 64;
/// Discovery replies handled per tick.
pub const MAX_REPLIES_PER_TICK: usize = 64;
/// Energy history refresh cadence (it takes three requests and moves slowly).
pub const ENERGY_HISTORY_MS: u64 = 3_600_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Registered {
    device_id: String,
    role: Role,
}

#[derive(Debug, Default)]
struct CameraState {
    features: HashMap<OnOffFeature, bool>,
    ptz: Option<PtzCapability>,
    presets: Vec<PtzPreset>,
    motion: camera::MotionHold,
    next_motion_ms: u64,
    motion_backoff_ms: u64,
    motion_failing: bool,
}

/// Four independent flags (two reported, two tracked): bools are clearest.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
struct Physical {
    ident: Identity,
    class: KasaClass,
    port: u16,
    color: bool,
    temp: bool,
    features: Features,
    children: Vec<ChildOutlet>,
    registered: BTreeMap<String, Registered>,
    relay_on: Option<bool>,
    light: Option<iot::LightState>,
    next_poll_ms: u64,
    next_history_ms: u64,
    history: Option<EnergyHistory>,
    reading: Option<EmeterReading>,
    unreachable: bool,
    /// State has been read from the device since adoption (persisted
    /// outlet states are placeholders until then).
    observed: bool,
    /// Another device answered at this address (DHCP reuse): nothing is
    /// sent there until discovery finds this device again.
    address_stale: bool,
    cam: CameraState,
}

impl Physical {
    fn addr(&self) -> Option<SocketAddrV4> {
        if self.address_stale {
            return None;
        }
        self.ident
            .address
            .parse::<Ipv4Addr>()
            .ok()
            .map(|ip| SocketAddrV4::new(ip, self.port))
    }

    /// The address for camera HTTPS, unless it is stale.
    fn host(&self) -> Option<String> {
        (!self.address_stale).then(|| self.ident.address.clone())
    }

    fn main_role(&self) -> Role {
        main_role(self.class, self.color, self.temp)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum CameraOp {
    Set(OnOffFeature, bool),
    /// Resolved when it runs, after earlier queued writes landed.
    Toggle(OnOffFeature),
    Reboot,
    Snapshot,
    PtzMove(PtzDirection, u8),
    PtzPreset(i64),
}

#[derive(Debug)]
struct Pending {
    command_id: String,
    kasa_id: String,
    device_id: String,
    op: CameraOp,
}

#[derive(Debug)]
struct Sweep {
    targets: Vec<Ipv4Addr>,
    next: usize,
    listen_until_ms: Option<u64>,
}

enum CallError {
    Net(NetError),
    Iot(IotError),
}

enum CamError {
    NoCredentials,
    NoAddress,
    Net(NetError),
    Linkie(LinkieError),
}

impl std::fmt::Display for CamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CamError::NoCredentials => {
                f.write_str("camera_username / camera_password are not configured")
            }
            CamError::NoAddress => f.write_str("camera has no usable address"),
            CamError::Net(e) => write!(f, "{e}"),
            CamError::Linkie(e) => write!(f, "{e}"),
        }
    }
}

/// The plugin.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent lifecycle flags (restore, dirty, unread inventory, warned), not a state machine"
)]
pub struct Kasa<H: Host, N: Net> {
    /// The host surface.
    pub host: H,
    /// The network surface.
    pub net: N,
    cfg: Config,
    devices: BTreeMap<String, Physical>,
    routes: HashMap<String, (String, Role)>,
    published: HashMap<(String, String), String>,
    queue: VecDeque<Pending>,
    sweep: Option<Sweep>,
    next_discovery_ms: Option<u64>,
    streams: HashMap<String, StreamSession>,
    /// The adopted-device list changed and must be saved.
    dirty: bool,
    /// Replaying the stored list (nothing to save yet).
    restoring: bool,
    /// The stored device list could not be read; saves wait for a read.
    inventory_unread: bool,
    tick_started_ms: u64,
    /// Inside `tick`: only then does [`TICK_BUDGET_MS`] apply.
    in_tick: bool,
    /// Device records as last stored, by Kasa id.
    saved: HashMap<String, String>,
    /// The index as last stored.
    saved_index: Option<String>,
    warned_full: bool,
    /// Warned that broadcast failed with no sweep to fall back on.
    warned_no_broadcast: bool,
}

fn unavailable(msg: impl Into<String>) -> CommandResult {
    CommandResult::Err(Error::Unavailable(msg.into()))
}

fn invalid(msg: impl Into<String>) -> CommandResult {
    CommandResult::Err(Error::InvalidArgument(msg.into()))
}

impl<H: Host, N: Net> Kasa<H, N> {
    /// A plugin instance; call [`Kasa::init`] next.
    pub fn new(host: H, net: N) -> Self {
        Self {
            host,
            net,
            cfg: Config::default(),
            devices: BTreeMap::new(),
            routes: HashMap::new(),
            published: HashMap::new(),
            queue: VecDeque::new(),
            sweep: None,
            next_discovery_ms: None,
            streams: HashMap::new(),
            dirty: false,
            restoring: false,
            inventory_unread: false,
            tick_started_ms: 0,
            in_tick: false,
            saved: HashMap::new(),
            saved_index: None,
            warned_full: false,
            warned_no_broadcast: false,
        }
    }

    fn log(&mut self, level: Level, msg: &str) {
        self.host.log(level, msg);
    }

    /// Loads config, re-registers persisted devices and schedules discovery.
    pub fn init(&mut self) {
        let host = &mut self.host;
        self.cfg = Config::load(|k| host.config(k));
        for w in self.cfg.warnings.clone() {
            self.log(Level::Warn, &w);
        }
        self.restore();
        self.dirty = false;
        if self.cfg.discovery {
            self.next_discovery_ms = Some(self.net.monotonic_ms());
        }
    }

    fn restore(&mut self) {
        // Replaying must not save: an interruption mid-way would leave a
        // truncated list behind.
        self.restoring = true;
        for (d, stale) in self.load_persisted() {
            // A retried read must not undo what was learned meanwhile.
            if !self.devices.contains_key(&d.id) {
                self.adopt(&d, false);
                // A device found live during a storage outage may hold
                // this address now.
                let address = d.address.to_string();
                let taken = self
                    .devices
                    .iter()
                    .any(|(id, p)| id != &d.id && p.ident.address == address && !p.address_stale);
                if let Some(p) = self.devices.get_mut(&d.id) {
                    p.address_stale = stale || taken;
                }
                if taken && !stale {
                    self.dirty = true;
                }
            }
        }
        self.restoring = false;
    }

    /// Closes the discovery socket and every stream.
    pub fn shutdown(&mut self) {
        self.net.udp_close();
        for (_, mut s) in self.streams.drain() {
            s.close();
        }
    }

    /// The registered host device ids (tests and diagnostics).
    #[must_use]
    pub fn device_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.routes.keys().cloned().collect();
        v.sort();
        v
    }

    /// The host device id of a local id.
    #[must_use]
    pub fn device_id_of(&self, local_id: &str) -> Option<String> {
        self.devices
            .values()
            .flat_map(|p| p.registered.iter())
            .find(|(l, _)| l.as_str() == local_id)
            .map(|(_, r)| r.device_id.clone())
    }

    // ── Persistence ────────────────────────────────────────────────────

    /// Reads the index, then each device's record. Any read error holds
    /// saves (see `inventory_unread`) and is retried next tick: saving
    /// now would replace the stored list with only the devices that happen
    /// to be online.
    /// Stored devices, each with whether its address was known stale.
    fn load_persisted(&mut self) -> Vec<(Discovered, bool)> {
        match self.read_inventory() {
            Ok(list) => {
                self.inventory_unread = false;
                list
            }
            Err(e) => {
                if !self.inventory_unread {
                    self.log(
                        Level::Warn,
                        &format!("reading the stored device list failed: {e:?}; retrying"),
                    );
                }
                self.inventory_unread = true;
                Vec::new()
            }
        }
    }

    fn read_inventory(&mut self) -> Result<Vec<(Discovered, bool)>, Error> {
        let Some(Value::JsonVal(s)) = self.host.storage_get(STORAGE_KEY)? else {
            return Ok(Vec::new());
        };
        let Ok(Json::Array(ids)) = serde_json::from_str::<Json>(&s) else {
            self.log(
                Level::Warn,
                "stored device list is unreadable; starting empty",
            );
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for id in ids.iter().filter_map(Json::as_str).take(MAX_DEVICES) {
            let Some(Value::JsonVal(rec)) = self.host.storage_get(&record_key(id))? else {
                continue;
            };
            let parsed = serde_json::from_str::<Json>(&rec).ok();
            if let Some(d) = parsed.as_ref().and_then(parse_record) {
                let stale = parsed
                    .as_ref()
                    .and_then(|v| v.get("address_stale"))
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                self.saved.insert(id.to_string(), rec);
                out.push((d, stale));
            }
        }
        // The index as stored, so an unchanged list isn't rewritten.
        self.saved_index = Some(s);
        Ok(out)
    }

    fn mark_dirty(&mut self) {
        if !self.restoring {
            self.dirty = true;
        }
    }

    /// Saves the adopted-device list once per tick when it changed.
    fn flush(&mut self) {
        if self.dirty && !self.inventory_unread && self.persist() {
            self.dirty = false;
        }
    }

    /// Writes changed device records, then the index, then drops records
    /// of devices that left. One value per device keeps each write far
    /// under the host's 64 KiB value cap. Whether everything was saved; a
    /// failure is retried next tick.
    fn persist(&mut self) -> bool {
        let records: Vec<(String, String)> = self
            .devices
            .iter()
            .map(|(id, p)| (id.clone(), record(id, p).to_string()))
            .collect();
        let mut ok = true;
        for (id, rec) in &records {
            if self.saved.get(id) == Some(rec) {
                continue;
            }
            match self
                .host
                .storage_set(&record_key(id), Value::JsonVal(rec.clone()))
            {
                Ok(()) => {
                    self.saved.insert(id.clone(), rec.clone());
                }
                Err(e) => {
                    ok = false;
                    self.log(Level::Warn, &format!("saving device {id} failed: {e:?}"));
                }
            }
        }
        if !ok {
            // The index must not name a record that isn't stored.
            return false;
        }
        let index = Json::Array(records.iter().map(|(id, _)| json!(id)).collect()).to_string();
        if self.saved_index.as_ref() != Some(&index) {
            if let Err(e) = self
                .host
                .storage_set(STORAGE_KEY, Value::JsonVal(index.clone()))
            {
                self.log(Level::Warn, &format!("persisting devices failed: {e:?}"));
                return false;
            }
            self.saved_index = Some(index);
        }
        let gone: Vec<String> = self
            .saved
            .keys()
            .filter(|id| !self.devices.contains_key(*id))
            .cloned()
            .collect();
        for id in gone {
            // A leftover record is harmless (the index doesn't name it).
            if self.host.storage_delete(&record_key(&id)).is_ok() {
                self.saved.remove(&id);
            }
        }
        true
    }

    // ── Adoption and registration ─────────────────────────────────────

    /// Adopts a discovered device: registers it on first sight, follows an
    /// address change, and applies the state in the reply.
    fn adopt(&mut self, d: &Discovered, live: bool) {
        // Whoever answers here, the address no longer belongs to another
        // adopted device; checked before any admission rule.
        if live && !d.id.is_empty() {
            self.claim_address(&d.id, &d.address.to_string());
        }
        let Some(class) = d.class else {
            return;
        };
        if d.id.is_empty() || d.id.len() > MAX_ID_LEN {
            return;
        }
        if !self.devices.contains_key(&d.id) && self.devices.len() >= MAX_DEVICES {
            if !std::mem::replace(&mut self.warned_full, true) {
                self.log(
                    Level::Warn,
                    &format!("{MAX_DEVICES} devices adopted; ignoring new ones"),
                );
            }
            return;
        }
        let address = d.address.to_string();
        let now = self.net.monotonic_ms();
        let mut changed = false;
        if let Some(p) = self.devices.get_mut(&d.id) {
            if live && std::mem::take(&mut p.address_stale) {
                changed = true;
            }
            if p.ident.address != address {
                p.ident.address.clone_from(&address);
                changed = true;
            }
            if let Some(fw) = &d.sysinfo.sw_ver
                && p.ident.firmware.as_deref() != Some(fw)
            {
                p.ident.firmware = Some(fw.clone());
                changed = true;
            }
        } else {
            let p = self.new_physical(d, class, live, now);
            self.log(
                Level::Info,
                &format!("adopted {} {} at {}", class.as_str(), p.ident.name, address),
            );
            self.devices.insert(d.id.clone(), p);
            changed = true;
        }
        self.register_all(&d.id);
        // A device the host refused stays out of memory (and of polling).
        if self
            .devices
            .get(&d.id)
            .is_some_and(|p| p.registered.is_empty())
        {
            self.devices.remove(&d.id);
            return;
        }
        if live && class != KasaClass::Camera {
            self.apply_sysinfo(&d.id, &d.sysinfo);
        }
        if changed {
            self.mark_dirty();
        }
    }

    /// Registers every role the device currently has.
    fn register_all(&mut self, kasa_id: &str) {
        let Some(p) = self.devices.get(kasa_id) else {
            return;
        };
        let mut roles = vec![(p.main_role(), None)];
        if p.features.emeter && matches!(p.main_role(), Role::Relay | Role::Dimmer) {
            roles.push((Role::Power, None));
        }
        for c in &p.children {
            roles.push((
                Role::Outlet {
                    child_id: c.id.clone(),
                },
                Some(c.alias.clone()),
            ));
        }
        for f in [
            OnOffFeature::Spotlight,
            OnOffFeature::Siren,
            OnOffFeature::CameraEnabled,
        ] {
            if p.cam.features.contains_key(&f) {
                roles.push((Role::CameraChild(f), None));
            }
        }
        for (role, name) in roles {
            self.ensure_registered(kasa_id, &role, name.as_deref());
        }
        self.retire_outlets(kasa_id);
    }

    /// Removes outlets the strip no longer reports, on the host and here.
    fn retire_outlets(&mut self, kasa_id: &str) {
        let Some(p) = self.devices.get(kasa_id) else {
            return;
        };
        let retired: Vec<(String, String)> = p
            .registered
            .iter()
            .filter(|(_, r)| match &r.role {
                Role::Outlet { child_id } => !p.children.iter().any(|c| &c.id == child_id),
                _ => false,
            })
            .map(|(local, r)| (local.clone(), r.device_id.clone()))
            .collect();
        for (local, id) in retired {
            if let Err(e) = self.host.remove_device(&id) {
                // Kept (and retried next read); commands to it are refused.
                self.log(Level::Warn, &format!("remove-device {local} failed: {e:?}"));
                continue;
            }
            self.routes.remove(&id);
            self.published.retain(|(dev, _), _| dev != &id);
            if let Some(p) = self.devices.get_mut(kasa_id) {
                p.registered.remove(&local);
            }
        }
    }

    /// A live reply from `kasa_id` at `address`: any other device still
    /// recorded there has moved, so commands must not reach this one.
    fn claim_address(&mut self, kasa_id: &str, address: &str) {
        let mut moved = Vec::new();
        for (id, p) in &mut self.devices {
            if id != kasa_id && p.ident.address == address && !p.address_stale {
                p.address_stale = true;
                moved.push(p.ident.name.clone());
            }
        }
        if !moved.is_empty() {
            self.mark_dirty();
        }
        for name in moved {
            self.log(
                Level::Warn,
                &format!("{name}: another device now answers at {address}; waiting for discovery"),
            );
        }
    }

    /// State for a device seen for the first time.
    fn new_physical(&mut self, d: &Discovered, class: KasaClass, live: bool, now: u64) -> Physical {
        let mut jitter = [0u8; 4];
        self.net.random(&mut jitter);
        let jitter_ms = u64::from(u32::from_le_bytes(jitter)) % self.cfg.poll_interval_ms.max(1);
        let s = &d.sysinfo;
        Physical {
            ident: Identity {
                kasa_id: d.id.clone(),
                name: s.alias.as_deref().filter(|a| !a.is_empty()).map_or_else(
                    || {
                        format!(
                            "Kasa {}",
                            clip(s.model.as_deref().unwrap_or(class.as_str()))
                        )
                    },
                    |a| clip(a).to_string(),
                ),
                model: s.model.as_deref().map(|m| clip(m).to_string()),
                firmware: s.sw_ver.as_deref().map(|f| clip(f).to_string()),
                mac: s.mac.clone(),
                address: d.address.to_string(),
                class: Some(class),
            },
            class,
            port: iot::IOT_PORT,
            color: s.is_color,
            temp: s.is_variable_color_temp,
            features: Features {
                led: s.led_off.is_some(),
                emeter: emeter_capable(class) && s.has_emeter(),
                motion: class == KasaClass::Camera && self.cfg.camera_motion,
                ptz: false,
            },
            children: bounded_children(&s.children),
            registered: BTreeMap::new(),
            relay_on: None,
            light: None,
            // A smart-home device found live was just read; cameras
            // (whose discovery reply carries no state) and persisted
            // devices poll soon, spread out so a restart doesn't hit
            // every device at once.
            next_poll_ms: now
                + if live && class != KasaClass::Camera {
                    self.cfg.poll_interval_ms
                } else {
                    jitter_ms
                },
            next_history_ms: now,
            history: None,
            reading: None,
            unreachable: false,
            observed: false,
            address_stale: false,
            cam: CameraState {
                motion_backoff_ms: camera::MOTION_POLL_MS,
                ..CameraState::default()
            },
        }
    }

    fn ensure_registered(&mut self, kasa_id: &str, role: &Role, outlet_name: Option<&str>) {
        let Some(p) = self.devices.get(kasa_id) else {
            return;
        };
        let info = device_info(&p.ident, role, p.features, outlet_name);
        let local = info.local_id.clone();
        if let Some(r) = p.registered.get(&local) {
            // Re-publish the info when capabilities or identity changed.
            let id = r.device_id.clone();
            let key = (id.clone(), "__info".to_string());
            let digest = format!("{info:?}");
            if self.published.get(&key) != Some(&digest) {
                match self.host.update_device(&id, info) {
                    Ok(()) => {
                        self.published.insert(key, digest);
                    }
                    Err(e) => {
                        self.log(Level::Warn, &format!("update-device {local} failed: {e:?}"));
                    }
                }
            }
            return;
        }
        let digest = format!("{info:?}");
        match self.host.register_device(info) {
            Ok(id) => {
                self.published.insert((id.clone(), "__info".into()), digest);
                self.routes
                    .insert(id.clone(), (kasa_id.to_string(), role.clone()));
                if let Some(p) = self.devices.get_mut(kasa_id) {
                    p.registered.insert(
                        local,
                        Registered {
                            device_id: id,
                            role: role.clone(),
                        },
                    );
                }
            }
            Err(e) => self.log(
                Level::Warn,
                &format!("register-device {local} failed: {e:?}"),
            ),
        }
    }

    fn device_id(&self, kasa_id: &str, role: &Role) -> Option<String> {
        let p = self.devices.get(kasa_id)?;
        p.registered
            .get(&mapping::local_id(kasa_id, role))
            .map(|r| r.device_id.clone())
    }

    /// Publishes a state change, only when it differs from the last one
    /// (each publish is a durable event-log row on the host).
    fn publish(&mut self, kasa_id: &str, role: &Role, capability: &str, fields: Vec<KeyValue>) {
        let Some(id) = self.device_id(kasa_id, role) else {
            return;
        };
        self.publish_to(&id, capability, fields);
    }

    /// [`Self::publish`] by host device id. The dedup cache only advances
    /// when the host took the event, so a refused one is retried.
    fn publish_to(&mut self, id: &str, capability: &str, fields: Vec<KeyValue>) {
        let key = (id.to_string(), capability.to_string());
        let digest = format!("{fields:?}");
        if self.published.get(&key) == Some(&digest) {
            return;
        }
        let now = self.net.unix_ms();
        match self.host.publish_state(id, capability, fields, now) {
            Ok(()) => {
                self.published.insert(key, digest);
            }
            Err(e) => self.log(Level::Warn, &format!("publish {capability} failed: {e:?}")),
        }
    }

    /// Publishes the state a confirmed command set. The host projects an
    /// `ok-with-state` into the device snapshot but only `publish-event`
    /// notifies subscribers, so automations see the change through this.
    fn publish_confirmed(&mut self, device_id: &str, capability: &str, fields: &[KeyValue]) {
        let key = (device_id.to_string(), capability.to_string());
        let digest = format!("{fields:?}");
        self.publish_to(device_id, capability, fields.to_vec());
        if self.published.get(&key) != Some(&digest) {
            // Refused: the host snapshot already holds the new state, so
            // the cache must not suppress the next observation.
            self.published.remove(&key);
        }
    }

    // ── Smart-home state ──────────────────────────────────────────────

    fn apply_sysinfo(&mut self, kasa_id: &str, s: &SysInfo) {
        let Some(p) = self.devices.get_mut(kasa_id) else {
            return;
        };
        // A strip's outlet states are only known from a reply that lists them.
        if p.class != KasaClass::Strip || !s.children.is_empty() {
            p.observed = true;
        }
        let mut reregister = false;
        if s.led_off.is_some() && !p.features.led {
            p.features.led = true;
            reregister = true;
        }
        let emeter = emeter_capable(p.class) && s.has_emeter();
        if emeter != p.features.emeter && s.feature.is_some() {
            p.features.emeter = emeter;
            reregister = true;
        }
        // A reply without children is partial, not a strip that lost all
        // its outlets.
        if p.class == KasaClass::Strip && !s.children.is_empty() {
            let children = bounded_children(&s.children);
            let ids: Vec<(&str, &str)> = p
                .children
                .iter()
                .map(|c| (c.id.as_str(), c.alias.as_str()))
                .collect();
            let new_ids: Vec<(&str, &str)> = children
                .iter()
                .map(|c| (c.id.as_str(), c.alias.as_str()))
                .collect();
            if ids != new_ids {
                reregister = true;
            }
            p.children = children;
        }
        let role = p.main_role();
        let class = p.class;
        // Every read reconciles: registrations the host refused earlier are
        // retried, retired outlets removed (no host calls when nothing
        // changed).
        self.register_all(kasa_id);
        if reregister {
            self.mark_dirty();
        }
        if let Some(off) = s.led_off {
            self.publish(kasa_id, &role, &Ext::StatusLed.key(), led_fields(!off));
        }
        match class {
            KasaClass::Plug | KasaClass::Switch | KasaClass::Dimmer => {
                if let Some(on) = s.relay_on {
                    if let Some(p) = self.devices.get_mut(kasa_id) {
                        p.relay_on = Some(on);
                    }
                    self.publish(kasa_id, &role, cap::SWITCH, switch_fields(on));
                }
                if class == KasaClass::Dimmer
                    && let Some(b) = s.brightness
                {
                    self.publish(kasa_id, &role, cap::DIMMER, dimmer_fields(b));
                }
            }
            KasaClass::Bulb => {
                if let Some(ls) = s.light {
                    self.apply_light(kasa_id, &role, ls);
                }
            }
            KasaClass::Strip => {
                // Only states this reply carries: cached ones may be
                // restore placeholders or stale after an ambiguous write.
                for c in bounded_children(&s.children) {
                    let r = Role::Outlet {
                        child_id: c.id.clone(),
                    };
                    self.publish(kasa_id, &r, cap::SWITCH, switch_fields(c.on));
                }
            }
            KasaClass::Camera => {}
        }
    }

    fn apply_light(&mut self, kasa_id: &str, role: &Role, mut ls: iot::LightState) {
        let (color, temp) = match self.devices.get_mut(kasa_id) {
            Some(p) => {
                // A reply can carry only `on_off`; keep what was known
                // instead of publishing invented color values.
                if let Some(prev) = p.light {
                    ls.brightness = ls.brightness.or(prev.brightness);
                    ls.hue = ls.hue.or(prev.hue);
                    ls.saturation = ls.saturation.or(prev.saturation);
                    ls.color_temp = ls.color_temp.or(prev.color_temp);
                }
                p.light = Some(ls);
                (p.color, p.temp)
            }
            None => return,
        };
        let known_color = ls.hue.is_some() || ls.saturation.is_some() || ls.color_temp.is_some();
        self.publish(kasa_id, role, cap::SWITCH, switch_fields(ls.on));
        if let Some(b) = ls.brightness {
            self.publish(kasa_id, role, cap::DIMMER, dimmer_fields(b));
        }
        if (color || temp) && known_color {
            self.publish(
                kasa_id,
                role,
                cap::COLOR_LIGHT,
                color_fields(
                    ls.hue.unwrap_or(0),
                    ls.saturation.unwrap_or(0),
                    ls.brightness.unwrap_or(100),
                    temp.then(|| ls.color_temp.unwrap_or(0)),
                ),
            );
        }
    }

    fn iot_call(&mut self, addr: SocketAddrV4, req: &Request) -> Result<Json, CallError> {
        let raw = self
            .net
            .tcp_exchange(addr, &req.to_frame(), iot::IOT_TIMEOUT_MS)
            .map_err(CallError::Net)?;
        iot::decode_frame(&raw).map_err(CallError::Iot)
    }

    fn poll_iot(&mut self, kasa_id: &str, now: u64) {
        let Some(p) = self.devices.get_mut(kasa_id) else {
            return;
        };
        p.next_poll_ms = now + self.cfg.poll_interval_ms;
        let (Some(addr), emeter, due_history) =
            (p.addr(), p.features.emeter, now >= p.next_history_ms)
        else {
            return;
        };
        let result = self
            .iot_call(addr, &iot::get_sysinfo())
            .and_then(|v| iot::parse_sysinfo_reply(&v).map_err(CallError::Iot));
        let s = match result {
            Ok(s) => s,
            Err(e) => {
                let msg = match e {
                    CallError::Net(n) => n.to_string(),
                    CallError::Iot(i) => i.to_string(),
                };
                if let Some(p) = self.devices.get_mut(kasa_id)
                    && !p.unreachable
                {
                    p.unreachable = true;
                    let name = p.ident.name.clone();
                    self.log(Level::Warn, &format!("{name}: state poll failed: {msg}"));
                }
                return;
            }
        };
        let stored_mac = self.devices.get(kasa_id).and_then(|p| p.ident.mac.clone());
        let same_mac = matches!((&stored_mac, s.mac.as_deref()), (Some(a), Some(b)) if norm_mac(a) == norm_mac(b));
        let other = if let Some(i) = s.device_id.as_deref().filter(|i| !i.is_empty()) {
            // A device adopted by MAC (no deviceId in discovery) is still
            // itself when its sysinfo adds one.
            (i != kasa_id && !same_mac).then(|| i.to_string())
        } else {
            // No device id: devices adopted by MAC are checked by MAC.
            let stored = self.devices.get(kasa_id).and_then(|p| p.ident.mac.clone());
            match (stored, s.mac.as_deref()) {
                (Some(a), Some(b)) if norm_mac(&a) != norm_mac(b) => Some(b.to_string()),
                _ => None,
            }
        };
        if let Some(other) = other {
            // DHCP gave this address to another device.
            self.mark_dirty();
            if let Some(p) = self.devices.get_mut(kasa_id) {
                p.address_stale = true;
                let name = p.ident.name.clone();
                self.log(
                    Level::Warn,
                    &format!(
                        "{name}: device {other} answers at its address; waiting for discovery"
                    ),
                );
            }
            return;
        }
        if let Some(p) = self.devices.get_mut(kasa_id)
            && p.unreachable
        {
            p.unreachable = false;
            let name = p.ident.name.clone();
            self.log(Level::Info, &format!("{name}: reachable again"));
        }
        self.apply_sysinfo(kasa_id, &s);
        if emeter {
            // Every energy exchange checks the tick budget; unfinished
            // history stays due and runs on a later poll.
            self.poll_energy(kasa_id, addr, due_history, now);
        }
    }

    /// Reads the energy meter (and the history when due) and publishes it.
    /// One request after the other: parallel connections to one plug
    /// occasionally lose a reply.
    fn poll_energy(
        &mut self,
        kasa_id: &str,
        addr: SocketAddrV4,
        with_history: bool,
        now: u64,
    ) -> (bool, Vec<KeyValue>) {
        if self.over_budget() {
            return (false, Vec::new());
        }
        let reading = self
            .iot_call(addr, &iot::get_emeter_realtime())
            .ok()
            .and_then(|v| iot::parse_emeter_reply(&v).ok().flatten());
        let history = if with_history {
            self.fetch_history(addr)
        } else {
            None
        };
        let Some(p) = self.devices.get_mut(kasa_id) else {
            return (false, Vec::new());
        };
        if reading.is_some() {
            p.reading = reading;
        }
        if history.is_some() {
            p.history = history;
            p.next_history_ms = now + ENERGY_HISTORY_MS;
        }
        let fields = energy_fields(p.reading.as_ref(), p.history.as_ref());
        let role = p.main_role();
        if let Some(r) = reading {
            self.publish(kasa_id, &Role::Power, cap::SENSOR, power_fields(r.power_w));
        }
        if !fields.is_empty() {
            self.publish(kasa_id, &role, &Ext::EnergyMeter.key(), fields.clone());
        }
        (reading.is_some(), fields)
    }

    /// `None` when a read fails or the tick runs out of budget between
    /// exchanges; the history then stays due for a later poll.
    fn fetch_history(&mut self, addr: SocketAddrV4) -> Option<EnergyHistory> {
        if self.over_budget() {
            return None;
        }
        let today = self
            .iot_call(addr, &iot::get_time())
            .ok()
            .and_then(|v| iot::parse_time_reply(&v).ok().flatten())?;
        let mut stats = Vec::new();
        for req in iot::energy_history_requests(today) {
            if self.over_budget() {
                return None;
            }
            let v = self.iot_call(addr, &req).ok()?;
            let (d, m) = iot::parse_energy_stats(&v);
            stats.extend(d);
            stats.extend(m);
        }
        Some(iot::energy_history(today, &stats))
    }

    // ── Cameras ───────────────────────────────────────────────────────

    fn linkie(&mut self, kasa_id: &str, req: &LinkieRequest) -> Result<Json, CamError> {
        let (user, pass) = self
            .cfg
            .camera_credentials()
            .map(|(u, p)| (u.to_string(), p.to_string()))
            .ok_or(CamError::NoCredentials)?;
        let host = self
            .devices
            .get(kasa_id)
            .and_then(Physical::host)
            .ok_or(CamError::NoAddress)?;
        let mut rnd = [0u8; 16];
        self.net.random(&mut rnd);
        let http = linkie::build_request(
            &host,
            linkie::LINKIE_PORT,
            &control_auth(&user, &pass),
            req,
            &linkie::uuid_v4(rnd),
        );
        let resp = self
            .net
            .https(&http, linkie::LINKIE_TIMEOUT_MS)
            .map_err(CamError::Net)?;
        linkie::decode_response(&resp).map_err(CamError::Linkie)
    }

    fn poll_camera(&mut self, kasa_id: &str, now: u64) {
        let Some(p) = self.devices.get_mut(kasa_id) else {
            return;
        };
        p.next_poll_ms = now + self.cfg.poll_interval_ms;
        if self.cfg.camera_credentials().is_none() {
            return;
        }
        let reply = match self.linkie(kasa_id, &camera::probe_all()) {
            Ok(v) => v,
            Err(e) => {
                if let Some(p) = self.devices.get_mut(kasa_id)
                    && !p.unreachable
                {
                    p.unreachable = true;
                    let name = p.ident.name.clone();
                    self.log(Level::Warn, &format!("{name}: camera probe failed: {e}"));
                }
                return;
            }
        };
        let ptz = camera::parse_ptz_capability(&reply);
        // `None` when the read failed: keep the last good list.
        let presets = if ptz.is_some() && !self.over_budget() {
            self.linkie(kasa_id, &camera::get_presets())
                .ok()
                .and_then(|v| camera::parse_presets(&v))
        } else {
            None
        };
        let Some(p) = self.devices.get_mut(kasa_id) else {
            return;
        };
        p.unreachable = false;
        let mut reregister = false;
        for f in OnOffFeature::ALL {
            if let Some(on) = camera::parse_state(&reply, f) {
                // Never dropped on a failed probe: an unreachable camera
                // looks like an unsupported one, and hardware doesn't lose
                // its spotlight.
                if p.cam.features.insert(f, on).is_none() {
                    reregister = true;
                }
            }
        }
        if p.cam.features.contains_key(&OnOffFeature::Led) && !p.features.led {
            p.features.led = true;
            reregister = true;
        }
        if let Some(c) = ptz {
            if !p.features.ptz {
                p.features.ptz = true;
                reregister = true;
            }
            p.cam.ptz = Some(c);
            if let Some(list) = presets {
                p.cam.presets = list;
            }
        }
        let states: Vec<(OnOffFeature, bool)> =
            p.cam.features.iter().map(|(f, v)| (*f, *v)).collect();
        let ptz_state = p.cam.ptz.map(|c| pan_tilt_fields(c, &p.cam.presets));
        // Every successful probe reconciles, so a registration or
        // capability update the host refused is retried.
        self.register_all(kasa_id);
        if reregister {
            self.mark_dirty();
        }
        for (f, on) in states {
            match f {
                OnOffFeature::Led => {
                    self.publish(
                        kasa_id,
                        &Role::Camera,
                        &Ext::StatusLed.key(),
                        led_fields(on),
                    );
                }
                // Privacy reads naturally in automations: on = camera off.
                OnOffFeature::CameraEnabled => {
                    self.publish(
                        kasa_id,
                        &Role::CameraChild(f),
                        cap::SWITCH,
                        switch_fields(!on),
                    );
                }
                _ => self.publish(
                    kasa_id,
                    &Role::CameraChild(f),
                    cap::SWITCH,
                    switch_fields(on),
                ),
            }
        }
        if let Some(fields) = ptz_state {
            self.publish(kasa_id, &Role::Camera, &Ext::PanTilt.key(), fields);
        }
    }

    fn poll_motion(&mut self, kasa_id: &str, now: u64) {
        let Some(p) = self.devices.get_mut(kasa_id) else {
            return;
        };
        // A camera that is off detects nothing.
        if p.cam.features.get(&OnOffFeature::CameraEnabled) == Some(&false) {
            p.cam.next_motion_ms = now + camera::MOTION_POLL_PRIVACY_MS;
            p.cam.motion.clear();
            self.publish_motion(kasa_id);
            return;
        }
        let unix_s = self.net.unix_ms() / 1000;
        let result = self.linkie(kasa_id, &camera::get_motion(unix_s));
        // Observations are stamped when the reply arrived.
        let now = self.net.monotonic_ms();
        let Some(p) = self.devices.get_mut(kasa_id) else {
            return;
        };
        let mut log = None;
        match result.and_then(|v| camera::parse_motion(&v).map_err(CamError::Linkie)) {
            Ok(moving) => {
                if p.cam.motion_failing {
                    p.cam.motion_failing = false;
                    log = Some((
                        Level::Info,
                        format!("{}: motion polling recovered", p.ident.name),
                    ));
                }
                p.cam.motion_backoff_ms = camera::MOTION_POLL_MS;
                p.cam.next_motion_ms = now + camera::MOTION_POLL_MS;
                p.cam.motion.observe(moving, now);
            }
            Err(CamError::Linkie(e)) if e.code() == Some(err::CAMERA_OFF) => {
                // Turned off outside the plugin; the state poll catches up
                // (as in the TS plugin, this doesn't decide privacy support).
                p.cam.next_motion_ms = now + camera::MOTION_POLL_PRIVACY_MS;
                p.cam.motion.clear();
            }
            Err(e) => {
                if !p.cam.motion_failing {
                    p.cam.motion_failing = true;
                    log = Some((
                        Level::Warn,
                        format!("{}: motion poll failed, backing off: {e}", p.ident.name),
                    ));
                }
                p.cam.next_motion_ms = now + p.cam.motion_backoff_ms;
                p.cam.motion_backoff_ms = camera::next_motion_backoff(p.cam.motion_backoff_ms);
                p.cam.motion.settle(now);
            }
        }
        if let Some((l, m)) = log {
            self.log(l, &m);
        }
        // The held state, every time: the deduper publishes transitions,
        // the first (idle) state, and retries a refused event.
        self.publish_motion(kasa_id);
    }

    fn publish_motion(&mut self, kasa_id: &str) {
        let detected = self
            .devices
            .get(kasa_id)
            .is_some_and(|p| p.cam.motion.detected());
        self.publish(
            kasa_id,
            &Role::Camera,
            &Ext::Motion.key(),
            motion_fields(detected),
        );
    }

    fn motion_published(&self, kasa_id: &str) -> bool {
        self.device_id(kasa_id, &Role::Camera)
            .is_some_and(|id| self.published.contains_key(&(id, Ext::Motion.key())))
    }

    // ── Discovery ─────────────────────────────────────────────────────

    fn discovery_step(&mut self, now: u64) {
        if self.sweep.is_none() {
            let Some(due) = self.next_discovery_ms else {
                return;
            };
            if now < due {
                return;
            }
            if let Err(e) = self.net.udp_open(self.cfg.broadcast) {
                self.log(Level::Warn, &format!("discovery socket: {e}"));
                self.schedule_next_discovery(now);
                return;
            }
            let probe = discovery::probe();
            if self.cfg.broadcast {
                let to = SocketAddrV4::new(Ipv4Addr::BROADCAST, discovery::DISCOVERY_PORT);
                if let Err(e) = self.net.udp_send(to, &probe) {
                    // The unicast sweep below is the fallback; with none
                    // configured, discovery can't find anything.
                    if self.cfg.sweep.is_empty()
                        && !std::mem::replace(&mut self.warned_no_broadcast, true)
                    {
                        self.log(
                            Level::Warn,
                            &format!(
                                "discovery broadcast: {e}; set discovery_targets to find devices"
                            ),
                        );
                    } else {
                        self.log(Level::Debug, &format!("discovery broadcast: {e}"));
                    }
                }
            }
            self.sweep = Some(Sweep {
                targets: discovery::sweep_hosts(&self.cfg.sweep),
                next: 0,
                listen_until_ms: None,
            });
        }
        // Paced unicast: a back-to-back burst of 254 datagrams gets dropped.
        let probe = discovery::probe();
        let mut batch = Vec::new();
        if let Some(s) = &mut self.sweep {
            let end = (s.next + PROBES_PER_TICK).min(s.targets.len());
            batch.extend_from_slice(&s.targets[s.next..end]);
            s.next = end;
        }
        for (i, ip) in batch.iter().enumerate() {
            if i > 0 {
                self.net.sleep_ms(discovery::UNICAST_PACING_MS);
            }
            // Per-target errors (EHOSTDOWN from dead IPs) are uninteresting.
            let _ = self
                .net
                .udp_send(SocketAddrV4::new(*ip, discovery::DISCOVERY_PORT), &probe);
        }
        for _ in 0..MAX_REPLIES_PER_TICK {
            let Some((ip, data)) = self.net.udp_try_recv() else {
                break;
            };
            match discovery::parse_reply(ip, &data) {
                Some(d) => self.adopt(&d, true),
                None => self.log(
                    Level::Debug,
                    &format!("discovery: unparsable reply from {ip}"),
                ),
            }
        }
        let now = self.net.monotonic_ms();
        let done = match &mut self.sweep {
            Some(s) if s.next >= s.targets.len() => {
                let until = *s
                    .listen_until_ms
                    .get_or_insert(now + discovery::DISCOVERY_WINDOW_MS);
                now >= until
            }
            _ => false,
        };
        if done {
            self.sweep = None;
            self.net.udp_close();
            self.schedule_next_discovery(now);
        }
    }

    fn schedule_next_discovery(&mut self, now: u64) {
        self.next_discovery_ms =
            (self.cfg.discovery_interval_ms > 0).then(|| now + self.cfg.discovery_interval_ms);
    }

    // ── Tick ──────────────────────────────────────────────────────────

    /// One `tick`: finish accepted camera commands, advance discovery, run
    /// due polls, pump streams.
    pub fn tick(&mut self) {
        self.tick_started_ms = self.net.monotonic_ms();
        self.in_tick = true;
        if self.inventory_unread {
            self.restore();
        }
        self.run_queue();
        let now = self.net.monotonic_ms();
        self.discovery_step(now);

        let now = self.net.monotonic_ms();
        // Most overdue first, so devices skipped by the per-tick cap go
        // ahead of ones that were just polled.
        let mut due: Vec<(u64, String, KasaClass)> = self
            .devices
            .iter()
            .filter(|(_, p)| p.next_poll_ms <= now)
            .map(|(id, p)| (p.next_poll_ms, id.clone(), p.class))
            .collect();
        due.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let mut iot_polls = 0;
        for (_, id, class) in due {
            // Left due: picked up first (most overdue) next tick.
            if self.over_budget() {
                break;
            }
            if class == KasaClass::Camera {
                self.poll_camera(&id, now);
            } else if iot_polls < MAX_IOT_POLLS_PER_TICK {
                iot_polls += 1;
                self.poll_iot(&id, now);
            }
        }
        if self.cfg.camera_motion && self.cfg.camera_credentials().is_some() {
            let now = self.net.monotonic_ms();
            let cams: Vec<String> = self
                .devices
                .iter()
                .filter(|(_, p)| p.class == KasaClass::Camera)
                .map(|(id, _)| id.clone())
                .collect();
            for id in cams {
                let due = self.devices.get_mut(&id).map(|p| {
                    let change = p.cam.motion.settle(now);
                    (p.cam.next_motion_ms <= now, change.is_some())
                });
                if self.over_budget() {
                    break;
                }
                if let Some((due, changed)) = due {
                    // Re-offered every tick once published: the dedup
                    // cache only advances on success, so a refused idle
                    // event is retried instead of leaving motion stuck on.
                    if changed || self.motion_published(&id) {
                        self.publish_motion(&id);
                    }
                    if due {
                        self.poll_motion(&id, now);
                    }
                }
            }
        }
        self.pump_streams();
        self.flush();
        self.in_tick = false;
    }

    // ── Commands ──────────────────────────────────────────────────────

    /// `execute-command`.
    pub fn execute_command(&mut self, device: &str, cmd: &Command) -> CommandResult {
        let Some((kasa_id, role)) = self.routes.get(device).cloned() else {
            return CommandResult::Err(Error::NotFound(format!(
                "device {device} is not owned by this plugin"
            )));
        };
        match role {
            Role::Camera | Role::CameraChild(_) => match self.camera_op(&kasa_id, &role, cmd) {
                Ok(op) => {
                    if self.cfg.camera_credentials().is_none() {
                        return unavailable(CamError::NoCredentials.to_string());
                    }
                    self.queue.push_back(Pending {
                        command_id: cmd.id.clone(),
                        kasa_id,
                        device_id: device.to_string(),
                        op,
                    });
                    CommandResult::Accepted
                }
                Err(r) => r,
            },
            Role::Power => invalid(format!(
                "{}::{} is not supported on a power sensor",
                cmd.capability, cmd.action
            )),
            _ => self.iot_command(&kasa_id, &role, device, cmd),
        }
    }

    fn iot_command(
        &mut self,
        kasa_id: &str,
        role: &Role,
        device: &str,
        cmd: &Command,
    ) -> CommandResult {
        let Some(p) = self.devices.get(kasa_id) else {
            return unavailable("device is gone");
        };
        let Some(addr) = p.addr() else {
            return unavailable("device has no usable address");
        };
        if cmd.action == "refresh"
            && Ext::from_key(&cmd.capability) == Some(Ext::EnergyMeter)
            && p.features.emeter
        {
            let now = self.net.monotonic_ms();
            return match self.poll_energy(kasa_id, addr, true, now) {
                (true, fields) => {
                    // The host projects this into the snapshot even when
                    // the event was refused; keep the cache honest.
                    let key = (device.to_string(), Ext::EnergyMeter.key());
                    if self.published.get(&key) != Some(&format!("{fields:?}")) {
                        self.published.remove(&key);
                    }
                    CommandResult::OkWithState(fields)
                }
                (false, _) => unavailable("energy meter did not answer"),
            };
        }
        // Validate and build the request; nothing has been sent yet, so
        // every refusal here is a plain `err`.
        let (req, capability, fields) = match iot_request(p, role, cmd) {
            Ok(r) => r,
            Err(r) => return r,
        };
        let result = self
            .iot_call(addr, &req)
            .and_then(|v| iot::check_reply(&req, &v).map_err(CallError::Iot));
        match result {
            Ok(()) => {
                self.remember_iot_state(kasa_id, role, &capability, &fields);
                self.publish_confirmed(device, &capability, &fields);
                CommandResult::OkWithState(fields)
            }
            Err(CallError::Net(e)) if !e.maybe_sent() => unavailable(e.message().to_string()),
            Err(CallError::Net(e)) => {
                self.forget_switch_state(kasa_id, role, &capability);
                CommandResult::Unknown(e.message().to_string())
            }
            Err(CallError::Iot(e @ IotError::Device { code, .. })) => {
                if code == -3 {
                    invalid(e.to_string())
                } else {
                    unavailable(e.to_string())
                }
            }
            Err(CallError::Iot(e)) => {
                self.forget_switch_state(kasa_id, role, &capability);
                CommandResult::Unknown(format!("reply unusable: {e}"))
            }
        }
    }

    /// After a write that may or may not have landed, the cached on/off is
    /// no longer trustworthy: a toggle needs a fresh read first.
    fn forget_switch_state(&mut self, kasa_id: &str, role: &Role, capability: &str) {
        let Some(p) = self.devices.get_mut(kasa_id) else {
            return;
        };
        if capability != cap::SWITCH {
            return;
        }
        match role {
            Role::Outlet { .. } => p.observed = false,
            Role::Bulb { .. } => p.light = None,
            _ => p.relay_on = None,
        }
    }

    fn remember_iot_state(
        &mut self,
        kasa_id: &str,
        role: &Role,
        capability: &str,
        fields: &[KeyValue],
    ) {
        let Some(p) = self.devices.get_mut(kasa_id) else {
            return;
        };
        let on = fields
            .iter()
            .find_map(|kv| match (&kv.value, kv.key.as_str()) {
                (Value::BoolVal(b), "state") => Some(*b),
                _ => None,
            });
        if matches!(role, Role::Bulb { .. }) && capability != cap::SWITCH {
            // Without a read there is no known power state to keep the
            // brightness next to; inventing `on: false` would mislead a
            // later toggle.
            if let Some(light) = p.light.as_mut() {
                remember_light(light, fields);
            }
            return;
        }
        if capability != cap::SWITCH {
            return;
        }
        match role {
            Role::Outlet { child_id } => {
                if let (Some(c), Some(on)) = (p.children.iter_mut().find(|c| &c.id == child_id), on)
                {
                    c.on = on;
                }
            }
            Role::Bulb { .. } => {
                if let Some(on) = on {
                    p.light.get_or_insert_with(iot::LightState::default).on = on;
                }
            }
            _ => p.relay_on = on.or(p.relay_on),
        }
    }

    fn camera_op(
        &self,
        kasa_id: &str,
        role: &Role,
        cmd: &Command,
    ) -> Result<CameraOp, CommandResult> {
        let p = self
            .devices
            .get(kasa_id)
            .ok_or_else(|| unavailable("device is gone"))?;
        let a = &cmd.args;
        let ext = Ext::from_key(&cmd.capability);
        match (role, cmd.capability.as_str(), cmd.action.as_str(), ext) {
            (Role::CameraChild(f), cap::SWITCH, "toggle", _) => {
                if !p.cam.features.contains_key(f) {
                    return Err(unavailable("current state unknown; use switch::set"));
                }
                Ok(CameraOp::Toggle(*f))
            }
            (Role::CameraChild(f), cap::SWITCH, "set", _) => {
                let on = args::bool(a, "state")
                    .ok_or_else(|| invalid("switch::set requires `state: bool`"))?;
                // Privacy on = camera off.
                let value = if *f == OnOffFeature::CameraEnabled {
                    !on
                } else {
                    on
                };
                Ok(CameraOp::Set(*f, value))
            }
            (Role::Camera, _, "set", Some(Ext::StatusLed)) if p.features.led => {
                let on = args::bool(a, "on")
                    .ok_or_else(|| invalid("status-led::set requires `on: bool`"))?;
                Ok(CameraOp::Set(OnOffFeature::Led, on))
            }
            (Role::Camera, _, "reboot", Some(Ext::CameraControl)) => Ok(CameraOp::Reboot),
            (Role::Camera, _, "snapshot", Some(Ext::CameraControl)) => Ok(CameraOp::Snapshot),
            (Role::Camera, _, "move", Some(Ext::PanTilt)) if p.features.ptz => {
                let dir = args::string(a, "direction")
                    .and_then(PtzDirection::parse)
                    .ok_or_else(|| {
                        invalid("pan-tilt::move requires `direction`: left|right|top|bottom")
                    })?;
                if (args::has(a, "speed") && args::int(a, "speed").is_none())
                    || (args::has(a, "magnitude")
                        && !args::number(a, "magnitude").is_some_and(|m| (-1.0..=1.0).contains(&m)))
                {
                    return Err(invalid(
                        "`speed` must be an integer, `magnitude` a number in [-1, 1]",
                    ));
                }
                let speed = match (args::int(a, "speed"), args::number(a, "magnitude")) {
                    (Some(s), _) if (1..=10).contains(&s) => u8::try_from(s).unwrap_or(1),
                    (Some(_), _) => return Err(invalid("`speed` must be 1..=10")),
                    (None, Some(m)) => camera::ptz_speed(m),
                    (None, None) => 1,
                };
                Ok(CameraOp::PtzMove(dir, speed))
            }
            (Role::Camera, _, "goto-preset", Some(Ext::PanTilt)) if p.features.ptz => {
                let index = args::int(a, "index")
                    .filter(|i| *i >= 1)
                    .ok_or_else(|| invalid("pan-tilt::goto-preset requires `index` >= 1"))?;
                Ok(CameraOp::PtzPreset(index))
            }
            (_, c, act, _) => Err(invalid(format!(
                "unsupported command {c}::{act} for this device"
            ))),
        }
    }

    /// Whether this tick has used its [`TICK_BUDGET_MS`].
    fn over_budget(&self) -> bool {
        self.in_tick
            && self.net.monotonic_ms().saturating_sub(self.tick_started_ms) >= TICK_BUDGET_MS
    }

    /// Runs queued camera commands in order, as far as the tick budget
    /// allows; the rest wait for the next tick.
    fn run_queue(&mut self) {
        while !self.over_budget()
            && let Some(job) = self.queue.pop_front()
        {
            let result = self.run_camera_op(&job);
            if let Err(e) = self.host.complete_command(&job.command_id, result) {
                // Expired on the host. The state it carried was published
                // as an event already.
                self.log(
                    Level::Warn,
                    &format!("complete-command {} failed: {e:?}", job.command_id),
                );
            }
        }
    }

    fn run_camera_op(&mut self, job: &Pending) -> CommandResult {
        let kasa_id = job.kasa_id.as_str();
        let op = match &job.op {
            // Flipping the feature's own value flips the switch either way
            // (privacy is the inverse of camera-enabled).
            CameraOp::Toggle(f) => match self
                .devices
                .get(kasa_id)
                .and_then(|p| p.cam.features.get(f).copied())
            {
                Some(v) => CameraOp::Set(*f, !v),
                None => return unavailable("current state unknown; use switch::set"),
            },
            op => op.clone(),
        };
        let outcome = match &op {
            CameraOp::Set(f, on) => self
                .linkie(kasa_id, &camera::set_state(*f, *on))
                .and_then(|v| camera::check_set(&v, *f).map_err(CamError::Linkie))
                .map(|()| {
                    if let Some(p) = self.devices.get_mut(kasa_id) {
                        p.cam.features.insert(*f, *on);
                    }
                    let (capability, fields) = match f {
                        OnOffFeature::Led => (Ext::StatusLed.key(), led_fields(*on)),
                        OnOffFeature::CameraEnabled => (cap::SWITCH.into(), switch_fields(!*on)),
                        _ => (cap::SWITCH.into(), switch_fields(*on)),
                    };
                    self.publish_confirmed(&job.device_id, &capability, &fields);
                    CommandResult::OkWithState(fields)
                }),
            CameraOp::Reboot => self
                .linkie(kasa_id, &camera::reboot())
                .and_then(|v| camera::check_reboot(&v).map_err(CamError::Linkie))
                .map(|()| CommandResult::Ok),
            CameraOp::PtzMove(d, s) => self
                .linkie(kasa_id, &camera::ptz_move(*d, *s))
                .and_then(|v| camera::check_ptz(&v, "set_move").map_err(CamError::Linkie))
                .map(|()| CommandResult::Ok),
            CameraOp::PtzPreset(i) => self
                .linkie(kasa_id, &camera::ptz_goto_preset(*i))
                .and_then(|v| camera::check_ptz(&v, "set_run_to_preset").map_err(CamError::Linkie))
                .map(|()| CommandResult::Ok),
            CameraOp::Snapshot => return self.snapshot(kasa_id),
            CameraOp::Toggle(_) => unreachable!("resolved above"),
        };
        match outcome {
            Ok(r) => r,
            // A server error after a write doesn't say whether it landed.
            Err(CamError::Linkie(LinkieError::Http(s))) if s >= 500 => {
                self.forget_camera_feature(kasa_id, &op);
                CommandResult::Unknown(format!("camera answered HTTP {s}"))
            }
            Err(e) => {
                let r = cam_error_result(&e);
                if matches!(r, CommandResult::Unknown(_)) {
                    self.forget_camera_feature(kasa_id, &op);
                }
                r
            }
        }
    }

    /// The camera-side twin of [`Self::forget_switch_state`].
    fn forget_camera_feature(&mut self, kasa_id: &str, op: &CameraOp) {
        if let (CameraOp::Set(f, _), Some(p)) = (op, self.devices.get_mut(kasa_id)) {
            p.cam.features.remove(f);
        }
    }

    fn snapshot(&mut self, kasa_id: &str) -> CommandResult {
        let Some((user, pass)) = self
            .cfg
            .camera_credentials()
            .map(|(u, p)| (u.to_string(), p.to_string()))
        else {
            return unavailable(CamError::NoCredentials.to_string());
        };
        let Some(host) = self.devices.get(kasa_id).and_then(Physical::host) else {
            return unavailable("device is gone");
        };
        let req = camera::snapshot_request(&host, camera::STREAM_PORT, &stream_auth(&user, &pass));
        let resp = match self.net.https(&req, camera::SNAPSHOT_TIMEOUT_MS) {
            Ok(r) => r,
            // A read: nothing changed on the camera, so a retry is safe.
            Err(e) => return unavailable(e.message().to_string()),
        };
        let jpeg = match camera::parse_snapshot(resp) {
            Ok(j) => j,
            Err(camera::SnapshotError::Http(401 | 403)) => {
                return CommandResult::Err(Error::PermissionDenied(
                    "the camera rejected the Kasa credentials".into(),
                ));
            }
            Err(e) => return unavailable(e.to_string()),
        };
        let size = i64::try_from(jpeg.len()).unwrap_or(i64::MAX);
        let name = format!("snapshot-{}.jpg", media::sink_name(kasa_id));
        match self.host.blob_write(&name, jpeg, "image/jpeg") {
            Ok(id) => CommandResult::OkWithState(vec![
                KeyValue {
                    key: "snapshot_blob".into(),
                    value: Value::StringVal(name),
                },
                KeyValue {
                    key: "snapshot_blob_id".into(),
                    value: Value::StringVal(id),
                },
                KeyValue {
                    key: "snapshot_bytes".into(),
                    value: Value::IntVal(size),
                },
            ]),
            Err(e) => CommandResult::Err(e),
        }
    }

    // ── Streams ───────────────────────────────────────────────────────

    /// `setup-pipeline`: describes the camera's pipeline and opens the
    /// stream feeding it.
    ///
    /// # Errors
    ///
    /// A message for the host.
    pub fn setup_pipeline(&mut self, device: &str) -> Result<MediaPipeline, String> {
        let Some((kasa_id, Role::Camera)) = self.routes.get(device).cloned() else {
            return Err(format!("{device} is not a Kasa camera of this plugin"));
        };
        if !self.streams.contains_key(&kasa_id) {
            self.start_stream(&kasa_id)?;
        }
        Ok(media::camera_pipeline(&kasa_id))
    }

    fn start_stream(&mut self, kasa_id: &str) -> Result<(), String> {
        let (user, pass) = self
            .cfg
            .camera_credentials()
            .map(|(u, p)| (u.to_string(), p.to_string()))
            .ok_or_else(|| CamError::NoCredentials.to_string())?;
        let host = self
            .devices
            .get(kasa_id)
            .and_then(Physical::host)
            .ok_or("device is gone or its address is stale")?;
        let req = multipart::stream_request(&host, camera::STREAM_PORT, &stream_auth(&user, &pass));
        let (head, body) = self
            .net
            .https_stream(&req, multipart::STREAM_CONNECT_TIMEOUT_MS)
            .map_err(|e| e.to_string())?;
        let boundary = multipart::boundary_from_response(&head).map_err(|e| e.to_string())?;
        let video = self
            .host
            .open_pipe(media::video_pipe_config())
            .map_err(|e| format!("open-pipe: {e:?}"))?;
        let mut video = video;
        let audio = match self.host.open_pipe(media::audio_pipe_config()) {
            Ok(a) => a,
            Err(e) => {
                video.close();
                return Err(format!("open-pipe: {e:?}"));
            }
        };
        let mut seeds = [0u8; 16];
        self.net.random(&mut seeds);
        let word =
            |i: usize| u32::from_le_bytes([seeds[i], seeds[i + 1], seeds[i + 2], seeds[i + 3]]);
        let session = StreamSession::new(
            &boundary,
            body,
            video,
            audio,
            self.net.monotonic_ms(),
            [word(0), word(4), word(8), word(12)],
        );
        self.streams.insert(kasa_id.to_string(), session);
        Ok(())
    }

    fn pump_streams(&mut self) {
        let now = self.net.monotonic_ms();
        let mut ended = Vec::new();
        for (id, s) in &mut self.streams {
            if let Err(e) = s.pump(now) {
                ended.push((id.clone(), e));
            }
        }
        for (id, e) in ended {
            if let Some(mut s) = self.streams.remove(&id) {
                s.close();
            }
            self.log(Level::Warn, &format!("{id}: live stream ended: {e}"));
        }
    }

    /// Whether a camera's stream is running (tests).
    #[must_use]
    pub fn streaming(&self, kasa_id: &str) -> bool {
        self.streams.contains_key(kasa_id)
    }
}

fn cam_error_result(e: &CamError) -> CommandResult {
    match e {
        CamError::NoCredentials | CamError::NoAddress => unavailable(e.to_string()),
        CamError::Net(n) if !n.maybe_sent() => unavailable(n.message().to_string()),
        CamError::Net(n) => CommandResult::Unknown(n.message().to_string()),
        CamError::Linkie(LinkieError::Http(401 | 403)) => CommandResult::Err(
            Error::PermissionDenied("the camera rejected the Kasa credentials".into()),
        ),
        CamError::Linkie(LinkieError::Http(s)) => unavailable(format!("camera answered HTTP {s}")),
        CamError::Linkie(l @ LinkieError::Device { code, .. }) => match *code {
            Some(err::CAMERA_OFF) => unavailable("the camera is turned off (privacy mode)"),
            Some(err::PTZ_ARGUMENT) => {
                invalid(format!("{l}: bad argument or the axis is at its end stop"))
            }
            Some(err::UNSUPPORTED) => invalid(format!("{l}: not supported by this camera")),
            // No err_code at all: the camera answered but didn't confirm.
            None => CommandResult::Unknown(l.to_string()),
            Some(_) => unavailable(l.to_string()),
        },
        CamError::Linkie(l) => CommandResult::Unknown(format!("reply unusable: {l}")),
    }
}

/// A smart-home command as request + the state it sets.
type IotPlan = (Request, String, Vec<KeyValue>);

/// Validates a smart-home command and builds its request.
fn iot_request(p: &Physical, role: &Role, cmd: &Command) -> Result<IotPlan, CommandResult> {
    let args = &cmd.args;
    let is_bulb = matches!(role, Role::Bulb { .. });
    match (
        cmd.capability.as_str(),
        cmd.action.as_str(),
        Ext::from_key(&cmd.capability),
    ) {
        (cap::SWITCH, action @ ("set" | "toggle"), _)
            if matches!(
                role,
                Role::Relay | Role::Dimmer | Role::Bulb { .. } | Role::Outlet { .. }
            ) =>
        {
            if let Role::Outlet { child_id } = role
                && !p.children.iter().any(|c| &c.id == child_id)
            {
                return Err(unavailable("the strip no longer reports this outlet"));
            }
            let current = match role {
                Role::Outlet { child_id } if p.observed => {
                    p.children.iter().find(|c| &c.id == child_id).map(|c| c.on)
                }
                Role::Outlet { .. } => None,
                Role::Bulb { .. } => p.light.map(|l| l.on),
                _ => p.relay_on,
            };
            let on = if action == "toggle" {
                !current.ok_or_else(|| unavailable("current state unknown; use switch::set"))?
            } else {
                args::bool(args, "state")
                    .ok_or_else(|| invalid("switch::set requires `state: bool`"))?
            };
            let req = match role {
                Role::Outlet { child_id } => iot::set_relay_state(on, Some(child_id)),
                _ if is_bulb => iot::transition_light_state(&LightChange {
                    on: Some(on),
                    ..LightChange::default()
                }),
                _ => iot::set_relay_state(on, None),
            };
            Ok((req, cap::SWITCH.into(), switch_fields(on)))
        }
        (cap::DIMMER, "set", _) if matches!(role, Role::Dimmer | Role::Bulb { .. }) => {
            let level = args::number(args, "level")
                .filter(|l| *l > 0.0 && *l <= 1.0)
                .ok_or_else(|| {
                    invalid("dimmer::set requires `level` in (0, 1]; turn off with switch::set")
                })?;
            // Kasa brightness is 1–100; a tiny positive level is 1 %.
            let pct = level_to_percent(level).max(1);
            let req = if is_bulb {
                iot::transition_light_state(&LightChange {
                    brightness: Some(pct),
                    ..LightChange::default()
                })
            } else {
                iot::set_brightness(pct)
            };
            Ok((req, cap::DIMMER.into(), dimmer_fields(pct)))
        }
        (cap::COLOR_LIGHT, "set", _) => match *role {
            Role::Bulb { color, temp } if color || temp => color_request(p, color, temp, args),
            _ => Err(invalid("color-light::set is not supported on this device")),
        },
        (_, "set", Some(Ext::StatusLed))
            if p.features.led && matches!(role, Role::Relay | Role::Dimmer | Role::Strip) =>
        {
            let on = args::bool(args, "on")
                .ok_or_else(|| invalid("status-led::set requires `on: bool`"))?;
            Ok((iot::set_led_off(!on), Ext::StatusLed.key(), led_fields(on)))
        }
        (c, act, _) => Err(invalid(format!(
            "unsupported command {c}::{act} for this device"
        ))),
    }
}

/// `color-light::set`: either `color_temp_kelvin`, or `hue` + `saturation`
/// (+ optional `value`).
fn color_request(
    p: &Physical,
    color: bool,
    temp: bool,
    args: &[KeyValue],
) -> Result<IotPlan, CommandResult> {
    let (kmin, kmax) = mapping::BULB_KELVIN_RANGE;
    let prev = p.light.unwrap_or_default();
    if args::has(args, "color_temp_kelvin") {
        if !temp {
            return Err(invalid("this bulb has no color temperature"));
        }
        let kelvin = args::int(args, "color_temp_kelvin")
            .and_then(|k| u32::try_from(k).ok())
            .filter(|k| (kmin..=kmax).contains(k))
            .ok_or_else(|| {
                invalid(format!(
                    "color_temp_kelvin must be an int in {kmin}..={kmax}"
                ))
            })?;
        let change = LightChange {
            color_temp: Some(kelvin),
            ..LightChange::default()
        };
        let fields = color_fields(
            prev.hue.unwrap_or(0),
            prev.saturation.unwrap_or(0),
            prev.brightness.unwrap_or(100),
            Some(kelvin),
        );
        return Ok((
            iot::transition_light_state(&change),
            cap::COLOR_LIGHT.into(),
            fields,
        ));
    }
    if !color {
        return Err(invalid("this bulb has no HSV color"));
    }
    let unit = |key: &str| args::number(args, key).filter(|x| (0.0..=1.0).contains(x));
    let (Some(hue), Some(sat)) = (unit("hue"), unit("saturation")) else {
        return Err(invalid(
            "color-light::set requires `hue` and `saturation` in [0, 1] (or `color_temp_kelvin`)",
        ));
    };
    let value = match args::number(args, "value") {
        None if args::has(args, "value") => return Err(invalid("`value` must be a number")),
        None => prev.brightness.map_or(1.0, |b| f64::from(b) / 100.0),
        Some(x) if (0.0..=1.0).contains(&x) => x,
        Some(_) => return Err(invalid("`value` must be in [0, 1]")),
    };
    let hue = u16::try_from(unit_to(hue, 360)).unwrap_or(360);
    let (sat, bri) = (level_to_percent(sat), level_to_percent(value));
    Ok((
        iot::transition_light_state(&LightChange::hsv(hue, sat, bri)),
        cap::COLOR_LIGHT.into(),
        color_fields(hue, sat, bri, temp.then_some(0)),
    ))
}

/// Folds a confirmed `dimmer` or `color-light` result into the cached bulb
/// state, so the next HSV command without `value` keeps the new brightness.
fn remember_light(light: &mut iot::LightState, fields: &[KeyValue]) {
    let pct = |x: f64, max: f64| (x.clamp(0.0, 1.0) * max).round();
    for kv in fields {
        // Values are clamped to 0..=360 / 0..=100 above.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        match (kv.key.as_str(), &kv.value) {
            ("level" | "value", Value::FloatVal(x)) => {
                light.brightness = Some(pct(*x, 100.0) as u8);
            }
            ("hue", Value::FloatVal(x)) => light.hue = Some(pct(*x, 360.0) as u16),
            ("saturation", Value::FloatVal(x)) => light.saturation = Some(pct(*x, 100.0) as u8),
            ("color_temp_kelvin", Value::IntVal(k)) => light.color_temp = u32::try_from(*k).ok(),
            _ => {}
        }
    }
}

/// A MAC without separators, uppercased, for comparing spellings.
fn norm_mac(mac: &str) -> String {
    mac.chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// One adopted device as stored: the sysinfo fields that rebuild it.
fn record(id: &str, p: &Physical) -> Json {
    let mut sys = json!({
        "deviceId": id,
        "alias": clip(&p.ident.name),
        "is_color": u8::from(p.color),
        "is_variable_color_temp": u8::from(p.temp),
        "children": p.children.iter()
            .map(|c| json!({"id": c.id, "alias": clip(&c.alias)}))
            .collect::<Vec<_>>(),
    });
    for (key, value) in [
        ("model", &p.ident.model),
        ("sw_ver", &p.ident.firmware),
        ("mac", &p.ident.mac),
    ] {
        if let Some(v) = value {
            sys[key] = json!(clip(v));
        }
    }
    if p.features.emeter {
        sys["feature"] = json!("ENE");
    }
    if p.features.led {
        sys["led_off"] = json!(0);
    }
    let mut rec =
        json!({ "id": id, "class": p.class.as_str(), "address": p.ident.address, "sysinfo": sys });
    // Survives a restart: the address must not become writable again
    // until discovery finds the device.
    if p.address_stale {
        rec["address_stale"] = json!(true);
    }
    rec
}

fn parse_record(e: &Json) -> Option<Discovered> {
    Some(Discovered {
        address: e.get("address")?.as_str()?.parse().ok()?,
        id: e.get("id")?.as_str()?.to_string(),
        class: discovery::KasaClass::parse(e.get("class")?.as_str()?),
        sysinfo: SysInfo::parse(e.get("sysinfo")?),
    })
}

/// `s` cut to [`MAX_STORED_NAME`] characters.
fn clip(s: &str) -> &str {
    s.char_indices()
        .nth(MAX_STORED_NAME)
        .map_or(s, |(i, _)| &s[..i])
}

/// Outlets as reported, minus malformed ones, capped at [`MAX_OUTLETS`].
fn bounded_children(children: &[ChildOutlet]) -> Vec<ChildOutlet> {
    children
        .iter()
        .filter(|c| !c.id.is_empty() && c.id.len() <= MAX_ID_LEN)
        .take(MAX_OUTLETS)
        .map(|c| ChildOutlet {
            // Device-supplied text is bounded before it is kept.
            alias: clip(&c.alias).to_string(),
            ..c.clone()
        })
        .collect()
}

/// Energy metering is modeled for single-relay devices. Strips (HS300)
/// meter per outlet, which needs `context.child_ids` on the `emeter` calls
/// (README, TODO).
fn emeter_capable(class: KasaClass) -> bool {
    matches!(
        class,
        KasaClass::Plug | KasaClass::Switch | KasaClass::Dimmer
    )
}

/// `level` in [0, 1] → 0–100, rounded.
fn level_to_percent(level: f64) -> u8 {
    u8::try_from(unit_to(level, 100)).unwrap_or(100)
}

/// `v` in [0, 1] → 0..=max, rounded.
fn unit_to(v: f64, max: u32) -> u32 {
    let scaled = (v.clamp(0.0, 1.0) * f64::from(max)).round();
    // Clamped to 0..=max above, so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let r = scaled as u32;
    r.min(max)
}
