//! The camera's live stream: `GET https://<camera>:19443/https/stream/mixed`.
//!
//! The response never ends; its body is `multipart/x-mixed-replace` with
//! the boundary from the `Content-Type` header. Each part is
//!
//! ```text
//! --<boundary>\r\n
//! Content-Type: video/x-h264\r\n
//! Content-Length: 12345\r\n
//! \r\n
//! <12345 bytes>
//! ```
//!
//! with H.264 Annex B video (`video/x-h264`) or G.711 µ-law 8 kHz mono
//! audio (`audio/g711u`). The parser reads the payload by
//! `Content-Length` (never by searching for the next boundary), tolerates
//! blank lines before a boundary line and LF-only framing, and matches
//! header names case-insensitively. Parts carry no timestamps.
//!
//! **One live stream per camera:** a second connection completes TCP and
//! then stalls in the TLS handshake until it times out.

use super::http::{HttpRequest, HttpResponse, Method};

/// Part content type of video.
pub const MIME_H264: &str = "video/x-h264";
/// Part content type of audio.
pub const MIME_G711U: &str = "audio/g711u";
/// Real part headers fit in well under 1 KB; the cap stops a malformed
/// stream from growing the buffer without bound.
pub const MAX_HEADER_BYTES: usize = 32 * 1024;
/// Largest part payload accepted (a 1080p IDR is ~100 KB).
pub const MAX_PART_BYTES: usize = 8 * 1024 * 1024;
/// Deadline for connecting and receiving the response headers.
pub const STREAM_CONNECT_TIMEOUT_MS: u64 = 10_000;
/// After the headers, a stream with no data for this long has ended. The
/// stream goes quiet for seconds on weak Wi-Fi and recovers, so this is
/// long; `wasi:http` has no TCP keepalive to detect a dead peer sooner.
pub const STREAM_IDLE_TIMEOUT_MS: u64 = 60_000;

/// Builds the live stream request with stream credentials.
#[must_use]
pub fn stream_request(host: &str, port: u16, stream_auth: &str) -> HttpRequest {
    HttpRequest {
        method: Method::Get,
        host: host.into(),
        port,
        path: super::camera::STREAM_PATH.into(),
        headers: vec![
            ("Authorization".into(), stream_auth.into()),
            ("Accept".into(), "*/*".into()),
            ("Connection".into(), "keep-alive".into()),
        ],
        body: Vec::new(),
    }
}

/// Why a stream failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// HTTP 503: the camera is turned off (Privacy Mode).
    CameraOff,
    /// Another status ≥ 400.
    Http(u16),
    /// The response isn't `multipart/x-mixed-replace` or has no boundary.
    BadContentType(String),
    /// The first non-blank line of a part isn't `--<boundary>`.
    UnexpectedBoundary(String),
    /// A part header block without (or with an invalid) `Content-Length`.
    NoContentLength,
    /// A header block exceeded [`MAX_HEADER_BYTES`].
    HeaderTooLarge,
    /// A part exceeded [`MAX_PART_BYTES`].
    PartTooLarge(usize),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::CameraOff => f.write_str("camera is turned off (HTTP 503)"),
            StreamError::Http(s) => write!(f, "stream HTTP {s}"),
            StreamError::BadContentType(c) => write!(f, "unexpected stream content-type {c:?}"),
            StreamError::UnexpectedBoundary(l) => write!(f, "unexpected boundary line {l:?}"),
            StreamError::NoContentLength => f.write_str("multipart part without a content length"),
            StreamError::HeaderTooLarge => {
                write!(f, "multipart header exceeded {MAX_HEADER_BYTES} bytes")
            }
            StreamError::PartTooLarge(n) => write!(f, "multipart part of {n} bytes is too large"),
        }
    }
}

impl std::error::Error for StreamError {}

/// Checks the response head and returns the boundary.
///
/// # Errors
///
/// [`StreamError`].
pub fn boundary_from_response(resp: &HttpResponse) -> Result<String, StreamError> {
    match resp.status {
        503 => return Err(StreamError::CameraOff),
        s if s >= 400 => return Err(StreamError::Http(s)),
        _ => {}
    }
    boundary_from_content_type(resp.header("content-type").unwrap_or_default())
}

