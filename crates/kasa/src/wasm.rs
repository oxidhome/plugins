//! The component: the `streaming-plugin` world exports (through the SDK),
//! the host imports behind [`Host`], and a [`Net`] over WASI.
//!
//! - TCP 9999 to plugs, switches, dimmers, bulbs and strips through
//!   `std::net` on `wasi:sockets`.
//! - Discovery through the host's discovery socket
//!   (`oxidhome_sdk::streaming::discovery`), which may send the
//!   `255.255.255.255` probe and subnet broadcasts; it stays open for the
//!   listening window, so replies wake the plugin early.
//! - Camera HTTPS (LINKIE2 on 10443, snapshots and the live stream on
//!   19443) through the SDK's HTTP client (`oxidhome_sdk::streaming::http`).
//!   The host trusts each camera's self-signed certificate on first use
//!   and pins it. One deadline covers a whole exchange (a call blocked
//!   inside the host is out of the watchdog's reach), and the live
//!   stream's body reader, kept between calls, wakes the plugin when
//!   data arrives.
//! - `host-media`: the import is wired, but the host answers
//!   `unavailable` until Phase 9.
//!
//! The host checks every connect, send and request against the granted
//! network rules. The import calls only work inside a component;
//! natively this module compiles (so clippy checks it) but is never
//! called.

use std::io::{Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use oxidhome_sdk::bindings::oxidhome::plugin::devices::{Command, CommandResult, DeviceInfo};
use oxidhome_sdk::bindings::oxidhome::plugin::events::{Event, EventPayload, StateChange};
use oxidhome_sdk::bindings::oxidhome::plugin::host_media;
use oxidhome_sdk::bindings::oxidhome::plugin::logging::{self, Level};
use oxidhome_sdk::bindings::oxidhome::plugin::types::{Error, KeyValue, Value};
use oxidhome_sdk::streaming::discovery::{self, DiscoverySocket, LocalNetwork};
use oxidhome_sdk::streaming::http::{self, BodyReader, StreamingResponse};
use oxidhome_sdk::streaming::media::{MediaPipeline as WitPipeline, PipeWriter};
use oxidhome_sdk::streaming::wasi::clocks::monotonic_clock;
use oxidhome_sdk::streaming::wasi::http::types::ErrorCode;
use oxidhome_sdk::streaming::wasi::io::poll::{Pollable, poll};
use oxidhome_sdk::{Plugin, StreamingPlugin, host};

use crate::kasa::Kasa;
use crate::media::{self, PluginPipeConfig};
use crate::platform::{ByteStream, Host, Net, NetError, PipeSink};
use crate::protocol::camera::SNAPSHOT_MAX_BYTES;
use crate::protocol::http::{HttpRequest, HttpResponse, Method};
use crate::protocol::iot::{FrameDecoder, MAX_FRAME_BYTES};
use crate::protocol::linkie;
use crate::protocol::multipart::STREAM_IDLE_TIMEOUT_MS;

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
            call_id: None,
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
    fn open_pipe(&mut self, config: PluginPipeConfig) -> Result<Box<dyn PipeSink>, Error> {
        // `unavailable` from the host until OxidHome Phase 9.
        let writer = host_media::open_pipe(media::to_wit_pipe_config(&config))?;
        Ok(Box::new(WasiPipe(Some(writer))))
    }
}

/// A `host-media` pipe writer.
struct WasiPipe(Option<PipeWriter>);

impl PipeSink for WasiPipe {
    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        let w = self
            .0
            .as_ref()
            .ok_or_else(|| Error::Unavailable("pipe closed".into()))?;
        let n = w.write(data)?;
        // A short write would hand the pipeline a truncated RTP packet.
        if usize::try_from(n).ok() == Some(data.len()) {
            Ok(())
        } else {
            Err(Error::Unavailable(format!(
                "short pipe write: {n} of {} bytes",
                data.len()
            )))
        }
    }

    fn close(&mut self) {
        if let Some(w) = self.0.take() {
            w.close();
        }
    }
}

// ── Net ──────────────────────────────────────────────────────────────────

/// The real network.
pub struct WasiNet {
    udp: Option<DiscoverySocket>,
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

    fn udp_open(&mut self) -> Result<(), NetError> {
        self.udp = Some(DiscoverySocket::bind_v4(0).map_err(not_sent)?);
        Ok(())
    }

    fn udp_send(&mut self, to: SocketAddrV4, data: &[u8]) -> Result<(), NetError> {
        let s = self.udp.as_ref().ok_or_else(|| not_sent("socket closed"))?;
        s.send_to(data, SocketAddr::V4(to)).map_err(not_sent)
    }

