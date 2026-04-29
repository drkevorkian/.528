use std::collections::{HashMap, VecDeque};
use std::path::Path;

use anyhow::Result;
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
            .map(|(idx, stream)| CompatTrackInfo {
                id: TrackId(idx as u16),
                kind: map_media_kind(stream.parameters().medium()),
                codec: map_codec(stream.parameters().id()),
                role: if idx == 0 {
                    StreamRole::Primary
                } else {
                    StreamRole::Alternate
                },
                language: stream.metadata().get("language").map(ToOwned::to_owned),
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

#[derive(Debug, Default)]
pub struct FfmpegIngestor {
    queue: VecDeque<SourcePacket>,
    opened: bool,
}

impl FfmpegIngestor {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            opened: false,
        }
    }
}

impl MediaIngestor for FfmpegIngestor {
    fn open_path(&mut self, input: &Path) -> Result<()> {
        ffmpeg::init()?;
        self.queue.clear();

        let mut ictx = ffmpeg::format::input(input)?;
        let mut timebases = HashMap::new();
        for (idx, stream) in ictx.streams().enumerate() {
            let tb = stream.time_base();
            let den = tb.denominator().max(1) as u32;
            let num = tb.numerator().max(1) as u32;
            timebases.insert(idx, Timebase::new(num, den));
        }

        for (stream, packet) in ictx.packets() {
            let stream_idx = stream.index();
            let timebase = timebases
                .get(&stream_idx)
                .copied()
                .unwrap_or_else(Timebase::milliseconds);
            let payload = packet.data().map_or_else(Vec::new, ToOwned::to_owned);
            self.queue.push_back(SourcePacket {
                packet: Packet {
                    stream_id: StreamId(stream_idx as u16),
                    pts: packet.pts().map(|v| Timestamp::new(v, timebase)),
                    dts: packet.dts().map(|v| Timestamp::new(v, timebase)),
                    duration: packet.duration().map(|v| Timestamp::new(v, timebase)),
                    keyframe: packet.is_key(),
                    data: payload,
                },
                source_offset: packet.position().try_into().ok(),
            });
        }

        self.opened = true;
        Ok(())
    }

    fn read_packet(&mut self) -> Result<Option<SourcePacket>> {
        if !self.opened {
            return Ok(None);
        }
        Ok(self.queue.pop_front())
    }

    fn seek_ms(&mut self, position_ms: u64) -> Result<()> {
        let target_ticks = (position_ms as i64) * 1_000;
        let index = self
            .queue
            .iter()
            .position(|pkt| pkt.packet.pts.is_some_and(|pts| pts.ticks >= target_ticks))
            .unwrap_or(self.queue.len());
        self.queue.drain(..index);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.queue.clear();
        self.opened = false;
        Ok(())
    }
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
