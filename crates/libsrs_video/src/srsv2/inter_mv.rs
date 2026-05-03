//! Median MV prediction and compact signed varints for FR2 **rev15**/**17** (P) and **rev16**/**18** (B) inter syntax.

use libsrs_bitio::{rans_decode, rans_encode, RansModel, RANS_SCALE};

use super::error::SrsV2Error;

/// Hostile-input cap on continuation bytes per varint.
pub const MAX_INTER_MV_VARINT_BYTES: usize = 8;

pub fn median_i32(a: i32, b: i32, c: i32) -> i32 {
    let mut v = [a, b, c];
    v.sort_unstable();
    v[1]
}

/// Median predictor over left / top / top-right **already reconstructed** MVs (raster order).
pub fn predict_mv_qpel(
    mb_x: u32,
    mb_y: u32,
    mb_cols: u32,
    decoded_mvs: &[(i32, i32)],
) -> (i32, i32) {
    let idx = (mb_y * mb_cols + mb_x) as usize;
    let left = if mb_x > 0 {
        Some(decoded_mvs[idx - 1])
    } else {
        None
    };
    let top = if mb_y > 0 {
        Some(decoded_mvs[idx - mb_cols as usize])
    } else {
        None
    };
    let tr = if mb_y > 0 && mb_x + 1 < mb_cols {
        Some(decoded_mvs[idx - mb_cols as usize + 1])
    } else {
        None
    };
    let (lx, ly) = left.unwrap_or((0, 0));
    let (tx, ty) = top.unwrap_or((0, 0));
    let (rx, ry) = tr.unwrap_or((0, 0));
    (median_i32(lx, tx, rx), median_i32(ly, ty, ry))
}

#[inline]
pub fn zigzag_encode_i32(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

#[inline]
pub fn zigzag_decode_u32(u: u32) -> i32 {
    ((u >> 1) as i32) ^ (-((u & 1) as i32))
}

pub fn write_uvarint32(out: &mut Vec<u8>, mut u: u32) {
    loop {
        let mut b = (u & 0x7f) as u8;
        u >>= 7;
        if u != 0 {
            b |= 0x80;
        }
        out.push(b);
        if u == 0 {
            break;
        }
    }
}

pub fn write_signed_varint(out: &mut Vec<u8>, v: i32) {
    write_uvarint32(out, zigzag_encode_i32(v));
}

pub fn read_uvarint32(data: &[u8], cur: &mut usize) -> Result<u32, SrsV2Error> {
    let mut shift = 0u32;
    let mut out = 0u32;
    let mut nbytes = 0usize;
    loop {
        if *cur >= data.len() {
            return Err(SrsV2Error::Truncated);
        }
        let b = data[*cur];
        *cur += 1;
        nbytes += 1;
        if nbytes > MAX_INTER_MV_VARINT_BYTES {
            return Err(SrsV2Error::syntax("inter MV varint too long"));
        }
        let val = (b & 0x7f) as u32;
        if shift >= 35 {
            return Err(SrsV2Error::syntax("inter MV varint overflow"));
        }
        out |= val << shift;
        shift += 7;
        if b & 0x80 == 0 {
            break;
        }
    }
    Ok(out)
}

pub fn read_signed_varint(data: &[u8], cur: &mut usize) -> Result<i32, SrsV2Error> {
    let u = read_uvarint32(data, cur)?;
    Ok(zigzag_decode_u32(u))
}

/// Serialize one MV component stream (e.g. backward MVs) using median prediction + varints.
/// Label echoed in bench JSON for MV prediction (left / top / top-right median).
pub const MV_PREDICTION_MODE_LABEL: &str = "median-left-top-topright";

/// Byte length of one zigzag signed varint as written by [`write_signed_varint`].
pub fn signed_varint_wire_bytes(v: i32) -> usize {
    let mut tmp = Vec::new();
    write_signed_varint(&mut tmp, v);
    tmp.len()
}

/// Per-component delta statistics for a compact MV grid (two varints per macroblock: Δx, Δy).
pub fn mv_compact_grid_delta_statistics(
    mvs: &[(i32, i32)],
    mb_cols: u32,
    mb_rows: u32,
) -> (u64, u64, u64, f64) {
    let mut decoded = vec![(0i32, 0i32); mvs.len()];
    let mut zero_v = 0_u64;
    let mut nonzero_v = 0_u64;
    let mut sum_abs = 0_u64;
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx = (mby * mb_cols + mbx) as usize;
            let (px, py) = predict_mv_qpel(mbx, mby, mb_cols, &decoded);
            let dx = mvs[idx].0 - px;
            let dy = mvs[idx].1 - py;
            for &d in &[dx, dy] {
                if d == 0 {
                    zero_v += 1;
                } else {
                    nonzero_v += 1;
                }
                sum_abs += d.unsigned_abs() as u64;
            }
            decoded[idx] = mvs[idx];
        }
    }
    let denom = (2u64 * mvs.len() as u64).max(1);
    let avg = sum_abs as f64 / denom as f64;
    (zero_v, nonzero_v, sum_abs, avg)
}