    fn udp_try_recv(&mut self) -> Option<(Ipv4Addr, Vec<u8>)> {
        // One at a time, so nothing waits in plugin memory where the
        // host's readiness wakeup can't see it. An error is the OS's on
        // this socket; the next run opens a new one.
        let d = self.udp.as_ref()?.recv(1).ok()?.into_iter().next()?;
        match d.from {
            SocketAddr::V4(from) => Some((*from.ip(), d.data)),
            SocketAddr::V6(_) => None,
        }
    }

    fn udp_close(&mut self) {
        self.udp = None;
    }

    fn local_networks(&mut self) -> Vec<LocalNetwork> {
        discovery::local_networks()
            .into_iter()
            .filter(|n| matches!(n.address, IpAddr::V4(_)))
            .collect()
    }

    fn https(&mut self, req: &HttpRequest, timeout_ms: u64) -> Result<HttpResponse, NetError> {
        // One deadline for the exchange, by the clock and by a timer
        // (either can run ahead of the other on a test host).
        let deadline = Instant::now() + ms(timeout_ms);
        let timer = monotonic_clock::subscribe_duration(nanos(ms(timeout_ms)));
        // Streamed even though the body is read whole: an error status is
        // the answer (401 and 403 mean the credentials were refused), its
        // body isn't needed, and failing to read it mustn't turn that
        // answer into "maybe sent", as `send` would.
        let mut resp = request(req, ms(timeout_ms), ms(timeout_ms))
            .send_streaming()
            .map_err(|e| net_error(&e))?;
        let mut head = head(&resp);
        if head.status < 400 {
            head.body = read_to_end(resp.body(), &timer, deadline, max_body(req))?;
        }
        Ok(head)
    }

    fn https_stream(
        &mut self,
        req: &HttpRequest,
        timeout_ms: u64,
    ) -> Result<(HttpResponse, Box<dyn ByteStream>), NetError> {
        // `timeout` bounds each gap in the endless body; the total timeout
        // covers everything up to the head.
        let resp = request(req, ms(STREAM_IDLE_TIMEOUT_MS), ms(timeout_ms))
            .send_streaming()
            .map_err(|e| net_error(&e))?;
        let head = head(&resp);
        Ok((head, Box::new(WasiBody(resp.into_body()))))
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

/// The largest body worth reading: a LINKIE2 reply or a snapshot.
fn max_body(req: &HttpRequest) -> usize {
    if req.port == linkie::LINKIE_PORT {
        linkie::MAX_REPLY_BYTES
    } else {
        SNAPSHOT_MAX_BYTES
    }
}

/// `req` for the SDK client. Redirects aren't followed (a camera doesn't
/// send them, and one would carry the credentials along); the client
/// drops the headers `wasi:http` refuses and sets `content-length`.
fn request(req: &HttpRequest, timeout: Duration, total: Duration) -> http::Request {
    let url = format!("https://{}:{}{}", req.host, req.port, req.path);
    let method = match req.method {
        Method::Get => http::Method::Get,
        Method::Post => http::Method::Post,
    };
    req.headers.iter().fold(
        http::Request::new(method, url)
            .body(req.body.clone())
            .timeout(timeout)
            .total_timeout(total)
            .max_redirects(0),
        |r, (k, v)| r.header(k.as_str(), v.as_bytes()),
    )
}

/// The response head, its body not read.
fn head(resp: &StreamingResponse) -> HttpResponse {
    HttpResponse {
        status: resp.status(),
        headers: resp
            .headers()
            .iter()
            .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).into_owned()))
            .collect(),
        body: Vec::new(),
    }
}

/// An SDK client failure, split by whether it may have reached the camera.
fn net_error(e: &http::Error) -> NetError {
    let msg = match e {
        http::Error::Http(ErrorCode::TlsCertificateError) => format!(
            "{e}: the host refused the camera's certificate (not the one pinned at \
             first use, or not valid); a pin mismatch is in the host's audit log"
        ),
        _ if e.is_denied() => format!("{e}: no granted network rule allows this camera port"),
        _ => e.to_string(),
    };
    if e.may_have_reached_server() {
        NetError::Sent(msg)
    } else {
        NetError::NotSent(msg)
    }
}

