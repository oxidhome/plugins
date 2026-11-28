//! The camera control protocol: `POST https://<camera>:10443/data/LINKIE2.json`.
//!
//! - Body: form-encoded `content=<url-encoded base64(xor(json))>`.
//! - `User-Agent` must look like the Kasa app (start with `Kasa/`);
//!   requests that don't are **silently dropped**.
//! - `Authorization` (control encoding, see [`super::http::control_auth`])
//!   on the first request; there is no 401 challenge.
//! - `Accept-Encoding: identity`, so the camera never compresses the reply,
//!   which would break the decoding.
//! - The JSON uses the smart-home module/method envelope with modules under
//!   `smartlife.cam.ipcamera.`, plus a `context.source` UUID the Kasa app
//!   sends and the camera ignores.
//! - Response body: `base64(xor(json))`; trim, decode, decrypt, parse. An
//!   HTTP status of 400 or more is a transport-level failure.
//! - Calls to one camera must be serialized (parallel ones occasionally
//!   lose a reply); a kept-alive connection takes a call from ~500 ms to
//!   ~30 ms; a request on a reused socket the camera closed while idle
//!   fails with a reset and is retried once. Timeout 5 s.

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value, json};

use super::cipher;
use super::http::{HttpRequest, HttpResponse, Method};
use super::json::get_str;

/// Control port.
pub const LINKIE_PORT: u16 = 10443;
/// Control path.
pub const LINKIE_PATH: &str = "/data/LINKIE2.json";
/// Per-call timeout.
pub const LINKIE_TIMEOUT_MS: u64 = 5_000;
/// Module prefix of every camera module.
pub const MODULE_PREFIX: &str = "smartlife.cam.ipcamera.";
/// The User-Agent of the Kasa iOS app the camera gates on.
pub const USER_AGENT: &str = "Kasa/1752 CFNetwork/3860.500.112 Darwin/25.4.0";
/// Largest reply body accepted (real ones are well under 4 KB).
pub const MAX_REPLY_BYTES: usize = 256 * 1024;

/// LINKIE2 error codes, the same on KC410S, KC420WS and KD110.
pub mod err {
    /// "Unsupported API call": unknown module or method.
    pub const UNSUPPORTED: i64 = -10008;
    /// The camera is turned off (Privacy Mode).
    pub const CAMERA_OFF: i64 = -40919;
    /// Missing time parameter (misleadingly `[start_time] is required`).
    pub const MISSING_TIME: i64 = -40700;
    /// Invalid time parameter.
    pub const INVALID_TIME: i64 = -40706;
    /// "Other error." from the motion poll; transient.
    pub const OTHER: i64 = -40709;
    /// Bad PTZ argument, or the axis is at its end stop.
    pub const PTZ_ARGUMENT: i64 = -41203;
}

/// Errors from a LINKIE2 exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkieError {
    /// HTTP status ≥ 400.
    Http(u16),
    /// The body didn't decode (base64, size, JSON).
    BadBody(String),
    /// The reply has no result for the method.
    Missing { module: String, method: String },
    /// The method answered a non-zero (or no) `err_code`.
    Device {
        module: String,
        method: String,
        code: Option<i64>,
        msg: Option<String>,
    },
}

impl LinkieError {
    /// The device error code, when there is one.
    #[must_use]
    pub fn code(&self) -> Option<i64> {
        match self {
            LinkieError::Device { code, .. } => *code,
            _ => None,
        }
    }
}

impl std::fmt::Display for LinkieError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkieError::Http(s) => write!(f, "LINKIE2 HTTP {s}"),
            LinkieError::BadBody(e) => write!(f, "LINKIE2 reply undecodable: {e}"),
            LinkieError::Missing { module, method } => {
                write!(f, "LINKIE2 reply has no {module}.{method}")
            }
            LinkieError::Device {
                module,
                method,
                code,
                msg,
            } => {
                let short = module.strip_prefix(MODULE_PREFIX).unwrap_or(module);
                match code {
                    Some(c) => write!(f, "{short} {method} err_code={c}")?,
                    None => write!(f, "{short} {method} err_code=none")?,
                }
                if let Some(m) = msg {
                    write!(f, " ({m})")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for LinkieError {}

/// A LINKIE2 request: one method per module (with two methods of one
/// module in a request, only the last is answered, so [`LinkieRequest::camera`]
/// replaces).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkieRequest {
    body: Map<String, Value>,
}

impl LinkieRequest {
    /// Empty request.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `smartlife.cam.ipcamera.<module>.<method>(args)`.
    #[must_use]
    pub fn camera(self, module: &str, method: &str, args: Value) -> Self {
        self.raw(&format!("{MODULE_PREFIX}{module}"), method, args)
    }

    /// Adds a call under a full module name (e.g. top-level `system`).
    #[must_use]
    pub fn raw(mut self, module: &str, method: &str, args: Value) -> Self {
        let mut m = Map::new();
        m.insert(method.to_string(), args);
        self.body.insert(module.to_string(), Value::Object(m));
        self
    }

    /// JSON without `context`.
    #[must_use]
    pub fn to_json(&self) -> String {
        Value::Object(self.body.clone()).to_string()
    }

    /// JSON with the `context.source` UUID the Kasa app adds.
    #[must_use]
    pub fn to_json_with_context(&self, source_uuid: &str) -> String {
        let mut b = self.body.clone();
        b.insert("context".into(), json!({ "source": source_uuid }));
        Value::Object(b).to_string()
    }

    /// `(module, method)` pairs.
    #[must_use]
    pub fn methods(&self) -> Vec<(String, String)> {
        self.body
            .iter()
            .filter_map(|(m, v)| {
                let method = v.as_object()?.keys().next()?;
                Some((m.clone(), method.clone()))
            })
            .collect()
    }
}

/// `encodeURIComponent` for base64 text: only `+`, `/` and `=` need it.
fn encode_uri_component_b64(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '+' => out.push_str("%2B"),
            '/' => out.push_str("%2F"),
            '=' => out.push_str("%3D"),
            c => out.push(c),
        }
    }
    out
}

