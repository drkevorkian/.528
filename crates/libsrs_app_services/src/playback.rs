//! `.528` / `.srsm` playback: demux + native SRS decode with explicit security limits.
//!
//! # Reality check (Phase 0)
//! - **Demux:** [`libsrs_demux::DemuxReader`] over `Read+Seek` — already enforces container limits
//!   (`MAX_PACKET_PAYLOAD_BYTES`, etc.) per `libsrs_container::io`.
//! - **Video decode:** [`libsrs_video::decode_frame`] (intra frames); needs width/height from track config.
//! - **Audio decode:** [`libsrs_audio::decode_frame_with_stream_version`] with v2 stream payloads.
//! - **App inspect:** [`crate::inspect_native_container`](super) lists tracks; playback uses the same file layout.
//! - **Player (before this module):** `playing=true` and wall-clock `position_ms` were scaffold-only.

use std::collections::VecDeque;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crc32c::crc32c;
use libsrs_audio::{decode_frame_with_stream_version, AudioFrame, STREAM_VERSION_V2};
use libsrs_container::{PacketFlags, TrackDescriptor, TrackKind};
use libsrs_demux::{DemuxReader, DemuxedPacket};
use libsrs_video::{decode_frame, FrameType, VideoFrame};
use thiserror::Error;

/// Hard cap on per-side video dimension (hostile `.528`).
pub const MAX_VIDEO_SIDE: u32 = 8192;
/// Max pixels (width × height) we will allocate for a decoded grayscale buffer.
pub const MAX_VIDEO_PIXELS: u64 = 32_000_000;
/// Max buffered packets waiting for the "other" stream (cross-track ordering).
pub const MAX_STASH_PACKETS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Paused,
    Playing,
    EndOfStream,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackPosition {
    /// Best-effort media time from the last decoded packet (primary track timescale).
    pub pts_ticks: u64,
    pub timescale_hz: u32,
}

impl PlaybackPosition {
    pub fn as_ms(self) -> u64 {
        if self.timescale_hz == 0 {
            return 0;
        }
        self.pts_ticks
            .saturating_mul(1000)
            .saturating_div(self.timescale_hz as u64)
    }
}

/// Monotonic wall clock for UI pacing (optional); media **position** should come from [`PlaybackPosition`].
#[derive(Debug, Clone, Default)]
pub struct PlaybackClock {
    // Reserved for A/V sync vs wall clock in future slices.
}

#[derive(Debug, Clone)]
pub struct PlaybackTrackInfo {
    pub mux_track_id: u16,
    pub kind: TrackKind,
    pub codec_id: u16,
    pub timescale_hz: u32,
}

#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    pub width: u32,
    pub height: u32,
    pub frame_index: u32,
    pub pts_ticks: u64,
    pub dts_ticks: u64,
    pub timescale_hz: u32,
    pub payload_crc32c: u32,
    pub gray8: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DecodedAudioChunk {
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_index: u32,
    pub pts_ticks: u64,
    pub dts_ticks: u64,
    pub timescale_hz: u32,
    pub samples_interleaved: Vec<i16>,
}

#[derive(Debug, Clone)]
pub enum PlaybackEvent {
    Video(DecodedVideoFrame),
    Audio(DecodedAudioChunk),
    EndOfStream,
}

#[derive(Debug, Clone)]
pub enum PlaybackCommand {
    Play,
    Pause,
    Stop,
    SeekMs(u64),
}

#[derive(Debug, Error)]
pub enum PlaybackError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Unsupported(String),
    #[error("unsupported codec for playback: mux track {track_id} codec_id {codec_id}")]
    UnsupportedCodec { track_id: u16, codec_id: u16 },
    #[error("invalid track layout: {0}")]
    InvalidTrack(String),
    #[error("malformed or hostile media: {0}")]
    Malformed(String),
    #[error("video decode error: {0}")]
    VideoDecode(String),
    #[error("audio decode error: {0}")]
    AudioDecode(String),
    #[error("seek is not available: no index entries for this file")]
    SeekUnsupported,
    #[error(
        "decode backpressure: too many stashed packets ({0}); consume audio/video in file order"
    )]
    DecodeBackpressure(usize),
}

