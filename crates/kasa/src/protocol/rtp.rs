//! RTP packetization of the camera's elementary streams: H.264 (RFC 6184,
//! single NAL unit + FU-A) and G.711 µ-law (RFC 3551, PT 0), plus RTCP
//! sender reports. Ported from the TS plugin's `rtp.ts`; it is what a
//! `plugin-pipe` with `pipe-format::rtp` needs, since multipart parts carry
//! no timestamps.
//!
//! Timing (from the TS stream loop): video parts are stamped with the
//! elapsed arrival time at 90 kHz; audio starts at the elapsed time at
//! 8 kHz on its first part and then advances by **sample count** (1 byte =
//! 1 sample), since wall-clock jitter on audio is audible.

use super::multipart::nal_units;

/// Dynamic payload type for H.264.
pub const PT_H264: u8 = 96;
/// Static payload type for PCMU.
pub const PT_PCMU: u8 = 0;
/// H.264 RTP clock.
pub const H264_CLOCK_HZ: u64 = 90_000;
/// G.711 RTP clock.
pub const G711_CLOCK_HZ: u64 = 8_000;
/// FU-A cutoff, keeping packets under a 1500-byte Ethernet MTU.
pub const PAYLOAD_MTU: usize = 1400;
/// 20 ms of µ-law.
pub const G711_SAMPLES_PER_PACKET: usize = 160;

const HEADER_LEN: usize = 12;
const FU_A_OVERHEAD: usize = 2;
/// Seconds between the NTP (1900) and Unix (1970) epochs.
const NTP_UNIX_OFFSET_S: u64 = 2_208_988_800;

/// An NTP 64-bit timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtpTime {
    pub seconds: u32,
    pub fraction: u32,
}

impl NtpTime {
    /// From Unix milliseconds.
    #[must_use]
    pub fn from_unix_ms(ms: u64) -> Self {
        let secs = ms / 1000 + NTP_UNIX_OFFSET_S;
        // (ms % 1000) / 1000 * 2^32, floored, always < 2^32.
        let fraction = ((ms % 1000) << 32) / 1000;
        Self {
            seconds: u32::try_from(secs & 0xffff_ffff).unwrap_or(0),
            fraction: u32::try_from(fraction).unwrap_or(u32::MAX),
        }
    }
}

/// An RTP timestamp at `clock_hz` for `elapsed_us` microseconds, wrapping
/// at 32 bits.
#[must_use]
pub fn rtp_ticks(elapsed_us: u64, clock_hz: u64) -> u32 {
    let ticks = u128::from(elapsed_us) * u128::from(clock_hz) / 1_000_000;
    u32::try_from(ticks & 0xffff_ffff).unwrap_or(0)
}

fn header(pt: u8, seq: u16, ts: u32, ssrc: u32, marker: bool, payload_len: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEADER_LEN + payload_len);
    b.push(2 << 6);
    b.push(if marker { 0x80 } else { 0 } | (pt & 0x7f));
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(&ts.to_be_bytes());
    b.extend_from_slice(&ssrc.to_be_bytes());
    b
}

#[derive(Debug, Clone)]
struct Counters {
    seq: u16,
    ssrc: u32,
    packets: u32,
    octets: u32,
    last_ts: u32,
}

impl Counters {
    fn emit(&mut self, payload_len: usize) -> u16 {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        self.packets = self.packets.wrapping_add(1);
        self.octets = self
            .octets
            .wrapping_add(u32::try_from(payload_len).unwrap_or(u32::MAX));
        seq
    }

    fn sender_report(&self, ntp: NtpTime) -> Option<Vec<u8>> {
        if self.packets == 0 {
            return None;
        }
        let mut b = Vec::with_capacity(28);
        b.push(2 << 6);
        b.push(200);
        b.extend_from_slice(&6u16.to_be_bytes());
        b.extend_from_slice(&self.ssrc.to_be_bytes());
        b.extend_from_slice(&ntp.seconds.to_be_bytes());
        b.extend_from_slice(&ntp.fraction.to_be_bytes());
        b.extend_from_slice(&self.last_ts.to_be_bytes());
        b.extend_from_slice(&self.packets.to_be_bytes());
        b.extend_from_slice(&self.octets.to_be_bytes());
        Some(b)
    }
}

