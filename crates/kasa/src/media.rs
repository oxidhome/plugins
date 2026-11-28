//! Live video: the camera's mixed multipart stream re-packetized as RTP and
//! written into `host-media` plugin pipes.
//!
//! This is written against the WIT `media` interface of the
//! `streaming-plugin` world. [`mirror`] restates the subset of its types
//! this plugin produces, with value semantics (`PartialEq`) the generated
//! bindings don't have, and [`to_wit`] converts a pipeline for the
//! `setup-pipeline` export. It can't run end to end until `OxidHome` ships
//! media (Phase 9): the host's `host-media` is a
//! placeholder that answers `unavailable`, and several pieces have no WIT
//! counterpart yet (see the README's "Platform gaps"): the pipeline
//! returned by `setup-pipeline` and the writer from `open-pipe` aren't
//! linked, a pipeline has one source while the camera muxes video and
//! audio on its one connection, and the plugin is told nothing when
//! consumers come and go.

pub use mirror::{
    ActivationPolicy, AudioCodec, ContainerFormat, MediaCodec, MediaPipeline, MediaSource,
    OutputSink, PipeFormat, PipelineStep, PluginPipeConfig, VideoCodec,
};

/// The subset of `oxidhome:plugin/media@0.1.0` this plugin produces, same
/// names and shapes as the WIT; [`to_wit`] maps them one
/// to one.
#[allow(missing_docs)]
pub mod mirror {
    /// `pipe-format`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PipeFormat {
        Rtp,
    }

    /// `video-codec`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum VideoCodec {
        H264,
    }

    /// `audio-codec`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AudioCodec {
        PcmMulaw,
    }

    /// `media-codec`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MediaCodec {
        Video(VideoCodec),
        Audio(AudioCodec),
    }

    /// `plugin-pipe-config`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PluginPipeConfig {
        pub format: PipeFormat,
        pub codec: MediaCodec,
        pub clock_rate: Option<u32>,
        pub sample_rate: Option<u32>,
        pub channels: Option<u8>,
    }

    /// `media-source`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum MediaSource {
        PluginPipe(PluginPipeConfig),
    }

    /// `container-format`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ContainerFormat {
        Rtp,
    }

    /// `pipeline-step`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum PipelineStep {
        Demux(ContainerFormat),
    }

    /// `output-sink`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum OutputSink {
        Webrtc,
        Rtsp(String),
    }

    /// `activation-policy`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ActivationPolicy {
        OnDemand,
    }

    /// `media-pipeline`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MediaPipeline {
        pub source: MediaSource,
        pub steps: Vec<PipelineStep>,
        pub sinks: Vec<OutputSink>,
        pub activation: ActivationPolicy,
    }
}
use crate::platform::{ByteStream, NetError, PipeSink};
use crate::protocol::multipart::{MultipartParser, PartKind, SpsPps, StreamError, more_nals_than};
use crate::protocol::rtp::{
    G711_CLOCK_HZ, G711Packetizer, H264_CLOCK_HZ, H264Packetizer, rtp_ticks,
};
use oxidhome_sdk::streaming::media as wit;

/// Most bytes read from the stream per tick.
pub const MAX_READ_PER_TICK: usize = 1024 * 1024;
/// Video parts with more NAL units than this are dropped.
pub const MAX_NALS_PER_PART: usize = 256;

/// The video pipe: RTP-framed H.264 at 90 kHz.
#[must_use]
pub fn video_pipe_config() -> PluginPipeConfig {
    PluginPipeConfig {
        format: PipeFormat::Rtp,
        codec: MediaCodec::Video(VideoCodec::H264),
        clock_rate: Some(u32::try_from(H264_CLOCK_HZ).unwrap_or(90_000)),
        sample_rate: None,
        channels: None,
    }
}

/// The audio pipe: RTP-framed PCMU, 8 kHz mono.
#[must_use]
pub fn audio_pipe_config() -> PluginPipeConfig {
    PluginPipeConfig {
        format: PipeFormat::Rtp,
        codec: MediaCodec::Audio(AudioCodec::PcmMulaw),
        clock_rate: Some(8_000),
        sample_rate: Some(8_000),
        channels: Some(1),
    }
}

