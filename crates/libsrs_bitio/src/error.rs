use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BitIoError {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("bit read count must be 1..=64, got {0}")]
    InvalidBitCount(u8),
    #[error("value does not fit in {bits} bits")]
    ValueOutOfRange { bits: u8 },
    #[error("varint too long or overflows u64")]
    InvalidVarint,
    #[error("varint encoding exceeds maximum length")]
    VarintTooLong,
    #[error("rANS: {0}")]
    Rans(String),
    #[error("rANS decode exceeded iteration budget")]
    RansDecodeBudget,
}

pub type BitIoResult<T> = Result<T, BitIoError>;
