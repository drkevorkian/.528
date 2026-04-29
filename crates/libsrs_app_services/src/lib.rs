use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use libsrs_audio::{
    decode_frame_with_stream_version, AudioFrame, AudioStreamReader, AudioStreamWriter,
    STREAM_VERSION_V2,
};
use libsrs_compat::{ProbeResult, SourcePacket};
use libsrs_container::{FileHeader, TrackDescriptor, TrackKind};
use libsrs_contract::{CodecType, MediaKind, Packet};
use libsrs_demux::DemuxReader;
use libsrs_licensing_proto::{EntitlementClaims, LicensedFeature};
use libsrs_mux::MuxWriter;
use libsrs_pipeline::{NativeTranscoder, TranscodePipeline};
use libsrs_video::{FrameType, VideoFrame, VideoStreamReader, VideoStreamWriter};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct AppServices {
    pipeline: TranscodePipeline,
}

impl Default for AppServices {
    fn default() -> Self {
        Self::new(TranscodePipeline::default())
    }
}

impl AppServices {
    pub const fn new(pipeline: TranscodePipeline) -> Self {
        Self { pipeline }
    }

    pub fn pipeline(&self) -> &TranscodePipeline {
        &self.pipeline
    }

    pub fn inspect_media<P: AsRef<Path>>(&self, input: P) -> Result<MediaInspection> {
        let input = input.as_ref();
        match extension(input) {
            Some("srsv") => inspect_native_video(input),
            Some("srsa") => inspect_native_audio(input),
            Some("528") | Some("srsm") => inspect_native_container(input),
            _ => inspect_foreign_source(&self.pipeline, input),
        }
    }

    pub fn encode_input_to_native<P: AsRef<Path>>(
        &self,
        input: P,
        output: P,
        entitlement: &EntitlementClaims,
    ) -> Result<()> {
        require_editor_feature(entitlement, LicensedFeature::Encode, "encode")?;
        encode_input_to_native(input.as_ref(), output.as_ref())
    }

    pub fn decode_native_to_raw<P: AsRef<Path>>(
        &self,
        input: P,
        output: P,
        entitlement: &EntitlementClaims,
    ) -> Result<()> {
        require_editor_feature(entitlement, LicensedFeature::Decode, "decode")?;
        decode_native_to_raw(input.as_ref(), output.as_ref())
    }

    pub fn mux_elementary_streams<P: AsRef<Path>>(
        &self,
        input: P,
        output: P,
        entitlement: &EntitlementClaims,
    ) -> Result<()> {
        require_editor_feature(entitlement, LicensedFeature::Mux, "mux")?;
        mux_elementary_streams(input.as_ref(), output.as_ref())
    }

    pub fn demux_container_to_elementary<P: AsRef<Path>>(
        &self,
        input: P,
        output_stem: P,
        entitlement: &EntitlementClaims,
    ) -> Result<()> {
        require_editor_feature(entitlement, LicensedFeature::Demux, "demux")?;
        demux_container_to_elementary(input.as_ref(), output_stem.as_ref())
    }

    pub fn import_to_native<P: AsRef<Path>>(
        &self,
        input: P,
        output: P,
        entitlement: &EntitlementClaims,
    ) -> Result<usize> {
        require_editor_feature(entitlement, LicensedFeature::Import, "import")?;
        self.ensure_supported_for_conversion(input.as_ref())?;
        run_native_import(&self.pipeline, input.as_ref(), output.as_ref())
    }

    pub fn transcode_to_native<P: AsRef<Path>>(
        &self,
        input: P,
        output: P,
        entitlement: &EntitlementClaims,
    ) -> Result<usize> {
        require_editor_feature(entitlement, LicensedFeature::Transcode, "transcode")?;
        self.ensure_supported_for_conversion(input.as_ref())?;
        run_native_import(&self.pipeline, input.as_ref(), output.as_ref())
    }