/// A sink name from a camera id: `[a-z0-9_-]{1,64}`. The readable part is
/// lossy (case, punctuation, length), so a hash of the full id keeps two
/// cameras from sharing a sink or a snapshot blob.
#[must_use]
pub fn sink_name(kasa_id: &str) -> String {
    // FNV-1a: stable across builds, which `DefaultHasher` isn't.
    let hash = kasa_id.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    let mut s: String = format!("kasa-{kasa_id}")
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    s.truncate(64 - 9);
    format!("{s}-{hash:08x}")
}

/// What `setup-pipeline` returns for a camera: the video plugin pipe,
/// depacketized by the host, served over WebRTC and RTSP, on demand (the
/// camera serves one live stream, so nothing should hold it open while no
/// one watches).
#[must_use]
pub fn camera_pipeline(kasa_id: &str) -> MediaPipeline {
    MediaPipeline {
        source: MediaSource::PluginPipe(video_pipe_config()),
        steps: vec![PipelineStep::Demux(ContainerFormat::Rtp)],
        sinks: vec![OutputSink::Webrtc, OutputSink::Rtsp(sink_name(kasa_id))],
        activation: ActivationPolicy::OnDemand,
    }
}

/// `pipe` as the WIT `plugin-pipe-config`.
#[must_use]
pub fn to_wit_pipe_config(pipe: &PluginPipeConfig) -> wit::PluginPipeConfig {
    wit::PluginPipeConfig {
        format: match pipe.format {
            PipeFormat::Rtp => wit::PipeFormat::Rtp,
        },
        codec: match pipe.codec {
            MediaCodec::Video(VideoCodec::H264) => wit::MediaCodec::Video(wit::VideoCodec::H264),
            MediaCodec::Audio(AudioCodec::PcmMulaw) => {
                wit::MediaCodec::Audio(wit::AudioCodec::PcmMulaw)
            }
        },
        clock_rate: pipe.clock_rate,
        sample_rate: pipe.sample_rate,
        channels: pipe.channels,
    }
}

/// `pipeline` as the WIT `media-pipeline`, for the `setup-pipeline` export.
#[must_use]
pub fn to_wit(pipeline: &MediaPipeline) -> wit::MediaPipeline {
    wit::MediaPipeline {
        source: match &pipeline.source {
            MediaSource::PluginPipe(p) => wit::MediaSource::PluginPipe(to_wit_pipe_config(p)),
        },
        steps: pipeline
            .steps
            .iter()
            .map(|s| match s {
                PipelineStep::Demux(ContainerFormat::Rtp) => {
                    wit::PipelineStep::Demux(wit::ContainerFormat::Rtp)
                }
            })
            .collect(),
        sinks: pipeline
            .sinks
            .iter()
            .map(|s| match s {
                OutputSink::Webrtc => wit::OutputSink::Webrtc,
                OutputSink::Rtsp(name) => wit::OutputSink::Rtsp(name.clone()),
            })
            .collect(),
        activation: match pipeline.activation {
            ActivationPolicy::OnDemand => wit::ActivationPolicy::OnDemand,
        },
    }
}

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    Net(NetError),
    Stream(StreamError),
    Pipe(String),
}

impl std::fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionEnd::Net(e) => write!(f, "{e}"),
            SessionEnd::Stream(e) => write!(f, "{e}"),
            SessionEnd::Pipe(e) => write!(f, "pipe: {e}"),
        }
    }
}

/// One live stream from one camera into two pipes.
pub struct StreamSession {
    body: Box<dyn ByteStream>,
    parser: MultipartParser,
    video: H264Packetizer,
    audio: G711Packetizer,
    video_pipe: Box<dyn PipeSink>,
    audio_pipe: Box<dyn PipeSink>,
    started_ms: u64,
    audio_ts: Option<u32>,
    /// SPS/PPS seen so far (for a future SDP or cache; the TS plugin
    /// caches them per camera for a fast start).
    pub sps_pps: SpsPps,
    /// Parts forwarded, for logs and tests.
    pub parts: u64,
}

