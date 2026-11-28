//! Transport-neutral HTTPS request/response shapes and the camera's HTTP
//! Basic credentials.
//!
//! Every camera endpoint is HTTPS with a **self-signed** certificate and
//! takes Basic auth with the Kasa account email and an *encoded* form of
//! the account password; the encoding differs per endpoint and each
//! endpoint rejects the other's. Credentials go on the **first** request:
//! LINKIE2 silently drops a request without them instead of answering 401.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use md5::{Digest, Md5};
use std::fmt::Write as _;

/// HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// One HTTPS request to a camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: Method,
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// A header's value (case-insensitive name).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

/// A buffered HTTPS response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A header's value (case-insensitive name).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn basic(username: &str, encoded_password: &str) -> String {
    format!(
        "Basic {}",
        B64.encode(format!("{username}:{encoded_password}"))
    )
}

/// Credentials for the stream port (19443: live stream, snapshot,
/// audio-only): `base64(password)` as the password.
#[must_use]
pub fn stream_auth(username: &str, password: &str) -> String {
    basic(username, &B64.encode(password.as_bytes()))
}

/// Credentials for talk (18443) and LINKIE2 (10443): lowercase hex
/// `md5(password)`. Observed on KC420WS firmware 2.3.26 from the Kasa iOS
/// app.
#[must_use]
pub fn control_auth(username: &str, password: &str) -> String {
    let digest = Md5::digest(password.as_bytes());
    let hex = digest.iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    basic(username, &hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known answers from `docs/PROTOCOL.md` for `user@example.com` / `secret`.
    #[test]
    fn auth_encodings_match_the_protocol_doc() {
        assert_eq!(
            stream_auth("user@example.com", "secret"),
            "Basic dXNlckBleGFtcGxlLmNvbTpjMlZqY21WMA=="
        );
        assert_eq!(
            control_auth("user@example.com", "secret"),
            "Basic dXNlckBleGFtcGxlLmNvbTo1ZWJlMjI5NGVjZDBlMGYwOGVhYjc2OTBkMmE2ZWU2OQ=="
        );
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let r = HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "image/jpg".into())],
            body: Vec::new(),
        };
        assert_eq!(r.header("content-type"), Some("image/jpg"));
        assert_eq!(r.header("x"), None);
        assert_eq!(Method::Post.as_str(), "POST");
        assert_eq!(Method::Get.as_str(), "GET");
    }
}
