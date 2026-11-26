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
//!   19443) over `wasi:http`: buffered exchanges through the SDK client,
//!   the endless stream body through the raw bindings. The host trusts
//!   each camera's self-signed certificate on first use and pins it.
//! - `host-media`: the import is wired, but the host answers
//!   `unavailable` until Phase 9.
//!
//! The host checks every connect, send and request against the granted
//! network rules. The import calls only work inside a component;
//! natively this module compiles (so clippy checks it) but is never
//! called.

use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use oxidhome_sdk::bindings::oxidhome::plugin::devices::{Command, CommandResult, DeviceInfo};
use oxidhome_sdk::bindings::oxidhome::plugin::events::{Event, EventPayload, StateChange};
use oxidhome_sdk::bindings::oxidhome::plugin::host_media;
use oxidhome_sdk::bindings::oxidhome::plugin::logging::{self, Level};
use oxidhome_sdk::bindings::oxidhome::plugin::types::{Error, KeyValue, Value};
use oxidhome_sdk::streaming::discovery::{self, DiscoverySocket, LocalNetwork};
use oxidhome_sdk::streaming::http as sdk_http;
use oxidhome_sdk::streaming::media::{MediaPipeline as WitPipeline, PipeWriter};
use oxidhome_sdk::streaming::wasi::clocks::monotonic_clock;
use oxidhome_sdk::streaming::wasi::http::outgoing_handler;
use oxidhome_sdk::streaming::wasi::http::types::{
    ErrorCode, Fields, IncomingBody, IncomingResponse, Method as WasiMethod, OutgoingBody,
    OutgoingRequest, RequestOptions, Scheme, http_error_code,
};
use oxidhome_sdk::streaming::wasi::io::poll::poll;
use oxidhome_sdk::streaming::wasi::io::streams::{InputStream, StreamError};
use oxidhome_sdk::{Plugin, StreamingPlugin, host};

use crate::kasa::Kasa;
use crate::media::{self, PluginPipeConfig};
use crate::platform::{ByteStream, Host, Net, NetError, PipeSink};
use crate::protocol::camera::SNAPSHOT_MAX_BYTES;
use crate::protocol::http::{HttpRequest, HttpResponse, Method};
use crate::protocol::iot::{FrameDecoder, MAX_FRAME_BYTES};
use crate::protocol::multipart::STREAM_KEEPALIVE_IDLE_MS;

/// Headers `wasi:http` refuses from a guest (hop-by-hop, or set by the
/// host), and `content-length`, which the client sets from the body.
const HOST_SET_HEADERS: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "host",
    "http2-settings",
    "content-length",
];

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
    /// Datagrams received but not handed out yet (`receive` returns up
    /// to 32 at once).
    inbox: VecDeque<(Ipv4Addr, Vec<u8>)>,
    start: Instant,
    rng: u64,
}

