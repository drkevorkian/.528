//! Experimental **residual_token_v2** — structured coefficient tokens + compact wire (**not** **`FR2`**).
//!
//! Block **1** delivers the token model, contexts, and encode/decode APIs without changing the frame
//! encoder or **`residual_entropy`** wiring beyond optional telemetry (**[`encode_ac_payload`]** stays the
//! legacy **VERSION 2** compact band format for benchmarks).

use std::fmt;

use super::dct::{ZIGZAG, ZIGZAG_4X4};
use super::error::SrsV2Error;

// -----------------------------------------------------------------------------
// limits (aligned with legacy rANS residual alphabet where applicable)
// -----------------------------------------------------------------------------

/// Maximum absolute **AC** coefficient (matches [`super::residual_tokens::MAX_RANS_ABS_COEFF`]).
pub const MAX_ABS_AC_COEFF: i16 = 127;

/// Small magnitude ceiling (**exclusive**): \|v\| ≤ this uses [`ResidualTokenV2::SmallCoeffMagnitude`] clusters + [`ResidualTokenV2::SignMask`].
pub const SMALL_COEFF_ABS_MAX: i16 = 8;

/// Hard cap on emitted semantic tokens per transformed block (hostile-input bound).
pub const MAX_TOKENS_PER_BLOCK: usize = 384;

/// Legacy compact (**VERSION 2**) zigzag AC bands — same geometry as earlier **`encode_ac_payload`**.
const LEGACY_BAND_FIRST_ZZ: [usize; 4] = [1, 17, 33, 49];
const LEGACY_BAND_LEN: [usize; 4] = [16, 16, 16, 15];

const MAGIC: u8 = 0x52;
const VERSION_LEGACY_V2: u8 = 2;
const VERSION_STRUCTURED_V3: u8 = 3;
/// Plane multiplex container (**magic** + **version** + plane + length-prefixed **VERSION 2**/**3** chunks).
const VERSION_PLANE_CONTAINER: u8 = 4;

// -----------------------------------------------------------------------------
// public types
// -----------------------------------------------------------------------------

/// Semantic residual token (**VERSION 3** structured stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResidualTokenV2 {
    EndOfBlock,
    /// Zero run **1..=15** AC positions (zigzag order within current AC scan).
    ZeroRunShort(u8),
    /// Zero run **≥16** (or when Short insufficient).
    ZeroRunLong(u32),
    /// \|mag\| ∈ **1..=[`SMALL_COEFF_ABS_MAX`]**, sign carried in a following [`ResidualTokenV2::SignMask`] for the current cluster.
    SmallCoeffMagnitude {
        zigzag_ac_index: u8,
        magnitude: u8,
    },
    /// \|mag\| > [`SMALL_COEFF_ABS_MAX`] — full zigzag-mapped coefficient value (sign embedded).
    LargeCoeffMagnitude {
        zigzag_ac_index: u8,
        value: i16,
    },
    /// Packed signs (**LSB** = first coeff in cluster), **up to 64** bits.
    SignMask(u64),
    /// Quantized **DC** sample (zigzag index **0**).
    DcDelta(i16),
    /// Frequency-band boundary (**AC only**; DC handled separately).
    BandSwitch(CoeffBand),
    /// Optional plane marker when multiplexing multiple planes in one experimental blob.
    PlaneSwitch(ResidualTokenV2Plane),
}

/// Quantized spectrum for one transform instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidualTokenV2Block<'a> {
    Tx8x8(&'a [i16; 64]),
    Tx4x4(&'a [i16; 16]),
}

/// **YUV** plane selector for context + optional [`ResidualTokenV2::PlaneSwitch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResidualTokenV2Plane {
    Y,
    U,
    V,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidualTokenV2TransformKind {
    Tx8x8,
    /// Single **4×4** spectrum (**16** coeffs). A **Four4×4** macroblock is modeled as **four** sequential **`Tx4x4`** blocks at the caller.
    Tx4x4,
}

/// AC frequency band (DC excluded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoeffBand {
    Dc,
    Low,
    Mid,
    High,
}

/// Texture heuristic carried into context (encoder may classify neighbors / activity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockActivityClass {
    Flat,
    Edge,
    Textured,
}

/// Encoder-side context steering (**does not** change lossless reconstruction — affects encoding choices / optional biases).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidualTokenV2Context {
    pub plane: ResidualTokenV2Plane,
    pub band_hint: Option<CoeffBand>,
    pub prev_nonzero_ac_count: u8,
    pub neighbor_residual_present: bool,
    pub activity: BlockActivityClass,
    pub transform: ResidualTokenV2TransformKind,
}

impl Default for ResidualTokenV2Context {
    fn default() -> Self {
        Self {
            plane: ResidualTokenV2Plane::Y,
            band_hint: None,
            prev_nonzero_ac_count: 0,
            neighbor_residual_present: false,
            activity: BlockActivityClass::Flat,
            transform: ResidualTokenV2TransformKind::Tx8x8,
        }
    }
}

/// Wire / experimental codec profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidualTokenV2Mode {
    /// [`VERSION_LEGACY_V2`] band-varint layout (**[`encode_ac_payload`]**).
    LegacyBandCompactV2,
    /// [`VERSION_STRUCTURED_V3`] tagged token stream.
    StructuredTokenV3,
}

/// Accounting returned from [`tokenize_residual_v2_block`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResidualTokenV2Stats {
    pub token_count: usize,
    pub nonzero_ac_count: u32,
    pub zero_run_tokens: u32,
    /// Sum of AC zigzag positions consumed by [`ResidualTokenV2::ZeroRunShort`] / [`ResidualTokenV2::ZeroRunLong`] tokens.
    pub zero_run_ac_positions: u64,
    pub small_mag_tokens: u32,
    pub large_mag_tokens: u32,
    pub sign_mask_tokens: u32,
    /// Count of [`ResidualTokenV2::EndOfBlock`] markers (one per transformed block in typical streams).
    pub eob_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResidualTokenV2Error {
    Truncated,
    TrailingGarbage,
    InvalidSymbol(u8),
    TokenBudgetExceeded { max: usize, got: usize },
    CoefficientOutOfRange { value: i16, max_abs: i16 },
    TooManyCoefficients { max: usize, got: usize },
    Syntax(&'static str),
    TransformMismatch,
    DecodeMismatch,
}

impl fmt::Display for ResidualTokenV2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "truncated residual_token_v2 stream"),
            Self::TrailingGarbage => write!(f, "trailing bytes after residual_token_v2 block"),
            Self::InvalidSymbol(b) => write!(f, "invalid residual_token_v2 symbol tag {b:#04x}"),
            Self::TokenBudgetExceeded { max, got } => {
                write!(f, "token budget exceeded (max={max}, got={got})")
            }
            Self::CoefficientOutOfRange { value, max_abs } => {
                write!(f, "coefficient out of range: {value} (max_abs={max_abs})")
            }
            Self::TooManyCoefficients { max, got } => {
                write!(f, "too many decoded coefficients (max={max}, got={got})")
            }
            Self::Syntax(s) => write!(f, "{s}"),
            Self::TransformMismatch => write!(f, "transform kind does not match coefficient slice"),
            Self::DecodeMismatch => write!(f, "decode reconstruction mismatch"),
        }
    }
}

