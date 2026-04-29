//! CPU-normalized decoded frames and extension points for future GPU encode paths.
//!
//! Implementations today are **CPU-only**: decode via `libsrs_video` / `libsrs_audio`, re-encode in
//! `libsrs_app_services` (mux writers). No GPU kernels are linked; [`GpuEncodeDispatch`] exists so
//! a future encoder can advertise hardware paths without changing sink call sites.

use anyhow::Result;
use libsrs_audio::AudioFrame;
use libsrs_video::{FrameType, VideoFrame};

/// Grayscale 8-bit decoded video suitable for native SRS re-encode.
pub trait DecodedVideoFrame {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn frame_index(&self) -> u32;
    fn frame_type(&self) -> FrameType;
    /// Planar packed grayscale (`width * height` bytes).
    fn gray8_pixels(&self) -> &[u8];
}

/// Decoded PCM audio suitable for native SRS re-encode (`samples` are interleaved `i16`).
pub trait DecodedAudioFrame {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u8;
    fn frame_index(&self) -> u32;
    fn samples_i16_interleaved(&self) -> &[i16];
}

impl DecodedVideoFrame for VideoFrame {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn frame_index(&self) -> u32 {
        self.frame_index
    }

    fn frame_type(&self) -> FrameType {
        self.frame_type
    }

    fn gray8_pixels(&self) -> &[u8] {
        &self.data
    }
}

impl DecodedAudioFrame for AudioFrame {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn frame_index(&self) -> u32 {
        self.frame_index
    }

    fn samples_i16_interleaved(&self) -> &[i16] {
        &self.samples
    }
}

/// Decodes **native SRS** payloads (elementary raw paths vs muxed codec packets).
pub trait MediaDecoder: Send {
    fn decode_video_packet(&mut self, payload: &[u8]) -> Result<VideoFrame>;
    fn decode_audio_packet(&mut self, payload: &[u8]) -> Result<AudioFrame>;
}

/// Consumes normalized frames and writes native mux packets (CPU encode).
pub trait NativeEncoderSink: Send {
    /// PTS in the mux track timescale; [`None`] selects a monotonic fallback.
    fn push_video(&mut self, frame: &dyn DecodedVideoFrame, pts_ticks: Option<u64>) -> Result<()>;
    fn push_audio(&mut self, frame: &dyn DecodedAudioFrame, pts_ticks: Option<u64>) -> Result<()>;
    fn finalize_mux(&mut self) -> Result<()>;
}

/// Future hook: optional GPU video encode scheduling (not implemented).
pub trait GpuEncodeDispatch: Send {
    fn gpu_video_encode_available(&self) -> bool {
        false
    }
}
