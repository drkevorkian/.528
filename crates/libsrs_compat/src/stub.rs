use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use libsrs_audio::{AudioStreamHeader, AudioStreamReader};
use libsrs_container::{PacketFlags, TrackKind};
use libsrs_contract::{
    CodecType, MediaKind, Packet, StreamId, StreamRole, Timebase, Timestamp, TrackId,
};
use libsrs_demux::DemuxReader;
use libsrs_video::{VideoStreamHeader, VideoStreamReader};

use crate::probe::{CompatTrackInfo, MediaIngestor, MediaProbe, ProbeResult, SourcePacket};

/// Message when the stub backend cannot decode a path (non-native media needs FFmpeg).
pub const FOREIGN_MEDIA_REQUIRES_FFMPEG: &str = "not a native .528 / .srsm / .srsv / .srsa source; probe and import of foreign media require `libsrs_compat` built with the `ffmpeg` feature (for example `cargo build -p libsrs_compat --features ffmpeg`)";

#[derive(Debug, Default)]
pub struct StubProbe;

impl MediaProbe for StubProbe {
    fn probe_path(&self, input: &Path) -> Result<ProbeResult> {
        if matches!(extension(input), Some("528" | "srsm")) {
            return probe_native_container(input);
        }
        if let Some("srsv") = extension(input) {
            return probe_native_video(input);
        }
        if let Some("srsa") = extension(input) {
            return probe_native_audio(input);
        }

        Err(anyhow!(FOREIGN_MEDIA_REQUIRES_FFMPEG))
    }
}

#[derive(Debug, Default)]
pub struct StubIngestor {
    opened_path: Option<PathBuf>,
    cursor: usize,
    packets: Vec<SourcePacket>,
}

impl StubIngestor {
    pub fn new() -> Self {
        Self {
            opened_path: None,
            cursor: 0,
            packets: Vec::new(),
        }
    }
}

impl MediaIngestor for StubIngestor {
    fn open_path(&mut self, input: &Path) -> Result<()> {
        self.opened_path = Some(input.to_path_buf());
        self.cursor = 0;
        self.packets.clear();
        self.packets = match extension(input) {
            Some("528" | "srsm") => ingest_native_container(input)?,
            Some("srsv") => ingest_native_video(input)?,
            Some("srsa") => ingest_native_audio(input)?,
            _ => {
                return Err(anyhow!(FOREIGN_MEDIA_REQUIRES_FFMPEG));
            }
        };
        Ok(())
    }

    fn read_packet(&mut self) -> Result<Option<SourcePacket>> {
        if self.opened_path.is_none() {
            return Err(anyhow!("ingestor not opened"));
        }
        let packet = self.packets.get(self.cursor).cloned();
        self.cursor += 1;
        Ok(packet)
    }

    fn seek_ms(&mut self, position_ms: u64) -> Result<()> {
        let target = position_ms as i64;
        self.cursor = self
            .packets
            .iter()
            .position(|pkt| {
                pkt.packet
                    .pts
                    .map(|ts| timestamp_to_ms(ts) >= target)
                    .unwrap_or(false)
            })
            .unwrap_or(self.packets.len());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.opened_path = None;
        self.cursor = 0;
        self.packets.clear();
        Ok(())
    }
}

fn probe_native_container(input: &Path) -> Result<ProbeResult> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut demux = DemuxReader::open(BufReader::new(file))
        .with_context(|| format!("parse {}", input.display()))?;
    let tracks = demux
        .tracks()
        .iter()
        .map(|track| {
            let (audio_sample_rate, audio_channels) = audio_params_from_native_track(track);
            let (video_width, video_height) = video_params_from_native_track(track);
            CompatTrackInfo {
                id: TrackId(track.track_id as u32),
                kind: map_track_kind(track.kind),
                codec: map_codec(track.codec_id),
                role: if track.track_id == 1 {
                    StreamRole::Primary
                } else {
                    StreamRole::Alternate
                },
                language: None,
                audio_sample_rate,
                audio_channels,
                video_width,
                video_height,
            }
        })
        .collect::<Vec<_>>();
    demux.rebuild_index()?;
    let duration_ms = demux
        .index()
        .iter()
        .map(|entry| entry.pts)
        .max()
        .map(|pts| pts / 1000);

    Ok(ProbeResult {
        format_name: format!("528-container-v{}", demux.header().version),
        duration_ms,
        tracks,
    })
}

