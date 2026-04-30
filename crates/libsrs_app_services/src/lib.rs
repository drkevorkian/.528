use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use libsrs_audio::{
    decode_frame_with_stream_version, AudioFrame, AudioStreamReader, AudioStreamWriter,
    STREAM_VERSION_V2,
};
use libsrs_container::{FileHeader, TrackDescriptor, TrackKind};
use libsrs_contract::CodecType;
use libsrs_demux::DemuxReader;
use libsrs_licensing_proto::{EntitlementClaims, LicensedFeature};
use libsrs_mux::MuxWriter;
use libsrs_pipeline::TranscodePipeline;
use libsrs_video::{
    decode_sequence_header_v2, decode_yuv420_intra_payload, encode_sequence_header_v2,
    encode_yuv420_intra_payload, FrameType, VideoFrame, VideoSequenceHeaderV2, VideoStreamReader,
    VideoStreamReaderV2, VideoStreamWriter, VideoStreamWriterV2, SEQUENCE_HEADER_BYTES,
};
use thiserror::Error;

mod import_pipeline;
pub mod playback;

pub use playback::{
    DecodedAudioChunk, DecodedVideoFrame, PlaybackClock, PlaybackCommand, PlaybackError,
    PlaybackEvent, PlaybackPosition, PlaybackSession, PlaybackState, PlaybackTrackInfo,
    MAX_STASH_PACKETS, MAX_VIDEO_PIXELS, MAX_VIDEO_SIDE,
};