impl std::error::Error for ResidualTokenV2Error {}

fn map_v2_err(e: ResidualTokenV2Error) -> SrsV2Error {
    match e {
        ResidualTokenV2Error::Truncated => SrsV2Error::Truncated,
        ResidualTokenV2Error::Syntax(s) => SrsV2Error::syntax(s),
        other => SrsV2Error::PartitionMapSyntax(other.to_string()),
    }
}

// -----------------------------------------------------------------------------
// band helpers (8×8 AC zigzag indices 1..63)
// -----------------------------------------------------------------------------

fn coeff_band_8x8_ac(zz_ac: usize) -> CoeffBand {
    debug_assert!((1..64).contains(&zz_ac));
    match zz_ac {
        1..=21 => CoeffBand::Low,
        22..=42 => CoeffBand::Mid,
        _ => CoeffBand::High,
    }
}

fn coeff_band_4x4_ac(zz_ac: usize) -> CoeffBand {
    debug_assert!((1..16).contains(&zz_ac));
    match zz_ac {
        1..=5 => CoeffBand::Low,
        6..=10 => CoeffBand::Mid,
        _ => CoeffBand::High,
    }
}

fn plane_tag(p: ResidualTokenV2Plane) -> u8 {
    match p {
        ResidualTokenV2Plane::Y => 0,
        ResidualTokenV2Plane::U => 1,
        ResidualTokenV2Plane::V => 2,
    }
}

fn plane_from_tag(t: u8) -> Result<ResidualTokenV2Plane, ResidualTokenV2Error> {
    match t {
        0 => Ok(ResidualTokenV2Plane::Y),
        1 => Ok(ResidualTokenV2Plane::U),
        2 => Ok(ResidualTokenV2Plane::V),
        _ => Err(ResidualTokenV2Error::Syntax("bad plane tag")),
    }
}

fn transform_tag(k: ResidualTokenV2TransformKind) -> u8 {
    match k {
        ResidualTokenV2TransformKind::Tx8x8 => 0,
        ResidualTokenV2TransformKind::Tx4x4 => 1,
    }
}

fn transform_from_tag(t: u8) -> Result<ResidualTokenV2TransformKind, ResidualTokenV2Error> {
    match t {
        0 => Ok(ResidualTokenV2TransformKind::Tx8x8),
        1 => Ok(ResidualTokenV2TransformKind::Tx4x4),
        _ => Err(ResidualTokenV2Error::InvalidSymbol(t)),
    }
}

#[inline]
fn zigzag_u16(n: i16) -> u32 {
    ((n as i32) << 1 ^ ((n as i32) >> 15)) as u32
}

#[inline]
fn unzigzag_u32(z: u32) -> i16 {
    let z = z as i32;
    ((z >> 1) ^ -(z & 1)) as i16
}

fn validate_ac_8x8(freq: &[i16; 64]) -> Result<(), ResidualTokenV2Error> {
    for &zi in ZIGZAG.iter().skip(1) {
        let v = freq[zi];
        if v != 0 && v.unsigned_abs() > MAX_ABS_AC_COEFF as u16 {
            return Err(ResidualTokenV2Error::CoefficientOutOfRange {
                value: v,
                max_abs: MAX_ABS_AC_COEFF,
            });
        }
    }
    Ok(())
}

fn validate_ac_4x4(freq: &[i16; 16]) -> Result<(), ResidualTokenV2Error> {
    for &zi in ZIGZAG_4X4.iter().skip(1) {
        let v = freq[zi];
        if v != 0 && v.unsigned_abs() > MAX_ABS_AC_COEFF as u16 {
            return Err(ResidualTokenV2Error::CoefficientOutOfRange {
                value: v,
                max_abs: MAX_ABS_AC_COEFF,
            });
        }
    }
    Ok(())
}

fn push_token(
    out: &mut Vec<ResidualTokenV2>,
    stats: &mut ResidualTokenV2Stats,
    t: ResidualTokenV2,
) -> Result<(), ResidualTokenV2Error> {
    out.push(t);
    stats.token_count = out.len();
    if stats.token_count > MAX_TOKENS_PER_BLOCK {
        return Err(ResidualTokenV2Error::TokenBudgetExceeded {
            max: MAX_TOKENS_PER_BLOCK,
            got: stats.token_count,
        });
    }
    Ok(())
}

