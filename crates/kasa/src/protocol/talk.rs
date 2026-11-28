//! Talkback (two-way audio): one long-lived chunked
//! `POST https://<camera>:18443/https/speaker/audio/g711block?playerId=<id>`
//! per session, control credentials (MD5 encoding).
//!
//! The body is `multipart/x-mixed-replace;boundary=audio-boundary--` and
//! stays open for the session. Audio is G.711 µ-law 8 kHz mono in **20 ms
//! blocks of 160 bytes**, sent in real time (the Kasa app's cadence; don't
//! aggregate). Each part (header, payload, trailing CRLF) goes in one write
//! so it arrives as one chunk. An empty `audio/heartbeat` part is sent at
//! the start and every 3 s so the camera keeps the connection through
//! silence. Without Nagle disabled, small parts get delayed and the audio
//! turns choppy.

use super::http::{HttpRequest, Method};

/// Multipart boundary the Kasa app uses.
pub const BOUNDARY: &str = "audio-boundary--";
/// Talk path.
pub const TALK_PATH: &str = "/https/speaker/audio/g711block";
/// Bytes per 20 ms block at 8 kHz µ-law.
pub const CHUNK_BYTES: usize = 160;
/// Heartbeat cadence.
pub const HEARTBEAT_MS: u64 = 3_000;

/// A player id: `tty.` and 32 alphanumeric characters, minted per session
/// by the client (not negotiated).
#[must_use]
pub fn player_id(random: &[u8; 32]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::with_capacity(36);
    s.push_str("tty.");
    for b in random {
        s.push(char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]));
    }
    s
}

/// The request head of a talk session. The body is streamed separately
/// with [`audio_part`] / [`heartbeat_part`] chunks.
#[must_use]
pub fn talk_request(host: &str, port: u16, control_auth: &str, player_id: &str) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        host: host.into(),
        port,
        path: format!("{TALK_PATH}?playerId={player_id}"),
        headers: vec![
            ("Authorization".into(), control_auth.into()),
            ("X-APP-ID".into(), player_id.into()),
            (
                "Content-Type".into(),
                format!("multipart/x-mixed-replace;boundary={BOUNDARY}"),
            ),
            ("Transfer-Encoding".into(), "chunked".into()),
            ("Connection".into(), "keep-alive".into()),
            ("Accept".into(), "*/*".into()),
        ],
        body: Vec::new(),
    }
}

fn part_header(content_type: &str, len: usize) -> Vec<u8> {
    format!("--{BOUNDARY}\r\nContent-Length: {len}\r\nContent-Type: {content_type}\r\n\r\n")
        .into_bytes()
}

/// One audio part: header, payload, CRLF, as one buffer.
#[must_use]
pub fn audio_part(g711u: &[u8]) -> Vec<u8> {
    let mut v = part_header("audio/g711u", g711u.len());
    v.extend_from_slice(g711u);
    v.extend_from_slice(b"\r\n");
    v
}

/// The empty heartbeat part.
#[must_use]
pub fn heartbeat_part() -> Vec<u8> {
    let mut v = part_header("audio/heartbeat", 0);
    v.extend_from_slice(b"\r\n");
    v
}

/// One HTTP/1.1 chunk (for a transport that writes the chunked encoding
/// itself rather than letting an HTTP stack do it).
#[must_use]
pub fn http_chunk(data: &[u8]) -> Vec<u8> {
    let mut v = format!("{:x}\r\n", data.len()).into_bytes();
    v.extend_from_slice(data);
    v.extend_from_slice(b"\r\n");
    v
}

/// Re-blocks arbitrary µ-law input into 160-byte parts and interleaves
/// heartbeats. Byte-counting queue, so a long session is O(n).
#[derive(Debug, Default)]
pub struct TalkFramer {
    pending: std::collections::VecDeque<u8>,
    last_heartbeat_ms: Option<u64>,
}

impl TalkFramer {
    /// A fresh framer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues decoded µ-law samples.
    pub fn push_audio(&mut self, g711u: &[u8]) {
        self.pending.extend(g711u);
    }

    /// Parts to send at monotonic `now_ms`: a heartbeat when one is due
    /// (first call, then every [`HEARTBEAT_MS`]) and every full block.
    /// Pacing the blocks in real time is the caller's job.
    pub fn drain(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if self
            .last_heartbeat_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= HEARTBEAT_MS)
        {
            self.last_heartbeat_ms = Some(now_ms);
            out.push(heartbeat_part());
        }
        while self.pending.len() >= CHUNK_BYTES {
            let block: Vec<u8> = self.pending.drain(..CHUNK_BYTES).collect();
            out.push(audio_part(&block));
        }
        out
    }

    /// Samples waiting for a full block.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_framing_matches_the_protocol_doc() {
        assert_eq!(
            heartbeat_part(),
            b"--audio-boundary--\r\nContent-Length: 0\r\nContent-Type: audio/heartbeat\r\n\r\n\r\n"
        );
        let p = audio_part(&[0x7f; 160]);
        let head =
            b"--audio-boundary--\r\nContent-Length: 160\r\nContent-Type: audio/g711u\r\n\r\n";
        assert!(p.starts_with(head));
        assert_eq!(p.len(), head.len() + 160 + 2);
        assert!(p.ends_with(b"\x7f\r\n"));
    }

    #[test]
    fn request_head() {
        let id = player_id(&[0; 32]);
        assert_eq!(id.len(), 36);
        assert!(id.starts_with("tty.AAAA"));
        let id2 = player_id(&[61; 32]);
        assert!(id2.ends_with('9'));
        let r = talk_request("10.0.0.5", 18443, "Basic c", &id);
        assert_eq!(
            r.path,
            format!("/https/speaker/audio/g711block?playerId={id}")
        );
        assert_eq!(r.header("x-app-id"), Some(id.as_str()));
        assert_eq!(
            r.header("content-type"),
            Some("multipart/x-mixed-replace;boundary=audio-boundary--")
        );
        assert_eq!(r.header("transfer-encoding"), Some("chunked"));
    }

    #[test]
    fn chunk_encoding() {
        assert_eq!(http_chunk(b"abc"), b"3\r\nabc\r\n");
        assert_eq!(http_chunk(&[0; 0xa0])[..4], *b"a0\r\n");
    }

    #[test]
    fn framer_blocks_audio_and_paces_heartbeats() {
        let mut f = TalkFramer::new();
        f.push_audio(&[1; 100]);
        let first = f.drain(0);
        assert_eq!(first, vec![heartbeat_part()]);
        assert_eq!(f.pending(), 100);
        f.push_audio(&[2; 300]);
        let parts = f.drain(1_000);
        assert_eq!(parts.len(), 2);
        assert_eq!(f.pending(), 80);
        // The first block straddles both pushes.
        let body = &parts[0][parts[0].len() - 162..parts[0].len() - 2];
        assert_eq!(&body[..100], &[1; 100][..]);
        assert_eq!(&body[100..], &[2; 60][..]);
        assert_eq!(f.drain(2_999), Vec::<Vec<u8>>::new());
        assert_eq!(f.drain(3_000), vec![heartbeat_part()]);
    }
}