/// The form body for a JSON payload.
#[must_use]
pub fn form_body(json: &str) -> String {
    let enc = cipher::encrypt(json.as_bytes());
    format!("content={}", encode_uri_component_b64(&B64.encode(enc)))
}

/// Formats 16 random bytes as a version-4 UUID.
#[must_use]
pub fn uuid_v4(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h = bytes.iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Builds the HTTPS request. `auth` is the control `Authorization` value.
#[must_use]
pub fn build_request(
    host: &str,
    port: u16,
    auth: &str,
    request: &LinkieRequest,
    source_uuid: &str,
) -> HttpRequest {
    let body = form_body(&request.to_json_with_context(source_uuid)).into_bytes();
    HttpRequest {
        method: Method::Post,
        host: host.to_string(),
        port,
        path: LINKIE_PATH.to_string(),
        headers: vec![
            ("Authorization".into(), auth.to_string()),
            (
                "Content-Type".into(),
                "application/x-www-form-urlencoded; charset=utf-8".into(),
            ),
            ("Content-Length".into(), body.len().to_string()),
            ("User-Agent".into(), USER_AGENT.into()),
            ("Accept".into(), "*/*".into()),
            ("Accept-Language".into(), "en-US,en;q=0.9".into()),
            ("Accept-Encoding".into(), "identity".into()),
            ("Connection".into(), "keep-alive".into()),
        ],
        body,
    }
}

/// Decodes a reply body: trim, base64, XOR, JSON.
///
/// # Errors
///
/// [`LinkieError::BadBody`].
pub fn decode_body(body: &[u8]) -> Result<Value, LinkieError> {
    if body.len() > MAX_REPLY_BYTES {
        return Err(LinkieError::BadBody(format!(
            "{} bytes exceeds {MAX_REPLY_BYTES}",
            body.len()
        )));
    }
    let text = std::str::from_utf8(body).map_err(|e| LinkieError::BadBody(e.to_string()))?;
    let mut raw = B64
        .decode(text.trim())
        .map_err(|e| LinkieError::BadBody(e.to_string()))?;
    cipher::decrypt_in_place(&mut raw);
    serde_json::from_slice(&raw).map_err(|e| LinkieError::BadBody(e.to_string()))
}

/// Decodes a response, failing on HTTP ≥ 400.
///
/// # Errors
///
/// [`LinkieError::Http`] or [`LinkieError::BadBody`].
pub fn decode_response(resp: &HttpResponse) -> Result<Value, LinkieError> {
    if resp.status >= 400 {
        return Err(LinkieError::Http(resp.status));
    }
    decode_body(&resp.body)
}

/// The result object of `module.method` (full module name), without
/// checking `err_code`.
///
/// # Errors
///
/// [`LinkieError::Missing`].
pub fn result<'a>(reply: &'a Value, module: &str, method: &str) -> Result<&'a Value, LinkieError> {
    reply
        .get(module)
        .and_then(|m| m.get(method))
        .ok_or_else(|| LinkieError::Missing {
            module: module.into(),
            method: method.into(),
        })
}

/// The result of `module.method`, requiring `err_code == 0`. A missing
/// `err_code` is a failure, as in the TS plugin's `command()`.
///
/// # Errors
///
/// [`LinkieError::Missing`] or [`LinkieError::Device`].
pub fn ok_result<'a>(
    reply: &'a Value,
    module: &str,
    method: &str,
) -> Result<&'a Value, LinkieError> {
    let r = result(reply, module, method)?;
    let code = r.get("err_code").and_then(Value::as_i64);
    if code == Some(0) {
        Ok(r)
    } else {
        Err(LinkieError::Device {
            module: module.into(),
            method: method.into(),
            code,
            msg: get_str(r, "err_msg").map(str::to_string),
        })
    }
}

/// Encodes a reply the way a camera does (for fakes and tests).
#[must_use]
pub fn encode_reply(json: &Value) -> Vec<u8> {
    B64.encode(cipher::encrypt(json.to_string().as_bytes()))
        .into_bytes()
}

