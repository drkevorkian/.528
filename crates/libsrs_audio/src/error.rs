use std::fmt::{Display, Formatter};
use std::io;

#[derive(Debug)]
pub enum AudioCodecError {
    InvalidData(&'static str),
    UnsupportedChannels(u8),
    CrcMismatch {
        expected: u32,
        actual: u32,
    },
    Io(io::Error),
}

impl Display for AudioCodecError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidData(msg) => write!(f, "invalid audio data: {msg}"),
            Self::UnsupportedChannels(ch) => write!(f, "unsupported channel count: {ch}"),
            Self::CrcMismatch { expected, actual } => {
                write!(f, "crc mismatch expected {expected:#010x}, got {actual:#010x}")
            }
            Self::Io(err) => write!(f, "i/o error: {err}"),
        }
    }
}

impl std::error::Error for AudioCodecError {}

impl From<io::Error> for AudioCodecError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