fn extension_allowed(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("528") || e.eq_ignore_ascii_case("srsm"))
}

fn validate_video_config(track: &TrackDescriptor) -> Result<(u32, u32), PlaybackError> {
    if track.kind != TrackKind::Video {
        return Err(PlaybackError::InvalidTrack("not a video track".to_string()));
    }
    if track.codec_id != 1 {
        return Err(PlaybackError::UnsupportedCodec {
            track_id: track.track_id,
            codec_id: track.codec_id,
        });
    }
    if track.config.len() < 8 {
        return Err(PlaybackError::InvalidTrack(
            "video track config must be at least 8 bytes (width, height)".to_string(),
        ));
    }
    let w = u32::from_le_bytes([
        track.config[0],
        track.config[1],
        track.config[2],
        track.config[3],
    ]);
    let h = u32::from_le_bytes([
        track.config[4],
        track.config[5],
        track.config[6],
        track.config[7],
    ]);
    if w == 0 || h == 0 {
        return Err(PlaybackError::Malformed(
            "video dimensions must be non-zero".to_string(),
        ));
    }
    if w > MAX_VIDEO_SIDE || h > MAX_VIDEO_SIDE {
        return Err(PlaybackError::Malformed(format!(
            "video dimensions {w}x{h} exceed max side {MAX_VIDEO_SIDE}"
        )));
    }
    let px = (w as u64).saturating_mul(h as u64);
    if px > MAX_VIDEO_PIXELS {
        return Err(PlaybackError::Malformed(format!(
            "video pixel count {px} exceeds cap {MAX_VIDEO_PIXELS}"
        )));
    }
    Ok((w, h))
}

fn validate_audio_config(track: &TrackDescriptor) -> Result<(u32, u8), PlaybackError> {
    if track.kind != TrackKind::Audio {
        return Err(PlaybackError::InvalidTrack(
            "not an audio track".to_string(),
        ));
    }
    if track.codec_id != 2 {
        return Err(PlaybackError::UnsupportedCodec {
            track_id: track.track_id,
            codec_id: track.codec_id,
        });
    }
    if track.config.len() < 5 {
        return Err(PlaybackError::InvalidTrack(
            "audio track config must be at least 5 bytes (sample_rate LE, channels)".to_string(),
        ));
    }
    let sr = u32::from_le_bytes([
        track.config[0],
        track.config[1],
        track.config[2],
        track.config[3],
    ]);
    let ch = track.config[4];
    if sr == 0 {
        return Err(PlaybackError::Malformed(
            "audio sample rate must be non-zero".to_string(),
        ));
    }
    if ch != 1 && ch != 2 {
        return Err(PlaybackError::Malformed(format!(
            "audio channel count {ch} not supported (expect 1 or 2)"
        )));
    }
    Ok((sr, ch))
}

fn validate_tracks_hostile(tracks: &[TrackDescriptor]) -> Result<(), PlaybackError> {
    if tracks.is_empty() {
        return Err(PlaybackError::Malformed(
            "container has no tracks".to_string(),
        ));
    }
    for t in tracks {
        match t.kind {
            TrackKind::Video => {
                let _ = validate_video_config(t)?;
            }
            TrackKind::Audio => {
                let _ = validate_audio_config(t)?;
            }
            TrackKind::Subtitle | TrackKind::Data | TrackKind::Metadata | TrackKind::Attachment => {
                // Allowed in file; not selected for decode unless extended later.
            }
        }
    }
    Ok(())
}

/// Shared `.528` decode session: single demux cursor, bounded cross-track stash.
pub struct PlaybackSession {
    demux: DemuxReader<BufReader<File>>,
    state: PlaybackState,
    position: PlaybackPosition,
    duration_ms: u64,
    seek_supported: bool,

    primary_video: Option<PlaybackTrackInfo>,
    primary_audio: Option<PlaybackTrackInfo>,
    video_w: u32,
    video_h: u32,
    audio_sr: u32,
    audio_ch: u8,

    video_stash: VecDeque<DemuxedPacket>,
    audio_stash: VecDeque<DemuxedPacket>,

    pub decoded_video_frames: u64,
    pub decoded_audio_chunks: u64,
    last_error: Option<String>,
}

