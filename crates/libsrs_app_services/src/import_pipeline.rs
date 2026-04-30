//! Normalized import: `MediaDecoder` → `NativeEncoderSink` (mux + native codec frames).

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use libsrs_audio::{
    decode_frame_with_stream_version, encode_frame as audio_encode_frame, AudioFrame,
    STREAM_VERSION_V2,
};
use libsrs_compat::{ProbeResult, SourcePacket};
use libsrs_container::{FileHeader, TrackDescriptor, TrackKind};
use libsrs_contract::MediaKind;
use libsrs_mux::MuxWriter;
use libsrs_pipeline::{
    DecodedAudioFrame, DecodedVideoFrame, MediaDecoder, NativeEncoderSink, TranscodePipeline,
};
use libsrs_video::{
    classify_srsv2_payload, decode_frame as video_decode_frame, decode_sequence_header_v2,
    decode_yuv420_srsv2_payload, encode_frame as video_encode_frame, encode_sequence_header_v2,
    encode_yuv420_inter_payload, gray8_packed_to_yuv420p8_neutral, FrameType, SrsV2EncodeSettings,
    Srsv2PayloadKind, VideoFrame, VideoSequenceHeaderV2, YuvFrame, SEQUENCE_HEADER_BYTES,
};

use crate::Native528VideoCodec;

/// Quantizer for SRSV2 frames produced by import (normalized grayscale → YUV420).
const IMPORT_SRSV2_QP: u8 = 28;
/// Periodic I-frame when emitting **P** frames during import (`encode_yuv420_inter_payload`).
const IMPORT_SRSV2_KEYFRAME_INTERVAL: u32 = 30;

fn import_srsv2_encode_settings() -> SrsV2EncodeSettings {
    SrsV2EncodeSettings {
        quantizer: IMPORT_SRSV2_QP,
        keyframe_interval: IMPORT_SRSV2_KEYFRAME_INTERVAL,
        motion_search_radius: 16,
        ..Default::default()
    }
}

pub(crate) fn run_native_import(
    pipeline: &TranscodePipeline,
    input: &Path,
    output: &Path,
    video_codec: Native528VideoCodec,
) -> Result<usize> {
    let probe = pipeline.analyze_source(input)?;
    let input_ext = input
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());

    let mut ingestor = pipeline.create_ingestor();
    ingestor.open_path(input)?;
    let mut packets: Vec<SourcePacket> = Vec::new();
    while let Some(p) = ingestor.read_packet()? {
        packets.push(p);
    }
    ingestor.close()?;
    if packets.is_empty() {
        return Ok(0);
    }
    let n = packets.len();

    let (tracks, stream_to_mux) = build_import_mux_tracks(&probe, &packets, video_codec)?;
    let video_raw = input_ext.as_deref() == Some("srsv");
    let audio_raw = input_ext.as_deref() == Some("srsa");
    let mut decoder = NativeSrsMediaDecoder::from_probe(&probe, video_raw, audio_raw)?;
    let mut sink = MuxNativeImportSink::new(output, tracks, stream_to_mux)?;

    let video_id = probe
        .tracks
        .iter()
        .find(|t| t.kind == MediaKind::Video)
        .map(|t| t.id.0);
    let audio_id = probe
        .tracks
        .iter()
        .find(|t| t.kind == MediaKind::Audio)
        .map(|t| t.id.0);

    for p in packets {
        let pkt = p.packet;
        let stream = pkt.stream_id.0;
        if Some(stream) == video_id {
            let frame = decoder.decode_video_packet(&pkt.data)?;
            let pts = pkt
                .pts
                .map(|t| timestamp_to_mux_ticks(t, sink.video_timescale));
            sink.push_video(&frame, pts)?;
        } else if Some(stream) == audio_id {
            let frame = decoder.decode_audio_packet(&pkt.data)?;
            let pts = pkt
                .pts
                .map(|t| timestamp_to_mux_ticks(t, sink.audio_timescale));
            sink.push_audio(&frame, pts)?;
        }
    }
    sink.finalize_mux()?;
    Ok(n)
}