impl Default for WasiNet {
    fn default() -> Self {
        Self {
            udp: None,
            inbox: VecDeque::new(),
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
        self.inbox.clear();
        self.udp = Some(DiscoverySocket::bind_v4(0).map_err(not_sent)?);
        Ok(())
    }

    fn udp_send(&mut self, to: SocketAddrV4, data: &[u8]) -> Result<(), NetError> {
        let s = self.udp.as_ref().ok_or_else(|| not_sent("socket closed"))?;
        s.send_to(data, SocketAddr::V4(to)).map_err(not_sent)
    }

    fn udp_try_recv(&mut self) -> Option<(Ipv4Addr, Vec<u8>)> {
        if self.inbox.is_empty() {
            let s = self.udp.as_ref()?;
            // An error is the OS's on this socket; the next run opens a
            // new one.
            for d in s.recv(discovery::MAX_RECV).unwrap_or_default() {
                if let SocketAddr::V4(from) = d.from {
                    self.inbox.push_back((*from.ip(), d.data));
                }
            }
        }
        self.inbox.pop_front()
    }

    fn udp_close(&mut self) {
        self.udp = None;
        self.inbox.clear();
    }

    fn local_networks(&mut self) -> Vec<LocalNetwork> {
        discovery::local_networks()
            .into_iter()
            .filter(|n| matches!(n.address, IpAddr::V4(_)))
            .collect()
    }

    fn https(&mut self, req: &HttpRequest, timeout_ms: u64) -> Result<HttpResponse, NetError> {
        let method = match req.method {
            Method::Get => sdk_http::Method::Get,
            Method::Post => sdk_http::Method::Post,
        };
        // Cameras don't redirect; one that does is answered as is.
        let mut r = sdk_http::Request::new(method, url(req))
            .timeout(ms(timeout_ms))
            .max_body(SNAPSHOT_MAX_BYTES)
            .max_redirects(0);
        for (name, value) in guest_headers(req) {
            r = r.header(name, value.as_bytes());
        }
        if !req.body.is_empty() {
            r = r.body(req.body.clone());
        }
        let resp = r.send().map_err(|e| match e {
            sdk_http::Error::Http(code) => http_error(&code),
            sdk_http::Error::InvalidUrl(_) | sdk_http::Error::Request(_) => not_sent(e),
            // The request went out and an answer came back.
            sdk_http::Error::BodyTooLarge { .. }
            | sdk_http::Error::TooManyRedirects { .. }
            | sdk_http::Error::InsecureRedirect(_) => sent(e),
        })?;
        Ok(HttpResponse {
            status: resp.status(),
            headers: resp
                .headers()
                .iter()
                .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).into_owned()))
                .collect(),
            body: resp.into_body(),
        })
    }

    fn https_stream(
        &mut self,
        req: &HttpRequest,
        timeout_ms: u64,
    ) -> Result<(HttpResponse, Box<dyn ByteStream>), NetError> {
        // The SDK client buffers whole bodies; the live stream never ends,
        // so it is read through `wasi:http` directly.
        let (head, body) = open_stream(req, timeout_ms)?;
        Ok((head, Box::new(body)))
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

/// `https://host:port/path` (the host is an IP literal: the certificate
/// name isn't checked, the pin binds the certificate to the address).
fn url(req: &HttpRequest) -> String {
    format!("https://{}:{}{}", req.host, req.port, req.path)
}

/// The request's headers without those the host sets or refuses.
fn guest_headers(req: &HttpRequest) -> impl Iterator<Item = (&str, &str)> {
    req.headers
        .iter()
        .filter(|(k, _)| !HOST_SET_HEADERS.iter().any(|h| k.eq_ignore_ascii_case(h)))
        .map(|(k, v)| (k.as_str(), v.as_str()))
}

/// Whether a `wasi:http` failure may have reached the camera: failures
/// before or during the connection and TLS handshake didn't, the rest
/// (a timeout waiting for the answer, a cut connection) may have.
fn http_error(code: &ErrorCode) -> NetError {
    let msg = match code {
        ErrorCode::TlsCertificateError => format!(
            "{code:?}: the camera's certificate doesn't match the one pinned \
             at first use; if it was replaced, reset the pin (`oxidhome tls-pin reset`)"
        ),
        ErrorCode::HttpRequestDenied => {
            format!("{code:?}: no granted network rule allows this camera port")
        }
        _ => format!("{code:?}"),
    };
    match code {
        ErrorCode::DnsTimeout
        | ErrorCode::DnsError(_)
        | ErrorCode::DestinationNotFound
        | ErrorCode::DestinationUnavailable
        | ErrorCode::DestinationIpProhibited
        | ErrorCode::DestinationIpUnroutable
        | ErrorCode::ConnectionRefused
        | ErrorCode::ConnectionTimeout
        | ErrorCode::ConnectionLimitReached
        | ErrorCode::TlsProtocolError
        | ErrorCode::TlsCertificateError
        | ErrorCode::TlsAlertReceived(_)
        | ErrorCode::HttpRequestDenied
        | ErrorCode::HttpRequestMethodInvalid
        | ErrorCode::HttpRequestUriInvalid
        | ErrorCode::HttpRequestUriTooLong
        | ErrorCode::HttpRequestHeaderSectionSize(_)
        | ErrorCode::HttpRequestHeaderSize(_) => NetError::NotSent(msg),
        _ => NetError::Sent(msg),
    }
}

/// A live response body. Fields drop in order: the stream before the
/// body it came from, the body before its response.
struct WasiBody {
    stream: InputStream,
    _body: IncomingBody,
    _response: IncomingResponse,
}

impl ByteStream for WasiBody {
    fn read_available(&mut self, max: usize) -> Result<Vec<u8>, NetError> {
        match self.stream.read(u64::try_from(max).unwrap_or(u64::MAX)) {
            Ok(data) => Ok(data),
            Err(StreamError::Closed) => Err(sent("the camera ended the stream")),
            Err(StreamError::LastOperationFailed(e)) => Err(match http_error_code(&e) {
                Some(code) => sent(format!("{code:?}")),
                None => sent(e.to_debug_string()),
            }),
        }
    }
}

/// Sends a body-less `GET` and waits up to `timeout_ms` for the response
/// head. The body is read later, without blocking; the host fails it
/// after [`STREAM_KEEPALIVE_IDLE_MS`] without data.
fn open_stream(req: &HttpRequest, timeout_ms: u64) -> Result<(HttpResponse, WasiBody), NetError> {
    let fail = |what: &str| not_sent(format!("invalid {what}"));
    let headers = Fields::new();
    for (name, value) in guest_headers(req) {
        headers
            .append(&name.to_ascii_lowercase(), value.as_bytes())
            .map_err(|e| not_sent(format!("header `{name}`: {e:?}")))?;
    }
    let request = OutgoingRequest::new(headers);
    request
        .set_method(&match req.method {
            Method::Get => WasiMethod::Get,
            Method::Post => WasiMethod::Post,
        })
        .map_err(|()| fail("method"))?;
    request
        .set_scheme(Some(&Scheme::Https))
        .map_err(|()| fail("scheme"))?;
    request
        .set_authority(Some(&format!("{}:{}", req.host, req.port)))
        .map_err(|()| fail("authority"))?;
    request
        .set_path_with_query(Some(&req.path))
        .map_err(|()| fail("path"))?;
    let nanos = |ms: u64| ms.saturating_mul(1_000_000);
    let options = RequestOptions::new();
    let _ = options.set_connect_timeout(Some(nanos(timeout_ms)));
    let _ = options.set_first_byte_timeout(Some(nanos(timeout_ms)));
    let _ = options.set_between_bytes_timeout(Some(nanos(STREAM_KEEPALIVE_IDLE_MS)));
    let outgoing = request.body().map_err(|()| fail("body"))?;
    let pending = outgoing_handler::handle(request, Some(options)).map_err(|e| http_error(&e))?;
    // No body: finishing it lets the request go out.
    OutgoingBody::finish(outgoing, None).map_err(|e| http_error(&e))?;
    // The host's own timeouts end the wait too; this bounds it for the
    // tick in case they don't.
    {
        let ready = pending.subscribe();
        let timer = monotonic_clock::subscribe_duration(nanos(timeout_ms.saturating_mul(2)));
        if !poll(&[&ready, &timer]).contains(&0) {
            return Err(sent("timed out waiting for the stream to start"));
        }
    }
    let response = pending
        .get()
        .ok_or_else(|| sent("response not ready"))?
        .map_err(|()| sent("response already taken"))?
        .map_err(|e| http_error(&e))?;
    let head = HttpResponse {
        status: response.status(),
        headers: response
            .headers()
            .entries()
            .into_iter()
            .map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned()))
            .collect(),
        body: Vec::new(),
    };
    let body = response
        .consume()
        .map_err(|()| sent("response body already taken"))?;
    let stream = body
        .stream()
        .map_err(|()| sent("response stream already taken"))?;
    Ok((
        head,
        WasiBody {
            stream,
            _body: body,
            _response: response,
        },
    ))
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