impl std::fmt::Debug for PlaybackSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackSession")
            .field("state", &self.state)
            .field("position", &self.position)
            .field("duration_ms", &self.duration_ms)
            .field("seek_supported", &self.seek_supported)
            .field("decoded_video_frames", &self.decoded_video_frames)
            .field("decoded_audio_chunks", &self.decoded_audio_chunks)
            .field("primary_video", &self.primary_video)
            .field("primary_audio", &self.primary_audio)
            .finish_non_exhaustive()
    }
}

impl PlaybackSession {
    /// Open a native multiplexed file (`.528` or legacy `.srsm`).
    pub fn open(path: &Path) -> Result<Self, PlaybackError> {
        if !extension_allowed(path) {
            return Err(PlaybackError::Unsupported(
                "expected .528 or .srsm extension (case-insensitive)".to_string(),
            ));
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut demux = DemuxReader::open(reader)?;

        let tracks: Vec<TrackDescriptor> = demux.tracks().to_vec();
        validate_tracks_hostile(&tracks)?;

        demux.rebuild_index()?;
        let seek_supported = !demux.index().is_empty();
        let max_pts = demux.index().iter().map(|e| e.pts).max().unwrap_or(0);

        let primary_video = tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Video && t.codec_id == 1)
            .min_by_key(|t| t.track_id)
            .map(|t| PlaybackTrackInfo {
                mux_track_id: t.track_id,
                kind: t.kind,
                codec_id: t.codec_id,
                timescale_hz: t.timescale,
            });

        let primary_audio = tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio && t.codec_id == 2)
            .min_by_key(|t| t.track_id)
            .map(|t| PlaybackTrackInfo {
                mux_track_id: t.track_id,
                kind: t.kind,
                codec_id: t.codec_id,
                timescale_hz: t.timescale,
            });

        let (video_w, video_h) = primary_video
            .as_ref()
            .and_then(|pv| tracks.iter().find(|t| t.track_id == pv.mux_track_id))
            .map(validate_video_config)
            .transpose()?
            .unwrap_or((0, 0));

        let (audio_sr, audio_ch) = primary_audio
            .as_ref()
            .and_then(|pa| tracks.iter().find(|t| t.track_id == pa.mux_track_id))
            .map(validate_audio_config)
            .transpose()?
            .unwrap_or((0, 0));

        let ts_for_duration = primary_video
            .as_ref()
            .map(|v| v.timescale_hz)
            .filter(|&t| t > 0)
            .or_else(|| primary_audio.as_ref().map(|a| a.timescale_hz))
            .unwrap_or(90_000)
            .max(1);

        let duration_ms = max_pts
            .saturating_mul(1000)
            .saturating_div(ts_for_duration as u64);