/// Extracts `boundary=` from a `multipart/x-mixed-replace` content type.
///
/// # Errors
///
/// [`StreamError::BadContentType`].
pub fn boundary_from_content_type(ct: &str) -> Result<String, StreamError> {
    let lower = ct.to_ascii_lowercase();
    if !lower.contains("multipart/x-mixed-replace") {
        return Err(StreamError::BadContentType(ct.into()));
    }
    let Some(at) = lower.find("boundary=") else {
        return Err(StreamError::BadContentType(ct.into()));
    };
    let value: String = ct[at + "boundary=".len()..]
        .chars()
        .take_while(|c| *c != ';' && !c.is_whitespace())
        .collect();
    if value.is_empty() {
        return Err(StreamError::BadContentType(ct.into()));
    }
    Ok(value)
}

/// The kind of a part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Video,
    Audio,
    Other,
}

/// One part of the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Part {
    /// Video, audio, or something else.
    #[must_use]
    pub fn kind(&self) -> PartKind {
        if self.content_type.eq_ignore_ascii_case(MIME_H264) {
            PartKind::Video
        } else if self.content_type.eq_ignore_ascii_case(MIME_G711U) {
            PartKind::Audio
        } else {
            PartKind::Other
        }
    }
}

#[derive(Debug)]
enum State {
    Header,
    Body { content_type: String, length: usize },
}

/// Incremental parser: push bytes as they arrive, pull complete parts.
#[derive(Debug)]
pub struct MultipartParser {
    boundary_line: String,
    buf: Vec<u8>,
    start: usize,
    state: State,
}

impl MultipartParser {
    /// A parser for `boundary` (without the leading `--`).
    #[must_use]
    pub fn new(boundary: &str) -> Self {
        Self {
            boundary_line: format!("--{boundary}"),
            buf: Vec::new(),
            start: 0,
            state: State::Header,
        }
    }