/// H.264 packetizer for one stream session.
#[derive(Debug, Clone)]
pub struct H264Packetizer {
    c: Counters,
}

impl H264Packetizer {
    /// `seq` and `ssrc` should be random per session.
    #[must_use]
    pub fn new(seq: u16, ssrc: u32) -> Self {
        Self {
            c: Counters {
                seq,
                ssrc,
                packets: 0,
                octets: 0,
                last_ts: 0,
            },
        }
    }

    /// SSRC of the stream.
    #[must_use]
    pub fn ssrc(&self) -> u32 {
        self.c.ssrc
    }

    /// Packets for one access unit (one video part). All share `ts`; the
    /// marker is set on the last packet.
    pub fn packetize(&mut self, annexb: &[u8], ts: u32) -> Vec<Vec<u8>> {
        let nals = nal_units(annexb);
        let mut out = Vec::new();
        for (i, nal) in nals.iter().enumerate() {
            let last_nal = i + 1 == nals.len();
            if nal.len() <= PAYLOAD_MTU {
                let seq = self.c.emit(nal.len());
                let mut p = header(PT_H264, seq, ts, self.c.ssrc, last_nal, nal.len());
                p.extend_from_slice(nal);
                out.push(p);
                continue;
            }
            let indicator = (nal[0] & 0xe0) | 0x1c;
            let ty = nal[0] & 0x1f;
            let payload = &nal[1..];
            let chunk = PAYLOAD_MTU - FU_A_OVERHEAD;
            let mut off = 0;
            while off < payload.len() {
                let take = chunk.min(payload.len() - off);
                let first = off == 0;
                let last = off + take == payload.len();
                let fu = if first { 0x80 } else { 0 } | if last { 0x40 } else { 0 } | ty;
                let seq = self.c.emit(FU_A_OVERHEAD + take);
                let mut p = header(
                    PT_H264,
                    seq,
                    ts,
                    self.c.ssrc,
                    last_nal && last,
                    FU_A_OVERHEAD + take,
                );
                p.push(indicator);
                p.push(fu);
                p.extend_from_slice(&payload[off..off + take]);
                out.push(p);
                off += take;
            }
        }
        if !out.is_empty() {
            self.c.last_ts = ts;
        }
        out
    }

    /// RTCP SR for the latest emission; `None` before the first packet.
    #[must_use]
    pub fn sender_report(&self, ntp: NtpTime) -> Option<Vec<u8>> {
        self.c.sender_report(ntp)
    }
}

/// G.711 µ-law packetizer for one stream session.
#[derive(Debug, Clone)]
pub struct G711Packetizer {
    c: Counters,
}

impl G711Packetizer {
    /// `seq` and `ssrc` should be random per session.
    #[must_use]
    pub fn new(seq: u16, ssrc: u32) -> Self {
        Self {
            c: Counters {
                seq,
                ssrc,
                packets: 0,
                octets: 0,
                last_ts: 0,
            },
        }
    }

    /// 20 ms packets for `mulaw` starting at `ts`; returns the timestamp of
    /// the next sample, to chain across parts.
    pub fn packetize(&mut self, mulaw: &[u8], ts: u32) -> (Vec<Vec<u8>>, u32) {
        let mut out = Vec::new();
        let mut ts = ts;
        for block in mulaw.chunks(G711_SAMPLES_PER_PACKET) {
            let seq = self.c.emit(block.len());
            let mut p = header(PT_PCMU, seq, ts, self.c.ssrc, false, block.len());
            p.extend_from_slice(block);
            out.push(p);
            self.c.last_ts = ts;
            ts = ts.wrapping_add(u32::try_from(block.len()).unwrap_or(0));
        }
        (out, ts)
    }

