use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use libsrs_audio::AudioStreamReader;
use libsrs_container::{PacketFlags, TrackKind};
use libsrs_contract::{
    CodecType, MediaKind, Packet, StreamId, StreamRole, Timebase, Timestamp, TrackId,
};
use libsrs_demux::DemuxReader;
use libsrs_video::VideoStreamReader;

use crate::probe::{CompatTrackInfo, MediaIngestor, MediaProbe, ProbeResult, SourcePacket};

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

        let format_name = input
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("synthetic")
            .to_string();

        Ok(ProbeResult {
            format_name,
            duration_ms: Some(5_000),
            tracks: vec![CompatTrackInfo {
                id: TrackId(0),
                kind: MediaKind::Video,
                codec: CodecType::Unknown,
                role: StreamRole::Primary,
                language: None,
            }],
        })
    }
}

#[derive(Debug, Default)]
pub struct StubIngestor {
    opened_path: Option<PathBuf>,
    cursor: usize,
    max_packets: u64,
    synthetic_mode: bool,
    packets: Vec<SourcePacket>,
}

impl StubIngestor {
    pub fn new() -> Self {
        Self {
            opened_path: None,
            cursor: 0,
            max_packets: 32,
            synthetic_mode: true,
            packets: Vec::new(),
        }
    }
}

impl MediaIngestor for StubIngestor {
    fn open_path(&mut self, input: &Path) -> Result<()> {
        self.opened_path = Some(input.to_path_buf());
        self.cursor = 0;
        self.packets.clear();
        self.synthetic_mode = !self.load_native_packets(input)?;
        Ok(())
    }

    fn read_packet(&mut self) -> Result<Option<SourcePacket>> {
        if self.opened_path.is_none() {
            return Err(anyhow!("ingestor not opened"));
        }
        if self.synthetic_mode {
            if self.cursor as u64 >= self.max_packets {
                return Ok(None);
            }
            let packet = crate::probe::CompatLayer::synthetic_packet(
                (self.cursor % 255) as u8,
                (self.cursor * 40) as i64,
            );
            self.cursor += 1;
            return Ok(Some(packet));
        }

        let packet = self.packets.get(self.cursor).cloned();
        self.cursor += 1;
        Ok(packet)
    }

    fn seek_ms(&mut self, position_ms: u64) -> Result<()> {
        if self.synthetic_mode {
            self.cursor = (position_ms / 40) as usize;
            return Ok(());
        }

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
        self.synthetic_mode = true;
        Ok(())
    }
}

impl StubIngestor {
    fn load_native_packets(&mut self, input: &Path) -> Result<bool> {
        match extension(input) {
            Some("528" | "srsm") => {
                self.packets = ingest_native_container(input)?;
                Ok(true)
            }
            Some("srsv") => {
                self.packets = ingest_native_video(input)?;
                Ok(true)
            }
            Some("srsa") => {
                self.packets = ingest_native_audio(input)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

fn probe_native_container(input: &Path) -> Result<ProbeResult> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut demux = DemuxReader::open(BufReader::new(file))
        .with_context(|| format!("parse {}", input.display()))?;
    let tracks = demux
        .tracks()
        .iter()
        .map(|track| CompatTrackInfo {
            id: TrackId(track.track_id as u32),
            kind: map_track_kind(track.kind),
            codec: map_codec(track.codec_id),
            role: if track.track_id == 1 {
                StreamRole::Primary
            } else {
                StreamRole::Alternate
            },
            language: None,
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
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let _reader = VideoStreamReader::new(BufReader::new(file))
        .with_context(|| format!("parse {}", input.display()))?;
    Ok(ProbeResult {
        format_name: "srsv".to_string(),
        duration_ms: None,
        tracks: vec![CompatTrackInfo {
            id: TrackId(0),
            kind: MediaKind::Video,
            codec: CodecType::NativeSrsVideo,
            role: StreamRole::Primary,
            language: None,
        }],
    })
}

fn probe_native_audio(input: &Path) -> Result<ProbeResult> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let _reader = AudioStreamReader::new(BufReader::new(file))
        .with_context(|| format!("parse {}", input.display()))?;
    Ok(ProbeResult {
        format_name: "srsa".to_string(),
        duration_ms: None,
        tracks: vec![CompatTrackInfo {
            id: TrackId(0),
            kind: MediaKind::Audio,
            codec: CodecType::NativeSrsAudio,
            role: StreamRole::Primary,
            language: None,
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

fn timestamp_to_ms(ts: Timestamp) -> i64 {
    if ts.timebase.den == 0 {
        return ts.ticks;
    }
    ((ts.ticks as i128) * (ts.timebase.num as i128) * 1_000 / (ts.timebase.den as i128)) as i64
}
