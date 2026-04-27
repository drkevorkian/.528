use crate::format::BLOCK_MAGIC;

pub fn find_next_block_magic(data: &[u8], start: usize) -> Option<usize> {
    if data.len() < BLOCK_MAGIC.len() || start >= data.len() {
        return None;
    }
    data[start..]
        .windows(BLOCK_MAGIC.len())
        .position(|window| window == BLOCK_MAGIC)
        .map(|offset| start + offset)
}
