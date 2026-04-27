use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{StreamId, Timebase, Timestamp, TrackId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    Audio,
    Video,
    Subtitle,
    Data,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamRole {
    Primary,
    Alternate,
    Commentary,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodecType {
    NativeSrsVideo,
    NativeSrsAudio,
    Aac,
    Opus,
    Vorbis,
    Flac,
    Speex,
    Pcm,
    H264,
    H265,
    Av1,
    Vp8,
    Vp9,
    Theora,
    Unknown,
}

impl CodecType {
    pub const fn is_royalty_free_playback_allowed(self) -> bool {
        matches!(
            self,
            Self::NativeSrsVideo
                | Self::NativeSrsAudio
                | Self::Opus
                | Self::Vorbis
                | Self::Flac
                | Self::Speex
                | Self::Pcm
                | Self::Av1
                | Self::Vp8
                | Self::Vp9
                | Self::Theora
        )
    }

    pub const fn requires_external_playback_license_attention(self) -> bool {
        matches!(self, Self::Aac | Self::H264 | Self::H265)
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::NativeSrsVideo => "SRS Native Video",
            Self::NativeSrsAudio => "SRS Native Audio",
            Self::Aac => "AAC",
            Self::Opus => "Opus",
            Self::Vorbis => "Vorbis",
            Self::Flac => "FLAC",
            Self::Speex => "Speex",
            Self::Pcm => "PCM",
            Self::H264 => "H.264/AVC",
            Self::H265 => "H.265/HEVC",
            Self::Av1 => "AV1",
            Self::Vp8 => "VP8",
            Self::Vp9 => "VP9",
            Self::Theora => "Theora",
            Self::Unknown => "Unknown",
        }
    }

    pub fn royalty_free_codecs() -> Vec<Self> {
        vec![
            Self::NativeSrsVideo,
            Self::NativeSrsAudio,
            Self::Av1,
            Self::Vp9,
            Self::Vp8,
            Self::Theora,
            Self::Opus,
            Self::Vorbis,
            Self::Flac,
            Self::Speex,
            Self::Pcm,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackInfo {
    pub id: TrackId,
    pub kind: MediaKind,
    pub codec: CodecType,
    pub role: StreamRole,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamInfo {
    pub id: StreamId,
    pub timebase: Timebase,
    pub track: TrackInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet {
    pub stream_id: StreamId,
    pub pts: Option<Timestamp>,
    pub dts: Option<Timestamp>,
    pub duration: Option<Timestamp>,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameInfo {
    pub stream_id: StreamId,
    pub pts: Option<Timestamp>,
    pub duration: Option<Timestamp>,
    pub sample_count: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Error)]
pub enum ContractError {
    #[error("invalid timebase {num}/{den}")]
    InvalidTimebase { num: u32, den: u32 },
    #[error("unsupported stream: {0}")]
    UnsupportedStream(String),
}

#[cfg(test)]
mod tests {
    use super::CodecType;

    #[test]
    fn royalty_free_codecs_are_allowed() {
        for codec in CodecType::royalty_free_codecs() {
            assert!(
                codec.is_royalty_free_playback_allowed(),
                "{} should be allowed",
                codec.display_name()
            );
        }
    }

    #[test]
    fn patent_sensitive_codecs_are_not_allowed_by_policy() {
        for codec in [CodecType::Aac, CodecType::H264, CodecType::H265] {
            assert!(codec.requires_external_playback_license_attention());
            assert!(!codec.is_royalty_free_playback_allowed());
        }
    }

    #[test]
    fn unknown_codec_is_not_allowed() {
        assert!(!CodecType::Unknown.is_royalty_free_playback_allowed());
    }
}