fn probe_native_video(input: &Path) -> Result<ProbeResult> {
    let mut file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut preamble = [0_u8; 16];
    file.read_exact(&mut preamble)
        .with_context(|| format!("read video stream header {}", input.display()))?;
    let header = VideoStreamHeader::decode(preamble)
        .with_context(|| format!("parse video stream header {}", input.display()))?;
    Ok(ProbeResult {
        format_name: "srsv".to_string(),
        duration_ms: None,
        tracks: vec![CompatTrackInfo {
            id: TrackId(0),
            kind: MediaKind::Video,
            codec: CodecType::NativeSrsVideo,
            role: StreamRole::Primary,
            language: None,
            audio_sample_rate: None,
            audio_channels: None,
            video_width: Some(header.width),
            video_height: Some(header.height),
        }],
    })
}

fn probe_native_audio(input: &Path) -> Result<ProbeResult> {
    let mut file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut preamble = [0_u8; 16];
    file.read_exact(&mut preamble)
        .with_context(|| format!("read audio stream header {}", input.display()))?;
    let header = AudioStreamHeader::decode(preamble)
        .with_context(|| format!("parse audio stream header {}", input.display()))?;
    Ok(ProbeResult {
        format_name: "srsa".to_string(),
        duration_ms: None,
        tracks: vec![CompatTrackInfo {
            id: TrackId(0),
            kind: MediaKind::Audio,
            codec: CodecType::NativeSrsAudio,
            role: StreamRole::Primary,
            language: None,
            audio_sample_rate: Some(header.sample_rate),
            audio_channels: Some(header.channels),
            video_width: None,
            video_height: None,
        }],
    })
}

fn ingest_native_container(input: &Path) -> Result<Vec<SourcePacket>> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut demux = DemuxReader::open(BufReader::new(file))
        .with_context(|| format!("parse {}", input.display()))?;

    let timescales = demux
        .tracks()
        .iter()
        .map(|track| (track.track_id, track.timescale))
        .collect::<HashMap<_, _>>();

    let mut packets = Vec::new();
    demux.reset_to_data_start()?;
    while let Some(pkt) = demux.next_packet()? {
        let timescale = timescales
            .get(&pkt.packet.header.track_id)
            .copied()
            .unwrap_or(1_000);
        let timebase = Timebase::new(1, timescale.max(1));
        let pts = Timestamp::new(pkt.packet.header.pts as i64, timebase);
        let dts = Timestamp::new(pkt.packet.header.dts as i64, timebase);
        let duration = if packets.last().is_some() {
            None
        } else {
            Some(Timestamp::new(0, timebase))
        };

        packets.push(SourcePacket {
            packet: Packet {
                stream_id: StreamId(pkt.packet.header.track_id as u32),
                pts: Some(pts),
                dts: Some(dts),
                duration,
                keyframe: pkt.packet.header.flags & PacketFlags::KEYFRAME != 0,
                data: pkt.packet.payload,
            },
            source_offset: Some(pkt.offset),
        });
    }
    Ok(packets)
}

fn ingest_native_video(input: &Path) -> Result<Vec<SourcePacket>> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut reader = VideoStreamReader::new(BufReader::new(file))
        .with_context(|| format!("parse {}", input.display()))?;
    let mut packets = Vec::new();
    let timebase = Timebase::milliseconds();
    let mut pts_ms = 0_i64;

    while let Some(frame) = reader.read_next_frame()? {
        packets.push(SourcePacket {
            packet: Packet {
                stream_id: StreamId(0),
                pts: Some(Timestamp::new(pts_ms, timebase)),
                dts: Some(Timestamp::new(pts_ms, timebase)),
                duration: Some(Timestamp::new(40, timebase)),
                keyframe: true,
                data: frame.data,
            },
            source_offset: None,
        });
        pts_ms += 40;
    }

    Ok(packets)
}