    /// Appends received bytes.
    pub fn push(&mut self, data: &[u8]) {
        if self.start > 0 && self.start >= self.buf.len() / 2 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// Bytes buffered but not consumed.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }

    /// The next complete part, `Ok(None)` while more bytes are needed.
    ///
    /// # Errors
    ///
    /// [`StreamError`] on malformed framing; the stream should be dropped.
    pub fn next_part(&mut self) -> Result<Option<Part>, StreamError> {
        loop {
            match &self.state {
                State::Header => {
                    let pending = &self.buf[self.start..];
                    let Some(end) = header_terminator(pending) else {
                        if pending.len() > MAX_HEADER_BYTES {
                            return Err(StreamError::HeaderTooLarge);
                        }
                        return Ok(None);
                    };
                    if end > MAX_HEADER_BYTES {
                        return Err(StreamError::HeaderTooLarge);
                    }
                    let (content_type, length) =
                        parse_header_block(&pending[..end], &self.boundary_line)?;
                    if length > MAX_PART_BYTES {
                        return Err(StreamError::PartTooLarge(length));
                    }
                    self.start += end;
                    self.state = State::Body {
                        content_type,
                        length,
                    };
                }
                State::Body {
                    content_type,
                    length,
                } => {
                    let length = *length;
                    if self.buffered() < length {
                        return Ok(None);
                    }
                    let body = self.buf[self.start..self.start + length].to_vec();
                    let content_type = content_type.clone();
                    self.start += length;
                    self.state = State::Header;
                    return Ok(Some(Part { content_type, body }));
                }
            }
        }
    }
}

/// Index just past the first `\r\n\r\n` or `\n\n`, mirroring the TS
/// `readUntilDoubleCrlf` state machine.
fn header_terminator(data: &[u8]) -> Option<usize> {
    let mut crlf = 0u8;
    let mut lf_only = 0u8;
    for (i, &b) in data.iter().enumerate() {
        crlf = match (crlf, b) {
            (0 | 2, b'\r') => crlf + 1,
            (1 | 3, b'\n') => {
                if crlf == 3 {
                    return Some(i + 1);
                }
                2
            }
            (_, b'\r') => 1,
            _ => 0,
        };
        if b == b'\n' {
            lf_only += 1;
            if lf_only == 2 {
                return Some(i + 1);
            }
        } else {
            lf_only = 0;
        }
    }
    None
}

fn parse_header_block(block: &[u8], boundary_line: &str) -> Result<(String, usize), StreamError> {
    let text = String::from_utf8_lossy(block);
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    // Tolerate blank lines before the boundary (some firmware sends a stray
    // CRLF after the previous payload).
    loop {
        match lines.next() {
            Some("") => {}
            Some(l) if l == boundary_line => break,
            Some(l) => {
                return Err(StreamError::UnexpectedBoundary(
                    l.chars().take(80).collect(),
                ));
            }
            None => return Err(StreamError::NoContentLength),
        }
    }
    let mut content_type = String::new();
    let mut length = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.eq_ignore_ascii_case("content-type") {
            content_type = value.trim().to_string();
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    length
        .map(|l| (content_type, l))
        .ok_or(StreamError::NoContentLength)
}

/// SPS and PPS NAL units (without start codes) found in a bitstream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpsPps {
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

impl SpsPps {
    /// Both found.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.sps.is_some() && self.pps.is_some()
    }

    /// Scans an Annex B buffer (`00 00 01` / `00 00 00 01` start codes) for
    /// the first SPS (type 7) and PPS (type 8) not yet found. Accumulates
    /// across parts; they arrive in-band before key frames.
    pub fn scan(&mut self, annexb: &[u8]) {
        for nal in nal_units(annexb) {
            match nal.first().map(|b| b & 0x1f) {
                Some(7) if self.sps.is_none() => self.sps = Some(nal.to_vec()),
                Some(8) if self.pps.is_none() => self.pps = Some(nal.to_vec()),
                _ => {}
            }
        }
    }
}

/// Whether an Annex B buffer has more than `max` start codes, without
/// allocating (bounds the work for a hostile part before splitting it).
#[must_use]
pub fn more_nals_than(data: &[u8], max: usize) -> bool {
    let mut count = 0;
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            count += 1;
            if count > max {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Splits an Annex B buffer into NAL units (start codes stripped). Bytes
/// before the first start code are skipped.
#[must_use]
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push((i, i + 3));
                i += 3;
                continue;
            }
            if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                starts.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(_, nal_start)) in starts.iter().enumerate() {
        let end = starts
            .get(k + 1)
            .map_or(data.len(), |&(code_at, _)| code_at);
        if nal_start < end {
            out.push(&data[nal_start..end]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(ct: &str, body: &[u8], eol: &str) -> Vec<u8> {
        let mut v = format!(
            "--bnd{eol}Content-Type: {ct}{eol}Content-Length: {}{eol}{eol}",
            body.len()
        )
        .into_bytes();
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn parses_interleaved_parts_fed_byte_by_byte() {
        let mut stream = part(MIME_H264, &[0, 0, 0, 1, 0x67, 1, 2], "\r\n");
        stream.extend_from_slice(b"\r\n");
        stream.extend(part(MIME_G711U, &[0xff; 160], "\r\n"));
        stream.extend(part("audio/x-other", &[], "\r\n"));
        let mut p = MultipartParser::new("bnd");
        let mut parts = Vec::new();
        for b in &stream {
            p.push(std::slice::from_ref(b));
            while let Some(part) = p.next_part().unwrap() {
                parts.push(part);
            }
        }
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].kind(), PartKind::Video);
        assert_eq!(parts[0].body, vec![0, 0, 0, 1, 0x67, 1, 2]);
        assert_eq!(parts[1].kind(), PartKind::Audio);
        assert_eq!(parts[1].body.len(), 160);
        assert_eq!(parts[2].kind(), PartKind::Other);
        assert_eq!(parts[2].body, b"");
        assert_eq!(p.buffered(), 0);
    }

    #[test]
    fn tolerates_lf_only_and_header_case() {
        let mut s = b"\n--bnd\ncontent-type: audio/G711U\nCONTENT-LENGTH: 3\n\n".to_vec();
        s.extend_from_slice(b"abc");
        let mut p = MultipartParser::new("bnd");
        p.push(&s);
        let part = p.next_part().unwrap().unwrap();
        assert_eq!(part.kind(), PartKind::Audio);
        assert_eq!(part.body, b"abc");
    }

    #[test]
    fn rejects_bad_framing() {
        let mut p = MultipartParser::new("bnd");
        p.push(b"--other\r\nContent-Length: 1\r\n\r\nx");
        assert!(matches!(
            p.next_part(),
            Err(StreamError::UnexpectedBoundary(_))
        ));

        let mut p = MultipartParser::new("bnd");
        p.push(b"--bnd\r\nContent-Type: video/x-h264\r\n\r\n");
        assert_eq!(p.next_part(), Err(StreamError::NoContentLength));

        let mut p = MultipartParser::new("bnd");
        p.push(b"--bnd\r\nContent-Length: -4\r\n\r\n");
        assert_eq!(p.next_part(), Err(StreamError::NoContentLength));

        let mut p = MultipartParser::new("bnd");
        p.push(&vec![b'a'; MAX_HEADER_BYTES + 1]);
        assert_eq!(p.next_part(), Err(StreamError::HeaderTooLarge));

        let mut p = MultipartParser::new("bnd");
        p.push(format!("--bnd\r\nContent-Length: {}\r\n\r\n", MAX_PART_BYTES + 1).as_bytes());
        assert_eq!(
            p.next_part(),
            Err(StreamError::PartTooLarge(MAX_PART_BYTES + 1))
        );
    }

    #[test]
    fn buffer_is_compacted_over_a_long_stream() {
        let mut p = MultipartParser::new("bnd");
        for _ in 0..1000 {
            p.push(&part(MIME_G711U, &[1; 160], "\r\n"));
            assert!(p.next_part().unwrap().is_some());
        }
        assert!(p.buf.len() < 4 * 200);
    }

    #[test]
    fn boundary_extraction() {
        assert_eq!(
            boundary_from_content_type("multipart/x-mixed-replace;boundary=data-boundary--")
                .unwrap(),
            "data-boundary--"
        );
        assert_eq!(
            boundary_from_content_type("Multipart/X-Mixed-Replace; Boundary=abc; charset=x")
                .unwrap(),
            "abc"
        );
        assert!(boundary_from_content_type("video/mp4").is_err());
        assert!(boundary_from_content_type("multipart/x-mixed-replace").is_err());
        assert!(boundary_from_content_type("multipart/x-mixed-replace;boundary=").is_err());
        let resp = |status, ct: &str| HttpResponse {
            status,
            headers: vec![("Content-Type".into(), ct.into())],
            body: vec![],
        };
        assert_eq!(
            boundary_from_response(&resp(503, "")),
            Err(StreamError::CameraOff)
        );
        assert_eq!(
            boundary_from_response(&resp(401, "")),
            Err(StreamError::Http(401))
        );
        assert_eq!(
            boundary_from_response(&resp(200, "multipart/x-mixed-replace;boundary=b")).unwrap(),
            "b"
        );
        let req = stream_request("10.0.0.5", 19443, "Basic s");
        assert_eq!(req.path, "/https/stream/mixed");
        assert_eq!(req.header("authorization"), Some("Basic s"));
    }

    #[test]
    fn finds_sps_and_pps_across_parts() {
        let mut found = SpsPps::default();
        found.scan(&[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x67, 0x4d, 0x00, 0x28]);
        assert!(!found.complete());
        assert_eq!(found.sps, Some(vec![0x67, 0x4d, 0x00, 0x28]));
        found.scan(&[0, 0, 1, 0x68, 0xee, 0x3c, 0x80, 0, 0, 0, 1, 0x65, 0x88]);
        assert!(found.complete());
        assert_eq!(found.pps, Some(vec![0x68, 0xee, 0x3c, 0x80]));
        // A later SPS doesn't replace the first.
        found.scan(&[0, 0, 1, 0x67, 0xff]);
        assert_eq!(found.sps, Some(vec![0x67, 0x4d, 0x00, 0x28]));
    }

    #[test]
    fn nal_splitting_edges() {
        assert_eq!(nal_units(&[]), Vec::<&[u8]>::new());
        assert_eq!(nal_units(&[1, 2, 3]), Vec::<&[u8]>::new());
        assert_eq!(nal_units(&[9, 0, 0, 1, 5, 6]), vec![&[5, 6][..]]);
        assert_eq!(nal_units(&[0, 0, 1, 0, 0, 1, 7]), vec![&[7][..]]);
        assert!(header_terminator(b"a\r\nb\r\n\r\n").is_some());
        assert_eq!(header_terminator(b"a\r\n\r"), None);
        assert_eq!(header_terminator(b"x\r\r\n\r\n"), Some(6));
    }
}