pub fn encode_mv_grid_compact(mvs: &[(i32, i32)], mb_cols: u32, mb_rows: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut decoded = vec![(0i32, 0i32); mvs.len()];
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx = (mby * mb_cols + mbx) as usize;
            let (px, py) = predict_mv_qpel(mbx, mby, mb_cols, &decoded);
            write_signed_varint(&mut buf, mvs[idx].0 - px);
            write_signed_varint(&mut buf, mvs[idx].1 - py);
            decoded[idx] = mvs[idx];
        }
    }
    buf
}

/// Decode compact MV grid from `data` starting at `cur` (advanced past MV bytes only).
pub fn decode_mv_grid_compact<F>(
    data: &[u8],
    cur: &mut usize,
    mb_cols: u32,
    mb_rows: u32,
    validate: F,
) -> Result<Vec<(i32, i32)>, SrsV2Error>
where
    F: Fn(i32, i32) -> Result<(), SrsV2Error>,
{
    let num = (mb_cols * mb_rows) as usize;
    let mut grid = vec![(0i32, 0i32); num];
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx = (mby * mb_cols + mbx) as usize;
            let (px, py) = predict_mv_qpel(mbx, mby, mb_cols, &grid);
            let dx = read_signed_varint(data, cur)?;
            let dy = read_signed_varint(data, cur)?;
            let mvx = px
                .checked_add(dx)
                .ok_or(SrsV2Error::CorruptedMotionVector)?;
            let mvy = py
                .checked_add(dy)
                .ok_or(SrsV2Error::CorruptedMotionVector)?;
            validate(mvx, mvy)?;
            grid[idx] = (mvx, mvy);
        }
    }
    Ok(grid)
}

/// Static **biased** byte model: low bytes (common in zigzag MV deltas) get higher frequency.
pub(crate) fn inter_mv_byte_rans_model() -> Result<RansModel, SrsV2Error> {
    let mut freqs = vec![14u32; 256];
    freqs[0] = RANS_SCALE - 255 * 14;
    RansModel::try_from_freqs(freqs).map_err(|_| SrsV2Error::syntax("inter MV rANS model"))
}

pub fn rans_encode_mv_bytes(bytes: &[u8]) -> Result<Vec<u8>, SrsV2Error> {
    let model = inter_mv_byte_rans_model()?;
    let symbols: Vec<usize> = bytes.iter().map(|&b| usize::from(b)).collect();
    rans_encode(&model, &symbols).map_err(|_| SrsV2Error::syntax("inter MV rANS encode failed"))
}

pub fn rans_decode_mv_bytes(
    blob: &[u8],
    nbytes: usize,
    budget: usize,
) -> Result<Vec<u8>, SrsV2Error> {
    let model = inter_mv_byte_rans_model()?;
    let syms = rans_decode(&model, blob, nbytes, budget)
        .map_err(|_| SrsV2Error::syntax("inter MV rANS decode failed"))?;
    Ok(syms.into_iter().map(|s| s as u8).collect())
}

/// `FR2` rev **19**+ macroblock partition types (low **2** bits; upper bits must be **0**).
pub const P_PART_WIRE_16X16: u8 = 0;
pub const P_PART_WIRE_16X8: u8 = 1;
pub const P_PART_WIRE_8X16: u8 = 2;
pub const P_PART_WIRE_8X8: u8 = 3;

#[inline]
pub fn pu_count_partition_wire(pt: u8) -> Result<usize, SrsV2Error> {
    let p = validate_partition_reserved_bits(pt)?;
    Ok(match p {
        P_PART_WIRE_16X16 => 1,
        P_PART_WIRE_16X8 | P_PART_WIRE_8X16 => 2,
        P_PART_WIRE_8X8 => 4,
        _ => {
            return Err(SrsV2Error::syntax("bad P partition wire"));
        }
    })
}

#[inline]
pub fn validate_partition_reserved_bits(b: u8) -> Result<u8, SrsV2Error> {
    if b & !3 != 0 {
        return Err(SrsV2Error::syntax("P partition reserved bits"));
    }
    Ok(b & 3)
}

