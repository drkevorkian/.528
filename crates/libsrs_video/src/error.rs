use std::fmt::{Display, Formatter};
use std::io;

#[derive(Debug)]
pub enum VideoCodecError {
    InvalidData(&'static str),
    DimensionMismatch {
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    UnsupportedFrameType(u8),
    CrcMismatch {
        expected: u32,
        actual: u32,
    },
    Io(io::Error),
}

impl Display for VideoCodecError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidData(msg) => write!(f, "invalid video data: {msg}"),
            Self::DimensionMismatch {
                expected_width,
                expected_height,
                actual_width,
                actual_height,
            } => write!(
                f,
                "dimension mismatch expected {}x{}, got {}x{}",
                expected_width, expected_height, actual_width, actual_height
            ),
            Self::UnsupportedFrameType(t) => write!(f, "unsupported frame type: {t}"),
            Self::CrcMismatch { expected, actual } => {
                write!(
                    f,
                    "crc mismatch expected {expected:#010x}, got {actual:#010x}"
                )
            }
            Self::Io(err) => write!(f, "i/o error: {err}"),
        }
    }
}

impl std::error::Error for VideoCodecError {}

impl From<io::Error> for VideoCodecError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