    pub fn ensure_supported_for_conversion(&self, input: &Path) -> Result<()> {
        let inspection = self.inspect_media(input)?;
        let unsupported = inspection
            .tracks
            .iter()
            .filter(|track| !track.supported_without_license)
            .map(|track| format!("track {} {}", track.id, track.codec))
            .collect::<Vec<_>>();
        if unsupported.is_empty() {
            Ok(())
        } else {
            Err(AppServicesError::UnsupportedCodecPolicy {
                tracks: unsupported.join(", "),
            }
            .into())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInspection {
    pub format_name: String,
    pub duration_ms: Option<u64>,
    pub summary: String,
    pub tracks: Vec<TrackSummary>,
    pub packet_count: Option<u64>,
    pub frame_count: Option<u64>,
    pub index_entries: Option<usize>,
}

impl MediaInspection {
    pub fn duration_for_ui(&self) -> u64 {
        self.duration_ms.unwrap_or(5_000)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSummary {
    pub id: u32,
    pub kind: String,
    pub codec: String,
    pub role: String,
    pub detail: String,
    pub supported_without_license: bool,
}

#[derive(Debug, Error)]
pub enum AppServicesError {
    #[error("editor entitlement required for {operation}")]
    EditorEntitlementRequired { operation: &'static str },
    #[error("input contains unsupported or license-encumbered codecs: {tracks}")]
    UnsupportedCodecPolicy { tracks: String },
}

fn require_editor_feature(
    entitlement: &EntitlementClaims,
    feature: LicensedFeature,
    operation: &'static str,
) -> Result<()> {
    let allowed = entitlement.allows_feature(feature)
        || entitlement.allows_feature(LicensedFeature::EditorWorkspace);
    if allowed {
        Ok(())
    } else {
        Err(AppServicesError::EditorEntitlementRequired { operation }.into())
    }
}

fn inspect_foreign_source(pipeline: &TranscodePipeline, input: &Path) -> Result<MediaInspection> {
    let metadata = pipeline.analyze_source(input)?;
    let tracks = metadata
        .tracks
        .iter()
        .map(|track| TrackSummary {
            id: track.id.0,
            kind: format!("{:?}", track.kind),
            codec: track.codec.display_name().to_string(),
            role: format!("{:?}", track.role),
            detail: codec_policy_detail(track.codec, track.language.as_deref()),
            supported_without_license: track.codec.is_royalty_free_playback_allowed(),
        })
        .collect::<Vec<_>>();
    Ok(MediaInspection {
        format_name: metadata.format_name.clone(),
        duration_ms: metadata.duration_ms,
        summary: format!(
            "foreign format: {} (tracks={})",
            metadata.format_name,
            metadata.tracks.len()
        ),
        tracks,
        packet_count: None,
        frame_count: None,
        index_entries: None,
    })
}

fn inspect_native_video(input: &Path) -> Result<MediaInspection> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut reader = VideoStreamReader::new(BufReader::new(file))?;
    let mut frame_count = 0_u64;
    while reader.read_next_frame()?.is_some() {
        frame_count += 1;
    }
    Ok(MediaInspection {
        format_name: "srsv".to_string(),
        duration_ms: Some(frame_count.saturating_mul(40)),
        summary: format!(
            "native video: {}x{}, frames={frame_count}",
            reader.header.width, reader.header.height
        ),
        tracks: vec![TrackSummary {
            id: 0,
            kind: "Video".to_string(),
            codec: CodecType::NativeSrsVideo.display_name().to_string(),
            role: "Primary".to_string(),
            detail: format!("{}x{}", reader.header.width, reader.header.height),
            supported_without_license: true,
        }],
        packet_count: None,
        frame_count: Some(frame_count),
        index_entries: None,
    })
}

fn inspect_native_audio(input: &Path) -> Result<MediaInspection> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut reader = AudioStreamReader::new(BufReader::new(file))?;
    let mut frame_count = 0_u64;
    let mut total_samples_per_channel = 0_u64;
    while let Some(frame) = reader.read_next_frame()? {
        frame_count += 1;
        total_samples_per_channel += u64::from(frame.sample_count_per_channel()?);
    }
    let duration_ms = if reader.header.sample_rate == 0 {
        None
    } else {
        Some(total_samples_per_channel.saturating_mul(1_000) / u64::from(reader.header.sample_rate))
    };
    Ok(MediaInspection {
        format_name: "srsa".to_string(),
        duration_ms,
        summary: format!(
            "native audio: sample_rate={}, channels={}, frames={frame_count}",
            reader.header.sample_rate, reader.header.channels
        ),
        tracks: vec![TrackSummary {
            id: 0,
            kind: "Audio".to_string(),
            codec: CodecType::NativeSrsAudio.display_name().to_string(),
            role: "Primary".to_string(),
            detail: format!(
                "{} Hz / {} channel(s)",
                reader.header.sample_rate, reader.header.channels
            ),
            supported_without_license: true,
        }],
        packet_count: None,
        frame_count: Some(frame_count),
        index_entries: None,
    })
}

fn inspect_native_container(input: &Path) -> Result<MediaInspection> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut demux = DemuxReader::open(BufReader::new(file))?;
    demux.rebuild_index()?;

    let timescales = demux
        .tracks()
        .iter()
        .map(|track| (track.track_id, track.timescale.max(1)))
        .collect::<HashMap<_, _>>();
    let duration_ms = demux
        .index()
        .iter()
        .map(|entry| {
            let timescale = timescales.get(&entry.track_id).copied().unwrap_or(1_000);
            entry.pts.saturating_mul(1_000) / u64::from(timescale)
        })
        .max();

    let mut packet_count = 0_u64;
    demux.reset_to_data_start()?;
    while demux.next_packet()?.is_some() {
        packet_count += 1;
    }

    let tracks = demux
        .tracks()
        .iter()
        .map(|track| TrackSummary {
            id: u32::from(track.track_id),
            kind: format!("{:?}", track.kind),
            codec: container_codec_name(track.codec_id).to_string(),
            role: if track.track_id == 1 {
                "Primary".to_string()
            } else {
                "Alternate".to_string()
            },
            detail: format!(
                "timescale={} config={} bytes",
                track.timescale,
                track.config.len()
            ),
            supported_without_license: container_codec(track.codec_id)
                .is_royalty_free_playback_allowed(),
        })
        .collect::<Vec<_>>();

    Ok(MediaInspection {
        format_name: format!("528-container-v{}", demux.header().version),
        duration_ms,
        summary: format!(
            "native container: tracks={}, index_entries={}, packets={packet_count}",
            demux.tracks().len(),
            demux.index().len()
        ),
        tracks,
        packet_count: Some(packet_count),
        frame_count: None,
        index_entries: Some(demux.index().len()),
    })
}

pub fn royalty_free_codec_names() -> Vec<&'static str> {
    CodecType::royalty_free_codecs()
        .into_iter()
        .map(CodecType::display_name)
        .collect()
}

fn codec_policy_detail(codec: CodecType, language: Option<&str>) -> String {
    let language = language.unwrap_or("n/a");
    if codec.is_royalty_free_playback_allowed() {
        format!("language={language}; royalty-free playback/conversion allowed")
    } else if codec.requires_external_playback_license_attention() {
        format!("language={language}; blocked by codec licensing policy")
    } else {
        format!("language={language}; unknown codec support")
    }
}

fn container_codec(codec_id: u16) -> CodecType {
    match codec_id {
        1 => CodecType::NativeSrsVideo,
        2 => CodecType::NativeSrsAudio,
        _ => CodecType::Unknown,
    }
}

fn container_codec_name(codec_id: u16) -> &'static str {
    container_codec(codec_id).display_name()
}

fn encode_input_to_native(input: &Path, output: &Path) -> Result<()> {
    match extension(output) {
        Some("srsv") => encode_raw_video(input, output),
        Some("srsa") => encode_raw_audio(input, output),
        Some("528") | Some("srsm") => {
            let stem = output.with_extension("");
            let video_path = stem.with_extension("srsv");
            encode_raw_video(input, &video_path)?;
            mux_elementary_streams(&stem, output)
        }
        _ => Err(anyhow!(
            "unsupported output extension; expected .528 (primary), legacy .srsm, .srsv, or .srsa"
        )),
    }
}

fn decode_native_to_raw(input: &Path, output: &Path) -> Result<()> {
    match extension(input) {
        Some("srsv") => decode_video_to_raw(input, output),
        Some("srsa") => decode_audio_to_pcm(input, output),
        Some("528") | Some("srsm") => demux_container_to_elementary(input, output),
        _ => Err(anyhow!(
            "unsupported input extension; expected .528 (primary), legacy .srsm, .srsv, or .srsa"
        )),
    }
}

fn encode_raw_video(input: &Path, output: &Path) -> Result<()> {
    let bytes = std::fs::read(input).with_context(|| format!("read {}", input.display()))?;
    let side = infer_square(bytes.len())?;
    let frame = VideoFrame {
        width: side,
        height: side,
        frame_index: 0,
        frame_type: FrameType::I,
        data: bytes,
    };
    let file = File::create(output).with_context(|| format!("create {}", output.display()))?;
    let mut writer = VideoStreamWriter::new(file, side, side)?;
    let _ = writer.write_frame(&frame)?;
    Ok(())
}

fn encode_raw_audio(input: &Path, output: &Path) -> Result<()> {
    let bytes = std::fs::read(input).with_context(|| format!("read {}", input.display()))?;
    if bytes.len() % 2 != 0 {
        return Err(anyhow!(
            "raw PCM input must be 16-bit little-endian samples"
        ));
    }
    let samples = bytes
        .chunks_exact(2)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    let frame = AudioFrame {
        sample_rate: 48_000,
        channels: 1,
        frame_index: 0,
        samples,
    };
    let file = File::create(output).with_context(|| format!("create {}", output.display()))?;
    let mut writer = AudioStreamWriter::new(file, 48_000, 1)?;
    let _ = writer.write_frame(&frame)?;
    Ok(())
}

fn decode_video_to_raw(input: &Path, output: &Path) -> Result<()> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut reader = VideoStreamReader::new(BufReader::new(file))?;
    let mut out = Vec::new();
    while let Some(frame) = reader.read_next_frame()? {
        out.extend_from_slice(&frame.data);
    }
    std::fs::write(output, out).with_context(|| format!("write {}", output.display()))?;
    Ok(())
}

