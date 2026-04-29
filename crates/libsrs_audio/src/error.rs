use std::io;

use libsrs_bitio::BitIoError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioCodecError {
    #[error("invalid audio data: {0}")]
    InvalidData(&'static str),
    #[error("unsupported channel count: {0}")]
    UnsupportedChannels(u8),
    #[error("crc mismatch expected {expected:#010x}, got {actual:#010x}")]
    CrcMismatch { expected: u32, actual: u32 },
    #[error("entropy: {0}")]
    Entropy(#[from] BitIoError),
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}
