//! SRSV2 logical model — independent of ITU-T/ISO codec bitstreams.
//!
//! Serialization uses explicit little-endian packed structs where noted.

/// Distinct from container `codec_id` — identifies elementary SRSV2 stream content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SrsVideoCodecId {
    /// Legacy grayscale intra prototype (`libsrs_video::codec` v1).
    Srsv1 = 1,
    /// Modern SRSV2 intra/inter architecture (this module).
    Srsv2 = 2,
}

impl SrsVideoCodecId {
    pub fn from_u8(v: u8) -> Result<Self, super::error::SrsV2Error> {
        match v {
            1 => Ok(Self::Srsv1),
            2 => Ok(Self::Srsv2),
            _ => Err(super::error::SrsV2Error::Unsupported(
                "unknown SrsVideoCodecId",
            )),
        }
    }
}

/// Production **`codec_id` 3** tier signaled in the 64-byte SRSV2 sequence header (byte offset 16).
///
/// Roadmap roles (resolution targets, tooling) live in `docs/srsv2_design_targets.md`. Today most paths still emit **`Main`**; higher tiers unlock features over time. **Unknown** profile byte values are **rejected** at decode until added to this enum and the `decode_sequence_header_v2` match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SrsVideoProfile {
    /// Fast decode, mobile / 1080p–1440p class workloads.
    Baseline = 0,
    /// Normal playback / production — **4K / 8K** tier focus for balanced presets.
    Main = 1,
    /// Creator / editing / archival — **4:2:2** and **4:4:4** readiness (tooling TBD).
    Pro = 2,
    /// Near-lossless / archival emphasis (distinct from **Pro** tooling tier).
    Lossless = 3,
    /// Screen content: UI, games, text, AI-generated frames (future screen tools).
    Screen = 4,
    /// **8K** high-quality compression emphasis — slower encode acceptable (`Ultra` preset roadmap).
    Ultra = 5,
    /// Above-8K and experimental features — permissive limits, not general interchange default.
    Research = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PixelFormat {
    Gray8 = 0,
    Yuv420p8 = 1,
    Yuv420p10 = 2,
    Yuv422p8 = 3,
    Yuv422p10 = 4,
    Yuv444p8 = 5,
    Yuv444p10 = 6,
    Rgb8 = 7,
    Rgba8 = 8,
    Bgra8 = 9,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ColorPrimaries {
    Bt601 = 0,
    Bt709 = 1,
    Bt2020 = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransferFunction {
    Sdr = 0,
    Pq = 1,
    Hlg = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MatrixCoefficients {
    Bt601 = 0,
    Bt709 = 1,
    Bt2020Ncl = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChromaSiting {
    Center = 0,
    Left = 1,
    TopLeft = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ColorRange {
    Limited = 0,
    Full = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameTypeV2 {
    Intra = 0,
    Predicted = 1,
}

impl FrameTypeV2 {
    pub const I: Self = Self::Intra;
    pub const P: Self = Self::Predicted;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BlockMode {
    /// Leaf CU — no further partition (baseline uses 8×8 leaves).
    Leaf = 0,
    SplitQuad = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IntraMode {
    Dc = 0,
    Planar = 1,
    Horizontal = 2,
    Vertical = 3,
    Diagonal = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InterMode {
    /// Reserved for future P/B tooling (never encoded in intra-only builds).
    Skip = 0,
    MvDelta = 1,
    MergeNearest = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransformType {
    Dct4 = 0,
    Dct8 = 1,
    Dct16 = 2,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QuantizerState {
    pub base_qp: u8,
    pub delta_qp: i8,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MotionVector {
    pub x: i16,
    pub y: i16,
}

#[derive(Debug, Clone, Default)]
pub struct ReferenceFrameSet {
    /// Future inter: ring buffer slots (not populated in intra-only path).
    pub max_slots: u8,
}

#[derive(Debug, Clone)]
pub struct VideoSequenceHeaderV2 {
    pub width: u32,
    pub height: u32,
    pub profile: SrsVideoProfile,
    pub pixel_format: PixelFormat,
    pub color_primaries: ColorPrimaries,
    pub transfer: TransferFunction,
    pub matrix: MatrixCoefficients,
    pub chroma_siting: ChromaSiting,
    pub range: ColorRange,
    /// When true, deblock filter stage is skipped (baseline intra default).
    pub disable_loop_filter: bool,
    pub max_ref_frames: u8,
}

impl VideoSequenceHeaderV2 {
    /// Default SRSV2 intra sequence used by import/mux when embedding `codec_id == 3` tracks.
    pub fn intra_main_yuv420_bt709_limited(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            profile: SrsVideoProfile::Main,
            pixel_format: PixelFormat::Yuv420p8,
            color_primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Sdr,
            matrix: MatrixCoefficients::Bt709,
            chroma_siting: ChromaSiting::Center,
            range: ColorRange::Limited,
            disable_loop_filter: true,
            max_ref_frames: 0,
        }
    }

    /// Like [`Self::intra_main_yuv420_bt709_limited`] but advertises one reference picture so mux/import may emit **P** frames (`FR2` rev 2) when dimensions are multiples of 16.
    pub fn intra_main_yuv420_bt709_limited_one_ref(width: u32, height: u32) -> Self {
        let mut s = Self::intra_main_yuv420_bt709_limited(width, height);
        s.max_ref_frames = 1;
        s
    }
}

#[derive(Debug, Clone)]
pub struct FrameHeaderV2 {
    pub frame_index: u32,
    pub frame_type: FrameTypeV2,
    pub quantizer: QuantizerState,
}

#[derive(Debug, Clone)]
pub struct TileHeaderV2 {
    pub tile_col: u16,
    pub tile_row: u16,
}

#[derive(Debug, Clone)]
pub struct SuperblockHeaderV2 {
    pub sb_x: u16,
    pub sb_y: u16,
}

/// Packed 64-byte on-wire sequence header for elementary `.srsv2` files.
pub const SEQUENCE_HEADER_BYTES: usize = 64;
pub const SEQUENCE_MAGIC: [u8; 4] = *b"SRS2";

pub fn encode_sequence_header_v2(seq: &VideoSequenceHeaderV2) -> [u8; SEQUENCE_HEADER_BYTES] {
    let mut b = [0_u8; SEQUENCE_HEADER_BYTES];
    b[0..4].copy_from_slice(&SEQUENCE_MAGIC);
    b[4] = 1; // header schema version
    b[8..12].copy_from_slice(&seq.width.to_le_bytes());
    b[12..16].copy_from_slice(&seq.height.to_le_bytes());
    b[16] = seq.profile as u8;
    b[17] = seq.pixel_format as u8;
    b[18] = seq.color_primaries as u8;
    b[19] = seq.transfer as u8;
    b[20] = seq.matrix as u8;
    b[21] = seq.chroma_siting as u8;
    b[22] = seq.range as u8;
    b[23] = u8::from(seq.disable_loop_filter);
    b[24] = seq.max_ref_frames;
    b
}

pub fn decode_sequence_header_v2(
    buf: &[u8],
) -> Result<VideoSequenceHeaderV2, super::error::SrsV2Error> {
    if buf.len() < SEQUENCE_HEADER_BYTES {
        return Err(super::error::SrsV2Error::Truncated);
    }
    if buf[0..4] != SEQUENCE_MAGIC {
        return Err(super::error::SrsV2Error::BadMagic);
    }
    if buf[4] != 1 {
        return Err(super::error::SrsV2Error::UnsupportedVersion(buf[4]));
    }
    let width = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let height = u32::from_le_bytes(buf[12..16].try_into().unwrap());
    if buf[24] > super::limits::MAX_REF_FRAMES {
        return Err(super::error::SrsV2Error::ExcessiveReferenceFrames(buf[24]));
    }

    Ok(VideoSequenceHeaderV2 {
        width,
        height,
        profile: match buf[16] {
            0 => SrsVideoProfile::Baseline,
            1 => SrsVideoProfile::Main,
            2 => SrsVideoProfile::Pro,
            3 => SrsVideoProfile::Lossless,
            4 => SrsVideoProfile::Screen,
            5 => SrsVideoProfile::Ultra,
            6 => SrsVideoProfile::Research,
            _ => {
                return Err(super::error::SrsV2Error::syntax(
                    "unknown SRSV2 profile byte",
                ));
            }
        },
        pixel_format: decode_pixel_format(buf[17])?,
        color_primaries: decode_primaries(buf[18])?,
        transfer: decode_transfer(buf[19])?,
        matrix: decode_matrix(buf[20])?,
        chroma_siting: decode_chroma_siting(buf[21])?,
        range: decode_range(buf[22])?,
        disable_loop_filter: buf[23] != 0,
        max_ref_frames: buf[24],
    })
}

fn decode_pixel_format(b: u8) -> Result<PixelFormat, super::error::SrsV2Error> {
    match b {
        0 => Ok(PixelFormat::Gray8),
        1 => Ok(PixelFormat::Yuv420p8),
        2 => Ok(PixelFormat::Yuv420p10),
        3 => Ok(PixelFormat::Yuv422p8),
        4 => Ok(PixelFormat::Yuv422p10),
        5 => Ok(PixelFormat::Yuv444p8),
        6 => Ok(PixelFormat::Yuv444p10),
        7 => Ok(PixelFormat::Rgb8),
        8 => Ok(PixelFormat::Rgba8),
        9 => Ok(PixelFormat::Bgra8),
        _ => Err(super::error::SrsV2Error::syntax("unknown pixel format")),
    }
}

fn decode_primaries(b: u8) -> Result<ColorPrimaries, super::error::SrsV2Error> {
    match b {
        0 => Ok(ColorPrimaries::Bt601),
        1 => Ok(ColorPrimaries::Bt709),
        2 => Ok(ColorPrimaries::Bt2020),
        _ => Err(super::error::SrsV2Error::syntax("unknown primaries")),
    }
}

fn decode_transfer(b: u8) -> Result<TransferFunction, super::error::SrsV2Error> {
    match b {
        0 => Ok(TransferFunction::Sdr),
        1 => Ok(TransferFunction::Pq),
        2 => Ok(TransferFunction::Hlg),
        _ => Err(super::error::SrsV2Error::syntax("unknown transfer")),
    }
}

fn decode_matrix(b: u8) -> Result<MatrixCoefficients, super::error::SrsV2Error> {
    match b {
        0 => Ok(MatrixCoefficients::Bt601),
        1 => Ok(MatrixCoefficients::Bt709),
        2 => Ok(MatrixCoefficients::Bt2020Ncl),
        _ => Err(super::error::SrsV2Error::syntax("unknown matrix")),
    }
}

fn decode_chroma_siting(b: u8) -> Result<ChromaSiting, super::error::SrsV2Error> {
    match b {
        0 => Ok(ChromaSiting::Center),
        1 => Ok(ChromaSiting::Left),
        2 => Ok(ChromaSiting::TopLeft),
        _ => Err(super::error::SrsV2Error::syntax("unknown chroma siting")),
    }
}

fn decode_range(b: u8) -> Result<ColorRange, super::error::SrsV2Error> {
    match b {
        0 => Ok(ColorRange::Limited),
        1 => Ok(ColorRange::Full),
        _ => Err(super::error::SrsV2Error::syntax("unknown color range")),
    }
}

#[cfg(test)]
mod profile_roundtrip_tests {
    use super::*;

    #[test]
    fn sequence_header_profiles_ultra_research_roundtrip() {
        for p in [SrsVideoProfile::Ultra, SrsVideoProfile::Research] {
            let seq = VideoSequenceHeaderV2 {
                width: 7680,
                height: 4320,
                profile: p,
                pixel_format: PixelFormat::Yuv420p8,
                color_primaries: ColorPrimaries::Bt709,
                transfer: TransferFunction::Sdr,
                matrix: MatrixCoefficients::Bt709,
                chroma_siting: ChromaSiting::Center,
                range: ColorRange::Limited,
                disable_loop_filter: true,
                max_ref_frames: 4,
            };
            let b = encode_sequence_header_v2(&seq);
            let d = decode_sequence_header_v2(&b).expect("decode");
            assert_eq!(d.profile, p);
            assert_eq!(d.width, 7680);
            assert_eq!(d.height, 4320);
        }
    }
}