fn decode_audio_to_pcm(input: &Path, output: &Path) -> Result<()> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut reader = AudioStreamReader::new(BufReader::new(file))?;
    let mut out = Vec::new();
    while let Some(frame) = reader.read_next_frame()? {
        for sample in frame.samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }
    std::fs::write(output, out).with_context(|| format!("write {}", output.display()))?;
    Ok(())
}

fn mux_elementary_streams(input: &Path, output: &Path) -> Result<()> {
    let video_path = if extension(input) == Some("srsv") {
        input.to_path_buf()
    } else {
        input.with_extension("srsv")
    };
    let audio_path = if extension(input) == Some("srsa") {
        input.to_path_buf()
    } else {
        input.with_extension("srsa")
    };
    if !video_path.exists() && !audio_path.exists() {
        return Err(anyhow!(
            "no input streams found; expected {} and/or {}",
            video_path.display(),
            audio_path.display()
        ));
    }

    let mut tracks = Vec::new();
    if video_path.exists() {
        let file = File::open(&video_path)?;
        let reader = VideoStreamReader::new(BufReader::new(file))?;
        let mut config = Vec::new();
        config.extend_from_slice(&reader.header.width.to_le_bytes());
        config.extend_from_slice(&reader.header.height.to_le_bytes());
        tracks.push(TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 1,
            flags: 0,
            timescale: 90_000,
            config,
        });
    }
    if audio_path.exists() {
        let file = File::open(&audio_path)?;
        let reader = AudioStreamReader::new(BufReader::new(file))?;
        let mut config = Vec::new();
        config.extend_from_slice(&reader.header.sample_rate.to_le_bytes());
        config.push(reader.header.channels);
        tracks.push(TrackDescriptor {
            track_id: 2,
            kind: TrackKind::Audio,
            codec_id: 2,
            flags: 0,
            timescale: reader.header.sample_rate,
            config,
        });
    }

    let out_file = File::create(output).with_context(|| format!("create {}", output.display()))?;
    let header = FileHeader::new(u16::try_from(tracks.len())?, 8);
    let mut mux = MuxWriter::new(out_file, header, tracks)?;

    if video_path.exists() {
        let file = File::open(&video_path)?;
        let mut reader = VideoStreamReader::new(BufReader::new(file))?;
        let mut pts = 0_u64;
        while let Some(frame) = reader.read_next_frame()? {
            let payload = libsrs_video::encode_frame(&frame)?;
            mux.write_packet(1, pts, pts, true, &payload)?;
            pts = pts.saturating_add(3_000);
        }
    }

    if audio_path.exists() {
        let file = File::open(&audio_path)?;
        let mut reader = AudioStreamReader::new(BufReader::new(file))?;
        let mut pts = 0_u64;
        while let Some(frame) = reader.read_next_frame()? {
            let sample_count = frame.sample_count_per_channel()?;
            let payload = libsrs_audio::encode_frame(&frame)?;
            mux.write_packet(2, pts, pts, true, &payload)?;
            pts = pts.saturating_add(u64::from(sample_count));
        }
    }

    let _ = mux.finalize()?;
    Ok(())
}

