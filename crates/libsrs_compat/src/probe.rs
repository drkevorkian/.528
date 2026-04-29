use std::path::Path;

use anyhow::Result;
use libsrs_contract::{CodecType, MediaKind, Packet, StreamId, StreamRole, Timebase, TrackId};

use crate::stub::{StubIngestor, StubProbe};

#[cfg(feature = "ffmpeg")]
use crate::ffmpeg_backend::{FfmpegIngestor, FfmpegProbe};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatBackend {
    Stub,
    #[cfg(feature = "ffmpeg")]
    Ffmpeg,
}

#[derive(Debug, Clone)]
pub struct CompatTrackInfo {
    pub id: TrackId,
    pub kind: MediaKind,
    pub codec: CodecType,
    pub role: StreamRole,
    pub language: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub format_name: String,
    pub duration_ms: Option<u64>,
    pub tracks: Vec<CompatTrackInfo>,
}

#[derive(Debug, Clone)]
pub struct SourcePacket {
    pub packet: Packet,
    pub source_offset: Option<u64>,
}

pub trait MediaProbe: Send + Sync {
    fn probe_path(&self, input: &Path) -> Result<ProbeResult>;
}

pub trait MediaIngestor: Send {
    fn open_path(&mut self, input: &Path) -> Result<()>;
    fn read_packet(&mut self) -> Result<Option<SourcePacket>>;
    fn seek_ms(&mut self, position_ms: u64) -> Result<()>;
    fn close(&mut self) -> Result<()>;
}

#[derive(Debug, Clone, Copy)]
pub struct CompatLayer {
    backend: CompatBackend,
}

impl Default for CompatLayer {
    fn default() -> Self {
        #[cfg(feature = "ffmpeg")]
        {
            return Self::new(CompatBackend::Ffmpeg);
        }
        #[cfg(not(feature = "ffmpeg"))]
        {
            Self::new(CompatBackend::Stub)
        }
    }
}

impl CompatLayer {
    pub const fn new(backend: CompatBackend) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> CompatBackend {
        self.backend
    }

    pub fn create_prober(&self) -> Box<dyn MediaProbe> {
        match self.backend {
            CompatBackend::Stub => Box::new(StubProbe),
            #[cfg(feature = "ffmpeg")]
            CompatBackend::Ffmpeg => Box::new(FfmpegProbe),
        }
    }

    pub fn create_ingestor(&self) -> Box<dyn MediaIngestor> {
        match self.backend {
            CompatBackend::Stub => Box::new(StubIngestor::new()),
            #[cfg(feature = "ffmpeg")]
            CompatBackend::Ffmpeg => Box::new(FfmpegIngestor::new()),
        }
    }

    pub fn synthetic_packet(seed: u8, pts_ms: i64) -> SourcePacket {
        SourcePacket {
            packet: Packet {
                stream_id: StreamId(0),
                pts: Some(libsrs_contract::Timestamp::new(
                    pts_ms,
                    Timebase::milliseconds(),
                )),
                dts: None,
                duration: None,
                keyframe: true,
                data: vec![seed; 64],
            },
            source_offset: Some(0),
        }
    }
}
