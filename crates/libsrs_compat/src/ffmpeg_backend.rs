use std::fmt;
use std::path::Path;

use anyhow::{anyhow, Result};
use ffmpeg_next as ffmpeg;
use libsrs_contract::{
    CodecType, MediaKind, Packet, StreamId, StreamRole, Timebase, Timestamp, TrackId,
};

use crate::probe::{CompatTrackInfo, MediaIngestor, MediaProbe, ProbeResult, SourcePacket};

#[derive(Debug, Default)]
pub struct FfmpegProbe;

impl MediaProbe for FfmpegProbe {
    fn probe_path(&self, input: &Path) -> Result<ProbeResult> {
        ffmpeg::init()?;
        let ictx = ffmpeg::format::input(input)?;
        let tracks = ictx
            .streams()
            .enumerate()
            .map(|(idx, stream)| {
                let params = stream.parameters();
                let (audio_sample_rate, audio_channels) =
                    audio_params_from_ffmpeg_parameters(&params);
                let (video_width, video_height) = video_params_from_ffmpeg_parameters(&params);
                CompatTrackInfo {
                    id: TrackId(idx as u32),
                    kind: map_media_kind(params.medium()),
                    codec: map_codec(params.id()),
                    role: if idx == 0 {
                        StreamRole::Primary
                    } else {
                        StreamRole::Alternate
                    },
                    language: stream.metadata().get("language").map(ToOwned::to_owned),
                    audio_sample_rate,
                    audio_channels,
                    video_width,
                    video_height,
                }
            })
            .collect();
        let duration_ms = if ictx.duration() > 0 {
            Some((ictx.duration() as u64) / 1_000)
        } else {
            None
        };

        Ok(ProbeResult {
            format_name: ictx.format().name().to_string(),
            duration_ms,
            tracks,
        })
    }
}

/// Streaming FFmpeg-backed packet source.
///
/// The previous implementation eagerly copied every demuxed packet into a VecDeque during
/// open_path, making memory use proportional to the entire input file. This implementation
/// retains the AVFormatContext and copies only the packet currently returned to the caller.
/// Consequently, ingest memory is bounded by FFmpeg's internal demux buffers plus one owned
/// SourcePacket, regardless of source duration.
pub struct FfmpegIngestor {
    input: Option<ffmpeg::format::context::Input>,
}

impl fmt::Debug for FfmpegIngestor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfmpegIngestor")
            .field("opened", &self.input.is_some())
            .finish()
    }
}

impl Default for FfmpegIngestor {
    fn default() -> Self {
        Self::new()
    }
}

impl FfmpegIngestor {
    pub const fn new() -> Self {
        Self { input: None }
    }
}

impl MediaIngestor for FfmpegIngestor {
    fn open_path(&mut self, input: &Path) -> Result<()> {
        ffmpeg::init()?;

        // Drop any previously-open input before attempting a replacement so stale file handles,
        // network handles, and demux buffers cannot survive a failed reopen.
        self.input = None;
        self.input = Some(ffmpeg::format::input(input)?);
        Ok(())
    }

    fn read_packet(&mut self) -> Result<Option<SourcePacket>> {
        let Some(ictx) = self.input.as_mut() else {
            return Ok(None);
        };

        // A fresh iterator is cheap: it borrows the retained AVFormatContext and advances that
        // context by one av_read_frame call. We intentionally take only one packet per API call
        // instead of buffering the rest of the source in Rust memory.
        let Some((stream, packet)) = ictx.packets().next() else {
            return Ok(None);
        };

        let stream_idx = stream.index();
        let tb = stream.time_base();
        let den = tb.denominator().max(1) as u32;
        let num = tb.numerator().max(1) as u32;
        let timebase = Timebase::new(num, den);
        let payload = packet.data().map_or_else(Vec::new, ToOwned::to_owned);

        Ok(Some(SourcePacket {
            packet: Packet {
                stream_id: StreamId(stream_idx as u32),
                pts: packet.pts().map(|v| Timestamp::new(v, timebase)),
                dts: packet.dts().map(|v| Timestamp::new(v, timebase)),
                duration: (packet.duration() != 0)
                    .then(|| Timestamp::new(packet.duration(), timebase)),
                keyframe: packet.is_key(),
                data: payload,
            },
            source_offset: packet.position().try_into().ok(),
        }))
    }