/// Reads the whole body, at most `max` bytes, before `timer` fires or
/// `deadline` passes: the SDK's total timeout ends at the head of a
/// streamed response.
fn read_to_end(
    body: &mut BodyReader,
    timer: &Pollable,
    deadline: Instant,
    max: usize,
) -> Result<Vec<u8>, NetError> {
    let timed_out = || sent("timed out reading the answer");
    let mut out = Vec::new();
    loop {
        // Checked before every read: a camera that keeps a little data
        // ready would otherwise never reach the wait below.
        if timer.ready() || Instant::now() >= deadline {
            return Err(timed_out());
        }
        let room = max.saturating_sub(out.len()).saturating_add(1);
        // The head has arrived, so a failed body reached the camera.
        match body.read(room.min(READ_CHUNK)).map_err(sent)? {
            None => return Ok(out),
            Some(chunk) if chunk.is_empty() => {
                if poll(&[body.pollable(), timer]).contains(&1) {
                    return Err(timed_out());
                }
            }
            Some(chunk) if chunk.len() > max - out.len() => {
                return Err(sent(format!("answer larger than {max} bytes")));
            }
            Some(chunk) => out.extend_from_slice(&chunk),
        }
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Largest read asked of a response body at once.
const READ_CHUNK: usize = 64 * 1024;

/// The live stream's body. Kept between calls, it wakes the plugin when
/// data (or the end) arrives.
struct WasiBody(BodyReader);

impl ByteStream for WasiBody {
    fn read_available(&mut self, max: usize) -> Result<Vec<u8>, NetError> {
        match self.0.read(max) {
            Ok(Some(data)) => Ok(data),
            Ok(None) => Err(sent("the camera ended the stream")),
            // The head has arrived, so a failed body reached the camera.
            Err(e) => Err(sent(e)),
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

impl StreamingPlugin for KasaPlugin {
    /// Not called by the host yet (Phase 9).
    fn setup_pipeline(&mut self, device: String) -> Result<WitPipeline, String> {
        self.0.setup_pipeline(&device).map(|p| media::to_wit(&p))
    }
}

#[cfg(target_family = "wasm")]
oxidhome_sdk::streaming_plugin!(KasaPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::camera;

    #[test]
    fn bodies_are_bounded_per_endpoint() {
        let req = linkie::build_request(
            "192.168.1.20",
            linkie::LINKIE_PORT,
            "Basic x",
            &linkie::LinkieRequest::new(),
            "uuid",
        );
        assert_eq!(max_body(&req), linkie::MAX_REPLY_BYTES);
        let snap = camera::snapshot_request("192.168.1.20", camera::STREAM_PORT, "Basic y");
        assert_eq!(max_body(&snap), SNAPSHOT_MAX_BYTES);
    }

    #[test]
    fn requests_keep_credentials_and_drop_hop_by_hop_headers() {
        let req = linkie::build_request(
            "192.168.1.20",
            linkie::LINKIE_PORT,
            "Basic x",
            &linkie::LinkieRequest::new(),
            "uuid",
        );
        assert!(req.header("connection").is_some(), "{req:?}");
        let built = format!("{:?}", request(&req, ms(1), ms(1)));
        assert!(built.contains("\"authorization\""), "{built}");
        assert!(!built.contains("\"connection\""), "{built}");
        assert!(built.contains("https://192.168.1.20:10443/"), "{built}");
        assert!(built.contains("max_redirects: 0"), "{built}");
    }

    #[test]
    fn errors_split_by_whether_the_request_went_out() {
        let http = |code| net_error(&http::Error::Http(code));
        for code in [
            ErrorCode::ConnectionRefused,
            ErrorCode::ConnectionTimeout,
            ErrorCode::TlsProtocolError,
            ErrorCode::TlsCertificateError,
            ErrorCode::HttpRequestDenied,
        ] {
            assert!(!http(code.clone()).maybe_sent(), "{code:?}");
        }
        for code in [
            ErrorCode::ConnectionReadTimeout,
            ErrorCode::ConnectionTerminated,
            ErrorCode::HttpResponseTimeout,
            ErrorCode::InternalError(None),
        ] {
            assert!(http(code.clone()).maybe_sent(), "{code:?}");
        }
        assert!(
            http(ErrorCode::TlsCertificateError)
                .message()
                .contains("pinned")
        );
        assert!(
            http(ErrorCode::HttpRequestDenied)
                .message()
                .contains("network rule")
        );
        let timeout = |dispatched| {
            net_error(&http::Error::TotalTimeout {
                limit: ms(1),
                dispatched,
            })
        };
        assert!(!timeout(false).maybe_sent());
        assert!(timeout(true).maybe_sent());
    }
}