    /// RTCP SR for the latest emission; `None` before the first packet.
    #[must_use]
    pub fn sender_report(&self, ntp: NtpTime) -> Option<Vec<u8>> {
        self.c.sender_report(ntp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_nals_are_single_packets_with_marker_on_the_last() {
        let mut p = H264Packetizer::new(0xfffe, 0xdead_beef);
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4, 5,
        ];
        let pkts = p.packetize(&au, 9000);
        assert_eq!(pkts.len(), 3);
        assert_eq!(
            pkts[0][..12],
            [
                0x80, 96, 0xff, 0xfe, 0, 0, 0x23, 0x28, 0xde, 0xad, 0xbe, 0xef
            ]
        );
        assert_eq!(&pkts[0][12..], &[0x67, 1, 2]);
        assert_eq!(pkts[1][1], 96);
        assert_eq!(pkts[2][1], 0x80 | 0x60);
        // Sequence wrapped.
        assert_eq!(&pkts[2][2..4], &[0, 0]);
        assert_eq!(p.packetize(&[1, 2, 3], 1), Vec::<Vec<u8>>::new());
        assert_eq!(p.ssrc(), 0xdead_beef);
    }

    #[test]
    fn large_nals_are_fu_a_fragmented() {
        let mut nal = vec![0x65];
        nal.extend((0..3000u32).map(|i| u8::try_from(i % 251).unwrap()));
        let mut au = vec![0, 0, 1];
        au.extend_from_slice(&nal);
        let mut p = H264Packetizer::new(0, 1);
        let pkts = p.packetize(&au, 0);
        assert_eq!(pkts.len(), 3);
        assert_eq!(pkts[0][12], (0x65 & 0xe0) | 0x1c);
        assert_eq!(pkts[0][13], 0x80 | 5);
        assert_eq!(pkts[1][13], 5);
        assert_eq!(pkts[2][13], 0x40 | 5);
        assert_eq!(pkts[2][1] & 0x80, 0x80);
        assert_eq!(pkts[0][1] & 0x80, 0);
        // Reassembly gives back the NAL.
        let mut back = vec![(pkts[0][12] & 0xe0) | (pkts[0][13] & 0x1f)];
        for pk in &pkts {
            back.extend_from_slice(&pk[14..]);
        }
        assert_eq!(back, nal);
        assert!(pkts.iter().all(|pk| pk.len() - 12 <= PAYLOAD_MTU));
    }

    #[test]
    fn g711_advances_by_sample_count() {
        let mut p = G711Packetizer::new(10, 2);
        let (pkts, next) = p.packetize(&[0x55; 400], u32::MAX - 100);
        assert_eq!(pkts.len(), 3);
        assert_eq!(pkts[2].len(), 12 + 80);
        assert_eq!(next, 299);
        assert_eq!(pkts[0][1], 0);
        let sr = p.sender_report(NtpTime::from_unix_ms(0)).unwrap();
        assert_eq!(sr.len(), 28);
        assert_eq!(sr[1], 200);
        assert_eq!(&sr[20..24], &3u32.to_be_bytes());
        assert_eq!(&sr[24..28], &400u32.to_be_bytes());
        assert!(
            G711Packetizer::new(0, 0)
                .sender_report(NtpTime::from_unix_ms(0))
                .is_none()
        );
        let h = H264Packetizer::new(0, 0);
        assert!(h.sender_report(NtpTime::from_unix_ms(0)).is_none());
    }

    #[test]
    fn clocks() {
        assert_eq!(rtp_ticks(1_000_000, H264_CLOCK_HZ), 90_000);
        assert_eq!(rtp_ticks(20_000, G711_CLOCK_HZ), 160);
        let ntp = NtpTime::from_unix_ms(1_500);
        assert_eq!(ntp.seconds, 2_208_988_801);
        assert_eq!(ntp.fraction, 1 << 31);
        assert!(NtpTime::from_unix_ms(999).fraction < u32::MAX);
    }
}
