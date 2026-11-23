//! The component: the `plugin` world exports (through the SDK), the host
//! imports behind [`Host`], and a [`Net`] over WASI.
//!
//! The plugin needs the `streaming-plugin` world (sockets, HTTPS,
//! `host-media`), but the SDK only has bindings for `plugin`, and the
//! host doesn't load `streaming-plugin` yet (Phase 8). Until then:
//!
//! - TCP and UDP go through `std::net`, which wasi-libc implements on
//!   `wasi:sockets`. The host links all of WASI p2, so the component
//!   instantiates, but it grants no network, so the calls fail cleanly
//!   (`err(unavailable)`, a logged discovery warning).
//! - HTTPS (cameras) and `host-media` pipes answer "unavailable": the
//!   `plugin` world has no `wasi:http` or `host-media` import.
//!
//! The import calls only work inside a component; natively this module
//! compiles (so clippy checks it) but is never called.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use oxidhome_sdk::bindings::oxidhome::plugin::devices::{Command, CommandResult, DeviceInfo};
use oxidhome_sdk::bindings::oxidhome::plugin::events::{Event, EventPayload, StateChange};
use oxidhome_sdk::bindings::oxidhome::plugin::logging::{self, Level};
use oxidhome_sdk::bindings::oxidhome::plugin::types::{Error, KeyValue, Value};
use oxidhome_sdk::{Plugin, host};

use crate::kasa::Kasa;
use crate::media::PluginPipeConfig;
use crate::platform::{ByteStream, Host, Net, NetError, PipeSink};
use crate::protocol::http::{HttpRequest, HttpResponse};
use crate::protocol::iot::{FrameDecoder, MAX_FRAME_BYTES};

/// Why camera HTTPS and media pipes can't work in this build.
const NEEDS_STREAMING_WORLD: &str = "needs the streaming-plugin world (wasi:http, host-media), \
     which the SDK and host don't offer yet (OxidHome Phase 8)";

// ── Host ─────────────────────────────────────────────────────────────────

/// The real host imports.
pub struct WasiHost;

impl Host for WasiHost {
    fn register_device(&mut self, info: DeviceInfo) -> Result<String, Error> {
        host::register_device(info)
    }
    fn update_device(&mut self, id: &str, info: DeviceInfo) -> Result<(), Error> {
        host::update_device(&id.to_string(), &info)
    }
    fn remove_device(&mut self, id: &str) -> Result<(), Error> {
        host::remove_device(&id.to_string())
    }
    fn publish_state(
        &mut self,
        device: &str,
        capability: &str,
        fields: Vec<KeyValue>,
        unix_ms: u64,
    ) -> Result<(), Error> {
        host::publish_event(&Event {
            device: Some(device.to_string()),
            timestamp: unix_ms,
            // Host-populated on publish.
            origin_plugin_id: String::new(),
            origin_instance_id: String::new(),
            row_id: None,
            payload: EventPayload::StateChanged(StateChange {
                capability: capability.to_string(),
                fields,
            }),
        })
    }
    fn complete_command(&mut self, id: &str, result: CommandResult) -> Result<(), Error> {
        host::complete_command(id, &result)
    }
    fn config(&mut self, key: &str) -> Option<Value> {
        host::config::get(key).ok().flatten()
    }
    fn storage_get(&mut self, key: &str) -> Result<Option<Value>, Error> {
        host::storage::get(key)
    }
    fn storage_set(&mut self, key: &str, value: Value) -> Result<(), Error> {
        host::storage::set(key, &value)
    }
    fn storage_delete(&mut self, key: &str) -> Result<(), Error> {
        host::storage::delete(key)
    }
    fn blob_write(&mut self, name: &str, data: Vec<u8>, mime: &str) -> Result<String, Error> {
        host::blobs::write(name, &data, Some(mime))
    }
    fn log(&mut self, level: Level, message: &str) {
        logging::log(level, message);
    }
    fn open_pipe(&mut self, _config: PluginPipeConfig) -> Result<Box<dyn PipeSink>, Error> {
        Err(Error::Unavailable(format!(
            "host-media {NEEDS_STREAMING_WORLD}"
        )))
    }
}

// ── Net ──────────────────────────────────────────────────────────────────

/// The real network.
pub struct WasiNet {
    udp: Option<UdpSocket>,
    start: Instant,
    rng: u64,
}

impl Default for WasiNet {
    fn default() -> Self {
        Self {
            udp: None,
            start: Instant::now(),
            rng: 0,
        }
    }
}

fn not_sent(e: impl std::fmt::Display) -> NetError {
    NetError::NotSent(e.to_string())
}

fn sent(e: impl std::fmt::Display) -> NetError {
    NetError::Sent(e.to_string())
}

fn ms(d: u64) -> Duration {
    Duration::from_millis(d)
}