/// Selects how **new** `.528` / import mux video tracks are written.
///
/// SRSV2 (`codec_id == 3`) is the default modern path; SRSV1 remains for legacy compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Native528VideoCodec {
    /// SRSV2 intra YUV420p8 (`codec_id` **3**, 64-byte sequence header in track config).
    #[default]
    Srsv2,
    /// Legacy grayscale intra (`codec_id` **1**, 8-byte width/height in track config).
    Srsv1Legacy,
}

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
            Some("srsv2") => inspect_native_video_v2(input),
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
        self.encode_input_to_native_with_video_codec(
            input,
            output,
            entitlement,
            Native528VideoCodec::default(),
        )
    }

    pub fn encode_input_to_native_with_video_codec<P: AsRef<Path>>(
        &self,
        input: P,
        output: P,
        entitlement: &EntitlementClaims,
        video_codec: Native528VideoCodec,
    ) -> Result<()> {
        require_editor_feature(entitlement, LicensedFeature::Encode, "encode")?;
        encode_input_to_native(input.as_ref(), output.as_ref(), video_codec)
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
        self.import_to_native_with_video_codec(
            input,
            output,
            entitlement,
            Native528VideoCodec::default(),
        )
    }

    pub fn import_to_native_with_video_codec<P: AsRef<Path>>(
        &self,
        input: P,
        output: P,
        entitlement: &EntitlementClaims,
        video_codec: Native528VideoCodec,
    ) -> Result<usize> {
        require_editor_feature(entitlement, LicensedFeature::Import, "import")?;
        self.ensure_supported_for_conversion(input.as_ref())?;
        run_native_import(&self.pipeline, input.as_ref(), output.as_ref(), video_codec)
    }

    pub fn transcode_to_native<P: AsRef<Path>>(
        &self,
        input: P,
        output: P,
        entitlement: &EntitlementClaims,
    ) -> Result<usize> {
        self.transcode_to_native_with_video_codec(
            input,
            output,
            entitlement,
            Native528VideoCodec::default(),
        )
    }

    pub fn transcode_to_native_with_video_codec<P: AsRef<Path>>(
        &self,
        input: P,
        output: P,
        entitlement: &EntitlementClaims,
        video_codec: Native528VideoCodec,
    ) -> Result<usize> {
        require_editor_feature(entitlement, LicensedFeature::Transcode, "transcode")?;
        self.ensure_supported_for_conversion(input.as_ref())?;
        run_native_import(&self.pipeline, input.as_ref(), output.as_ref(), video_codec)
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

fn inspect_native_video_v2(input: &Path) -> Result<MediaInspection> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut reader = VideoStreamReaderV2::new(BufReader::new(file))?;
    let w = reader.seq.width;
    let h = reader.seq.height;
    let mut frame_count = 0_u64;
    while reader.read_next_payload()?.is_some() {
        frame_count += 1;
    }
    Ok(MediaInspection {
        format_name: "srsv2".to_string(),
        duration_ms: Some(frame_count.saturating_mul(40)),
        summary: format!("native SRSV2 elementary: {}x{}, frames={frame_count}", w, h),
        tracks: vec![TrackSummary {
            id: 0,
            kind: "Video".to_string(),
            codec: CodecType::NativeSrsVideoV2.display_name().to_string(),
            role: "Primary".to_string(),
            detail: format!("{}x{} YUV420p8 intra", w, h),
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
        .map(|track| {
            let detail = container_track_detail(track);
            TrackSummary {
                id: u32::from(track.track_id),
                kind: format!("{:?}", track.kind),
                codec: container_codec_name(track.codec_id).to_string(),
                role: if track.track_id == 1 {
                    "Primary".to_string()
                } else {
                    "Alternate".to_string()
                },
                detail,
                supported_without_license: container_codec(track.codec_id)
                    .is_royalty_free_playback_allowed(),
            }
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
        3 => CodecType::NativeSrsVideoV2,
        _ => CodecType::Unknown,
    }
}

fn container_track_detail(track: &TrackDescriptor) -> String {
    match track.kind {
        TrackKind::Video if track.codec_id == 3 && track.config.len() >= SEQUENCE_HEADER_BYTES => {
            match decode_sequence_header_v2(&track.config[..SEQUENCE_HEADER_BYTES]) {
                Ok(seq) => format!(
                    "SRSV2 {}x{} timescale={}",
                    seq.width, seq.height, track.timescale
                ),
                Err(_) => format!(
                    "SRSV2 invalid sequence config ({} bytes), timescale={}",
                    track.config.len(),
                    track.timescale
                ),
            }
        }
        TrackKind::Video if track.codec_id == 1 && track.config.len() >= 8 => {
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
            format!("SRSV1 {}x{} timescale={}", width, height, track.timescale)
        }
        _ => format!(
            "timescale={} config={} bytes",
            track.timescale,
            track.config.len()
        ),
    }
}

fn container_codec_name(codec_id: u16) -> &'static str {
    container_codec(codec_id).display_name()
}

fn encode_square_gray_raw_to_srsv2_elementary(input: &Path, srsv2_out: &Path) -> Result<()> {
    let bytes = std::fs::read(input).with_context(|| format!("read {}", input.display()))?;
    let side = infer_square(bytes.len())?;
    let seq = VideoSequenceHeaderV2::intra_main_yuv420_bt709_limited(side, side);
    let yuv = libsrs_video::gray8_packed_to_yuv420p8_neutral(&bytes, side, side)
        .map_err(|e| anyhow!("{}", e))?;
    let payload = encode_yuv420_intra_payload(&seq, &yuv, 0, 28).map_err(|e| anyhow!("{}", e))?;
    let f = File::create(srsv2_out).with_context(|| format!("create {}", srsv2_out.display()))?;
    let mut w = VideoStreamWriterV2::new(f, &seq).map_err(|e| anyhow!("SRSV2 writer: {}", e))?;
    w.write_frame_payload(0, &payload)
        .map_err(|e| anyhow!("SRSV2 frame: {}", e))?;
    Ok(())
}

fn encode_input_to_native(
    input: &Path,
    output: &Path,
    video_codec: Native528VideoCodec,
) -> Result<()> {
    match extension(output) {
        Some("srsv") => encode_raw_video(input, output),
        Some("srsa") => encode_raw_audio(input, output),
        Some("srsv2") => match video_codec {
            Native528VideoCodec::Srsv2 => encode_square_gray_raw_to_srsv2_elementary(input, output),
            Native528VideoCodec::Srsv1Legacy => Err(anyhow!(
                ".srsv2 elementary output requires SRSV2 codec policy (omit --codec srsv1)"
            )),
        },
        Some("528") | Some("srsm") => {
            let stem = output.with_extension("");
            let video_srsv2 = stem.with_extension("srsv2");
            let video_srsv = stem.with_extension("srsv");
            if video_srsv2.exists() && video_srsv.exists() {
                return Err(anyhow!(
                    "ambiguous elementary video: remove {} or {}",
                    video_srsv2.display(),
                    video_srsv.display()
                ));
            }
            match video_codec {
                Native528VideoCodec::Srsv2 => {
                    if video_srsv2.exists() || video_srsv.exists() {
                        mux_elementary_streams(&stem, output)
                    } else {
                        encode_square_gray_raw_to_srsv2_elementary(input, &video_srsv2)?;
                        mux_elementary_streams(&stem, output)
                    }
                }
                Native528VideoCodec::Srsv1Legacy => {
                    if video_srsv2.exists() {
                        return Err(anyhow!(
                            "found {} but legacy SRSV1 mux was requested; remove it or use SRSV2 policy",
                            video_srsv2.display()
                        ));
                    }
                    encode_raw_video(input, &video_srsv)?;
                    mux_elementary_streams(&stem, output)
                }
            }
        }
        _ => Err(anyhow!(
            "unsupported output extension; expected .528 (primary), legacy .srsm, .srsv, .srsv2, or .srsa"
        )),
    }
}

fn decode_native_to_raw(input: &Path, output: &Path) -> Result<()> {
    match extension(input) {
        Some("srsv") => decode_video_to_raw(input, output),
        Some("srsv2") => decode_srsv2_video_to_raw(input, output),
        Some("srsa") => decode_audio_to_pcm(input, output),
        Some("528") | Some("srsm") => demux_container_to_elementary(input, output),
        _ => Err(anyhow!(
            "unsupported input extension; expected .528 (primary), legacy .srsm, .srsv, .srsv2, or .srsa"
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

fn append_plane_tight(out: &mut Vec<u8>, plane: &libsrs_video::VideoPlane<u8>) {
    for row in 0..plane.height as usize {
        out.extend_from_slice(plane.row(row));
    }
}

/// Concatenates Y then U then V planes per frame (YUV420p8), tight rows.
fn decode_srsv2_video_to_raw(input: &Path, output: &Path) -> Result<()> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut reader = VideoStreamReaderV2::new(BufReader::new(file))?;
    let seq = reader.seq.clone();
    let mut out = Vec::new();
    while let Some((_idx, payload)) = reader
        .read_next_payload()
        .map_err(|e| anyhow!("SRSV2 elementary read: {}", e))?
    {
        let dec = decode_yuv420_intra_payload(&seq, &payload)
            .map_err(|e| anyhow!("SRSV2 decode: {}", e))?;
        append_plane_tight(&mut out, &dec.yuv.y);
        append_plane_tight(&mut out, &dec.yuv.u);
        append_plane_tight(&mut out, &dec.yuv.v);
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
    let video_srsv2 = if extension(input) == Some("srsv2") {
        input.to_path_buf()
    } else {
        input.with_extension("srsv2")
    };
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
    if video_srsv2.exists() && video_path.exists() {
        return Err(anyhow!(
            "ambiguous elementary video: remove {} or {}",
            video_srsv2.display(),
            video_path.display()
        ));
    }
    if !video_srsv2.exists() && !video_path.exists() && !audio_path.exists() {
        return Err(anyhow!(
            "no input streams found; expected {}, {}, and/or {}",
            video_srsv2.display(),
            video_path.display(),
            audio_path.display()
        ));
    }

    let mut tracks = Vec::new();
    if video_srsv2.exists() {
        let file = File::open(&video_srsv2)?;
        let reader = VideoStreamReaderV2::new(BufReader::new(file))
            .map_err(|e| anyhow!("SRSV2 elementary: {}", e))?;
        let cfg = encode_sequence_header_v2(&reader.seq);
        tracks.push(TrackDescriptor {
            track_id: 1,
            kind: TrackKind::Video,
            codec_id: 3,
            flags: 0,
            timescale: 90_000,
            config: cfg.to_vec(),
        });
    } else if video_path.exists() {
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

    if video_srsv2.exists() {
        let file = File::open(&video_srsv2)?;
        let mut reader = VideoStreamReaderV2::new(BufReader::new(file))
            .map_err(|e| anyhow!("SRSV2 elementary: {}", e))?;
        let mut pts = 0_u64;
        while let Some((frame_index, payload)) = reader
            .read_next_payload()
            .map_err(|e| anyhow!("SRSV2 read frame: {}", e))?
        {
            mux.write_packet(1, pts, pts, true, &payload)?;
            let _ = frame_index;
            pts = pts.saturating_add(3_000);
        }
    } else if video_path.exists() {
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

enum DemuxVideoSink {
    V1 {
        writer: VideoStreamWriter<File>,
        width: u32,
        height: u32,
    },
    V2 {
        writer: VideoStreamWriterV2<File>,
    },
}

fn demux_container_to_elementary(input: &Path, output_stem: &Path) -> Result<()> {
    let in_file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut demux = DemuxReader::open(BufReader::new(in_file))?;
    let tracks = demux.tracks().to_vec();

    let mut video_writer: Option<DemuxVideoSink> = None;
    let mut audio_writer: Option<(AudioStreamWriter<File>, u32)> = None;

    for track in &tracks {
        match track.kind {
            TrackKind::Video => match track.codec_id {
                1 => {
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
                    video_writer = Some(DemuxVideoSink::V1 {
                        writer: VideoStreamWriter::new(File::create(path)?, width, height)?,
                        width,
                        height,
                    });
                }
                3 => {
                    if track.config.len() < SEQUENCE_HEADER_BYTES {
                        return Err(anyhow!("SRSV2 video track config is too short"));
                    }
                    let seq = decode_sequence_header_v2(&track.config[..SEQUENCE_HEADER_BYTES])
                        .map_err(|e| anyhow!("SRSV2 sequence header: {}", e))?;
                    let path = output_stem.with_extension("srsv2");
                    video_writer = Some(DemuxVideoSink::V2 {
                        writer: VideoStreamWriterV2::new(File::create(path)?, &seq)
                            .map_err(|e| anyhow!("SRSV2 writer: {}", e))?,
                    });
                }
                other => {
                    return Err(anyhow!("unsupported video codec_id {} for demux", other));
                }
            },
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
            match video_writer.as_mut() {
                Some(DemuxVideoSink::V1 {
                    writer,
                    width,
                    height,
                }) => {
                    let frame = libsrs_video::decode_frame(
                        *width,
                        *height,
                        pkt.packet.header.sequence as u32,
                        FrameType::I,
                        &pkt.packet.payload,
                    )?;
                    let _ = writer.write_frame(&frame)?;
                }
                Some(DemuxVideoSink::V2 { writer }) => {
                    writer
                        .write_frame_payload(pkt.packet.header.sequence as u32, &pkt.packet.payload)
                        .map_err(|e| anyhow!("SRSV2 write frame: {}", e))?;
                }
                None => {}
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

fn run_native_import(
    pipeline: &TranscodePipeline,
    input: &Path,
    output: &Path,
    video_codec: Native528VideoCodec,
) -> Result<usize> {
    import_pipeline::run_native_import(pipeline, input, output, video_codec)
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

    #[test]
    fn mux_rejects_both_srsv_and_srsv2_elementary() {
        let dir = std::env::temp_dir();
        let stem = dir.join(format!(
            "mux-dup-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::File::create(stem.with_extension("srsv")).unwrap();
        std::fs::File::create(stem.with_extension("srsv2")).unwrap();
        let err = mux_elementary_streams(&stem, &stem.with_extension("528")).unwrap_err();
        assert!(
            err.to_string().contains("ambiguous"),
            "expected ambiguity error, got {err}"
        );
        let _ = std::fs::remove_file(stem.with_extension("srsv"));
        let _ = std::fs::remove_file(stem.with_extension("srsv2"));
    }
}
