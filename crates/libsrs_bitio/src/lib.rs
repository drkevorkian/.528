//! Bit-aligned I/O, LEB128 varints, and rANS helpers for the `.528` stack.
//!
//! All operations are panic-free on malformed input; errors are [`BitIoError`].

mod bit_io;
mod error;
mod rans;
mod varint;

pub use bit_io::{BitReader, BitWriter};
pub use error::{BitIoError, BitIoResult};
pub use rans::{
    rans_decode, rans_decode_step_symbol, rans_decode_symbols_multi_context, rans_encode,
    rans_encode_symbols_multi_context, RansModel, RANS_L, RANS_MAX_ALPHABET, RANS_SCALE,
    RANS_SCALE_BITS,
};
pub use varint::{
    decode_i64_varint, decode_u64_varint, encode_i64_varint_into, encode_u64_varint_into,
    read_i64_varint_from_bits, read_u64_varint_from_bits, MAX_U64_VARINT_BYTES,
};