    fn seek_ms(&mut self, position_ms: u64) -> Result<()> {
        let Some(ictx) = self.input.as_mut() else {
            return Ok(());
        };

        // FFmpeg's global seek timestamp is AV_TIME_BASE units (microseconds) when no stream
        // index is supplied. Use checked conversion so attacker-controlled or corrupted timeline
        // values cannot wrap into a negative seek target.
        let target_us = i64::try_from(position_ms)
            .ok()
            .and_then(|ms| ms.checked_mul(1_000))
            .ok_or_else(|| anyhow!("seek position exceeds FFmpeg timestamp range"))?;

        // Seeking the retained demuxer resets read position without reopening the source. FFmpeg
        // may land on an earlier keyframe, which is the normal demux-level seek contract.
        ictx.seek(target_us, ..target_us)?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        // Dropping Input releases the AVFormatContext and its owned I/O resources immediately.
        self.input = None;
        Ok(())
    }
}

fn video_params_from_ffmpeg_parameters(
    par: &ffmpeg::codec::Parameters,
) -> (Option<u32>, Option<u32>) {
    if par.medium() != ffmpeg::media::Type::Video {
        return (None, None);
    }
    let (width, height) = unsafe {
        let p = par.as_ptr();
        ((*p).width, (*p).height)
    };
    let w = if width > 0 { Some(width as u32) } else { None };
    let h = if height > 0 {
        Some(height as u32)
    } else {
        None
    };
    (w, h)
}

fn audio_params_from_ffmpeg_parameters(
    par: &ffmpeg::codec::Parameters,
) -> (Option<u32>, Option<u8>) {
    if par.medium() != ffmpeg::media::Type::Audio {
        return (None, None);
    }
    // SAFETY: Parameters wraps a live AVCodecParameters for this stream; sample_rate/channels
    // are valid for audio types per FFmpeg's public ABI.
    let (sample_rate, channels) = unsafe {
        let p = par.as_ptr();
        ((*p).sample_rate, (*p).channels)
    };
    let sr = if sample_rate > 0 {
        Some(sample_rate as u32)
    } else {
        None
    };
    let ch = if channels > 0 {
        let c = channels as u8;
        if c == 1 || c == 2 {
            Some(c)
        } else {
            None
        }
    } else {
        None
    };
    (sr, ch)
}

fn map_media_kind(kind: ffmpeg::media::Type) -> MediaKind {
    match kind {
        ffmpeg::media::Type::Audio => MediaKind::Audio,
        ffmpeg::media::Type::Video => MediaKind::Video,
        ffmpeg::media::Type::Subtitle => MediaKind::Subtitle,
        _ => MediaKind::Data,
    }
}

fn map_codec(id: ffmpeg::codec::Id) -> CodecType {
    match id {
        ffmpeg::codec::Id::AAC => CodecType::Aac,
        ffmpeg::codec::Id::OPUS => CodecType::Opus,
        ffmpeg::codec::Id::VORBIS => CodecType::Vorbis,
        ffmpeg::codec::Id::FLAC => CodecType::Flac,
        ffmpeg::codec::Id::SPEEX => CodecType::Speex,
        ffmpeg::codec::Id::PCM_S16LE
        | ffmpeg::codec::Id::PCM_S16BE
        | ffmpeg::codec::Id::PCM_S24LE
        | ffmpeg::codec::Id::PCM_S24BE
        | ffmpeg::codec::Id::PCM_S32LE
        | ffmpeg::codec::Id::PCM_S32BE
        | ffmpeg::codec::Id::PCM_F32LE
        | ffmpeg::codec::Id::PCM_F32BE => CodecType::Pcm,
        ffmpeg::codec::Id::H264 => CodecType::H264,
        ffmpeg::codec::Id::HEVC => CodecType::H265,
        ffmpeg::codec::Id::AV1 => CodecType::Av1,
        ffmpeg::codec::Id::VP8 => CodecType::Vp8,
        ffmpeg::codec::Id::VP9 => CodecType::Vp9,
        ffmpeg::codec::Id::THEORA => CodecType::Theora,
        _ => CodecType::Unknown,
    }
}