impl StreamSession {
    /// Starts a session on an open stream body.
    #[must_use]
    pub fn new(
        boundary: &str,
        body: Box<dyn ByteStream>,
        video_pipe: Box<dyn PipeSink>,
        audio_pipe: Box<dyn PipeSink>,
        started_ms: u64,
        seeds: [u32; 4],
    ) -> Self {
        let [vseq, vssrc, aseq, assrc] = seeds;
        Self {
            body,
            parser: MultipartParser::new(boundary),
            video: H264Packetizer::new(u16::try_from(vseq & 0xffff).unwrap_or(0), vssrc),
            audio: G711Packetizer::new(u16::try_from(aseq & 0xffff).unwrap_or(0), assrc),
            video_pipe,
            audio_pipe,
            started_ms,
            audio_ts: None,
            sps_pps: SpsPps::default(),
            parts: 0,
        }
    }

    /// Reads what's available and forwards complete parts. Parts are
    /// stamped with the time they were pulled (`now_ms`); audio then
    /// advances by sample count.
    ///
    /// # Errors
    ///
    /// [`SessionEnd`]; the caller drops the session and closes the pipes.
    pub fn pump(&mut self, now_ms: u64) -> Result<(), SessionEnd> {
        // A read may return less than is ready (WASI allows short reads),
        // so keep reading until the stream is drained or the budget is
        // spent.
        let mut budget = MAX_READ_PER_TICK;
        let mut ended = None;
        while budget > 0 {
            match self.body.read_available(budget) {
                Ok(data) if data.is_empty() => break,
                Ok(data) => {
                    budget = budget.saturating_sub(data.len());
                    self.parser.push(&data);
                }
                // Forward what already arrived before ending the session.
                Err(e) => {
                    ended = Some(SessionEnd::Net(e));
                    break;
                }
            }
        }
        let elapsed_us = now_ms.saturating_sub(self.started_ms).saturating_mul(1000);
        while let Some(part) = self.parser.next_part().map_err(SessionEnd::Stream)? {
            self.parts += 1;
            match part.kind() {
                PartKind::Video => {
                    // A real access unit has a handful of NAL units; a part
                    // with thousands would make the packetizer allocate one
                    // buffer per slice.
                    if more_nals_than(&part.body, MAX_NALS_PER_PART) {
                        continue;
                    }
                    if !self.sps_pps.complete() {
                        self.sps_pps.scan(&part.body);
                    }
                    let ts = rtp_ticks(elapsed_us, H264_CLOCK_HZ);
                    for pkt in self.video.packetize(&part.body, ts) {
                        self.video_pipe
                            .write(&pkt)
                            .map_err(|e| SessionEnd::Pipe(format!("{e:?}")))?;
                    }
                }
                PartKind::Audio => {
                    let start = *self
                        .audio_ts
                        .get_or_insert_with(|| rtp_ticks(elapsed_us, G711_CLOCK_HZ));
                    let (pkts, next) = self.audio.packetize(&part.body, start);
                    self.audio_ts = Some(next);
                    for pkt in pkts {
                        self.audio_pipe
                            .write(&pkt)
                            .map_err(|e| SessionEnd::Pipe(format!("{e:?}")))?;
                    }
                }
                PartKind::Other => {}
            }
        }
        match ended {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Closes both pipes.
    pub fn close(&mut self) {
        self.video_pipe.close();
        self.audio_pipe.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidhome_sdk::bindings::oxidhome::plugin::types::Error;
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Chunks(Vec<Vec<u8>>);
    impl ByteStream for Chunks {
        fn read_available(&mut self, _max: usize) -> Result<Vec<u8>, NetError> {
            if self.0.is_empty() {
                Err(NetError::Sent("eof".into()))
            } else {
                Ok(self.0.remove(0))
            }
        }
    }

    #[derive(Clone, Default)]
    struct Pipe(Rc<RefCell<(Vec<Vec<u8>>, bool)>>);
    impl PipeSink for Pipe {
        fn write(&mut self, data: &[u8]) -> Result<(), Error> {
            self.0.borrow_mut().0.push(data.to_vec());
            Ok(())
        }
        fn close(&mut self) {
            self.0.borrow_mut().1 = true;
        }
    }

    fn part(ct: &str, body: &[u8]) -> Vec<u8> {
        let mut v = format!(
            "--b\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn forwards_video_and_audio_as_rtp() {
        let mut stream = part(
            "video/x-h264",
            &[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 3],
        );
        stream.extend(part("audio/g711u", &[0x7f; 320]));
        let split = stream.len() / 2;
        // An empty chunk is "nothing ready yet": one chunk per pump.
        let body = Chunks(vec![
            stream[..split].to_vec(),
            Vec::new(),
            stream[split..].to_vec(),
            Vec::new(),
        ]);
        let (v, a) = (Pipe::default(), Pipe::default());
        let mut s = StreamSession::new(
            "b",
            Box::new(body),
            Box::new(v.clone()),
            Box::new(a.clone()),
            1_000,
            [1, 2, 3, 4],
        );
        s.pump(1_000).unwrap();
        s.pump(2_000).unwrap();
        assert_eq!(s.parts, 2);
        assert!(s.sps_pps.complete());
        assert_eq!(v.0.borrow().0.len(), 3);
        {
            let audio = &a.0.borrow().0;
            assert_eq!(audio.len(), 2);
            // Audio started at 1 s elapsed → 8000 ticks, then +160.
            assert_eq!(&audio[0][4..8], &8_000u32.to_be_bytes());
            assert_eq!(&audio[1][4..8], &8_160u32.to_be_bytes());
        }
        assert!(matches!(s.pump(3_000), Err(SessionEnd::Net(_))));
        s.close();
        assert!(v.0.borrow().1 && a.0.borrow().1);
    }

    #[test]
    fn malformed_stream_ends_the_session() {
        let body = Chunks(vec![b"--x\r\nContent-Length: 1\r\n\r\nz".to_vec()]);
        let mut s = StreamSession::new(
            "b",
            Box::new(body),
            Box::new(Pipe::default()),
            Box::new(Pipe::default()),
            0,
            [0; 4],
        );
        let e = s.pump(0).unwrap_err();
        assert!(matches!(
            e,
            SessionEnd::Stream(StreamError::UnexpectedBoundary(_))
        ));
        assert!(e.to_string().contains("unexpected boundary"));
    }

    #[test]
    fn pipeline_description() {
        let p = camera_pipeline("80DE:AD");
        assert!(matches!(
            p.source,
            MediaSource::PluginPipe(PluginPipeConfig {
                format: PipeFormat::Rtp,
                ..
            })
        ));
        assert!(matches!(&p.sinks[1], OutputSink::Rtsp(n) if n.starts_with("kasa-80de-ad-")));
        assert_ne!(sink_name("cam.1"), sink_name("cam-1"));
        assert!(matches!(p.activation, ActivationPolicy::OnDemand));
        assert_eq!(sink_name(&"x".repeat(100)).len(), 64);
        assert!(matches!(
            audio_pipe_config().codec,
            MediaCodec::Audio(AudioCodec::PcmMulaw)
        ));
    }

    #[test]
    fn pipeline_converts_to_the_wit_types() {
        let p = to_wit(&camera_pipeline("CAM1"));
        let wit::MediaSource::PluginPipe(src) = &p.source else {
            panic!("expected a plugin pipe: {:?}", p.source);
        };
        assert!(matches!(src.format, wit::PipeFormat::Rtp));
        assert!(matches!(
            src.codec,
            wit::MediaCodec::Video(wit::VideoCodec::H264)
        ));
        assert_eq!(src.clock_rate, Some(90_000));
        assert!(matches!(
            p.steps[..],
            [wit::PipelineStep::Demux(wit::ContainerFormat::Rtp)]
        ));
        assert!(
            matches!(&p.sinks[..], [wit::OutputSink::Webrtc, wit::OutputSink::Rtsp(n)] if *n == sink_name("CAM1"))
        );
        assert!(matches!(p.activation, wit::ActivationPolicy::OnDemand));

        let audio = to_wit_pipe_config(&audio_pipe_config());
        assert!(matches!(
            audio.codec,
            wit::MediaCodec::Audio(wit::AudioCodec::PcmMulaw)
        ));
        assert_eq!(
            (audio.clock_rate, audio.sample_rate, audio.channels),
            (Some(8_000), Some(8_000), Some(1))
        );
    }
}