/// Build semantic tokens for one block (**deterministic** zigzag AC order).
pub fn tokenize_residual_v2_block(
    block: ResidualTokenV2Block<'_>,
    ctx: &ResidualTokenV2Context,
) -> Result<(Vec<ResidualTokenV2>, ResidualTokenV2Stats), ResidualTokenV2Error> {
    let mut tokens = Vec::new();
    let mut stats = ResidualTokenV2Stats::default();

    match (&block, ctx.transform) {
        (ResidualTokenV2Block::Tx8x8(freq), ResidualTokenV2TransformKind::Tx8x8) => {
            validate_ac_8x8(freq)?;
            let dc = freq[ZIGZAG[0]];
            push_token(&mut tokens, &mut stats, ResidualTokenV2::DcDelta(dc))?;

            let mut prev_band = coeff_band_8x8_ac(1);
            push_token(
                &mut tokens,
                &mut stats,
                ResidualTokenV2::BandSwitch(prev_band),
            )?;

            let mut zz = 1usize;
            while zz < 64 {
                let band = coeff_band_8x8_ac(zz);
                if band != prev_band {
                    push_token(&mut tokens, &mut stats, ResidualTokenV2::BandSwitch(band))?;
                    prev_band = band;
                }

                if freq[ZIGZAG[zz]] == 0 {
                    let mut run = 0_u32;
                    while zz < 64 && freq[ZIGZAG[zz]] == 0 {
                        run += 1;
                        zz += 1;
                    }
                    stats.zero_run_tokens += 1;
                    stats.zero_run_ac_positions =
                        stats.zero_run_ac_positions.saturating_add(run as u64);
                    if run <= 15 {
                        push_token(
                            &mut tokens,
                            &mut stats,
                            ResidualTokenV2::ZeroRunShort(run as u8),
                        )?;
                    } else {
                        push_token(&mut tokens, &mut stats, ResidualTokenV2::ZeroRunLong(run))?;
                    }
                    continue;
                }

                let v = freq[ZIGZAG[zz]];
                stats.nonzero_ac_count += 1;
                let zzi = u8::try_from(zz).map_err(|_| ResidualTokenV2Error::Syntax("zz idx"))?;
                if v.unsigned_abs() > SMALL_COEFF_ABS_MAX as u16 {
                    stats.large_mag_tokens += 1;
                    push_token(
                        &mut tokens,
                        &mut stats,
                        ResidualTokenV2::LargeCoeffMagnitude {
                            zigzag_ac_index: zzi,
                            value: v,
                        },
                    )?;
                    zz += 1;
                    continue;
                }

                let mut signs = 0_u64;
                let mut cluster: Vec<(u8, u8)> = Vec::new();
                let mut k = 0_u32;
                while zz < 64
                    && freq[ZIGZAG[zz]] != 0
                    && freq[ZIGZAG[zz]].unsigned_abs() <= SMALL_COEFF_ABS_MAX as u16
                {
                    let vv = freq[ZIGZAG[zz]];
                    let zi =
                        u8::try_from(zz).map_err(|_| ResidualTokenV2Error::Syntax("zz idx"))?;
                    cluster.push((zi, vv.unsigned_abs() as u8));
                    if vv < 0 {
                        signs |= 1_u64 << k;
                    }
                    k += 1;
                    stats.small_mag_tokens += 1;
                    zz += 1;
                }
                stats.sign_mask_tokens += 1;
                for (zi, mag) in &cluster {
                    push_token(
                        &mut tokens,
                        &mut stats,
                        ResidualTokenV2::SmallCoeffMagnitude {
                            zigzag_ac_index: *zi,
                            magnitude: *mag,
                        },
                    )?;
                }
                push_token(&mut tokens, &mut stats, ResidualTokenV2::SignMask(signs))?;
            }

            stats.eob_count += 1;
            push_token(&mut tokens, &mut stats, ResidualTokenV2::EndOfBlock)?;
        }
        (ResidualTokenV2Block::Tx4x4(freq), ResidualTokenV2TransformKind::Tx4x4) => {
            validate_ac_4x4(freq)?;
            let dc = freq[ZIGZAG_4X4[0]];
            push_token(&mut tokens, &mut stats, ResidualTokenV2::DcDelta(dc))?;

            let mut prev_band = coeff_band_4x4_ac(1);
            push_token(
                &mut tokens,
                &mut stats,
                ResidualTokenV2::BandSwitch(prev_band),
            )?;

            let mut zz = 1usize;
            while zz < 16 {
                let band = coeff_band_4x4_ac(zz);
                if band != prev_band {
                    push_token(&mut tokens, &mut stats, ResidualTokenV2::BandSwitch(band))?;
                    prev_band = band;
                }

                if freq[ZIGZAG_4X4[zz]] == 0 {
                    let mut run = 0_u32;
                    while zz < 16 && freq[ZIGZAG_4X4[zz]] == 0 {
                        run += 1;
                        zz += 1;
                    }
                    stats.zero_run_tokens += 1;
                    stats.zero_run_ac_positions =
                        stats.zero_run_ac_positions.saturating_add(run as u64);
                    if run <= 15 {
                        push_token(
                            &mut tokens,
                            &mut stats,
                            ResidualTokenV2::ZeroRunShort(run as u8),
                        )?;
                    } else {
                        push_token(&mut tokens, &mut stats, ResidualTokenV2::ZeroRunLong(run))?;
                    }
                    continue;
                }

                let v = freq[ZIGZAG_4X4[zz]];
                stats.nonzero_ac_count += 1;
                let zzi = u8::try_from(zz).map_err(|_| ResidualTokenV2Error::Syntax("zz idx"))?;
                if v.unsigned_abs() > SMALL_COEFF_ABS_MAX as u16 {
                    stats.large_mag_tokens += 1;
                    push_token(
                        &mut tokens,
                        &mut stats,
                        ResidualTokenV2::LargeCoeffMagnitude {
                            zigzag_ac_index: zzi,
                            value: v,
                        },
                    )?;
                    zz += 1;
                    continue;
                }

                let mut signs = 0_u64;
                let mut cluster: Vec<(u8, u8)> = Vec::new();
                let mut k = 0_u32;
                while zz < 16
                    && freq[ZIGZAG_4X4[zz]] != 0
                    && freq[ZIGZAG_4X4[zz]].unsigned_abs() <= SMALL_COEFF_ABS_MAX as u16
                {
                    let vv = freq[ZIGZAG_4X4[zz]];
                    let zi =
                        u8::try_from(zz).map_err(|_| ResidualTokenV2Error::Syntax("zz idx"))?;
                    cluster.push((zi, vv.unsigned_abs() as u8));
                    if vv < 0 {
                        signs |= 1_u64 << k;
                    }
                    k += 1;
                    stats.small_mag_tokens += 1;
                    zz += 1;
                }
                stats.sign_mask_tokens += 1;
                for (zi, mag) in &cluster {
                    push_token(
                        &mut tokens,
                        &mut stats,
                        ResidualTokenV2::SmallCoeffMagnitude {
                            zigzag_ac_index: *zi,
                            magnitude: *mag,
                        },
                    )?;
                }
                push_token(&mut tokens, &mut stats, ResidualTokenV2::SignMask(signs))?;
            }
            stats.eob_count += 1;
            push_token(&mut tokens, &mut stats, ResidualTokenV2::EndOfBlock)?;
        }
        _ => return Err(ResidualTokenV2Error::TransformMismatch),
    }

    Ok((tokens, stats))
}

/// Reconstruct quantized coefficients from semantic tokens (**must** end with [`ResidualTokenV2::EndOfBlock`]).
pub fn detokenize_residual_v2_block(
    tokens: &[ResidualTokenV2],
    ctx: &ResidualTokenV2Context,
    out: &mut ResidualTokenV2BlockMut<'_>,
) -> Result<(), ResidualTokenV2Error> {
    match (&mut *out, ctx.transform) {
        (ResidualTokenV2BlockMut::Tx8x8(freq), ResidualTokenV2TransformKind::Tx8x8) => {
            detokenize_tokens_8x8(tokens, freq)?;
        }
        (ResidualTokenV2BlockMut::Tx4x4(freq), ResidualTokenV2TransformKind::Tx4x4) => {
            detokenize_tokens_4x4(tokens, freq)?;
        }
        _ => return Err(ResidualTokenV2Error::TransformMismatch),
    }
    Ok(())
}