fn demux_container_to_elementary(input: &Path, output_stem: &Path) -> Result<()> {
    let in_file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut demux = DemuxReader::open(BufReader::new(in_file))?;
    let tracks = demux.tracks().to_vec();

    let mut video_writer: Option<(VideoStreamWriter<File>, u32, u32)> = None;
    let mut audio_writer: Option<(AudioStreamWriter<File>, u32)> = None;

    for track in &tracks {
        match track.kind {
            TrackKind::Video => {
                if track.config.len() < 8 {
                    return Err(anyhow!("video track config is too short"));
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
                let path = output_stem.with_extension("srsv");
                video_writer = Some((
                    VideoStreamWriter::new(File::create(path)?, width, height)?,
                    width,
                    height,
                ));
            }
            TrackKind::Audio => {
                if track.config.len() < 5 {
                    return Err(anyhow!("audio track config is too short"));
                }
                let sample_rate = u32::from_le_bytes([
                    track.config[0],
                    track.config[1],
                    track.config[2],
                    track.config[3],
                ]);
                let channels = track.config[4];
                let path = output_stem.with_extension("srsa");
                audio_writer = Some((
                    AudioStreamWriter::new(File::create(path)?, sample_rate, channels)?,
                    sample_rate,
                ));
            }
            TrackKind::Data | TrackKind::Subtitle | TrackKind::Metadata | TrackKind::Attachment => {
            }
        }
    }

    demux.reset_to_data_start()?;
    while let Some(pkt) = demux.next_packet()? {
        if pkt.packet.header.track_id == 1 {
            if let Some((writer, width, height)) = video_writer.as_mut() {
                let frame = libsrs_video::decode_frame(
                    *width,
                    *height,
                    pkt.packet.header.sequence as u32,
                    FrameType::I,
                    &pkt.packet.payload,
                )?;
                let _ = writer.write_frame(&frame)?;
            }
        } else if pkt.packet.header.track_id == 2 {
            if let Some((writer, sample_rate)) = audio_writer.as_mut() {
                let frame = decode_frame_with_stream_version(
                    *sample_rate,
                    pkt.packet.header.sequence as u32,
                    &pkt.packet.payload,
                    STREAM_VERSION_V2,
                )?;
                let _ = writer.write_frame(&frame)?;
            }
        }
    }
    Ok(())
}

fn run_native_import(pipeline: &TranscodePipeline, input: &Path, output: &Path) -> Result<usize> {
    let probe = pipeline.analyze_source(input)?;
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
    let (tracks, stream_to_mux) = build_import_mux_tracks(&probe, &packets)?;
    let mut native = NativeImportTranscoder::new(output, tracks, stream_to_mux)?;
    for p in packets {
        native.transcode_packet(p.packet)?;
    }
    native.finalize()?;
    Ok(n)
}

fn raw_video_canvas_side(len: usize) -> Result<u32> {
    if len == 0 {
        return Err(anyhow!("empty raw video payload"));
    }
    let side = (len as f64).sqrt().ceil() as u32;
    if side == 0 {
        return Err(anyhow!("invalid raw video payload length"));
    }
    Ok(side)
}

fn video_frame_from_rgb_bytes(
    data: &[u8],
    side: u32,
    frame_index: u32,
) -> Result<VideoFrame> {
    let w = side as usize;
    let expected = w * w;
    if data.len() > expected {
        return Err(anyhow!(
            "video frame data {} bytes exceeds {}x{} canvas",
            data.len(),
            side,
            side
        ));
    }
    let mut pixels = vec![0u8; expected];
    pixels[..data.len()].copy_from_slice(data);
    Ok(VideoFrame {
        width: side,
        height: side,
        frame_index,
        frame_type: FrameType::I,
        data: pixels,
    })
}

fn audio_frame_from_pcm16le(
    data: &[u8],
    sample_rate: u32,
    channels: u8,
    frame_index: u32,
) -> Result<AudioFrame> {
    let ch = channels as usize;
    if ch == 0 || data.len() % (2 * ch) != 0 {
        return Err(anyhow!(
            "PCM payload length {} is not multiple of {} byte frames",
            data.len(),
            2 * ch
        ));
    }
    let samples = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(AudioFrame {
        sample_rate,
        channels,
        frame_index,
        samples,
    })
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
) -> Result<(Vec<TrackDescriptor>, HashMap<u32, u16>)> {
    // When probe does not report audio layout (e.g. some synthetic paths), assume 48 kHz mono.
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
        let first_v = packets
            .iter()
            .find(|p| p.packet.stream_id.0 == v.id.0)
            .ok_or_else(|| anyhow!("no video packets for import"))?;
        let side = raw_video_canvas_side(first_v.packet.data.len())?;
        let mut config = Vec::new();
        config.extend_from_slice(&side.to_le_bytes());
        config.extend_from_slice(&side.to_le_bytes());
        descriptors.push(TrackDescriptor {
            track_id: next_mux_id,
            kind: TrackKind::Video,
            codec_id: 1,
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

fn infer_square(len: usize) -> Result<u32> {
    let side = (len as f64).sqrt() as u32;
    if side == 0 || (side as usize) * (side as usize) != len {
        return Err(anyhow!(
            "raw video input must be a perfect square of 8-bit pixels"
        ));
    }
    Ok(side)
}

fn extension(path: &Path) -> Option<&str> {
    path.extension().and_then(|ext| ext.to_str())
}

struct NativeImportTranscoder {
    mux: Option<MuxWriter<File>>,
    stream_to_mux: HashMap<u32, u16>,
    video_mux_id: Option<u16>,
    audio_mux_id: Option<u16>,
    video_side: Option<u32>,
    video_timescale: u32,
    audio_timescale: u32,
    audio_sample_rate: u32,
    audio_channels: u8,
    video_frame_index: u32,
    audio_frame_index: u32,
    next_video_pts: u64,
    next_audio_pts: u64,
}

impl NativeImportTranscoder {
    fn new(
        output: &Path,
        tracks: Vec<TrackDescriptor>,
        stream_to_mux: HashMap<u32, u16>,
    ) -> Result<Self> {
        let mut video_mux_id = None;
        let mut audio_mux_id = None;
        let mut video_side = None;
        let mut video_timescale = 90_000u32;
        let mut audio_timescale = 48_000u32;
        let mut audio_sample_rate = 48_000u32;
        let mut audio_channels = 1u8;

        for t in &tracks {
            match t.kind {
                TrackKind::Video => {
                    if t.config.len() >= 8 {
                        let w = u32::from_le_bytes([
                            t.config[0],
                            t.config[1],
                            t.config[2],
                            t.config[3],
                        ]);
                        let h = u32::from_le_bytes([
                            t.config[4],
                            t.config[5],
                            t.config[6],
                            t.config[7],
                        ]);
                        if w != h {
                            return Err(anyhow!("import expects square native video frames"));
                        }
                        video_side = Some(w);
                    }
                    video_mux_id = Some(t.track_id);
                    video_timescale = t.timescale;
                }
                TrackKind::Audio => {
                    if t.config.len() >= 5 {
                        audio_sample_rate = u32::from_le_bytes([
                            t.config[0],
                            t.config[1],
                            t.config[2],
                            t.config[3],
                        ]);
                        audio_channels = t.config[4];
                    }
                    audio_mux_id = Some(t.track_id);
                    audio_timescale = t.timescale;
                }
                TrackKind::Data | TrackKind::Subtitle | TrackKind::Metadata | TrackKind::Attachment => {}
            }
        }

        let mux = MuxWriter::new(
            File::create(output).with_context(|| format!("create {}", output.display()))?,
            FileHeader::new(u16::try_from(tracks.len())?, 8),
            tracks,
        )?;

        Ok(Self {
            mux: Some(mux),
            stream_to_mux,
            video_mux_id,
            audio_mux_id,
            video_side,
            video_timescale,
            audio_timescale,
            audio_sample_rate,
            audio_channels,
            video_frame_index: 0,
            audio_frame_index: 0,
            next_video_pts: 0,
            next_audio_pts: 0,
        })
    }
}

impl NativeTranscoder for NativeImportTranscoder {
    fn transcode_packet(&mut self, packet: Packet) -> Result<()> {
        let mux_id = *self
            .stream_to_mux
            .get(&packet.stream_id.0)
            .ok_or_else(|| anyhow!("unknown stream id {}", packet.stream_id.0))?;

        let mux = self
            .mux
            .as_mut()
            .ok_or_else(|| anyhow!("mux already finalized"))?;

        if Some(mux_id) == self.video_mux_id {
            let side = self
                .video_side
                .ok_or_else(|| anyhow!("video track not configured"))?;
            let frame = video_frame_from_rgb_bytes(&packet.data, side, self.video_frame_index)?;
            self.video_frame_index += 1;
            let payload = libsrs_video::encode_frame(&frame)?;
            let pts = packet
                .pts
                .map(|t| timestamp_to_mux_ticks(t, self.video_timescale))
                .unwrap_or(self.next_video_pts);
            mux.write_packet(mux_id, pts, pts, true, &payload)?;
            self.next_video_pts = pts.saturating_add(3_000);
            return Ok(());
        }

        if Some(mux_id) == self.audio_mux_id {
            let frame = audio_frame_from_pcm16le(
                &packet.data,
                self.audio_sample_rate,
                self.audio_channels,
                self.audio_frame_index,
            )?;
            self.audio_frame_index += 1;
            let sample_count = u64::from(frame.sample_count_per_channel()?);
            let payload = libsrs_audio::encode_frame(&frame)?;
            let pts = packet
                .pts
                .map(|t| timestamp_to_mux_ticks(t, self.audio_timescale))
                .unwrap_or(self.next_audio_pts);
            mux.write_packet(mux_id, pts, pts, true, &payload)?;
            self.next_audio_pts = pts.saturating_add(sample_count);
            return Ok(());
        }

        Err(anyhow!("unhandled mux track {mux_id}"))
    }

    fn finalize(&mut self) -> Result<()> {
        if let Some(mux) = self.mux.take() {
            let _ = mux.finalize()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsrs_licensing_proto::{EntitlementStatus, LicensedFeature};

    fn basic_entitlement() -> EntitlementClaims {
        EntitlementClaims {
            license_id: "license-1".to_string(),
            key_id: "key-1".to_string(),
            features: LicensedFeature::basic_defaults(),
            status: EntitlementStatus::Active,
            issued_at_epoch_s: 1,
            expires_at_epoch_s: 2,
            device_install_id: "install-1".to_string(),
            message: "basic".to_string(),
            replacement_key: None,
        }
    }

    #[test]
    fn basic_entitlement_blocks_editor_actions() {
        let err = require_editor_feature(&basic_entitlement(), LicensedFeature::Encode, "encode")
            .expect_err("basic entitlement should not unlock encode");
        assert_eq!(err.to_string(), "editor entitlement required for encode");
    }
}