fn ingest_native_audio(input: &Path) -> Result<Vec<SourcePacket>> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut reader = AudioStreamReader::new(BufReader::new(file))
        .with_context(|| format!("parse {}", input.display()))?;
    let mut packets = Vec::new();
    let timebase = Timebase::milliseconds();
    let mut pts_ms = 0_i64;

    while let Some(frame) = reader.read_next_frame()? {
        let sample_count = frame.sample_count_per_channel()? as i64;
        let duration_ms = if frame.sample_rate == 0 {
            0
        } else {
            (sample_count * 1_000) / frame.sample_rate as i64
        };
        let mut bytes = Vec::with_capacity(frame.samples.len() * 2);
        for sample in frame.samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        packets.push(SourcePacket {
            packet: Packet {
                stream_id: StreamId(0),
                pts: Some(Timestamp::new(pts_ms, timebase)),
                dts: Some(Timestamp::new(pts_ms, timebase)),
                duration: Some(Timestamp::new(duration_ms, timebase)),
                keyframe: true,
                data: bytes,
            },
            source_offset: None,
        });
        pts_ms += duration_ms;
    }

    Ok(packets)
}

fn extension(path: &Path) -> Option<&str> {
    path.extension().and_then(|ext| ext.to_str())
}

fn map_track_kind(kind: TrackKind) -> MediaKind {
    match kind {
        TrackKind::Audio => MediaKind::Audio,
        TrackKind::Video => MediaKind::Video,
        TrackKind::Subtitle => MediaKind::Subtitle,
        TrackKind::Data | TrackKind::Metadata | TrackKind::Attachment => MediaKind::Data,
    }
}

fn map_codec(codec_id: u16) -> CodecType {
    match codec_id {
        1 => CodecType::NativeSrsVideo,
        2 => CodecType::NativeSrsAudio,
        _ => CodecType::Unknown,
    }
}

/// Parses multiplexed native audio track config: `sample_rate` (le u32) + `channels` (u8), same layout as mux `TrackDescriptor.config`.
fn audio_params_from_native_track(
    track: &libsrs_container::TrackDescriptor,
) -> (Option<u32>, Option<u8>) {
    if track.kind != TrackKind::Audio {
        return (None, None);
    }
    if track.config.len() < 5 {
        return (None, None);
    }
    let sample_rate = u32::from_le_bytes([
        track.config[0],
        track.config[1],
        track.config[2],
        track.config[3],
    ]);
    let channels = track.config[4];
    if sample_rate == 0 || (channels != 1 && channels != 2) {
        return (None, None);
    }
    (Some(sample_rate), Some(channels))
}

fn video_params_from_native_track(
    track: &libsrs_container::TrackDescriptor,
) -> (Option<u32>, Option<u32>) {
    if track.kind != TrackKind::Video || track.config.len() < 8 {
        return (None, None);
    }
    let width = u32::from_le_bytes([
        track.config[0],
        track.config[1],
        track.config[2],
        track.config[3],
    ]);
    let height = u32::from_le_bytes([
        track.config[4],
        track.config[5],
        track.config[6],
        track.config[7],
    ]);
    if width == 0 || height == 0 {
        return (None, None);
    }
    (Some(width), Some(height))
}

fn timestamp_to_ms(ts: Timestamp) -> i64 {
    if ts.timebase.den == 0 {
        return ts.ticks;
    }
    ((ts.ticks as i128) * (ts.timebase.num as i128) * 1_000 / (ts.timebase.den as i128)) as i64
}

#[cfg(test)]
mod probe_audio_tests {
    use super::*;
    use libsrs_audio::STREAM_VERSION_V2;

    #[test]
    fn probe_srsa_reads_rate_and_channels_from_header_only() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "srsa-probe-{}.srsa",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let header = AudioStreamHeader {
            sample_rate: 44_100,
            channels: 2,
            stream_version: STREAM_VERSION_V2,
        };
        std::fs::write(&path, header.encode().as_slice()).unwrap();
        let probe = StubProbe.probe_path(&path).expect("probe");
        assert_eq!(probe.tracks.len(), 1);
        assert_eq!(probe.tracks[0].audio_sample_rate, Some(44_100));
        assert_eq!(probe.tracks[0].audio_channels, Some(2));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stub_probe_rejects_foreign_extension() {
        let dir = std::env::temp_dir();
        let path = dir.join("foreign-test.xyz");
        std::fs::write(&path, b"not media").unwrap();
        let err = StubProbe.probe_path(&path).expect_err("foreign should err");
        assert!(
            err.to_string().contains("ffmpeg"),
            "expected ffmpeg hint: {err}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stub_ingestor_rejects_foreign_extension() {
        let dir = std::env::temp_dir();
        let path = dir.join("foreign-ingest.bin");
        std::fs::write(&path, b"x").unwrap();
        let mut ing = StubIngestor::new();
        ing.open_path(&path).expect_err("ingest foreign should err");
        std::fs::remove_file(&path).ok();
    }
}