fn timestamp_to_mux_ticks(ts: libsrs_contract::Timestamp, timescale: u32) -> u64 {
    if ts.timebase.den == 0 {
        return 0;
    }
    let v = (ts.ticks as i128)
        .saturating_mul(timescale as i128)
        .saturating_mul(ts.timebase.num as i128)
        / (ts.timebase.den as i128);
    v.max(0) as u64
}

fn build_import_mux_tracks(
    probe: &ProbeResult,
    packets: &[SourcePacket],
    video_codec: Native528VideoCodec,
) -> Result<(Vec<TrackDescriptor>, HashMap<u32, u16>)> {
    const DEFAULT_IMPORT_AUDIO_SAMPLE_RATE: u32 = 48_000;
    const DEFAULT_IMPORT_AUDIO_CHANNELS: u8 = 1;

    let vt = probe.tracks.iter().find(|t| t.kind == MediaKind::Video);
    let at = probe.tracks.iter().find(|t| t.kind == MediaKind::Audio);
    if vt.is_none() && at.is_none() {
        return Err(anyhow!("no muxable audio/video tracks in probe"));
    }

    let mut stream_to_mux = HashMap::new();
    let mut descriptors = Vec::new();
    let mut next_mux_id = 1u16;

    if let Some(v) = vt {
        if !packets.iter().any(|p| p.packet.stream_id.0 == v.id.0) {
            return Err(anyhow!("no video packets for import"));
        }
        let width = v
            .video_width
            .filter(|&x| x > 0)
            .ok_or_else(|| anyhow!("probe missing native video width"))?;
        let height = v
            .video_height
            .filter(|&x| x > 0)
            .ok_or_else(|| anyhow!("probe missing native video height"))?;
        let (codec_id, config) = match video_codec {
            Native528VideoCodec::Srsv2 => {
                let seq =
                    VideoSequenceHeaderV2::intra_main_yuv420_bt709_limited_one_ref(width, height);
                (3_u16, encode_sequence_header_v2(&seq).to_vec())
            }
            Native528VideoCodec::Srsv1Legacy => {
                let mut c = Vec::new();
                c.extend_from_slice(&width.to_le_bytes());
                c.extend_from_slice(&height.to_le_bytes());
                (1_u16, c)
            }
        };
        descriptors.push(TrackDescriptor {
            track_id: next_mux_id,
            kind: TrackKind::Video,
            codec_id,
            flags: 0,
            timescale: 90_000,
            config,
        });
        stream_to_mux.insert(v.id.0, next_mux_id);
        next_mux_id += 1;
    }

    if let Some(a) = at {
        if !packets.iter().any(|p| p.packet.stream_id.0 == a.id.0) {
            return Err(anyhow!("no audio packets for import"));
        }
        let sample_rate = a
            .audio_sample_rate
            .filter(|&r| r > 0)
            .unwrap_or(DEFAULT_IMPORT_AUDIO_SAMPLE_RATE);
        let channels = a.audio_channels.unwrap_or(DEFAULT_IMPORT_AUDIO_CHANNELS);
        if channels != 1 && channels != 2 {
            return Err(anyhow!(
                "import supports mono or stereo native audio (probe reported {} channels)",
                channels
            ));
        }
        let mut config = Vec::new();
        config.extend_from_slice(&sample_rate.to_le_bytes());
        config.push(channels);
        descriptors.push(TrackDescriptor {
            track_id: next_mux_id,
            kind: TrackKind::Audio,
            codec_id: 2,
            flags: 0,
            timescale: sample_rate,
            config,
        });
        stream_to_mux.insert(a.id.0, next_mux_id);
    }

    Ok((descriptors, stream_to_mux))
}

struct NativeSrsMediaDecoder {
    video_width: u32,
    video_height: u32,
    video_payload_is_raw_srsv_elementary: bool,
    next_video_index: u32,
    audio_sample_rate: u32,
    audio_channels: u8,
    audio_payload_is_raw_pcm_srsa_elementary: bool,
    next_audio_index: u32,
}