/// Decodes a request form body back to JSON (for fakes and tests).
///
/// # Errors
///
/// [`LinkieError::BadBody`].
pub fn decode_form_body(body: &[u8]) -> Result<Value, LinkieError> {
    let text = std::str::from_utf8(body).map_err(|e| LinkieError::BadBody(e.to_string()))?;
    let b64 = text
        .strip_prefix("content=")
        .ok_or_else(|| LinkieError::BadBody("no content= field".into()))?
        .replace("%2B", "+")
        .replace("%2F", "/")
        .replace("%3D", "=");
    decode_body(b64.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_body_matches_protocol_doc_vector() {
        let r = LinkieRequest::new().camera("led", "get_status", json!({}));
        assert_eq!(
            r.to_json(),
            r#"{"smartlife.cam.ipcamera.led":{"get_status":{}}}"#
        );
        assert_eq!(
            form_body(&r.to_json()),
            "content=0PKB7I3%2Fi%2BeO6I2jwKHM4ov7mPmU8YPizKDFoYO5wuCH4pbJus6v267d%2F8W%2Bw77D"
        );
    }

    #[test]
    fn request_has_the_gating_headers_and_context() {
        let r = LinkieRequest::new().camera("siren", "set_state", json!({"value":"on"}));
        let req = build_request("10.0.0.5", LINKIE_PORT, "Basic x", &r, "uuid-1");
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.path, "/data/LINKIE2.json");
        assert_eq!(req.port, 10443);
        assert!(req.header("user-agent").unwrap().starts_with("Kasa/"));
        assert_eq!(req.header("Accept-Encoding"), Some("identity"));
        assert_eq!(req.header("Authorization"), Some("Basic x"));
        assert_eq!(
            req.header("Content-Length"),
            Some(req.body.len().to_string().as_str())
        );
        let sent = decode_form_body(&req.body).unwrap();
        assert_eq!(sent["context"]["source"], "uuid-1");
        assert_eq!(
            sent["smartlife.cam.ipcamera.siren"]["set_state"]["value"],
            "on"
        );
    }

    #[test]
    fn decodes_replies_and_errors() {
        let reply =
            json!({"smartlife.cam.ipcamera.led":{"get_status":{"value":"on","err_code":0}}});
        let mut body = encode_reply(&reply);
        body.extend_from_slice(b"\r\n");
        let v = decode_response(&HttpResponse {
            status: 200,
            headers: vec![],
            body,
        })
        .unwrap();
        assert_eq!(v, reply);
        assert_eq!(
            ok_result(&v, "smartlife.cam.ipcamera.led", "get_status").unwrap()["value"],
            "on"
        );
        assert_eq!(
            decode_response(&HttpResponse {
                status: 401,
                headers: vec![],
                body: vec![]
            }),
            Err(LinkieError::Http(401))
        );
        assert!(matches!(
            decode_body(b"!!notbase64"),
            Err(LinkieError::BadBody(_))
        ));
        assert!(matches!(
            decode_body(&[0xff, 0xfe]),
            Err(LinkieError::BadBody(_))
        ));
        let huge = vec![b'A'; MAX_REPLY_BYTES + 1];
        assert!(matches!(decode_body(&huge), Err(LinkieError::BadBody(_))));
        assert!(decode_form_body(b"x=1").is_err());
    }

    #[test]
    fn missing_or_nonzero_err_code_fails() {
        let v = json!({"m":{"a":{"err_code":-40919},"b":{},"c":{"err_code":0}}});
        let e = ok_result(&v, "m", "a").unwrap_err();
        assert_eq!(e.code(), Some(err::CAMERA_OFF));
        assert_eq!(e.to_string(), "m a err_code=-40919");
        let e = ok_result(&v, "m", "b").unwrap_err();
        assert_eq!(e.code(), None);
        assert_eq!(e.to_string(), "m b err_code=none");
        assert!(ok_result(&v, "m", "c").is_ok());
        assert!(matches!(
            ok_result(&v, "m", "d"),
            Err(LinkieError::Missing { .. })
        ));
        assert_eq!(LinkieError::Http(503).code(), None);
    }

    #[test]
    fn one_method_per_module() {
        let r = LinkieRequest::new()
            .camera("ptz", "get_capability", json!({}))
            .camera("ptz", "get_all_preset", json!({}))
            .raw("system", "get_sysinfo", json!({}));
        let methods = r.methods();
        assert_eq!(methods.len(), 2);
        assert!(methods.contains(&("smartlife.cam.ipcamera.ptz".into(), "get_all_preset".into())));
        assert!(methods.contains(&("system".into(), "get_sysinfo".into())));
    }

    #[test]
    fn uuid_has_version_and_variant_bits() {
        let u = uuid_v4([0xff; 16]);
        assert_eq!(u, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(uuid_v4([0; 16]), "00000000-0000-4000-8000-000000000000");
    }
}