impl WasiNet {
    fn next_random(&mut self) -> u64 {
        if self.rng == 0 {
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            // Truncation is fine for a seed.
            #[allow(clippy::cast_possible_truncation)]
            let seed = (t as u64) ^ (self.start.elapsed().as_nanos() as u64);
            self.rng = seed | 1;
        }
        // xorshift64*: ids and jitter only, nothing security-relevant.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

impl Net for WasiNet {
    fn tcp_exchange(
        &mut self,
        addr: SocketAddrV4,
        frame: &[u8],
        timeout_ms: u64,
    ) -> Result<Vec<u8>, NetError> {
        // One deadline across connect, write and read (as the TS client):
        // a reply trickling in must not stretch an inline command.
        let deadline = Instant::now() + ms(timeout_ms);
        let left = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
        };
        let mut s =
            TcpStream::connect_timeout(&SocketAddr::V4(addr), ms(timeout_ms)).map_err(not_sent)?;
        // Track what the socket accepted: an error before the first byte
        // left means nothing reached the device.
        let mut written = 0;
        let fail = |written: usize, e: &dyn std::fmt::Display| {
            if written == 0 { not_sent(e) } else { sent(e) }
        };
        while written < frame.len() {
            let Some(remaining) = left() else {
                return Err(fail(written, &"timed out while sending"));
            };
            s.set_write_timeout(Some(remaining))
                .map_err(|e| fail(written, &e))?;
            match s.write(&frame[written..]) {
                Ok(0) => return Err(fail(written, &"connection closed while sending")),
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(fail(written, &e)),
            }
        }
        let mut dec = FrameDecoder::new();
        let mut raw = Vec::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let Some(remaining) = left() else {
                return Err(sent("timed out waiting for the reply"));
            };
            s.set_read_timeout(Some(remaining)).map_err(sent)?;
            let n = s.read(&mut buf).map_err(sent)?;
            if n == 0 {
                return Err(sent("connection closed before the full reply"));
            }
            raw.extend_from_slice(&buf[..n]);
            dec.push(&buf[..n]);
            // `missing` counts from the declared length, so an oversized
            // prefix is refused as soon as the first four bytes are in.
            match dec.missing() {
                Some(0) => return Ok(raw),
                Some(m) if raw.len() >= 4 && raw.len() - 4 + m > MAX_FRAME_BYTES => {
                    return Err(sent("reply frame too large"));
                }
                _ => {}
            }
        }
    }

    fn udp_open(&mut self, broadcast: bool) -> Result<(), NetError> {
        let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(not_sent)?;
        if broadcast {
            // wasi:sockets has no SO_BROADCAST; tolerate its absence.
            let _ = s.set_broadcast(true);
        }
        s.set_nonblocking(true).map_err(not_sent)?;
        self.udp = Some(s);
        Ok(())
    }

    fn udp_send(&mut self, to: SocketAddrV4, data: &[u8]) -> Result<(), NetError> {
        let s = self.udp.as_ref().ok_or_else(|| not_sent("socket closed"))?;
        s.send_to(data, to).map(|_| ()).map_err(not_sent)
    }

    fn udp_try_recv(&mut self) -> Option<(Ipv4Addr, Vec<u8>)> {
        let s = self.udp.as_ref()?;
        let mut buf = vec![0u8; 64 * 1024];
        match s.recv_from(&mut buf) {
            Ok((n, SocketAddr::V4(from))) => {
                buf.truncate(n);
                Some((*from.ip(), buf))
            }
            _ => None,
        }
    }

    fn udp_close(&mut self) {
        self.udp = None;
    }

    fn https(&mut self, _req: &HttpRequest, _timeout_ms: u64) -> Result<HttpResponse, NetError> {
        Err(not_sent(format!("camera HTTPS {NEEDS_STREAMING_WORLD}")))
    }

    fn https_stream(
        &mut self,
        _req: &HttpRequest,
        _timeout_ms: u64,
    ) -> Result<(HttpResponse, Box<dyn ByteStream>), NetError> {
        Err(not_sent(format!("camera stream {NEEDS_STREAMING_WORLD}")))
    }

    fn monotonic_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    fn sleep_ms(&mut self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    fn random(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let r = self.next_random().to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
    }
}

// ── Exports ──────────────────────────────────────────────────────────────

/// The exported plugin.
pub struct KasaPlugin(Kasa<WasiHost, WasiNet>);

impl Default for KasaPlugin {
    fn default() -> Self {
        Self(Kasa::new(WasiHost, WasiNet::default()))
    }
}

impl Plugin for KasaPlugin {
    fn init(&mut self) -> Result<(), String> {
        self.0.init();
        Ok(())
    }

    fn shutdown(&mut self) {
        self.0.shutdown();
    }

    fn execute_command(&mut self, device: String, cmd: Command) -> CommandResult {
        self.0.execute_command(&device, &cmd)
    }

    fn tick(&mut self) {
        self.0.tick();
    }
}

#[cfg(target_family = "wasm")]
oxidhome_sdk::plugin!(KasaPlugin);