impl NativeSrsMediaDecoder {
    fn from_probe(probe: &ProbeResult, video_raw: bool, audio_raw: bool) -> Result<Self> {
        let v = probe.tracks.iter().find(|t| t.kind == MediaKind::Video);
        let a = probe.tracks.iter().find(|t| t.kind == MediaKind::Audio);

        let mut video_width = 0u32;
        let mut video_height = 0u32;
        if v.is_some() {
            video_width = v
                .and_then(|t| t.video_width)
                .filter(|&x| x > 0)
                .ok_or_else(|| anyhow!("video track missing width in probe"))?;
            video_height = v
                .and_then(|t| t.video_height)
                .filter(|&x| x > 0)
                .ok_or_else(|| anyhow!("video track missing height in probe"))?;
        }

        let mut audio_sample_rate = 48_000u32;
        let mut audio_channels = 1u8;
        if a.is_some() {
            audio_sample_rate = a
                .and_then(|t| t.audio_sample_rate)
                .filter(|&r| r > 0)
                .unwrap_or(48_000);
            audio_channels = a.and_then(|t| t.audio_channels).unwrap_or(1);
            if audio_channels != 1 && audio_channels != 2 {
                return Err(anyhow!(
                    "native import audio supports mono or stereo (got {})",
                    audio_channels
                ));
            }
        }

        Ok(Self {
            video_width,
            video_height,
            video_payload_is_raw_srsv_elementary: video_raw,
            next_video_index: 0,
            audio_sample_rate,
            audio_channels,
            audio_payload_is_raw_pcm_srsa_elementary: audio_raw,
            next_audio_index: 0,
        })
    }
}

impl MediaDecoder for NativeSrsMediaDecoder {
    fn decode_video_packet(&mut self, payload: &[u8]) -> Result<VideoFrame> {
        let idx = self.next_video_index;
        self.next_video_index = self.next_video_index.wrapping_add(1);
        if self.video_payload_is_raw_srsv_elementary {
            let expected = (self.video_width as usize).saturating_mul(self.video_height as usize);
            if payload.len() != expected {
                return Err(anyhow!(
                    "raw video packet length {} != {}x{}",
                    payload.len(),
                    self.video_width,
                    self.video_height
                ));
            }
            Ok(VideoFrame {
                width: self.video_width,
                height: self.video_height,
                frame_index: idx,
                frame_type: FrameType::I,
                data: payload.to_vec(),
            })
        } else {
            Ok(video_decode_frame(
                self.video_width,
                self.video_height,
                idx,
                FrameType::I,
                payload,
            )?)
        }
    }

    fn decode_audio_packet(&mut self, payload: &[u8]) -> Result<AudioFrame> {
        let idx = self.next_audio_index;
        self.next_audio_index = self.next_audio_index.wrapping_add(1);
        if self.audio_payload_is_raw_pcm_srsa_elementary {
            let ch = self.audio_channels as usize;
            if ch == 0 || payload.len() % (2 * ch) != 0 {
                return Err(anyhow!(
                    "PCM payload length {} is not a multiple of {} byte frames",
                    payload.len(),
                    2 * ch
                ));
            }
            let samples = payload
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            Ok(AudioFrame {
                sample_rate: self.audio_sample_rate,
                channels: self.audio_channels,
                frame_index: idx,
                samples,
            })
        } else {
            Ok(decode_frame_with_stream_version(
                self.audio_sample_rate,
                idx,
                payload,
                STREAM_VERSION_V2,
            )?)
        }
    }
}

struct MuxNativeImportSink {
    mux: Option<MuxWriter<File>>,
    video_mux_id: Option<u16>,
    audio_mux_id: Option<u16>,
    video_timescale: u32,
    audio_timescale: u32,
    next_video_pts: u64,
    next_audio_pts: u64,
    /// `1` = SRSV1 legacy packet payloads; `3` = SRSV2 (`encode_yuv420_inter_payload` + ref refresh).
    video_codec_id: u16,
    video_seq_v2: Option<VideoSequenceHeaderV2>,
    /// Previous **decoded** SRSV2 frame — matches playback `decode_yuv420_srsv2_payload` state.
    srsv2_decoded_ref: Option<YuvFrame>,
}