        Ok(Self {
            demux,
            state: PlaybackState::Stopped,
            position: PlaybackPosition {
                pts_ticks: 0,
                timescale_hz: ts_for_duration,
            },
            duration_ms,
            seek_supported,
            primary_video,
            primary_audio,
            video_w,
            video_h,
            audio_sr,
            audio_ch,
            video_stash: VecDeque::new(),
            audio_stash: VecDeque::new(),
            decoded_video_frames: 0,
            decoded_audio_chunks: 0,
            last_error: None,
        })
    }

    pub fn state(&self) -> PlaybackState {
        self.state
    }

    pub fn position(&self) -> PlaybackPosition {
        self.position
    }

    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    pub fn seek_supported(&self) -> bool {
        self.seek_supported
    }

    pub fn primary_video(&self) -> Option<&PlaybackTrackInfo> {
        self.primary_video.as_ref()
    }

    pub fn primary_audio(&self) -> Option<&PlaybackTrackInfo> {
        self.primary_audio.as_ref()
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn stash_total(&self) -> usize {
        self.video_stash.len() + self.audio_stash.len()
    }

    fn push_stash_video(&mut self, p: DemuxedPacket) -> Result<(), PlaybackError> {
        if self.stash_total() >= MAX_STASH_PACKETS {
            return Err(PlaybackError::DecodeBackpressure(MAX_STASH_PACKETS));
        }
        self.video_stash.push_back(p);
        Ok(())
    }

    fn push_stash_audio(&mut self, p: DemuxedPacket) -> Result<(), PlaybackError> {
        if self.stash_total() >= MAX_STASH_PACKETS {
            return Err(PlaybackError::DecodeBackpressure(MAX_STASH_PACKETS));
        }
        self.audio_stash.push_back(p);
        Ok(())
    }

    fn next_demux_packet(&mut self) -> Result<Option<DemuxedPacket>, PlaybackError> {
        Ok(self.demux.next_packet()?)
    }

    fn update_position_from_pts(&mut self, pts: u64, ts: u32) {
        self.position.pts_ticks = pts;
        if ts > 0 {
            self.position.timescale_hz = ts;
        }
    }

    /// Decode the next **video** frame (may read and stash non-video packets).
    pub fn decode_next_video_frame(&mut self) -> Result<Option<DecodedVideoFrame>, PlaybackError> {
        let (vid_track, vid_ts) = match &self.primary_video {
            Some(v) => (v.mux_track_id, v.timescale_hz),
            None => {
                return Err(PlaybackError::Unsupported(
                    "no primary video track".to_string(),
                ));
            }
        };
        let aud_track = self.primary_audio.as_ref().map(|a| a.mux_track_id);

        if !self.video_stash.is_empty() {
            let p = self.video_stash.pop_front().unwrap();
            return self.decode_video_packet(&p, vid_ts);
        }

        const MAX_SKIP: usize = 65_536;
        for _ in 0..MAX_SKIP {
            let Some(p) = self.next_demux_packet()? else {
                return Ok(None);
            };
            if p.packet.header.track_id == vid_track {
                return self.decode_video_packet(&p, vid_ts);
            }
            if let Some(at) = aud_track {
                if p.packet.header.track_id == at {
                    self.push_stash_audio(p)?;
                    continue;
                }
            }
        }
        Err(PlaybackError::Malformed(
            "video decode exceeded skip budget (non-primary interleaving)".to_string(),
        ))
    }

    /// Decode the next **audio** chunk (may read and stash non-audio packets).
    pub fn decode_next_audio_chunk(&mut self) -> Result<Option<DecodedAudioChunk>, PlaybackError> {
        let (aud_track, aud_ts) = match &self.primary_audio {
            Some(a) => (a.mux_track_id, a.timescale_hz),
            None => {
                return Err(PlaybackError::Unsupported(
                    "no primary audio track".to_string(),
                ));
            }
        };
        let vid_track = self.primary_video.as_ref().map(|v| v.mux_track_id);

        if !self.audio_stash.is_empty() {
            let p = self.audio_stash.pop_front().unwrap();
            return self.decode_audio_packet(&p, aud_ts);
        }

        const MAX_SKIP: usize = 65_536;
        for _ in 0..MAX_SKIP {
            let Some(p) = self.next_demux_packet()? else {
                return Ok(None);
            };
            if p.packet.header.track_id == aud_track {
                return self.decode_audio_packet(&p, aud_ts);
            }
            if let Some(vt) = vid_track {
                if p.packet.header.track_id == vt {
                    self.push_stash_video(p)?;
                    continue;
                }
            }
        }
        Err(PlaybackError::Malformed(
            "audio decode exceeded skip budget (non-primary interleaving)".to_string(),
        ))
    }

    /// One demux step in **file order** — suitable for players that want interleaved A/V.
    pub fn decode_next_step(&mut self) -> Result<PlaybackEvent, PlaybackError> {
        if self.primary_video.is_none() && self.primary_audio.is_none() {
            return Err(PlaybackError::Unsupported(
                "no decodable A/V tracks".to_string(),
            ));
        }

        let vid = self
            .primary_video
            .as_ref()
            .map(|v| (v.mux_track_id, v.timescale_hz));
        let aud = self
            .primary_audio
            .as_ref()
            .map(|a| (a.mux_track_id, a.timescale_hz));

        const MAX_SKIP: usize = 65_536;
        for _ in 0..MAX_SKIP {
            let Some(pkt) = self.next_demux_packet()? else {
                self.state = PlaybackState::EndOfStream;
                return Ok(PlaybackEvent::EndOfStream);
            };

            if let Some((tid, ts)) = vid {
                if pkt.packet.header.track_id == tid {
                    if let Some(frame) = self.decode_video_packet(&pkt, ts)? {
                        return Ok(PlaybackEvent::Video(frame));
                    }
                    continue;
                }
            }
            if let Some((tid, ts)) = aud {
                if pkt.packet.header.track_id == tid {
                    if let Some(chunk) = self.decode_audio_packet(&pkt, ts)? {
                        return Ok(PlaybackEvent::Audio(chunk));
                    }
                    continue;
                }
            }
        }
        Err(PlaybackError::Malformed(
            "excessive consecutive packets with no decodable primary A/V payload".to_string(),
        ))
    }

    fn decode_video_packet(
        &mut self,
        p: &DemuxedPacket,
        timescale: u32,
    ) -> Result<Option<DecodedVideoFrame>, PlaybackError> {
        if p.packet.header.flags & PacketFlags::CONFIG != 0 {
            return Ok(None);
        }
        if p.packet.header.flags & PacketFlags::CORRUPT != 0 {
            return Err(PlaybackError::Malformed(
                "video packet flagged corrupt".to_string(),
            ));
        }
        let seq = u32::try_from(p.packet.header.sequence)
            .map_err(|_| PlaybackError::Malformed("video sequence number too large".to_string()))?;
        let vf: VideoFrame = decode_frame(
            self.video_w,
            self.video_h,
            seq,
            FrameType::I,
            &p.packet.payload,
        )
        .map_err(|e| PlaybackError::VideoDecode(e.to_string()))?;
        self.update_position_from_pts(p.packet.header.pts, timescale);
        self.decoded_video_frames += 1;
        let payload_crc32c = crc32c(&p.packet.payload);
        Ok(Some(DecodedVideoFrame {
            width: vf.width,
            height: vf.height,
            frame_index: vf.frame_index,
            pts_ticks: p.packet.header.pts,
            dts_ticks: p.packet.header.dts,
            timescale_hz: timescale,
            payload_crc32c,
            gray8: vf.data,
        }))
    }

    fn decode_audio_packet(
        &mut self,
        p: &DemuxedPacket,
        timescale: u32,
    ) -> Result<Option<DecodedAudioChunk>, PlaybackError> {
        if p.packet.header.flags & PacketFlags::CONFIG != 0 {
            return Ok(None);
        }
        if p.packet.header.flags & PacketFlags::CORRUPT != 0 {
            return Err(PlaybackError::Malformed(
                "audio packet flagged corrupt".to_string(),
            ));
        }
        let seq = u32::try_from(p.packet.header.sequence)
            .map_err(|_| PlaybackError::Malformed("audio sequence number too large".to_string()))?;
        let af: AudioFrame = decode_frame_with_stream_version(
            self.audio_sr,
            seq,
            &p.packet.payload,
            STREAM_VERSION_V2,
        )
        .map_err(|e| PlaybackError::AudioDecode(e.to_string()))?;
        if af.channels != self.audio_ch {
            return Err(PlaybackError::Malformed(
                "decoded audio channel layout disagrees with track config".to_string(),
            ));
        }
        self.update_position_from_pts(p.packet.header.pts, timescale);
        self.decoded_audio_chunks += 1;
        Ok(Some(DecodedAudioChunk {
            sample_rate: af.sample_rate,
            channels: af.channels,
            frame_index: af.frame_index,
            pts_ticks: p.packet.header.pts,
            dts_ticks: p.packet.header.dts,
            timescale_hz: timescale,
            samples_interleaved: af.samples,
        }))
    }

    pub fn play(&mut self) {
        if self.state == PlaybackState::EndOfStream || self.state == PlaybackState::Error {
            return;
        }
        self.state = PlaybackState::Playing;
    }

    pub fn pause(&mut self) {
        if self.state == PlaybackState::Playing {
            self.state = PlaybackState::Paused;
        }
    }

    pub fn stop(&mut self) -> Result<(), PlaybackError> {
        self.demux.reset_to_data_start()?;
        self.video_stash.clear();
        self.audio_stash.clear();
        self.state = PlaybackState::Stopped;
        self.position = PlaybackPosition {
            pts_ticks: 0,
            timescale_hz: self.position.timescale_hz.max(1),
        };
        self.decoded_video_frames = 0;
        self.decoded_audio_chunks = 0;
        self.last_error = None;
        Ok(())
    }

    /// Seek to approximately `target_ms` using the demux index, if available.
    pub fn seek_ms(&mut self, target_ms: u64) -> Result<(), PlaybackError> {
        if !self.seek_supported {
            return Err(PlaybackError::SeekUnsupported);
        }
        let ts = self.position.timescale_hz.max(1) as u64;
        let pts = target_ms.saturating_mul(ts).saturating_div(1000);
        let ent = self
            .demux
            .seek_nearest(pts)?
            .ok_or(PlaybackError::SeekUnsupported)?;
        self.video_stash.clear();
        self.audio_stash.clear();
        self.position = PlaybackPosition {
            pts_ticks: ent.pts,
            timescale_hz: self.position.timescale_hz.max(1),
        };
        Ok(())
    }

    pub fn apply_command(&mut self, cmd: PlaybackCommand) -> Result<(), PlaybackError> {
        match cmd {
            PlaybackCommand::Play => self.play(),
            PlaybackCommand::Pause => self.pause(),
            PlaybackCommand::Stop => self.stop()?,
            PlaybackCommand::SeekMs(ms) => self.seek_ms(ms)?,
        }
        Ok(())
    }

    /// Pump up to `max_steps` `decode_next_step` calls (for CLI / tests).
    pub fn pump_file_order(
        &mut self,
        max_steps: usize,
    ) -> Result<Vec<PlaybackEvent>, PlaybackError> {
        let mut out = Vec::new();
        for _ in 0..max_steps {
            let e = self.decode_next_step()?;
            let done = matches!(e, PlaybackEvent::EndOfStream);
            out.push(e);
            if done {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod playback_tests {
    use super::*;
    use libsrs_container::{FileHeader, TrackDescriptor};
    use libsrs_mux::MuxWriter;
    use libsrs_video::{encode_frame, FrameType, VideoFrame};

    fn write_video_only_528(path: &Path) {
        let w = 8u32;
        let frame = VideoFrame {
            width: w,
            height: w,
            frame_index: 0,
            frame_type: FrameType::I,
            data: vec![0xAB; (w * w) as usize],
        };
        let enc = encode_frame(&frame).unwrap();
        let tracks = vec![TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 1,
            flags: 0,
            timescale: 90_000,
            config: [w.to_le_bytes(), w.to_le_bytes()].concat(),
        }];
        let f = File::create(path).unwrap();
        let mut mux = MuxWriter::new(f, FileHeader::new(1, 4), tracks).unwrap();
        mux.write_packet(1, 0, 0, true, &enc).unwrap();
        mux.finalize().unwrap();
    }

    fn write_two_frame_video_528(path: &Path) {
        let w = 8u32;
        let f0 = VideoFrame {
            width: w,
            height: w,
            frame_index: 0,
            frame_type: FrameType::I,
            data: vec![0x11; (w * w) as usize],
        };
        let f1 = VideoFrame {
            width: w,
            height: w,
            frame_index: 1,
            frame_type: FrameType::I,
            data: vec![0x22; (w * w) as usize],
        };
        let enc0 = encode_frame(&f0).unwrap();
        let enc1 = encode_frame(&f1).unwrap();
        let tracks = vec![TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 1,
            flags: 0,
            timescale: 90_000,
            config: [w.to_le_bytes(), w.to_le_bytes()].concat(),
        }];
        let f = File::create(path).unwrap();
        let mut mux = MuxWriter::new(f, FileHeader::new(1, 4), tracks).unwrap();
        mux.write_packet(1, 0, 0, true, &enc0).unwrap();
        mux.write_packet(1, 3_000, 3_000, true, &enc1).unwrap();
        mux.finalize().unwrap();
    }

    fn write_audio_only_528(path: &Path) {
        use libsrs_audio::{encode_frame as aenc, AudioFrame};
        let frame = AudioFrame {
            sample_rate: 48_000,
            channels: 1,
            frame_index: 0,
            samples: vec![1_i16, 2_i16, -3_i16],
        };
        let enc = aenc(&frame).unwrap();
        let tracks = vec![TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Audio,
            codec_id: 2,
            flags: 0,
            timescale: 48_000,
            config: [48_000u32.to_le_bytes().to_vec(), vec![1u8]].concat(),
        }];
        let f = File::create(path).unwrap();
        let mut mux = MuxWriter::new(f, FileHeader::new(1, 4), tracks).unwrap();
        mux.write_packet(1, 0, 0, true, &enc).unwrap();
        mux.finalize().unwrap();
    }

    fn write_av_528(path: &Path) {
        use libsrs_audio::{encode_frame as aenc, AudioFrame};
        let w = 8u32;
        let vf = VideoFrame {
            width: w,
            height: w,
            frame_index: 0,
            frame_type: FrameType::I,
            data: vec![0xCD; (w * w) as usize],
        };
        let ve = encode_frame(&vf).unwrap();
        let af = AudioFrame {
            sample_rate: 48_000,
            channels: 1,
            frame_index: 0,
            samples: vec![9_i16; 128],
        };
        let ae = aenc(&af).unwrap();
        let tracks = vec![
            TrackDescriptor {
                track_id: 1,
                kind: TrackKind::Video,
                codec_id: 1,
                flags: 0,
                timescale: 90_000,
                config: [w.to_le_bytes(), w.to_le_bytes()].concat(),
            },
            TrackDescriptor {
                track_id: 2,
                kind: TrackKind::Audio,
                codec_id: 2,
                flags: 0,
                timescale: 48_000,
                config: [48_000u32.to_le_bytes().to_vec(), vec![1u8]].concat(),
            },
        ];
        let f = File::create(path).unwrap();
        let mut mux = MuxWriter::new(f, FileHeader::new(2, 4), tracks).unwrap();
        mux.write_packet(1, 0, 0, true, &ve).unwrap();
        mux.write_packet(2, 0, 0, true, &ae).unwrap();
        mux.finalize().unwrap();
    }

    #[test]
    fn video_only_decode() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "pb-v-{}.528",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_video_only_528(&p);
        let mut s = PlaybackSession::open(&p).unwrap();
        let f = s.decode_next_video_frame().unwrap().unwrap();
        assert_eq!(f.width, 8);
        assert!(!f.gray8.is_empty());
        assert_eq!(s.decoded_video_frames, 1);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn audio_only_decode() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "pb-a-{}.528",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_audio_only_528(&p);
        let mut s = PlaybackSession::open(&p).unwrap();
        let a = s.decode_next_audio_chunk().unwrap().unwrap();
        assert_eq!(a.sample_rate, 48_000);
        assert_eq!(s.decoded_audio_chunks, 1);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn audio_video_decode_steps() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "pb-av-{}.528",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_av_528(&p);
        let mut s = PlaybackSession::open(&p).unwrap();
        let e0 = s.decode_next_step().unwrap();
        let e1 = s.decode_next_step().unwrap();
        assert!(matches!(e0, PlaybackEvent::Video(_)));
        assert!(matches!(e1, PlaybackEvent::Audio(_)));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn stop_resets() {
        let dir = std::env::temp_dir();
        let p = dir.join("pb-stop.528");
        write_video_only_528(&p);
        let mut s = PlaybackSession::open(&p).unwrap();
        let _ = s.decode_next_video_frame().unwrap();
        s.stop().unwrap();
        assert_eq!(s.decoded_video_frames, 0);
        assert_eq!(s.position().pts_ticks, 0);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn pause_preserves_counters() {
        let dir = std::env::temp_dir();
        let p = dir.join("pb-pause.528");
        write_video_only_528(&p);
        let mut s = PlaybackSession::open(&p).unwrap();
        let _ = s.decode_next_video_frame().unwrap();
        let pos = s.position().pts_ticks;
        s.pause();
        assert_eq!(s.position().pts_ticks, pos);
        assert_eq!(s.decoded_video_frames, 1);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn unsupported_codec_fails_open() {
        let dir = std::env::temp_dir();
        let p = dir.join("pb-badcodec.528");
        let tracks = vec![TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 99,
            flags: 0,
            timescale: 90_000,
            config: [8u32.to_le_bytes(), 8u32.to_le_bytes()].concat(),
        }];
        let f = File::create(&p).unwrap();
        let mut mux = MuxWriter::new(f, FileHeader::new(1, 4), tracks).unwrap();
        mux.write_packet(1, 0, 0, true, b"x").unwrap();
        mux.finalize().unwrap();
        let err = PlaybackSession::open(&p).unwrap_err();
        assert!(matches!(err, PlaybackError::UnsupportedCodec { .. }));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn huge_dimensions_fail_open() {
        let dir = std::env::temp_dir();
        let p = dir.join("pb-huge.528");
        let w = MAX_VIDEO_SIDE + 1;
        let h = 4u32;
        let tracks = vec![TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 1,
            flags: 0,
            timescale: 90_000,
            config: [w.to_le_bytes(), h.to_le_bytes()].concat(),
        }];
        let f = File::create(&p).unwrap();
        let mut mux = MuxWriter::new(f, FileHeader::new(1, 4), tracks).unwrap();
        mux.write_packet(1, 0, 0, true, b"x").unwrap();
        mux.finalize().unwrap();
        let err = PlaybackSession::open(&p).unwrap_err();
        assert!(matches!(err, PlaybackError::Malformed(_)));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn seek_replays_first_video_by_crc() {
        let dir = std::env::temp_dir();
        let p = dir.join("pb-seek.528");
        write_two_frame_video_528(&p);
        let mut s = PlaybackSession::open(&p).unwrap();
        assert!(s.seek_supported());
        let a = match s.decode_next_step().unwrap() {
            PlaybackEvent::Video(f) => f,
            other => panic!("expected video, got {other:?}"),
        };
        s.seek_ms(0).unwrap();
        let b = match s.decode_next_step().unwrap() {
            PlaybackEvent::Video(f) => f,
            other => panic!("expected video, got {other:?}"),
        };
        assert_eq!(a.payload_crc32c, b.payload_crc32c);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn bad_video_payload_decode_errors() {
        let dir = std::env::temp_dir();
        let p = dir.join("pb-badpayload.528");
        let w = 8u32;
        let tracks = vec![TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 1,
            flags: 0,
            timescale: 90_000,
            config: [w.to_le_bytes(), w.to_le_bytes()].concat(),
        }];
        let f = File::create(&p).unwrap();
        let mut mux = MuxWriter::new(f, FileHeader::new(1, 4), tracks).unwrap();
        mux.write_packet(1, 0, 0, true, b"\xde\xad\xbe\xef")
            .unwrap();
        mux.finalize().unwrap();
        let mut s = PlaybackSession::open(&p).unwrap();
        let err = s.decode_next_video_frame().unwrap_err();
        assert!(matches!(err, PlaybackError::VideoDecode(_)));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn truncated_file_errors_on_decode() {
        let dir = std::env::temp_dir();
        let p = dir.join("pb-trunc.528");
        write_video_only_528(&p);
        let bytes = std::fs::read(&p).unwrap();
        let cut = bytes.len().saturating_sub(24).max(1);
        std::fs::write(&p, &bytes[..cut]).unwrap();
        let mut s = match PlaybackSession::open(&p) {
            Ok(s) => s,
            Err(_) => {
                std::fs::remove_file(&p).ok();
                return;
            }
        };
        assert!(s.decode_next_video_frame().is_err());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn empty_stream_seek_unsupported() {
        let dir = std::env::temp_dir();
        let p = dir.join("pb-empty-packets.528");
        let w = 8u32;
        let tracks = vec![TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 1,
            flags: 0,
            timescale: 90_000,
            config: [w.to_le_bytes(), w.to_le_bytes()].concat(),
        }];
        let f = File::create(&p).unwrap();
        let mux = MuxWriter::new(f, FileHeader::new(1, 4), tracks).unwrap();
        mux.finalize().unwrap();
        let mut s = PlaybackSession::open(&p).unwrap();
        assert!(!s.seek_supported());
        assert!(matches!(s.seek_ms(0), Err(PlaybackError::SeekUnsupported)));
        std::fs::remove_file(&p).ok();
    }
}