/// Sum of partition units; rejects if larger than **macroblocks × 4**.
pub fn total_partition_units_bounded(
    partition_bytes: &[u8],
    max_mb: usize,
) -> Result<usize, SrsV2Error> {
    let mut t = 0usize;
    for &b in partition_bytes {
        let pt = validate_partition_reserved_bits(b)?;
        t = t
            .checked_add(pu_count_partition_wire(pt)?)
            .ok_or_else(|| SrsV2Error::syntax("P partition unit count overflow"))?;
        if t > max_mb.saturating_mul(4) {
            return Err(SrsV2Error::syntax("P partition unit cap exceeded"));
        }
    }
    Ok(t)
}

/// Encode MVs for variable partitions: first PU per MB uses spatial median; further PUs use previous PU MV.
pub fn encode_mv_stream_partitioned(
    mb_cols: u32,
    mb_rows: u32,
    partition_types: &[u8],
    pu_mvs: &[(i32, i32)],
) -> Result<Vec<u8>, SrsV2Error> {
    let n_mb = (mb_cols * mb_rows) as usize;
    if partition_types.len() != n_mb {
        return Err(SrsV2Error::syntax("P partition byte count mismatch"));
    }
    let total = total_partition_units_bounded(partition_types, n_mb)?;
    if pu_mvs.len() != total {
        return Err(SrsV2Error::syntax("P MV PU count mismatch"));
    }
    let mut grid_first = vec![(0_i32, 0_i32); n_mb];
    let mut out = Vec::new();
    let mut pu_i = 0usize;
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx = (mby * mb_cols + mbx) as usize;
            let pt = validate_partition_reserved_bits(partition_types[idx])?;
            let npu = pu_count_partition_wire(pt)?;
            for pi in 0..npu {
                let pred = if pi == 0 {
                    predict_mv_qpel(mbx, mby, mb_cols, &grid_first)
                } else {
                    pu_mvs[pu_i - 1]
                };
                let mv = pu_mvs[pu_i];
                write_signed_varint(&mut out, mv.0 - pred.0);
                write_signed_varint(&mut out, mv.1 - pred.1);
                pu_i += 1;
            }
            grid_first[idx] = pu_mvs[pu_i - npu];
        }
    }
    Ok(out)
}

