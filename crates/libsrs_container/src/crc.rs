use crc32fast::Hasher;

/// IEEE CRC-32 as used by legacy v1 SRSM container block envelopes.
pub fn crc32(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

/// CRC-32C (Castagnoli) for v2+ `.528` block bodies (per envelope spec).
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c::crc32c(data)
}