impl MuxNativeImportSink {
    fn new(
        output: &Path,
        tracks: Vec<TrackDescriptor>,
        _stream_to_mux: HashMap<u32, u16>,
    ) -> Result<Self> {
        let mut video_mux_id = None;
        let mut audio_mux_id = None;
        let mut video_timescale = 90_000u32;
        let mut audio_timescale = 48_000u32;

        let mut video_codec_id = 0_u16;
        let mut video_seq_v2 = None::<VideoSequenceHeaderV2>;
        for t in &tracks {
            match t.kind {
                TrackKind::Video => {
                    video_mux_id = Some(t.track_id);
                    video_timescale = t.timescale;
                    video_codec_id = t.codec_id;
                    if t.codec_id == 3 {
                        if t.config.len() < SEQUENCE_HEADER_BYTES {
                            return Err(anyhow!(
                                "SRSV2 video track config must embed {}-byte sequence header",
                                SEQUENCE_HEADER_BYTES
                            ));
                        }
                        video_seq_v2 = Some(
                            decode_sequence_header_v2(&t.config[..SEQUENCE_HEADER_BYTES])
                                .map_err(|e| anyhow!("SRSV2 sequence header in mux sink: {}", e))?,
                        );
                    }
                }
                TrackKind::Audio => {
                    audio_mux_id = Some(t.track_id);
                    audio_timescale = t.timescale;
                }
                TrackKind::Data
                | TrackKind::Subtitle
                | TrackKind::Metadata
                | TrackKind::Attachment => {}
            }
        }

        let mux = MuxWriter::new(
            File::create(output).with_context(|| format!("create {}", output.display()))?,
            FileHeader::new(u16::try_from(tracks.len())?, 8),
            tracks,
        )?;

        Ok(Self {
            mux: Some(mux),
            video_mux_id,
            audio_mux_id,
            video_timescale,
            audio_timescale,
            next_video_pts: 0,
            next_audio_pts: 0,
            video_codec_id,
            video_seq_v2,
            srsv2_decoded_ref: None,
        })
    }
}

impl NativeEncoderSink for MuxNativeImportSink {
    fn push_video(&mut self, frame: &dyn DecodedVideoFrame, pts_ticks: Option<u64>) -> Result<()> {
        let mux_id = self
            .video_mux_id
            .ok_or_else(|| anyhow!("video track not configured"))?;
        let mux = self
            .mux
            .as_mut()
            .ok_or_else(|| anyhow!("mux already finalized"))?;

        let vf = VideoFrame {
            width: frame.width(),
            height: frame.height(),
            frame_index: frame.frame_index(),
            frame_type: frame.frame_type(),
            data: frame.gray8_pixels().to_vec(),
        };
        let payload = if self.video_codec_id == 3 {
            let seq = self
                .video_seq_v2
                .as_ref()
                .ok_or_else(|| anyhow!("SRSV2 mux sink missing sequence header state"))?;
            let yuv = gray8_packed_to_yuv420p8_neutral(&vf.data, vf.width, vf.height)
                .map_err(|e| anyhow!("grayscale to YUV420: {}", e))?;
            let settings = import_srsv2_encode_settings();
            let enc = encode_yuv420_inter_payload(
                seq,
                &yuv,
                self.srsv2_decoded_ref.as_ref(),
                vf.frame_index,
                IMPORT_SRSV2_QP,
                &settings,
            )
            .map_err(|e| anyhow!("SRSV2 encode: {}", e))?;
            decode_yuv420_srsv2_payload(seq, &enc, &mut self.srsv2_decoded_ref)
                .map_err(|e| anyhow!("SRSV2 reference refresh (must match decode): {}", e))?;
            enc
        } else {
            video_encode_frame(&vf)?
        };
        let pts = pts_ticks.unwrap_or(self.next_video_pts);
        let is_keyframe = if self.video_codec_id == 3 {
            match classify_srsv2_payload(&payload)
                .map_err(|e| anyhow!("SRSV2 payload classification: {e}"))?
            {
                Srsv2PayloadKind::Intra => true,
                Srsv2PayloadKind::Predicted => false,
                Srsv2PayloadKind::Unknown => {
                    return Err(anyhow!(
                        "unsupported SRSV2 FR2 revision in mux path; refusing to mux"
                    ));
                }
            }
        } else {
            true
        };
        mux.write_packet(mux_id, pts, pts, is_keyframe, &payload)?;
        self.next_video_pts = pts.saturating_add(3_000);
        Ok(())
    }