pub fn decode_mv_stream_partitioned<F>(
    data: &[u8],
    cur: &mut usize,
    mb_cols: u32,
    mb_rows: u32,
    partition_types: &[u8],
    validate: &mut F,
) -> Result<Vec<(i32, i32)>, SrsV2Error>
where
    F: FnMut(i32, i32) -> Result<(), SrsV2Error>,
{
    let n_mb = (mb_cols * mb_rows) as usize;
    if partition_types.len() != n_mb {
        return Err(SrsV2Error::syntax("P partition byte count mismatch"));
    }
    let total = total_partition_units_bounded(partition_types, n_mb)?;
    let mut out = Vec::with_capacity(total);
    let mut grid_first = vec![(0_i32, 0_i32); n_mb];
    for mby in 0..mb_rows {
        for mbx in 0..mb_cols {
            let idx = (mby * mb_cols + mbx) as usize;
            let pt = validate_partition_reserved_bits(partition_types[idx])?;
            let npu = pu_count_partition_wire(pt)?;
            for pi in 0..npu {
                let pred = if pi == 0 {
                    predict_mv_qpel(mbx, mby, mb_cols, &grid_first)
                } else {
                    *out.last()
                        .ok_or_else(|| SrsV2Error::syntax("P MV PU decode underflow"))?
                };
                let dx = read_signed_varint(data, cur)?;
                let dy = read_signed_varint(data, cur)?;
                let mvx = pred
                    .0
                    .checked_add(dx)
                    .ok_or(SrsV2Error::CorruptedMotionVector)?;
                let mvy = pred
                    .1
                    .checked_add(dy)
                    .ok_or(SrsV2Error::CorruptedMotionVector)?;
                validate(mvx, mvy)?;
                out.push((mvx, mvy));
            }
            let start = out.len() - npu;
            grid_first[idx] = out[start];
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_three() {
        assert_eq!(median_i32(3, 1, 2), 2);
        assert_eq!(median_i32(-5, 0, -1), -1);
    }

    #[test]
    fn zero_mv_field_zero_deltas() {
        let mb_cols = 4;
        let mb_rows = 4;
        let mvs = vec![(0i32, 0i32); 16];
        let enc = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        assert!(
            enc.iter().all(|&b| b == 0),
            "zigzag 0 -> single zero byte each"
        );
        let mut c = 0usize;
        let dec = decode_mv_grid_compact(&enc, &mut c, mb_cols, mb_rows, |_x, _y| Ok(())).unwrap();
        assert_eq!(c, enc.len());
        assert_eq!(dec, mvs);
    }

    #[test]
    fn smooth_motion_roundtrip() {
        let mb_cols = 4;
        let mb_rows = 1;
        let mvs: Vec<(i32, i32)> = (0..4).map(|i| (i * 4, 0)).collect();
        let enc = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let mut c = 0usize;
        let dec = decode_mv_grid_compact(&enc, &mut c, mb_cols, mb_rows, |_x, _y| Ok(())).unwrap();
        assert_eq!(c, enc.len());
        assert_eq!(dec, mvs);
    }

    #[test]
    fn edge_mb_predictor_safe() {
        let mb_cols = 2;
        let mb_rows = 2;
        let mvs = vec![(8, -4), (12, 0), (-4, 8), (20, 4)];
        let enc = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let mut c = 0usize;
        let dec = decode_mv_grid_compact(&enc, &mut c, mb_cols, mb_rows, |_x, _y| Ok(())).unwrap();
        assert_eq!(c, enc.len());
        assert_eq!(dec, mvs);
    }

    #[test]
    fn truncated_varint_fails() {
        let mut cur = 0usize;
        assert!(read_signed_varint(&[0x80], &mut cur).is_err());
    }

    #[test]
    fn mv_rans_roundtrip_bytes() {
        let b = encode_mv_grid_compact(&[(0, 0), (4, -8)], 2, 1);
        let blob = rans_encode_mv_bytes(&b).unwrap();
        let out = rans_decode_mv_bytes(&blob, b.len(), blob.len().saturating_mul(64)).unwrap();
        assert_eq!(out, b);
    }

    #[test]
    fn mv_rans_decode_step_budget_exhausted_fails() {
        let b = encode_mv_grid_compact(&[(0, 0), (4, -8)], 2, 1);
        let blob = rans_encode_mv_bytes(&b).unwrap();
        assert!(rans_decode_mv_bytes(&blob, b.len(), 0).is_err());
    }

    #[test]
    fn random_mv_grid_roundtrip_deterministic() {
        let mb_cols = 5_u32;
        let mb_rows = 5_u32;
        let n = (mb_cols * mb_rows) as usize;
        let mut mvs = vec![(0_i32, 0_i32); n];
        let mut s: u64 = 0xDECAFBAD;
        for mv in &mut mvs {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            mv.0 = ((s >> 17) & 0x1FF) as i32 - 256;
            mv.1 = ((s >> 33) & 0x1FF) as i32 - 256;
        }
        let enc = encode_mv_grid_compact(&mvs, mb_cols, mb_rows);
        let mut c = 0usize;
        let dec = decode_mv_grid_compact(&enc, &mut c, mb_cols, mb_rows, |_x, _y| Ok(())).unwrap();
        assert_eq!(c, enc.len());
        assert_eq!(dec, mvs);
    }

    #[test]
    fn half_grid_even_qpels_roundtrip() {
        let mvs = vec![(0_i32, 0_i32), (8, -4), (-16, 12)];
        let enc = encode_mv_grid_compact(&mvs, 3, 1);
        let mut c = 0usize;
        let dec = decode_mv_grid_compact(&enc, &mut c, 3, 1, |_x, _y| Ok(())).unwrap();
        assert_eq!(dec, mvs);
    }

    #[test]
    fn partitioned_mv_stream_roundtrips_two_mbs() {
        let mb_cols = 2u32;
        let mb_rows = 1u32;
        let parts = vec![P_PART_WIRE_16X16, P_PART_WIRE_8X8];
        let mvs = vec![(0, 0), (8, 0), (0, 8), (8, 8), (4, -4)];
        let enc = encode_mv_stream_partitioned(mb_cols, mb_rows, &parts, &mvs).unwrap();
        let mut cur = 0usize;
        let mut val = |_mx: i32, _my: i32| Ok(());
        let dec = decode_mv_stream_partitioned(&enc, &mut cur, mb_cols, mb_rows, &parts, &mut val)
            .unwrap();
        assert_eq!(dec, mvs);
        assert_eq!(cur, enc.len());
    }

    #[test]
    fn partition_reserved_bits_rejected() {
        assert!(validate_partition_reserved_bits(4).is_err());
    }

    #[test]
    fn partition_unit_cap_exceeded_when_rows_do_not_match_budget() {
        // Hostile or mismatched slice: three macroblocks worth of split8×8 partition bytes
        // but budget allows only one macroblock → 12 PU > 4×1.
        let parts = vec![P_PART_WIRE_8X8, P_PART_WIRE_8X8, P_PART_WIRE_8X8];
        assert!(total_partition_units_bounded(&parts, 1).is_err());
        assert_eq!(total_partition_units_bounded(&parts, 3).unwrap(), 12);
    }

    #[test]
    fn partition_unit_sum_hits_exact_cap_all_split8x8() {
        let n_mb = 5usize;
        let parts = vec![P_PART_WIRE_8X8; n_mb];
        assert_eq!(
            total_partition_units_bounded(&parts, n_mb).unwrap(),
            n_mb * 4
        );
    }
}