/// Mutable output variant for [`detokenize_residual_v2_block`].
#[derive(Debug)]
pub enum ResidualTokenV2BlockMut<'a> {
    Tx8x8(&'a mut [i16; 64]),
    Tx4x4(&'a mut [i16; 16]),
}

fn detokenize_tokens_8x8(
    tokens: &[ResidualTokenV2],
    freq: &mut [i16; 64],
) -> Result<(), ResidualTokenV2Error> {
    for x in freq.iter_mut() {
        *x = 0;
    }
    let mut zz: usize = 1;
    let mut idx = 0usize;
    let mut pending: Vec<(u8, u8)> = Vec::new();
    let mut ac_decoded: usize = 0;

    while idx < tokens.len() {
        match &tokens[idx] {
            ResidualTokenV2::DcDelta(d) => {
                freq[ZIGZAG[0]] = *d;
                idx += 1;
            }
            ResidualTokenV2::BandSwitch(_) => idx += 1,
            ResidualTokenV2::PlaneSwitch(_) => idx += 1,
            ResidualTokenV2::ZeroRunShort(n) => {
                zz = zz.saturating_add(*n as usize);
                // **`zz`** is the next AC zigzag index (**1..63**) or **`64`** when the scan is exhausted.
                if zz > 64 {
                    return Err(ResidualTokenV2Error::Syntax("zero run overflow 8x8"));
                }
                idx += 1;
            }
            ResidualTokenV2::ZeroRunLong(n) => {
                zz = zz.saturating_add(*n as usize);
                if zz > 64 {
                    return Err(ResidualTokenV2Error::Syntax("zero run overflow 8x8"));
                }
                idx += 1;
            }
            ResidualTokenV2::SmallCoeffMagnitude {
                zigzag_ac_index,
                magnitude,
            } => {
                if pending.is_empty() {
                    if *zigzag_ac_index as usize != zz {
                        return Err(ResidualTokenV2Error::Syntax("small zz mismatch"));
                    }
                } else {
                    let last = pending.last().unwrap().0 as usize;
                    if *zigzag_ac_index as usize != last + 1 {
                        return Err(ResidualTokenV2Error::Syntax(
                            "small cluster not consecutive",
                        ));
                    }
                }
                pending.push((*zigzag_ac_index, *magnitude));
                idx += 1;
            }
            ResidualTokenV2::SignMask(bits) => {
                if pending.is_empty() {
                    return Err(ResidualTokenV2Error::Syntax("orphan SignMask"));
                }
                if pending.len() > 64 {
                    return Err(ResidualTokenV2Error::Syntax("sign cluster too long"));
                }
                for (i, (zzi, mag)) in pending.iter().enumerate() {
                    let neg = (bits >> i) & 1 != 0;
                    let v = *mag as i16;
                    let vv = if neg { -v } else { v };
                    let pos = *zzi as usize;
                    if pos == 0 || pos >= 64 {
                        return Err(ResidualTokenV2Error::Syntax("bad zz small"));
                    }
                    freq[ZIGZAG[pos]] = vv;
                    ac_decoded += 1;
                    if ac_decoded > 63 {
                        return Err(ResidualTokenV2Error::TooManyCoefficients {
                            max: 63,
                            got: ac_decoded,
                        });
                    }
                }
                let last_zz = pending.last().unwrap().0 as usize;
                zz = last_zz + 1;
                pending.clear();
                idx += 1;
            }
            ResidualTokenV2::LargeCoeffMagnitude {
                zigzag_ac_index,
                value,
            } => {
                if !pending.is_empty() {
                    return Err(ResidualTokenV2Error::Syntax("pending small before large"));
                }
                let pos = *zigzag_ac_index as usize;
                if pos != zz {
                    return Err(ResidualTokenV2Error::Syntax("large zz mismatch"));
                }
                if zz >= 64 {
                    return Err(ResidualTokenV2Error::Syntax("large after AC exhausted"));
                }
                if pos == 0 || pos >= 64 {
                    return Err(ResidualTokenV2Error::Syntax("bad zz large"));
                }
                freq[ZIGZAG[pos]] = *value;
                ac_decoded += 1;
                if ac_decoded > 63 {
                    return Err(ResidualTokenV2Error::TooManyCoefficients {
                        max: 63,
                        got: ac_decoded,
                    });
                }
                zz = pos + 1;
                idx += 1;
            }
            ResidualTokenV2::EndOfBlock => {
                if !pending.is_empty() {
                    return Err(ResidualTokenV2Error::Syntax("pending at EOB"));
                }
                idx += 1;
                break;
            }
        }
    }

    if !matches!(tokens.last(), Some(ResidualTokenV2::EndOfBlock)) {
        return Err(ResidualTokenV2Error::Syntax("missing EndOfBlock"));
    }
    if idx != tokens.len() {
        return Err(ResidualTokenV2Error::TrailingGarbage);
    }
    Ok(())
}

fn detokenize_tokens_4x4(
    tokens: &[ResidualTokenV2],
    freq: &mut [i16; 16],
) -> Result<(), ResidualTokenV2Error> {
    for x in freq.iter_mut() {
        *x = 0;
    }
    let mut zz: usize = 1;
    let mut idx = 0usize;
    let mut pending: Vec<(u8, u8)> = Vec::new();
    let mut ac_decoded: usize = 0;

    while idx < tokens.len() {
        match &tokens[idx] {
            ResidualTokenV2::DcDelta(d) => {
                freq[ZIGZAG_4X4[0]] = *d;
                idx += 1;
            }
            ResidualTokenV2::BandSwitch(_) => idx += 1,
            ResidualTokenV2::PlaneSwitch(_) => idx += 1,
            ResidualTokenV2::ZeroRunShort(n) => {
                zz = zz.saturating_add(*n as usize);
                if zz > 16 {
                    return Err(ResidualTokenV2Error::Syntax("zero run overflow 4x4"));
                }
                idx += 1;
            }
            ResidualTokenV2::ZeroRunLong(n) => {
                zz = zz.saturating_add(*n as usize);
                if zz > 16 {
                    return Err(ResidualTokenV2Error::Syntax("zero run overflow 4x4"));
                }
                idx += 1;
            }
            ResidualTokenV2::SmallCoeffMagnitude {
                zigzag_ac_index,
                magnitude,
            } => {
                if pending.is_empty() {
                    if *zigzag_ac_index as usize != zz {
                        return Err(ResidualTokenV2Error::Syntax("small zz mismatch"));
                    }
                } else {
                    let last = pending.last().unwrap().0 as usize;
                    if *zigzag_ac_index as usize != last + 1 {
                        return Err(ResidualTokenV2Error::Syntax(
                            "small cluster not consecutive",
                        ));
                    }
                }
                pending.push((*zigzag_ac_index, *magnitude));
                idx += 1;
            }
            ResidualTokenV2::SignMask(bits) => {
                if pending.is_empty() {
                    return Err(ResidualTokenV2Error::Syntax("orphan SignMask"));
                }
                for (i, (zzi, mag)) in pending.iter().enumerate() {
                    let neg = (bits >> i) & 1 != 0;
                    let v = *mag as i16;
                    let vv = if neg { -v } else { v };
                    let pos = *zzi as usize;
                    if pos == 0 || pos >= 16 {
                        return Err(ResidualTokenV2Error::Syntax("bad zz 4x4"));
                    }
                    freq[ZIGZAG_4X4[pos]] = vv;
                    ac_decoded += 1;
                    if ac_decoded > 15 {
                        return Err(ResidualTokenV2Error::TooManyCoefficients {
                            max: 15,
                            got: ac_decoded,
                        });
                    }
                }
                let last_zz = pending.last().unwrap().0 as usize;
                zz = last_zz + 1;
                pending.clear();
                idx += 1;
            }
            ResidualTokenV2::LargeCoeffMagnitude {
                zigzag_ac_index,
                value,
            } => {
                if !pending.is_empty() {
                    return Err(ResidualTokenV2Error::Syntax("pending small before large"));
                }
                let pos = *zigzag_ac_index as usize;
                if pos != zz {
                    return Err(ResidualTokenV2Error::Syntax("large zz mismatch"));
                }
                if zz >= 16 {
                    return Err(ResidualTokenV2Error::Syntax("large after AC exhausted"));
                }
                if pos == 0 || pos >= 16 {
                    return Err(ResidualTokenV2Error::Syntax("bad zz large 4x4"));
                }
                freq[ZIGZAG_4X4[pos]] = *value;
                ac_decoded += 1;
                if ac_decoded > 15 {
                    return Err(ResidualTokenV2Error::TooManyCoefficients {
                        max: 15,
                        got: ac_decoded,
                    });
                }
                zz = pos + 1;
                idx += 1;
            }
            ResidualTokenV2::EndOfBlock => {
                if !pending.is_empty() {
                    return Err(ResidualTokenV2Error::Syntax("pending at EOB"));
                }
                idx += 1;
                break;
            }
        }
    }

    if !matches!(tokens.last(), Some(ResidualTokenV2::EndOfBlock)) {
        return Err(ResidualTokenV2Error::Syntax("missing EndOfBlock"));
    }
    if idx != tokens.len() {
        return Err(ResidualTokenV2Error::TrailingGarbage);
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Wire codec VERSION 3
// -----------------------------------------------------------------------------

const TAG_EOB: u8 = 0x00;
const TAG_ZR_SHORT: u8 = 0x01;
const TAG_ZR_LONG: u8 = 0x02;
const TAG_SMALL: u8 = 0x03;
const TAG_LARGE: u8 = 0x04;
const TAG_SIGN: u8 = 0x05;
const TAG_DC: u8 = 0x06;
const TAG_BAND: u8 = 0x07;
const TAG_PLANE: u8 = 0x08;

fn band_tag(b: CoeffBand) -> u8 {
    match b {
        CoeffBand::Dc => 0,
        CoeffBand::Low => 1,
        CoeffBand::Mid => 2,
        CoeffBand::High => 3,
    }
}

fn band_from_tag(t: u8) -> Result<CoeffBand, ResidualTokenV2Error> {
    match t {
        0 => Ok(CoeffBand::Dc),
        1 => Ok(CoeffBand::Low),
        2 => Ok(CoeffBand::Mid),
        3 => Ok(CoeffBand::High),
        _ => Err(ResidualTokenV2Error::InvalidSymbol(t)),
    }
}

#[inline]
fn pack_ctx_flags(ctx: &ResidualTokenV2Context) -> u8 {
    let neighbor = ctx.neighbor_residual_present as u8;
    let act = match ctx.activity {
        BlockActivityClass::Flat => 0_u8,
        BlockActivityClass::Edge => 1,
        BlockActivityClass::Textured => 2,
    };
    let prev = ctx.prev_nonzero_ac_count.min(31);
    neighbor | (act << 1) | (prev << 3)
}

fn write_uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(((v & 0x7F) as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn read_uvarint(data: &[u8], cur: &mut usize) -> Result<u64, ResidualTokenV2Error> {
    let mut shift = 0_u32;
    let mut val = 0_u64;
    loop {
        let b = *data.get(*cur).ok_or(ResidualTokenV2Error::Truncated)?;
        *cur += 1;
        val |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Err(ResidualTokenV2Error::Syntax("varint overflow"));
        }
    }
    Ok(val)
}

fn encode_tokens_v3(tokens: &[ResidualTokenV2]) -> Result<Vec<u8>, ResidualTokenV2Error> {
    if tokens.len() > MAX_TOKENS_PER_BLOCK {
        return Err(ResidualTokenV2Error::TokenBudgetExceeded {
            max: MAX_TOKENS_PER_BLOCK,
            got: tokens.len(),
        });
    }
    let mut out = Vec::new();
    for t in tokens {
        match t {
            ResidualTokenV2::EndOfBlock => out.push(TAG_EOB),
            ResidualTokenV2::ZeroRunShort(n) => {
                let n = *n;
                if n == 0 || n > 15 {
                    return Err(ResidualTokenV2Error::Syntax("ZeroRunShort range"));
                }
                out.push(TAG_ZR_SHORT);
                out.push(n);
            }
            ResidualTokenV2::ZeroRunLong(n) => {
                out.push(TAG_ZR_LONG);
                write_uvarint(&mut out, u64::from(*n));
            }
            ResidualTokenV2::SmallCoeffMagnitude {
                zigzag_ac_index,
                magnitude,
            } => {
                out.push(TAG_SMALL);
                out.push(*zigzag_ac_index);
                out.push(*magnitude);
            }
            ResidualTokenV2::LargeCoeffMagnitude {
                zigzag_ac_index,
                value,
            } => {
                out.push(TAG_LARGE);
                out.push(*zigzag_ac_index);
                write_uvarint(&mut out, u64::from(zigzag_u16(*value)));
            }
            ResidualTokenV2::SignMask(bits) => {
                out.push(TAG_SIGN);
                write_uvarint(&mut out, *bits);
            }
            ResidualTokenV2::DcDelta(dc) => {
                out.push(TAG_DC);
                write_uvarint(&mut out, u64::from(zigzag_u16(*dc)));
            }
            ResidualTokenV2::BandSwitch(b) => {
                out.push(TAG_BAND);
                out.push(band_tag(*b));
            }
            ResidualTokenV2::PlaneSwitch(p) => {
                out.push(TAG_PLANE);
                out.push(plane_tag(*p));
            }
        }
    }
    Ok(out)
}

fn decode_tokens_v3(data: &[u8]) -> Result<Vec<ResidualTokenV2>, ResidualTokenV2Error> {
    let mut cur = 0usize;
    let mut out = Vec::new();
    while cur < data.len() {
        let tag = *data.get(cur).ok_or(ResidualTokenV2Error::Truncated)?;
        cur += 1;
        let tok = match tag {
            TAG_EOB => ResidualTokenV2::EndOfBlock,
            TAG_ZR_SHORT => {
                let n = *data.get(cur).ok_or(ResidualTokenV2Error::Truncated)?;
                cur += 1;
                if n == 0 || n > 15 {
                    return Err(ResidualTokenV2Error::Syntax("ZeroRunShort range"));
                }
                ResidualTokenV2::ZeroRunShort(n)
            }
            TAG_ZR_LONG => {
                let v = read_uvarint(data, &mut cur)?;
                ResidualTokenV2::ZeroRunLong(v.min(u32::MAX as u64) as u32)
            }
            TAG_SMALL => {
                let zz = *data.get(cur).ok_or(ResidualTokenV2Error::Truncated)?;
                let mag = *data.get(cur + 1).ok_or(ResidualTokenV2Error::Truncated)?;
                cur += 2;
                ResidualTokenV2::SmallCoeffMagnitude {
                    zigzag_ac_index: zz,
                    magnitude: mag,
                }
            }
            TAG_LARGE => {
                let zz = *data.get(cur).ok_or(ResidualTokenV2Error::Truncated)?;
                cur += 1;
                let z = read_uvarint(data, &mut cur)?;
                if z > u32::MAX as u64 {
                    return Err(ResidualTokenV2Error::Syntax("large coeff zz overflow"));
                }
                let v = unzigzag_u32(z as u32);
                ResidualTokenV2::LargeCoeffMagnitude {
                    zigzag_ac_index: zz,
                    value: v,
                }
            }
            TAG_SIGN => {
                let bits = read_uvarint(data, &mut cur)?;
                ResidualTokenV2::SignMask(bits)
            }
            TAG_DC => {
                let z = read_uvarint(data, &mut cur)?;
                ResidualTokenV2::DcDelta(unzigzag_u32(z.min(u32::MAX as u64) as u32))
            }
            TAG_BAND => {
                let b = *data.get(cur).ok_or(ResidualTokenV2Error::Truncated)?;
                cur += 1;
                ResidualTokenV2::BandSwitch(band_from_tag(b)?)
            }
            TAG_PLANE => {
                let p = *data.get(cur).ok_or(ResidualTokenV2Error::Truncated)?;
                cur += 1;
                ResidualTokenV2::PlaneSwitch(plane_from_tag(p)?)
            }
            _ => return Err(ResidualTokenV2Error::InvalidSymbol(tag)),
        };
        out.push(tok);
        if out.len() > MAX_TOKENS_PER_BLOCK {
            return Err(ResidualTokenV2Error::TokenBudgetExceeded {
                max: MAX_TOKENS_PER_BLOCK,
                got: out.len(),
            });
        }
    }
    Ok(out)
}

/// Structured **VERSION 3** wire: header + token blob (**no trailing garbage** allowed at block scope).
pub fn encode_residual_token_v2_block(
    block: ResidualTokenV2Block<'_>,
    ctx: &ResidualTokenV2Context,
    mode: ResidualTokenV2Mode,
) -> Result<Vec<u8>, ResidualTokenV2Error> {
    Ok(encode_residual_token_v2_block_with_stats(block, ctx, mode)?.0)
}

/// Encode one block and return wire bytes plus [`ResidualTokenV2Stats`] from the tokenization pass.
pub fn encode_residual_token_v2_block_with_stats(
    block: ResidualTokenV2Block<'_>,
    ctx: &ResidualTokenV2Context,
    mode: ResidualTokenV2Mode,
) -> Result<(Vec<u8>, ResidualTokenV2Stats), ResidualTokenV2Error> {
    match mode {
        ResidualTokenV2Mode::LegacyBandCompactV2 => match block {
            ResidualTokenV2Block::Tx8x8(freq) => {
                let v = encode_ac_payload_legacy_v2(freq, plane_tag(ctx.plane))?;
                Ok((v, ResidualTokenV2Stats::default()))
            }
            ResidualTokenV2Block::Tx4x4(_) => Err(ResidualTokenV2Error::Syntax(
                "LegacyBandCompactV2 requires Tx8x8",
            )),
        },
        ResidualTokenV2Mode::StructuredTokenV3 => {
            let (tokens, stats) = tokenize_residual_v2_block(block, ctx)?;
            let body = encode_tokens_v3(&tokens)?;
            let mut out = Vec::with_capacity(8 + body.len());
            out.push(MAGIC);
            out.push(VERSION_STRUCTURED_V3);
            out.push(transform_tag(ctx.transform));
            out.push(plane_tag(ctx.plane));
            out.push(pack_ctx_flags(ctx));
            out.extend_from_slice(&body);
            Ok((out, stats))
        }
    }
}

/// Decode one block (**VERSION 2** or **3**). Returns bytes consumed from **`data`**.
pub fn decode_residual_token_v2_block(
    data: &[u8],
    ctx_expected: &ResidualTokenV2Context,
    out: &mut ResidualTokenV2BlockMut<'_>,
    _mode_hint: Option<ResidualTokenV2Mode>,
) -> Result<usize, ResidualTokenV2Error> {
    if data.len() < 2 {
        return Err(ResidualTokenV2Error::Truncated);
    }
    if data[0] != MAGIC {
        return Err(ResidualTokenV2Error::Syntax("bad magic"));
    }
    match data[1] {
        VERSION_LEGACY_V2 => match out {
            ResidualTokenV2BlockMut::Tx8x8(freq) => {
                let n = decode_ac_payload_legacy_v2(data, freq)?;
                if n != data.len() {
                    return Err(ResidualTokenV2Error::TrailingGarbage);
                }
                Ok(n)
            }
            ResidualTokenV2BlockMut::Tx4x4(_) => Err(ResidualTokenV2Error::Syntax(
                "legacy v2 requires Tx8x8 output",
            )),
        },
        VERSION_STRUCTURED_V3 => {
            if data.len() < 6 {
                return Err(ResidualTokenV2Error::Truncated);
            }
            let transform = transform_from_tag(data[2])?;
            let plane = plane_from_tag(data[3])?;
            let _flags = data[4];
            if transform != ctx_expected.transform || plane != ctx_expected.plane {
                return Err(ResidualTokenV2Error::Syntax("header/context mismatch"));
            }
            let body = &data[5..];
            let tokens = decode_tokens_v3(body)?;
            let mut tmp_ctx = ctx_expected.clone();
            tmp_ctx.transform = transform;
            tmp_ctx.plane = plane;
            detokenize_residual_v2_block(&tokens, &tmp_ctx, out)?;
            Ok(data.len())
        }
        _ => Err(ResidualTokenV2Error::InvalidSymbol(data[1])),
    }
}

/// Concatenate multiple blocks for one plane (**each** must use the same **`ResidualTokenV2Plane`** in **`ctx`**).
pub fn encode_residual_token_v2_plane(
    plane: ResidualTokenV2Plane,
    blocks: &[(ResidualTokenV2Context, ResidualTokenV2Block<'_>)],
    mode: ResidualTokenV2Mode,
) -> Result<Vec<u8>, ResidualTokenV2Error> {
    let mut out = Vec::new();
    out.push(MAGIC);
    out.push(VERSION_PLANE_CONTAINER);
    out.push(plane_tag(plane));
    write_uvarint(&mut out, blocks.len() as u64);
    for (ctx, block) in blocks {
        if ctx.plane != plane {
            return Err(ResidualTokenV2Error::Syntax("plane/context mismatch"));
        }
        let chunk = encode_residual_token_v2_block(*block, ctx, mode)?;
        write_uvarint(&mut out, chunk.len() as u64);
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Decode plane container produced by [`encode_residual_token_v2_plane`].
pub fn decode_residual_token_v2_plane(
    data: &[u8],
    _mode_hint: Option<ResidualTokenV2Mode>,
) -> Result<(ResidualTokenV2Plane, Vec<Vec<u8>>), ResidualTokenV2Error> {
    if data.len() < 4 || data[0] != MAGIC || data[1] != VERSION_PLANE_CONTAINER {
        return Err(ResidualTokenV2Error::Syntax("bad plane container"));
    }
    let plane = plane_from_tag(data[2])?;
    let mut cur = 3usize;
    let nblocks = read_uvarint(data, &mut cur)? as usize;
    let mut chunks = Vec::with_capacity(nblocks);
    for _ in 0..nblocks {
        let blen = read_uvarint(data, &mut cur)? as usize;
        let end = cur
            .checked_add(blen)
            .ok_or(ResidualTokenV2Error::Truncated)?;
        if end > data.len() {
            return Err(ResidualTokenV2Error::Truncated);
        }
        chunks.push(data[cur..end].to_vec());
        cur = end;
    }
    if cur != data.len() {
        return Err(ResidualTokenV2Error::TrailingGarbage);
    }
    Ok((plane, chunks))
}

pub fn estimate_residual_token_v2_bytes(
    block: ResidualTokenV2Block<'_>,
    ctx: &ResidualTokenV2Context,
    mode: ResidualTokenV2Mode,
) -> Result<usize, ResidualTokenV2Error> {
    Ok(encode_residual_token_v2_block(block, ctx, mode)?.len())
}

pub fn residual_token_v2_summary(stats: &ResidualTokenV2Stats) -> String {
    format!(
        "tokens={} nonzero_ac={} zr_tok={} zr_ac={} small={} large={} sign_masks={} eob={}",
        stats.token_count,
        stats.nonzero_ac_count,
        stats.zero_run_tokens,
        stats.zero_run_ac_positions,
        stats.small_mag_tokens,
        stats.large_mag_tokens,
        stats.sign_mask_tokens,
        stats.eob_count
    )
}

// -----------------------------------------------------------------------------
// Legacy VERSION 2 (**[`encode_ac_payload`]** / **`decode_ac_payload`**)
// -----------------------------------------------------------------------------

#[inline]
fn plane_zrun_bias_legacy(plane: u8) -> u64 {
    match plane & 3 {
        0 => 0,
        1 => 2,
        2 => 5,
        _ => 0,
    }
}

fn encode_ac_payload_legacy_v2(
    freq: &[i16; 64],
    plane: u8,
) -> Result<Vec<u8>, ResidualTokenV2Error> {
    validate_ac_8x8(freq)?;
    let mut out = Vec::new();
    out.push(MAGIC);
    out.push(VERSION_LEGACY_V2);
    out.push(plane & 3);
    let bias = plane_zrun_bias_legacy(plane);

    for band in 0..4 {
        let len = LEGACY_BAND_LEN[band];
        let first = LEGACY_BAND_FIRST_ZZ[band];
        let mut local = vec![0_i16; len];
        for (i, slot) in local.iter_mut().enumerate().take(len) {
            let zi = first + i;
            *slot = freq[ZIGZAG[zi]];
        }

        let mut last_nz = None::<usize>;
        for i in (0..len).rev() {
            if local[i] != 0 {
                last_nz = Some(i);
                break;
            }
        }

        if last_nz.is_none() {
            out.push(0x00);
            continue;
        }
        let last = last_nz.unwrap();
        let hl = u8::try_from(last + 1).map_err(|_| ResidualTokenV2Error::Syntax("band hdr"))?;
        out.push(hl);

        let mut pos = 0usize;
        while pos <= last {
            let mut zrun = 0_u64;
            while pos <= last && local[pos] == 0 {
                zrun += 1;
                pos += 1;
            }
            write_uvarint(&mut out, zrun.saturating_add(bias));

            if pos > last {
                break;
            }

            let v = local[pos];
            write_uvarint(&mut out, u64::from(zigzag_u16(v)));
            pos += 1;
        }
    }

    Ok(out)
}

fn decode_ac_payload_legacy_v2(
    data: &[u8],
    freq: &mut [i16; 64],
) -> Result<usize, ResidualTokenV2Error> {
    if data.len() < 3 {
        return Err(ResidualTokenV2Error::Truncated);
    }
    if data[0] != MAGIC {
        return Err(ResidualTokenV2Error::Syntax("bad magic"));
    }
    if data[1] != VERSION_LEGACY_V2 {
        return Err(ResidualTokenV2Error::Syntax("expected legacy v2"));
    }
    let plane = data[2] & 3;
    let bias = plane_zrun_bias_legacy(plane);
    let mut cur = 3usize;

    for k in ZIGZAG.iter().skip(1) {
        freq[*k] = 0;
    }

    for band in 0..4 {
        let len = LEGACY_BAND_LEN[band];
        let first = LEGACY_BAND_FIRST_ZZ[band];
        let ctrl = *data.get(cur).ok_or(ResidualTokenV2Error::Truncated)?;
        cur += 1;
        if ctrl == 0x00 {
            continue;
        }
        let last = (ctrl as usize)
            .checked_sub(1)
            .ok_or(ResidualTokenV2Error::Syntax("band hdr"))?;
        if last >= len {
            return Err(ResidualTokenV2Error::Syntax("last_local"));
        }

        let mut pos = 0usize;
        while pos <= last {
            let z_enc = read_uvarint(data, &mut cur)?;
            let zrun = z_enc.saturating_sub(bias);
            let zr = usize::try_from(zrun).map_err(|_| ResidualTokenV2Error::Syntax("zrun"))?;
            pos = pos
                .checked_add(zr)
                .ok_or(ResidualTokenV2Error::Syntax("zrun overflow"))?;
            if pos > last {
                return Err(ResidualTokenV2Error::Syntax("zrun overflow"));
            }

            let zz = read_uvarint(data, &mut cur)?;
            if zz > u64::from(u32::MAX) {
                return Err(ResidualTokenV2Error::Syntax("coeff zz"));
            }
            let v = unzigzag_u32(zz as u32);
            if v == 0 || v.unsigned_abs() > MAX_ABS_AC_COEFF as u16 {
                return Err(ResidualTokenV2Error::Syntax("bad coeff"));
            }
            let idx = ZIGZAG[first + pos];
            freq[idx] = v;
            pos += 1;
        }
    }

    Ok(cur)
}

/// Legacy **VERSION 2** compact AC payload (**DC unchanged**). Maps errors to telemetry callers via [`SrsV2Error`].
pub fn encode_ac_payload(freq: &[i16; 64], plane: u8) -> Result<Vec<u8>, SrsV2Error> {
    encode_ac_payload_legacy_v2(freq, plane).map_err(map_v2_err)
}

/// Legacy **VERSION 2** decode; returns bytes consumed.
pub fn decode_ac_payload(data: &[u8], freq: &mut [i16; 64]) -> Result<usize, SrsV2Error> {
    decode_ac_payload_legacy_v2(data, freq).map_err(map_v2_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_8() -> ResidualTokenV2Context {
        ResidualTokenV2Context {
            plane: ResidualTokenV2Plane::Y,
            transform: ResidualTokenV2TransformKind::Tx8x8,
            ..Default::default()
        }
    }

    fn ctx_4() -> ResidualTokenV2Context {
        ResidualTokenV2Context {
            plane: ResidualTokenV2Plane::Y,
            transform: ResidualTokenV2TransformKind::Tx4x4,
            ..Default::default()
        }
    }

    fn roundtrip_v3(freq: &[i16; 64], ctx: &ResidualTokenV2Context) {
        let enc = encode_residual_token_v2_block(
            ResidualTokenV2Block::Tx8x8(freq),
            ctx,
            ResidualTokenV2Mode::StructuredTokenV3,
        )
        .unwrap();
        let mut out = [0_i16; 64];
        let mut block = ResidualTokenV2BlockMut::Tx8x8(&mut out);
        let n = decode_residual_token_v2_block(&enc, ctx, &mut block, None).unwrap();
        assert_eq!(n, enc.len());
        assert_eq!(out, *freq);
    }

    #[test]
    fn all_zero_block_roundtrip_v3() {
        let mut f = [0_i16; 64];
        f[0] = 11;
        roundtrip_v3(&f, &ctx_8());
    }

    #[test]
    fn dc_only_roundtrip_v3() {
        let mut f = [0_i16; 64];
        f[ZIGZAG[0]] = -40;
        roundtrip_v3(&f, &ctx_8());
    }

    #[test]
    fn sparse_ac_roundtrip_v3() {
        let mut f = [0_i16; 64];
        f[0] = 3;
        f[ZIGZAG[5]] = -2;
        f[ZIGZAG[40]] = 7;
        roundtrip_v3(&f, &ctx_8());
    }

    #[test]
    fn dense_noisy_roundtrip_v3() {
        let mut f = [0_i16; 64];
        f[0] = 5;
        let mut zi = 1;
        while zi < 64 {
            f[ZIGZAG[zi]] = (((zi * 13) % 17) as i16).saturating_sub(8).clamp(-127, 127);
            zi += 1;
        }
        roundtrip_v3(&f, &ctx_8());
    }

    #[test]
    fn long_zero_run_roundtrip_v3() {
        let mut f = [0_i16; 64];
        f[0] = 1;
        f[ZIGZAG[50]] = 3;
        roundtrip_v3(&f, &ctx_8());
    }

    #[test]
    fn large_coeff_roundtrip_v3() {
        let mut f = [0_i16; 64];
        f[0] = 0;
        f[ZIGZAG[10]] = MAX_ABS_AC_COEFF;
        f[ZIGZAG[11]] = -MAX_ABS_AC_COEFF;
        roundtrip_v3(&f, &ctx_8());
    }

    #[test]
    fn sign_packing_small_roundtrip_v3() {
        let mut f = [0_i16; 64];
        f[0] = 8;
        f[ZIGZAG[1]] = -3;
        f[ZIGZAG[2]] = 4;
        roundtrip_v3(&f, &ctx_8());
    }

    #[test]
    fn malformed_tag_rejected_v3() {
        let mut junk = vec![MAGIC, VERSION_STRUCTURED_V3, 0, 0, 0];
        junk.push(0xFF);
        let mut out = [0_i16; 64];
        let ctx = ctx_8();
        let mut b = ResidualTokenV2BlockMut::Tx8x8(&mut out);
        assert!(decode_residual_token_v2_block(&junk, &ctx, &mut b, None).is_err());
    }

    #[test]
    fn token_budget_enforced() {
        let tokens = vec![ResidualTokenV2::BandSwitch(CoeffBand::Low); MAX_TOKENS_PER_BLOCK + 1];
        assert!(encode_tokens_v3(&tokens).is_err());
    }

    #[test]
    fn truncated_stream_rejected_v3() {
        let enc = encode_residual_token_v2_block(
            ResidualTokenV2Block::Tx8x8(&[0_i16; 64]),
            &ctx_8(),
            ResidualTokenV2Mode::StructuredTokenV3,
        )
        .unwrap();
        let chopped = &enc[..enc.len().saturating_sub(2)];
        let mut out = [0_i16; 64];
        let mut b = ResidualTokenV2BlockMut::Tx8x8(&mut out);
        assert!(matches!(
            decode_residual_token_v2_block(chopped, &ctx_8(), &mut b, None),
            Err(ResidualTokenV2Error::Truncated)
        ));
    }

    #[test]
    fn trailing_garbage_rejected_v3() {
        let mut enc = encode_residual_token_v2_block(
            ResidualTokenV2Block::Tx8x8(&[0_i16; 64]),
            &ctx_8(),
            ResidualTokenV2Mode::StructuredTokenV3,
        )
        .unwrap();
        enc.push(0x00);
        let mut out = [0_i16; 64];
        let mut b = ResidualTokenV2BlockMut::Tx8x8(&mut out);
        assert!(matches!(
            decode_residual_token_v2_block(&enc, &ctx_8(), &mut b, None),
            Err(ResidualTokenV2Error::TrailingGarbage)
        ));
    }

    #[test]
    fn invalid_wire_version_rejected() {
        let mut enc = encode_residual_token_v2_block(
            ResidualTokenV2Block::Tx8x8(&[0_i16; 64]),
            &ctx_8(),
            ResidualTokenV2Mode::StructuredTokenV3,
        )
        .unwrap();
        enc[1] = 0x99;
        let mut out = [0_i16; 64];
        let mut b = ResidualTokenV2BlockMut::Tx8x8(&mut out);
        assert!(matches!(
            decode_residual_token_v2_block(&enc, &ctx_8(), &mut b, None),
            Err(ResidualTokenV2Error::InvalidSymbol(0x99))
        ));
    }

    #[test]
    fn residual_token_v2_summary_smoke() {
        let s = residual_token_v2_summary(&ResidualTokenV2Stats {
            token_count: 10,
            nonzero_ac_count: 3,
            zero_run_tokens: 1,
            zero_run_ac_positions: 5,
            small_mag_tokens: 4,
            large_mag_tokens: 0,
            sign_mask_tokens: 1,
            eob_count: 1,
        });
        assert!(s.contains("tokens=10"));
    }

    #[test]
    fn encode_plane_roundtrip_container() {
        let mut f = [0_i16; 64];
        f[0] = 2;
        f[ZIGZAG[2]] = -4;
        let ctx = ctx_8();
        let blob = encode_residual_token_v2_plane(
            ResidualTokenV2Plane::Y,
            &[(ctx.clone(), ResidualTokenV2Block::Tx8x8(&f))],
            ResidualTokenV2Mode::StructuredTokenV3,
        )
        .unwrap();
        let (plane, chunks) = decode_residual_token_v2_plane(&blob, None).unwrap();
        assert_eq!(plane, ResidualTokenV2Plane::Y);
        assert_eq!(chunks.len(), 1);
        let mut out = [0_i16; 64];
        let mut b = ResidualTokenV2BlockMut::Tx8x8(&mut out);
        decode_residual_token_v2_block(&chunks[0], &ctx, &mut b, None).unwrap();
        assert_eq!(out, f);
    }

    #[test]
    fn deterministic_encode_v3() {
        let mut f = [0_i16; 64];
        f[ZIGZAG[3]] = -9;
        let ctx = ctx_8();
        let a = encode_residual_token_v2_block(
            ResidualTokenV2Block::Tx8x8(&f),
            &ctx,
            ResidualTokenV2Mode::StructuredTokenV3,
        )
        .unwrap();
        let b = encode_residual_token_v2_block(
            ResidualTokenV2Block::Tx8x8(&f),
            &ctx,
            ResidualTokenV2Mode::StructuredTokenV3,
        )
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn legacy_v2_roundtrip_unchanged() {
        let mut f = [0_i16; 64];
        f[0] = 100;
        f[ZIGZAG[8]] = -33;
        let enc = encode_ac_payload(&f, 0).unwrap();
        let mut o = f;
        let n = decode_ac_payload(&enc, &mut o).unwrap();
        assert_eq!(n, enc.len());
        assert_eq!(o, f);
    }

    #[test]
    fn tx4x4_roundtrip_v3() {
        let mut f = [0_i16; 16];
        f[0] = 4;
        f[ZIGZAG_4X4[3]] = -2;
        let ctx = ctx_4();
        let enc = encode_residual_token_v2_block(
            ResidualTokenV2Block::Tx4x4(&f),
            &ctx,
            ResidualTokenV2Mode::StructuredTokenV3,
        )
        .unwrap();
        let mut out = [0_i16; 16];
        let mut b = ResidualTokenV2BlockMut::Tx4x4(&mut out);
        decode_residual_token_v2_block(&enc, &ctx, &mut b, None).unwrap();
        assert_eq!(out, f);
    }
}