    fn push_audio(&mut self, frame: &dyn DecodedAudioFrame, pts_ticks: Option<u64>) -> Result<()> {
        let mux_id = self
            .audio_mux_id
            .ok_or_else(|| anyhow!("audio track not configured"))?;
        let mux = self
            .mux
            .as_mut()
            .ok_or_else(|| anyhow!("mux already finalized"))?;

        let af = AudioFrame {
            sample_rate: frame.sample_rate(),
            channels: frame.channels(),
            frame_index: frame.frame_index(),
            samples: frame.samples_i16_interleaved().to_vec(),
        };
        let sample_count = u64::from(af.sample_count_per_channel()?);
        let payload = audio_encode_frame(&af)?;
        let pts = pts_ticks.unwrap_or(self.next_audio_pts);
        mux.write_packet(mux_id, pts, pts, true, &payload)?;
        self.next_audio_pts = pts.saturating_add(sample_count);
        Ok(())
    }

    fn finalize_mux(&mut self) -> Result<()> {
        if let Some(mux) = self.mux.take() {
            let _ = mux.finalize()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use libsrs_container::TrackKind;
    use libsrs_licensing_proto::{EntitlementClaims, EntitlementStatus, LicensedFeature};
    use libsrs_mux::MuxWriter;
    use libsrs_video::{decode_sequence_header_v2, decode_yuv420_srsv2_payload};
    use std::io::Cursor;

    fn editor_claims() -> EntitlementClaims {
        EntitlementClaims {
            license_id: "license-1".to_string(),
            key_id: "key-1".to_string(),
            features: LicensedFeature::editor_defaults(),
            status: EntitlementStatus::Active,
            issued_at_epoch_s: 1,
            expires_at_epoch_s: 2,
            device_install_id: "install-1".to_string(),
            message: "editor".to_string(),
            replacement_key: None,
        }
    }

    /// Import defaults to SRSV2 (`codec_id` 3); decoded YUV luma must stay non-flat across frames.
    #[test]
    fn native_import_roundtrip_retains_video_variance() {
        let dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let src = dir.join(format!("import-src-{nanos}.528"));
        let dst = dir.join(format!("import-dst-{nanos}.528"));

        let w = 16u32;
        let h = 16u32;
        let mut pix_a: Vec<u8> = (0..(w * h)).map(|i| (i * 7) as u8).collect();
        let mut pix_b: Vec<u8> = (0..(w * h))
            .map(|i| (i + 41).wrapping_mul(13) as u8)
            .collect();

        let f0 = VideoFrame {
            width: w,
            height: h,
            frame_index: 0,
            frame_type: FrameType::I,
            data: std::mem::take(&mut pix_a),
        };
        let f1 = VideoFrame {
            width: w,
            height: h,
            frame_index: 1,
            frame_type: FrameType::I,
            data: std::mem::take(&mut pix_b),
        };
        let e0 = video_encode_frame(&f0).unwrap();
        let e1 = video_encode_frame(&f1).unwrap();

        let tracks = vec![TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 1,
            flags: 0,
            timescale: 90_000,
            config: [w.to_le_bytes(), h.to_le_bytes()].concat(),
        }];
        let file = File::create(&src).unwrap();
        let mut mux = MuxWriter::new(file, FileHeader::new(1, 4), tracks).unwrap();
        mux.write_packet(1, 0, 0, true, &e0).unwrap();
        mux.write_packet(1, 3_000, 3_000, true, &e1).unwrap();
        mux.finalize().unwrap();

        let svc = crate::AppServices::default();
        let claims = editor_claims();
        svc.import_to_native(&src, &dst, &claims).unwrap();

        let out_bytes = std::fs::read(&dst).unwrap();
        let mut demux = libsrs_demux::DemuxReader::open(Cursor::new(out_bytes)).unwrap();
        demux.rebuild_index().unwrap();
        use libsrs_container::PacketFlags;
        let vid = demux
            .tracks()
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .expect("video track")
            .track_id;
        let ix0 = demux
            .index()
            .iter()
            .find(|e| e.track_id == vid && e.pts == 0)
            .unwrap();
        let ix1 = demux
            .index()
            .iter()
            .find(|e| e.track_id == vid && e.pts == 3_000)
            .unwrap();
        assert_ne!(
            ix0.flags & PacketFlags::KEYFRAME,
            0,
            "intra must be keyframe"
        );
        assert_eq!(
            ix1.flags & PacketFlags::KEYFRAME,
            0,
            "P-frame must not be indexed as keyframe"
        );
        let vt = demux
            .tracks()
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .expect("video track");
        assert_eq!(vt.codec_id, 3, "default import must mux SRSV2");
        let seq = decode_sequence_header_v2(&vt.config[..SEQUENCE_HEADER_BYTES]).unwrap();
        assert_eq!(seq.max_ref_frames, 1);
        let p0 = demux.next_packet().unwrap().unwrap();
        let p1 = demux.next_packet().unwrap().unwrap();
        assert_ne!(
            p0.packet.header.flags & PacketFlags::KEYFRAME,
            0,
            "first SRSV2 packet header must carry KEYFRAME"
        );
        assert_eq!(
            p1.packet.header.flags & PacketFlags::KEYFRAME,
            0,
            "predicted SRSV2 packet must not carry KEYFRAME"
        );
        assert_eq!(
            p0.packet.payload[3], 1,
            "first SRSV2 video packet should be intra"
        );
        assert_eq!(
            p1.packet.payload[3], 2,
            "second frame should be P (import uses inter encode)"
        );
        let mut slot = None;
        let d0 =
            decode_yuv420_srsv2_payload(&seq, &p0.packet.payload, &mut slot).expect("srsv2 f0");
        let d1 =
            decode_yuv420_srsv2_payload(&seq, &p1.packet.payload, &mut slot).expect("srsv2 f1");

        let min0 = d0.yuv.y.samples.iter().copied().min().unwrap();
        let max0 = d0.yuv.y.samples.iter().copied().max().unwrap();
        assert!(
            max0.saturating_sub(min0) > 10,
            "frame 0 should not be near-constant placeholder"
        );
        assert_ne!(
            d0.yuv.y.samples, d1.yuv.y.samples,
            "successive imported frames must differ (real normalized path)"
        );

        std::fs::remove_file(&src).ok();
        std::fs::remove_file(&dst).ok();
    }

    #[test]
    fn native_import_legacy_srsv1_mux_track() {
        let dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let src = dir.join(format!("import-src-leg-{nanos}.528"));
        let dst = dir.join(format!("import-dst-leg-{nanos}.528"));

        let w = 16u32;
        let h = 16u32;
        let mut pix_a: Vec<u8> = (0..(w * h)).map(|i| (i * 7) as u8).collect();
        let f0 = VideoFrame {
            width: w,
            height: h,
            frame_index: 0,
            frame_type: FrameType::I,
            data: std::mem::take(&mut pix_a),
        };
        let e0 = video_encode_frame(&f0).unwrap();
        let tracks = vec![TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 1,
            flags: 0,
            timescale: 90_000,
            config: [w.to_le_bytes(), h.to_le_bytes()].concat(),
        }];
        let file = File::create(&src).unwrap();
        let mut mux = MuxWriter::new(file, FileHeader::new(1, 4), tracks).unwrap();
        mux.write_packet(1, 0, 0, true, &e0).unwrap();
        mux.finalize().unwrap();

        let svc = crate::AppServices::default();
        let claims = editor_claims();
        svc.import_to_native_with_video_codec(
            &src,
            &dst,
            &claims,
            crate::Native528VideoCodec::Srsv1Legacy,
        )
        .unwrap();

        let out_bytes = std::fs::read(&dst).unwrap();
        let demux = libsrs_demux::DemuxReader::open(Cursor::new(out_bytes)).unwrap();
        let vt = demux
            .tracks()
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .unwrap();
        assert_eq!(vt.codec_id, 1);

        std::fs::remove_file(&src).ok();
        std::fs::remove_file(&dst).ok();
    }
}
