//! The two seams between the plugin logic and the outside world, so the
//! logic runs natively in tests with fakes:
//!
//! - [`Host`]: the `OxidHome` host imports the plugin uses (devices,
//!   events, config, storage, blob-store, logging, media).
//! - [`Net`]: sockets, HTTPS, clocks and randomness. The real
//!   implementation ([`crate::wasm::WasiNet`]) runs TCP over
//!   `wasi:sockets`, discovery over the host's discovery socket, and
//!   camera HTTPS over `wasi:http`, in the `streaming-plugin` world.
//!
//! Errors: [`Host`] methods return the host's `error` unchanged; [`Net`]
//! methods return [`NetError`], split by whether anything was sent.
#![allow(clippy::missing_errors_doc)]

use std::net::{Ipv4Addr, SocketAddrV4};

use crate::media::PluginPipeConfig;
use crate::protocol::http::{HttpRequest, HttpResponse};
use oxidhome_sdk::bindings::oxidhome::plugin::devices::{CommandResult, DeviceInfo};
use oxidhome_sdk::bindings::oxidhome::plugin::logging::Level;
use oxidhome_sdk::bindings::oxidhome::plugin::types::{Error, KeyValue, Value};
pub use oxidhome_sdk::streaming::discovery::LocalNetwork;

/// The host surface the plugin uses.
pub trait Host {
    /// `host-devices::register-device`.
    fn register_device(&mut self, info: DeviceInfo) -> Result<String, Error>;
    /// `host-devices::update-device`.
    fn update_device(&mut self, id: &str, info: DeviceInfo) -> Result<(), Error>;
    /// `device-registry::remove-device`.
    fn remove_device(&mut self, id: &str) -> Result<(), Error>;
    /// `host-events::publish-event` with a `state-changed` payload.
    fn publish_state(
        &mut self,
        device: &str,
        capability: &str,
        fields: Vec<KeyValue>,
        unix_ms: u64,
    ) -> Result<(), Error>;
    /// `host-devices::complete-command`.
    fn complete_command(&mut self, id: &str, result: CommandResult) -> Result<(), Error>;
    /// `host-config::get-config`.
    fn config(&mut self, key: &str) -> Option<Value>;
    /// `storage::get`.
    fn storage_get(&mut self, key: &str) -> Result<Option<Value>, Error>;
    /// `storage::set`.
    fn storage_set(&mut self, key: &str, value: Value) -> Result<(), Error>;
    /// `storage::delete`.
    fn storage_delete(&mut self, key: &str) -> Result<(), Error>;
    /// `blob-store::write`; returns the blob id.
    fn blob_write(&mut self, name: &str, data: Vec<u8>, mime: &str) -> Result<String, Error>;
    /// `logging::log`.
    fn log(&mut self, level: Level, message: &str);
    /// `host-media::open-pipe`.
    fn open_pipe(&mut self, config: PluginPipeConfig) -> Result<Box<dyn PipeSink>, Error>;
}

/// A `pipe-writer`.
pub trait PipeSink {
    /// Writes one chunk (one RTP packet for `pipe-format::rtp`).
    fn write(&mut self, data: &[u8]) -> Result<(), Error>;
    /// Signals end of stream.
    fn close(&mut self);
}

/// A transport failure, split by whether the request may have reached the
/// device. That split is what decides the H5 outcome: nothing sent →
/// `err` (safe to retry), possibly sent → `unknown`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetError {
    /// Failed before anything was written (connect refused, unreachable,
    /// TLS handshake, DNS, refused by the host's network rules, or not
    /// supported by this host yet).
    NotSent(String),
    /// Failed after the request went out (timeout, reset, short reply).
    Sent(String),
}

impl NetError {
    /// The request may have reached the device.
    #[must_use]
    pub fn maybe_sent(&self) -> bool {
        matches!(self, NetError::Sent(_))
    }

    /// The message.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            NetError::NotSent(m) | NetError::Sent(m) => m,
        }
    }
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::NotSent(m) => write!(f, "not sent: {m}"),
            NetError::Sent(m) => write!(f, "sent, no reply: {m}"),
        }
    }
}

/// A long-lived response body (the live stream).
pub trait ByteStream {
    /// Bytes available now, without blocking; `Ok(empty)` when none are
    /// ready yet.
    fn read_available(&mut self, max: usize) -> Result<Vec<u8>, NetError>;
}

/// Network, time and randomness.
pub trait Net {
    /// One smart-home exchange on TCP: connect, write `frame`, read one
    /// length-prefixed reply frame (raw, prefix included), close.
    fn tcp_exchange(
        &mut self,
        addr: SocketAddrV4,
        frame: &[u8],
        timeout_ms: u64,
    ) -> Result<Vec<u8>, NetError>;
    /// Opens the discovery socket (bound to an ephemeral port). It may
    /// send to `255.255.255.255` and to subnet broadcasts, as far as the
    /// granted network rules allow.
    fn udp_open(&mut self) -> Result<(), NetError>;
    /// Sends one datagram on the discovery socket.
    fn udp_send(&mut self, to: SocketAddrV4, data: &[u8]) -> Result<(), NetError>;
    /// The host's interface networks that the granted rules cover whole
    /// (`host-network::local-networks`); their subnet broadcasts reach
    /// devices on networks where the limited broadcast doesn't.
    fn local_networks(&mut self) -> Vec<LocalNetwork>;
    /// One received datagram, without blocking.
    fn udp_try_recv(&mut self) -> Option<(Ipv4Addr, Vec<u8>)>;
    /// Closes the discovery socket.
    fn udp_close(&mut self);
    /// One buffered HTTPS exchange with a camera. Cameras have
    /// self-signed certificates: the host trusts each on first use and
    /// pins it per address and port.
    fn https(&mut self, req: &HttpRequest, timeout_ms: u64) -> Result<HttpResponse, NetError>;
    /// Opens a streaming HTTPS response: status and headers now, body via
    /// the returned stream.
    fn https_stream(
        &mut self,
        req: &HttpRequest,
        timeout_ms: u64,
    ) -> Result<(HttpResponse, Box<dyn ByteStream>), NetError>;
    /// Monotonic milliseconds.
    fn monotonic_ms(&self) -> u64;
    /// Wall-clock Unix milliseconds.
    fn unix_ms(&self) -> u64;
    /// Sleeps (used for the 3 ms discovery pacing only).
    fn sleep_ms(&mut self, ms: u64);
    /// Fills `buf` with random bytes (UUIDs, player ids, jitter, RTP
    /// seeds; nothing security-relevant).
    fn random(&mut self, buf: &mut [u8]);
}
